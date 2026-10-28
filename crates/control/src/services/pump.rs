//! A host process wired to a stream: its stdout and stderr to the client, the
//! client's stdin and signals to it, and the client leaving ends it. This is
//! how commands in SSH guests (Linux, macOS, Windows) run.
use crate::api_server::{Input, StreamIo};
use anyhow::{Context, Result};
use std::{
    io::{Read, Write},
    os::unix::process::ExitStatusExt,
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::Duration,
};

/// Runs `command` to its end; with `stdin`, the client's input feeds it,
/// otherwise it gets none. The result is its exit code, `128 + signal` when a
/// signal ended it.
pub fn run(command: Command, stdin: bool, io: &StreamIo) -> Result<i32> {
    run_with(command, stdin, io, None)
}

/// [`run`], with the client's signals going to `signal` (the guest command
/// behind an SSH session, which a local signal would not reach) instead of
/// the host process. Each runs on a thread of its own, so that input keeps
/// flowing meanwhile.
pub fn run_with(
    mut command: Command,
    stdin: bool,
    io: &StreamIo,
    signal: Option<super::machine_guest::Signal>,
) -> Result<i32> {
    let mut child = command
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot start the command")?;
    let pid = child.id() as libc::pid_t;
    let exited = exit_notice(child.id());
    let readers = [
        (
            child
                .stdout
                .take()
                .map(|o| Box::new(o) as Box<dyn Read + Send>),
            true,
        ),
        (
            child
                .stderr
                .take()
                .map(|e| Box::new(e) as Box<dyn Read + Send>),
            false,
        ),
    ];
    thread::scope(|scope| -> Result<i32> {
        for (reader, out) in readers {
            let Some(mut reader) = reader else { continue };
            scope.spawn(move || {
                let mut buffer = vec![0u8; 64 * 1024];
                while let Ok(n) = reader.read(&mut buffer) {
                    if n == 0 {
                        break;
                    }
                    let sent = if out {
                        io.stdout(&buffer[..n])
                    } else {
                        io.stderr(&buffer[..n])
                    };
                    if sent.is_err() {
                        break;
                    }
                }
            });
        }
        let mut input = child.stdin.take();
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)));
            }
            if io.closed() {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            while let Some(message) = io.control(Duration::ZERO) {
                let Some(name) = message["signal"].as_str() else {
                    continue;
                };
                match (&signal, signal_number(name)) {
                    (Some(forward), Some(_)) => {
                        let (forward, name) = (forward.clone(), name.to_owned());
                        thread::spawn(move || forward(&name));
                    }
                    (None, Some(number)) => unsafe {
                        libc::kill(pid, number);
                    },
                    (_, None) => {}
                }
            }
            match input.as_mut().map(|_| io.stdin(Duration::from_millis(20))) {
                Some(Input::Data(data)) => {
                    if input.as_mut().unwrap().write_all(&data).is_err() {
                        input = None;
                    }
                }
                Some(Input::Eof | Input::Closed) => input = None,
                Some(Input::Idle) => {}
                // Without input, the exit wakes the loop at once; the timeout
                // only bounds how late a signal or the client leaving is seen.
                None => match exited.recv_timeout(Duration::from_millis(20)) {
                    Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => thread::sleep(Duration::from_millis(20)),
                },
            }
        }
    })
}

/// Fires once `pid`, a child of this process, has exited, without polling: a
/// thread blocks in `waitid(WNOWAIT)`, which leaves the child unreaped, so its
/// PID stays reserved for a later kill or wait.
pub(crate) fn exit_notice(pid: u32) -> Receiver<()> {
    let (exited, notice) = mpsc::channel();
    thread::spawn(move || loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc != 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        // Darwin can report a stopped child even with WEXITED alone.
        if rc == 0
            && ![libc::CLD_EXITED, libc::CLD_KILLED, libc::CLD_DUMPED].contains(&info.si_code)
        {
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        let _ = exited.send(());
        break;
    });
    notice
}

/// The signal a `control` message names.
pub fn signal_number(name: &str) -> Option<libc::c_int> {
    Some(match name {
        "TERM" => libc::SIGTERM,
        "INT" => libc::SIGINT,
        "HUP" => libc::SIGHUP,
        "KILL" => libc::SIGKILL,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_notice_fires_at_the_exit_and_leaves_the_child_to_reap() {
        let mut child = Command::new("/bin/sleep").arg("0.05").spawn().unwrap();
        let exited = exit_notice(child.id());
        assert!(exited.recv_timeout(Duration::from_millis(5)).is_err());
        let started = std::time::Instant::now();
        exited.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(child.try_wait().unwrap().unwrap().success());
    }
}
