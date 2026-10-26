//! Host-time guardian for one exact child. No PID lookup, process-group reuse or reconnect.
use anyhow::{bail, Context, Result};
use std::{
    ffi::OsString,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub const ACTIVE_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Fallback observation step when no kernel event source is available.
const POLL_STEP: Duration = Duration::from_millis(20);
#[cfg(test)]
thread_local! {
    /// Supervision loop passes made on this thread; proves the wait is event-driven.
    static PASSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
/// Blocks the supervision loop until something it must act on can have changed: the owner
/// socket became readable or closed, the exact owned child exited, or the next timer is due.
/// Every failure degrades to the original fixed 20 ms step; nothing here decides an outcome,
/// the loop re-observes the child and the socket itself after every return.
struct Wake {
    #[cfg(target_os = "macos")]
    queue: Option<std::os::fd::OwnedFd>,
    /// Darwin can post the exit note just before the child is waitable. From then on step.
    child_noted: bool,
}
impl Wake {
    #[cfg(target_os = "macos")]
    fn new(stream: &UnixStream, child: &Child) -> Self {
        let queue = (|| {
            let raw = unsafe { libc::kqueue() };
            if raw < 0 {
                return None;
            }
            let queue = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
            if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return None;
            }
            // The child is ours and unreaped, so its PID cannot have been reused. If it has
            // already exited, registration fails and the loop keeps its 20 ms step.
            for (ident, filter, fflags) in [
                (stream.as_raw_fd() as libc::uintptr_t, libc::EVFILT_READ, 0),
                (
                    child.id() as libc::uintptr_t,
                    libc::EVFILT_PROC,
                    libc::NOTE_EXIT,
                ),
            ] {
                let mut change: libc::kevent = unsafe { std::mem::zeroed() };
                change.ident = ident;
                change.filter = filter;
                change.flags = libc::EV_ADD;
                change.fflags = fflags;
                if unsafe {
                    libc::kevent(raw, &change, 1, std::ptr::null_mut(), 0, std::ptr::null())
                } != 0
                {
                    return None;
                }
            }
            Some(queue)
        })();
        Self {
            queue,
            child_noted: false,
        }
    }
    #[cfg(not(target_os = "macos"))]
    fn new(_stream: &UnixStream, _child: &Child) -> Self {
        Self { child_noted: false }
    }
    fn wait(&mut self, until: Instant) {
        #[cfg(target_os = "macos")]
        if let (Some(queue), false) = (&self.queue, self.child_noted) {
            let left = until.saturating_duration_since(Instant::now());
            let timeout = libc::timespec {
                tv_sec: left.as_secs() as libc::time_t,
                tv_nsec: left.subsec_nanos() as libc::c_long,
            };
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            let ready = unsafe {
                libc::kevent(
                    queue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    &mut event,
                    1,
                    &timeout,
                )
            };
            if ready > 0 && event.filter == libc::EVFILT_PROC {
                self.child_noted = true;
            } else if ready < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                self.queue = None;
            }
            return;
        }
        let _ = until;
        std::thread::sleep(POLL_STEP);
    }
}
pub struct Parent {
    stream: UnixStream,
    child_end: Option<UnixStream>,
    pub backend_pid: Option<u32>,
    challenge: Vec<u8>,
}
impl Parent {
    pub fn new() -> Result<Self> {
        let (stream, child_end) = UnixStream::pair()?;
        Ok(Self {
            stream,
            child_end: Some(child_end),
            backend_pid: None,
            challenge: Vec::new(),
        })
    }
    pub fn wrap(&self, command: &mut Command) -> Result<()> {
        let fd = self
            .child_end
            .as_ref()
            .context("guardian endpoint already transferred")?
            .as_raw_fd();
        command
            .arg("--supervise-qemu")
            .arg(fd.to_string())
            .arg("--");
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }
    pub fn accept(&mut self) -> Result<u32> {
        drop(self.child_end.take());
        self.stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut b = [0; 4];
        self.stream.read_exact(&mut b)?;
        let pid = u32::from_be_bytes(b);
        if pid == 0 {
            bail!("invalid guardian child identity")
        };
        self.backend_pid = Some(pid);
        self.stream.set_nonblocking(true)?;
        Ok(pid)
    }
    pub fn heartbeat(&mut self) -> Result<()> {
        let mut bytes = [0; 8];
        loop {
            match self.stream.read(&mut bytes[..8 - self.challenge.len()]) {
                Ok(0) => bail!("QEMU guardian disconnected"),
                Ok(n) => self.challenge.extend_from_slice(&bytes[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
            if self.challenge.len() == 8 {
                self.stream
                    .write_all(&self.challenge)
                    .context("QEMU guardian response failed")?;
                self.challenge.clear();
                return Ok(());
            }
        }
    }
    pub fn close(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}
impl Drop for Parent {
    fn drop(&mut self) {
        self.close();
    }
}

pub fn run(args: &[OsString]) -> Result<()> {
    if args.len() < 3 || args[1] != "--" {
        bail!("invalid guardian arguments")
    }
    let fd: i32 = args[0]
        .to_str()
        .context("invalid guardian descriptor")?
        .parse()?;
    if fd < 3 {
        bail!("invalid guardian descriptor")
    }
    let mut info: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut info) } != 0 || info.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        bail!("guardian requires inherited socket")
    }
    let mut stream = unsafe { UnixStream::from_raw_fd(fd) };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut child = Command::new(&args[2])
        .args(&args[3..])
        .stdin(Stdio::null())
        .spawn()
        .context("guardian spawning QEMU")?;
    // From this point every exit path owns and reaps this exact Child.
    let result = supervise(&mut stream, &mut child, STARTUP_TIMEOUT, ACTIVE_TIMEOUT);
    if child.try_wait()?.is_none() {
        child.kill()?;
        child.wait()?;
    }
    result
}
fn supervise(
    stream: &mut UnixStream,
    child: &mut Child,
    startup: Duration,
    active: Duration,
) -> Result<()> {
    stream.set_nonblocking(true)?;
    stream.write_all(&child.id().to_be_bytes())?;
    let mut deadline = Instant::now() + startup;
    let mut sequence = 1u64;
    stream.write_all(&sequence.to_be_bytes())?;
    let mut waiting = true;
    let mut received = Vec::new();
    let mut next = Instant::now();
    let mut wake = Wake::new(stream, child);
    loop {
        #[cfg(test)]
        PASSES.with(|p| p.set(p.get() + 1));
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            } else {
                bail!("QEMU child exited unsuccessfully")
            }
        }
        if waiting && Instant::now() >= deadline {
            bail!("QEMU guardian challenge expired; incarnation terminated")
        }
        if !waiting && Instant::now() >= next {
            sequence = sequence
                .checked_add(1)
                .context("guardian sequence exhausted")?;
            stream.write_all(&sequence.to_be_bytes())?;
            waiting = true;
            deadline = Instant::now() + active;
        }
        let mut bytes = [0; 8];
        match stream.read(&mut bytes[..8 - received.len()]) {
            Ok(0) => bail!("QEMU guardian owner EOF; incarnation terminated"),
            Ok(n) => {
                if !waiting {
                    bail!("unsolicited guardian heartbeat")
                };
                received.extend_from_slice(&bytes[..n]);
                if received.len() == 8 {
                    if received != sequence.to_be_bytes() {
                        bail!("stale guardian challenge response")
                    };
                    received.clear();
                    waiting = false;
                    next = Instant::now() + Duration::from_millis(500).min(active / 2);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        // While a challenge is outstanding the only timer is its expiry; otherwise it is the
        // next challenge. Both are re-evaluated at the top of the loop, so neither can spin.
        wake.wait(if waiting { deadline } else { next });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_owned_child_is_reaped_on_eof_and_expiry() {
        for eof in [true, false] {
            let (mut a, mut b) = UnixStream::pair().unwrap();
            let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
            let pid = child.id();
            let peer = std::thread::spawn(move || {
                let mut bytes = [0; 4];
                a.read_exact(&mut bytes).unwrap();
                assert_eq!(u32::from_be_bytes(bytes), pid);
                let mut challenge = [0; 8];
                a.read_exact(&mut challenge).unwrap();
                a.write_all(&challenge).unwrap();
                if !eof {
                    std::thread::sleep(Duration::from_millis(250));
                }
            });
            assert!(supervise(
                &mut b,
                &mut child,
                Duration::from_secs(1),
                Duration::from_millis(80)
            )
            .is_err());
            child.kill().unwrap();
            let status = child.wait().unwrap();
            assert!(!status.success());
            assert!(child.try_wait().unwrap().is_some());
            peer.join().unwrap();
        }
    }
    /// Idle supervision must sleep in the kernel between challenges instead of stepping
    /// every 20 ms, and must still notice the owned child exiting and owner EOF at once.
    #[cfg(target_os = "macos")]
    #[test]
    fn supervision_blocks_on_events_between_challenges() {
        // Answered challenges for ~1.3 s, then owner EOF. The fixed step made ~65 passes.
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let peer = std::thread::spawn(move || {
            let mut bytes = [0; 4];
            a.read_exact(&mut bytes).unwrap();
            let until = Instant::now() + Duration::from_millis(1300);
            a.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
            let mut answered = 0;
            while Instant::now() < until {
                let mut challenge = [0; 8];
                if a.read_exact(&mut challenge).is_ok() {
                    a.write_all(&challenge).unwrap();
                    answered += 1;
                }
            }
            answered
        });
        PASSES.with(|p| p.set(0));
        let started = Instant::now();
        let error = supervise(
            &mut b,
            &mut child,
            Duration::from_secs(10),
            Duration::from_secs(10),
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        let passes = PASSES.with(|p| p.get());
        assert!(error.to_string().contains("owner EOF"), "{error}");
        // EOF is an event: no waiting out the 500 ms challenge interval or the 10 s expiry.
        assert!(elapsed < Duration::from_millis(1700), "elapsed={elapsed:?}");
        let answered = peer.join().unwrap();
        assert!((2..=4).contains(&answered), "answered={answered}");
        assert!(passes <= 20, "passes={passes}");
        child.kill().unwrap();
        child.wait().unwrap();

        // An unanswered challenge with a 10 s expiry: only the child exiting can end this.
        let (_a, mut b) = UnixStream::pair().unwrap();
        let mut child = Command::new("/bin/sleep").arg("0.3").spawn().unwrap();
        PASSES.with(|p| p.set(0));
        let started = Instant::now();
        supervise(
            &mut b,
            &mut child,
            Duration::from_secs(10),
            Duration::from_secs(10),
        )
        .unwrap();
        let elapsed = started.elapsed();
        let passes = PASSES.with(|p| p.get());
        assert!(elapsed < Duration::from_secs(3), "elapsed={elapsed:?}");
        // One pass per wake; a few 20 ms steps are allowed between the exit note and waitability.
        assert!(passes <= 8, "passes={passes}");
    }
    /// A child that is already gone when supervision starts cannot be registered for an exit
    /// note; the fixed step must still report it.
    #[test]
    fn already_exited_child_is_still_observed() {
        let (_a, mut b) = UnixStream::pair().unwrap();
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        supervise(
            &mut b,
            &mut child,
            Duration::from_secs(10),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
