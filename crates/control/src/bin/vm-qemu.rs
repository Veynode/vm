//! One independently supervised QEMU/HVF instance. Protocol output is JSON only.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, Read, Write},
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
        for name in ["qmp.sock", "spice.sock", "console.sock"] {
            let _ = fs::remove_file(self.0.join(name));
        }
        let _ = fs::remove_dir(&self.0);
    }
}
struct QemuChild(Child);
impl Drop for QemuChild {
    fn drop(&mut self) {
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
        let number = self.next;
        self.next += 1;
        serde_json::to_writer(
            &mut self.stream,
            &json!({"execute":method,"id":number,"arguments":arguments}),
        )?;
        self.stream.write_all(b"\n")?;
        // Event storms cannot extend one request forever.
        let deadline = Instant::now() + Duration::from_secs(3);
        for _ in 0..128 {
            self.reader.get_ref().set_read_timeout(Some(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
            ))?;
            let value: Value = serde_json::from_slice(
                &read_frame(&mut self.reader)?.context("QMP disconnected")?,
            )?;
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
}
#[derive(Default)]
struct InputState {
    operation: Option<String>,
    until: Option<Instant>,
    held: std::collections::BTreeMap<String, Value>,
}
impl InputState {
    fn cleanup(&mut self, qmp: &mut Qmp) -> Result<()> {
        if !self.held.is_empty() {
            qmp.arguments(
                "input-send-event",
                json!({"events":self.held.values().collect::<Vec<_>>()}),
            )?;
        }
        self.held.clear();
        self.operation = None;
        self.until = None;
        Ok(())
    }
    fn step(
        &mut self,
        args: &Value,
        frames: &vm_control::input::Frames,
        console: &Path,
        qmp: &mut Qmp,
    ) -> Result<Value> {
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
            self.operation = Some(operation.into());
            self.until = Some(Instant::now() + Duration::from_secs(4));
        }
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
        qmp.arguments("input-send-event", json!({"events":events}))?;
        for identity in releases {
            self.held.remove(&identity);
        }
        Ok(json!({"dispatched":true,"source_generation":current["source_generation"]}))
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
        );
        let mut pipe = child.0.stdout.take().unwrap();
        let (tx, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let result = (|| {
                let mut bytes = Vec::new();
                (&mut pipe)
                    .take((vm_control::MAX_MESSAGE + 1) as u64)
                    .read_to_end(&mut bytes)?;
                if bytes.len() > vm_control::MAX_MESSAGE {
                    bail!("screenshot output exceeds 1MiB")
                }
                let mut value: Value = serde_json::from_slice(&bytes)
                    .context("screenshot helper emitted invalid JSON")?;
                if value.get("error").is_some() {
                    bail!("screenshot capture failed: {}", value["error"])
                }
                if value["mime_type"] != "image/png"
                    || value["byte_length"].as_u64().is_none_or(|n| n > 700 * 1024)
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
    if header[4] != 1 || length == 0 || length > vm_control::MAX_MESSAGE {
        bail!("invalid or oversized console screenshot frame")
    }
    let mut body = vec![0u8; length];
    read_until(socket, &mut body, deadline)?;
    let reply: Value = serde_json::from_slice(&body)?;
    if reply["request_id"] != request_id {
        bail!("console screenshot response identity mismatch")
    }
    if let Some(error) = reply.get("error") {
        bail!("console screenshot failed: {error}")
    }
    reply
        .get("result")
        .cloned()
        .context("missing console result")
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
    let mut value = read_console_reply(socket, request_id, deadline)?;
    if value["mime_type"] != "image/png"
        || value["byte_length"].as_u64().is_none_or(|n| n > 700 * 1024)
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
}
impl ConsoleBroker {
    fn start(
        binary: &Path,
        helper: &Path,
        spice: &Path,
        socket: &Path,
        stderr: PathBuf,
    ) -> Result<Self> {
        let mut owned = Self {
            child: QemuChild(
                Command::new(binary)
                    .arg(helper)
                    .arg(spice)
                    .arg(socket)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .context("starting console broker")?,
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
            "--config",
            "--launcher",
            "--library",
            "--resources",
            "--spice-library",
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
    let launcher = options.remove("--launcher").context("missing launcher")?;
    let library = options.remove("--library").context("missing library")?;
    let resources = options.remove("--resources").context("missing resources")?;
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
    let mut command = Command::new(launcher);
    command
        .arg(library)
        .arg("-L")
        .arg(&resources)
        .arg("-bios")
        .arg(resources.join("edk2-aarch64-code.fd"))
        .args([
            "-machine",
            "virt,accel=hvf",
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
            "-device",
            "virtio-gpu-gl-pci",
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
    if options.contains_key("--console-broker") {
        command.arg("-S");
    }
    if let Some(initrd) = &config.initrd {
        command.arg("-initrd").arg(initrd);
    }
    if let Some(spice_library) = options.get("--spice-library") {
        command.env("VEYNODE_SPICE_LIBRARY", spice_library);
    }
    let mut child = QemuChild(command.spawn().context("starting QEMU/HVF runtime")?);
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
    let mut broker = if let Some(binary) = options.get("--console-broker") {
        let broker = ConsoleBroker::start(
            binary,
            &options["--console-helper"],
            &spice_path,
            &console_path,
            log_prefix.with_extension("console.log"),
        )?;
        qmp.call("cont")?;
        Some(broker)
    } else {
        None
    };
    let status = qmp.call("query-status")?;
    let mut state = json!({"event":"state","state":if status["running"]==true {"running"}else{"paused"},"qmp_status":status["status"],"guest_ready":null,"vm_id":config.id,"backend_pid":child.0.id(),"spice_socket":spice_path,"serial_log":serial_log,"stderr_log":stderr_log,"log_limit_bytes_per_stream":8388608});
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
    emit(state.clone())?;
    let mut status_poll = Instant::now();
    let mut capture: Option<Capture> = None;
    let mut frames = vm_control::input::Frames::default();
    let mut input = InputState::default();
    loop {
        if input.until.is_some_and(|until| Instant::now() >= until) {
            input.cleanup(&mut qmp)?;
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
        let result: Result<Value> = match request["method"].as_str() {
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
            Some("input_step") => {
                if broker.is_none() {Err(anyhow::anyhow!("persistent console unavailable"))}
                else {input.step(&request["arguments"],&frames,&console_path,&mut qmp)}
            },
            Some("input_cleanup") => {
                if input.operation.as_deref()==request["arguments"]["operation_id"].as_str() {input.cleanup(&mut qmp).map(|_|json!({"released":true}))}
                else {Ok(json!({"released":false,"reason":"operation is no longer active"}))}
            },
            Some("status") => qmp.call("query-status").map(|status|json!({"qmp_status":status,"guest_ready":null,"vm_id":config.id})),
            Some("shutdown") => qmp.call("system_powerdown").map(|_|json!({"requested":true,"mechanism":"qmp-system-powerdown","completed":false,"requires_guest_power_button_support":true})),
            Some("stop") => { emit(json!({"id":id,"result":{"requested":true,"forced":true}}))?; break },
            _ => Err(anyhow::anyhow!("unsupported QEMU worker operation")),
        };
        emit(match result {
            Ok(value) => json!({"id":id,"result":value}),
            Err(error) => json!({"id":id,"error":error.to_string()}),
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
    emit(
        json!({"event":"state","state":"stopped","reason":"supervisor_disconnected_or_stop","vm_id":config.id}),
    )?;
    Ok(())
}
fn main() {
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
        };
        let mut state = InputState {
            operation: Some("a".repeat(32)),
            until: Some(Instant::now()),
            held: std::collections::BTreeMap::from([(
                "left".into(),
                vm_control::input::button("left", false),
            )]),
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
        header[..4].copy_from_slice(&((vm_control::MAX_MESSAGE + 1) as u32).to_be_bytes());
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
}
