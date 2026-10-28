//! `desktop.stream`: low-latency desktop relay for the web UI.
//!
//! The web UI opens one stream per browser session (today through
//! `veynodevm desktop --stream`, which copies it to its pipes). Video comes from a hardware encoder child that already speaks the
//! sender framing (`u32 size, u64 pts_us, u8 keyframe, Annex B`):
//!
//! - QEMU/HVF: `vm-console-stream` against the worker's persistent SPICE
//!   console broker (VideoToolbox on the console surface).
//! - Apple Virtualization Linux: `vz-desktop-stream` against the worker's
//!   vsock 5001 bridge to the guest agent's DRM scanout capture.
//! - Apple Virtualization macOS: the same encoder and bridge, fed by the
//!   worker's Screen Sharing (RFB) session instead of a guest agent.
//!
//! `stdout` carries records `u32 big-endian length, u8 kind, payload`; kind 1 is a JSON
//! event, kind 2 is `u64 pts_us, u8 keyframe, Annex B`. With `--tracks`
//! (remote desktop's shared encoder) an Apple Virtualization encoder runs
//! with crop tracks and its frames come as kind 10, `u8 track, u64 pts_us,
//! u8 keyframe, Annex B`; QEMU has one track and keeps kind 2.
//! Guest audio (a VM with `audio`, announced as `"audio":true` in `ready`) comes
//! as kind 20, `u32 rate, u8 channels, s16le PCM`, while the browser asks for it
//! with `{"audio":true}` (`false` stops it); `{"mic":"BASE64"}` is up to 20 ms of
//! 48 kHz mono s16le microphone PCM. QEMU's audio is the console broker's
//! (`audio_start`/`audio_in`), Apple Virtualization's rides the guest stream
//! (`vz-desktop-stream --audio-fd`).
//! The Guest clipboard (Linux desktops and macOS containers, `"clipboard":true` in `ready`;
//! `"guest"` names the Guest OS): the `control` message
//! `{"clipboard":"TEXT"}` (UTF-8, at most 200 KiB) offers text to the Guest,
//! `{"clipboard_watch":true}` reports every Guest copy and `{"clipboard_get":true}`
//! the current text, each as the event `{"event":"clipboard","text"}` (or
//! `"error"`). QEMU's goes through the broker to the Guest's spice-vdagent, Apple
//! Virtualization's through the guest stream to the agent's X11 clipboard, and a
//! macOS container's over SSH to `macos_clipboard.js` in its login session.
//! The Guest pointer and monitor (QEMU, through the broker; `"cursorShape"` and
//! `"monitor"` true in `ready`): `{"cursor_watch":true}` reports the Guest's
//! pointer shape now and on every change as `{"event":"cursor","width","height",
//! "hot_x","hot_y","rgba"}` (base64 of straight-alpha RGBA rows) or
//! `{"event":"cursor","hidden":true}`; `{"monitor":{"width","height"}}` asks the
//! Guest's spice-vdagent for that size, answered as `{"event":"monitor",…}` with the
//! size or an `error`.
//! `control` messages: `{"events":[...]}` (see `crate::desktop_input`),
//! `{"keyframe":true}`, or `{"encoder":"LINE"}`, one encoder command from
//! docs/browser-desktop.md (`[@T ]keyframe|bitrate|crop|scale`, `fps N`, `track add|remove T`;
//! QEMU takes `bitrate` over its rate channel, 250000..40000000, and no tracks). The client
//! leaving stops the stream and releases held keys/buttons.
use super::{container, daemon_call, sibling, util::random_hex};
use crate::api_server::{Ctx, StreamIo};
use crate::{desktop_input, home};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    io::{BufRead, BufReader, Read, Write},
    net::Shutdown,
    os::{
        fd::AsRawFd,
        unix::{fs::DirBuilderExt, net::UnixStream, process::CommandExt},
    },
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_AU: usize = 4 * 1024 * 1024;

type Out = Arc<Mutex<Box<dyn Write + Send>>>;

fn record(out: &Out, kind: u8, payload: &[u8]) -> Result<()> {
    // One write, so a record is one stream frame.
    let mut bytes = Vec::with_capacity(payload.len() + 5);
    bytes.extend_from_slice(&((payload.len() + 1) as u32).to_be_bytes());
    bytes.push(kind);
    bytes.extend_from_slice(payload);
    let mut stdout = out.lock().unwrap();
    stdout.write_all(&bytes)?;
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
    /// The VM has a sound card.
    audio: bool,
    /// The Guest can share its clipboard.
    clipboard: bool,
    /// A macOS container, whose clipboard is reached over SSH.
    macos: Option<container::Container>,
    /// `linux`, `windows` or `macos`, for the browser's shortcut mapping.
    guest: String,
    link: Option<AudioLink>,
    /// A QEMU VM's one encoder session, released when the session is dropped.
    _gate: Option<EncoderGate>,
}

/// The QEMU VMs whose encoder session is running. The console broker takes
/// one encoder at a time, so a viewer that reconnects waits here until the
/// previous session has stopped its encoder instead of being refused as busy.
static ENCODING: (Mutex<Option<HashSet<String>>>, Condvar) = (Mutex::new(None), Condvar::new());

struct EncoderGate(String);

impl EncoderGate {
    /// The VM's gate, or `None` once `wait` passes with another session still
    /// holding it (a live viewer of another client: the broker then refuses).
    fn enter(vm: &str, wait: Duration) -> Option<Self> {
        let deadline = Instant::now() + wait;
        let mut running = ENCODING.0.lock().unwrap();
        while running.get_or_insert_with(HashSet::new).contains(vm) {
            let left = deadline.checked_duration_since(Instant::now())?;
            running = ENCODING.1.wait_timeout(running, left).unwrap().0;
        }
        running
            .get_or_insert_with(HashSet::new)
            .insert(vm.to_owned());
        Some(Self(vm.to_owned()))
    }
}

impl Drop for EncoderGate {
    fn drop(&mut self) {
        if let Some(running) = ENCODING.0.lock().unwrap().as_mut() {
            running.remove(&self.0);
        }
        ENCODING.1.notify_all();
    }
}

/// A socket pair whose second end `command`'s child gets as `flag FD`. The
/// end stays close-on-exec here and is cleared only in that child, so a spawn
/// on another daemon thread can never inherit it.
fn encoder_channel(command: &mut Command, flag: &str) -> Result<(UnixStream, UnixStream)> {
    let (ours, theirs) = UnixStream::pair()?;
    let fd = theirs.as_raw_fd();
    command.args([flag, &fd.to_string()]);
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok((ours, theirs))
}

/// The session's own connection to the QEMU console broker: requests go
/// through `link`, a thread turns its records into events. Dropping it shuts
/// the connection, which ends that thread and frees the broker's slot at once
/// (the broker serves 8 connections and drops an idle one only after 300 s).
struct BrokerLink {
    link: Arc<Mutex<UnixStream>>,
    socket: UnixStream,
}

impl BrokerLink {
    fn start(link: UnixStream, hearing: Arc<AtomicBool>, out: Out) -> Result<Self> {
        let socket = link.try_clone()?;
        let reader = link.try_clone()?;
        thread::spawn(move || pump_audio(reader, true, hearing, out));
        let link = Arc::new(Mutex::new(link));
        // The broker ends a connection that sends nothing for 300 s (a muted microphone).
        let keepalive = Arc::downgrade(&link);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(60));
            let Some(link) = keepalive.upgrade() else {
                return;
            };
            let line = b"{\"request_id\":\"keepalive\",\"method\":\"status\"}\n";
            if link.lock().unwrap().write_all(line).is_err() {
                return;
            }
        });
        Ok(Self { link, socket })
    }

    fn send(&self, line: &[u8]) -> std::io::Result<()> {
        self.link.lock().unwrap().write_all(line)
    }
}

impl Drop for BrokerLink {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

/// Where Guest audio comes from and the microphone goes.
enum AudioLink {
    /// A client connection of its own to the QEMU console broker, which also carries the clipboard.
    Broker(UnixStream),
    /// The read end of `vz-desktop-stream --audio-fd`; the microphone rides the encoder's stdin.
    Encoder(UnixStream),
}

/// A browser microphone frame: base64 of at most 20 ms of 48 kHz mono s16le.
pub fn mic_frame(value: &Value) -> Option<&str> {
    let pcm = value.as_str()?;
    let base64 = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=');
    (!pcm.is_empty() && pcm.len() <= 2560 && pcm.len().is_multiple_of(4) && pcm.bytes().all(base64))
        .then_some(pcm)
}

/// Most clipboard text a viewer and the Guest exchange.
const MAX_CLIPBOARD: usize = 200 * 1024;

/// A clipboard message from the browser as the lines that carry it: broker requests (the
/// text in base64 parts that fit its 4 KiB request line) or one guest stream line.
/// `None` when the message is not about the clipboard.
pub fn clipboard_lines(message: &Value, broker: bool) -> Option<Result<Vec<Value>, String>> {
    let text = &message["clipboard"];
    let watch = message["clipboard_watch"].as_bool();
    let get = message["clipboard_get"] == true;
    if text.is_null() && watch.is_none() && !get {
        return None;
    }
    let text = match text {
        Value::Null => None,
        Value::String(text) if text.len() <= MAX_CLIPBOARD => Some(text.as_str()),
        Value::String(_) => return Some(Err("clipboard text is over 200 KiB".into())),
        _ => return Some(Err("clipboard text must be a string".into())),
    };
    if !broker {
        let mut line = json!({});
        if let Some(text) = text {
            line["clipboard"] = json!(text);
        }
        if let Some(watch) = watch {
            line["clipboard_watch"] = json!(watch);
        }
        if get {
            line["clipboard_get"] = json!(true);
        }
        return Some(Ok(vec![line]));
    }
    let mut lines = Vec::new();
    if let Some(text) = text {
        // 1800 bytes are 2400 base64 characters, so each part decodes on its own.
        let parts: Vec<&[u8]> = if text.is_empty() {
            vec![b""]
        } else {
            text.as_bytes().chunks(1800).collect()
        };
        for (i, part) in parts.iter().enumerate() {
            lines.push(json!({"request_id":"clipboard","method":"clipboard_chunk","data":super::machines::base64(part),"last":i + 1 == parts.len()}));
        }
    }
    if let Some(watch) = watch {
        lines.push(json!({"request_id":"clipboard","method":if watch {"clipboard_watch"} else {"clipboard_unwatch"}}));
    }
    if get {
        lines.push(json!({"request_id":"clipboard","method":"clipboard_get"}));
    }
    Some(Ok(lines))
}

/// The broker lines for a browser `{"audio"}` or `{"mic"}` message. The broker
/// sends its microphone to the client that last asked for audio, so the first
/// of either claims it and the claim is kept; muting only stops `hearing`, the
/// speaker records this relay forwards.
fn broker_audio(
    on: Option<bool>,
    mic: Option<&str>,
    claimed: &mut bool,
    hearing: &AtomicBool,
) -> Vec<Value> {
    if let Some(on) = on {
        hearing.store(on, Ordering::Release);
    }
    let mut lines = Vec::new();
    if !*claimed && (on == Some(true) || mic.is_some()) {
        *claimed = true;
        lines.push(json!({"request_id":"audio","method":"audio_start"}));
    }
    if let (None, Some(pcm)) = (on, mic) {
        lines.push(json!({"request_id":"mic","method":"audio_in","pcm":pcm}));
    }
    lines
}

/// Guest audio to kind-20 records while `hearing`: broker records (13-byte
/// header, kind 3) or the VZ encoder's (`u32 size`, body). The broker's
/// clipboard events and refusals become events.
fn pump_audio(link: UnixStream, broker: bool, hearing: Arc<AtomicBool>, out: Out) -> Result<()> {
    let mut from = BufReader::new(link);
    loop {
        let mut header = [0u8; 13];
        let header = &mut header[..if broker { 13 } else { 4 }];
        from.read_exact(header)?;
        let size = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        if size == 0 || size > 12 * 1024 * 1024 || (header.len() == 4 && size > 5 + 64 * 1024) {
            bail!("audio record size {size} outside bound")
        }
        let mut body = vec![0; size];
        from.read_exact(&mut body)?;
        match header.get(4) {
            None | Some(3) if hearing.load(Ordering::Acquire) => record(&out, 20, &body)?,
            None | Some(3) => {}
            Some(1) => {
                let reply: Value = serde_json::from_slice(&body)?;
                if reply["event"] == "clipboard" || reply["event"] == "cursor" {
                    event(&out, reply)?;
                } else if reply["request_id"] == "monitor" {
                    let mut answer = json!({"event":"monitor"});
                    match reply["error"].is_null() {
                        true => {
                            answer["width"] = reply["result"]["width"].clone();
                            answer["height"] = reply["result"]["height"].clone();
                        }
                        false => answer["error"] = reply["error"].clone(),
                    }
                    event(&out, answer)?;
                } else if !reply["error"].is_null() {
                    // Only a refusal matters; acknowledgements are not forwarded.
                    event(
                        &out,
                        match reply["request_id"].as_str() {
                            Some("clipboard") => {
                                json!({"event":"clipboard","error":reply["error"]})
                            }
                            Some("cursor") => json!({"event":"cursor","error":reply["error"]}),
                            _ => json!({"event":"audio_error","message":reply["error"]}),
                        },
                    )?;
                }
            }
            Some(_) => {}
        }
    }
}

/// The broker line for a browser `{"cursor_watch"}` or `{"monitor"}` message;
/// `None` when the message is neither, an error when it is malformed.
pub fn pointer_line(message: &Value) -> Option<Result<Value, String>> {
    if let Some(watch) = message["cursor_watch"].as_bool() {
        return Some(Ok(json!({"request_id":"cursor",
            "method":if watch {"cursor_watch"} else {"cursor_unwatch"}})));
    }
    let monitor = &message["monitor"];
    if monitor.is_null() {
        return None;
    }
    let side = |key: &str, low: u64| {
        monitor[key]
            .as_u64()
            .filter(|v| (low..=8192).contains(v))
            .ok_or_else(|| format!("monitor.{key} must be {low} through 8192"))
    };
    Some((|| {
        Ok(json!({"request_id":"monitor","method":"monitor_config",
            "width":side("width", 320)?,"height":side("height", 200)?}))
    })())
}

/// JSON with every non-ASCII character as a `\u` escape: the input `macos_clipboard.js`
/// reads. Non-ASCII characters only occur inside JSON strings, where the escape is valid.
fn ascii_json(value: &Value) -> String {
    let mut out = String::new();
    for c in value.to_string().chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

/// Starts a macOS container's clipboard script: its events go out as they come, and a
/// tick every 300 ms lets it report Guest copies (it cannot wait on stdin and a timer at once).
fn mac_clipboard_start(
    container: &container::Container,
    out: &Out,
) -> Result<Arc<Mutex<ChildStdin>>> {
    let mut child = super::macos::clipboard_command(container)?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting the Guest's clipboard session")?;
    let stdin = Arc::new(Mutex::new(child.stdin.take().unwrap()));
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let events = out.clone();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            match serde_json::from_str::<Value>(&line) {
                Ok(value) if value["event"] == "clipboard" => {
                    if event(&events, value).is_err() {
                        break;
                    }
                }
                _ => {}
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        let mut reason = String::new();
        let _ = stderr.take(400).read_to_string(&mut reason);
        let reason = reason.trim();
        let _ = event(
            &events,
            json!({"event":"clipboard","error":if reason.is_empty() {"the Guest's clipboard session ended".to_owned()} else {format!("the Guest's clipboard session ended: {reason}")}}),
        );
    });
    let tick = Arc::downgrade(&stdin);
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(300));
        let Some(stdin) = tick.upgrade() else {
            return;
        };
        if stdin
            .lock()
            .unwrap()
            .write_all(b"{\"poll\":true}\n")
            .is_err()
        {
            return;
        }
    });
    Ok(stdin)
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
            Err(_) => super::machines::resolve(name),
        };
        let status = daemon_call("status", json!({"id":vm}))?;
        if !matches!(status["state"].as_str(), Some("running")) {
            bail!("{name} is not running")
        }
        let backend = status["config"]["backend"].as_str().unwrap_or_default();
        let guest = status["config"]["guest"].as_str().unwrap_or_default();
        let has_audio = status["config"]["audio"] == true;
        let mut command;
        let mut rate = None;
        let mut audio = None;
        let mut gate = None;
        let session_backend = match (backend, guest) {
            ("qemu-hvf", _) => {
                let console = status["runtime"]["console_socket"]
                    .as_str()
                    .context("this QEMU VM has no persistent console; register the console broker and restart it")?;
                // A replaced viewer's session stops its encoder within about 3 s.
                gate = EncoderGate::enter(&vm, Duration::from_secs(4));
                command = Command::new(sibling("vm-console-stream")?);
                command.args([console, "--codec", codec]);
                rate = Some(encoder_channel(&mut command, "--rate-control-fd")?);
                // Linux Guests share the clipboard through the broker too.
                if has_audio || guest == "linux" {
                    audio = Some((AudioLink::Broker(UnixStream::connect(console)?), None));
                }
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
                if has_audio {
                    let (relay_end, encoder_end) = encoder_channel(&mut command, "--audio-fd")?;
                    audio = Some((AudioLink::Encoder(relay_end), Some(encoder_end)));
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
            Ok(child) => {
                // A macOS Guest's clipboard goes over its container's SSH login.
                let macos = (guest == "macos")
                    .then(|| container::find(name).ok())
                    .flatten();
                Ok(Self {
                    vm,
                    backend: session_backend,
                    // Windows paints a software cursor into the display surface, which the
                    // encoder captures; Linux and macOS guests use a hardware cursor plane.
                    guest_cursor: backend == "qemu-hvf" && guest == "windows",
                    child,
                    rate: rate.map(|(controller, _encoder_end)| controller),
                    audio: has_audio,
                    clipboard: guest == "linux" || macos.is_some(),
                    macos,
                    guest: guest.to_owned(),
                    // The encoder's end closes here, in the parent, once the child holds it.
                    link: audio.map(|(link, _encoder_end)| link),
                    _gate: gate,
                })
            }
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

/// Encoder diagnostics are JSON lines; pass them through as events. `last`
/// keeps the latest line, the reason an encoder that fails prints last.
fn pump_diagnostics(from: impl Read, out: Out, last: &Mutex<Option<String>>) {
    for line in BufReader::new(from).lines().map_while(Result::ok) {
        *last.lock().unwrap() = Some(line.chars().take(400).collect());
        let value = serde_json::from_str::<Value>(&line)
            .unwrap_or_else(|_| json!({"text":line.chars().take(400).collect::<String>()}));
        // The guest agent's clipboard events reach the encoder as guest diagnostics.
        let value = if value["event"] == "guest" && value["detail"]["event"] == "clipboard" {
            value["detail"].clone()
        } else {
            json!({"event":"encoder","detail":value})
        };
        if event(&out, value).is_err() {
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
        // The viewer's screen refresh; it holds the whole stream, never one track.
        "fps" if !line.starts_with('@') => numbers(1),
        "track" if !qemu && !line.starts_with('@') => {
            rest.len() == 2
                && matches!(rest[0], "add" | "remove")
                && matches!(rest[1].parse::<u8>(), Ok(1..=7))
        }
        _ => false,
    };
    ok.then_some(line)
}

/// `desktop.stream {name, codec?, tracks?}`.
pub fn stream(_: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
    let name = super::util::string(&args, "name")?;
    let codec = super::util::opt_string(&args, "codec")?.unwrap_or("hevc");
    if !matches!(codec, "hevc" | "h264") {
        return Err(veynodevm_api::refuse(
            veynodevm_api::ErrorCode::Invalid,
            "codec must be hevc or h264",
        ));
    }
    let code = run(name, codec, super::util::flag(&args, "tracks")?, io)?;
    Ok(json!({ "exitCode": code }))
}

fn run(name: &str, codec: &str, tracks: bool, io: &StreamIo) -> Result<i32> {
    let out: Out = Arc::new(Mutex::new(Box::new(io.output())));
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
        json!({"event":"ready","vm":session.vm,"backend":if qemu {"qemu-hvf"} else {"apple-virtualization"},"codec":codec,"cursor":if session.guest_cursor {"guest"} else {"local"},"audio":session.audio,"clipboard":session.clipboard,"guest":session.guest,"cursorShape":qemu && session.link.is_some(),"monitor":qemu && session.link.is_some()}),
    )?;
    // The broker connection takes requests from the input thread; the encoder's audio socket only reads.
    let hearing = Arc::new(AtomicBool::new(false));
    // Dropped on every way out of this function, which closes the broker connection.
    let broker = match session.link.take() {
        Some(AudioLink::Broker(link)) => {
            Some(BrokerLink::start(link, hearing.clone(), out.clone())?)
        }
        Some(AudioLink::Encoder(link)) => {
            let audio_out = out.clone();
            // The encoder itself stops sending when told `{"audio":false}`.
            let hearing = Arc::new(AtomicBool::new(true));
            thread::spawn(move || pump_audio(link, false, hearing, audio_out));
            None
        }
        None => None,
    };
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
    let last_diagnostic = Arc::new(Mutex::new(None));
    let last = last_diagnostic.clone();
    let diag_thread = thread::spawn(move || pump_diagnostics(diagnostics, diag_out, &last));

    // Browser input arrives as control messages; the encoder ends on its own
    // on failure.
    let vm = session.vm.clone();
    let clipboard = session.clipboard;
    let macos = session.macos.take();
    let control = input.clone();
    let input_out = out.clone();
    let encoder_ended = {
        let mut held = desktop_input::Held::default();
        let mut claimed = false;
        let mut mac_clipboard: Option<Arc<Mutex<ChildStdin>>> = None;
        let mut last_error = Instant::now() - Duration::from_secs(10);
        let ended = loop {
            if video_thread.is_finished() {
                break true;
            }
            if io.closed() {
                break false;
            }
            let Some(message) = io.control(Duration::from_millis(100)) else {
                continue;
            };
            if message["keyframe"] == true {
                let _ = control.lock().unwrap().write_all(b"keyframe\n");
                continue;
            }
            if message["audio"].is_boolean() || !message["mic"].is_null() {
                let on = message["audio"].as_bool();
                let mic = mic_frame(&message["mic"]);
                let lines = match &broker {
                    Some(_) => broker_audio(on, mic, &mut claimed, &hearing),
                    None if qemu => continue,
                    None => match (on, mic) {
                        (Some(on), _) => vec![json!({"audio":on})],
                        (None, Some(pcm)) => vec![json!({"mic":pcm})],
                        (None, None) => continue,
                    },
                };
                for line in lines {
                    let mut line = serde_json::to_vec(&line).unwrap_or_default();
                    line.push(b'\n');
                    let _ = match &broker {
                        Some(broker) => broker.send(&line),
                        None => control.lock().unwrap().write_all(&line),
                    };
                }
                continue;
            }
            if let Some(line) = pointer_line(&message) {
                let line = match (line, &broker) {
                    (Ok(line), Some(broker)) => {
                        let mut bytes = serde_json::to_vec(&line).unwrap_or_default();
                        bytes.push(b'\n');
                        let _ = broker.send(&bytes);
                        continue;
                    }
                    (Ok(_), None) => "this desktop has no Guest pointer or monitor link".to_owned(),
                    (Err(error), _) => error,
                };
                let kind = if message["monitor"].is_null() {
                    "cursor"
                } else {
                    "monitor"
                };
                let _ = event(&input_out, json!({"event":kind,"error":line}));
                continue;
            }
            if let Some(lines) = clipboard_lines(&message, broker.is_some()) {
                let lines = match lines {
                    Ok(_) if !clipboard => Err("this desktop does not share its clipboard".into()),
                    lines => lines,
                };
                match lines {
                    Ok(lines) if macos.is_some() => {
                        let link = match &mac_clipboard {
                            Some(link) => Ok(link.clone()),
                            None => mac_clipboard_start(macos.as_ref().unwrap(), &input_out),
                        };
                        match link {
                            Ok(link) => {
                                mac_clipboard = Some(link.clone());
                                for line in lines {
                                    let line = format!("{}\n", ascii_json(&line));
                                    if link.lock().unwrap().write_all(line.as_bytes()).is_err() {
                                        // The SSH session ended (and said so): start a new one next time.
                                        mac_clipboard = None;
                                        break;
                                    }
                                }
                            }
                            Err(error) => {
                                let _ = event(
                                    &input_out,
                                    json!({"event":"clipboard","error":format!("{error:#}")}),
                                );
                            }
                        }
                    }
                    Ok(lines) => {
                        for line in lines {
                            let mut line = serde_json::to_vec(&line).unwrap_or_default();
                            line.push(b'\n');
                            let _ = match &broker {
                                Some(broker) => broker.send(&line),
                                None => control.lock().unwrap().write_all(&line),
                            };
                        }
                    }
                    Err(error) => {
                        let _ = event(&input_out, json!({"event":"clipboard","error":error}));
                    }
                }
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
        };
        let releases = held.releases();
        if !releases.is_empty() {
            let _ = inject(&vm, qemu, &control, &releases);
        }
        ended
    };
    if !encoder_ended {
        session.stop(&input);
        let _ = video_thread.join();
        return Ok(0);
    }
    let result = video_thread.join().unwrap();
    let status = session.child.wait()?;
    // Its stderr ends with it; give the last line a moment to arrive.
    let deadline = Instant::now() + Duration::from_secs(1);
    while !diag_thread.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let mut message = match result {
        Err(error) => format!("{error:#}"),
        Ok(()) => format!("encoder exited ({status})"),
    };
    if let Some(last) = last_diagnostic.lock().unwrap().take() {
        message = format!("{message}: {last}");
    }
    event(&out, json!({"event":"error","message":message}))?;
    session.stop(&input);
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_and_monitor_messages_become_broker_lines() {
        assert_eq!(
            pointer_line(&json!({"cursor_watch":true}))
                .unwrap()
                .unwrap(),
            json!({"request_id":"cursor","method":"cursor_watch"})
        );
        assert_eq!(
            pointer_line(&json!({"cursor_watch":false}))
                .unwrap()
                .unwrap()["method"],
            "cursor_unwatch"
        );
        assert_eq!(
            pointer_line(&json!({"monitor":{"width":1920,"height":1080}}))
                .unwrap()
                .unwrap(),
            json!({"request_id":"monitor","method":"monitor_config","width":1920,"height":1080})
        );
        assert!(
            pointer_line(&json!({"monitor":{"width":100,"height":1080}}))
                .unwrap()
                .is_err()
        );
        assert!(pointer_line(&json!({"keyframe":true})).is_none());
    }

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

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The `(kind, payload)` records `run` writes.
    fn capture(write: impl FnOnce(Out)) -> Vec<(u8, Vec<u8>)> {
        let sink = Sink::default();
        write(Arc::new(Mutex::new(Box::new(sink.clone()))));
        let bytes = sink.0.lock().unwrap().clone();
        let mut records = Vec::new();
        let mut rest = &bytes[..];
        while !rest.is_empty() {
            let size = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
            records.push((rest[4], rest[5..4 + size].to_vec()));
            rest = &rest[4 + size..];
        }
        records
    }

    #[test]
    fn the_macos_clipboard_script_reads_ascii_lines() {
        let line = clipboard_lines(&json!({"clipboard":"café 你好 😀 \"q\"\n"}), false)
            .unwrap()
            .unwrap();
        let text = ascii_json(&line[0]);
        assert!(text.is_ascii());
        assert_eq!(
            text,
            r#"{"clipboard":"caf\u00e9 \u4f60\u597d \ud83d\ude00 \"q\"\n"}"#
        );
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), line[0]);
    }

    #[test]
    fn microphone_frames_are_bounded_base64() {
        assert_eq!(mic_frame(&json!("AQAB")), Some("AQAB"));
        assert_eq!(
            mic_frame(&json!("A".repeat(2560))),
            Some(&*"A".repeat(2560))
        );
        for bad in [
            json!(""),
            json!("A".repeat(2564)),
            json!("AQA"),
            json!("AQ\"B"),
            json!("AQ\nB"),
            json!(12),
        ] {
            assert_eq!(mic_frame(&bad), None, "{bad}");
        }
    }

    #[test]
    fn muting_keeps_the_brokers_microphone() {
        let hearing = AtomicBool::new(false);
        let mut claimed = false;
        // A muted viewer's first microphone frame claims the broker's audio, then is sent.
        assert_eq!(
            broker_audio(None, Some("AQAB"), &mut claimed, &hearing),
            [
                json!({"request_id":"audio","method":"audio_start"}),
                json!({"request_id":"mic","method":"audio_in","pcm":"AQAB"}),
            ]
        );
        assert!(!hearing.load(Ordering::Acquire));
        assert_eq!(
            broker_audio(Some(true), None, &mut claimed, &hearing),
            Vec::<Value>::new()
        );
        assert!(hearing.load(Ordering::Acquire));
        // Muting sends nothing: no audio_stop, so later frames still reach the Guest.
        assert_eq!(
            broker_audio(Some(false), None, &mut claimed, &hearing),
            Vec::<Value>::new()
        );
        assert!(!hearing.load(Ordering::Acquire));
        assert_eq!(
            broker_audio(None, Some("AQAB"), &mut claimed, &hearing).len(),
            1
        );
        let mut fresh = false;
        assert_eq!(
            broker_audio(Some(false), None, &mut fresh, &hearing),
            Vec::<Value>::new()
        );
        assert_eq!(
            broker_audio(Some(true), None, &mut fresh, &hearing),
            [json!({"request_id":"audio","method":"audio_start"})]
        );
    }

    #[test]
    fn audio_records_become_kind_20_and_broker_refusals_events() {
        let (mut guest, relay) = UnixStream::pair().unwrap();
        let mut body = 48_000u32.to_be_bytes().to_vec();
        body.extend([2, 1, 0, 2, 0]);
        guest.write_all(&(body.len() as u32).to_be_bytes()).unwrap();
        guest.write_all(&body).unwrap();
        drop(guest);
        let records = capture(|out| {
            let _ = pump_audio(relay, false, Arc::new(AtomicBool::new(true)), out);
        });
        assert_eq!(records, [(20, body.clone())]);

        let (mut broker, relay) = UnixStream::pair().unwrap();
        for (kind, payload) in [
            (
                1u8,
                br#"{"request_id":"audio","result":{"listening":true}}"#.to_vec(),
            ),
            (3, body.clone()),
            (
                1,
                br#"{"request_id":"audio","error":"capture process unavailable"}"#.to_vec(),
            ),
            (
                1,
                br#"{"request_id":"clipboard","result":{"watching":true}}"#.to_vec(),
            ),
            (1, br#"{"event":"clipboard","text":"hi"}"#.to_vec()),
            (
                1,
                br#"{"request_id":"clipboard","error":"invalid clipboard part"}"#.to_vec(),
            ),
        ] {
            broker
                .write_all(&(payload.len() as u32).to_be_bytes())
                .unwrap();
            broker.write_all(&[kind]).unwrap();
            broker.write_all(&0u64.to_be_bytes()).unwrap();
            broker.write_all(&payload).unwrap();
        }
        drop(broker);
        let records = capture(|out| {
            let _ = pump_audio(relay, true, Arc::new(AtomicBool::new(true)), out);
        });
        assert_eq!(records[0], (20, body));
        assert_eq!(records[1].0, 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&records[1].1).unwrap(),
            json!({"event":"audio_error","message":"capture process unavailable"})
        );
        let events: Vec<Value> = records[2..]
            .iter()
            .map(|(_, body)| serde_json::from_slice(body).unwrap())
            .collect();
        assert_eq!(
            events,
            [
                json!({"event":"clipboard","text":"hi"}),
                json!({"event":"clipboard","error":"invalid clipboard part"})
            ]
        );
    }

    #[test]
    fn clipboard_text_goes_to_the_broker_in_parts_and_to_the_guest_whole() {
        assert!(clipboard_lines(&json!({"events":[]}), true).is_none());
        // 4000 bytes: parts of 1800, 1800 and 400 bytes, each its own base64.
        let text = "é".repeat(2000);
        let lines = clipboard_lines(&json!({"clipboard":text,"clipboard_watch":true}), true)
            .unwrap()
            .unwrap();
        let methods: Vec<_> = lines
            .iter()
            .map(|l| l["method"].as_str().unwrap())
            .collect();
        assert_eq!(
            methods,
            [
                "clipboard_chunk",
                "clipboard_chunk",
                "clipboard_chunk",
                "clipboard_watch"
            ]
        );
        let lasts: Vec<_> = lines[..3].iter().map(|l| l["last"] == true).collect();
        assert_eq!(lasts, [false, false, true]);
        assert!(lines[..3]
            .iter()
            .all(|l| l["data"].as_str().unwrap().len() <= 2400));
        assert_eq!(
            lines[0]["data"],
            super::super::machines::base64(&text.as_bytes()[..1800])
        );
        // An empty text still clears the Guest's clipboard: one empty, last part.
        assert_eq!(
            clipboard_lines(&json!({"clipboard":""}), true)
                .unwrap()
                .unwrap(),
            [json!({"request_id":"clipboard","method":"clipboard_chunk","data":"","last":true})]
        );
        assert_eq!(
            clipboard_lines(&json!({"clipboard":"hi","clipboard_get":true}), false)
                .unwrap()
                .unwrap(),
            [json!({"clipboard":"hi","clipboard_get":true})]
        );
        assert_eq!(
            clipboard_lines(&json!({"clipboard_watch":false}), true)
                .unwrap()
                .unwrap(),
            [json!({"request_id":"clipboard","method":"clipboard_unwatch"})]
        );
        for bad in [
            json!({"clipboard":"x".repeat(MAX_CLIPBOARD + 1)}),
            json!({"clipboard":7}),
        ] {
            assert!(clipboard_lines(&bad, true).unwrap().is_err(), "{bad}");
        }
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
            "fps 120",
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
            "@1 fps 60",
            "fps",
            "fps 60 1",
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
        assert_eq!(encoder_line("fps 60", true), Some("fps 60"));
        for qemu_bad in ["@1 keyframe", "track add 1"] {
            assert_eq!(encoder_line(qemu_bad, true), None, "{qemu_bad}");
        }
    }

    #[test]
    fn an_ended_session_holds_no_broker_connection() {
        let (ours, broker) = UnixStream::pair().unwrap();
        let out: Out = Arc::new(Mutex::new(Box::new(std::io::sink())));
        let link = BrokerLink::start(ours, Arc::new(AtomicBool::new(true)), out).unwrap();
        link.send(b"{\"request_id\":\"cursor\",\"method\":\"cursor_watch\"}\n")
            .unwrap();
        drop(link);
        // The audio reader still waits on the connection; only its shutdown
        // lets the broker see the end and free the slot.
        broker
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut seen = String::new();
        (&broker)
            .read_to_string(&mut seen)
            .expect("the broker sees the connection end");
        assert_eq!(
            seen,
            "{\"request_id\":\"cursor\",\"method\":\"cursor_watch\"}\n"
        );
    }

    #[test]
    fn an_encoder_channel_reaches_only_its_own_child() {
        // `sh -c SCRIPT sh FLAG FD`: the script checks the descriptor in `$2`.
        let probe = || {
            let mut sh = Command::new("/bin/sh");
            sh.args(["-c", "[ -e /dev/fd/$2 ]", "sh"]);
            sh
        };
        let mut encoder = probe();
        let (_ours, theirs) = encoder_channel(&mut encoder, "--rate-control-fd").unwrap();
        let fd = theirs.as_raw_fd();
        // Close-on-exec here until the encoder's own exec, so a spawn from
        // any other daemon thread goes without it.
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let other = probe().args(["--other", &fd.to_string()]).status().unwrap();
        assert!(
            !other.success(),
            "an unrelated child inherited the encoder's end"
        );
        assert!(
            encoder.status().unwrap().success(),
            "the encoder lacks its end"
        );
    }

    #[test]
    fn a_reconnecting_viewer_waits_for_the_previous_encoder() {
        let vm = format!("gate-{}", random_hex(4));
        let first = EncoderGate::enter(&vm, Duration::ZERO).unwrap();
        assert!(EncoderGate::enter(&vm, Duration::from_millis(50)).is_none());
        let waiter = {
            let vm = vm.clone();
            thread::spawn(move || EncoderGate::enter(&vm, Duration::from_secs(5)).is_some())
        };
        thread::sleep(Duration::from_millis(100));
        drop(first);
        assert!(waiter.join().unwrap());
        assert!(EncoderGate::enter(&format!("{vm}-other"), Duration::ZERO).is_some());
    }
}
