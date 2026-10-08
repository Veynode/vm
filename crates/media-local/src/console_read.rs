//! Deadline-aware reads for a Unix stream shared with a nonblocking writer.
use std::{
    io::{self, Read},
    os::{fd::AsRawFd, unix::net::UnixStream},
    time::Instant,
};

/// The caller sets O_NONBLOCK once on the connection and resets the deadline
/// before each logical record, including all its header/body or JSONL bytes.
/// A BufReader may wrap this object to preserve already-read partial data.
pub struct PollReader {
    stream: UnixStream,
    deadline: Option<Instant>,
}
impl PollReader {
    /// No implicit unlimited read: set_deadline must precede the first read.
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            deadline: None,
        }
    }
    pub fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = Some(deadline);
    }
}
impl Read for PollReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let deadline = self.deadline.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "console read deadline not set")
        })?;
        let fd = self.stream.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if flags & libc::O_NONBLOCK == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "console reads require a nonblocking socket",
            ));
        }
        loop {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "console record read deadline",
                ));
            }
            // Always try reading before poll, including after POLLHUP. Unix
            // sockets may have valid queued bytes followed by EOF.
            match self.stream.read(buf) {
                Ok(n) => return Ok(n),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "console record read deadline",
                    ));
                }
                let millis = remaining
                    .as_millis()
                    .saturating_add(1)
                    .min(i32::MAX as u128);
                let mut pollfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let rc = unsafe { libc::poll(&mut pollfd, 1, millis as i32) };
                if rc < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                if rc == 0 {
                    continue;
                }
                if pollfd.revents & libc::POLLNVAL != 0 {
                    return Err(io::Error::from_raw_os_error(libc::EBADF));
                }
                // HUP/ERR are not synthetic EOF/errors: drain buffered bytes,
                // then let read report EOF or its actual socket error.
                if pollfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console_wire::{read_record, write_record, Record};
    use std::{
        io::{BufRead, BufReader, Write},
        net::Shutdown,
        thread,
        time::Duration,
    };

    fn reader(stream: UnixStream, timeout: Duration) -> PollReader {
        stream.set_nonblocking(true).unwrap();
        let mut reader = PollReader::new(stream);
        reader.set_deadline(Instant::now() + timeout);
        reader
    }
    #[test]
    fn fragmented_header_and_body_survive_waits() {
        let (receive, mut send) = UnixStream::pair().unwrap();
        let original = Record::json(8, &serde_json::json!({"fragmented":"body"}));
        let mut bytes = Vec::new();
        write_record(&mut bytes, &original).unwrap();
        let writer = thread::spawn(move || {
            for part in bytes.chunks(3) {
                send.write_all(part).unwrap();
                thread::sleep(Duration::from_millis(2));
            }
            send.shutdown(Shutdown::Write).unwrap();
        });
        let mut receive = BufReader::with_capacity(5, reader(receive, Duration::from_secs(2)));
        let actual = read_record(&mut receive).unwrap();
        assert_eq!(actual.kind, original.kind);
        assert_eq!(actual.epoch, original.epoch);
        assert_eq!(actual.body, original.body);
        assert_eq!(receive.read(&mut [0; 1]).unwrap(), 0);
        writer.join().unwrap();
    }
    #[test]
    fn hup_preserves_all_buffered_bytes_then_eof() {
        let (receive, mut send) = UnixStream::pair().unwrap();
        send.write_all(b"first\nsecond\n").unwrap();
        send.shutdown(Shutdown::Write).unwrap();
        drop(send);
        let mut receive = BufReader::with_capacity(4, reader(receive, Duration::from_secs(1)));
        let mut first = String::new();
        assert_eq!(receive.read_line(&mut first).unwrap(), 6);
        assert_eq!(first, "first\n");
        // Resetting a record deadline must not discard BufReader's read-ahead.
        receive
            .get_mut()
            .set_deadline(Instant::now() + Duration::from_secs(1));
        let mut second = String::new();
        assert_eq!(receive.read_line(&mut second).unwrap(), 7);
        assert_eq!(second, "second\n");
        assert_eq!(receive.read(&mut [0; 1]).unwrap(), 0);
    }
    #[test]
    fn no_data_has_an_absolute_deadline() {
        let (receive, _send) = UnixStream::pair().unwrap();
        let mut receive = reader(receive, Duration::from_millis(100));
        let started = Instant::now();
        assert_eq!(
            receive.read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() >= Duration::from_millis(80));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn partial_jsonl_does_not_extend_its_deadline() {
        let (receive, mut send) = UnixStream::pair().unwrap();
        let mut receive = BufReader::new(reader(receive, Duration::from_millis(100)));
        let writer = thread::spawn(move || {
            send.write_all(b"{\"id\":").unwrap();
            thread::sleep(Duration::from_millis(60));
            send.write_all(b"1").unwrap();
            thread::sleep(Duration::from_millis(100));
        });
        let mut line = Vec::new();
        let started = Instant::now();
        assert_eq!(
            receive.read_until(b'\n', &mut line).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(line, b"{\"id\":1");
        assert!(started.elapsed() < Duration::from_millis(150));
        writer.join().unwrap();
    }
    #[test]
    fn reading_never_changes_shared_fd_flags() {
        let (receive, mut send) = UnixStream::pair().unwrap();
        receive.set_nonblocking(true).unwrap();
        let clone = receive.try_clone().unwrap();
        let before = unsafe { libc::fcntl(clone.as_raw_fd(), libc::F_GETFL) };
        assert!(before >= 0);
        send.write_all(b"x").unwrap();
        let mut receive = reader(receive, Duration::from_secs(1));
        let mut byte = [0; 1];
        receive.read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"x");
        assert_eq!(
            unsafe { libc::fcntl(clone.as_raw_fd(), libc::F_GETFL) },
            before
        );
    }
    #[test]
    fn refuses_missing_deadline_or_blocking_socket() {
        let (receive, _send) = UnixStream::pair().unwrap();
        let mut receive = PollReader::new(receive);
        assert_eq!(
            receive.read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        receive.set_deadline(Instant::now() + Duration::from_secs(1));
        assert_eq!(
            receive.read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(receive.read(&mut []).unwrap(), 0);
    }
}
