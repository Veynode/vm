//! One independently supervised QEMU/HVF instance. Protocol output is JSON only.
#[path = "../qemu_deadline_io.rs"]
mod deadline_io;
#[path = "../qemu_guardian.rs"]
mod guardian;
#[path = "../qemu_guest_exec.rs"]
mod guest_exec;
#[path = "../qemu_guest_transport.rs"]
mod guest_transport;
#[path = "../qemu_native_input.rs"]
mod native_input_worker;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::OpenOptionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use vm_control::{backend::Backend, read_frame, Config};
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop_signal(_: i32) {
    STOP.store(true, Ordering::Release);
}
fn emit(value: Value) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &value)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
fn publish_startup_then_resume(
    qmp: &mut Qmp,
    state: &mut Value,
    paused_launch: bool,
    mut publish: impl FnMut(Value) -> Result<()>,
) -> Result<()> {
    // Verified metadata is published before cont. Do not query after cont to
    // establish it: a fast Guest can terminate and close QMP immediately.
    if paused_launch {
        state["state"] = json!("starting");
    }
    publish(state.clone())?;
    if paused_launch {
        qmp.call("cont")?;
        state["state"] = json!("running");
        state["qmp_status"] = json!("running");
        publish(state.clone())?;
    }
    Ok(())
}
struct RuntimeDirectory(PathBuf);
impl RuntimeDirectory {
    fn new() -> Result<Self> {
        let mut random = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = PathBuf::from(format!("/tmp/veynode-qemu-{suffix}"));
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for RuntimeDirectory {
    fn drop(&mut self) {
        // Never recursively remove an externally provided location.
        for name in ["qmp.sock", "spice.sock", "console.sock", "agent.sock"] {
            let _ = fs::remove_file(self.0.join(name));
        }
        let _ = fs::remove_dir(&self.0);
    }
}
struct QemuChild(Child, Option<guardian::Parent>);
impl Drop for QemuChild {
    fn drop(&mut self) {
        if let Some(guardian) = &mut self.1 {
            guardian.close();
            let until = Instant::now() + Duration::from_secs(6);
            while self.0.try_wait().ok().flatten().is_none() && Instant::now() < until {
                thread::sleep(Duration::from_millis(20));
            }
            // Never kill the guardian alone: it owns the unreaped QEMU child.
            // A stalled guardian is unconfirmed; daemon worker-group cleanup remains fail-closed.
            return;
        }
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
struct Qmp {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
    poisoned: bool,
    reply_prefix: Vec<u8>,
}
impl Qmp {
    fn connect(path: &Path, child: &mut Child) -> Result<Self> {
        let deadline = Instant::now() + Duration::from_secs(12);
        let stream = loop {
            if STOP.load(Ordering::Acquire) {
                bail!("worker interrupted during QEMU startup")
            }
            if let Some(status) = child.try_wait()? {
                bail!("QEMU exited during startup: {status}")
            }
            if let Ok(stream) = UnixStream::connect(path) {
                break stream;
            }
            if Instant::now() >= deadline {
                bail!("QMP startup timed out after 12 seconds")
            }
            thread::sleep(Duration::from_millis(50));
        };
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let mut qmp = Self {
            reader: BufReader::new(stream.try_clone()?),
            stream,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
        };
        let greeting: Value =
            serde_json::from_slice(&read_frame(&mut qmp.reader)?.context("missing QMP greeting")?)?;
        if !greeting["QMP"].is_object() {
            bail!("invalid QMP greeting")
        }
        qmp.call("qmp_capabilities")?;
        Ok(qmp)
    }
    fn call(&mut self, method: &str) -> Result<Value> {
        self.arguments(method, json!({}))
    }
    fn arguments(&mut self, method: &str, arguments: Value) -> Result<Value> {
        // Cleanup is also a framed write: a partial release must never permit
        // another JSON record to be appended. Its independent budget remains
        // three seconds and is never inherited from an expired input receipt.
        self.arguments_before(method, arguments, Instant::now() + Duration::from_secs(3))
    }
    fn ensure_usable(&self) -> Result<()> {
        if self.poisoned {
            bail!("QMP framing uncertain; input cleanup unconfirmed")
        }
        Ok(())
    }
    fn arguments_before(
        &mut self,
        method: &str,
        arguments: Value,
        deadline: Instant,
    ) -> Result<Value> {
        self.ensure_usable()?;
        let number = self.next;
        let mut bytes =
            serde_json::to_vec(&json!({"execute":method,"id":number,"arguments":arguments}))?;
        if bytes.len() >= vm_control::MAX_MESSAGE {
            bail!("QMP input record bound")
        }
        bytes.push(b'\n');
        self.next += 1;
        self.write_record_before(&bytes, deadline)?;
        self.reply(number, method)
    }
    fn write_record_before(&mut self, bytes: &[u8], deadline: Instant) -> Result<()> {
        self.ensure_usable()?;
        let write = deadline_io::write(&mut self.stream, bytes, deadline);
        if write.written > 0 && write.written < bytes.len() {
            self.poisoned = true;
        }
        // BufReader shares this descriptor's nonblocking flag. Restore even on failure.
        if let Err(error) = self.stream.set_nonblocking(false) {
            self.poisoned = true;
            return Err(error.into());
        }
        write.result
    }
    fn reply(&mut self, number: u64, method: &str) -> Result<Value> {
        // ACK waiting does not admit new input or renew acquisition eligibility.
        // Event storms cannot extend one request forever.
        let deadline = Instant::now() + Duration::from_secs(3);
        for _ in 0..128 {
            let value: Value = serde_json::from_slice(&self.line_before(deadline)?)?;
            if Instant::now() >= deadline {
                bail!("QMP response deadline expired after decoding")
            }
            if value["id"] == number {
                if let Some(error) = value.get("error") {
                    bail!("QMP {method} failed: {error}")
                }
                return value
                    .get("return")
                    .cloned()
                    .context("QMP response has no return value");
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        bail!("QMP response deadline or event bound exceeded")
    }
    fn line_before(&mut self, deadline: Instant) -> Result<Vec<u8>> {
        self.ensure_usable()?;
        if let Err(error) = self.stream.set_nonblocking(true) {
            self.poisoned = true;
            return Err(error.into());
        }
        let result = self.line_nonblocking_before(deadline);
        if let Err(error) = self.stream.set_nonblocking(false) {
            self.poisoned = true;
            return Err(error.into());
        }
        result
    }
    fn line_nonblocking_before(&mut self, deadline: Instant) -> Result<Vec<u8>> {
        use std::os::fd::AsRawFd;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .context("QMP response deadline expired")?;
            if self.reader.buffer().is_empty() {
                let mut pollfd = libc::pollfd {
                    fd: self.stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe {
                    libc::poll(
                        &mut pollfd,
                        1,
                        remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
                    )
                };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    self.poisoned = true;
                    return Err(error.into());
                }
                if ready == 0 {
                    continue;
                }
                if Instant::now() >= deadline {
                    bail!("QMP response deadline expired")
                }
            }
            let available = match self.reader.fill_buf() {
                Ok(bytes) if !bytes.is_empty() => bytes,
                Ok(_) => {
                    self.poisoned = true;
                    bail!("QMP disconnected; input cleanup unconfirmed")
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    // Readiness can race. Keep the exact prefix and poll again
                    // against the same deadline, never a fresh per-byte timeout.
                    continue;
                }
                Err(error) => {
                    self.poisoned = true;
                    return Err(error.into());
                }
            };
            // If completion is observed late, leave even newly buffered bytes
            // untouched for the next bounded reply operation.
            if Instant::now() >= deadline {
                bail!("QMP response deadline expired")
            }
            let end = available.iter().position(|b| *b == b'\n');
            let count = end.map_or(available.len(), |n| n + 1);
            if count > vm_control::MAX_MESSAGE.saturating_sub(self.reply_prefix.len()) {
                self.poisoned = true;
                bail!("QMP response bound exceeded; input cleanup unconfirmed")
            }
            self.reply_prefix.extend_from_slice(&available[..count]);
            self.reader.consume(count);
            if end.is_some() {
                return Ok(std::mem::take(&mut self.reply_prefix));
            }
        }
    }
}
#[derive(Default)]
struct InputState {
    revoked_through: u64,
    active_epoch: Option<u64>,
    completed_operations: std::collections::VecDeque<String>,
    operation: Option<String>,
    until: Option<Instant>,
    held: std::collections::BTreeMap<String, Value>,
}
impl InputState {
    fn cleanup(&mut self, qmp: &mut Qmp) -> Result<()> {
        qmp.ensure_usable()?;
        if !self.held.is_empty() {
            qmp.arguments(
                "input-send-event",
                json!({"events":self.held.values().collect::<Vec<_>>()}),
            )?;
        }
        self.held.clear();
        if let Some(operation) = self.operation.take() {
            self.completed_operations.push_back(operation);
            while self.completed_operations.len() > 64 {
                self.completed_operations.pop_front();
            }
        }
        self.active_epoch = None;
        self.until = None;
        Ok(())
    }
    fn cleanup_operation(&mut self, args: &Value, qmp: &mut Qmp) -> Result<Value> {
        let operation = args["operation_id"]
            .as_str()
            .context("missing input operation")?;
        if self.operation.as_deref() == Some(operation) {
            self.cleanup(qmp)?;
            return Ok(json!({"released":true,"synthetic_release_dispatched":true}));
        }
        if self
            .completed_operations
            .iter()
            .any(|known| known == operation)
        {
            return Ok(
                json!({"released":true,"synthetic_release_dispatched":true,"already_released":true}),
            );
        }
        bail!("unknown input operation; cleanup is unconfirmed")
    }
    fn revoke(&mut self, args: &Value, qmp: &mut Qmp) -> Result<Value> {
        let through = args["through_epoch"]
            .as_u64()
            .filter(|epoch| *epoch > 0)
            .context("positive revoke epoch required")?;
        // Fence before any fallible I/O. Failure retains held-state for retry.
        self.revoked_through = self.revoked_through.max(through);
        if self.active_epoch.is_some_and(|epoch| epoch <= through) {
            self.cleanup(qmp)?;
        }
        Ok(
            json!({"released":true,"synthetic_release_dispatched":true,"through_epoch":through,"guest_delivery_confirmed":false}),
        )
    }
    fn send_events(&mut self, events: &[Value], qmp: &mut Qmp) -> Result<()> {
        self.send_events_before(events, qmp, None)
    }
    fn send_events_before(
        &mut self,
        events: &[Value],
        qmp: &mut Qmp,
        deadline: Option<Instant>,
    ) -> Result<()> {
        let mut releases = Vec::new();
        for event in events {
            if ["key", "btn"].contains(&event["type"].as_str().unwrap_or("")) {
                let identity = if event["type"] == "key" {
                    format!("key:{}", event["data"]["key"])
                } else {
                    format!("btn:{}", event["data"]["button"])
                };
                if event["data"]["down"] == true {
                    let mut release = event.clone();
                    release["data"]["down"] = json!(false);
                    self.held.insert(identity, release);
                } else {
                    releases.push(identity);
                }
            }
        }
        if let Some(deadline) = deadline {
            qmp.arguments_before("input-send-event", json!({"events":events}), deadline)?;
        } else {
            qmp.arguments("input-send-event", json!({"events":events}))?;
        }
        for identity in releases {
            self.held.remove(&identity);
        }
        Ok(())
    }
    fn step(
        &mut self,
        args: &Value,
        frames: &vm_control::input::Frames,
        console: &Path,
        qmp: &mut Qmp,
    ) -> Result<Value> {
        let epoch = args["epoch"]
            .as_u64()
            .filter(|epoch| *epoch > 0)
            .context("positive checked input epoch required")?;
        if epoch <= self.revoked_through {
            bail!("input epoch has been revoked")
        }
        if self.active_epoch.is_some_and(|active| active != epoch) {
            bail!("input operation belongs to another epoch")
        }
        let operation = args["operation_id"]
            .as_str()
            .context("missing input operation")?;
        if operation.len() != 32 || !operation.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!("invalid input operation")
        }
        if args["begin"] == true {
            if self.operation.is_some() {
                bail!("input busy: another operation is active")
            }
        } else if self.operation.as_deref() != Some(operation) {
            bail!("input operation expired or replaced")
        }
        let current = console_frame_status(console)?;
        frames.check(args, &current, Instant::now())?;
        if self.until.is_some_and(|until| Instant::now() >= until) {
            bail!("input operation deadline expired")
        }
        let events = args["events"].as_array().context("events missing")?;
        if events.is_empty() || events.len() > 16 {
            bail!("input event bound exceeded")
        }
        if args["begin"] == true {
            if self
                .completed_operations
                .iter()
                .any(|known| known == operation)
            {
                bail!("input operation was already completed")
            }
            self.operation = Some(operation.into());
            self.active_epoch = Some(epoch);
            self.until = Some(Instant::now() + Duration::from_secs(4));
        }
        self.send_events(events, qmp)?;
        Ok(
            json!({"dispatched":true,"guest_delivery_confirmed":false,"source_generation":current["source_generation"]}),
        )
    }
}
fn log_pipe(mut pipe: impl Read + Send + 'static, path: Option<PathBuf>) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mut output = if let Some(path) = path {
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            bail!("diagnostic log must be owner-private, regular, and single-link")
        }
        Some(file)
    } else {
        None
    };
    let mut remaining = output
        .as_ref()
        .map(|f| {
            f.metadata()
                .map(|m| (8u64 * 1024 * 1024).saturating_sub(m.len()) as usize)
        })
        .transpose()?
        .unwrap_or(0);
    thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut buffer) {
            if n == 0 {
                break;
            }
            let keep = n.min(remaining);
            if keep > 0 {
                if output.as_mut().unwrap().write_all(&buffer[..keep]).is_err() {
                    remaining = 0;
                } else {
                    remaining -= keep;
                }
            }
            // Guest serial logging is opt-in. Always drain disabled/full/failed logs.
        }
    });
    Ok(())
}
struct Capture {
    child: Option<QemuChild>,
    socket: Option<UnixStream>,
    receiver: mpsc::Receiver<Result<Value>>,
    deadline: Instant,
    id: Value,
}
impl Capture {
    fn start(helper: &Path, socket: &Path, id: Value) -> Result<Self> {
        let mut child = QemuChild(
            Command::new(helper)
                .arg(socket)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("starting screenshot helper")?,
            None,
        );
        let mut pipe = child.0.stdout.take().unwrap();
        let (tx, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let result = (|| {
                let mut bytes = Vec::new();
                (&mut pipe)
                    .take((vm_control::MAX_SCREENSHOT_RESPONSE + 1) as u64)
                    .read_to_end(&mut bytes)?;
                if bytes.len() > vm_control::MAX_SCREENSHOT_RESPONSE {
                    bail!("screenshot output exceeds 3MiB")
                }
                let mut value: Value = serde_json::from_slice(&bytes)
                    .context("screenshot helper emitted invalid JSON")?;
                if value.get("error").is_some() {
                    bail!("screenshot capture failed: {}", value["error"])
                }
                if value["mime_type"] != "image/png"
                    || value["byte_length"]
                        .as_u64()
                        .is_none_or(|n| n > vm_control::MAX_SCREENSHOT_BYTES as u64)
                    || !value["data_base64"].is_string()
                    || !value["frame_id"].is_string()
                {
                    bail!("invalid screenshot metadata")
                }
                // Internal renderer identity is not an external screenshot coordinate token.
                value
                    .as_object_mut()
                    .context("screenshot object required")?
                    .remove("internal_scanout_id");
                Ok(value)
            })();
            let _ = tx.send(result);
        });
        Ok(Self {
            child: Some(child),
            socket: None,
            receiver,
            deadline: Instant::now() + Duration::from_secs(15),
            id,
        })
    }
    fn broker(socket_path: &Path, id: Value) -> Result<Self> {
        let mut socket = UnixStream::connect(socket_path).context("console broker unavailable")?;
        socket.set_write_timeout(Some(Duration::from_secs(1)))?;
        let request_id = format!("screenshot-{id}");
        serde_json::to_writer(
            &mut socket,
            &json!({"request_id":request_id,"method":"screenshot"}),
        )?;
        socket.write_all(b"\n")?;
        let cancel = socket.try_clone()?;
        let deadline = Instant::now() + Duration::from_secs(15);
        let (tx, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = tx.send(read_console_screenshot(&mut socket, &request_id, deadline));
        });
        Ok(Self {
            child: None,
            socket: Some(cancel),
            receiver,
            deadline,
            id,
        })
    }
    fn poll(&mut self) -> Option<Result<Value>> {
        if self.child.is_none() {
            match self.receiver.try_recv() {
                Ok(result) => return Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Some(Err(anyhow::anyhow!("console request reader disconnected")))
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        match self
            .child
            .as_mut()
            .map(|child| child.0.try_wait())
            .transpose()
        {
            Ok(Some(Some(exit))) => match self.receiver.try_recv() {
                Ok(Err(error)) => return Some(Err(error)),
                Ok(Ok(value)) => {
                    return Some(if exit.success() {
                        Ok(value)
                    } else {
                        Err(anyhow::anyhow!("screenshot helper failed with {exit}"))
                    })
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Some(Err(anyhow::anyhow!("screenshot reader disconnected")))
                }
                Err(mpsc::TryRecvError::Empty) => {}
            },
            Err(error) => return Some(Err(error.into())),
            Ok(None) | Ok(Some(None)) => {}
        }
        if Instant::now() >= self.deadline {
            return Some(Err(anyhow::anyhow!(
                "screenshot exceeded 15 second deadline"
            )));
        }
        None
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(socket) = &self.socket {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }
}
fn read_console_reply(
    socket: &mut UnixStream,
    request_id: &str,
    deadline: Instant,
) -> Result<Value> {
    read_console_reply_bounded(socket, request_id, deadline, vm_control::MAX_MESSAGE)
}
fn read_console_reply_bounded(
    socket: &mut UnixStream,
    request_id: &str,
    deadline: Instant,
    limit: usize,
) -> Result<Value> {
    fn read_until(socket: &mut UnixStream, mut buffer: &mut [u8], deadline: Instant) -> Result<()> {
        while !buffer.is_empty() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("console screenshot deadline exceeded")?;
            use std::os::fd::AsRawFd;
            socket.set_nonblocking(true)?;
            let mut pollfd = libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe {
                libc::poll(
                    &mut pollfd,
                    1,
                    remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
                )
            };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if ready == 0 {
                bail!("console screenshot deadline exceeded")
            }
            let n = match socket.read(buffer) {
                Ok(n) => n,
                Err(error)
                    if [
                        std::io::ErrorKind::WouldBlock,
                        std::io::ErrorKind::Interrupted,
                    ]
                    .contains(&error.kind()) =>
                {
                    continue
                }
                Err(error) => return Err(error.into()),
            };
            if n == 0 {
                bail!("console disconnected before screenshot completed")
            }
            buffer = &mut buffer[n..];
        }
        Ok(())
    }
    let mut header = [0u8; 13];
    read_until(socket, &mut header, deadline)?;
    let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
    if header[4] != 1 || header[5..13] != [0; 8] || length == 0 || length > limit {
        bail!("invalid or oversized console screenshot frame")
    }
    let mut body = vec![0u8; length];
    read_until(socket, &mut body, deadline)?;
    let reply: Value = serde_json::from_slice(&body)?;
    if reply["request_id"] != request_id {
        bail!("console screenshot response identity mismatch")
    }
    if let Some(error) = reply.get("error") {
        if request_id == "native-frame" {
            if let Some(stage) = vm_control::native_input::RejectionStage::display_rejection(
                &reply["display_rejection"],
            ) {
                return Err(stage.into());
            }
        }
        bail!("console operation failed: {error}")
    }
    reply
        .get("result")
        .cloned()
        .context("missing console result")
}
fn console_window(path: &Path, method: &str) -> Result<Value> {
    if !["show_window", "close_window", "window_status"].contains(&method) {
        bail!("invalid native window operation")
    }
    let mut socket = UnixStream::connect(path)?;
    socket.set_write_timeout(Some(Duration::from_millis(500)))?;
    serde_json::to_writer(
        &mut socket,
        &json!({"request_id":"native-window","method":method}),
    )?;
    socket.write_all(b"\n")?;
    let result = read_console_reply(
        &mut socket,
        "native-window",
        Instant::now() + Duration::from_secs(3),
    )?;
    vm_control::backend::validate_window_capabilities(result.clone())?;
    if !result["window_open"].is_boolean() || !vm_control::backend::window_supported(&result) {
        bail!("invalid native display-only window acknowledgement")
    }
    if (method == "show_window" && result["window_open"] != true)
        || (method == "close_window" && result["window_open"] != false)
    {
        bail!("native window did not acknowledge requested state")
    }
    Ok(result)
}
fn console_frame_status(path: &Path) -> Result<Value> {
    let mut socket = UnixStream::connect(path)?;
    socket.set_write_timeout(Some(Duration::from_millis(500)))?;
    socket.write_all(b"{\"request_id\":\"frame-input\",\"method\":\"frame_status\"}\n")?;
    read_console_reply(
        &mut socket,
        "frame-input",
        Instant::now() + Duration::from_millis(750),
    )
}
fn read_console_screenshot(
    socket: &mut UnixStream,
    request_id: &str,
    deadline: Instant,
) -> Result<Value> {
    let mut value = read_console_reply_bounded(
        socket,
        request_id,
        deadline,
        vm_control::MAX_SCREENSHOT_RESPONSE,
    )?;
    if value["mime_type"] != "image/png"
        || value["byte_length"]
            .as_u64()
            .is_none_or(|n| n > vm_control::MAX_SCREENSHOT_BYTES as u64)
        || !value["data_base64"].is_string()
        || !value["captured_monotonic_us"].is_u64()
        || !value["width"].is_u64()
        || !value["height"].is_u64()
    {
        bail!("invalid console screenshot metadata")
    }
    let mut random = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    random[6] = (random[6] & 0x0f) | 0x40;
    random[8] = (random[8] & 0x3f) | 0x80;
    let hex = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    value["frame_id"] = json!(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ));
    value["display_id"] = json!(0);
    value["guest_ui_scale"] = Value::Null;
    value.as_object_mut().unwrap().remove("internal_scanout_id");
    Ok(value)
}
struct ConsoleBroker {
    child: QemuChild,
    capabilities: Value,
}
impl ConsoleBroker {
    fn start(
        binary: &Path,
        helper: &Path,
        spice: &Path,
        socket: &Path,
        stderr: PathBuf,
        native_args: &[String],
    ) -> Result<Self> {
        let mut owned = Self {
            capabilities: Value::Null,
            child: QemuChild(
                Command::new(binary)
                    .arg(helper)
                    .arg(spice)
                    .arg(socket)
                    .args(native_args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .context("starting console broker")?,
                None,
            ),
        };
        log_pipe(owned.child.0.stderr.take().unwrap(), Some(stderr))?;
        let stdout = owned.child.0.stdout.take().unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let reply = (|| {
                let bytes =
                    read_frame(&mut reader)?.context("console broker closed before ready")?;
                serde_json::from_slice::<Value>(&bytes)
                    .context("invalid console readiness response")
            })();
            let _ = tx.send(reply);
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        });
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            if STOP.load(Ordering::Acquire) {
                bail!("console startup interrupted")
            }
            if let Some(exit) = owned.child.0.try_wait()? {
                bail!("console broker exited before ready: {exit}")
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(reply) => {
                    let reply = reply?;
                    if reply["event"] != "ready"
                        || reply["protocol"] != 1
                        || reply["capture_connected"] != true
                    {
                        bail!("console broker did not confirm capture connection")
                    }
                    owned.capabilities = reply["capabilities"].clone();
                    if !owned.capabilities.is_object() {
                        bail!("invalid console capabilities")
                    }
                    if owned.capabilities["broker_window_protocol"] != 1 {
                        owned.capabilities["native_window"] = json!(false);
                    }
                    return Ok(owned);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("console broker readiness channel closed")
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if Instant::now() >= deadline {
                bail!("console broker readiness exceeded 12 seconds")
            }
        }
    }
}
impl Drop for ConsoleBroker {
    fn drop(&mut self) {
        if self.child.0.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.child.0.id() as i32, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            while self.child.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
        }
        // QemuChild's fallback kills/reaps only this still-owned child.
    }
}
fn run() -> Result<()> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        bail!("QEMU/HVF backend requires Apple Silicon macOS")
    }
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("run") {
        bail!("usage: vm-qemu run --config FILE --launcher FILE --library FILE --resources DIR")
    }
    let mut options = std::collections::BTreeMap::new();
    while let Some(key) = args.next() {
        if ![
            "--control-socket",
            "--vm-id",
            "--worker-instance",
            "--guest-worker-instance",
            "--exec-witness-fd",
            "--config",
            "--launcher",
            "--library",
            "--resources",
            "--spice-library",
            "--venus-runtime",
            "--screenshot-helper",
            "--console-broker",
            "--console-helper",
        ]
        .contains(&key.as_str())
            || options.contains_key(&key)
        {
            bail!("unknown or duplicate runtime option")
        }
        options.insert(
            key,
            PathBuf::from(args.next().context("missing option value")?),
        );
    }
    if options.contains_key("--console-broker") != options.contains_key("--console-helper") {
        bail!("console broker and helper must be supplied together")
    }
    let native_fields = ["--control-socket", "--vm-id", "--worker-instance"];
    let native_count = native_fields
        .iter()
        .filter(|key| options.contains_key(**key))
        .count();
    if native_count != 0 && native_count != 3 {
        bail!("native control arguments must be complete")
    }
    let mut native_args = Vec::new();
    if native_count == 3 {
        if !Path::new(&options["--control-socket"]).is_absolute()
            || !vm_control::native_input::hex(
                options["--worker-instance"]
                    .to_str()
                    .context("native nonce must be UTF8")?,
                64,
            )
        {
            bail!("invalid native control binding")
        }
        for key in native_fields {
            native_args.push(key.into());
            native_args.push(
                options[key]
                    .to_str()
                    .context("native argument must be UTF8")?
                    .to_owned(),
            );
        }
    }
    let config_path = options.remove("--config").context("missing config")?;
    let bytes = fs::read(&config_path)?;
    if bytes.len() > 65536 {
        bail!("config exceeds 64KiB")
    }
    let config: Config = serde_json::from_slice(&bytes)?;
    config.validate()?;
    if config.backend != Backend::QemuHvf {
        bail!("wrong backend for QEMU worker")
    }
    if native_count == 3 && options["--vm-id"].to_str() != Some(config.id.as_str()) {
        bail!("native VM binding mismatch")
    }
    if config.window && !options.contains_key("--console-broker") {
        bail!("native window requires persistent console registration")
    }
    let mut launcher = options.remove("--launcher").context("missing launcher")?;
    let library = options.remove("--library").context("missing library")?;
    let resources = options.remove("--resources").context("missing resources")?;
    let (venus, graphics_host) = if config.qemu_graphics_profile.venus() {
        let registration = vm_control::qemu_graphics::Registration::load(
            options
                .get("--venus-runtime")
                .context("Venus runtime not locally registered")?,
        )?;
        registration.validate(
            &library,
            &resources,
            options.get("--spice-library").map(PathBuf::as_path),
        )?;
        let host = vm_control::qemu_graphics::host_admission()?;
        launcher = registration.loader.path.clone();
        (Some(registration), Some(host))
    } else {
        (None, None)
    };
    // Only signal-safe atomic work in handlers. Blocking operations run on the main loop.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            stop_signal as *const () as libc::sighandler_t,
        );
        libc::signal(libc::SIGINT, stop_signal as *const () as libc::sighandler_t);
    }
    let runtime = RuntimeDirectory::new()?;
    let qmp_path = runtime.0.join("qmp.sock");
    let spice_path = runtime.0.join("spice.sock");
    let console_path = runtime.0.join("console.sock");
    let agent_path = runtime.0.join("agent.sock");
    let disk_guard = config
        .disk
        .as_deref()
        .map(vm_control::qemu_disk::DiskGuard::open)
        .transpose()?;
    let exec_instance = options
        .get("--guest-worker-instance")
        .and_then(|p| p.to_str());
    if config.qemu_guest_exec && exec_instance.is_none_or(|s| !vm_control::native_input::hex(s, 64))
    {
        bail!("serial execution requires worker incarnation")
    }
    let witness = if config.qemu_guest_exec {
        let fd: i32 = options
            .get("--exec-witness-fd")
            .and_then(|v| v.to_str())
            .context("execution witness descriptor required")?
            .parse()?;
        Some(vm_control::exec_witness::Witness::inherited(
            fd,
            &config.id,
            exec_instance.unwrap(),
        )?)
    } else {
        if options.contains_key("--exec-witness-fd") {
            bail!("unexpected execution witness")
        }
        None
    };
    let guardian = if config.qemu_guest_exec {
        Some(guardian::Parent::new()?)
    } else {
        None
    };
    let mut command = if let Some(watch) = &guardian {
        let mut c = Command::new(std::env::current_exe()?);
        watch.wrap(&mut c)?;
        c.arg(launcher);
        c
    } else {
        Command::new(launcher)
    };
    vm_control::qemu_graphics::clean_environment(&mut command);
    if let Some(registration) = &venus {
        registration.configure(&mut command);
    }
    command
        .arg(library)
        .arg("-L")
        .arg(&resources)
        .arg("-bios")
        .arg(resources.join("edk2-aarch64-code.fd"))
        .args(config.qemu_graphics_profile.machine_arguments())
        .args([
            "-cpu",
            "host",
            "-nodefaults",
            "-display",
            "none",
            "-monitor",
            "none",
            "-serial",
            "stdio",
            "-nic",
            "user,model=virtio-net-pci",
        ])
        .arg("-device")
        .arg(
            config
                .qemu_graphics_profile
                .device(config.width, config.height)?,
        )
        .args([
            "-device",
            "virtio-keyboard-pci",
            "-device",
            "virtio-tablet-pci",
        ])
        .arg("-m")
        .arg(format!("{}M", config.memory_mib))
        .arg("-smp")
        .arg(config.cpus.to_string())
        .arg("-qmp")
        .arg(format!("unix:{},server=on,wait=off", qmp_path.display()))
        .arg("-spice")
        .arg(format!(
            "unix=on,addr={},disable-ticketing=on,gl=es",
            spice_path.display()
        ))
        .arg("-kernel")
        .arg(config.kernel.as_ref().unwrap())
        .arg("-append")
        .arg(config.command_line.as_deref().unwrap_or("console=ttyAMA0"))
        .env("ANGLE_DEFAULT_PLATFORM", "metal")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if options.contains_key("--console-broker")
        || disk_guard.is_some()
        || witness.is_some()
        || venus.is_some()
    {
        command.arg("-S");
    }
    if let Some(initrd) = &config.initrd {
        command.arg("-initrd").arg(initrd);
    }
    command
        .args(["-device", "virtio-serial-pci,id=veynode-serial", "-chardev"])
        .arg(format!(
            "socket,id=veynode-agent,path={},server=on,wait=off",
            agent_path.display()
        ))
        .args([
            "-device",
            "virtserialport,bus=veynode-serial.0,chardev=veynode-agent,name=org.veynode.agent.0",
        ]);
    if let Some(spice_library) = options.get("--spice-library") {
        command.env("VEYNODE_SPICE_LIBRARY", spice_library);
    }
    if let Some(disk) = &disk_guard {
        disk.configure_command(&mut command)?;
    }
    if let Some(witness) = &witness {
        witness.configure_qemu(&mut command)?;
    }
    let mut child = QemuChild(
        command.spawn().context("starting QEMU/HVF runtime")?,
        guardian,
    );
    let backend_pid = if let Some(watch) = &mut child.1 {
        watch.accept()?
    } else {
        child.0.id()
    };
    let log_prefix = config_path
        .parent()
        .context("config has no parent")?
        .join(format!("{}-{}", config.id, child.0.id()));
    let serial_log = config.serial_log.clone();
    let stderr_log = log_prefix.with_extension("stderr.log");
    log_pipe(child.0.stdout.take().unwrap(), serial_log.clone())?;
    log_pipe(child.0.stderr.take().unwrap(), Some(stderr_log.clone()))?;
    let (tx, rx) = mpsc::sync_channel(16);
    thread::spawn(move || {
        let mut reader = BufReader::new(std::io::stdin());
        loop {
            match read_frame(&mut reader) {
                Ok(Some(bytes)) => {
                    if tx
                        .send(serde_json::from_slice::<Value>(&bytes).map_err(anyhow::Error::from))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(None) => {
                    STOP.store(true, Ordering::Release);
                    break;
                }
                Err(e) => {
                    STOP.store(true, Ordering::Release);
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
    });
    let mut qmp = Qmp::connect(&qmp_path, &mut child.0)?;
    if let Some(disk) = &disk_guard {
        disk.verify_fdsets(&qmp.call("query-fdsets")?)?;
    }
    if let Some(witness) = &witness {
        witness.verify_fdsets(&qmp.call("query-fdsets")?)?;
    }
    if venus.is_some() {
        vm_control::qemu_graphics::verify_gpu_properties(|method, args| {
            qmp.arguments(method, args)
        })?;
    }
    let guest = if config.qemu_guest_exec {
        None
    } else {
        Some(guest_transport::Transport::start(&agent_path))
    };
    let guest_exec = config
        .qemu_guest_exec
        .then(|| guest_exec::Transport::start(&agent_path, exec_instance.unwrap().into()));
    let mut exec_pending: Vec<(Value, guest_exec::Pending)> = Vec::new();
    let mut guest_pending: Option<(Value, guest_transport::Pending)> = None;
    let mut broker = if let Some(binary) = options.get("--console-broker") {
        let broker = ConsoleBroker::start(
            binary,
            &options["--console-helper"],
            &spice_path,
            &console_path,
            log_prefix.with_extension("console.log"),
            &native_args,
        )?;
        if config.window {
            if !vm_control::backend::window_supported(&broker.capabilities) {
                bail!("native display-only window unavailable")
            }
            console_window(&console_path, "show_window")?;
        }
        Some(broker)
    } else {
        None
    };
    let status = qmp.call("query-status")?;
    let mut state = json!({"event":"state","state":if status["running"]==true {"running"}else{"paused"},"qmp_status":status["status"],"guest_ready":null,"vm_id":config.id,"backend_pid":backend_pid,"spice_socket":spice_path,"serial_log":serial_log,"stderr_log":stderr_log,"log_limit_bytes_per_stream":8388608});
    if let Some(host) = graphics_host {
        state["graphics_profile"] = json!({"configured":config.qemu_graphics_profile,"host_supported":true,"guest_verified":false,"host_admission":host,"runtime_revision":"owned-group-v1","qmp_gpu_properties_verified":true,"ipa_granule_requested_bytes":4096,"hostmem_aperture_bytes":268435456u64,"renderer_ownership":"inherits-worker-process-group"});
    }
    if let Some(disk) = &disk_guard {
        let identity = disk.identity();
        state["disk_provenance"] = json!({"device":identity.device,"inode":identity.inode,"descriptor_backed":true,"qmp_fdsets_verified_at_start":true});
    }
    state["console_socket"] = if broker.is_some() {
        json!(console_path)
    } else {
        Value::Null
    };
    state["capture_mode"] = json!(if broker.is_some() {
        "persistent-console"
    } else if options.contains_key("--screenshot-helper") {
        "diagnostic-helper"
    } else {
        "disabled"
    });
    state["capture_connected"] = json!(broker.is_some());
    if config.qemu_guest_exec {
        state["execution_guardian"] = json!({"guardian_pid":child.0.id(),"backend_pid":backend_pid,"heartbeat_timeout_ms":5000,"startup_timeout_ms":30000,"owns_qemu_child":true});
    }
    state["native_window_capabilities"] = broker
        .as_ref()
        .map(|b| b.capabilities.clone())
        .unwrap_or(Value::Null);
    state["native_window_available"] = json!(broker
        .as_ref()
        .is_some_and(|b| vm_control::backend::window_supported(&b.capabilities)));
    if let Some(witness) = &witness {
        state["execution_witness"] = serde_json::to_value(&witness.identity)?;
        state["execution_witness"]["qmp_fdsets_verified_at_start"] = json!(true);
    }
    publish_startup_then_resume(
        &mut qmp,
        &mut state,
        broker.is_some() || disk_guard.is_some() || witness.is_some() || venus.is_some(),
        emit,
    )?;
    let mut status_poll = Instant::now();
    let mut capture: Option<Capture> = None;
    let mut frames = vm_control::input::Frames::default();
    let mut input = InputState::default();
    let mut native_input = native_input_worker::NativeInput::default();
    let mut execution_shutdown = ExecutionShutdown::default();
    let mut guardian_heartbeat = Instant::now() - Duration::from_secs(1);
    loop {
        if config.qemu_guest_exec {
            let mut parent = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut parent, 1, 0) };
            if ready < 0 {
                bail!("execution supervisor pipe observation failed")
            }
            if ready > 0 && parent.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                break;
            }
        }
        // Observe the owned child before reading its guardian socket: normal exit closes both.
        if let Some(status) = child.0.try_wait()? {
            emit(
                json!({"event":"state","state":if status.success(){"stopped"}else{"crashed"},"exit_code":status.code(),"guest_ready":null,"vm_id":config.id}),
            )?;
            if !status.success() {
                bail!("QEMU process crashed: {status}")
            }
            return Ok(());
        }
        execution_shutdown.check(Instant::now())?;
        if guardian_heartbeat.elapsed() >= Duration::from_millis(500) {
            if let Some(watch) = &mut child.1 {
                if let Err(error) = watch.heartbeat() {
                    if !execution_shutdown.active() {
                        return Err(error);
                    }
                    // A terminating guardian may close just before waitable exit. Never
                    // call this success: continue polling the actual child under deadline.
                }
            }
            guardian_heartbeat = Instant::now();
        }
        if !execution_shutdown.active()
            && guest_exec.as_ref().is_some_and(|g| g.state() == "fenced")
        {
            bail!("serial execution incarnation fenced; QEMU will be terminated")
        }
        let mut index = 0;
        while index < exec_pending.len() {
            if let Some(result) = exec_pending[index].1.poll() {
                let (id, _) = exec_pending.remove(index);
                let value = result?;
                emit(json!({"id":id,"result":value}))?;
            } else {
                index += 1
            }
        }
        if input.until.is_some_and(|until| Instant::now() >= until) {
            if native_input.active(&input) {
                if let Some(epoch) = input.active_epoch {
                    input.revoked_through = input.revoked_through.max(epoch);
                }
            }
            input.cleanup(&mut qmp)?;
        }
        if let Some((id, pending)) = &mut guest_pending {
            if let Some(result) = pending.poll() {
                let id = id.clone();
                guest_pending = None;
                emit(match result {
                    Ok(value) => json!({"id":id,"result":value}),
                    Err(error) => json!({"id":id,"error":error.to_string()}),
                })?;
            }
        }
        if let Some(active) = &mut capture {
            if let Some(result) = active.poll() {
                let id = active.id.clone();
                drop(capture.take());
                emit(match result {
                    Ok(value) => {
                        frames.record(&value, Instant::now());
                        json!({"id":id,"result":value})
                    }
                    Err(error) => json!({"id":id,"error":error.to_string()}),
                })?;
            }
        }
        if STOP.load(Ordering::Acquire) {
            break;
        }
        if let Some(active) = &mut broker {
            if let Some(exit) = active.child.0.try_wait()? {
                // Retain the VM, but fail capture closed. Never reconnect a static source silently.
                state["capture_error"] = json!(format!("console broker exited: {exit}"));
                state["capture_connected"] = json!(false);
                state["native_window_available"] = json!(false);
                emit(state.clone())?;
                drop(broker.take());
            }
        }
        if let Some(status) = child.0.try_wait()? {
            emit(
                json!({"event":"state","state":if status.success(){"stopped"}else{"crashed"},"exit_code":status.code(),"guest_ready":null,"vm_id":config.id}),
            )?;
            if !status.success() {
                bail!("QEMU process crashed: {status}")
            }
            return Ok(());
        }
        if status_poll.elapsed() >= Duration::from_secs(1) && capture.is_none() {
            let mut updated = state.clone();
            match qmp.call("query-status") {
                Ok(status) => {
                    updated["qmp_status"] = status["status"].clone();
                    updated["state"] = json!(if status["running"] == true {
                        "running"
                    } else {
                        "paused"
                    });
                    updated.as_object_mut().unwrap().remove("qmp_error");
                }
                Err(error) => {
                    updated["state"] = json!("unavailable");
                    updated["qmp_error"] = json!(error.to_string());
                }
            }
            if updated != state {
                emit(updated.clone())?;
                state = updated;
            }
            status_poll = Instant::now();
        }
        let request = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(request) => request?,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let id = request["id"].clone();
        if execution_shutdown.active()
            && matches!(
                request["method"].as_str(),
                Some(
                    "guest_request"
                        | "guest_scope_revoke"
                        | "native_begin"
                        | "native_heartbeat"
                        | "native_event"
                        | "input_step"
                )
            )
        {
            emit(json!({"id":id,"error":"VM execution is quiescing for shutdown"}))?;
            continue;
        }
        let result: Result<Value> = match request["method"].as_str() {
            Some("guest_request" | "guest_scope_revoke") => {
                if let Some(exec)=&guest_exec {
                    match exec.request_before(request["request"].clone(), { let remaining=request["qemu_exec_deadline_ns"].as_u64().unwrap_or(vm_control::exec_scope::monotonic_ns()?+1_000_000_000).saturating_sub(vm_control::exec_scope::monotonic_ns()?); Instant::now()+Duration::from_nanos(remaining.min(5_000_000_000)) }){Ok(pending)=>{exec_pending.push((id.clone(),pending));continue},Err(error)=>Err(error)}
                } else if guest_pending.is_some() {Err(anyhow::anyhow!("guest integration busy: one request per VM"))}
                else {match guest.as_ref().unwrap().request(request["request"].clone()) {Ok(pending)=>{guest_pending=Some((id.clone(),pending));continue},Err(error)=>Err(error)}}
            },
            Some(method @ ("show_window" | "close_window" | "window_status")) => {
                if !broker.as_ref().is_some_and(|b|vm_control::backend::window_supported(&b.capabilities)) {
                    Err(anyhow::anyhow!("native display-only window unavailable"))
                } else { console_window(&console_path,method) }
            },
            Some("screenshot") => {
                if capture.is_some() { Err(anyhow::anyhow!("screenshot busy: one capture per VM")) }
                else if options.contains_key("--console-broker") {
                    if broker.is_none() { Err(anyhow::anyhow!("console broker unavailable; VM restart required to restore capture")) }
                    else { match Capture::broker(&console_path,id.clone()) { Ok(started)=>{capture=Some(started);continue},Err(error)=>Err(error) } }
                }
                else if let Some(helper) = options.get("--screenshot-helper") {
                    match Capture::start(helper, &spice_path, id.clone()) { Ok(started) => { capture = Some(started); continue; }, Err(error) => Err(error) }
                } else { Err(anyhow::anyhow!("unsupported: screenshot helper is not registered")) }
            }
            Some(method @ ("native_begin" | "native_heartbeat" | "native_event")) => native_input.handle(method,&request["arguments"],&mut input,&console_path,&mut qmp),
            Some("input_step") => {
                if broker.is_none() {Err(anyhow::anyhow!("persistent console unavailable"))}
                else {input.step(&request["arguments"],&frames,&console_path,&mut qmp)}
            },
            Some("input_cleanup") => {
                input.cleanup_operation(&request["arguments"], &mut qmp)
            },
            Some("input_revoke") => input.revoke(&request["arguments"], &mut qmp),
            Some("status") => qmp.call("query-status").map(|status|json!({"qmp_status":status,"guest_ready":null,"vm_id":config.id,"disk_provenance":state.get("disk_provenance"),"guest_integration_state":guest.as_ref().map(|g|g.state()).unwrap_or_else(||guest_exec.as_ref().unwrap().state())})),
            Some("shutdown") => {
                let prepared = if let Some(exec) = &guest_exec {
                    execution_shutdown.begin(&request["execution_scopes_empty"],exec_pending.len(),exec.state(),Instant::now())
                } else { Ok(()) };
                prepared.and_then(|_| request_powerdown(&mut qmp, &mut input, guest_exec.is_some())).map(|_|json!({"requested":true,"mechanism":"qmp-system-powerdown","completed":false,"requires_guest_power_button_support":true,"execution_quiescing":execution_shutdown.active(),"shutdown_timeout_seconds":if execution_shutdown.active(){json!(60)}else{Value::Null}}))
            },
            Some("stop") => { emit(json!({"id":id,"result":{"requested":true,"forced":true}}))?; break },
            _ => Err(anyhow::anyhow!("unsupported QEMU worker operation")),
        };
        emit(match result {
            Ok(value) => json!({"id":id,"result":value}),
            Err(error) => {
                let mut reply = json!({"id":id,"error":error.to_string()});
                if let Some(diagnostic) =
                    error.downcast_ref::<vm_control::native_input::RejectionDiagnostic>()
                {
                    reply["native_input_diagnostic"] = serde_json::to_value(diagnostic)?;
                }
                reply
            }
        })?;
    }
    let _ = input.cleanup(&mut qmp);
    drop(capture);
    drop(broker);
    // EOF/signal are supervisor loss, not a Guest normal shutdown promise.
    let _ = qmp.call("quit");
    let deadline = Instant::now() + Duration::from_secs(2);
    while child.0.try_wait()?.is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    drop(child);
    drop(disk_guard);
    if execution_shutdown.active() {
        bail!(
            "normal shutdown interrupted by supervisor loss or forced stop; completion unconfirmed"
        )
    }
    emit(
        json!({"event":"state","state":"stopped","reason":"supervisor_disconnected_or_stop","vm_id":config.id}),
    )?;
    Ok(())
}
fn request_powerdown(qmp: &mut Qmp, input: &mut InputState, retire_input: bool) -> Result<Value> {
    if retire_input {
        if let Some(epoch) = input.active_epoch {
            input.revoked_through = input.revoked_through.max(epoch);
        }
        input
            .cleanup(qmp)
            .context("shutdown input cleanup unconfirmed")?;
    }
    qmp.call("system_powerdown")
}
/// Only trusted controller proof plus drained local RPCs may relax serial liveness.
#[derive(Default)]
struct ExecutionShutdown {
    deadline: Option<Instant>,
}
impl ExecutionShutdown {
    fn begin(
        &mut self,
        proof: &Value,
        pending: usize,
        transport: &str,
        now: Instant,
    ) -> Result<()> {
        if self.deadline.is_some() {
            return Ok(());
        }
        if proof != &json!(true) || pending != 0 || transport != "ready" {
            bail!("execution shutdown requires confirmed empty scopes, drained requests and ready transport")
        }
        self.deadline = Some(now + Duration::from_secs(60));
        Ok(())
    }
    fn active(&self) -> bool {
        self.deadline.is_some()
    }
    fn check(&self, now: Instant) -> Result<()> {
        if self.deadline.is_some_and(|d| now >= d) {
            bail!("normal shutdown deadline exceeded; completion unconfirmed")
        }
        Ok(())
    }
}
fn main() {
    let early = std::env::args_os().skip(1).collect::<Vec<_>>();
    if early.first().is_some_and(|v| v == "--supervise-qemu") {
        if let Err(error) = guardian::run(&early[1..]) {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = run() {
        let _ = emit(json!({"event":"state","state":"error","error":error.to_string()}));
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    fn broker_reply(reply: Value, kind: u8) -> Result<Value> {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let body = serde_json::to_vec(&reply).unwrap();
        let mut header = [0u8; 13];
        header[..4].copy_from_slice(&(body.len() as u32).to_be_bytes());
        header[4] = kind;
        writer.write_all(&header).unwrap();
        writer.write_all(&body).unwrap();
        drop(writer);
        read_console_screenshot(
            &mut reader,
            "expected",
            Instant::now() + Duration::from_secs(1),
        )
    }
    #[test]
    fn startup_proof_is_published_before_cont_even_when_qmp_closes_after_ack() {
        let (client, mut peer) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let published = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = published.clone();
        let server = thread::spawn(move || {
            let mut reader = BufReader::new(peer.try_clone().unwrap());
            let request: Value =
                serde_json::from_slice(&read_frame(&mut reader).unwrap().unwrap()).unwrap();
            assert_eq!(request["execute"], "cont");
            assert!(observed.load(Ordering::Acquire));
            writeln!(peer, "{}", json!({"id":request["id"],"return":{}})).unwrap();
            // No later query-status is needed to establish the launch identity.
        });
        let mut qmp = Qmp {
            reader: BufReader::new(client.try_clone().unwrap()),
            stream: client,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
        };
        let mut state = json!({"state":"paused","disk_provenance":{"device":7,"inode":9,"qmp_fdsets_verified_at_start":true}});
        let mut events = Vec::new();
        publish_startup_then_resume(&mut qmp, &mut state, true, |event| {
            assert_eq!(event["disk_provenance"]["inode"], 9);
            published.store(true, Ordering::Release);
            events.push(event);
            Ok(())
        })
        .unwrap();
        server.join().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["state"], "starting");
        assert_eq!(events[1]["state"], "running");
    }
    #[test]
    fn failed_startup_publication_never_resumes_guest() {
        let (client, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut qmp = Qmp {
            reader: BufReader::new(client.try_clone().unwrap()),
            stream: client,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
        };
        assert!(publish_startup_then_resume(
            &mut qmp,
            &mut json!({"state":"paused"}),
            true,
            |_| bail!("injected stdout failure")
        )
        .is_err());
        let mut bytes = [0; 1];
        assert_eq!(
            peer.read(&mut bytes).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    #[test]
    fn runtime_v2_window_status_requires_json_booleans_not_numeric_flags() {
        let status = json!({"native_window":true,"native_window_protocol":2,"native_input_protocol":1,
            "native_input_supported":true,"native_input_available":false,"input_enabled":false,"display_only":true,
            "window_open":true,"display_instance":"a".repeat(32),"native_input_state":"connecting",
            "native_input_ready":false,"native_human_hold":false,"native_release_confirmed":true,"native_input_epoch":null});
        for field in [
            None,
            Some("native_input_available"),
            Some("input_enabled"),
            Some("display_only"),
        ] {
            let mut status = status.clone();
            if let Some(field) = field {
                status[field] = json!(if field == "display_only" { 1 } else { 0 });
            }
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("console.sock");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let peer = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request: Value = serde_json::from_slice(
                    &read_frame(&mut BufReader::new(socket.try_clone().unwrap()))
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(
                    request,
                    json!({"request_id":"native-window","method":"window_status"})
                );
                let body =
                    serde_json::to_vec(&json!({"request_id":"native-window","result":status}))
                        .unwrap();
                socket
                    .write_all(&(body.len() as u32).to_be_bytes())
                    .unwrap();
                socket.write_all(&[1]).unwrap();
                socket.write_all(&[0; 8]).unwrap();
                socket.write_all(&body).unwrap();
            });
            assert_eq!(
                console_window(&path, "window_status").is_ok(),
                field.is_none(),
                "field {field:?}"
            );
            peer.join().unwrap();
        }
    }
    #[test]
    fn window_rpc_requires_exact_epoch_identity_and_display_only_ack() {
        for (epoch, ident, opened, enabled, okay) in [
            (0, "native-window", true, false, true),
            (1, "native-window", true, false, false),
            (0, "other-request", true, false, false),
            (0, "native-window", false, false, false),
            (0, "native-window", true, true, false),
        ] {
            let directory = tempfile::Builder::new()
                .prefix("window-")
                .tempdir_in("/tmp")
                .unwrap();
            let path = directory.path().join("console.sock");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let peer = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request: Value = serde_json::from_slice(
                    &read_frame(&mut BufReader::new(socket.try_clone().unwrap()))
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(
                    request,
                    json!({"request_id":"native-window","method":"show_window"})
                );
                let body=serde_json::to_vec(&json!({"request_id":ident,"result":{"native_window":true,"native_window_protocol":1,"display_only":true,"input_enabled":enabled,"window_open":opened}})).unwrap();
                let mut header = [0u8; 13];
                header[..4].copy_from_slice(&(body.len() as u32).to_be_bytes());
                header[4] = 1;
                header[5..].copy_from_slice(&(epoch as u64).to_be_bytes());
                socket.write_all(&header).unwrap();
                socket.write_all(&body).unwrap();
            });
            assert_eq!(console_window(&path, "show_window").is_ok(), okay);
            peer.join().unwrap();
        }
    }
    #[test]
    fn qmp_process_epoch_fence_releases_held_state_and_rejects_stale_steps() {
        // Actual child process + Unix QMP framing; synthetic QMP, not a Guest.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("qmp.sock");
        let program = r#"
import socket,json,sys
s=socket.socket(socket.AF_UNIX);s.bind(sys.argv[1]);s.listen(1);s.settimeout(5)
c,_=s.accept();c.settimeout(5);f=c.makefile('rwb',buffering=0)
def send(v): f.write(json.dumps(v).encode()+b'\n')
send({'QMP':{}})
r=json.loads(f.readline());assert r['execute']=='qmp_capabilities';send({'id':r['id'],'return':{}})
for n,down in enumerate([True,False,False,True,False],1):
 r=json.loads(f.readline());assert r['execute']=='input-send-event'
 assert r['arguments']['events']==[{'type':'btn','data':{'button':'left','down':down}}]
 send({'id':r['id'],'error':{'desc':'injected release failure'}} if n==2 else {'id':r['id'],'return':{}})
f.close();c.close();s.close()
"#;
        struct Owned(std::process::Child);
        impl Drop for Owned {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Owned(
            Command::new("python3")
                .arg("-c")
                .arg(program)
                .arg(&path)
                .spawn()
                .unwrap(),
        );
        let mut qmp = Qmp::connect(&path, &mut child.0).unwrap();
        let console = temp.path().join("console.sock");
        let listener = std::os::unix::net::UnixListener::bind(&console).unwrap();
        listener.set_nonblocking(true).unwrap();
        let console_thread = thread::spawn(move || {
            for _ in 0..2 {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => panic!("console fixture accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let request: Value = serde_json::from_slice(
                    &read_frame(&mut BufReader::new(stream.try_clone().unwrap()))
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                let body = serde_json::to_vec(&json!({"request_id":request["request_id"],"result":{"capture_connected":true,"frame_available":true,"source_generation":7,"width":1280,"height":800}})).unwrap();
                stream
                    .write_all(&(body.len() as u32).to_be_bytes())
                    .unwrap();
                stream.write_all(&[1]).unwrap();
                stream.write_all(&0u64.to_be_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        let anchor = json!({"frame_id":"00000000-0000-0000-0000-000000000001","width":1280,"height":800,"source_generation":7});
        let mut frames = vm_control::input::Frames::default();
        frames.record(&anchor, Instant::now());
        let mut args = anchor.clone();
        args["operation_id"] = json!("a".repeat(32));
        args["epoch"] = json!(3);
        args["begin"] = json!(true);
        args["events"] = json!([vm_control::input::button("left", true)]);
        let mut state = InputState::default();
        assert!(state.step(&args, &frames, &console, &mut qmp).is_ok());
        assert!(state.revoke(&json!({"through_epoch":4}), &mut qmp).is_err());
        assert_eq!(state.revoked_through, 4);
        assert_eq!(state.held.len(), 1);
        assert!(state
            .step(&args, &frames, &console, &mut qmp)
            .unwrap_err()
            .to_string()
            .contains("revoked"));
        assert_eq!(
            state.revoke(&json!({"through_epoch":4}), &mut qmp).unwrap()["released"],
            true
        );
        assert!(state.held.is_empty());
        assert_eq!(
            state.cleanup_operation(&args, &mut qmp).unwrap()["already_released"],
            true
        );
        args["operation_id"] = json!("b".repeat(32));
        args["epoch"] = json!(5);
        assert!(state.step(&args, &frames, &console, &mut qmp).is_ok());
        assert_eq!(
            state.revoke(&json!({"through_epoch":4}), &mut qmp).unwrap()["released"],
            true
        );
        assert_eq!(state.active_epoch, Some(5));
        assert_eq!(state.held.len(), 1);
        assert_eq!(
            state
                .cleanup_operation(&json!({"operation_id":"a".repeat(32)}), &mut qmp)
                .unwrap()["already_released"],
            true
        );
        assert_eq!(
            state.held.len(),
            1,
            "old operation cleanup cannot release newer buttons"
        );
        assert!(state
            .cleanup_operation(&json!({"operation_id":"c".repeat(32)}), &mut qmp)
            .is_err());
        assert!(state.revoke(&json!({"through_epoch":0}), &mut qmp).is_err());
        assert_eq!(
            state.revoke(&json!({"through_epoch":5}), &mut qmp).unwrap()["through_epoch"],
            5
        );
        assert!(state.held.is_empty());
        console_thread.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn failed_input_release_retains_state_for_retry() {
        let (stream, mut server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(server.try_clone().unwrap());
        let handle = thread::spawn(move || {
            for success in [false, true] {
                let request: Value =
                    serde_json::from_slice(&read_frame(&mut reader).unwrap().unwrap()).unwrap();
                assert_eq!(request["execute"], "input-send-event");
                assert_eq!(request["arguments"]["events"][0]["data"]["down"], false);
                let reply = if success {
                    json!({"id":request["id"],"return":{}})
                } else {
                    json!({"id":request["id"],"error":{"desc":"injected QMP failure"}})
                };
                serde_json::to_writer(&mut server, &reply).unwrap();
                server.write_all(b"\n").unwrap();
            }
        });
        let mut qmp = Qmp {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
        };
        let mut state = InputState {
            active_epoch: Some(1),
            operation: Some("a".repeat(32)),
            until: Some(Instant::now()),
            held: std::collections::BTreeMap::from([(
                "left".into(),
                vm_control::input::button("left", false),
            )]),
            ..Default::default()
        };
        assert!(state.cleanup(&mut qmp).is_err());
        assert_eq!(state.held.len(), 1);
        assert!(state.operation.is_some());
        state.cleanup(&mut qmp).unwrap();
        assert!(state.held.is_empty());
        assert!(state.operation.is_none());
        handle.join().unwrap();
    }
    #[test]
    fn broker_screenshot_preserves_frame_time_and_strips_internal_identity() {
        let result=broker_reply(json!({"request_id":"expected","result":{"mime_type":"image/png","byte_length":3,"data_base64":"YWJj","captured_monotonic_us":1234,"captured_at_us":4567,"source_generation":2,"source_sequence":4,"width":1280,"height":800,"internal_scanout_id":999}}),1).unwrap();
        assert_eq!(result["captured_monotonic_us"], 1234);
        assert_eq!(result["captured_at_us"], 4567);
        assert_eq!(result["source_sequence"], 4);
        assert!(result.get("internal_scanout_id").is_none());
        assert_eq!(result["frame_id"].as_str().unwrap().len(), 36);
    }
    #[test]
    fn broker_screenshot_rejects_unrelated_wrong_kind_oversized_and_expired_responses() {
        assert!(broker_reply(json!({"request_id":"wrong","result":{}}), 1).is_err());
        assert!(broker_reply(json!({"request_id":"expected","result":{}}), 2).is_err());
        assert!(broker_reply(
            json!({"request_id":"expected","error":"capture unavailable"}),
            1
        )
        .unwrap_err()
        .to_string()
        .contains("capture unavailable"));
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let mut header = [0u8; 13];
        header[..4]
            .copy_from_slice(&((vm_control::MAX_SCREENSHOT_RESPONSE + 1) as u32).to_be_bytes());
        header[4] = 1;
        writer.write_all(&header).unwrap();
        assert!(read_console_screenshot(
            &mut reader,
            "expected",
            Instant::now() + Duration::from_secs(1)
        )
        .unwrap_err()
        .to_string()
        .contains("oversized"));
        assert!(read_console_screenshot(
            &mut reader,
            "expected",
            Instant::now() - Duration::from_secs(1)
        )
        .unwrap_err()
        .to_string()
        .contains("deadline"));
    }
    #[test]
    fn logging_rejects_symlinks_hardlinks_and_nonprivate_files() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        fs::write(&original, b"preserved").unwrap();
        let link = dir.path().join("symlink");
        symlink(&original, &link).unwrap();
        assert!(log_pipe(std::io::Cursor::new(b"new"), Some(link)).is_err());
        assert!(log_pipe(std::io::Cursor::new(b"new"), Some(original.clone())).is_err());
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&original, fs::Permissions::from_mode(0o600)).unwrap();
        let hardlink = dir.path().join("hardlink");
        fs::hard_link(&original, &hardlink).unwrap();
        assert!(log_pipe(std::io::Cursor::new(b"new"), Some(hardlink)).is_err());
        assert_eq!(fs::read(&original).unwrap(), b"preserved");
    }
    #[test]
    fn log_bound_preserves_existing_bytes_and_drains_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serial");
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(b"original").unwrap();
        file.set_len(8 * 1024 * 1024 - 128).unwrap();
        drop(file);
        log_pipe(
            std::io::Cursor::new(vec![b'x'; 1024 * 1024]),
            Some(path.clone()),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while fs::metadata(&path).unwrap().len() != 8 * 1024 * 1024 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let contents = fs::read(path).unwrap();
        assert_eq!(contents.len(), 8 * 1024 * 1024);
        assert_eq!(&contents[..8], b"original");
        assert_eq!(&contents[contents.len() - 128..], &[b'x'; 128]);
    }
    #[test]
    fn execution_shutdown_requires_strict_proof_drained_ready_and_never_extends_deadline() {
        let now = Instant::now();
        for (proof, pending, transport) in [
            (json!(null), 0, "ready"),
            (json!(1), 0, "ready"),
            (json!(true), 1, "ready"),
            (json!(true), 0, "fenced"),
            (json!(true), 0, "starting"),
        ] {
            let mut state = ExecutionShutdown::default();
            assert!(state.begin(&proof, pending, transport, now).is_err());
            assert!(!state.active());
        }
        let mut state = ExecutionShutdown::default();
        state.begin(&json!(true), 0, "ready", now).unwrap();
        assert!(state.active()); // Only this state permits Guest service EOF.
        state.check(now + Duration::from_secs(59)).unwrap();
        state
            .begin(&json!(true), 0, "fenced", now + Duration::from_secs(59))
            .unwrap();
        assert!(state
            .check(now + Duration::from_secs(60))
            .unwrap_err()
            .to_string()
            .contains("completion unconfirmed"));
    }
    #[test]
    fn execution_shutdown_qmp_waits_for_held_release_ack_before_power_button() {
        for fail_release in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("qmp.sock");
            let script = r#"
import socket,json,sys
s=socket.socket(socket.AF_UNIX);s.bind(sys.argv[1]);s.listen(1);s.settimeout(5)
c,_=s.accept();c.settimeout(5);f=c.makefile('rwb',buffering=0)
def send(v): f.write(json.dumps(v).encode()+b'\n')
send({'QMP':{}})
r=json.loads(f.readline());assert r['execute']=='qmp_capabilities';send({'id':r['id'],'return':{}})
r=json.loads(f.readline());assert r['execute']=='input-send-event'
assert r['arguments']['events']==[{'type':'btn','data':{'button':'left','down':False}}]
if sys.argv[2]=='true':
 send({'id':r['id'],'error':{'desc':'release rejected'}})
 assert f.readline()==b''
else:
 send({'id':r['id'],'return':{}})
 r=json.loads(f.readline());assert r['execute']=='system_powerdown';send({'id':r['id'],'return':{}})
f.close();c.close();s.close()
"#;
            let mut child = Command::new("python3")
                .arg("-c")
                .arg(script)
                .arg(&path)
                .arg(fail_release.to_string())
                .spawn()
                .unwrap();
            let mut qmp = Qmp::connect(&path, &mut child).unwrap();
            let mut input = InputState {
                active_epoch: Some(7),
                ..Default::default()
            };
            input.held.insert(
                "btn:left".into(),
                json!({"type":"btn","data":{"button":"left","down":false}}),
            );
            let result = request_powerdown(&mut qmp, &mut input, true);
            assert_eq!(result.is_err(), fail_release);
            assert_eq!(input.revoked_through, 7);
            assert_eq!(input.held.is_empty(), !fail_release);
            if fail_release {
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("cleanup unconfirmed"));
            }
            drop(qmp);
            assert!(child.wait().unwrap().success());
        }
    }
}

#[cfg(test)]
#[path = "../screenshot_test_support.rs"]
mod screenshot_test_support;
#[cfg(test)]
mod screenshot_response_tests {
    use super::*;
    #[test]
    fn actual_console_socket_preserves_large_png_and_rejects_other_reply_bound() {
        let value = screenshot_test_support::value();
        for screenshot in [true, false] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            let payload = serde_json::to_vec(&json!({"request_id":"test","result":value})).unwrap();
            assert!(payload.len() > vm_control::MAX_MESSAGE);
            let writer = std::thread::spawn(move || {
                let mut h = [0u8; 13];
                h[..4].copy_from_slice(&(payload.len() as u32).to_be_bytes());
                h[4] = 1;
                let _ = server.write_all(&h);
                let _ = server.write_all(&payload);
            });
            let deadline = Instant::now() + Duration::from_secs(3);
            if screenshot {
                let reply = read_console_screenshot(&mut client, "test", deadline).unwrap();
                assert_eq!(reply["data_base64"], value["data_base64"]);
            } else {
                assert!(read_console_reply(&mut client, "test", deadline).is_err());
            }
            drop(client);
            writer.join().unwrap();
        }
    }
}

#[cfg(test)]
mod venus_qmp_admission_tests {
    use super::*;
    #[test]
    fn actual_qmp_socket_checks_properties_before_cont_and_denies_mismatch() {
        for wrong_blob in [false, true] {
            let (client, mut server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let peer = std::thread::spawn(move || {
                let mut reader = BufReader::new(server.try_clone().unwrap());
                let mut methods = Vec::new();
                while let Some(line) = read_frame(&mut reader).unwrap() {
                    let request: Value = serde_json::from_slice(&line).unwrap();
                    let method = request["execute"].as_str().unwrap();
                    methods.push(method.to_owned());
                    let result = match method {
                        "qom-get" => {
                            assert_eq!(
                                request["arguments"]["path"],
                                "/machine/peripheral/veynode-gpu"
                            );
                            match request["arguments"]["property"].as_str().unwrap() {
                                "venus" => json!(true),
                                "blob" => json!(!wrong_blob),
                                "hostmem" => json!(268435456u64),
                                other => panic!("unexpected property {other}"),
                            }
                        }
                        "query-status" => json!({"running":false,"status":"prelaunch"}),
                        "cont" => json!({}),
                        other => panic!("unexpected QMP method {other}"),
                    };
                    serde_json::to_writer(
                        &mut server,
                        &json!({"id":request["id"],"return":result}),
                    )
                    .unwrap();
                    server.write_all(b"\n").unwrap();
                }
                methods
            });
            let mut qmp = Qmp {
                reader: BufReader::new(client.try_clone().unwrap()),
                stream: client,
                next: 1,
                poisoned: false,
                reply_prefix: Vec::new(),
            };
            let admitted = vm_control::qemu_graphics::verify_gpu_properties(|method, args| {
                qmp.arguments(method, args)
            });
            assert_eq!(admitted.is_ok(), !wrong_blob);
            if admitted.is_ok() {
                let mut state = json!({"state":"paused","qmp_status":"prelaunch"});
                let mut states = Vec::new();
                publish_startup_then_resume(&mut qmp, &mut state, true, |value| {
                    states.push(value["state"].clone());
                    Ok(())
                })
                .unwrap();
                assert_eq!(states, vec![json!("starting"), json!("running")]);
            }
            drop(qmp);
            let methods = peer.join().unwrap();
            if wrong_blob {
                assert!(!methods.iter().any(|m| m == "cont"));
            } else {
                assert_eq!(methods.iter().position(|m| m == "cont"), Some(4));
            }
        }
    }
}
