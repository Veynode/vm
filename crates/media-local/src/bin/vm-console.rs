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
fn native_write(input: &mut ChildStdin, value: &Value) -> Result<()> {
    use std::os::fd::AsRawFd;
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if bytes.len() > 1024 {
        bail!("native command exceeds bound")
    }
    let until = Instant::now() + Duration::from_millis(500);
    let mut offset = 0;
    while offset < bytes.len() {
        match input.write(&bytes[offset..]) {
            Ok(0) => bail!("native pipe closed"),
            Ok(n) => offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if STOP.load(Ordering::Acquire) {
                    bail!("broker interrupted during capability probe")
                }
                if Instant::now() >= until {
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
    fn call(&self, mut command: Value) -> Result<Value> {
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
        let send = {
            let mut input = self.input.lock().unwrap();
            native_write(&mut input, &command)
        };
        if let Err(error) = send {
            self.pending.lock().unwrap().remove(&id);
            self.alive.store(false, Ordering::Release);
            return Err(error);
        }
        let reply = rx.recv_timeout(Duration::from_secs(if command["method"] == "shutdown" {
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
        }));
        self.pending.lock().unwrap().remove(&id);
        match reply {
            Ok(value) => {
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
                    bail!("native operation failed: {error}")
                }
                match value.get("result").filter(|result| result.is_object()) {
                    Some(result) => Ok(result.clone()),
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
    reply
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
                "screenshot" => match native.snapshot.try_lock() {
                    Ok(_guard) => native.call(json!({"method":"screenshot"})),
                    Err(_) => Err(anyhow!("screenshot busy")),
                },
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
