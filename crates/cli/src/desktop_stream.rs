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
//! - Apple Virtualization macOS: the same encoder and bridge, fed by the
//!   worker's Screen Sharing (RFB) session instead of a guest agent.
//!
//! stdout records: `u32 big-endian length, u8 kind, payload`; kind 1 is a JSON
//! event, kind 2 is `u64 pts_us, u8 keyframe, Annex B`. With `--tracks`
//! (remote desktop's shared encoder) an Apple Virtualization encoder runs
//! with crop tracks and its frames come as kind 10, `u8 track, u64 pts_us,
//! u8 keyframe, Annex B`; QEMU has one track and keeps kind 2.
//! stdin: JSON lines, `{"events":[...]}` (see `vm_control::desktop_input`),
//! `{"keyframe":true}`, or `{"encoder":"LINE"}`, one encoder command from
//! docs/browser-desktop.md (`[@T ]keyframe|bitrate|crop|scale`, `track add|remove T`;
//! QEMU takes `bitrate` over its rate channel, 250000..40000000, and no tracks). EOF stops the stream and
//! releases held keys/buttons.
use crate::{container, daemon_call, random_hex, sibling};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::{fd::AsRawFd, unix::fs::DirBuilderExt, unix::net::UnixStream},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
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
    /// Whether the guest paints its cursor into the picture, so the browser must not draw its own.
    guest_cursor: bool,
    child: Child,
    /// The QEMU encoder's private rate channel (`vm-console-stream --rate-control-fd`).
    rate: Option<UnixStream>,
}

/// The controller end of the encoder's rate channel: one request at a time, each acknowledged
/// within the encoder's 3 s deadline (`vm_media_local::rate_control`).
struct RateControl {
    socket: BufReader<UnixStream>,
    instance: Option<String>,
    epoch: u64,
    sequence: u64,
    id: u64,
}

impl RateControl {
    fn new(socket: UnixStream) -> Result<Self> {
        socket.set_read_timeout(Some(Duration::from_secs(3)))?;
        socket.set_write_timeout(Some(Duration::from_secs(1)))?;
        Ok(Self {
            socket: BufReader::new(socket),
            instance: None,
            epoch: 0,
            sequence: 0,
            id: 0,
        })
    }

    fn call(&mut self, mut request: Value) -> Result<Value> {
        self.id += 1;
        request["version"] = json!(1);
        request["request_id"] = json!(format!("r{}", self.id));
        let mut line = serde_json::to_vec(&request)?;
        line.push(b'\n');
        self.socket.get_mut().write_all(&line)?;
        let mut reply = String::new();
        if self.socket.read_line(&mut reply)? == 0 {
            bail!("encoder rate channel closed")
        }
        Ok(serde_json::from_str(&reply)?)
    }

    fn status(&mut self) -> Result<()> {
        let reply = self.call(json!({"method":"rate_status"}))?;
        let result = reply
            .get("result")
            .with_context(|| format!("rate_status refused: {reply}"))?;
        self.instance = result["encoder_instance"].as_str().map(str::to_owned);
        self.epoch = result["epoch"].as_u64().unwrap_or(0);
        self.sequence = result["sequence"].as_u64().unwrap_or(0);
        if self.instance.is_none() || self.epoch == 0 {
            bail!("rate_status without an encoder instance")
        }
        Ok(())
    }

    /// Set the encoder's target and wait for its read-back; one status refresh and retry when
    /// the encoder was replaced (a new epoch or instance).
    fn set(&mut self, bps: u64) -> Result<()> {
        for attempt in 0..2 {
            if self.instance.is_none() || attempt > 0 {
                self.status()?;
            }
            let reply = self.call(json!({
                "method":"set_bitrate",
                "encoder_instance":self.instance,
                "epoch":self.epoch,
                "sequence":self.sequence + 1,
                "target_bps":bps,
            }))?;
            if let Some(result) = reply.get("result") {
                if result["target_bps"] != bps || result["readback_bps"] != bps {
                    bail!("encoder read back {result}, not {bps}")
                }
                self.sequence = result["sequence"].as_u64().unwrap_or(self.sequence + 1);
                return Ok(());
            }
            if attempt == 1 {
                bail!("set_bitrate refused: {reply}")
            }
        }
        unreachable!()
    }
}

/// Apply the newest `bitrate` command; a refusal is reported once as an encoder event.
fn rate_worker(mut control: RateControl, commands: mpsc::Receiver<u64>, out: Out) {
    while let Ok(mut target) = commands.recv() {
        while let Ok(newer) = commands.try_recv() {
            target = newer;
        }
        if let Err(error) = control.set(target) {
            let _ = event(
                &out,
                json!({"event":"encoder","detail":{"event":"rate","command":format!("bitrate {target}"),"error":format!("{error:#}")}}),
            );
        }
    }
}

impl Session {
    fn start(name: &str, codec: &str, tracks: bool) -> Result<Self> {
        let vm = match container::find(name) {
            Ok(c) => c.vm_id,
            Err(_) => crate::machine::resolve(name),
        };
        let status = daemon_call("status", json!({"id":vm}))?;
        if !matches!(status["state"].as_str(), Some("running")) {
            bail!("{name} is not running")
        }
        let backend = status["config"]["backend"].as_str().unwrap_or_default();
        let guest = status["config"]["guest"].as_str().unwrap_or_default();
        let mut command;
        let mut rate = None;
        let session_backend = match (backend, guest) {
            ("qemu-hvf", _) => {
                let console = status["runtime"]["console_socket"]
                    .as_str()
                    .context("this QEMU VM has no persistent console; register the console broker and restart it")?;
                command = Command::new(sibling("vm-console-stream")?);
                command.args([console, "--codec", codec]);
                let (controller, encoder_end) = UnixStream::pair()?;
                // std opens descriptors close-on-exec; the encoder's end must survive the exec.
                if unsafe { libc::fcntl(encoder_end.as_raw_fd(), libc::F_SETFD, 0) } != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                command.args(["--rate-control-fd", &encoder_end.as_raw_fd().to_string()]);
                rate = Some((controller, encoder_end));
                Backend::Qemu
            }
            ("apple-virtualization", "linux" | "macos") => {
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
                if tracks {
                    command.arg("--tracks");
                }
                Backend::Apple { bridge_dir }
            }
            _ => bail!(
                "browser desktop supports Linux containers on QEMU/HVF or Apple Virtualization, and macOS containers"
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
                // Windows paints a software cursor into the display surface, which the
                // encoder captures; Linux and macOS guests use a hardware cursor plane.
                guest_cursor: backend == "qemu-hvf" && guest == "windows",
                child,
                rate: rate.map(|(controller, _encoder_end)| controller),
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

/// Forward the encoder's access units as kind-2 records, or kind 10 with
/// the track byte when the encoder runs with `--tracks`.
fn pump_video(mut from: impl Read, out: Out, tracks: bool) -> Result<()> {
    let mut header = [0u8; 14];
    let header_len = if tracks { 14 } else { 13 };
    loop {
        if let Err(error) = from.read_exact(&mut header[..header_len]) {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                return Ok(());
            }
            return Err(error.into());
        }
        let size = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        if size == 0 || size > MAX_AU {
            bail!("encoder access unit size {size} outside bound")
        }
        if tracks {
            let mut payload = vec![0u8; 10 + size];
            payload[0] = header[13];
            payload[1..10].copy_from_slice(&header[4..13]);
            from.read_exact(&mut payload[10..])?;
            record(&out, 10, &payload)?;
        } else {
            let mut payload = vec![0u8; 9 + size];
            payload[..9].copy_from_slice(&header[4..13]);
            from.read_exact(&mut payload[9..])?;
            record(&out, 2, &payload)?;
        }
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

/// An encoder command line the relay passes through, or `None` when the
/// encoder would not accept it (a malformed line ends the VZ helper).
pub fn encoder_line(line: &str, qemu: bool) -> Option<&str> {
    let allowed =
        |(i, b): (usize, u8)| b == b' ' || b.is_ascii_alphanumeric() || (i == 0 && b == b'@');
    if line.is_empty() || line.len() > 96 || !line.bytes().enumerate().all(allowed) {
        return None;
    }
    let mut words = line.split(' ');
    let mut word = words.next()?;
    let rest: Vec<&str> = if let Some(track) = word.strip_prefix('@') {
        if qemu || !matches!(track.parse::<u8>(), Ok(0..=7)) {
            return None;
        }
        word = words.next()?;
        words.collect()
    } else {
        words.collect()
    };
    let numbers = |n: usize| rest.len() == n && rest.iter().all(|w| w.parse::<u32>().is_ok());
    let ok = match word {
        "keyframe" => rest.is_empty(),
        "crop" => numbers(4),
        "scale" => numbers(2),
        "bitrate" => numbers(1),
        "track" if !qemu && !line.starts_with('@') => {
            rest.len() == 2
                && matches!(rest[0], "add" | "remove")
                && matches!(rest[1].parse::<u8>(), Ok(1..=7))
        }
        _ => false,
    };
    ok.then_some(line)
}

pub fn run(name: &str, codec: &str, tracks: bool) -> Result<i32> {
    if !matches!(codec, "hevc" | "h264") {
        bail!("--codec must be hevc or h264")
    }
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    let mut session = match Session::start(name, codec, tracks) {
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
        json!({"event":"ready","vm":session.vm,"backend":if qemu {"qemu-hvf"} else {"apple-virtualization"},"codec":codec,"cursor":if session.guest_cursor {"guest"} else {"local"}}),
    )?;
    let video = session.child.stdout.take().unwrap();
    let diagnostics = session.child.stderr.take().unwrap();
    let input = Arc::new(Mutex::new(session.child.stdin.take().unwrap()));
    let video_out = out.clone();
    let multi_track = tracks && !qemu;
    // QEMU takes `bitrate` over the encoder's private rate channel, not its stdin.
    let rate_tx = match session.rate.take() {
        Some(socket) => {
            let (tx, rx) = mpsc::channel();
            let control = RateControl::new(socket)?;
            let rate_out = out.clone();
            thread::spawn(move || rate_worker(control, rx, rate_out));
            Some(tx)
        }
        None => None,
    };
    let video_thread = thread::spawn(move || pump_video(video, video_out, multi_track));
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
            if let Some(line) = message["encoder"].as_str() {
                match encoder_line(line, qemu) {
                    Some(line) if rate_tx.is_some() && line.starts_with("bitrate ") => {
                        let target = line["bitrate ".len()..].parse::<u64>().unwrap_or(0);
                        if (250_000..=40_000_000).contains(&target) {
                            let _ = rate_tx.as_ref().unwrap().send(target);
                        } else {
                            let _ = event(
                                &input_out,
                                json!({"event":"encoder","detail":{"event":"rate","command":line,"error":"outside 250000..40000000"}}),
                            );
                        }
                    }
                    Some(line) => {
                        let _ = control
                            .lock()
                            .unwrap()
                            .write_all(format!("{line}\n").as_bytes());
                    }
                    None => {
                        let _ = event(
                            &input_out,
                            json!({"event":"encoder","detail":{"event":"rate","command":line,"error":"not passed to this encoder"}}),
                        );
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An encoder end that acknowledges like the console helper: a status receipt, then each
    /// `set_bitrate` echoed with its sequence, refusing the first as stale when `stale` is set.
    fn fake_encoder(socket: UnixStream, stale: bool) -> thread::JoinHandle<Vec<Value>> {
        thread::spawn(move || {
            let mut seen = Vec::new();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut writer = socket;
            let mut refused = !stale;
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let request: Value = serde_json::from_str(&line).unwrap();
                line.clear();
                let reply = if request["method"] == "rate_status" {
                    json!({"version":1,"request_id":request["request_id"],"result":{"encoder_instance":"0123456789abcdef0123456789abcdef","epoch":7,"sequence":4,"target_bps":8000000,"readback_bps":8000000,"submitted_frames":1,"hardware_verified":true}})
                } else if !refused {
                    refused = true;
                    json!({"version":1,"request_id":request["request_id"],"error":{"code":"stale_encoder"}})
                } else {
                    json!({"version":1,"request_id":request["request_id"],"result":{"encoder_instance":request["encoder_instance"],"epoch":request["epoch"],"sequence":request["sequence"],"target_bps":request["target_bps"],"readback_bps":request["target_bps"],"submitted_frames":2,"hardware_verified":true}})
                };
                seen.push(request);
                writeln!(writer, "{reply}").unwrap();
            }
            seen
        })
    }

    #[test]
    fn rate_control_reads_status_then_sets_with_the_next_sequence() {
        let (controller, encoder) = UnixStream::pair().unwrap();
        let peer = fake_encoder(encoder, false);
        let mut control = RateControl::new(controller).unwrap();
        control.set(12_000_000).unwrap();
        control.set(16_000_000).unwrap();
        drop(control);
        let seen = peer.join().unwrap();
        let methods: Vec<_> = seen.iter().map(|r| r["method"].as_str().unwrap()).collect();
        assert_eq!(methods, ["rate_status", "set_bitrate", "set_bitrate"]);
        assert_eq!(seen[1]["sequence"], 5);
        assert_eq!(seen[2]["sequence"], 6);
        assert_eq!(seen[2]["target_bps"], 16_000_000);
        assert_eq!(seen[1]["epoch"], 7);
    }

    #[test]
    fn rate_control_refreshes_status_once_after_a_stale_refusal() {
        let (controller, encoder) = UnixStream::pair().unwrap();
        let peer = fake_encoder(encoder, true);
        let mut control = RateControl::new(controller).unwrap();
        control.set(24_000_000).unwrap();
        drop(control);
        let methods: Vec<_> = peer
            .join()
            .unwrap()
            .iter()
            .map(|r| r["method"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            methods,
            ["rate_status", "set_bitrate", "rate_status", "set_bitrate"]
        );
    }

    #[test]
    fn encoder_lines_follow_each_encoders_grammar() {
        for ok in [
            "keyframe",
            "@3 keyframe",
            "bitrate 4000000",
            "@1 crop 0 0 960 540",
            "scale 1280 720",
            "track add 2",
            "track remove 7",
        ] {
            assert_eq!(encoder_line(ok, false), Some(ok), "{ok}");
        }
        for bad in [
            "",
            "stop",
            "@8 keyframe",
            "@1 track add 2",
            "track add 0",
            "crop 1 2 3",
            "bitrate -1",
            "keyframe\nstop",
            "{\"input\":[]}",
            "scale 1 2 3",
        ] {
            assert_eq!(encoder_line(bad, false), None, "{bad:?}");
        }
        assert_eq!(
            encoder_line("crop 0 0 960 540", true),
            Some("crop 0 0 960 540")
        );
        assert_eq!(
            encoder_line("bitrate 1000000", true),
            Some("bitrate 1000000")
        );
        for qemu_bad in ["@1 keyframe", "track add 1"] {
            assert_eq!(encoder_line(qemu_bad, true), None, "{qemu_bad}");
        }
    }
}
