//! Host-side TCP publishing for Apple Virtualization containers.
//!
//! VZ NAT guests are reachable from the host at their DHCP address, so `-p`
//! is a small detached helper (`vm-control __proxy <id>`) that listens on the
//! requested host address and splices connections to `<guest-ip>:<port>`. It
//! exits when the container's VM stops. QEMU containers use slirp hostfwd.
use super::container::{self, Container};
use anyhow::{Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    os::{
        fd::AsRawFd,
        unix::{fs::OpenOptionsExt, net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// Stop a previous publisher for this container (restart, stop or rm).
pub fn stop_existing(container: &Container) -> Result<()> {
    let pid_file = container::container_dir(&container.id)?.join("proxy.pid");
    if let Ok(text) = std::fs::read_to_string(&pid_file) {
        if let Ok(pid) = text.trim().parse::<i32>() {
            // Only signal our own helper: confirm its command line first.
            let output = Command::new("ps")
                .args(["-o", "command=", "-p", &pid.to_string()])
                .output()?;
            let command = String::from_utf8_lossy(&output.stdout);
            if command.contains("__proxy") && command.contains(&container.id) {
                unsafe { libc::kill(pid, libc::SIGTERM) };
                for _ in 0..50 {
                    if unsafe { libc::kill(pid, 0) } != 0 {
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
        let _ = std::fs::remove_file(pid_file);
    }
    Ok(())
}

pub fn spawn(container: &Container) -> Result<()> {
    stop_existing(container)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(container::container_dir(&container.id)?.join("proxy.log"))?;
    let mut command = Command::new(super::core_exe()?);
    command
        .args(["__proxy", &container.id])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = command.spawn().context("start port publisher")?;
    std::fs::write(
        container::container_dir(&container.id)?.join("proxy.pid"),
        child.id().to_string(),
    )?;
    Ok(())
}

/// Connect timeout to the Guest.
const CONNECT: Duration = Duration::from_secs(5);

/// Copies `from` to `to`. End of stream is passed on as a half-close, so the
/// other direction keeps flowing; an error ends both directions.
fn splice(mut from: TcpStream, mut to: TcpStream) {
    let mut buffer = [0u8; 65536];
    loop {
        match from.read(&mut buffer) {
            Ok(0) => {
                let _ = to.shutdown(Shutdown::Write);
                return;
            }
            Ok(n) => {
                if to.write_all(&buffer[..n]).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    // Wakes the other direction's read on `to` and closes `from`'s peer.
    let _ = to.shutdown(Shutdown::Both);
    let _ = from.shutdown(Shutdown::Both);
}

/// One accepted connection: connects to `target` and splices both ways (this
/// thread one way, a second thread the other).
fn forward(client: TcpStream, target: SocketAddr) {
    let _ = client.set_nonblocking(false);
    let _ = client.set_nodelay(true);
    let upstream = match TcpStream::connect_timeout(&target, CONNECT) {
        Ok(upstream) => upstream,
        Err(e) => {
            eprintln!("connect {target}: {e}");
            let _ = client.shutdown(Shutdown::Both);
            return;
        }
    };
    let _ = upstream.set_nodelay(true);
    let (Ok(client_back), Ok(upstream_back)) = (client.try_clone(), upstream.try_clone()) else {
        return;
    };
    thread::spawn(move || splice(upstream_back, client_back));
    splice(client, upstream);
}

/// Accepts on `listener` until `stop`'s peer is dropped, forwarding each
/// connection to `target` on its own thread. Blocks in `poll` between
/// connections: no wakeups and no added latency.
fn serve(listener: TcpListener, target: SocketAddr, stop: UnixStream) {
    if let Err(e) = listener.set_nonblocking(true) {
        eprintln!("listener for {target}: {e}");
        return;
    }
    let mut fds = [
        libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: stop.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // A failure streak is logged once and backed off, never fatal: the
    // listener must live as long as the publisher.
    let mut failing = false;
    loop {
        for fd in &mut fds {
            fd.revents = 0;
        }
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                back_off(&mut failing, target, "poll", &error);
            }
            continue;
        }
        if fds[1].revents != 0 {
            return;
        }
        match listener.accept() {
            Ok((client, _)) => {
                // Out of threads: the client is dropped (closed) and the loop backs off.
                match thread::Builder::new().spawn(move || forward(client, target)) {
                    Ok(_) => failing = false,
                    Err(e) => back_off(&mut failing, target, "connection thread", &e),
                }
            }
            // Gone before it was accepted, or a signal: wait again.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::ConnectionAborted
                ) => {}
            // Out of descriptors and the like: the listener stays readable,
            // so back off instead of spinning.
            Err(e) => back_off(&mut failing, target, "accept", &e),
        }
    }
}

/// Logs the first failure of a streak and backs off, so a persistent one
/// neither spins nor grows the log.
fn back_off(failing: &mut bool, target: SocketAddr, what: &str, error: &dyn std::fmt::Display) {
    if !std::mem::replace(failing, true) {
        eprintln!("{what} for {target}: {error}");
    }
    thread::sleep(Duration::from_millis(100));
}

/// Starts [`serve`] on its own thread. Dropping the returned stream stops it
/// and closes the listener.
fn start_serving(listener: TcpListener, target: SocketAddr) -> Result<UnixStream> {
    let (keep, stop) = UnixStream::pair()?;
    thread::spawn(move || serve(listener, target, stop));
    Ok(keep)
}

/// Parse `docker ps --format {{.Ports}}` output into published TCP host
/// ports: `0.0.0.0:8080->80/tcp, [::]:8080->80/tcp, 127.0.0.1:9000->9000/tcp`.
pub fn docker_published(ports: &str) -> BTreeSet<u16> {
    let mut out = BTreeSet::new();
    for entry in ports.split([',', '\n']).map(str::trim) {
        let Some((host, rest)) = entry.split_once("->") else {
            continue;
        };
        if !rest.ends_with("/tcp") {
            continue;
        }
        let host_port = host.rsplit(':').next().unwrap_or_default();
        if let Some((a, b)) = host_port.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.parse::<u16>(), b.parse::<u16>()) {
                out.extend((a..=b).take(64));
            }
        } else if let Ok(p) = host_port.parse() {
            out.insert(p);
        }
    }
    out
}

/// How often the publisher checks its VM's state (it exits when the VM
/// stops, releasing the host ports).
const TICK: Duration = Duration::from_secs(2);

/// The interval between the engine publisher's `docker ps` queries:
/// [`Self::MIN`] after a change, doubling to [`Self::MAX`] while nothing
/// changes. Inner container starts and stops also query at once.
#[derive(Debug)]
pub(crate) struct Backoff(Duration);
impl Backoff {
    pub(crate) const MIN: Duration = TICK;
    pub(crate) const MAX: Duration = Duration::from_secs(10);
    pub(crate) fn new() -> Self {
        Self(Self::MIN)
    }
    pub(crate) fn delay(&self) -> Duration {
        self.0
    }
    /// After a query: back to `MIN` when it saw a change, else twice as long.
    pub(crate) fn checked(&mut self, changed: bool) {
        self.0 = if changed {
            Self::MIN
        } else {
            (self.0 * 2).min(Self::MAX)
        };
    }
}

/// The publisher's view of the engine's own dockerd: the docker CLI and its
/// environment.
struct Engine {
    docker: std::path::PathBuf,
    env: Vec<(String, String)>,
    events: Option<Events>,
}

impl Engine {
    fn new() -> Option<Self> {
        let docker = veynodevm_oci::store::which("docker")?;
        let env = super::engine::env().ok()?;
        Some(Self {
            docker,
            env,
            events: None,
        })
    }

    /// How long one `docker ps` may take: one that stalls must not keep the
    /// loop from seeing the VM stop.
    const QUERY: Duration = Duration::from_secs(10);

    fn ports(&self) -> Option<BTreeSet<u16>> {
        let mut child = Command::new(&self.docker)
            .args(["ps", "--format", "{{.Ports}}"])
            .envs(self.env.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut stdout = child.stdout.take()?;
        let (done, read) = mpsc::channel();
        thread::spawn(move || {
            let mut text = String::new();
            let _ = done.send(stdout.read_to_string(&mut text).map(|_| text));
        });
        let Ok(Ok(text)) = read.recv_timeout(Self::QUERY) else {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        };
        child
            .wait()
            .ok()?
            .success()
            .then(|| docker_published(&text))
    }

    /// Waits up to `delay` for an inner container to start or stop; true
    /// when one did.
    fn wait(&mut self, delay: Duration) -> bool {
        let Some(events) = &self.events else {
            thread::sleep(delay);
            return false;
        };
        match events.seen.recv_timeout(delay) {
            Ok(()) => {
                while events.seen.try_recv().is_ok() {}
                true
            }
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The stream ended (engine restarting): poll until the next
                // check starts another.
                self.events = None;
                thread::sleep(delay);
                false
            }
        }
    }

    /// Follows `docker events` while the engine runs, so a newly published
    /// port is picked up at once rather than at the next check.
    fn follow(&mut self) {
        if self.events.is_none() {
            self.events = Events::start(&self.docker, &self.env);
        }
    }
}

/// `docker events` for inner container starts and stops.
struct Events {
    child: Child,
    seen: mpsc::Receiver<()>,
}

impl Events {
    fn start(docker: &std::path::Path, env: &[(String, String)]) -> Option<Self> {
        let mut child = Command::new(docker)
            .args([
                "events",
                "--format",
                "{{.Action}}",
                "--filter",
                "type=container",
                "--filter",
                "event=start",
                "--filter",
                "event=die",
            ])
            .envs(env.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        let (tx, seen) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if line.is_err() || tx.send(()).is_err() {
                    return;
                }
            }
        });
        Some(Self { child, seen })
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn run(id: &str) -> Result<()> {
    let container = container::find(id)?;
    let mut listeners = Vec::new();
    for port in &container.ports {
        let listener = TcpListener::bind((port.host_address, port.host_port))
            .with_context(|| format!("bind {}:{}", port.host_address, port.host_port))?;
        listeners.push((listener, port.guest_port));
    }
    let ip = container::wait_for_ip(&container, Duration::from_secs(180))?;
    let guest: std::net::IpAddr = ip.parse().with_context(|| format!("guest address {ip}"))?;
    eprintln!("publishing {} port(s) to {ip}", listeners.len());
    // The stop handles of listeners that live as long as this process.
    let mut keep = Vec::new();
    for (listener, guest_port) in listeners {
        let target = SocketAddr::new(guest, guest_port);
        keep.push(start_serving(listener, target)?);
    }
    // The engine also follows ports published by its inner containers
    // (`docker run -p`), always on host loopback.
    let mut engine = None;
    let reserved: BTreeSet<u16> = container.ports.iter().map(|p| p.host_port).collect();
    // A port with no stop handle could not be bound; it is not retried until
    // the inner container stops publishing it.
    let mut dynamic: BTreeMap<u16, Option<UnixStream>> = BTreeMap::new();
    let mut backoff = Backoff::new();
    let mut next_query = Instant::now();
    let mut last_state = String::new();
    // Exit with the VM; a missing record (container removed) also ends it.
    loop {
        let event = match &mut engine {
            Some(engine) => Engine::wait(engine, TICK),
            None => {
                thread::sleep(TICK);
                false
            }
        };
        let current = match container::find(id) {
            Ok(c) => c,
            Err(_) => return Ok(()),
        };
        let state = container::vm_state(&current);
        if matches!(state.as_str(), "stopped" | "crashed" | "missing") {
            return Ok(());
        }
        let now = Instant::now();
        if state != last_state {
            last_state.clone_from(&state);
            backoff = Backoff::new();
            next_query = now;
        }
        // Querying a VM that is not running would wait for it: only a running
        // engine is asked.
        if !container.engine || state != "running" || (!event && now < next_query) {
            continue;
        }
        if engine.is_none() {
            engine = Engine::new();
        }
        let wanted = engine.as_mut().and_then(|engine| {
            engine.follow();
            engine.ports()
        });
        let Some(wanted) = wanted else {
            // No docker CLI, or dockerd not answering: try again later.
            backoff.checked(false);
            next_query = Instant::now() + backoff.delay();
            continue;
        };
        let mut changed = false;
        for port in dynamic.keys().filter(|p| !wanted.contains(p)) {
            changed = true;
            eprintln!("unpublished {port}");
        }
        // Dropping a stop handle closes that listener.
        dynamic.retain(|port, _| wanted.contains(port));
        for port in wanted {
            if dynamic.contains_key(&port) || reserved.contains(&port) {
                continue;
            }
            changed = true;
            let served = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
                .map_err(anyhow::Error::from)
                .and_then(|listener| start_serving(listener, SocketAddr::new(guest, port)));
            match served {
                Ok(stop) => {
                    dynamic.insert(port, Some(stop));
                    eprintln!("published engine port 127.0.0.1:{port}");
                }
                Err(e) => {
                    dynamic.insert(port, None);
                    eprintln!("cannot publish engine port {port}: {e}");
                }
            }
        }
        backoff.checked(changed);
        next_query = Instant::now() + backoff.delay();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_ports() {
        let p = super::docker_published("0.0.0.0:8080->80/tcp, [::]:8080->80/tcp, 127.0.0.1:9000->9000/tcp, 0.0.0.0:53->53/udp\n0.0.0.0:7000-7002->7000-7002/tcp, 6379/tcp");
        assert_eq!(
            p.into_iter().collect::<Vec<_>>(),
            [7000, 7001, 7002, 8080, 9000]
        );
    }

    #[test]
    fn checks_back_off_until_something_changes() {
        let mut backoff = Backoff::new();
        let mut delays = vec![backoff.delay().as_secs()];
        for _ in 0..4 {
            backoff.checked(false);
            delays.push(backoff.delay().as_secs());
        }
        assert_eq!(delays, [2, 4, 8, 10, 10]);
        backoff.checked(true);
        assert_eq!(backoff.delay(), Backoff::MIN);
    }

    /// Data both ways, a client half-close reaches
    /// the server as end of stream while the reply still flows back, and
    /// dropping the stop handle closes the listener.
    #[test]
    fn splices_with_half_close_and_stops() {
        let server = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let target = server.local_addr().unwrap();
        let echo = thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            let mut request = Vec::new();
            stream.read_to_end(&mut request).unwrap();
            stream.write_all(b"got ").unwrap();
            stream.write_all(&request).unwrap();
        });
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let published = listener.local_addr().unwrap();
        let stop = start_serving(listener, target).unwrap();
        let mut client = TcpStream::connect(published).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        client.write_all(b"ping").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        assert_eq!(reply, "got ping");
        echo.join().unwrap();
        drop(stop);
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(published).is_ok() {
            assert!(Instant::now() < deadline, "listener still open");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
