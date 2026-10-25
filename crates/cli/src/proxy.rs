//! Host-side TCP publishing for Apple Virtualization containers.
//!
//! VZ NAT guests are reachable from the host at their DHCP address, so `-p`
//! is a small detached helper (`veynodevm __proxy <id>`) that listens on the
//! requested host address and splices connections to `<guest-ip>:<port>`. It
//! exits when the container's VM stops. QEMU containers use slirp hostfwd.
use crate::container::{self, Container};
use anyhow::{Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
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
    let mut command = Command::new(std::env::current_exe()?);
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

fn splice(mut from: TcpStream, mut to: TcpStream) {
    let mut buffer = [0u8; 65536];
    loop {
        match from.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to.write_all(&buffer[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = to.shutdown(Shutdown::Write);
    let _ = from.shutdown(Shutdown::Read);
}

/// Accept on `listener` until `stop` is set, splicing to `ip:guest_port`.
fn serve(listener: TcpListener, ip: String, guest_port: u16, stop: Arc<AtomicBool>) {
    let _ = listener.set_nonblocking(true);
    let target = match format!("{ip}:{guest_port}").parse::<SocketAddr>() {
        Ok(t) => t,
        Err(_) => return,
    };
    while !stop.load(Ordering::SeqCst) {
        let client = match listener.accept() {
            Ok((client, _)) => client,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(_) => continue,
        };
        let _ = client.set_nonblocking(false);
        match TcpStream::connect_timeout(&target, Duration::from_secs(5)) {
            Ok(upstream) => {
                let (a, b) = match (client.try_clone(), upstream.try_clone()) {
                    (Ok(a), Ok(b)) => (a, b),
                    _ => continue,
                };
                thread::spawn(move || splice(client, upstream));
                thread::spawn(move || splice(b, a));
            }
            Err(e) => {
                eprintln!("connect {target}: {e}");
                let _ = client.shutdown(Shutdown::Both);
            }
        }
    }
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

fn engine_ports() -> Option<BTreeSet<u16>> {
    let docker = veynodevm_oci::store::which("docker")?;
    let output = Command::new(docker)
        .args(["ps", "--format", "{{.Ports}}"])
        .envs(crate::engine::env().ok()?)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| docker_published(&String::from_utf8_lossy(&output.stdout)))
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
    eprintln!("publishing {} port(s) to {ip}", listeners.len());
    for (listener, guest_port) in listeners {
        let ip = ip.clone();
        let never = Arc::new(AtomicBool::new(false));
        thread::spawn(move || serve(listener, ip, guest_port, never));
    }
    // The engine also follows ports published by its inner containers
    // (`docker run -p`), always on host loopback.
    let reserved: BTreeSet<u16> = container.ports.iter().map(|p| p.host_port).collect();
    let mut dynamic: BTreeMap<u16, Arc<AtomicBool>> = BTreeMap::new();
    // Exit with the VM; a missing record (container removed) also ends it.
    loop {
        thread::sleep(Duration::from_secs(2));
        let current = match container::find(id) {
            Ok(c) => c,
            Err(_) => return Ok(()),
        };
        let state = container::vm_state(&current);
        if matches!(state.as_str(), "stopped" | "crashed" | "missing") {
            return Ok(());
        }
        if !container.engine {
            continue;
        }
        let Some(wanted) = engine_ports() else {
            continue;
        };
        for (port, stop) in &dynamic {
            if !wanted.contains(port) {
                stop.store(true, Ordering::SeqCst);
                eprintln!("unpublished {port}");
            }
        }
        dynamic.retain(|port, _| wanted.contains(port));
        for port in wanted {
            if dynamic.contains_key(&port) || reserved.contains(&port) {
                continue;
            }
            match TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)) {
                Ok(listener) => {
                    let stop = Arc::new(AtomicBool::new(false));
                    dynamic.insert(port, stop.clone());
                    let ip = ip.clone();
                    thread::spawn(move || serve(listener, ip, port, stop));
                    eprintln!("published engine port 127.0.0.1:{port}");
                }
                Err(e) => {
                    // Not retried until the inner container stops publishing it.
                    dynamic.insert(port, Arc::new(AtomicBool::new(true)));
                    eprintln!("cannot publish engine port {port}: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn docker_ports() {
        let p = super::docker_published("0.0.0.0:8080->80/tcp, [::]:8080->80/tcp, 127.0.0.1:9000->9000/tcp, 0.0.0.0:53->53/udp\n0.0.0.0:7000-7002->7000-7002/tcp, 6379/tcp");
        assert_eq!(
            p.into_iter().collect::<Vec<_>>(),
            [7000, 7001, 7002, 8080, 9000]
        );
    }
}
