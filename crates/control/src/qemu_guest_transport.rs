//! One private, observer-only virtio-serial incarnation. No reconnect or replay.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    #[serde(rename = "type")]
    kind: String,
    guest_nonce: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_nonce: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response: Option<Value>,
}
pub(crate) fn nonce() -> Result<String> {
    let mut b = [0; 32];
    File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|b| format!("{b:02x}")).collect())
}
fn valid_nonce(v: &str) -> bool {
    v.len() == 64
        && v.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn wait_ready(s: &UnixStream, event: i16, deadline: Instant) -> Result<()> {
    use std::os::fd::AsRawFd;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("guest transport deadline")?;
        let mut p = libc::pollfd {
            fd: s.as_raw_fd(),
            events: event,
            revents: 0,
        };
        let timeout = remaining.as_millis().clamp(1, 50) as i32;
        let rc = unsafe { libc::poll(&mut p, 1, timeout) };
        if rc > 0 {
            return Ok(());
        }
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e.into());
            }
        }
    }
}
pub(super) fn read_exact(s: &mut UnixStream, b: &mut [u8], deadline: Instant) -> Result<()> {
    s.set_nonblocking(true)?;
    let mut n = 0;
    while n < b.len() {
        if Instant::now() >= deadline {
            bail!("guest transport deadline")
        }
        match s.read(&mut b[n..]) {
            Ok(0) => bail!("guest transport EOF"),
            Ok(got) => n += got,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                wait_ready(s, libc::POLLIN, deadline)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn read(s: &mut UnixStream, deadline: Instant) -> Result<Envelope> {
    let mut h = [0; 4];
    read_exact(s, &mut h, deadline)?;
    let n = u32::from_be_bytes(h) as usize;
    if n == 0 || n > 1048576 {
        bail!("guest transport response size")
    };
    let mut b = vec![0; n];
    read_exact(s, &mut b, deadline)?;
    serde_json::from_slice(&b).context("invalid guest transport envelope")
}
fn write(s: &mut UnixStream, e: &Envelope, deadline: Instant) -> Result<()> {
    let b = serde_json::to_vec(e)?;
    if b.len() > 65536 {
        bail!("guest transport request size")
    };
    let mut framed = (b.len() as u32).to_be_bytes().to_vec();
    framed.extend(b);
    s.set_nonblocking(true)?;
    let mut n = 0;
    while n < framed.len() {
        if Instant::now() >= deadline {
            bail!("guest transport deadline")
        }
        match s.write(&framed[n..]) {
            Ok(0) => bail!("guest transport short write"),
            Ok(got) => n += got,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                wait_ready(s, libc::POLLOUT, deadline)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
struct Connection {
    stream: UnixStream,
    guest: String,
    host: String,
    sequence: u64,
}
impl Connection {
    fn handshake(mut stream: UnixStream, deadline: Instant) -> Result<Self> {
        let challenge = read(&mut stream, deadline)?;
        if challenge.version != 1
            || challenge.kind != "challenge"
            || !valid_nonce(&challenge.guest_nonce)
            || challenge.host_nonce.is_some()
            || challenge.sequence.is_some()
            || challenge.request.is_some()
            || challenge.response.is_some()
        {
            bail!("guest challenge rejected")
        }
        let host = nonce()?;
        let guest = challenge.guest_nonce;
        write(
            &mut stream,
            &Envelope {
                version: 1,
                kind: "hello".into(),
                guest_nonce: guest.clone(),
                host_nonce: Some(host.clone()),
                sequence: None,
                request: None,
                response: None,
            },
            deadline,
        )?;
        let ready = read(&mut stream, deadline)?;
        if ready.version != 1
            || ready.kind != "ready"
            || ready.guest_nonce != guest
            || ready.host_nonce.as_deref() != Some(&host)
            || ready.sequence.is_some()
            || ready.request.is_some()
            || ready.response.is_some()
        {
            bail!("guest handshake rejected")
        }
        Ok(Self {
            stream,
            guest,
            host,
            sequence: 0,
        })
    }
    fn info(&mut self, request: Value, deadline: Instant) -> Result<Value> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("guest sequence exhausted")?;
        write(
            &mut self.stream,
            &Envelope {
                version: 1,
                kind: "request".into(),
                guest_nonce: self.guest.clone(),
                host_nonce: Some(self.host.clone()),
                sequence: Some(self.sequence),
                request: Some(request.clone()),
                response: None,
            },
            deadline,
        )?;
        let r = read(&mut self.stream, deadline)?;
        if r.version != 1
            || r.kind != "response"
            || r.guest_nonce != self.guest
            || r.host_nonce.as_deref() != Some(&self.host)
            || r.sequence != Some(self.sequence)
            || r.request.is_some()
        {
            bail!("guest response binding rejected")
        }
        let reply = r.response.context("missing guest response")?;
        if reply["id"] != request["id"]
            || !reply.is_object()
            || reply.get("result").is_some() == reply.get("error").is_some()
        {
            bail!("guest response id/result rejected")
        }
        // Independently enforce the observer contract; a misconfigured service is not authority.
        if let Some(result) = reply.get("result") {
            if result["transport"] != "virtio-serial-private-observer-v1"
                || result["transport_reconnect"] != false
                || [
                    "exec",
                    "async_exec",
                    "shutdown",
                    "files",
                    "desktop",
                    "descendant_containment",
                ]
                .iter()
                .any(|k| result["capabilities"][k] != false)
            {
                bail!("guest observer capability contract rejected")
            }
        }
        Ok(reply)
    }
}
struct Task {
    request: Value,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    reply: mpsc::SyncSender<Result<Value>>,
}
#[derive(Default)]
pub(super) struct Lifetime {
    stopped: bool,
    stream: Option<UnixStream>,
}
impl Lifetime {
    pub(super) fn stop(&mut self) {
        self.stopped = true;
        if let Some(s) = self.stream.take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
    fn install(&mut self, s: &UnixStream) -> Result<()> {
        if self.stopped {
            bail!("guest transport cancelled before connect")
        };
        self.stream = Some(s.try_clone()?);
        Ok(())
    }
}
pub(super) fn connect_private(path: &Path, lifetime: &Mutex<Lifetime>) -> Result<UnixStream> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    };
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
        bail!("invalid private guest socket path")
    }
    addr.sun_family = libc::AF_UNIX as _;
    for (out, b) in addr.sun_path.iter_mut().zip(bytes) {
        *out = *b as _
    }
    let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    #[cfg(target_os = "macos")]
    {
        addr.sun_len = len as u8;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    stream.set_nonblocking(true)?;
    lifetime.lock().unwrap().install(&stream)?;
    let rc = unsafe { libc::connect(fd, (&addr as *const libc::sockaddr_un).cast(), len) };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        if !matches!(
            e.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EAGAIN)
        ) {
            return Err(e.into());
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if lifetime.lock().unwrap().stopped || Instant::now() >= deadline {
                bail!("guest socket connect cancelled or expired")
            }
            let mut p = libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut p, 1, 50) };
            if rc < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                };
                return Err(e.into());
            }
            if rc > 0 {
                if let Some(e) = stream.take_error()? {
                    return Err(e.into());
                };
                break;
            }
        }
    }
    if lifetime.lock().unwrap().stopped {
        bail!("guest socket cancelled")
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}
pub struct Pending {
    rx: mpsc::Receiver<Result<Value>>,
    cancelled: Arc<AtomicBool>,
    lifetime: Arc<Mutex<Lifetime>>,
    completed: bool,
}
impl Pending {
    pub fn poll(&mut self) -> Option<Result<Value>> {
        let result = match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err(anyhow::anyhow!("guest transport fenced")))
            }
            Err(mpsc::TryRecvError::Empty) => None,
        };
        if result.is_some() {
            self.completed = true
        };
        result
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        if !self.completed {
            self.cancelled.store(true, Ordering::Release);
            self.lifetime.lock().unwrap().stop();
        }
    }
}
pub struct Transport {
    tx: mpsc::SyncSender<Task>,
    state: Arc<Mutex<&'static str>>,
    lifetime: Arc<Mutex<Lifetime>>,
    busy: Arc<AtomicBool>,
}
impl Transport {
    pub fn state(&self) -> &'static str {
        *self.state.lock().unwrap()
    }
    pub fn start(path: &Path) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Task>(1);
        let state = Arc::new(Mutex::new("starting"));
        let status = state.clone();
        let lifetime = Arc::new(Mutex::new(Lifetime::default()));
        let saved = lifetime.clone();
        let busy = Arc::new(AtomicBool::new(false));
        let active = busy.clone();
        let path = path.to_owned();
        thread::spawn(move || {
            let run = || -> Result<()> {
                if saved.lock().unwrap().stopped {
                    bail!("guest transport cancelled")
                }
                let stream = connect_private(&path, &saved)?;
                let mut conn =
                    Connection::handshake(stream, Instant::now() + Duration::from_secs(120))?;
                *status.lock().unwrap() = "ready";
                while let Ok(task) = rx.recv() {
                    if task.cancelled.load(Ordering::Acquire)
                        || saved.lock().unwrap().stopped
                        || Instant::now() >= task.deadline
                    {
                        bail!("guest request cancelled or expired before dispatch")
                    }
                    let result = conn.info(task.request, task.deadline);
                    let failed = result.is_err();
                    active.store(false, Ordering::Release);
                    let _ = task.reply.send(result);
                    if failed {
                        bail!("guest integration fenced")
                    }
                }
                Ok(())
            };
            let _ = run();
            *status.lock().unwrap() = "fenced";
            saved.lock().unwrap().stop();
            active.store(false, Ordering::Release);
        });
        Self {
            tx,
            state,
            lifetime,
            busy,
        }
    }
    pub fn request(&self, request: Value) -> Result<Pending> {
        let deadline = Instant::now() + Duration::from_secs(3);
        if request["method"] != "info" {
            bail!("unsupported: QEMU guest integration is observer-only")
        }
        let obj = request.as_object().context("invalid guest info request")?;
        if obj.len() != 2
            || request["id"]
                .as_str()
                .is_none_or(|id| id.is_empty() || id.len() > 128)
        {
            bail!("invalid guest info request")
        }
        if *self.state.lock().unwrap() != "ready" || self.lifetime.lock().unwrap().stopped {
            bail!("guest integration unavailable: not ready or permanently fenced")
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            bail!("guest integration busy: one in-flight request")
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        if self
            .tx
            .try_send(Task {
                request,
                deadline,
                cancelled: cancelled.clone(),
                reply: tx,
            })
            .is_err()
        {
            self.busy.store(false, Ordering::Release);
            bail!("guest integration fenced")
        }
        Ok(Pending {
            rx,
            cancelled,
            lifetime: self.lifetime.clone(),
            completed: false,
        })
    }
}
impl Drop for Transport {
    fn drop(&mut self) {
        self.lifetime.lock().unwrap().stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn envelope(kind: &str) -> Envelope {
        Envelope {
            version: 1,
            kind: kind.into(),
            guest_nonce: "a".repeat(64),
            host_nonce: None,
            sequence: None,
            request: None,
            response: None,
        }
    }
    fn server_handshake(s: &mut UnixStream) -> Envelope {
        let d = Instant::now() + Duration::from_secs(2);
        write(s, &envelope("challenge"), d).unwrap();
        let hello = read(s, d).unwrap();
        let mut ready = envelope("ready");
        ready.host_nonce = hello.host_nonce.clone();
        write(s, &ready, d).unwrap();
        hello
    }
    fn response(hello: &Envelope, request: Envelope) -> Envelope {
        Envelope {
            version: 1,
            kind: "response".into(),
            guest_nonce: hello.guest_nonce.clone(),
            host_nonce: hello.host_nonce.clone(),
            sequence: request.sequence,
            request: None,
            response: Some(
                json!({"id":request.request.unwrap()["id"],"result":{"transport":"virtio-serial-private-observer-v1","transport_reconnect":false,"capabilities":{"exec":false,"async_exec":false,"files":false,"shutdown":false,"desktop":false,"descendant_containment":false}}}),
            ),
        }
    }
    #[test]
    fn private_unix_binding_and_monotonic_requests() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let server = thread::spawn(move || {
            let hello = server_handshake(&mut b);
            for seq in 1..=2 {
                let r = read(&mut b, Instant::now() + Duration::from_secs(2)).unwrap();
                assert_eq!(r.sequence, Some(seq));
                assert_eq!(r.host_nonce, hello.host_nonce);
                write(
                    &mut b,
                    &response(&hello, r),
                    Instant::now() + Duration::from_secs(2),
                )
                .unwrap();
            }
        });
        let mut c = Connection::handshake(a, Instant::now() + Duration::from_secs(2)).unwrap();
        for id in ["one", "two"] {
            assert_eq!(
                c.info(
                    json!({"id":id,"method":"info"}),
                    Instant::now() + Duration::from_secs(2)
                )
                .unwrap()["id"],
                id
            )
        }
        server.join().unwrap();
    }
    #[test]
    fn rejects_foreign_or_stale_response_and_mutating_capabilities() {
        for fault in ["nonce", "sequence", "id", "caps", "partial"] {
            let (a, mut b) = UnixStream::pair().unwrap();
            let server = thread::spawn(move || {
                let hello = server_handshake(&mut b);
                let request = read(&mut b, Instant::now() + Duration::from_secs(2)).unwrap();
                let mut reply = response(&hello, request);
                match fault {
                    "nonce" => reply.host_nonce = Some("f".repeat(64)),
                    "sequence" => reply.sequence = Some(2),
                    "id" => reply.response.as_mut().unwrap()["id"] = json!("old"),
                    "caps" => {
                        reply.response.as_mut().unwrap()["result"]["capabilities"]["exec"] =
                            json!(true)
                    }
                    _ => {
                        b.write_all(&[0, 0]).unwrap();
                        return;
                    }
                }
                write(&mut b, &reply, Instant::now() + Duration::from_secs(2)).unwrap();
            });
            let mut c = Connection::handshake(a, Instant::now() + Duration::from_secs(2)).unwrap();
            assert!(
                c.info(
                    json!({"id":"one","method":"info"}),
                    Instant::now() + Duration::from_secs(2)
                )
                .is_err(),
                "{fault}"
            );
            server.join().unwrap();
        }
    }
    #[test]
    fn actor_is_permanently_fenced_after_eof_and_never_reconnects() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::Builder::new()
            .prefix("vn-serial-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("agent");
        let l = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let _ = server_handshake(&mut s);
            let _ = read(&mut s, Instant::now() + Duration::from_secs(2)).unwrap();
            drop(s);
            l.set_nonblocking(true).unwrap();
            thread::sleep(Duration::from_millis(100));
            assert!(l.accept().is_err());
        });
        let t = Transport::start(&path);
        let d = Instant::now() + Duration::from_secs(2);
        while *t.state.lock().unwrap() == "starting" && Instant::now() < d {
            thread::sleep(Duration::from_millis(1))
        }
        assert!(t.request(json!({"id":"bad","method":"exec"})).is_err());
        let mut pending = t.request(json!({"id":"one","method":"info"})).unwrap();
        let response = loop {
            if let Some(r) = pending.poll() {
                break r;
            };
            assert!(Instant::now() < d);
            thread::sleep(Duration::from_millis(1))
        };
        assert!(response.is_err());
        while *t.state.lock().unwrap() != "fenced" && Instant::now() < d {
            thread::sleep(Duration::from_millis(1))
        }
        assert!(t.request(json!({"id":"two","method":"info"})).is_err());
        server.join().unwrap();
    }
    #[test]
    fn absolute_read_budget_rejects_slow_partial_frame() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let server = thread::spawn(move || {
            b.write_all(&[0]).unwrap();
            thread::sleep(Duration::from_millis(100));
        });
        assert!(read(&mut a, Instant::now() + Duration::from_millis(20)).is_err());
        server.join().unwrap();
    }
    #[test]
    fn malformed_and_oversized_challenges_fail_before_hello() {
        for b in [1048577u32.to_be_bytes().to_vec(), {
            let text = br#"{"version":1,"type":"challenge","guest_nonce":"a","extra":true}"#;
            let mut b = (text.len() as u32).to_be_bytes().to_vec();
            b.extend(text);
            b
        }] {
            let (a, mut s) = UnixStream::pair().unwrap();
            s.write_all(&b).unwrap();
            assert!(Connection::handshake(a, Instant::now() + Duration::from_secs(1)).is_err());
        }
    }
    #[test]
    fn pending_cancel_fences_stream_and_rejects_second_inflight() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::Builder::new()
            .prefix("vn-agent-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("agent");
        let listener = UnixListener::bind(&path).unwrap();
        let (seen_tx, seen_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            server_handshake(&mut s);
            read(&mut s, Instant::now() + Duration::from_secs(2)).unwrap();
            seen_tx.send(()).unwrap();
            let mut b = [0];
            s.set_nonblocking(false).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            assert_eq!(s.read(&mut b).unwrap(), 0);
        });
        let t = Transport::start(&path);
        let deadline = Instant::now() + Duration::from_secs(2);
        while *t.state.lock().unwrap() == "starting" {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1))
        }
        let pending = t.request(json!({"id":"one","method":"info"})).unwrap();
        seen_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(t.request(json!({"id":"two","method":"info"})).is_err());
        drop(pending);
        assert!(t.request(json!({"id":"three","method":"info"})).is_err());
        server.join().unwrap();
    }
    #[test]
    fn drop_before_socket_install_cannot_resurrect_incarnation() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut lifetime = Lifetime::default();
        lifetime.stop();
        assert!(lifetime.install(&a).is_err());
        drop(a);
        let mut byte = [0];
        assert_eq!(b.read(&mut byte).unwrap(), 0);
    }
    #[test]
    fn drop_during_handshake_closes_owned_socket_promptly() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::Builder::new()
            .prefix("vn-agent-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("agent");
        let l = UnixListener::bind(&path).unwrap();
        let t = Transport::start(&path);
        let (mut s, _) = l.accept().unwrap();
        drop(t);
        s.set_nonblocking(true).unwrap();
        let until = Instant::now() + Duration::from_secs(1);
        let mut b = [0];
        loop {
            match s.read(&mut b) {
                Ok(n) => {
                    assert_eq!(n, 0);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    wait_ready(&s, libc::POLLIN, until).unwrap()
                }
                Err(e) => panic!("{e}"),
            }
        }
    }
    #[test]
    fn expired_dispatch_budget_never_writes_request() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let server = thread::spawn(move || {
            server_handshake(&mut b);
            b.set_nonblocking(false).unwrap();
            b.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
            let mut byte = [0];
            assert!(b.read(&mut byte).is_err());
        });
        let mut c = Connection::handshake(a, Instant::now() + Duration::from_secs(2)).unwrap();
        assert!(c
            .info(
                json!({"id":"old","method":"info"}),
                Instant::now() - Duration::from_millis(1)
            )
            .is_err());
        server.join().unwrap();
    }
    #[test]
    #[ignore = "requires explicitly built Go protocol test executable, no VM"]
    fn real_go_service_interoperability() {
        let binary = std::env::var_os("VEYNODE_SERIAL_GO_TEST_BIN").expect("Go test binary path");
        let dir = tempfile::Builder::new()
            .prefix("vn-go-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("agent.sock");
        let mut child = std::process::Command::new(binary)
            .arg("-test.run=^TestSerialInteropService$")
            .env("VEYNODE_SERIAL_INTEROP_SOCKET", &path)
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        struct Reap<'a>(&'a mut std::process::Child);
        impl Drop for Reap<'_> {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let owned = Reap(&mut child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        let t = Transport::start(&path);
        while *t.state.lock().unwrap() == "starting" {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        for id in ["first", "second"] {
            let mut pending = t.request(json!({"id":id,"method":"info"})).unwrap();
            let value = loop {
                if let Some(r) = pending.poll() {
                    break r.unwrap();
                };
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(2));
            };
            assert_eq!(value["id"], id);
            assert_eq!(value["result"]["capabilities"]["exec"], false);
        }
        drop(t);
        loop {
            if let Some(status) = owned.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            };
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
    }
}
