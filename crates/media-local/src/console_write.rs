//! Bounded Unix-console writes. A failure can leave a partial record on the
//! wire: the caller must close the connection, never append another record.
use crate::console_wire::{Record, MAX_BODY};
use anyhow::{bail, Context, Result};
use std::{
    io,
    os::{fd::AsRawFd, unix::net::UnixStream},
    time::{Duration, Instant},
};

/// Send a complete header and body within one monotonic deadline.
///
/// The caller must set the socket NONBLOCKING and use a poll-aware reader on
/// its clones. This function verifies that contract without changing flags.
/// Darwin's MSG_DONTWAIT alone does not prevent a stream send from waiting for
/// buffer space; an actual slow-peer regression reproduced that failure.
/// MSG_NOSIGNAL prevents a broken peer from terminating the process. No
/// process-global signal disposition is changed. Exactly one writer may use
/// this connection; concurrent record writers would interleave the protocol.
pub fn write_record_deadline(
    socket: &UnixStream,
    record: &Record,
    timeout: Duration,
) -> Result<()> {
    if record.body.is_empty()
        || record.body.len() > MAX_BODY
        || !matches!(record.kind, 1 | 2)
        || (record.kind == 2 && (record.epoch == 0 || record.body.len() <= 9 || record.body[8] > 1))
    {
        bail!("invalid console record")
    }
    let fd = socket.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error()).context("console socket flags");
    }
    if flags & libc::O_NONBLOCK == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "console writes require a nonblocking socket and poll-aware reader",
        )
        .into());
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("console write deadline overflow")?;
    let mut header = [0; 13];
    header[..4].copy_from_slice(&(record.body.len() as u32).to_be_bytes());
    header[4] = record.kind;
    header[5..].copy_from_slice(&record.epoch.to_be_bytes());
    for part in [&header[..], &record.body[..]] {
        let mut offset = 0;
        while offset < part.len() {
            if Instant::now() >= deadline {
                return Err(
                    io::Error::new(io::ErrorKind::TimedOut, "console record deadline").into(),
                );
            }
            // socket remains borrowed and open through each syscall.
            let n = unsafe {
                libc::send(
                    fd,
                    part[offset..].as_ptr().cast(),
                    part.len() - offset,
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if n > 0 {
                offset += n as usize;
                continue;
            }
            if n == 0 {
                return Err(
                    io::Error::new(io::ErrorKind::WriteZero, "console socket closed").into(),
                );
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => {}
                _ => return Err(error).context("console send"),
            }
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(
                        io::Error::new(io::ErrorKind::TimedOut, "console record deadline").into(),
                    );
                }
                // Round up only the poll interval, then check the original
                // deadline again before sending. Never reset it after progress.
                let millis = remaining
                    .as_millis()
                    .saturating_add(1)
                    .min(i32::MAX as u128);
                let mut pollfd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let rc = unsafe { libc::poll(&mut pollfd, 1, millis as i32) };
                if rc == 0 {
                    continue;
                }
                if rc < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error).context("console write poll");
                }
                if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    return Err(
                        io::Error::new(io::ErrorKind::BrokenPipe, "console peer closed").into(),
                    );
                }
                if pollfd.revents & libc::POLLOUT != 0 {
                    break;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console_wire::read_record;
    use std::{
        io::Read,
        net::Shutdown,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread,
    };

    fn small_send_buffer(stream: &UnixStream) {
        let size: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
    }
    fn flags(stream: &UnixStream) -> i32 {
        let value = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
        assert!(value >= 0);
        value
    }
    #[test]
    fn partial_writes_complete_without_changing_reader_flags() {
        let (sender, mut receiver) = UnixStream::pair().unwrap();
        small_send_buffer(&sender);
        sender.set_nonblocking(true).unwrap();
        let reader_clone = sender.try_clone().unwrap();
        let before = flags(&reader_clone);
        let record = Record::json(9, &serde_json::json!({"payload":"x".repeat(512 * 1024)}));
        let read = thread::spawn(move || read_record(&mut receiver).unwrap());
        write_record_deadline(&sender, &record, Duration::from_secs(2)).unwrap();
        let received = read.join().unwrap();
        assert_eq!(received.kind, record.kind);
        assert_eq!(received.epoch, record.epoch);
        assert_eq!(received.body, record.body);
        assert_eq!(flags(&reader_clone), before);
    }
    #[test]
    fn progress_from_a_slow_reader_does_not_reset_record_deadline() {
        let (sender, mut receiver) = UnixStream::pair().unwrap();
        small_send_buffer(&sender);
        sender.set_nonblocking(true).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_millis(30)))
            .unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let read_done = done.clone();
        let read = thread::spawn(move || {
            let mut total = 0;
            let mut bytes = [0; 1024];
            while !read_done.load(Ordering::Acquire) {
                if let Ok(n) = receiver.read(&mut bytes) {
                    if n == 0 {
                        break;
                    }
                    total += n;
                }
                thread::sleep(Duration::from_millis(10));
            }
            total
        });
        let record = Record::json(0, &serde_json::json!({"payload":"x".repeat(1024 * 1024)}));
        let started = Instant::now();
        let result = write_record_deadline(&sender, &record, Duration::from_millis(200));
        let elapsed = started.elapsed();
        done.store(true, Ordering::Release);
        let _ = sender.shutdown(Shutdown::Both);
        let read_bytes = read.join().unwrap();
        let error = result.unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(
            elapsed >= Duration::from_millis(180),
            "premature timeout: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "deadline extended: {elapsed:?}"
        );
        assert!(read_bytes > 0 && read_bytes < record.body.len());
    }
    #[test]
    fn broken_peer_returns_error_without_sigpipe() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        drop(receiver);
        sender.set_nonblocking(true).unwrap();
        let record = Record::json(0, &serde_json::json!({"result":true}));
        assert!(write_record_deadline(&sender, &record, Duration::from_millis(500)).is_err());
    }
    #[test]
    fn blocking_socket_is_rejected_without_changing_shared_flags_or_writing() {
        let (sender, mut receiver) = UnixStream::pair().unwrap();
        let clone = sender.try_clone().unwrap();
        let before = flags(&clone);
        let record = Record::json(0, &serde_json::json!({"result":true}));
        let error =
            write_record_deadline(&sender, &record, Duration::from_millis(500)).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(flags(&clone), before);
        receiver.set_nonblocking(true).unwrap();
        assert_eq!(
            receiver.read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
    #[test]
    fn expired_deadline_writes_nothing() {
        let (sender, mut receiver) = UnixStream::pair().unwrap();
        sender.set_nonblocking(true).unwrap();
        let record = Record::json(0, &serde_json::json!({"result":true}));
        assert!(write_record_deadline(&sender, &record, Duration::ZERO).is_err());
        receiver.set_nonblocking(true).unwrap();
        assert_eq!(
            receiver.read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
