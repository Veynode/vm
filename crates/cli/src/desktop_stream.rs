//! `veynodevm desktop --stream`: low-latency desktop relay for the web UI.
//!
//! The web UI spawns one relay per browser session and speaks to it over
//! pipes. Video comes from a hardware encoder child that already speaks the
//! sender framing (`u32 size, u64 pts_us, u8 keyframe, Annex B`):
//!
//! - QEMU/HVF: `vm-console-stream` against the worker's persistent SPICE
//!   console broker (VideoToolbox on the console surface).
//! - Apple Virtualization Linux: `vz-desktop-stream` against the worker's
//!   vsock 5001 bridge to the guest agent's DRM scanout capture.
//!
//! stdout records: `u32 big-endian length, u8 kind, payload`; kind 1 is a JSON
//! event, kind 2 is `u64 pts_us, u8 keyframe, Annex B`.
//! stdin: JSON lines, `{"events":[...]}` (see `vm_control::desktop_input`) or
//! `{"keyframe":true}`. EOF stops the stream and releases held keys/buttons.
use crate::{container, daemon_call, random_hex, sibling};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use vm_control::{desktop_input, home};

const MAX_AU: usize = 4 * 1024 * 1024;

type Out = Arc<Mutex<std::io::Stdout>>;

fn record(out: &Out, kind: u8, payload: &[u8]) -> Result<()> {
    let mut stdout = out.lock().unwrap();
    stdout.write_all(&((payload.len() + 1) as u32).to_be_bytes())?;
    stdout.write_all(&[kind])?;
    stdout.write_all(payload)?;
    stdout.flush()?;
    Ok(())
}

fn event(out: &Out, value: Value) -> Result<()> {
    record(out, 1, &serde_json::to_vec(&value)?)
}

enum Backend {
    Qemu,
    Apple { bridge_dir: PathBuf },
}

struct Session {
    vm: String,
    backend: Backend,
    child: Child,
}

impl Session {
    fn start(name: &str, codec: &str) -> Result<Self> {
        let vm = match container::find(name) {
            Ok(c) => c.vm_id,
            Err(_) => name.to_owned(),
        };
        let status = daemon_call("status", json!({"id":vm}))?;
        if !matches!(status["state"].as_str(), Some("running")) {
            bail!("{name} is not running")
        }
        let backend = status["config"]["backend"].as_str().unwrap_or_default();
        let guest = status["config"]["guest"].as_str().unwrap_or_default();
        let mut command;
        let session_backend = match (backend, guest) {
            ("qemu-hvf", _) => {
                let console = status["runtime"]["console_socket"]
                    .as_str()
                    .context("this QEMU VM has no persistent console; register the console broker and restart it")?;
                command = Command::new(sibling("vm-console-stream")?);
                command.args([console, "--codec", codec]);
                Backend::Qemu
            }
            ("apple-virtualization", "linux") => {
                let encoder = home::Runtime::load()?
                    .vz_desktop_stream
                    .context("vz_desktop_stream is not registered in runtime.json (scripts/install-dev-runtime.py)")?;
                // sun_path is short; keep the private bridge directory in /tmp.
                let bridge_dir =
                    PathBuf::from(format!("/private/tmp/vvm-desktop-{}", random_hex(8)));
                std::fs::DirBuilder::new().mode(0o700).create(&bridge_dir)?;
                let socket = bridge_dir.join("s");
                // The daemon holds one bridge attempt per VM. "already
                // attempted" means another viewer is live; the worker's
                // "busy" means the previous viewer (reload, codec switch) is
                // still closing: stop waits for that, then retry once.
                let start = || {
                    daemon_call(
                        "start_guest_stream_bridge",
                        json!({"id":vm,"socket_path":socket}),
                    )
                };
                let started = match start() {
                    Err(error) if format!("{error:#}").contains("\"busy\"") => {
                        daemon_call("stop_guest_stream_bridge", json!({"id":vm}))?;
                        start()
                    }
                    Err(error) if format!("{error:#}").contains("already attempted") => {
                        let _ = std::fs::remove_dir(&bridge_dir);
                        bail!("another browser is showing this desktop; Apple Virtualization desktops stream to one viewer at a time")
                    }
                    other => other,
                };
                if let Err(error) = started {
                    // A failed attempt still occupies the daemon's bridge slot.
                    close_bridge(&vm, &Backend::Apple { bridge_dir });
                    return Err(error);
                }
                command = Command::new(encoder);
                command.arg(&socket).args(["--codec", codec]);
                Backend::Apple { bridge_dir }
            }
            _ => bail!(
                "browser desktop supports Linux containers on QEMU/HVF or Apple Virtualization"
            ),
        };
        let spawned = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting desktop encoder");
        match spawned {
            Ok(child) => Ok(Self {
                vm,
                backend: session_backend,
                child,
            }),
            Err(error) => {
                close_bridge(&vm, &session_backend);
                Err(error)
            }
        }
    }

    fn stop(mut self, input: &Mutex<ChildStdin>) {
        let _ = input.lock().unwrap().write_all(b"stop\n");
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        close_bridge(&self.vm, &self.backend);
    }
}

fn close_bridge(vm: &str, backend: &Backend) {
    if let Backend::Apple { bridge_dir } = backend {
        let _ = daemon_call("stop_guest_stream_bridge", json!({"id":vm}));
        let _ = std::fs::remove_file(bridge_dir.join("s"));
        let _ = std::fs::remove_dir(bridge_dir);
    }
}

/// Forward the encoder's access units as kind-2 records.
fn pump_video(mut from: impl Read, out: Out) -> Result<()> {
    let mut header = [0u8; 13];
    loop {
        if let Err(error) = from.read_exact(&mut header) {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                return Ok(());
            }
            return Err(error.into());
        }
        let size = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        if size == 0 || size > MAX_AU {
            bail!("encoder access unit size {size} outside bound")
        }
        let mut payload = vec![0u8; 9 + size];
        payload[..9].copy_from_slice(&header[4..]);
        from.read_exact(&mut payload[9..])?;
        record(&out, 2, &payload)?;
    }
}

/// Encoder diagnostics are JSON lines; pass them through as events.
fn pump_diagnostics(from: impl Read, out: Out) {
    for line in BufReader::new(from).lines().map_while(Result::ok) {
        let value = serde_json::from_str::<Value>(&line)
            .unwrap_or_else(|_| json!({"text":line.chars().take(400).collect::<String>()}));
        if event(&out, json!({"event":"encoder","detail":value})).is_err() {
            return;
        }
    }
}

fn inject(
    session_vm: &str,
    qemu: bool,
    input: &Mutex<ChildStdin>,
    events: &[desktop_input::Event],
) -> Result<()> {
    let raw = serde_json::to_value(
        events
            .iter()
            .map(|e| serde_json::to_value(EventWire(e)))
            .collect::<Result<Vec<_>, _>>()?,
    )?;
    // QEMU injects in the worker; for Apple Virtualization the daemon only
    // checks the native window and input rides the stream.
    let reply = daemon_call("desktop_input", json!({"id":session_vm,"events":raw}))?;
    if !qemu && reply["inject"] == "stream" {
        let line = serde_json::to_vec(&json!({"input":desktop_input::evdev(events)?}))?;
        let mut stdin = input.lock().unwrap();
        stdin.write_all(&line)?;
        stdin.write_all(b"\n")?;
    }
    Ok(())
}

/// Re-serialize a parsed event in the browser wire shape for the daemon.
struct EventWire<'a>(&'a desktop_input::Event);
impl serde::Serialize for EventWire<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use desktop_input::Event::*;
        match self.0 {
            Move { x, y, w, h } => json!({"t":"move","x":x,"y":y,"w":w,"h":h}),
            Button { button, down } => json!({"t":"button","button":button,"down":down}),
            Wheel { dx, dy } => json!({"t":"wheel","dx":dx,"dy":dy}),
            Key { code, down } => json!({"t":"key","code":code,"down":down}),
        }
        .serialize(s)
    }
}

pub fn run(name: &str, codec: &str) -> Result<i32> {
    if !matches!(codec, "hevc" | "h264") {
        bail!("--codec must be hevc or h264")
    }
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    let mut session = match Session::start(name, codec) {
        Ok(session) => session,
        Err(error) => {
            event(
                &out,
                json!({"event":"error","message":format!("{error:#}")}),
            )?;
            return Ok(1);
        }
    };
    let qemu = matches!(session.backend, Backend::Qemu);
    event(
        &out,
        json!({"event":"ready","vm":session.vm,"backend":if qemu {"qemu-hvf"} else {"apple-virtualization"},"codec":codec}),
    )?;
    let video = session.child.stdout.take().unwrap();
    let diagnostics = session.child.stderr.take().unwrap();
    let input = Arc::new(Mutex::new(session.child.stdin.take().unwrap()));
    let video_out = out.clone();
    let video_thread = thread::spawn(move || pump_video(video, video_out));
    let diag_out = out.clone();
    thread::spawn(move || pump_diagnostics(diagnostics, diag_out));

    // Browser input arrives on stdin; the encoder ends on its own on failure.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let vm = session.vm.clone();
    let control = input.clone();
    let input_out = out.clone();
    thread::spawn(move || {
        let mut held = desktop_input::Held::default();
        let mut last_error = Instant::now() - Duration::from_secs(10);
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message["keyframe"] == true {
                let _ = control.lock().unwrap().write_all(b"keyframe\n");
                continue;
            }
            let result = desktop_input::parse(&message["events"]).and_then(|events| {
                inject(&vm, qemu, &control, &events)?;
                held.update(&events);
                Ok(())
            });
            if let Err(error) = result {
                // At most one error report per second; input keeps flowing.
                if last_error.elapsed() > Duration::from_secs(1) {
                    last_error = Instant::now();
                    let _ = event(
                        &input_out,
                        json!({"event":"input_error","message":format!("{error:#}")}),
                    );
                }
            }
        }
        let releases = held.releases();
        if !releases.is_empty() {
            let _ = inject(&vm, qemu, &control, &releases);
        }
        let _ = done_tx.send(());
    });
    loop {
        if done_rx.recv_timeout(Duration::from_millis(100)).is_ok() {
            session.stop(&input);
            let _ = video_thread.join();
            return Ok(0);
        }
        if video_thread.is_finished() {
            let result = video_thread.join().unwrap();
            let status = session.child.wait()?;
            let message = match result {
                Err(error) => format!("{error:#}"),
                Ok(()) => format!("encoder exited ({status})"),
            };
            event(&out, json!({"event":"error","message":message}))?;
            session.stop(&input);
            return Ok(1);
        }
    }
}
