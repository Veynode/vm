//! Bounded pipe compatibility adapter; capture ownership stays with vm-console.
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};
use vm_media_local::console_wire::{read_record, Record};
enum Event {
    Record(Record),
    Command(String),
    Error,
}
fn native_failure_reason(value: &serde_json::Value) -> &'static str {
    // Native protocol messages must not become arbitrary log text. Keep a
    // small allowlist of actionable encoder failures; unknown text is redacted.
    match value["error"]["message"].as_str() {
        Some("HEVC geometry exceeds validated level4 bound") => "hevc_geometry_limit",
        Some("Owned scaling failed") => "owned_scaling_failed",
        Some("Source generation unavailable; start a fresh encoding epoch after replacement") => {
            "source_replaced"
        }
        Some("Scale buffer unavailable") => "scale_buffer_unavailable",
        Some("Encoder preparation failed") => "encoder_preparation_failed",
        Some("Frame submission failed") => "frame_submission_failed",
        Some("Frame completion failed") => "frame_completion_failed",
        Some("Actual HEVC SPS exceeds Main/tier0/level4 or has unsupported/malformed syntax") => {
            "hevc_profile_validation_failed"
        }
        Some("Encoder did not confirm hardware acceleration; software fallback rejected") => {
            "hardware_acceleration_unconfirmed"
        }
        _ => "unclassified_native_failure",
    }
}
fn request(socket: &mut UnixStream, id: &str, method: &str, codec: Option<&str>) -> Result<()> {
    let mut value = json!({"request_id":id,"method":method});
    if let Some(codec) = codec {
        value["codec"] = json!(codec);
        // A WebCodecs viewer decodes whatever level the SPS declares: 1080p instead of 720p.
        if method == "start" && codec == "h264" {
            value["level"] = json!(40);
        }
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
    // Binary media must bypass Rust's line-buffered stdout. A small AU or
    // bytes after the final 0x0a would otherwise wait for a later frame/exit.
    write_pipe_deadline(libc::STDOUT_FILENO, &bytes, Duration::from_millis(500))?;
    Ok(())
}
fn write_pipe_deadline(fd: libc::c_int, bytes: &[u8], timeout: Duration) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "media pipe must be nonblocking",
        ));
    }
    write_until(
        bytes,
        timeout,
        |part| {
            let n = unsafe { libc::write(fd, part.as_ptr().cast(), part.len()) };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        },
        |remaining| {
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            let millis = remaining.as_millis().saturating_add(1).min(10) as i32;
            let result = unsafe { libc::poll(&mut poll, 1, millis) };
            if result < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "media pipe closed",
                ));
            }
            Ok(())
        },
    )
}
// One deadline covers progress, interruptions and waiting. Failure may have
// written a partial record: never append another AU on this media pipe.
fn write_until(
    bytes: &[u8],
    timeout: Duration,
    mut write: impl FnMut(&[u8]) -> std::io::Result<usize>,
    mut wait: impl FnMut(Duration) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    let until = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "media pipe deadline overflow"))?;
    let mut offset = 0;
    while offset < bytes.len() {
        if Instant::now() >= until {
            return Err(Error::new(ErrorKind::TimedOut, "media pipe write deadline"));
        }
        match write(&bytes[offset..]) {
            Ok(0) => return Err(Error::new(ErrorKind::WriteZero, "media pipe closed")),
            Ok(n) => offset += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                match wait(until.saturating_duration_since(Instant::now())) {
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    other => other?,
                }
            }
            Err(e) => return Err(e),
        }
    }
    if Instant::now() >= until {
        return Err(Error::new(ErrorKind::TimedOut, "media pipe write deadline"));
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
    if !matches!(args.len(), 4 | 6)
        || args[2] != "--codec"
        || !matches!(args[3].as_str(), "hevc" | "h264")
    {
        bail!("usage: vm-console-stream CONSOLE_SOCKET --codec h264|hevc; or --probe-codecs CONSOLE_SOCKET")
    }
    let mut rate = if args.len() == 6 {
        if args[4] != "--rate-control-fd" {
            bail!("invalid rate option")
        }
        Some(unsafe { vm_media_local::rate_control::Channel::from_fd(args[5].parse()?) }?)
    } else {
        None
    };
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
    let mut epoch = reply["result"]["epoch"]
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
    let media_overflow = Arc::new(AtomicBool::new(false));
    let reader_overflow = media_overflow.clone();
    let incoming = tx.clone();
    let mut reader = socket.try_clone()?;
    thread::spawn(move || loop {
        match read_record(&mut reader) {
            Ok(record) => {
                // Overflow is terminal for this video stream, but closing the
                // bidirectional socket here also destroys its stop-ACK path.
                // Discard subsequent AUs while the owner requests stop; retain
                // the bounded control reply so cleanup can still be attested.
                if record.kind == 2 && reader_overflow.load(Ordering::Acquire) {
                    continue;
                }
                match incoming.try_send(Event::Record(record)) {
                    Ok(()) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                    Err(mpsc::TrySendError::Full(Event::Record(record))) => {
                        reader_overflow.store(true, Ordering::Release);
                        if record.kind != 2 && incoming.send(Event::Record(record)).is_err() {
                            break;
                        }
                    }
                    Err(mpsc::TrySendError::Full(_)) => unreachable!("only records are sent"),
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
        let mut key = 0u64;
        // A guest resolution change replaces the source and ends the encoding
        // epoch; the stream stops it and starts the next one (a new SPS).
        let mut restart = 0u64;
        let mut retired = Vec::new();
        let mut rate_id = 0u64;
        let mut rate_pending_id = None;
        let mut view_id = 0u64;
        let mut last_record = Instant::now();
        loop {
            if media_overflow.load(Ordering::Acquire) {
                bail!("console media queue overflow; terminating stream")
            }
            if let Some(channel) = &mut rate {
                if let Some(request) = channel.poll()? {
                    rate_id += 1;
                    let id = format!("rate{rate_id}");
                    let mut value = serde_json::to_value(request)?;
                    value["request_id"] = json!(id);
                    serde_json::to_writer(&mut socket, &value)?;
                    socket.write_all(b"\n")?;
                    rate_pending_id = Some(id);
                }
            }
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(Event::Record(record)) => {
                    last_record = Instant::now();
                    if record.kind == 2 {
                        if retired.contains(&record.epoch) {
                            continue;
                        }
                        if record.epoch != epoch {
                            bail!("cross-epoch media rejected")
                        }
                        output(record)?;
                    } else {
                        let value = record.value()?;
                        if rate_pending_id
                            .as_deref()
                            .is_some_and(|id| value["request_id"] == id)
                        {
                            let channel = rate.as_mut().context("missing rate controller")?;
                            if let Some(result) = value.get("result") {
                                let receipt: vm_media_local::rate_control::Receipt =
                                    serde_json::from_value(result.clone())?;
                                receipt.validate(
                                    &channel.pending.as_ref().context("missing rate request")?.0,
                                    epoch,
                                )?;
                                if record.epoch != epoch {
                                    bail!("stale rate epoch")
                                }
                            }
                            channel.reply(value)?;
                            rate_pending_id = None;
                            continue;
                        }
                        if value["request_id"] == format!("restop{restart}") {
                            if value.get("error").is_some() {
                                bail!("console media restart failed: stop refused")
                            }
                            request(
                                &mut socket,
                                &format!("restart{restart}"),
                                "start",
                                Some(&args[3]),
                            )?;
                            continue;
                        }
                        if value["request_id"] == format!("restart{restart}") {
                            let next = value["result"]["epoch"].as_u64();
                            if value.get("error").is_some()
                                || next != Some(record.epoch)
                                || next <= Some(epoch)
                            {
                                bail!("console media restart failed: start refused")
                            }
                            retired.push(epoch);
                            epoch = record.epoch;
                            continue;
                        }
                        if value["event"] == "session-error"
                            && record.epoch == epoch
                            && native_failure_reason(&value) == "source_replaced"
                        {
                            restart += 1;
                            request(&mut socket, &format!("restop{restart}"), "stop", None)?;
                            continue;
                        }
                        // A refused scale, crop or fps leaves the stream as it was.
                        if value["request_id"]
                            .as_str()
                            .is_some_and(|id| id.starts_with("view"))
                        {
                            if let Some(error) = value.get("error") {
                                eprintln!("{}", json!({"event":"rate","track":0,"error":error}));
                            }
                            continue;
                        }
                        // Same stderr line as vz-desktop-stream's.
                        if value["event"] == "view" {
                            eprintln!("{value}");
                            continue;
                        }
                        if value.get("error").is_some()
                            || value["event"] == "session-error"
                            || value["event"] == "capture-error"
                        {
                            bail!(
                                "console media operation failed: {}",
                                native_failure_reason(&value)
                            )
                        }
                    }
                }
                Ok(Event::Command(command)) if command == "keyframe" => {
                    key += 1;
                    request(&mut socket, &format!("k{key}"), "keyframe", None)?;
                }
                Ok(Event::Command(command))
                    if command.starts_with("scale ") || command.starts_with("crop ") =>
                {
                    let (word, fields) = command.split_once(' ').unwrap();
                    let fields: Vec<u64> = fields
                        .split(' ')
                        .map(str::parse)
                        .collect::<Result<_, _>>()
                        .context("invalid view command")?;
                    view_id += 1;
                    let id = format!("view{view_id}");
                    let value = match (word, &fields[..]) {
                        ("scale", [width, height]) => {
                            json!({"request_id":id,"method":"set_scale","width":width,"height":height})
                        }
                        ("crop", [x, y, width, height]) => {
                            json!({"request_id":id,"method":"set_crop","x":x,"y":y,"width":width,"height":height})
                        }
                        _ => bail!("invalid view command"),
                    };
                    serde_json::to_writer(&mut socket, &value)?;
                    socket.write_all(b"\n")?;
                }
                Ok(Event::Command(command)) if command.starts_with("fps ") => {
                    let fps: u64 = command[4..].parse().context("invalid fps command")?;
                    view_id += 1;
                    let value =
                        json!({"request_id":format!("view{view_id}"),"method":"set_fps","fps":fps});
                    serde_json::to_writer(&mut socket, &value)?;
                    socket.write_all(b"\n")?;
                }
                Ok(Event::Command(command)) if command == "stop" => return Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout)
                    if last_record.elapsed() < Duration::from_secs(12) => {}
                _ => bail!("console media disconnected, stalled or invalid command"),
            }
        }
    })();
    // Keep the record reader alive while draining pending AUs and awaiting stop ACK.
    if media_overflow.load(Ordering::Acquire) {
        eprintln!("console media queue overflow: video terminated, awaiting stop acknowledgement");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};
    #[test]
    fn native_failure_diagnostics_are_allowlisted_not_arbitrary_text() {
        assert_eq!(
            native_failure_reason(
                &json!({"error":{"message":"HEVC geometry exceeds validated level4 bound"}})
            ),
            "hevc_geometry_limit"
        );
        for value in [
            json!({}),
            json!({"error":{"message":123}}),
            json!({"error":{"message":"credential=do-not-log"}}),
        ] {
            assert_eq!(native_failure_reason(&value), "unclassified_native_failure");
        }
    }
    #[test]
    fn successful_partial_progress_does_not_reset_or_skip_deadline() {
        let mut writes = 0;
        let began = Instant::now();
        let error = write_until(
            &[1; 100],
            Duration::from_millis(30),
            |_| {
                writes += 1;
                thread::sleep(Duration::from_millis(10));
                Ok(1)
            },
            |_| panic!("no WouldBlock needed for deadline enforcement"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        assert!(writes > 0 && writes < 100);
        assert!(began.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn repeated_interruptions_and_initial_expiry_are_bounded() {
        let began = Instant::now();
        let error = write_until(
            &[1],
            Duration::from_millis(30),
            |_| {
                thread::sleep(Duration::from_millis(10));
                Err(Error::from(ErrorKind::Interrupted))
            },
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        assert!(began.elapsed() < Duration::from_secs(1));
        assert_eq!(
            write_until(
                &[1],
                Duration::ZERO,
                |_| panic!("expired writer"),
                |_| Ok(())
            )
            .unwrap_err()
            .kind(),
            ErrorKind::TimedOut
        );
    }
    #[test]
    fn short_progress_preserves_exact_binary_content_and_zero_write_fails() {
        let mut received = Vec::new();
        write_until(
            &[0, 1, 10, 255, 0],
            Duration::from_secs(1),
            |part| {
                received.push(part[0]);
                Ok(1)
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(received, [0, 1, 10, 255, 0]);
        assert_eq!(
            write_until(&[1], Duration::from_secs(1), |_| Ok(0), |_| Ok(()))
                .unwrap_err()
                .kind(),
            ErrorKind::WriteZero
        );
    }
}
