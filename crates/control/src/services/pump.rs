//! A host process wired to a stream: its stdout and stderr to the client, the
//! client's stdin and signals to it, and the client leaving ends it. This is
//! how commands in SSH guests (macOS, Windows) run.
use crate::api_server::{Input, StreamIo};
use anyhow::{Context, Result};
use std::{
    io::{Read, Write},
    os::unix::process::ExitStatusExt,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

/// Runs `command` to its end; with `stdin`, the client's input feeds it,
/// otherwise it gets none. The result is its exit code, `128 + signal` when a
/// signal ended it.
pub fn run(mut command: Command, stdin: bool, io: &StreamIo) -> Result<i32> {
    let mut child = command
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot start the command")?;
    let pid = child.id() as libc::pid_t;
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
                if let Some(signal) = message["signal"].as_str().and_then(signal_number) {
                    unsafe { libc::kill(pid, signal) };
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
                None => thread::sleep(Duration::from_millis(20)),
            }
        }
    })
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
