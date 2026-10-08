//! Bounded pipe compatibility adapter; capture ownership stays with vm-console.
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use vm_media_local::console_wire::{read_record, Record};
enum Event {
    Record(Record),
    Command(String),
    Error,
}
fn request(socket: &mut UnixStream, id: &str, method: &str, codec: Option<&str>) -> Result<()> {
    let mut value = json!({"request_id":id,"method":method});
    if let Some(codec) = codec {
        value["codec"] = json!(codec);
    }
    serde_json::to_writer(&mut *socket, &value)?;
    socket.write_all(b"\n")?;
    Ok(())
}
fn output(record: Record) -> Result<()> {
    let size = record.body.len() - 9;
    let mut bytes = Vec::with_capacity(size + 13);
    bytes.extend_from_slice(&(size as u32).to_be_bytes());
    bytes.extend_from_slice(&record.body[..9]);
    bytes.extend_from_slice(&record.body[9..]);
    let mut stdout = std::io::stdout().lock();
    let mut offset = 0;
    let until = Instant::now() + Duration::from_millis(500);
    while offset < bytes.len() {
        match stdout.write(&bytes[offset..]) {
            Ok(0) => bail!("media pipe closed"),
            Ok(n) => offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= until {
                    bail!("media pipe write deadline")
                }
                let mut fd = libc::pollfd {
                    fd: libc::STDOUT_FILENO,
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
fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--probe-codecs" {
        let mut socket = UnixStream::connect(&args[2])?;
        socket.set_read_timeout(Some(Duration::from_secs(3)))?;
        socket.set_write_timeout(Some(Duration::from_millis(500)))?;
        request(&mut socket, "probe", "capabilities", None)?;
        let value = read_record(&mut socket)?.value()?;
        if value.get("error").is_some() {
            bail!("console capability query failed")
        }
        println!("{}", value["result"]);
        return Ok(());
    }
    if args.len() != 4 || args[2] != "--codec" || !matches!(args[3].as_str(), "hevc" | "h264") {
        bail!("usage: vm-console-stream CONSOLE_SOCKET --codec h264|hevc; or --probe-codecs CONSOLE_SOCKET")
    }
    let mut socket = UnixStream::connect(&args[1])?;
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    socket.set_write_timeout(Some(Duration::from_millis(500)))?;
    request(&mut socket, "start", "start", Some(&args[3]))?;
    let start = read_record(&mut socket)?;
    let reply = start.value()?;
    if reply["request_id"] != "start" || reply.get("error").is_some() {
        bail!(
            "console start failed: {}",
            reply.get("error").unwrap_or(&json!("unexpected response"))
        )
    }
    let epoch = reply["result"]["epoch"]
        .as_u64()
        .context("missing encoder epoch")?;
    if epoch == 0 || epoch != start.epoch {
        bail!("invalid encoder epoch")
    }
    let flags = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let (tx, rx) = mpsc::sync_channel(4);
    let incoming = tx.clone();
    let mut reader = socket.try_clone()?;
    thread::spawn(move || loop {
        match read_record(&mut reader) {
            Ok(record) => {
                if incoming.try_send(Event::Record(record)).is_err() {
                    let _ = reader.shutdown(std::net::Shutdown::Both);
                    break;
                }
            }
            Err(_) => {
                let _ = incoming.try_send(Event::Error);
                break;
            }
        }
    });
    thread::spawn(move || {
        let mut stdin = BufReader::new(std::io::stdin());
        loop {
            let mut line = Vec::new();
            match (&mut stdin).take(65).read_until(b'\n', &mut line) {
                Ok(0) => {
                    let _ = tx.send(Event::Command("stop".into()));
                    break;
                }
                Ok(n) if n <= 64 && line.last() == Some(&b'\n') => {
                    let cmd = String::from_utf8_lossy(&line).trim().to_owned();
                    if tx.send(Event::Command(cmd)).is_err() {
                        break;
                    }
                }
                _ => {
                    let _ = tx.send(Event::Error);
                    break;
                }
            }
        }
    });
    let result = (|| {
        let until = Instant::now() + Duration::from_secs(180);
        let mut key = 0u64;
        while Instant::now() < until {
            match rx.recv_timeout(Duration::from_secs(12)) {
                Ok(Event::Record(record)) => {
                    if record.kind == 2 {
                        if record.epoch != epoch {
                            bail!("cross-epoch media rejected")
                        }
                        output(record)?;
                    } else {
                        let value = record.value()?;
                        if value.get("error").is_some()
                            || value["event"] == "session-error"
                            || value["event"] == "capture-error"
                        {
                            bail!("console media operation failed")
                        }
                    }
                }
                Ok(Event::Command(command)) if command == "keyframe" => {
                    key += 1;
                    request(&mut socket, &format!("k{key}"), "keyframe", None)?;
                }
                Ok(Event::Command(command)) if command == "stop" => return Ok(()),
                _ => bail!("console media disconnected, stalled or invalid command"),
            }
        }
        Ok(())
    })();
    // Keep the record reader alive while draining pending AUs and awaiting stop ACK.
    request(&mut socket, "stop", "stop", None)?;
    let until = Instant::now() + Duration::from_millis(1500);
    let mut confirmed = false;
    while Instant::now() < until {
        match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(Event::Record(record)) if record.kind == 1 => {
                let value = record.value()?;
                if value["request_id"] == "stop" {
                    confirmed = value.get("error").is_none()
                        && (value["result"]["state"] == "stopped"
                            || value["result"]["stopped"] == true);
                    break;
                }
            }
            Ok(Event::Record(_)) | Ok(Event::Command(_)) => {}
            _ => break,
        }
    }
    let _ = socket.shutdown(std::net::Shutdown::Both);
    if !confirmed {
        bail!("native encoder stop unconfirmed")
    }
    if let Err(error) = result {
        eprintln!("console media ended: {error:#}");
    }
    // Exit zero attests acknowledged encoder cleanup, not successful video playback.
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("console stream: {error:#}");
        std::process::exit(1)
    }
}
