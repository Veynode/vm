//! The host-side publisher of an Apple Virtualization machine's ports: each
//! `{host, guest}` listens on `127.0.0.1:host` and splices to the Guest's
//! lease address at `guest`. It runs as threads of the daemon from
//! `machine.start` until `machine.stop`, `machine.rm` or the machine stops by
//! itself. QEMU machines forward their ports in the VM (user networking).
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// Running publishers by machine name.
static RUNNING: Mutex<Option<HashMap<String, Arc<AtomicBool>>>> = Mutex::new(None);

/// What a publisher needs of its machine.
#[derive(Clone)]
pub struct Published {
    /// The daemon's state directory, for its VM status.
    pub state_dir: PathBuf,
    pub name: String,
    pub mac_address: Option<String>,
    /// `(host, guest)` TCP ports.
    pub ports: Vec<(u16, u16)>,
}

/// Starts the machine's publisher, replacing a running one. Binding happens
/// here, so a host port in use fails `machine.start`.
pub fn start(machine: Published) -> Result<()> {
    stop(&machine.name);
    if machine.ports.is_empty() {
        return Ok(());
    }
    let mut listeners = Vec::new();
    for (host, guest) in &machine.ports {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, *host))
            .with_context(|| format!("bind 127.0.0.1:{host}"))?;
        listener.set_nonblocking(true)?;
        listeners.push((listener, *guest));
    }
    let stopped = Arc::new(AtomicBool::new(false));
    RUNNING
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(machine.name.clone(), stopped.clone());
    thread::spawn(move || {
        if let Err(error) = run(&machine, listeners, &stopped) {
            crate::log_line(&format!("publisher of {}: {error:#}", machine.name));
        }
        let mut running = RUNNING.lock().unwrap();
        if let Some(map) = running.as_mut() {
            if map
                .get(&machine.name)
                .is_some_and(|s| Arc::ptr_eq(s, &stopped))
            {
                map.remove(&machine.name);
            }
        }
    });
    Ok(())
}

fn vm_state(state_dir: &Path, name: &str) -> String {
    crate::call(state_dir, "status", json!({"id":name}))
        .ok()
        .and_then(|v| v["state"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

/// The guest's lease, waited for while the machine runs.
fn wait_lease(machine: &Published) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        if let Some(ip) = super::machine_guest::lease(&machine.name, machine.mac_address.as_deref())
        {
            return Ok(ip);
        }
        let state = vm_state(&machine.state_dir, &machine.name);
        if !matches!(state.as_str(), "running" | "starting" | "created") {
            bail!(
                "{} stopped before it took a DHCP lease ({state})",
                machine.name
            )
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {}'s DHCP lease", machine.name)
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// Stops the machine's publisher, if one runs; its connections end.
pub fn stop(name: &str) {
    if let Some(stopped) = RUNNING
        .lock()
        .unwrap()
        .as_mut()
        .and_then(|m| m.remove(name))
    {
        stopped.store(true, Ordering::SeqCst);
    }
}

/// Whether a publisher runs for `name`.
pub fn running(name: &str) -> bool {
    RUNNING
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|m| m.contains_key(name))
}

fn run(
    machine: &Published,
    listeners: Vec<(TcpListener, u16)>,
    stopped: &Arc<AtomicBool>,
) -> Result<()> {
    let ip = wait_lease(machine)?;
    let targets: Vec<(TcpListener, SocketAddr)> = listeners
        .into_iter()
        .map(|(listener, guest)| Ok((listener, format!("{ip}:{guest}").parse()?)))
        .collect::<Result<_>>()?;
    let mut checked = Instant::now();
    while !stopped.load(Ordering::SeqCst) {
        let mut idle = true;
        for (listener, target) in &targets {
            match listener.accept() {
                Ok((client, _)) => {
                    idle = false;
                    relay(client, *target, stopped.clone());
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => {}
            }
        }
        if checked.elapsed() > Duration::from_secs(2) {
            checked = Instant::now();
            if vm_state(&machine.state_dir, &machine.name) != "running" {
                return Ok(());
            }
        }
        if idle {
            thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(())
}

fn relay(client: TcpStream, target: SocketAddr, stopped: Arc<AtomicBool>) {
    let _ = client.set_nonblocking(false);
    thread::spawn(move || {
        let upstream = match TcpStream::connect_timeout(&target, Duration::from_secs(5)) {
            Ok(upstream) => upstream,
            Err(_) => {
                let _ = client.shutdown(Shutdown::Both);
                return;
            }
        };
        let (Ok(a), Ok(b)) = (client.try_clone(), upstream.try_clone()) else {
            return;
        };
        // A stopped publisher cuts its connections.
        let (watch_a, watch_b) = (client.try_clone(), upstream.try_clone());
        thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(200));
                if watch_a.as_ref().is_ok_and(|s| s.peer_addr().is_err()) {
                    return;
                }
            }
            for s in [watch_a, watch_b].into_iter().flatten() {
                let _ = s.shutdown(Shutdown::Both);
            }
        });
        thread::spawn(move || splice(b, a));
        splice(client, upstream);
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_carries_bytes_and_half_closes() {
        let echo = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = echo.local_addr().unwrap();
        thread::spawn(move || {
            let (mut s, _) = echo.accept().unwrap();
            let mut got = Vec::new();
            s.read_to_end(&mut got).unwrap();
            s.write_all(&got).unwrap();
        });
        let front = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = front.local_addr().unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let relay_stopped = stopped.clone();
        thread::spawn(move || {
            let (client, _) = front.accept().unwrap();
            relay(client, target, relay_stopped);
        });
        let mut client = TcpStream::connect(address).unwrap();
        client.write_all(b"published").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut back = Vec::new();
        client.read_to_end(&mut back).unwrap();
        assert_eq!(back, b"published");
        stopped.store(true, Ordering::SeqCst);
    }
}
