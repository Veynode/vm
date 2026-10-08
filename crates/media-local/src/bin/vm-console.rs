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
    path::PathBuf,
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
    pending: Mutex<BTreeMap<String, mpsc::SyncSender<Value>>>,
    next: AtomicU64,
    alive: AtomicBool,
    ready: (Mutex<bool>, Condvar),
    owner: Mutex<Option<Owner>>,
    next_epoch: AtomicU64,
    snapshot: Mutex<()>,
    capabilities: Value,
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
            pending.insert(id.clone(), tx);
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
        } else {
            12
        }));
        self.pending.lock().unwrap().remove(&id);
        match reply {
            Ok(value) => {
                if let Some(error) = value.get("error") {
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
            let result = match request.method.as_str() {
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
                "frame_status" => native.call(json!({"method":"frame_status"})),
                "screenshot" => match native.snapshot.try_lock() {
                    Ok(_guard) => native.call(json!({"method":"screenshot"})),
                    Err(_) => Err(anyhow!("screenshot busy")),
                },
                _ => Err(anyhow!("unsupported console operation")),
            };
            let started = request.method == "start" && result.is_ok();
            let epoch = native
                .owner
                .lock()
                .unwrap()
                .as_ref()
                .filter(|o| o.client == client)
                .map_or(0, |o| o.epoch);
            output.deliver(Record::json(
                epoch,
                &match result {
                    Ok(value) => json!({"request_id":request.request_id,"result":value}),
                    Err(error) => {
                        json!({"request_id":request.request_id,"error":error.to_string()})
                    }
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
fn probe(path: &PathBuf) -> Result<Value> {
    let mut child = OwnedChild(
        Command::new(path)
            .arg("--probe-codecs")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(status) = child.0.try_wait()? {
            if !status.success() {
                bail!("native hardware probe failed")
            }
            break;
        }
        if Instant::now() >= until {
            bail!("native hardware probe timed out")
        }
        thread::sleep(Duration::from_millis(20));
    }
    let mut bytes = Vec::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .take(8193)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        bail!("hardware probe exceeds bound")
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    if value["h264"] != "verified-hardware" {
        bail!("hardware H264 fallback unavailable")
    }
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
    if args.len() != 4 {
        bail!("usage: vm-console NATIVE_ENCODER SPICE_SOCKET PRIVATE_CONSOLE_SOCKET")
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
        let result = (|| {
            loop {
                let record = read_record(&mut pipe)?;
                if record.kind == 2 {
                    let mut owner = receive.owner.lock().unwrap();
                    if let Some(owner) = owner.as_mut().filter(|o| o.epoch == record.epoch) {
                        owner.deliver(record);
                    }
                    continue;
                }
                let value = record.value()?;
                if let Some(id) = value["id"].as_str() {
                    if let Some(tx) = receive.pending.lock().unwrap().remove(id) {
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
        if let Err(error) = result {
            eprintln!("console reader stopped: {error}");
        }
        receive.alive.store(false, Ordering::Release);
        receive.ready.1.notify_all();
        for (_, tx) in std::mem::take(&mut *receive.pending.lock().unwrap()) {
            let _ = tx.send(json!({"error":"capture process unavailable"}));
        }
        if let Some(owner) = receive.owner.lock().unwrap().as_ref() {
            let _ = owner.output.socket.shutdown(std::net::Shutdown::Both);
        }
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
                thread::sleep(Duration::from_millis(20))
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
