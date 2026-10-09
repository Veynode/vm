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
    loop {
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
        std::thread::sleep(Duration::from_millis(20));
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
}
