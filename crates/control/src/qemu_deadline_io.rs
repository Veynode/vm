//! Absolute local input I/O eligibility. No remote-delivery or kernel-time claim.
use super::*;
use std::os::fd::{AsRawFd, FromRawFd};

fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .context("native acquisition I/O deadline expired")
}
fn writable(stream: &UnixStream, deadline: Instant) -> Result<()> {
    loop {
        let left = remaining(deadline)?;
        let mut p = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let ready = unsafe {
            libc::poll(
                &mut p,
                1,
                left.as_millis().clamp(1, i32::MAX as u128) as i32,
            )
        };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        remaining(deadline)?;
        if ready > 0 {
            return Ok(());
        }
    }
}

pub(super) fn connect(path: &Path, deadline: Instant) -> Result<UnixStream> {
    use std::os::unix::ffi::OsStrExt;
    remaining(deadline)?;
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
        bail!("invalid native console socket path")
    }
    addr.sun_family = libc::AF_UNIX as _;
    for (out, b) in addr.sun_path.iter_mut().zip(bytes) {
        *out = *b as _;
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
    let connected = unsafe { libc::connect(fd, (&addr as *const libc::sockaddr_un).cast(), len) };
    if connected < 0 {
        let error = std::io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EAGAIN)
        ) {
            return Err(error.into());
        }
        writable(&stream, deadline)?;
        if let Some(error) = stream.take_error()? {
            return Err(error.into());
        }
        // EAGAIN is not a completed connect on every Unix implementation.
        stream
            .peer_addr()
            .context("native console connection incomplete")?;
    }
    remaining(deadline)?;
    Ok(stream)
}

/// Exact bytes accepted by successful nonblocking write calls, including a full
/// write whose completion was observed late. Never equates this with Guest delivery.
pub(super) struct WriteResult {
    pub written: usize,
    pub result: Result<()>,
}
pub(super) fn write(stream: &mut UnixStream, bytes: &[u8], deadline: Instant) -> WriteResult {
    write_using_clock(stream, bytes, deadline, Instant::now)
}
fn write_using_clock(
    stream: &mut UnixStream,
    bytes: &[u8],
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
) -> WriteResult {
    let mut written = 0;
    let result = (|| {
        stream.set_nonblocking(true)?;
        while written < bytes.len() {
            if now() >= deadline {
                bail!("native acquisition I/O deadline expired")
            }
            match stream.write(&bytes[written..]) {
                Ok(0) => bail!("native input write returned zero"),
                Ok(count) => {
                    written += count;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    writable(stream, deadline)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        // A thread can be preempted during/after poll; retain the actual byte count.
        if now() >= deadline {
            bail!("native acquisition I/O deadline expired")
        }
        Ok(())
    })();
    WriteResult { written, result }
}

pub(super) fn console(
    path: &Path,
    request: &Value,
    request_id: &str,
    deadline: Instant,
) -> Result<Value> {
    let mut bytes = serde_json::to_vec(request)?;
    if bytes.len() >= vm_control::native_input::MAX_LINE {
        bail!("native console request bound");
    }
    bytes.push(b'\n');
    let mut socket = connect(path, deadline)?;
    write(&mut socket, &bytes, deadline).result?;
    let reply = read_console_reply(&mut socket, request_id, deadline)?;
    remaining(deadline)?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expired_connect_and_write_admit_no_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(connect(&path, Instant::now()).is_err());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let (mut stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let result = write(&mut stream, b"never\n", Instant::now());
        assert!(result.result.is_err());
        assert_eq!(result.written, 0);
        assert_eq!(
            peer.read(&mut [0; 1]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    #[test]
    fn full_socket_write_observed_after_deadline_retains_accepted_byte_count() {
        let (mut stream, mut peer) = UnixStream::pair().unwrap();
        let started = Instant::now();
        let mut calls = 0;
        let result = write_using_clock(
            &mut stream,
            b"{}\n",
            started + Duration::from_millis(20),
            || {
                calls += 1;
                if calls == 1 {
                    started
                } else {
                    started + Duration::from_millis(21)
                }
            },
        );
        assert!(result.result.is_err());
        assert_eq!(
            result.written, 3,
            "never claim already accepted bytes were blocked"
        );
        let mut received = [0; 3];
        peer.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"{}\n");
    }
    #[test]
    fn actual_partial_qmp_write_poison_forbids_release_append() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let small: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&small as *const libc::c_int).cast(),
                    std::mem::size_of_val(&small) as _,
                )
            },
            0
        );
        let mut qmp = Qmp {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
        };
        let mut input = InputState {
            held: std::collections::BTreeMap::from([(
                "held".into(),
                vm_control::input::key("a", false),
            )]),
            active_epoch: Some(3),
            operation: Some("a".repeat(32)),
            ..Default::default()
        };
        // Build before starting this fixture's deadline so it deterministically
        // exercises a partial socket write, not legitimate pre-write expiry.
        let mut bytes = serde_json::to_vec(&json!({"execute":"input-send-event","id":1,"arguments":{"padding":"a".repeat(500_000)}})).unwrap();
        bytes.push(b'\n');
        assert!(qmp
            .write_record_before(&bytes, Instant::now() + Duration::from_millis(20))
            .is_err());
        assert!(qmp.poisoned);
        assert!(input.cleanup(&mut qmp).is_err());
        assert_eq!(input.held.len(), 1);
        assert!(input.operation.is_some());
        assert!(qmp.arguments("query-status", json!({})).is_err());
        peer.set_nonblocking(true).unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            match peer.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        assert!(!bytes.is_empty() && bytes.len() < 500_000);
        assert!(
            !bytes.contains(&b'\n'),
            "no release or query appended to incomplete JSON"
        );
    }
    #[test]
    fn full_record_late_ack_keeps_stream_framing_and_real_release() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let task = thread::spawn(move || {
            let mut reader = BufReader::new(peer.try_clone().unwrap());
            for down in [true, false] {
                let v: Value =
                    serde_json::from_slice(&read_frame(&mut reader).unwrap().unwrap()).unwrap();
                assert_eq!(v["arguments"]["events"][0]["data"]["down"], down);
                if down {
                    thread::sleep(Duration::from_millis(50));
                }
                serde_json::to_writer(&mut peer, &json!({"id":v["id"],"return":{}})).unwrap();
                peer.write_all(b"\n").unwrap();
            }
        });
        let mut qmp = Qmp {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
        };
        let mut input = InputState::default();
        let deadline = Instant::now() + Duration::from_millis(30);
        input
            .send_events_before(
                &[vm_control::input::key("a", true)],
                &mut qmp,
                Some(deadline),
            )
            .unwrap();
        assert!(Instant::now() > deadline);
        assert!(!qmp.poisoned);
        assert_eq!(input.held.len(), 1);
        input.cleanup(&mut qmp).unwrap();
        assert!(input.held.is_empty());
        task.join().unwrap();
    }
}

#[cfg(test)]
mod ack_deadline_tests {
    use super::*;
    #[test]
    fn actual_qmp_dribble_cannot_extend_ack_absolute_deadline() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let task = thread::spawn(move || {
            let mut reader = BufReader::new(peer.try_clone().unwrap());
            let request: Value =
                serde_json::from_slice(&read_frame(&mut reader).unwrap().unwrap()).unwrap();
            let mut reply = serde_json::to_vec(&json!({"id":request["id"],"return":{}})).unwrap();
            reply.push(b'\n');
            for b in reply {
                if peer.write_all(&[b]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(180));
            }
        });
        let mut qmp = Qmp {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
        };
        let started = Instant::now();
        let result = qmp.arguments("query-status", json!({}));
        let elapsed = started.elapsed();
        drop(qmp);
        task.join().unwrap();
        assert!(
            result.is_err(),
            "dribbled full response must not gain a fresh timeout per byte"
        );
        assert!(elapsed < Duration::from_millis(3400), "elapsed={elapsed:?}");
    }
}

#[cfg(test)]
mod ack_framing_tests {
    use super::*;
    #[test]
    fn actual_partial_ack_timeout_retains_prefix_for_id_matched_release() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let task = thread::spawn(move || {
            let mut reader = BufReader::new(peer.try_clone().unwrap());
            let first: Value =
                serde_json::from_slice(&read_frame(&mut reader).unwrap().unwrap()).unwrap();
            assert_eq!(first["id"], 1);
            assert_eq!(first["arguments"]["events"][0]["data"]["down"], true);
            peer.write_all(b"{\"id\":1,").unwrap();
            // The worker times out at its original three-second ACK deadline.
            thread::sleep(Duration::from_millis(3150));
            peer.write_all(b"\"return\":{}}\n").unwrap();
            let release: Value =
                serde_json::from_slice(&read_frame(&mut reader).unwrap().unwrap()).unwrap();
            assert_eq!(release["id"], 2);
            assert_eq!(release["arguments"]["events"][0]["data"]["down"], false);
            peer.write_all(b"{\"id\":2,\"return\":{}}\n").unwrap();
        });
        let mut qmp = Qmp {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
        };
        let mut input = InputState::default();
        let started = Instant::now();
        assert!(input
            .send_events_before(
                &[vm_control::input::key("a", true)],
                &mut qmp,
                Some(started + Duration::from_millis(200))
            )
            .is_err());
        assert!(started.elapsed() < Duration::from_millis(3100));
        assert!(!qmp.poisoned);
        assert_eq!(qmp.reply_prefix, b"{\"id\":1,");
        assert_eq!(input.held.len(), 1);
        input.cleanup(&mut qmp).unwrap();
        assert!(input.held.is_empty());
        assert!(qmp.reply_prefix.is_empty());
        task.join().unwrap();
    }
    #[test]
    fn partial_ack_eof_and_oversize_poison_without_clearing_held_state() {
        for oversized in [false, true] {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            let task = thread::spawn(move || {
                let mut reader = BufReader::new(peer.try_clone().unwrap());
                read_frame(&mut reader).unwrap().unwrap();
                if oversized {
                    let _ = peer.write_all(&vec![b'x'; vm_control::MAX_MESSAGE + 1]);
                } else {
                    peer.write_all(b"{\"id\":1,").unwrap();
                }
            });
            let mut qmp = Qmp {
                reader: BufReader::new(stream.try_clone().unwrap()),
                stream,
                next: 1,
                poisoned: false,
                reply_prefix: Vec::new(),
            };
            let mut input = InputState::default();
            let error = input
                .send_events_before(
                    &[vm_control::input::key("a", true)],
                    &mut qmp,
                    Some(Instant::now() + Duration::from_millis(200)),
                )
                .unwrap_err();
            assert!(
                qmp.poisoned,
                "oversized={oversized} prefix={} error={error:#}",
                qmp.reply_prefix.len()
            );
            assert!(input.cleanup(&mut qmp).is_err());
            assert_eq!(input.held.len(), 1);
            assert!(qmp.reply_prefix.len() <= vm_control::MAX_MESSAGE);
            drop(qmp);
            task.join().unwrap();
        }
    }
}

#[cfg(test)]
mod release_write_tests {
    use super::*;
    #[test]
    fn actual_partial_release_poison_prevents_retry_appending_json() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let small: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&small as *const libc::c_int).cast(),
                    std::mem::size_of_val(&small) as _,
                )
            },
            0
        );
        // Legacy before-code has a per-write socket timeout. Candidate replaces
        // it with the ordinary production three-second absolute write budget.
        stream
            .set_write_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let mut qmp = Qmp {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
        };
        let mut input = InputState {
            operation: Some("a".repeat(32)),
            active_epoch: Some(3),
            ..Default::default()
        };
        // Synthetic held set pressures real release serialization/socket I/O;
        // not a claim the native keyboard can hold this many physical keys.
        for i in 0..1000 {
            input
                .held
                .insert(format!("fixture-{i}"), vm_control::input::key("a", false));
        }
        assert!(input.cleanup(&mut qmp).is_err());
        assert!(
            qmp.poisoned,
            "partial cleanup write must poison before a retry"
        );
        assert_eq!(input.held.len(), 1000);
        assert!(input.operation.is_some());
        peer.set_nonblocking(true).unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            match peer.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        assert!(!bytes.is_empty());
        assert!(!bytes.contains(&b'\n'));
        assert!(input.cleanup(&mut qmp).is_err());
        assert_eq!(
            peer.read(&mut [0; 1]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(input.held.len(), 1000);
    }
}
