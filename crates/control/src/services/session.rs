//! Interactive sessions with Linux micro-VMs, served as API 1 streams:
//! `container.exec` with `stdin`/`tty`, `container.attach`, and the web UI
//! terminal (`terminal.open`). The client keeps only its own terminal.
//!
//! The guest agent's session server (`guest/veynode-agent/session_linux.go`)
//! listens on TCP `GUEST_PORT`. Apple Virtualization guests are reached at
//! their NAT address; QEMU containers get a loopback hostfwd chosen at create
//! (and again at start if it was taken meanwhile). Every connection presents
//! the container's random token, kept in `<container>/session.token`.
//!
//! After one JSON request line and one JSON reply line, both sides send frames
//! of `u8 type, u32 big-endian length, payload`: to the guest `i` stdin, `e`
//! stdin EOF, `r` resize (u16 rows, u16 cols); from it `o` stdout, `x`
//! stderr, `q` exit status.
use super::container::{self, Container};
use super::daemon_call;
use crate::api_server::{Ctx, Input as StreamInput, StreamIo};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    io::{BufReader, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

pub const TOKEN_FILE: &str = "session.token";
pub const GUEST_PORT: u16 = 52022;
const MAX_FRAME: usize = 1 << 20;
/// Shell for `exec -it` without a command and the web terminal.
const SHELL: &str = "if command -v bash >/dev/null 2>&1; then exec bash -l; fi; exec sh -l";

pub fn default_shell() -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), SHELL.into()]
}

pub fn free_port() -> Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

/// Add the session forward to a QEMU VM configuration's hostfwd list.
pub fn add_forward(vm: &mut Value, port: u16) {
    let ports = vm["ports"].as_array_mut().expect("ports array");
    ports.retain(|p| p["guest_port"] != GUEST_PORT);
    ports.push(json!({"protocol":"tcp","host_address":"127.0.0.1","host_port":port,"guest_port":GUEST_PORT}));
}

/// Before a QEMU container starts again: if another program took its session
/// port, move the forward to a free one (the VM configuration is replaced).
pub fn refresh_forward(container: &Container) -> Result<()> {
    let Some(port) = container.session_port else {
        return Ok(());
    };
    if TcpListener::bind(("127.0.0.1", port)).is_ok() {
        return Ok(());
    }
    let status = daemon_call("status", json!({"id":container.vm_id}))?;
    let mut vm = status["config"].clone();
    if !vm.is_object() {
        bail!("daemon returned no configuration for {}", container.vm_id)
    }
    let port = free_port()?;
    add_forward(&mut vm, port);
    daemon_call(
        "delete",
        json!({"id":container.vm_id,"confirm":container.vm_id}),
    )?;
    daemon_call("create", vm)?;
    let mut record = container.clone();
    record.session_port = Some(port);
    container::save(&record)
}

pub struct Request {
    pub op: &'static str,
    pub argv: Vec<String>,
    pub tty: bool,
    pub stdin: bool,
    pub env: Vec<String>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    pub size: Option<(u16, u16)>,
}

/// Connect and authenticate, waiting up to a minute for a booting guest.
pub fn connect(container: &Container, request: &Request) -> Result<TcpStream> {
    if container.os == "darwin" {
        bail!("macOS containers use SSH sessions")
    }
    if container.init.is_some() {
        // systemd replaces container-init before the session server starts.
        bail!(
            "{} runs --init systemd and has no interactive session; use `veynodevm exec` without -i/-t",
            container.name
        )
    }
    // `start` may have moved the QEMU forward; read the current record.
    let container = container::find(&container.id)?;
    let token = std::fs::read_to_string(container::container_dir(&container.id)?.join(TOKEN_FILE))
        .map_err(|_| {
            anyhow::anyhow!(
                "{} was created before interactive sessions; recreate it",
                container.name
            )
        })?;
    let (rows, cols) = request.size.unwrap_or((24, 80));
    let mut line = serde_json::to_vec(&json!({
        "token": token.trim(),
        "op": request.op,
        "argv": request.argv,
        "tty": request.tty,
        "stdin": request.stdin,
        "rows": rows,
        "cols": cols,
        "env": request.env,
        "workdir": request.workdir,
        "user": request.user,
    }))?;
    line.push(b'\n');
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last_error = None;
    // A refused connect is cheap; retry soon while the Guest is still coming up.
    let mut delay = Duration::from_millis(10);
    while Instant::now() < deadline {
        if !container::active(&container::vm_state(&container)) {
            bail!("container {} is not running", container.name)
        }
        let address: SocketAddr = match container.session_port {
            Some(port) => ([127, 0, 0, 1], port).into(),
            None => match container::wait_for_ip(&container, Duration::from_secs(5)) {
                Ok(ip) => format!("{ip}:{GUEST_PORT}").parse()?,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            },
        };
        match handshake(address, &line) {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(refusal)) => bail!("{}: {refusal}", container.name),
            Err(e) => last_error = Some(e),
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(100));
    }
    Err(last_error
        .unwrap_or_else(|| anyhow::anyhow!("timed out"))
        .context(format!(
            "connecting to {}'s session service",
            container.name
        )))
}

/// Transport errors are retried (the guest may still be booting, and a QEMU
/// forward accepts before the guest listens); a reply error is final.
fn handshake(address: SocketAddr, line: &[u8]) -> Result<std::result::Result<TcpStream, String>> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_nodelay(true)?;
    stream.write_all(line)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reply = Vec::new();
    let mut byte = [0u8; 1];
    while byte[0] != b'\n' {
        if stream.read(&mut byte)? == 0 {
            bail!("session service closed the connection")
        }
        reply.push(byte[0]);
        if reply.len() > 4096 {
            bail!("session reply too long")
        }
    }
    stream.set_read_timeout(None)?;
    let reply: Value = serde_json::from_slice(&reply)?;
    if reply["ok"] == true {
        return Ok(Ok(stream));
    }
    Ok(Err(reply["error"].as_str().unwrap_or("refused").to_owned()))
}

#[derive(Clone)]
pub struct Writer(Arc<Mutex<TcpStream>>);

impl Writer {
    pub fn new(stream: &TcpStream) -> Result<Self> {
        Ok(Self(Arc::new(Mutex::new(stream.try_clone()?))))
    }
    pub fn frame(&self, kind: u8, payload: &[u8]) -> std::io::Result<()> {
        let mut stream = self.0.lock().unwrap();
        let mut header = [kind, 0, 0, 0, 0];
        header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        stream.write_all(&header)?;
        stream.write_all(payload)
    }
    pub fn hang_up(&self) {
        let _ = self.0.lock().unwrap().shutdown(Shutdown::Both);
    }
    pub fn resize(&self, rows: u16, cols: u16) -> std::io::Result<()> {
        let mut payload = [0u8; 4];
        payload[..2].copy_from_slice(&rows.to_be_bytes());
        payload[2..].copy_from_slice(&cols.to_be_bytes());
        self.frame(b'r', &payload)
    }
}

pub fn read_frame(from: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    from.read_exact(&mut header)?;
    let size = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    if size > MAX_FRAME {
        return Err(std::io::Error::other("session frame too large"));
    }
    let mut payload = vec![0u8; size];
    from.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

/// How a session ended.
pub enum Ended {
    Exit(i32),
    /// The guest closed the connection without a status (attach: the
    /// container stopped).
    Closed,
    /// The client left (it detached, or was interrupted).
    Left,
}

/// Relays a connected session and a stream: the guest's output to
/// `stdout`/`stderr`, the client's `stdin` (when `stdin`) and `resize`
/// control messages to the guest. The client leaving hangs it up.
pub fn serve(stream: TcpStream, io: &StreamIo, stdin: bool) -> Result<Ended> {
    let writer = Writer::new(&stream)?;
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        let guest = scope.spawn(|| {
            let mut reader = BufReader::new(stream);
            let ended = loop {
                match read_frame(&mut reader) {
                    Ok((b'o', data)) => {
                        if io.stdout(&data).is_err() {
                            break Ended::Left;
                        }
                    }
                    Ok((b'x', data)) => {
                        if io.stderr(&data).is_err() {
                            break Ended::Left;
                        }
                    }
                    Ok((b'q', data)) => {
                        match std::str::from_utf8(&data).map(|d| d.trim().parse()) {
                            Ok(Ok(code)) => break Ended::Exit(code),
                            _ => break Ended::Closed,
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break Ended::Closed,
                }
            };
            done.store(true, Ordering::SeqCst);
            ended
        });
        let mut reading = stdin;
        while !done.load(Ordering::SeqCst) {
            if io.closed() {
                writer.hang_up();
                break;
            }
            while let Some(message) = io.control(Duration::ZERO) {
                if let Some((rows, cols)) = size(&message["resize"]) {
                    let _ = writer.resize(rows, cols);
                }
            }
            if !reading {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            match io.stdin(Duration::from_millis(20)) {
                StreamInput::Data(data) => {
                    if writer.frame(b'i', &data).is_err() {
                        reading = false;
                    }
                }
                StreamInput::Eof => {
                    let _ = writer.frame(b'e', &[]);
                    reading = false;
                }
                StreamInput::Idle | StreamInput::Closed => {}
            }
        }
        let ended = guest.join().unwrap_or(Ended::Closed);
        Ok(if io.closed() { Ended::Left } else { ended })
    })
}

/// `{"rows","cols"}`, each 1..1000.
fn size(value: &Value) -> Option<(u16, u16)> {
    let (rows, cols) = (value["rows"].as_u64()?, value["cols"].as_u64()?);
    Some((rows.clamp(1, 1000) as u16, cols.clamp(1, 1000) as u16))
}

/// `terminal.open {name, rows, cols, argv?}`: one web UI terminal.
/// `stdout` carries records `u32 big-endian length, u8 kind, payload` with
/// kind 1 a JSON event (`ready`, `exit`, `error`) and kind 2 terminal output;
/// `control` takes `{"input":"..."}` and `{"resize":{"rows":R,"cols":C}}`.
/// The client leaving hangs the session up.
pub fn terminal(_: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
    let name = super::util::string(&args, "name")?;
    let (rows, cols) = size(&args).unwrap_or((24, 80));
    let argv: Vec<String> = args["argv"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let out = std::sync::Mutex::new(io.output());
    let record = |kind: u8, payload: &[u8]| -> Result<()> {
        let mut bytes = Vec::with_capacity(payload.len() + 5);
        bytes.extend_from_slice(&((payload.len() + 1) as u32).to_be_bytes());
        bytes.push(kind);
        bytes.extend_from_slice(payload);
        out.lock().unwrap().write_all(&bytes)?;
        Ok(())
    };
    let result = (|| -> Result<i32> {
        let session = open(name, rows, cols, argv)?;
        record(1, br#"{"event":"ready"}"#)?;
        let input = session.input()?;
        let done = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(Ordering::SeqCst) {
                    if io.closed() {
                        // The page went away: hang up the session.
                        input.hang_up();
                        return;
                    }
                    let Some(message) = io.control(Duration::from_millis(100)) else {
                        continue;
                    };
                    let sent = if let Some(text) = message["input"].as_str() {
                        input.write(text.as_bytes())
                    } else if let Some((rows, cols)) = size(&message["resize"]) {
                        input.resize(rows, cols)
                    } else {
                        Ok(())
                    };
                    if sent.is_err() {
                        return;
                    }
                }
            });
            let mut output = |data: &[u8]| record(2, data);
            let code = session.pump(&mut output);
            done.store(true, Ordering::SeqCst);
            code
        })
    })();
    let code = match result {
        Ok(code) => {
            record(
                1,
                json!({"event":"exit","code":code}).to_string().as_bytes(),
            )?;
            code
        }
        Err(error) => {
            record(
                1,
                json!({"event":"error","message":format!("{error:#}")})
                    .to_string()
                    .as_bytes(),
            )?;
            1
        }
    };
    Ok(json!({ "exitCode": code }))
}

/// A terminal's session: a Linux container's guest session, a macOS
/// container's or a machine's SSH login.
fn open(name: &str, rows: u16, cols: u16, argv: Vec<String>) -> Result<Box<dyn Session>> {
    Ok(match container::find(name) {
        Ok(c) if c.os == "darwin" => {
            let mut ssh = super::macos::shell_command(&c, &argv)?;
            // ssh sends its own TERM with the pty request.
            ssh.env("TERM", "xterm-256color");
            Box::new(LocalPty::spawn(ssh, rows, cols)?)
        }
        Ok(c) => {
            let stream = connect(
                &c,
                &Request {
                    op: "exec",
                    argv: if argv.is_empty() {
                        default_shell()
                    } else {
                        argv
                    },
                    tty: true,
                    stdin: true,
                    env: vec!["TERM=xterm-256color".into()],
                    workdir: None,
                    user: None,
                    size: Some((rows, cols)),
                },
            )?;
            Box::new(Remote(stream))
        }
        // A VM (machine) has no guest agent: its terminal is an SSH login.
        Err(e) => {
            let Some(mut ssh) = machine_terminal(name, &argv)? else {
                return Err(e);
            };
            ssh.env("TERM", "xterm-256color");
            Box::new(LocalPty::spawn(ssh, rows, cols)?)
        }
    })
}

trait Input: Send + Sync {
    fn write(&self, data: &[u8]) -> std::io::Result<()>;
    fn resize(&self, rows: u16, cols: u16) -> std::io::Result<()>;
    fn hang_up(&self);
}

trait Session {
    fn input(&self) -> Result<Box<dyn Input>>;
    /// Copy output until the session ends; returns its exit status.
    fn pump(self: Box<Self>, output: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<i32>;
}

struct Remote(TcpStream);

impl Input for Writer {
    fn write(&self, data: &[u8]) -> std::io::Result<()> {
        self.frame(b'i', data)
    }
    fn resize(&self, rows: u16, cols: u16) -> std::io::Result<()> {
        Writer::resize(self, rows, cols)
    }
    fn hang_up(&self) {
        Writer::hang_up(self)
    }
}

impl Session for Remote {
    fn input(&self) -> Result<Box<dyn Input>> {
        Ok(Box::new(Writer::new(&self.0)?))
    }
    fn pump(self: Box<Self>, output: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<i32> {
        let mut reader = BufReader::new(self.0);
        loop {
            match read_frame(&mut reader) {
                Ok((b'o' | b'x', data)) => output(&data)?,
                Ok((b'q', data)) => return Ok(std::str::from_utf8(&data)?.trim().parse()?),
                Ok(_) => {}
                Err(_) => bail!("the session closed without an exit status"),
            }
        }
    }
}

/// A command on a host pty (`ssh -tt` for macOS containers).
struct LocalPty {
    master: OwnedFd,
    child: std::process::Child,
}

impl LocalPty {
    fn spawn(mut command: std::process::Command, rows: u16, cols: u16) -> Result<Self> {
        use std::os::unix::process::CommandExt;
        let (mut master, mut slave) = (0, 0);
        let mut size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        } != 0
        {
            bail!("openpty: {}", std::io::Error::last_os_error())
        }
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        command
            .stdin(std::process::Stdio::from(slave.try_clone()?))
            .stdout(std::process::Stdio::from(slave.try_clone()?))
            .stderr(std::process::Stdio::from(slave));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().context("starting ssh")?;
        Ok(Self { master, child })
    }
}

struct PtyInput(OwnedFd, u32);

impl Input for PtyInput {
    fn write(&self, data: &[u8]) -> std::io::Result<()> {
        let mut file = std::fs::File::from(self.0.try_clone()?);
        file.write_all(data)
    }
    fn resize(&self, rows: u16, cols: u16) -> std::io::Result<()> {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(self.0.as_raw_fd(), libc::TIOCSWINSZ, &size) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    fn hang_up(&self) {
        unsafe { libc::kill(self.1 as i32, libc::SIGHUP) };
    }
}

impl Session for LocalPty {
    fn input(&self) -> Result<Box<dyn Input>> {
        Ok(Box::new(PtyInput(
            self.master.try_clone()?,
            self.child.id(),
        )))
    }
    fn pump(mut self: Box<Self>, output: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<i32> {
        let mut file = std::fs::File::from(self.master.try_clone()?);
        let mut buffer = [0u8; 32 << 10];
        // macOS reports EOF (or EIO) on the master once ssh and its
        // descendants have closed the slave.
        loop {
            match file.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => output(&buffer[..n])?,
            }
        }
        let status = self.child.wait()?;
        Ok(status.code().unwrap_or(255))
    }
}

/// A machine's terminal: an SSH login, or `None` when `name` is no machine.
fn machine_terminal(name: &str, argv: &[String]) -> Result<Option<std::process::Command>> {
    let name = super::machines::resolve(name);
    if super::machines::load(&name).is_err() {
        return Ok(None);
    }
    if !argv.is_empty() {
        anyhow::bail!("a machine's terminal opens its login shell; it takes no command")
    }
    let status = super::daemon_call("status", serde_json::json!({"id":name}))?;
    if !super::machines::running(status["state"].as_str().unwrap_or("unknown")) {
        anyhow::bail!("{name} is not running")
    }
    let config: crate::Config = serde_json::from_value(status["config"].clone())?;
    let ssh = crate::windows_guest::Ssh::locate(&crate::home::state_dir()?, &config)?;
    Ok(Some(ssh.terminal()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_replaces_only_the_session_port() {
        let mut vm = json!({"ports":[
            {"protocol":"tcp","host_address":"0.0.0.0","host_port":8080,"guest_port":80},
            {"protocol":"tcp","host_address":"127.0.0.1","host_port":40000,"guest_port":GUEST_PORT},
        ]});
        add_forward(&mut vm, 41000);
        let ports = vm["ports"].as_array().unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0]["host_port"], 8080);
        assert_eq!(ports[1]["host_port"], 41000);
        assert_eq!(ports[1]["host_address"], "127.0.0.1");
    }

    #[test]
    fn frames_round_trip_and_are_bounded() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let writer = Writer::new(&client).unwrap();
        writer.frame(b'i', b"abc").unwrap();
        writer.resize(40, 132).unwrap();
        assert_eq!(read_frame(&mut server).unwrap(), (b'i', b"abc".to_vec()));
        assert_eq!(
            read_frame(&mut server).unwrap(),
            (b'r', vec![0, 40, 0, 132])
        );
        let oversized = [b'o', 0xff, 0xff, 0xff, 0xff];
        assert!(read_frame(&mut &oversized[..]).is_err());
    }
}
