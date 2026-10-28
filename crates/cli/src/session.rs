//! This terminal on an interactive API 1 stream (`container.exec` with
//! `stdin`/`tty`, `container.attach`): raw mode with `tty`, window size
//! changes as `resize` control messages, stdin as `stdin` frames, and the
//! stream's output on stdout and stderr. The daemon holds the guest session.
use crate::api;
use anyhow::Result;
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
use veynodevm_api::Event;

/// The terminal's size as (rows, cols), when stdout is a terminal.
pub fn terminal_size() -> Option<(u16, u16)> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    (ok && size.ws_row > 0 && size.ws_col > 0).then_some((size.ws_row, size.ws_col))
}

fn stdin_is_terminal() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

/// Puts stdin in raw mode until dropped.
struct RawMode(libc::termios);

impl RawMode {
    fn enable() -> Option<Self> {
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return None;
        }
        let mut raw = original;
        unsafe {
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
        }
        Some(Self(original))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}

static RESIZED: AtomicBool = AtomicBool::new(false);
extern "C" fn on_winch(_: libc::c_int) {
    RESIZED.store(true, Ordering::SeqCst);
}

pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);
extern "C" fn on_signal(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Ctrl-C (and SIGTERM) end a foreground stream instead of this process, so
/// the caller can stop what it ran.
pub fn install_interrupt_handler() {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

/// How a session ended for this terminal.
pub enum Ended {
    /// The stream's answer.
    Done(Value),
    /// Ctrl-P Ctrl-Q while attached.
    Detached,
    /// Ctrl-C (see [`install_interrupt_handler`]).
    Interrupted,
}

/// Runs `method` with this terminal: `stdin` feeds it, `tty` switches stdin
/// to raw mode, `detachable` makes Ctrl-P Ctrl-Q leave it running.
pub fn run(method: &str, args: Value, tty: bool, stdin: bool, detachable: bool) -> Result<Ended> {
    run_on(&api()?, method, args, tty, stdin, detachable)
}

/// [`run`] against `client`'s daemon.
pub fn run_on(
    client: &veynodevm_api::Client,
    method: &str,
    mut args: Value,
    tty: bool,
    stdin: bool,
    detachable: bool,
) -> Result<Ended> {
    if tty {
        if let Some((rows, cols)) = terminal_size() {
            args["rows"] = json!(rows);
            args["cols"] = json!(cols);
        }
    }
    let mut stream = client.stream(method, args)?;
    let sender = stream.sender();
    let raw = if tty && stdin_is_terminal() {
        RawMode::enable()
    } else {
        None
    };
    let finished = Arc::new(AtomicBool::new(false));
    let detached = Arc::new(AtomicBool::new(false));
    if tty {
        unsafe { libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t) };
    }
    {
        // Window size changes, and Ctrl-C ending the stream.
        let (sender, finished) = (sender.clone(), finished.clone());
        thread::spawn(move || {
            while !finished.load(Ordering::SeqCst) {
                if tty && RESIZED.swap(false, Ordering::SeqCst) {
                    if let Some((rows, cols)) = terminal_size() {
                        let _ = sender.control(&json!({"resize":{"rows":rows,"cols":cols}}));
                    }
                }
                if INTERRUPTED.load(Ordering::SeqCst) {
                    sender.cancel();
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
    }
    if stdin {
        let (sender, detached) = (sender.clone(), detached.clone());
        thread::spawn(move || {
            let mut input = std::io::stdin().lock();
            let mut buffer = [0u8; 8192];
            let mut ctrl_p = false;
            loop {
                let n = match input.read(&mut buffer) {
                    Ok(0) | Err(_) => {
                        let _ = sender.close_stdin();
                        return;
                    }
                    Ok(n) => n,
                };
                if detachable {
                    for &byte in &buffer[..n] {
                        if ctrl_p && byte == 0x11 {
                            detached.store(true, Ordering::SeqCst);
                            sender.cancel();
                            return;
                        }
                        ctrl_p = byte == 0x10;
                    }
                }
                if sender.stdin(&buffer[..n]).is_err() {
                    return;
                }
            }
        });
    }
    let (mut stdout, mut stderr) = (std::io::stdout(), std::io::stderr());
    let ended = loop {
        match stream.next_event() {
            Ok(Event::Stdout(data)) => {
                stdout.write_all(&data)?;
                stdout.flush()?;
            }
            Ok(Event::Stderr(data)) => {
                stderr.write_all(&data)?;
                stderr.flush()?;
            }
            Ok(Event::Event(_)) => {}
            Ok(Event::End(result)) => break Ok(Ended::Done(result?)),
            Err(_) if detached.load(Ordering::SeqCst) => break Ok(Ended::Detached),
            Err(_) if INTERRUPTED.load(Ordering::SeqCst) => break Ok(Ended::Interrupted),
            Err(e) => break Err(e),
        }
    };
    finished.store(true, Ordering::SeqCst);
    drop(raw);
    ended
}

/// `veynodevm __terminal NAME ROWS COLS [COMMAND...]`: one web UI terminal
/// over `terminal.open`. stdin: JSON lines `{"input":"..."}` or
/// `{"resize":{"rows":R,"cols":C}}`; stdout: the stream's records.
pub fn relay(name: &str, rows: u16, cols: u16, argv: Vec<String>) -> Result<i32> {
    pipe(
        "terminal.open",
        json!({"name":name,"rows":rows,"cols":cols,"argv":argv}),
    )
}

/// A stream whose `stdout` is records for a page and whose `control` is the
/// page's JSON lines on stdin; the end of stdin ends it.
pub fn pipe(method: &str, args: Value) -> Result<i32> {
    use std::io::BufRead;
    let mut stream = api()?.stream(method, args)?;
    let sender = stream.sender();
    thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(|l| l.ok()) {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if sender.control(&message).is_err() {
                return;
            }
        }
        // The page went away: end the stream.
        sender.cancel();
    });
    let mut stdout = std::io::stdout();
    loop {
        match stream.next_event() {
            Ok(Event::Stdout(data)) => {
                stdout.write_all(&data)?;
                stdout.flush()?;
            }
            Ok(Event::End(result)) => return Ok(result?["exitCode"].as_i64().unwrap_or(1) as i32),
            Ok(_) => {}
            // Cancelled after stdin ended.
            Err(_) => return Ok(0),
        }
    }
}
