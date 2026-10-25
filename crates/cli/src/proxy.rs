//! Host-side TCP publishing for Apple Virtualization containers.
//!
//! VZ NAT guests are reachable from the host at their DHCP address, so `-p`
//! is a small detached helper (`veynodevm __proxy <id>`) that listens on the
//! requested host address and splices connections to `<guest-ip>:<port>`. It
//! exits when the container's VM stops. QEMU containers use slirp hostfwd.
use crate::container::{self, Container};
use anyhow::{Context, Result};
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    process::{Command, Stdio},
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
        thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let target = (ip.as_str(), guest_port);
                match TcpStream::connect_timeout(
                    &std::net::ToSocketAddrs::to_socket_addrs(&target)
                        .ok()
                        .and_then(|mut a| a.next())
                        .unwrap(),
                    Duration::from_secs(5),
                ) {
                    Ok(upstream) => {
                        let (a, b) = match (client.try_clone(), upstream.try_clone()) {
                            (Ok(a), Ok(b)) => (a, b),
                            _ => continue,
                        };
                        thread::spawn(move || splice(client, upstream));
                        thread::spawn(move || splice(b, a));
                    }
                    Err(e) => {
                        eprintln!("connect {ip}:{guest_port}: {e}");
                        let _ = client.shutdown(Shutdown::Both);
                    }
                }
            }
        });
    }
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
    }
}
