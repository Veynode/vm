//! Private per-VM native capture owner. No Internet socket or remote authorization.
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use vm_control::screenshot_scaled::{Refusal, Refused, Scaled};
use vm_media_local::console_read::PollReader;
use vm_media_local::console_wire::{read_record, Record};
use vm_media_local::console_write::write_record_deadline;
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: i32) {
    STOP.store(true, Ordering::Release);
}
// Readiness wakes on a new connection; timeout only bounds maintenance latency.
// An interrupted wait returns to the outer STOP/child checks without retrying.
fn wait_listener_readable(fd: std::os::fd::RawFd, timeout: Duration) -> Result<bool> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe {
        libc::poll(
            &mut pollfd,
            1,
            timeout.as_millis().clamp(1, i32::MAX as u128) as i32,
        )
    };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error.into());
    }
    if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        bail!("console listener readiness failed");
    }
    Ok(result > 0 && pollfd.revents & libc::POLLIN != 0)
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
#[derive(Clone)]
struct Output {
    tx: mpsc::SyncSender<Record>,
    socket: Arc<UnixStream>,
}
impl Output {
    fn deliver(&self, record: Record) {
        if self.tx.try_send(record).is_err() {
            let _ = self.socket.shutdown(std::net::Shutdown::Both);
        }
    }
}
fn native_write(
    input: &mut (impl Write + std::os::fd::AsRawFd),
    value: &Value,
    budget: Option<&ScreenshotBudget<'_>>,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if bytes.len() > 1024 {
        bail!("native command exceeds bound")
    }
    let until = Instant::now() + Duration::from_millis(500);
    let until = budget.map_or(until, |budget| until.min(budget.deadline));
    let mut offset = 0;
    while offset < bytes.len() {
        if let Some(budget) = budget {
            budget.check_write(offset)?;
        }
        match input.write(&bytes[offset..]) {
            Ok(0) => bail!("native pipe closed"),
            Ok(n) => offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if STOP.load(Ordering::Acquire) {
                    bail!("broker interrupted during capability probe")
                }
                if Instant::now() >= until {
                    if let Some(budget) = budget {
                        budget.check_write(offset)?;
                    }
                    bail!("native pipe write deadline")
                }
                let mut fd = libc::pollfd {
                    fd: input.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                unsafe {
                    libc::poll(&mut fd, 1, 10);
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
struct Owner {
    client: u64,
    epoch: u64,
    output: Output,
    acknowledged: bool,
    native_ended: bool,
    early: Vec<Record>,
}
impl Owner {
    fn deliver(&mut self, record: Record) {
        if self.acknowledged {
            self.output.deliver(record);
        } else if self.early.len() < 2 {
            self.early.push(record);
        } else {
            let _ = self.output.socket.shutdown(std::net::Shutdown::Both);
        }
    }
}
struct Native {
    input: Mutex<ChildStdin>,
    pending: Mutex<BTreeMap<String, (u64, mpsc::SyncSender<Value>)>>,
    next: AtomicU64,
    alive: AtomicBool,
    ready: (Mutex<bool>, Condvar),
    owner: Mutex<Option<Owner>>,
    next_epoch: AtomicU64,
    snapshot: Mutex<()>,
    capabilities: Value,
}
fn receive_native(receive: &Native, pipe: &mut impl Read) -> Result<()> {
    let result = (|| {
        loop {
            let record = read_record(pipe)?;
            if record.kind == 2 {
                let mut owner = receive.owner.lock().unwrap();
                if let Some(owner) = owner.as_mut().filter(|o| o.epoch == record.epoch) {
                    owner.deliver(record);
                }
                continue;
            }
            let value = record.value()?;
            if let Some(id) = value["id"].as_str() {
                if let Some((epoch, tx)) = receive.pending.lock().unwrap().remove(id) {
                    if epoch != record.epoch {
                        bail!("native reply epoch mismatch")
                    }
                    let _ = tx.send(value);
                }
                continue;
            }
            if value["event"] == "ready"
                && value["protocol"] == 1
                && value["capture_connected"] == true
            {
                let mut ready = receive.ready.0.lock().unwrap();
                *ready = true;
                receive.ready.1.notify_all();
            }
            if value["event"] == "capture-error" {
                bail!("native capture failed")
            }
            if value["event"] == "session-error" {
                if record.epoch == 0 || value["epoch"].as_u64() != Some(record.epoch) {
                    bail!("native session error has inconsistent epoch")
                }
                if let Some(owner) = receive
                    .owner
                    .lock()
                    .unwrap()
                    .as_mut()
                    .filter(|o| o.epoch == record.epoch)
                {
                    owner.native_ended = true;
                    owner.deliver(record);
                }
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    })();
    if let Err(error) = &result {
        eprintln!("console reader stopped: {error}");
    }
    receive.alive.store(false, Ordering::Release);
    receive.ready.1.notify_all();
    for (_, (_, tx)) in std::mem::take(&mut *receive.pending.lock().unwrap()) {
        let _ = tx.send(json!({"error":"capture process unavailable"}));
    }
    if let Some(owner) = receive.owner.lock().unwrap().as_ref() {
        let _ = owner.output.socket.shutdown(std::net::Shutdown::Both);
    }
    result
}

impl Native {
    fn call(&self, command: Value) -> Result<Value> {
        self.call_with_budget(command, None)
    }
    fn call_with_budget(
        &self,
        command: Value,
        budget: Option<&ScreenshotBudget<'_>>,
    ) -> Result<Value> {
        self.call_with_writer(command, budget, native_write)
    }
    fn call_with_writer(
        &self,
        mut command: Value,
        budget: Option<&ScreenshotBudget<'_>>,
        write: impl FnOnce(&mut ChildStdin, &Value, Option<&ScreenshotBudget<'_>>) -> Result<()>,
    ) -> Result<Value> {
        if let Some(budget) = budget {
            budget.check()?;
        }
        if !self.alive.load(Ordering::Acquire) {
            bail!("capture process unavailable")
        }
        let id = format!("c{}", self.next.fetch_add(1, Ordering::Relaxed));
        command["id"] = json!(id);
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.len() >= 16 {
                bail!("native request capacity reached")
            }
            // Display commands use envelope epoch zero. Their acquisition epoch
            // belongs to native input authority, not an encoder incarnation.
            let epoch = if matches!(
                command["method"].as_str(),
                Some("show_window" | "close_window" | "window_status" | "validate_displayed_frame")
            ) {
                0
            } else {
                command["epoch"].as_u64().unwrap_or(0)
            };
            pending.insert(id.clone(), (epoch, tx));
        }
        let send = (|| {
            let mut input = if let Some(budget) = budget {
                loop {
                    budget.check().map_err(ScreenshotNotSent)?;
                    match self.input.try_lock() {
                        Ok(input) => break input,
                        Err(std::sync::TryLockError::WouldBlock) => {
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(std::sync::TryLockError::Poisoned(_)) => {
                            bail!("native input lock poisoned")
                        }
                    }
                }
            } else {
                self.input.lock().unwrap()
            };
            write(&mut input, &command, budget)
        })();
        if let Err(error) = send {
            self.pending.lock().unwrap().remove(&id);
            if !error.is::<ScreenshotNotSent>() {
                self.alive.store(false, Ordering::Release);
            }
            return Err(error);
        }
        let timeout = Duration::from_secs(if command["method"] == "shutdown" {
            1
        } else if [
            "show_window",
            "close_window",
            "window_status",
            "validate_displayed_frame",
            "rate_status",
            "set_bitrate",
        ]
        .contains(&command["method"].as_str().unwrap_or(""))
        {
            2
        } else {
            12
        });
        let until = Instant::now() + timeout;
        let until = budget.map_or(until, |budget| until.min(budget.deadline));
        let reply = loop {
            if let Some(budget) = budget {
                if client_disconnected(budget.client) {
                    self.pending.lock().unwrap().remove(&id);
                    bail!("screenshot client disconnected")
                }
            }
            let remaining = until.saturating_duration_since(Instant::now());
            let wait = if budget.is_some() {
                remaining.min(SCREENSHOT_CANCEL_POLL)
            } else {
                remaining
            };
            match rx.recv_timeout(wait) {
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < until => continue,
                reply => break reply,
            }
        };
        self.pending.lock().unwrap().remove(&id);
        match reply {
            Ok(mut value) => {
                if let Some(error) = value.get("error") {
                    if let Some(stage) = helper_display_rejection(&command["method"], error) {
                        return Err(stage.into());
                    }
                    if matches!(
                        command["method"].as_str(),
                        Some("rate_status" | "set_bitrate")
                    ) {
                        bail!(
                            "{}",
                            vm_media_local::rate_control::error_code(
                                error["code"].as_str().unwrap_or("")
                            )
                        )
                    }
                    if command["method"] == "screenshot" && error["code"] == "screenshot_busy" {
                        return Err(HelperBusy(error.to_string()).into());
                    }
                    // A request of the scaled form: the helper's refusal keeps its fixed word beside the text.
                    if command["method"] == "screenshot" && command.get("format").is_some() {
                        if let Some(refusal) = Refusal::from_helper(error) {
                            return Err(Refused::new(
                                refusal,
                                format!("native operation failed: {error}"),
                            )
                            .into());
                        }
                    }
                    bail!("native operation failed: {error}")
                }
                // Moved out, not copied: a screenshot result is several MiB.
                match value.get_mut("result").filter(|result| result.is_object()) {
                    Some(result) => Ok(result.take()),
                    None => {
                        self.alive.store(false, Ordering::Release);
                        bail!("malformed native acknowledgement; capture owner fenced")
                    }
                }
            }
            Err(_) => {
                self.alive.store(false, Ordering::Release);
                bail!("native response deadline exceeded; capture owner fenced")
            }
        }
    }
    fn start(&self, client: u64, output: Output, codec: &str) -> Result<Value> {
        if !matches!(codec, "h264" | "hevc") {
            bail!("unsupported codec")
        }
        let epoch = self.next_epoch.fetch_add(1, Ordering::Relaxed);
        if epoch > 9_007_199_254_740_991 {
            bail!("encoder epoch exhausted")
        }
        {
            let mut owner = self.owner.lock().unwrap();
            if owner.is_some() {
                bail!("encoder busy")
            }
            *owner = Some(Owner {
                client,
                epoch,
                output,
                acknowledged: false,
                native_ended: false,
                early: Vec::new(),
            });
        }
        match self.call(json!({"method":"start","epoch":epoch,"codec":codec})) {
            Ok(mut value) => {
                value["epoch"] = json!(epoch);
                Ok(value)
            }
            Err(error) => {
                let _ = self.close(client);
                Err(error)
            }
        }
    }
    fn acknowledge_start(&self, client: u64) {
        let mut owner = self.owner.lock().unwrap();
        if let Some(owner) = owner.as_mut().filter(|o| o.client == client) {
            owner.acknowledged = true;
            for record in owner.early.drain(..) {
                owner.output.deliver(record);
            }
        }
    }
    fn close(&self, client: u64) -> Result<Value> {
        let epoch = {
            let mut owner = self.owner.lock().unwrap();
            if owner
                .as_ref()
                .is_some_and(|o| o.client == client && o.native_ended)
            {
                *owner = None;
                return Ok(json!({"stopped":true,"reason":"native_session_ended"}));
            }
            match &*owner {
                Some(o) if o.client == client => o.epoch,
                Some(_) => bail!("not encoder owner"),
                None => return Ok(json!({"stopped":true})),
            }
        };
        // Retain ownership until native stop has drained the encoder and acknowledged.
        let result = self
            .call(json!({"method":"stop","epoch":epoch}))
            .and_then(|value| {
                if value["state"] != "stopped" {
                    bail!("native encoder stop not confirmed")
                }
                Ok(value)
            });
        if result.is_err() {
            // A rejected or lost stop does not establish that native encoding ended.
            // Keep ownership and fence the whole capture owner; supervisor reaps it.
            self.alive.store(false, Ordering::Release);
            return result;
        }
        let mut owner = self.owner.lock().unwrap();
        if owner
            .as_ref()
            .is_some_and(|o| o.client == client && o.epoch == epoch)
        {
            *owner = None;
        }
        result
    }
    fn rate(&self, client: u64, request: &Request) -> Result<Value> {
        use vm_media_local::rate_control::{Receipt, Request as RateRequest};
        let rate = RateRequest {
            version: request.version.context("missing rate version")?,
            request_id: request.request_id.clone(),
            method: request.method.clone(),
            encoder_instance: request.encoder_instance.clone(),
            epoch: request.epoch,
            sequence: request.sequence,
            target_bps: request.target_bps,
        };
        rate.validate()?;
        if request.codec.is_some() || request.frame.is_some() {
            bail!("invalid_request")
        }
        if self.capabilities["rate_control_version"] != 1 {
            bail!("rate control unsupported")
        }
        let epoch = {
            let owner = self.owner.lock().unwrap();
            owner
                .as_ref()
                .filter(|o| o.client == client && !o.native_ended)
                .context("not encoder owner")?
                .epoch
        };
        if rate.epoch.is_some_and(|e| e != epoch) {
            bail!("stale_encoder")
        }
        let mut command = serde_json::to_value(&rate)?;
        command.as_object_mut().unwrap().remove("request_id");
        command["epoch"] = json!(epoch);
        let result = self.call(command)?;
        let receipt: Receipt =
            serde_json::from_value(result.clone()).context("invalid rate receipt")?;
        receipt.validate(&rate, epoch)?;
        let owner = self.owner.lock().unwrap();
        if !owner
            .as_ref()
            .is_some_and(|o| o.client == client && o.epoch == epoch && !o.native_ended)
        {
            bail!("stale_encoder")
        }
        Ok(result)
    }
    fn keyframe(&self, client: u64) -> Result<Value> {
        let epoch = {
            let owner = self.owner.lock().unwrap();
            owner
                .as_ref()
                .filter(|o| o.client == client)
                .context("not encoder owner")?
                .epoch
        };
        self.call(json!({"method":"keyframe","epoch":epoch}))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    request_id: String,
    method: String,
    codec: Option<String>,
    frame: Option<vm_control::native_input::DisplayedFrame>,
    #[serde(
        default,
        deserialize_with = "vm_control::native_input::deserialize_diagnostic_id"
    )]
    diagnostic_id: Option<u64>,
    #[serde(
        default,
        deserialize_with = "vm_control::native_input::deserialize_acquisition_receipt"
    )]
    acquisition_receipt: Option<String>,
    version: Option<u64>,
    encoder_instance: Option<String>,
    epoch: Option<u64>,
    sequence: Option<u64>,
    target_bps: Option<u64>,
    // The scaled-JPEG screenshot form. Only named here so that the request is not refused as unknown; the values
    // are read from the request line by `Scaled::parse`, which refuses a partial or out-of-range set.
    format: Option<Value>,
    max_width: Option<Value>,
    max_height: Option<Value>,
    quality: Option<Value>,
}
fn helper_display_rejection(
    method: &Value,
    error: &Value,
) -> Option<vm_control::native_input::RejectionStage> {
    if method != "validate_displayed_frame" {
        return None;
    }
    vm_control::native_input::RejectionStage::display_rejection(&error["code"])
}

fn rejection_reply(request_id: &str, error: &anyhow::Error) -> Value {
    let mut reply = json!({"request_id":request_id,"error":error.to_string()});
    if let Some(stage) = error.downcast_ref::<vm_control::native_input::RejectionStage>() {
        reply["display_rejection"] = json!(stage);
    }
    Refused::mark(&mut reply, error);
    reply
}

/// The helper refused a screenshot as busy. Same text as any other native failure.
#[derive(Debug)]
struct HelperBusy(String);
impl std::fmt::Display for HelperBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native operation failed: {}", self.0)
    }
}
impl std::error::Error for HelperBusy {}
/// The helper keeps its previous screenshot job until its 10 ms owner tick has seen that reply written, and refuses
/// the next request as busy until then. Screenshots are serialized by `Native::snapshot`, so that window is the
/// only busy it can report here: a request that follows a reply at once waits it out, at most this often.
const HELPER_BUSY_RETRIES: usize = 4;
const HELPER_BUSY_STEP: Duration = Duration::from_millis(4);
// Capture::broker_scaled in vm-qemu uses 15 seconds after writing the request.
// The wire request carries no deadline, so start the same budget when it is received.
const SCREENSHOT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const HELPER_BUSY_MIN_ATTEMPT: Duration = Duration::from_millis(1);
const SCREENSHOT_CANCEL_POLL: Duration = Duration::from_millis(10);

#[derive(Debug, PartialEq, Eq)]
enum ScreenshotInterrupted {
    Cancelled,
    Deadline,
}
impl std::fmt::Display for ScreenshotInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "screenshot client disconnected",
            Self::Deadline => "screenshot request deadline exceeded",
        })
    }
}
impl std::error::Error for ScreenshotInterrupted {}

// A send failure may leave the helper unfenced only if no command byte was written.
#[derive(Debug)]
struct ScreenshotNotSent(ScreenshotInterrupted);
impl std::fmt::Display for ScreenshotNotSent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for ScreenshotNotSent {}

struct ScreenshotBudget<'a> {
    deadline: Instant,
    client: &'a UnixStream,
}
impl ScreenshotBudget<'_> {
    fn check_write(&self, written: usize) -> Result<()> {
        self.check().map_err(|error| {
            if written == 0 {
                ScreenshotNotSent(error).into()
            } else {
                error.into()
            }
        })
    }
    fn check(&self) -> std::result::Result<(), ScreenshotInterrupted> {
        if client_disconnected(self.client) {
            return Err(ScreenshotInterrupted::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(ScreenshotInterrupted::Deadline);
        }
        Ok(())
    }
}
// The connection reader is synchronous, so peek without consuming a pipelined request.
// Capture::drop shuts down both socket directions when the worker abandons its request.
fn client_disconnected(client: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut fd = libc::pollfd {
        fd: client.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut fd, 1, 0) } > 0
        && fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
    {
        return true;
    }
    let mut byte = 0u8;
    let result = unsafe {
        libc::recv(
            client.as_raw_fd(),
            (&mut byte as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    result == 0
        || (result < 0
            && !matches!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ))
}
fn screenshot_retry(
    snapshot: &Mutex<()>,
    deadline: Instant,
    mut call: impl FnMut(Instant) -> Result<Value>,
    now: impl Fn() -> Instant,
    mut sleep: impl FnMut(Duration),
    disconnected: impl Fn() -> bool,
) -> Result<Value> {
    let _guard = snapshot
        .try_lock()
        .map_err(|_| anyhow!("screenshot busy"))?;
    if disconnected() {
        return Err(ScreenshotInterrupted::Cancelled.into());
    }
    if now() >= deadline {
        return Err(ScreenshotInterrupted::Deadline.into());
    }
    let mut result = call(deadline);
    for _ in 0..HELPER_BUSY_RETRIES {
        if !matches!(&result, Err(error) if error.is::<HelperBusy>())
            || disconnected()
            || deadline.saturating_duration_since(now())
                < HELPER_BUSY_STEP + HELPER_BUSY_MIN_ATTEMPT
        {
            break;
        }
        sleep(HELPER_BUSY_STEP);
        // Sleep may overshoot, and the worker may have cancelled during it.
        if disconnected() || deadline.saturating_duration_since(now()) < HELPER_BUSY_MIN_ATTEMPT {
            break;
        }
        result = call(deadline);
    }
    result
}
/// `scaled` adds the four fields of the scaled-JPEG form to the helper command; `None` sends the command as before.
/// A JPEG answer to the scaled form is checked against the request and against itself before it is forwarded. A
/// PNG answer comes from a helper without that form and is forwarded as any PNG is; the caller reads `mime_type`.
fn screenshot(
    native: &Native,
    scaled: Option<Scaled>,
    budget: ScreenshotBudget<'_>,
) -> Result<Value> {
    let mut command = json!({"method":"screenshot"});
    if let Some(scaled) = &scaled {
        scaled.apply(&mut command);
    }
    screenshot_retry(
        &native.snapshot,
        budget.deadline,
        |_| {
            let value = native.call_with_budget(command.clone(), Some(&budget))?;
            if let Some(scaled) = &scaled {
                if value["mime_type"] != "image/png" {
                    scaled
                        .verify(&value)
                        .context("native scaled screenshot invalid")?;
                }
            }
            Ok(value)
        },
        Instant::now,
        thread::sleep,
        || client_disconnected(budget.client),
    )
}
fn encoder_owner(native: &Native, client: u64) -> bool {
    native
        .owner
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|owner| owner.client == client)
}
fn connection(stream: UnixStream, native: Arc<Native>, client: u64) -> Result<()> {
    stream.set_nonblocking(true)?;
    let shared = Arc::new(stream.try_clone()?);
    let (tx, rx) = mpsc::sync_channel(3);
    let output = Output {
        tx,
        socket: shared.clone(),
    };
    let writer = thread::spawn(move || {
        for record in rx {
            if write_record_deadline(&stream, &record, Duration::from_millis(500)).is_err() {
                break;
            }
        }
        let _ = stream.shutdown(std::net::Shutdown::Both);
    });
    let result = (|| {
        let mut reader = BufReader::new(PollReader::new(shared.try_clone()?));
        loop {
            reader
                .get_mut()
                .set_deadline(Instant::now() + Duration::from_secs(300));
            let mut line = Vec::new();
            let n = (&mut reader).take(4097).read_until(b'\n', &mut line)?;
            if n == 0 {
                break;
            }
            if n > 4096 || line.last() != Some(&b'\n') {
                bail!("invalid request frame")
            }
            let received = Instant::now();
            let request: Request = serde_json::from_slice(&line)?;
            if request.request_id.is_empty() || request.request_id.len() > 64 {
                bail!("invalid request identity")
            }
            if matches!(request.method.as_str(), "rate_status" | "set_bitrate") {
                vm_media_local::rate_control::Request::parse(&line)?;
            }
            if request.acquisition_receipt.is_some() && request.method != "validate_displayed_frame"
            {
                bail!("unexpected frame acquisition receipt")
            }
            if !matches!(
                request.method.as_str(),
                "rate_status" | "set_bitrate" | "validate_displayed_frame"
            ) && (request.version.is_some()
                || request.encoder_instance.is_some()
                || request.epoch.is_some()
                || request.sequence.is_some()
                || request.target_bps.is_some())
            {
                bail!("unexpected rate fields")
            }
            if request.method != "screenshot"
                && (request.format.is_some()
                    || request.max_width.is_some()
                    || request.max_height.is_some()
                    || request.quality.is_some())
            {
                bail!("unexpected screenshot fields")
            }
            if request.diagnostic_id.is_some() && request.method != "validate_displayed_frame" {
                bail!("unexpected native diagnostic identity")
            }
            let result = match request.method.as_str() {
                "rate_status" | "set_bitrate" => native.rate(client, &request),
                "capabilities" => Ok(native.capabilities.clone()),
                "status" => Ok(
                    json!({"capture_connected":native.alive.load(Ordering::Acquire),"encoder_active":native.owner.lock().unwrap().is_some()}),
                ),
                "start" => native.start(
                    client,
                    output.clone(),
                    request.codec.as_deref().context("missing codec")?,
                ),
                "stop" => native.close(client),
                "keyframe" => native.keyframe(client),
                "show_window" | "close_window" | "window_status" => {
                    if !vm_control::backend::window_supported(&native.capabilities) {
                        Err(anyhow!("native display-only window unavailable"))
                    } else {
                        native
                            .call(json!({"method":request.method}))
                            .and_then(|result| {
                                vm_control::backend::validate_window_capabilities(result.clone())?;
                                if !result["window_open"].is_boolean()
                                    || !vm_control::backend::window_supported(&result)
                                {
                                    bail!("invalid native window acknowledgement")
                                }
                                if (request.method == "show_window"
                                    && result["window_open"] != true)
                                    || (request.method == "close_window"
                                        && result["window_open"] != false)
                                {
                                    bail!("native window did not acknowledge requested state")
                                }
                                Ok(result)
                            })
                    }
                }
                "validate_displayed_frame" => {
                    let frame = request.frame.as_ref().context("displayed frame required")?;
                    frame.validate()?;
                    let mut command = json!({"method":"validate_displayed_frame","frame":frame});
                    if let Some(id) = request.diagnostic_id {
                        command["diagnostic_id"] = json!(id);
                    }
                    let raw: Value = serde_json::from_slice(&line)?;
                    if raw.get("version").is_some()
                        || raw.get("encoder_instance").is_some()
                        || raw.get("target_bps").is_some()
                    {
                        bail!("unexpected frame validation fields")
                    }
                    if let Some(receipt) = &request.acquisition_receipt {
                        let epoch = request
                            .epoch
                            .filter(|n| *n > 0)
                            .context("frame acquisition epoch required")?;
                        let sequence = request
                            .sequence
                            .filter(|n| *n > 0)
                            .context("frame acquisition sequence required")?;
                        command["acquisition_receipt"] = json!(receipt);
                        command["epoch"] = json!(epoch);
                        command["sequence"] = json!(sequence);
                    } else if raw.get("epoch").is_some() || raw.get("sequence").is_some() {
                        bail!("unexpected legacy frame acquisition fields")
                    }
                    native.call(command)
                }
                "frame_status" => native.call(json!({"method":"frame_status"})),
                "screenshot" => {
                    let budget = ScreenshotBudget {
                        deadline: received + SCREENSHOT_REQUEST_TIMEOUT,
                        client: &shared,
                    };
                    let result = Scaled::parse(&serde_json::from_slice::<Value>(&line)?)
                        .and_then(|scaled| screenshot(&native, scaled, budget));
                    // Not on an encoder owner's connection: its access units keep the default buffer.
                    if result.is_ok() && !encoder_owner(&native, client) {
                        vm_control::widen_send_buffer(&shared);
                    }
                    result
                }
                _ => Err(anyhow!("unsupported console operation")),
            };
            let started = request.method == "start" && result.is_ok();
            let epoch = if [
                "show_window",
                "close_window",
                "window_status",
                "validate_displayed_frame",
            ]
            .contains(&request.method.as_str())
            {
                0
            } else {
                native
                    .owner
                    .lock()
                    .unwrap()
                    .as_ref()
                    .filter(|o| o.client == client)
                    .map_or(0, |o| o.epoch)
            };
            output.deliver(Record::json(
                epoch,
                &match result {
                    Ok(value) => json!({"request_id":request.request_id,"result":value}),
                    Err(error) => rejection_reply(&request.request_id, &error),
                },
            ));
            if started {
                native.acknowledge_start(client);
            }
        }
        Ok(())
    })();
    let _ = native.close(client);
    let _ = shared.shutdown(std::net::Shutdown::Both);
    drop(output);
    let _ = writer.join();
    result
}
fn probe(path: &Path) -> Result<Value> {
    let display = vm_control::backend::probe_native_window(path)?;
    let (status, bytes, _) = vm_control::backend::probe_process(path, "--probe-codecs")?;
    // A codec allocation failure must not disable a verified display-only path.
    let mut value = if status.success() {
        let value: Value =
            serde_json::from_slice(&bytes).context("invalid hardware capability JSON")?;
        if !["verified-hardware", "unavailable"].contains(&value["h264"].as_str().unwrap_or(""))
            || !["verified-hardware", "unavailable"].contains(&value["h265"].as_str().unwrap_or(""))
        {
            bail!("invalid hardware codec capability contract")
        }
        value
    } else if vm_control::backend::window_supported(&display) {
        json!({"h264":"unavailable","h265":"unavailable","source":"hardware probe failed; display-only available"})
    } else {
        bail!("native hardware probe failed")
    };
    value
        .as_object_mut()
        .context("capabilities must be an object")?
        .extend(display.as_object().unwrap().clone());
    value["broker_window_protocol"] = json!(1);
    value["broker_native_input_protocol"] = json!(1);
    value["encoder_fps_request_supported"] = json!(false);
    // This broker forwards and checks the scaled-JPEG screenshot form; whether the helper has it shows in its reply.
    value["broker_screenshot_scaled_protocol"] = json!(1);
    Ok(value)
}

struct SocketPath(PathBuf);
impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn run() -> Result<()> {
    unsafe {
        libc::signal(libc::SIGTERM, stop as *const () as usize);
        libc::signal(libc::SIGINT, stop as *const () as usize);
    }
    let args: Vec<_> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--probe-capabilities" {
        let mut capabilities = vm_control::backend::probe_native_window(&PathBuf::from(&args[2]))?;
        capabilities["broker_window_protocol"] = json!(1);
        capabilities["broker_native_input_protocol"] = json!(1);
        capabilities["encoder_fps_request_supported"] = json!(false);
        capabilities["broker_screenshot_scaled_protocol"] = json!(1);
        println!("{capabilities}");
        return Ok(());
    }
    if args.len() != 4 && args.len() != 10 {
        bail!("usage: vm-console NATIVE_ENCODER SPICE_SOCKET PRIVATE_CONSOLE_SOCKET")
    }
    let native_args = &args[4..];
    if !native_args.is_empty()
        && (native_args[0] != "--control-socket"
            || native_args[2] != "--vm-id"
            || native_args[4] != "--worker-instance"
            || !Path::new(&native_args[1]).is_absolute()
            || !vm_control::native_input::hex(&native_args[5], 64))
    {
        bail!("invalid native controller binding")
    }
    let encoder = PathBuf::from(&args[1]).canonicalize()?;
    let spice = PathBuf::from(&args[2]).canonicalize()?;
    let path = PathBuf::from(&args[3]);
    let parent = path.parent().context("socket parent required")?;
    let meta = fs::symlink_metadata(parent)?;
    if !path.is_absolute()
        || !meta.is_dir()
        || meta.file_type().is_symlink()
        || meta.permissions().mode() & 0o077 != 0
    {
        bail!("console socket requires an existing private directory")
    }
    let listener = UnixListener::bind(&path)?;
    let _path = SocketPath(path.clone());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let capabilities = probe(&encoder)?;
    let mut child = OwnedChild(
        Command::new(&encoder)
            .arg(spice)
            .arg("--persistent")
            .args(native_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let mut pipe = child.0.stdout.take().unwrap();
    use std::os::fd::AsRawFd;
    let input_fd = child.0.stdin.as_ref().unwrap().as_raw_fd();
    if unsafe {
        libc::fcntl(
            input_fd,
            libc::F_SETFL,
            libc::fcntl(input_fd, libc::F_GETFL) | libc::O_NONBLOCK,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let native = Arc::new(Native {
        input: Mutex::new(child.0.stdin.take().unwrap()),
        pending: Mutex::new(BTreeMap::new()),
        next: AtomicU64::new(1),
        alive: AtomicBool::new(true),
        ready: (Mutex::new(false), Condvar::new()),
        owner: Mutex::new(None),
        next_epoch: AtomicU64::new(1),
        snapshot: Mutex::new(()),
        capabilities,
    });
    let receive = native.clone();
    thread::spawn(move || {
        let _ = receive_native(&receive, &mut pipe);
    });
    let until = Instant::now() + Duration::from_secs(12);
    let mut ready = native.ready.0.lock().unwrap();
    while !*ready
        && native.alive.load(Ordering::Acquire)
        && !STOP.load(Ordering::Acquire)
        && Instant::now() < until
    {
        ready = native
            .ready
            .1
            .wait_timeout(ready, Duration::from_millis(50))
            .unwrap()
            .0;
    }
    if !*ready {
        bail!("native capture did not connect before startup deadline or was interrupted")
    }
    drop(ready);
    println!(
        "{}",
        json!({"event":"ready","protocol":1,"console_socket":path,"capture_connected":true,"guest_frame_available":null,"capabilities":native.capabilities})
    );
    std::io::stdout().flush()?;
    // Only this broker's native child is reaped; QEMU lifetime belongs to its worker.
    unsafe {
        libc::signal(libc::SIGTERM, stop as *const () as usize);
        libc::signal(libc::SIGINT, stop as *const () as usize);
    }
    let active = Arc::new(Mutex::new(BTreeMap::<u64, UnixStream>::new()));
    let mut next = 1;
    while !STOP.load(Ordering::Acquire) && native.alive.load(Ordering::Acquire) {
        if child.0.try_wait()?.is_some() {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let mut connections = active.lock().unwrap();
                if connections.len() >= 8 {
                    drop(stream);
                    continue;
                }
                let client = next;
                next += 1;
                connections.insert(client, stream.try_clone()?);
                drop(connections);
                let active = active.clone();
                let native = native.clone();
                thread::spawn(move || {
                    let _ = connection(stream, native, client);
                    active.lock().unwrap().remove(&client);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                use std::os::fd::AsRawFd;
                wait_listener_readable(listener.as_raw_fd(), Duration::from_millis(20))?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    for stream in active.lock().unwrap().values() {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    let expected = STOP.load(Ordering::Acquire);
    if expected && native.alive.load(Ordering::Acquire) {
        let _ = native.call(json!({"method":"shutdown"}));
    }
    let until = Instant::now() + Duration::from_secs(1);
    while child.0.try_wait()?.is_none() && Instant::now() < until {
        thread::sleep(Duration::from_millis(10));
    }
    drop(child);
    if !expected {
        bail!("native capture stopped; broker unavailable")
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("vm-console: {error:#}");
        std::process::exit(1)
    }
}

#[cfg(test)]
mod display_rejection_tests {
    use super::*;
    #[test]
    fn helper_rejection_is_method_scoped_and_does_not_copy_raw_fields() {
        let error =
            json!({"code":"display_history_miss","message":"SECRET raw error","key":"SECRET"});
        let stage = helper_display_rejection(&json!("validate_displayed_frame"), &error).unwrap();
        let reply = rejection_reply("original-request", &stage.into());
        assert_eq!(reply["request_id"], "original-request");
        assert_eq!(reply["display_rejection"], "display_history_miss");
        assert!(!reply.to_string().contains("SECRET"));
        for method in ["window_status", "screenshot", "start"] {
            assert!(helper_display_rejection(&json!(method), &error).is_none());
        }
        for error in [
            json!({"code":"authority"}),
            json!({"code":"unknown","message":"display_history_miss"}),
            json!({"code":{"stage":"display_history_miss"}}),
        ] {
            assert!(helper_display_rejection(&json!("validate_displayed_frame"), &error).is_none());
        }
        assert!(rejection_reply("r", &anyhow!("ordinary transport failure"))
            .get("display_rejection")
            .is_none());
    }
}

#[cfg(test)]
mod listener_readiness_tests {
    use super::*;
    use std::os::fd::AsRawFd;
    #[test]
    fn actual_unix_connections_wake_wait_and_preserve_separate_requests() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("listener.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            for expected in *b"12" {
                assert_eq!(
                    listener.accept().unwrap_err().kind(),
                    std::io::ErrorKind::WouldBlock
                );
                ready_tx.send(()).unwrap();
                // Longer test-only wait makes an unconditional sleep fail clearly;
                // production always uses the unchanged 20 ms maintenance budget.
                assert!(
                    wait_listener_readable(listener.as_raw_fd(), Duration::from_secs(2)).unwrap()
                );
                let (mut accepted, _) = listener.accept().unwrap();
                accepted.set_nonblocking(false).unwrap();
                accepted
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut byte = [0];
                accepted.read_exact(&mut byte).unwrap();
                assert_eq!(byte, [expected]);
                done_tx.send(()).unwrap();
            }
        });
        for byte in *b"12" {
            ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            let mut client = UnixStream::connect(&path).unwrap();
            client.write_all(&[byte]).unwrap();
            done_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("connection must wake poll before its 2-second budget");
        }
        server.join().unwrap();
    }
    #[test]
    fn idle_listener_returns_for_maintenance_without_accepting() {
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("idle.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(!wait_listener_readable(listener.as_raw_fd(), Duration::from_millis(20)).unwrap());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    #[test]
    fn invalid_listener_fd_is_failure_not_readiness() {
        assert!(wait_listener_readable(i32::MAX, Duration::from_millis(20)).is_err());
    }
}

#[cfg(test)]
mod frame_diagnostic_id_tests {
    use super::*;
    #[test]
    fn real_unix_request_keeps_exact_optional_id_through_helper_pipe() {
        for (id, receipt) in [
            (None, None),
            (Some(1), None),
            (Some(9_007_199_254_740_991u64), None),
            (None, Some("a".repeat(32))),
            (Some(9_007_199_254_740_991u64), Some("f".repeat(32))),
        ] {
            let mut child = OwnedChild(
                Command::new("/bin/cat")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let stdout = child.0.stdout.take().unwrap();
            let native = Arc::new(Native {
                input: Mutex::new(child.0.stdin.take().unwrap()),
                pending: Mutex::new(BTreeMap::new()),
                next: AtomicU64::new(1),
                alive: AtomicBool::new(true),
                ready: (Mutex::new(true), Condvar::new()),
                owner: Mutex::new(None),
                next_epoch: AtomicU64::new(1),
                snapshot: Mutex::new(()),
                capabilities: json!({"native_window":true,"native_window_protocol":1,"display_only":true,"input_enabled":false}),
            });
            let sink = native.clone();
            let (mut record_write, mut record_read) = UnixStream::pair().unwrap();
            record_read
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let receiver = thread::spawn(move || receive_native(&sink, &mut record_read));
            let helper_receipt = receipt.clone();
            let helper = thread::spawn(move || {
                let mut line = String::new();
                let mut commands = BufReader::new(stdout);
                commands.read_line(&mut line).unwrap();
                let command: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(command.get("diagnostic_id"), id.map(|n| json!(n)).as_ref());
                assert_eq!(command["frame"].as_object().unwrap().len(), 8);
                assert_eq!(
                    command.get("acquisition_receipt"),
                    helper_receipt.as_ref().map(|s| json!(s)).as_ref()
                );
                if helper_receipt.is_some() {
                    assert_eq!(command["epoch"], u64::MAX);
                    assert_eq!(command["sequence"], u64::MAX - 1);
                } else {
                    assert!(command.get("epoch").is_none() && command.get("sequence").is_none());
                }
                let mut result = json!({"valid":true,"current":true,"frame":command["frame"]});
                if helper_receipt.is_some() {
                    result["acquisition"] = json!({"version":1,"epoch":u64::MAX,"sequence":u64::MAX-1,"remaining_us":123456});
                }
                vm_media_local::console_wire::write_record(
                    &mut record_write,
                    &Record::json(0, &json!({"id":command["id"],"result":result})),
                )
                .unwrap();
                line.clear();
                commands.read_line(&mut line).unwrap();
                let status: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(status["method"], "window_status");
                assert!(status.get("epoch").is_none());
                vm_media_local::console_wire::write_record(&mut record_write, &Record::json(0, &json!({"id":status["id"],"result":{"window_open":true,"native_window":true,"native_window_protocol":1,"display_only":true,"input_enabled":false}}))).unwrap();
                command
            });
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let broker = thread::spawn(move || connection(server, native, 1));
            let mut request = json!({"request_id":"test-frame","method":"validate_displayed_frame","frame":{
                "display_instance":"d".repeat(32),"presentation_serial":1,"window_epoch":1,"invalidation_epoch":0,"source_generation":7,"source_sequence":42,"width":1280,"height":800}});
            if let Some(n) = id {
                request["diagnostic_id"] = json!(n);
            }
            if let Some(receipt) = &receipt {
                request["acquisition_receipt"] = json!(receipt);
                request["epoch"] = json!(u64::MAX);
                request["sequence"] = json!(u64::MAX - 1);
            }
            serde_json::to_writer(&mut client, &request).unwrap();
            client.write_all(b"\n").unwrap();
            let reply = read_record(&mut client).unwrap().value().unwrap();
            assert_eq!(reply["result"]["valid"], true);
            if receipt.is_some() {
                assert_eq!(
                    reply["result"]["acquisition"],
                    json!({"version":1,"epoch":u64::MAX,"sequence":u64::MAX-1,"remaining_us":123456})
                );
            } else {
                assert!(reply["result"].get("acquisition").is_none());
            }
            serde_json::to_writer(
                &mut client,
                &json!({"request_id":"status-after-receipt","method":"window_status"}),
            )
            .unwrap();
            client.write_all(b"\n").unwrap();
            let status = read_record(&mut client).unwrap();
            assert_eq!(status.epoch, 0);
            assert_eq!(status.value().unwrap()["result"]["window_open"], true);
            let command = helper.join().unwrap();
            assert!(receiver.join().unwrap().is_err()); // helper EOF after its framed response
            assert_eq!(command["frame"], request["frame"]);
            drop(client);
            broker.join().unwrap().unwrap();
        }
    }
    #[test]
    fn real_framed_reader_rejects_wrong_display_and_encoder_epochs() {
        for (method, authority_epoch, reply_epoch) in [
            ("validate_displayed_frame", u64::MAX, u64::MAX),
            ("start", 7, 0),
            ("set_bitrate", 7, 8),
        ] {
            let mut child = OwnedChild(
                Command::new("/bin/cat")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let stdout = child.0.stdout.take().unwrap();
            let native = Arc::new(Native {
                input: Mutex::new(child.0.stdin.take().unwrap()),
                pending: Mutex::new(BTreeMap::new()),
                next: AtomicU64::new(1),
                alive: AtomicBool::new(true),
                ready: (Mutex::new(true), Condvar::new()),
                owner: Mutex::new(None),
                next_epoch: AtomicU64::new(1),
                snapshot: Mutex::new(()),
                capabilities: json!({}),
            });
            let (mut writer, mut reader) = UnixStream::pair().unwrap();
            reader
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let receive = native.clone();
            let receiving = thread::spawn(move || receive_native(&receive, &mut reader));
            let helper = thread::spawn(move || {
                let mut line = String::new();
                BufReader::new(stdout).read_line(&mut line).unwrap();
                let command: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(command["epoch"], authority_epoch);
                vm_media_local::console_wire::write_record(
                    &mut writer,
                    &Record::json(
                        reply_epoch,
                        &json!({"id":command["id"],"result":{"valid":true}}),
                    ),
                )
                .unwrap();
            });
            assert!(native
                .call(json!({"method":method,"epoch":authority_epoch}))
                .is_err());
            helper.join().unwrap();
            assert_eq!(
                receiving.join().unwrap().unwrap_err().to_string(),
                "native reply epoch mismatch"
            );
            assert!(!native.alive.load(Ordering::Acquire));
            assert!(native.pending.lock().unwrap().is_empty());
            assert!(native.call(json!({"method":"window_status"})).is_err());
        }
    }
    #[test]
    fn acquisition_incomplete_or_malformed_unix_request_never_reaches_helper() {
        let good = json!({"acquisition_receipt":"a".repeat(32),"epoch":1,"sequence":2});
        let mut cases = vec![
            json!({"epoch":1}),
            json!({"sequence":2}),
            json!({"epoch":null}),
            json!({"acquisition_receipt":"a".repeat(32)}),
            json!({"acquisition_receipt":"a".repeat(32),"epoch":1}),
            json!({"acquisition_receipt":"a".repeat(32),"sequence":2}),
        ];
        for key in ["epoch", "sequence"] {
            for value in [Value::Null, json!(true), json!(1.0), json!("1"), json!(0)] {
                let mut bad = good.clone();
                bad[key] = value;
                cases.push(bad);
            }
        }
        for value in [
            Value::Null,
            json!(true),
            json!(1),
            json!("A".repeat(32)),
            json!("a".repeat(31)),
        ] {
            let mut bad = good.clone();
            bad["acquisition_receipt"] = value;
            cases.push(bad);
        }
        for key in ["version", "encoder_instance", "target_bps"] {
            let mut bad = good.clone();
            bad[key] = Value::Null;
            cases.push(bad);
        }
        for fields in cases {
            let mut child = OwnedChild(
                Command::new("/bin/cat")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let native = Arc::new(Native {
                input: Mutex::new(child.0.stdin.take().unwrap()),
                pending: Mutex::new(BTreeMap::new()),
                next: AtomicU64::new(1),
                alive: AtomicBool::new(true),
                ready: (Mutex::new(true), Condvar::new()),
                owner: Mutex::new(None),
                next_epoch: AtomicU64::new(1),
                snapshot: Mutex::new(()),
                capabilities: json!({}),
            });
            let observed = native.clone();
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let broker = thread::spawn(move || connection(server, native, 1));
            let mut request = json!({"request_id":"bad","method":"validate_displayed_frame","frame":{
                "display_instance":"d".repeat(32),"presentation_serial":1,"window_epoch":1,"invalidation_epoch":0,"source_generation":7,"source_sequence":42,"width":1280,"height":800}});
            request
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            serde_json::to_writer(&mut client, &request).unwrap();
            client.write_all(b"\n").unwrap();
            assert!(broker.join().unwrap().is_err(), "{fields}");
            assert_eq!(
                observed.next.load(Ordering::Acquire),
                1,
                "no helper request allocated: {fields}"
            );
            assert!(observed.pending.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn malformed_present_id_is_not_treated_as_absent() {
        for id in [
            Value::Null,
            json!(true),
            json!(1.0),
            json!(0),
            json!(-1),
            json!("1"),
            json!(9_007_199_254_740_992u64),
            json!({}),
        ] {
            assert!(serde_json::from_value::<Request>(
                json!({"request_id":"x","method":"validate_displayed_frame","diagnostic_id":id})
            )
            .is_err());
        }
    }
}

#[cfg(test)]
mod screenshot_forward_tests {
    use super::*;
    use std::os::fd::AsRawFd;
    fn send_buffer(socket: &UnixStream) -> usize {
        let mut size: libc::c_int = 0;
        let mut length = std::mem::size_of_val(&size) as libc::socklen_t;
        assert_eq!(
            unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&mut size as *mut libc::c_int).cast(),
                    &mut length,
                )
            },
            0
        );
        size as usize
    }
    /// A broker around `/bin/cat` as the command pipe; `helper` is called with each forwarded command and the record
    /// pipe until it returns false.
    fn exchange(
        owner: bool,
        helper: impl FnMut(Value, &mut UnixStream) -> bool + Send + 'static,
    ) -> (Record, usize) {
        exchange_request(
            owner,
            b"{\"request_id\":\"screenshot-7\",\"method\":\"screenshot\"}\n",
            true,
            helper,
        )
    }
    /// `exchange` with the client's request line given. `reaches_helper` is false for a request the broker must
    /// answer by itself: the helper is then never called, and the command pipe is closed to end its reader.
    fn exchange_request(
        owner: bool,
        request: &[u8],
        reaches_helper: bool,
        mut helper: impl FnMut(Value, &mut UnixStream) -> bool + Send + 'static,
    ) -> (Record, usize) {
        let mut child = OwnedChild(
            Command::new("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdout = child.0.stdout.take().unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        let widened = server.try_clone().unwrap();
        let native = Arc::new(Native {
            input: Mutex::new(child.0.stdin.take().unwrap()),
            pending: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            ready: (Mutex::new(true), Condvar::new()),
            owner: Mutex::new(owner.then(|| Owner {
                client: 1,
                epoch: 0,
                output: Output {
                    tx: mpsc::sync_channel(1).0,
                    socket: Arc::new(server.try_clone().unwrap()),
                },
                acknowledged: true,
                native_ended: true,
                early: Vec::new(),
            })),
            next_epoch: AtomicU64::new(1),
            snapshot: Mutex::new(()),
            capabilities: json!({}),
        });
        let sink = native.clone();
        let (mut record_write, mut record_read) = UnixStream::pair().unwrap();
        let receiver = thread::spawn(move || receive_native(&sink, &mut record_read));
        // Helper EOF shuts an encoder owner's socket; keep the pipe open until the reply has been read.
        let (read, hold) = mpsc::channel::<()>();
        let helper = thread::spawn(move || {
            let mut commands = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                if commands.read_line(&mut line).unwrap() == 0 {
                    assert!(!reaches_helper, "command pipe closed before a command");
                    return;
                }
                assert!(
                    reaches_helper,
                    "the broker forwarded a request it had to refuse: {line}"
                );
                if !helper(serde_json::from_str(&line).unwrap(), &mut record_write) {
                    break;
                }
            }
            let _ = hold.recv();
        });
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let broker = thread::spawn(move || connection(server, native, 1));
        client.write_all(request).unwrap();
        let reply = read_record(&mut client).unwrap();
        let buffer = send_buffer(&widened);
        drop(read);
        if !reaches_helper {
            // Nothing was written to the command pipe; ending `cat` ends the helper's read.
            let _ = child.0.kill();
        }
        helper.join().unwrap();
        assert!(receiver.join().unwrap().is_err()); // helper EOF or refused record
        drop(client);
        broker.join().unwrap().unwrap();
        (reply, buffer)
    }
    fn picture(bytes: usize) -> Value {
        // No slash, quote or control byte: the serialized form of the string is the string itself.
        json!({"byte_length":bytes,"captured_monotonic_us":2,"data_base64":"iVBORw0KGgo+".repeat(bytes.div_ceil(9)),"height":1440,"mime_type":"image/png","width":2560})
    }
    #[test]
    fn screenshot_result_is_forwarded_byte_for_byte_in_the_same_record_format() {
        for (bytes, owner) in [(21 * 1024, false), (2_900_000, false), (2_900_000, true)] {
            let sent = picture(bytes);
            let answer = sent.clone();
            let (reply, buffer) = exchange(owner, move |command, records| {
                assert_eq!(command["method"], "screenshot");
                vm_media_local::console_wire::write_record(
                    records,
                    &Record::json(0, &json!({"id":command["id"],"result":answer})),
                )
                .unwrap();
                false
            });
            assert_eq!((reply.kind, reply.epoch), (1, 0));
            let expected =
                serde_json::to_vec(&json!({"request_id":"screenshot-7","result":sent})).unwrap();
            assert!(
                reply.body == expected,
                "forwarded record differs at {bytes} bytes"
            );
            // Only a connection that is not the encoder owner's gets the larger kernel buffer.
            assert_eq!(buffer >= vm_control::LARGE_REPLY_SEND_BUFFER, !owner);
        }
    }
    #[test]
    fn helper_record_over_the_response_bound_is_still_refused_and_fences_capture() {
        let (reply, buffer) = exchange(false, |_, records| {
            let mut header = [0u8; 13];
            let over = vm_media_local::console_wire::MAX_JSON_BODY as u32 + 1;
            header[..4].copy_from_slice(&over.to_be_bytes());
            header[4] = 1;
            records.write_all(&header).unwrap();
            let _ = records.write_all(&[b' '; 65536]);
            false
        });
        assert_eq!(
            reply.value().unwrap(),
            json!({"request_id":"screenshot-7","error":"native operation failed: \"capture process unavailable\""})
        );
        assert!(buffer < vm_control::LARGE_REPLY_SEND_BUFFER);
    }
    fn busy(command: &Value, records: &mut UnixStream) {
        assert_eq!(command["method"], "screenshot");
        vm_media_local::console_wire::write_record(
            records,
            &Record::json(0, &json!({"id":command["id"],"error":{"code":"screenshot_busy","message":"screenshot_busy"}})),
        )
        .unwrap();
    }
    #[test]
    fn helper_busy_after_its_previous_reply_is_waited_out_within_a_bound() {
        let (sent, mut refusals, started) = (picture(21 * 1024), 2, Instant::now());
        let answer = sent.clone();
        let (reply, _) = exchange(false, move |command, records| {
            if refusals > 0 {
                refusals -= 1;
                busy(&command, records);
                return true;
            }
            vm_media_local::console_wire::write_record(
                records,
                &Record::json(0, &json!({"id":command["id"],"result":answer})),
            )
            .unwrap();
            false
        });
        assert_eq!(
            reply.value().unwrap(),
            json!({"request_id":"screenshot-7","result":sent})
        );
        assert!(started.elapsed() >= 2 * HELPER_BUSY_STEP);
        // A helper that stays busy is reported as before, after one request and the bounded retries.
        let mut commands = 0;
        let (reply, _) = exchange(false, move |command, records| {
            commands += 1;
            busy(&command, records);
            commands <= HELPER_BUSY_RETRIES
        });
        assert_eq!(
            reply.value().unwrap(),
            json!({"request_id":"screenshot-7","error":"native operation failed: {\"code\":\"screenshot_busy\",\"message\":\"screenshot_busy\"}"})
        );
        assert!(HELPER_BUSY_RETRIES as u32 * HELPER_BUSY_STEP < Duration::from_millis(20));
    }

    /// SOI, one frame header, padding, EOI: what the dimension check reads. Not a picture.
    fn jpeg(width: u16, height: u16, total: usize) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8, 0xff, 0xc0, 0, 17, 8];
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        let mut left = total - out.len() - 2;
        while left > 0 {
            let mut take = (left - 4).min(60000);
            if (1..4).contains(&(left - take - 4)) {
                take -= 4;
            }
            out.extend_from_slice(&[0xff, 0xfe]);
            out.extend_from_slice(&((take + 2) as u16).to_be_bytes());
            out.extend_from_slice(&vec![b'x'; take]);
            left -= take + 4;
        }
        out.extend_from_slice(&[0xff, 0xd9]);
        assert_eq!(out.len(), total);
        out
    }
    /// The helper's reply to `{format:jpeg,max_width:1280,max_height:720,quality:50}` for a 2560x1440 source.
    fn scaled_picture(frame: (u16, u16), bytes: usize) -> Value {
        use base64::Engine;
        let jpeg = jpeg(frame.0, frame.1, bytes);
        json!({"byte_length":jpeg.len(),"captured_monotonic_us":2,
            "data_base64":base64::engine::general_purpose::STANDARD.encode(&jpeg),
            "frame_height":720,"frame_quality":50,"frame_width":1280,"height":1440,"mime_type":"image/jpeg",
            "scale_divisor":2,"width":2560})
    }
    const SCALED_REQUEST: &[u8] = b"{\"request_id\":\"screenshot-7\",\"method\":\"screenshot\",\"format\":\"jpeg\",\"max_width\":1280,\"max_height\":720,\"quality\":50}\n";
    fn answer(records: &mut UnixStream, command: &Value, reply: Value) {
        let mut reply = reply;
        reply["id"] = command["id"].clone();
        vm_media_local::console_wire::write_record(records, &Record::json(0, &reply)).unwrap();
    }

    #[test]
    fn helper_command_without_the_scaled_form_is_exactly_what_it_was() {
        let sent = picture(21 * 1024);
        let answer_with = sent.clone();
        let (reply, _) = exchange(false, move |command, records| {
            // Two fields: the method and the broker's own request number. The serializer is deterministic, so the
            // same value is the same bytes on the command pipe.
            assert_eq!(command, json!({"id":"c1","method":"screenshot"}));
            answer(records, &command, json!({"result":answer_with}));
            false
        });
        assert_eq!(
            reply.value().unwrap(),
            json!({"request_id":"screenshot-7","result":sent})
        );
    }
    #[test]
    fn scaled_form_is_forwarded_with_its_four_fields_and_the_jpeg_comes_back_byte_for_byte() {
        for bytes in [4096, vm_control::screenshot_scaled::MAX_BYTES] {
            let sent = scaled_picture((1280, 720), bytes);
            let answer_with = sent.clone();
            let (reply, buffer) = exchange_request(
                false,
                SCALED_REQUEST,
                true,
                move |command, records| {
                    // The helper demands exactly these six fields.
                    assert_eq!(
                        command,
                        json!({"id":"c1","method":"screenshot","format":"jpeg","max_width":1280,"max_height":720,"quality":50})
                    );
                    answer(records, &command, json!({"result":answer_with}));
                    false
                },
            );
            let expected =
                serde_json::to_vec(&json!({"request_id":"screenshot-7","result":sent})).unwrap();
            assert!(
                reply.body == expected,
                "forwarded scaled record differs at {bytes} bytes"
            );
            assert!(buffer >= vm_control::LARGE_REPLY_SEND_BUFFER);
        }
        // A helper without the form ignores the fields and answers with its PNG: forwarded as any PNG.
        let sent = picture(2_900_000);
        let answer_with = sent.clone();
        let (reply, _) = exchange_request(false, SCALED_REQUEST, true, move |command, records| {
            answer(records, &command, json!({"result":answer_with}));
            false
        });
        let expected =
            serde_json::to_vec(&json!({"request_id":"screenshot-7","result":sent})).unwrap();
        assert!(
            reply.body == expected,
            "PNG answer to the scaled form was changed"
        );
    }
    #[test]
    fn scaled_reply_that_disagrees_with_the_request_or_itself_is_not_forwarded() {
        let over = vm_control::screenshot_scaled::MAX_BYTES + 1;
        let cases: Vec<(&str, Value)> = vec![
            ("real dimensions", scaled_picture((1280, 722), 4096)),
            ("exceeds the frame bound", scaled_picture((1280, 720), over)),
            ("scale disagrees", {
                let mut value = scaled_picture((1280, 720), 4096);
                value["scale_divisor"] = json!(4);
                value
            }),
            ("exceeds the requested size", {
                let mut value = scaled_picture((2560, 1440), 4096);
                value["scale_divisor"] = json!(1);
                value["frame_width"] = json!(2560);
                value["frame_height"] = json!(1440);
                value
            }),
            ("quality is not the requested one", {
                let mut value = scaled_picture((1280, 720), 4096);
                value["frame_quality"] = json!(80);
                value
            }),
            ("size mismatch", {
                let mut value = scaled_picture((1280, 720), 4096);
                value["byte_length"] = json!(4095);
                value
            }),
            ("not a JPEG", {
                let mut value = scaled_picture((1280, 720), 4096);
                value["mime_type"] = json!("image/webp");
                value
            }),
        ];
        for (expected, sent) in cases {
            let (reply, _) =
                exchange_request(false, SCALED_REQUEST, true, move |command, records| {
                    answer(records, &command, json!({"result":sent}));
                    false
                });
            let reply = reply.value().unwrap();
            let error = reply["error"]
                .as_str()
                .unwrap_or_else(|| panic!("{expected}: forwarded"));
            assert!(
                error.starts_with("native scaled screenshot invalid"),
                "{expected}: {error}"
            );
            assert_eq!(
                reply.as_object().unwrap().len(),
                2,
                "{expected}: request_id and error only"
            );
        }
    }
    #[test]
    fn helper_refusals_of_the_scaled_form_keep_a_fixed_word_and_other_failures_do_not() {
        let size_limit = json!({"code":"screenshot_unavailable_or_too_large","message":"screenshot_unavailable_or_too_large","diagnostic":{"version":1,"stage":"encoded_size_limit","encoded_bytes":null,"maximum_encoded_bytes":716800}});
        let code = |code: &str| json!({"code":code,"message":code});
        for (error, word) in [
            (size_limit.clone(), Some("encoded_size_limit")),
            (code("invalid_screenshot_request"), Some("invalid_request")),
            (
                code("screenshot_format_unavailable"),
                Some("format_unavailable"),
            ),
            (
                code("screenshot_scale_unavailable"),
                Some("scale_unavailable"),
            ),
            (code("frame_unavailable"), None),
            (
                json!({"code":"screenshot_unavailable_or_too_large","message":"x","diagnostic":{"version":1,"stage":"jpeg_finalize_failed"}}),
                None,
            ),
        ] {
            let sent = error.clone();
            let (reply, _) =
                exchange_request(false, SCALED_REQUEST, true, move |command, records| {
                    answer(records, &command, json!({"error":sent}));
                    false
                });
            let mut expected = json!({"request_id":"screenshot-7","error":format!("native operation failed: {error}")});
            if let Some(word) = word {
                expected["screenshot_refusal"] = json!(word);
            }
            assert_eq!(reply.value().unwrap(), expected);
        }
        // The same helper error to a request without the form: the reply it always was, one error field.
        let sent = size_limit.clone();
        let (reply, _) = exchange(false, move |command, records| {
            answer(records, &command, json!({"error":sent}));
            false
        });
        assert_eq!(
            reply.value().unwrap(),
            json!({"request_id":"screenshot-7","error":format!("native operation failed: {size_limit}")})
        );
    }
    #[test]
    fn partial_or_out_of_range_scaled_request_is_refused_by_the_broker_and_never_reaches_the_helper(
    ) {
        for fields in [
            r#""format":"jpeg""#,
            r#""max_width":1280"#,
            r#""quality":50"#,
            r#""format":"jpeg","max_width":1280,"max_height":720"#,
            r#""format":"png","max_width":1280,"max_height":720,"quality":50"#,
            r#""format":"jpeg","max_width":3841,"max_height":720,"quality":50"#,
            r#""format":"jpeg","max_width":1280,"max_height":1,"quality":50"#,
            r#""format":"jpeg","max_width":1280,"max_height":720,"quality":0"#,
            r#""format":"jpeg","max_width":1280,"max_height":720,"quality":101"#,
            r#""format":"jpeg","max_width":1280.0,"max_height":720,"quality":50"#,
            r#""format":"jpeg","max_width":"1280","max_height":720,"quality":50"#,
            r#""format":"jpeg","max_width":1280,"max_height":720,"quality":null"#,
        ] {
            let request =
                format!("{{\"request_id\":\"screenshot-7\",\"method\":\"screenshot\",{fields}}}\n");
            let (reply, buffer) = exchange_request(false, request.as_bytes(), false, |_, _| false);
            let reply = reply.value().unwrap();
            assert_eq!(reply["screenshot_refusal"], "invalid_request", "{fields}");
            assert!(
                reply["error"]
                    .as_str()
                    .unwrap()
                    .starts_with("invalid scaled screenshot request"),
                "{fields}: {reply}"
            );
            assert!(buffer < vm_control::LARGE_REPLY_SEND_BUFFER);
        }
    }
    #[test]
    fn scaled_fields_on_any_other_method_end_the_connection() {
        // As any unknown field always did: the request is not acted on and nothing is answered.
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut child = OwnedChild(
            Command::new("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let native = Arc::new(Native {
            input: Mutex::new(child.0.stdin.take().unwrap()),
            pending: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            ready: (Mutex::new(true), Condvar::new()),
            owner: Mutex::new(None),
            next_epoch: AtomicU64::new(1),
            snapshot: Mutex::new(()),
            capabilities: json!({}),
        });
        // Set while the peer is open: Darwin refuses a read timeout on a socket whose peer has closed (EINVAL).
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client
            .write_all(b"{\"request_id\":\"s\",\"method\":\"status\",\"quality\":50}\n")
            .unwrap();
        let error = connection(server, native, 1).unwrap_err();
        assert_eq!(error.to_string(), "unexpected screenshot fields");
        assert!(read_record(&mut client).is_err(), "no record follows");
    }
}

#[cfg(test)]
mod screenshot_deadline_tests {
    use super::*;
    use std::cell::Cell;

    fn busy() -> Result<Value> {
        Err(HelperBusy("screenshot_busy".into()).into())
    }

    #[test]
    fn fast_busy_then_success_keeps_four_retries_and_four_ms_steps() {
        let snapshot = Mutex::new(());
        let start = Instant::now();
        let clock = Cell::new(start);
        let deadline = start + SCREENSHOT_REQUEST_TIMEOUT;
        let mut attempts = 0;
        let mut sleeps = 0;
        let result = screenshot_retry(
            &snapshot,
            deadline,
            |until| {
                assert_eq!(until, deadline);
                assert!(snapshot.try_lock().is_err());
                attempts += 1;
                if attempts <= HELPER_BUSY_RETRIES {
                    busy()
                } else {
                    Ok(json!({"captured":true}))
                }
            },
            || clock.get(),
            |step| {
                assert_eq!(step, Duration::from_millis(4));
                sleeps += 1;
                clock.set(clock.get() + step);
            },
            || false,
        );
        assert_eq!(result.unwrap(), json!({"captured":true}));
        assert_eq!(attempts, 5);
        assert_eq!(sleeps, 4);
        assert!(snapshot.try_lock().is_ok());
    }

    #[test]
    fn busy_until_overall_deadline_returns_typed_busy_and_releases_snapshot() {
        let snapshot = Mutex::new(());
        let start = Instant::now();
        let clock = Cell::new(start);
        let deadline = start + SCREENSHOT_REQUEST_TIMEOUT;
        let mut attempts = 0;
        let result = screenshot_retry(
            &snapshot,
            deadline,
            |until| {
                assert_eq!(until, deadline);
                assert!(snapshot.try_lock().is_err());
                attempts += 1;
                clock.set((clock.get() + Duration::from_secs(11)).min(until));
                busy()
            },
            || clock.get(),
            |step| clock.set(clock.get() + step),
            || false,
        );
        assert!(result.unwrap_err().is::<HelperBusy>());
        assert_eq!(attempts, 2);
        assert_eq!(clock.get(), deadline);
        assert!(snapshot.try_lock().is_ok());
    }

    #[test]
    fn attempt_consuming_whole_budget_starts_no_retry() {
        let snapshot = Mutex::new(());
        let start = Instant::now();
        let clock = Cell::new(start);
        let deadline = start + SCREENSHOT_REQUEST_TIMEOUT;
        let mut attempts = 0;
        let result = screenshot_retry(
            &snapshot,
            deadline,
            |until| {
                attempts += 1;
                clock.set(until);
                busy()
            },
            || clock.get(),
            |_| panic!("no time to sleep or retry"),
            || false,
        );
        assert!(result.unwrap_err().is::<HelperBusy>());
        assert_eq!(attempts, 1);
        assert!(snapshot.try_lock().is_ok());
    }

    #[test]
    fn retry_requires_sleep_plus_minimal_useful_attempt() {
        for remaining in [
            HELPER_BUSY_STEP,
            HELPER_BUSY_STEP + HELPER_BUSY_MIN_ATTEMPT - Duration::from_nanos(1),
            HELPER_BUSY_STEP + HELPER_BUSY_MIN_ATTEMPT,
        ] {
            let snapshot = Mutex::new(());
            let start = Instant::now();
            let clock = Cell::new(start);
            let deadline = start + SCREENSHOT_REQUEST_TIMEOUT;
            let mut attempts = 0;
            let result = screenshot_retry(
                &snapshot,
                deadline,
                |_| {
                    attempts += 1;
                    if attempts == 1 {
                        clock.set(deadline - remaining);
                        busy()
                    } else {
                        Ok(json!({}))
                    }
                },
                || clock.get(),
                |step| clock.set(clock.get() + step),
                || false,
            );
            let can_retry = remaining == HELPER_BUSY_STEP + HELPER_BUSY_MIN_ATTEMPT;
            assert_eq!(result.is_ok(), can_retry);
            assert_eq!(attempts, if can_retry { 2 } else { 1 });
        }
    }

    #[test]
    fn cancellation_or_sleep_overshoot_stops_retry_and_releases_snapshot() {
        for cancel in [false, true] {
            let snapshot = Mutex::new(());
            let start = Instant::now();
            let deadline = start + SCREENSHOT_REQUEST_TIMEOUT;
            let clock = Cell::new(start);
            let disconnected = Cell::new(false);
            let mut attempts = 0;
            let result = screenshot_retry(
                &snapshot,
                deadline,
                |_| {
                    attempts += 1;
                    busy()
                },
                || clock.get(),
                |_| {
                    if cancel {
                        disconnected.set(true);
                    } else {
                        clock.set(deadline);
                    }
                },
                || disconnected.get(),
            );
            assert!(result.unwrap_err().is::<HelperBusy>());
            assert_eq!(attempts, 1);
            assert!(snapshot.try_lock().is_ok());
        }
    }

    fn native(child: &mut OwnedChild) -> Native {
        Native {
            input: Mutex::new(child.0.stdin.take().unwrap()),
            pending: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            ready: (Mutex::new(true), Condvar::new()),
            owner: Mutex::new(None),
            next_epoch: AtomicU64::new(1),
            snapshot: Mutex::new(()),
            capabilities: json!({}),
        }
    }

    fn wait_pending(native: &Native, count: usize) {
        let until = Instant::now() + Duration::from_secs(1);
        while native.pending.lock().unwrap().len() != count {
            assert!(Instant::now() < until, "native request did not reach send");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn interrupted_before_write_preserves_other_connection(cancel: bool) {
        let mut child = OwnedChild(
            Command::new("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let native = Arc::new(native(&mut child));
        let stdout = child.0.stdout.take().unwrap();
        // Hold the command mutex so both connections must wait before writing.
        let input = native.input.lock().unwrap();
        let (mut other_client, other_server) = UnixStream::pair().unwrap();
        other_client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let other_native = native.clone();
        let other = thread::spawn(move || connection(other_server, other_native, 1));
        other_client
            .write_all(b"{\"request_id\":\"other\",\"method\":\"frame_status\"}\n")
            .unwrap();
        wait_pending(&native, 1);

        let (client, server) = UnixStream::pair().unwrap();
        let waiting_native = native.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let waiting = thread::spawn(move || {
            done_tx
                .send(screenshot(
                    &waiting_native,
                    None,
                    ScreenshotBudget {
                        deadline: Instant::now()
                            + if cancel {
                                SCREENSHOT_REQUEST_TIMEOUT
                            } else {
                                Duration::from_millis(200)
                            },
                        client: &server,
                    },
                ))
                .unwrap();
        });
        wait_pending(&native, 2);
        assert!(native.snapshot.try_lock().is_err());
        if cancel {
            client.shutdown(std::net::Shutdown::Both).unwrap();
        }
        let error = done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err();
        waiting.join().unwrap();
        assert_eq!(
            error.downcast_ref::<ScreenshotNotSent>().unwrap().0,
            if cancel {
                ScreenshotInterrupted::Cancelled
            } else {
                ScreenshotInterrupted::Deadline
            }
        );
        assert!(native.alive.load(Ordering::Acquire));
        assert!(native.snapshot.try_lock().is_ok());
        assert_eq!(
            native
                .pending
                .lock()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["c1"]
        );
        assert!(!client_disconnected(&other_client));

        let helper_native = native.clone();
        let helper = thread::spawn(move || {
            let mut commands = BufReader::new(stdout);
            for (id, method, result) in [
                ("c1", "frame_status", json!({"connected":true})),
                ("c3", "screenshot", json!({"captured":true})),
            ] {
                let mut line = String::new();
                commands.read_line(&mut line).unwrap();
                let command: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(command["id"], id);
                assert_eq!(command["method"], method);
                let (_, tx) = helper_native.pending.lock().unwrap().remove(id).unwrap();
                tx.send(json!({"id":id,"result":result})).unwrap();
            }
        });
        drop(input);
        assert_eq!(
            read_record(&mut other_client).unwrap().value().unwrap(),
            json!({"request_id":"other","result":{"connected":true}})
        );
        other_client
            .write_all(b"{\"request_id\":\"next\",\"method\":\"screenshot\"}\n")
            .unwrap();
        assert_eq!(
            read_record(&mut other_client).unwrap().value().unwrap(),
            json!({"request_id":"next","result":{"captured":true}})
        );
        assert!(native.alive.load(Ordering::Acquire));
        assert!(native.pending.lock().unwrap().is_empty());
        assert!(native.snapshot.try_lock().is_ok());
        helper.join().unwrap();
        drop(other_client);
        other.join().unwrap().unwrap();
    }

    #[test]
    fn disconnect_before_write_preserves_other_connection_and_next_screenshot() {
        interrupted_before_write_preserves_other_connection(true);
    }

    #[test]
    fn deadline_before_write_preserves_other_connection_and_next_screenshot() {
        interrupted_before_write_preserves_other_connection(false);
    }

    struct PartialWrite<'a> {
        input: &'a mut ChildStdin,
        cancel: Option<&'a UnixStream>,
        written: usize,
    }
    impl Write for PartialWrite<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.written != 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
            let n = self.input.write(&bytes[..1])?;
            self.written += n;
            if let Some(client) = self.cancel {
                client.shutdown(std::net::Shutdown::Both)?;
            }
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.input.flush()
        }
    }
    impl std::os::fd::AsRawFd for PartialWrite<'_> {
        fn as_raw_fd(&self) -> std::os::fd::RawFd {
            self.input.as_raw_fd()
        }
    }

    fn partial_write_fences(cancel: bool) {
        let mut child = OwnedChild(
            Command::new("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let native = native(&mut child);
        let (client, server) = UnixStream::pair().unwrap();
        let budget = ScreenshotBudget {
            deadline: Instant::now() + SCREENSHOT_REQUEST_TIMEOUT,
            client: &server,
        };
        let error = screenshot_retry(
            &native.snapshot,
            budget.deadline,
            |_| {
                native.call_with_writer(
                    json!({"method":"screenshot"}),
                    Some(&budget),
                    |input, command, budget| {
                        let mut writer = PartialWrite {
                            input,
                            cancel: cancel.then_some(&client),
                            written: 0,
                        };
                        let result = native_write(&mut writer, command, budget);
                        assert_eq!(writer.written, 1);
                        result
                    },
                )
            },
            Instant::now,
            thread::sleep,
            || client_disconnected(&server),
        )
        .unwrap_err();
        assert!(!error.is::<ScreenshotNotSent>());
        if cancel {
            assert_eq!(
                error.downcast_ref::<ScreenshotInterrupted>(),
                Some(&ScreenshotInterrupted::Cancelled)
            );
        } else {
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::BrokenPipe
            );
        }
        assert!(!native.alive.load(Ordering::Acquire));
        assert!(native.pending.lock().unwrap().is_empty());
        assert!(native.snapshot.try_lock().is_ok());
        let mut prefix = [0];
        child
            .0
            .stdout
            .as_mut()
            .unwrap()
            .read_exact(&mut prefix)
            .unwrap();
        assert_eq!(&prefix, b"{");
    }

    #[test]
    fn cancellation_after_partial_write_fences_native() {
        partial_write_fences(true);
    }

    #[test]
    fn broken_pipe_after_partial_write_fences_native() {
        partial_write_fences(false);
    }

    #[test]
    fn native_receive_uses_remaining_budget_and_still_fences_timeout() {
        let mut child = OwnedChild(
            Command::new("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let native = native(&mut child);
        let (_client, server) = UnixStream::pair().unwrap();
        let started = Instant::now();
        let result = screenshot(
            &native,
            None,
            ScreenshotBudget {
                deadline: started + Duration::from_millis(30),
                client: &server,
            },
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "native response deadline exceeded; capture owner fenced"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(native.next.load(Ordering::Relaxed), 2);
        assert!(native.pending.lock().unwrap().is_empty());
        assert!(!native.alive.load(Ordering::Acquire));
        assert!(native.snapshot.try_lock().is_ok());
    }

    #[test]
    fn disconnected_client_interrupts_native_wait_and_releases_snapshot() {
        let mut child = OwnedChild(
            Command::new("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let native = native(&mut child);
        let stdout = child.0.stdout.take().unwrap();
        let (client, server) = UnixStream::pair().unwrap();
        let cancel = thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).unwrap();
            assert!(line.contains("screenshot"));
            client.shutdown(std::net::Shutdown::Both).unwrap();
        });
        let started = Instant::now();
        let result = screenshot(
            &native,
            None,
            ScreenshotBudget {
                deadline: started + SCREENSHOT_REQUEST_TIMEOUT,
                client: &server,
            },
        );
        cancel.join().unwrap();
        assert_eq!(
            result.unwrap_err().to_string(),
            "screenshot client disconnected"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(native.next.load(Ordering::Relaxed), 2);
        assert!(native.pending.lock().unwrap().is_empty());
        assert!(native.alive.load(Ordering::Acquire));
        assert!(native.snapshot.try_lock().is_ok());
    }

    #[test]
    fn disconnect_probe_preserves_queued_request_bytes() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        assert!(!client_disconnected(&server));
        client.write_all(b"next\n").unwrap();
        assert!(!client_disconnected(&server));
        let mut bytes = [0; 5];
        server.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"next\n");
        client.write_all(b"queued\n").unwrap();
        client.shutdown(std::net::Shutdown::Both).unwrap();
        assert!(client_disconnected(&server));
    }
}
