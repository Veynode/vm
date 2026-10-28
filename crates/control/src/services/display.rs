//! A running machine's screen endpoint, in Veynode core's format
//! (docs/api.md, `machine.display`).
//!
//! `machine.display` answers `{format:"stream", address, port, token,
//! epoch}`: a loopback TCP listener, run by threads of the daemon, that
//! speaks core's screen-stream protocol (the header of core's
//! `apps/macos-runtime/native/screen-stream.swift`). The viewer sends the
//! token and a newline, then input messages (kind u8 and a fixed-size
//! payload); the endpoint sends kind u8, length u32 BE, payload:
//! capabilities (11) first, then video (1: width, height u16 BE, flags u8
//! with 1 = key frame, Annex B) and codec changes (12). Each connection runs
//! `vm-control desktop-stream` (`crate::desktop_stream`: QEMU's SPICE console encoder, or the Apple
//! Virtualization guest agent / Screen Sharing encoder) and translates its
//! records; the newest connection replaces the one before it. A new epoch
//! replaces the endpoint and cuts every connection of the old one; it ends
//! when the machine stops.
use super::{progress, random_hex, refuse, Machine};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

/// Serves one authorized viewer of an endpoint until it disconnects or is cut.
pub(super) type Viewer = Arc<dyn Fn(&str, TcpStream) -> Result<()> + Send + Sync>;

/// One machine's endpoint of one stream.
struct Endpoint {
    epoch: u64,
    port: u16,
    token: String,
    /// What the answer carries beyond the address (the format, a sound's layout).
    fields: Value,
    stopped: Arc<AtomicBool>,
    /// The viewer being served, cut when the endpoint is replaced.
    active: Arc<Mutex<Option<TcpStream>>>,
}

impl Endpoint {
    fn answer(&self) -> Value {
        let mut answer = self.fields.clone();
        answer["address"] = json!("127.0.0.1");
        answer["port"] = json!(self.port);
        answer["token"] = json!(self.token);
        answer["epoch"] = json!(self.epoch);
        answer
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(viewer) = self.active.lock().unwrap().take() {
            let _ = viewer.shutdown(Shutdown::Both);
        }
    }
}

/// Endpoints by machine name and stream (`display`, `sound`, `microphone`).
type Endpoints = HashMap<(String, &'static str), Endpoint>;
static ENDPOINTS: Mutex<Option<Endpoints>> = Mutex::new(None);

/// `machine.display`: the endpoint of `epoch`, started if it is new.
pub fn display(machine: &Machine, epoch: u64) -> Result<Value> {
    // Held by the viewer whose encoder owns the desktop.
    let desktop = Arc::new(Mutex::new(()));
    endpoint(
        machine,
        "display",
        epoch,
        json!({"format":"stream"}),
        Arc::new(move |name, stream| serve(name, stream, &desktop)),
    )
}

/// The machine's endpoint of `stream` for `epoch`: the running one, or a new
/// one (new port and token) that cuts the one before it. Each authorized
/// viewer goes to `viewer`, the newest cutting the one before it.
pub(super) fn endpoint(
    machine: &Machine,
    stream: &'static str,
    epoch: u64,
    fields: Value,
    viewer: Viewer,
) -> Result<Value> {
    if !super::running(&super::state(&machine.name)) {
        refuse!("state", "{} is not running", machine.name)
    }
    let key = (machine.name.clone(), stream);
    let mut endpoints = ENDPOINTS.lock().unwrap();
    let endpoints = endpoints.get_or_insert_with(HashMap::new);
    if let Some(current) = endpoints.get(&key) {
        if current.epoch == epoch && !current.stopped.load(Ordering::SeqCst) {
            return Ok(current.answer());
        }
        current.stop();
    }
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let endpoint = Endpoint {
        epoch,
        port: listener.local_addr()?.port(),
        token: random_hex(24),
        fields,
        stopped: Arc::new(AtomicBool::new(false)),
        active: Arc::new(Mutex::new(None)),
    };
    let answer = endpoint.answer();
    let (name, token, stopped, active) = (
        machine.name.clone(),
        endpoint.token.clone(),
        endpoint.stopped.clone(),
        endpoint.active.clone(),
    );
    thread::spawn(move || listen(&name, stream, listener, &token, &stopped, &active, &viewer));
    endpoints.insert(key, endpoint);
    Ok(answer)
}

/// Stops the machine's endpoints, if any run (stop, rm, rename).
pub fn stop(name: &str) {
    if let Some(endpoints) = ENDPOINTS.lock().unwrap().as_mut() {
        endpoints.retain(|(machine, _), endpoint| {
            if machine == name {
                endpoint.stop();
            }
            machine != name
        });
    }
}

/// Bytes of each viewer input message, kind included (screen-stream's `inputSizes`).
pub fn input_size(kind: u8) -> Option<usize> {
    Some(match kind {
        1 | 3 => 6,
        2 => 4,
        4 => 1,
        5 | 6 | 8 | 12 | 13 | 15 | 19 => 2,
        7 | 11 | 14 | 16 | 17 | 18 => 5,
        20 => 9,
        21 => 10,
        22 | 23 => 7,
        _ => return None,
    })
}

/// macOS virtual key code → `KeyboardEvent.code` (`vm_control::desktop_input`).
pub fn mac_key(code: u16) -> Option<&'static str> {
    Some(match code {
        0 => "KeyA",
        1 => "KeyS",
        2 => "KeyD",
        3 => "KeyF",
        4 => "KeyH",
        5 => "KeyG",
        6 => "KeyZ",
        7 => "KeyX",
        8 => "KeyC",
        9 => "KeyV",
        10 => "IntlBackslash",
        11 => "KeyB",
        12 => "KeyQ",
        13 => "KeyW",
        14 => "KeyE",
        15 => "KeyR",
        16 => "KeyY",
        17 => "KeyT",
        18 => "Digit1",
        19 => "Digit2",
        20 => "Digit3",
        21 => "Digit4",
        22 => "Digit6",
        23 => "Digit5",
        24 => "Equal",
        25 => "Digit9",
        26 => "Digit7",
        27 => "Minus",
        28 => "Digit8",
        29 => "Digit0",
        30 => "BracketRight",
        31 => "KeyO",
        32 => "KeyU",
        33 => "BracketLeft",
        34 => "KeyI",
        35 => "KeyP",
        36 => "Enter",
        37 => "KeyL",
        38 => "KeyJ",
        39 => "Quote",
        40 => "KeyK",
        41 => "Semicolon",
        42 => "Backslash",
        43 => "Comma",
        44 => "Slash",
        45 => "KeyN",
        46 => "KeyM",
        47 => "Period",
        48 => "Tab",
        49 => "Space",
        50 => "Backquote",
        51 => "Backspace",
        53 => "Escape",
        54 => "MetaRight",
        55 => "MetaLeft",
        56 => "ShiftLeft",
        57 => "CapsLock",
        58 => "AltLeft",
        59 => "ControlLeft",
        60 => "ShiftRight",
        61 => "AltRight",
        62 => "ControlRight",
        65 => "NumpadDecimal",
        67 => "NumpadMultiply",
        69 => "NumpadAdd",
        71 => "NumLock",
        75 => "NumpadDivide",
        76 => "NumpadEnter",
        78 => "NumpadSubtract",
        82 => "Numpad0",
        83 => "Numpad1",
        84 => "Numpad2",
        85 => "Numpad3",
        86 => "Numpad4",
        87 => "Numpad5",
        88 => "Numpad6",
        89 => "Numpad7",
        91 => "Numpad8",
        92 => "Numpad9",
        96 => "F5",
        97 => "F6",
        98 => "F7",
        99 => "F3",
        100 => "F8",
        101 => "F9",
        103 => "F11",
        105 => "PrintScreen",
        107 => "ScrollLock",
        109 => "F10",
        110 => "ContextMenu",
        111 => "F12",
        113 => "Pause",
        114 => "Insert",
        115 => "Home",
        116 => "PageUp",
        117 => "Delete",
        118 => "F4",
        119 => "End",
        120 => "F2",
        121 => "PageDown",
        122 => "F1",
        123 => "ArrowLeft",
        124 => "ArrowRight",
        125 => "ArrowDown",
        126 => "ArrowUp",
        _ => return None,
    })
}

/// The physical key, and whether Shift is needed, that types `c` on a US layout.
pub fn us_key(c: char) -> Option<(String, bool)> {
    const SHIFTED: &str = ")!@#$%^&*(";
    let named = |code: &str, shift| Some((code.to_owned(), shift));
    match c {
        'a'..='z' => Some((format!("Key{}", c.to_ascii_uppercase()), false)),
        'A'..='Z' => Some((format!("Key{c}"), true)),
        '0'..='9' => Some((format!("Digit{c}"), false)),
        ' ' => named("Space", false),
        '\n' | '\r' => named("Enter", false),
        '\t' => named("Tab", false),
        _ => {
            if let Some(i) = SHIFTED.find(c) {
                return Some((format!("Digit{i}"), true));
            }
            let (code, shift) = match c {
                '-' => ("Minus", false),
                '_' => ("Minus", true),
                '=' => ("Equal", false),
                '+' => ("Equal", true),
                '[' => ("BracketLeft", false),
                '{' => ("BracketLeft", true),
                ']' => ("BracketRight", false),
                '}' => ("BracketRight", true),
                '\\' => ("Backslash", false),
                '|' => ("Backslash", true),
                ';' => ("Semicolon", false),
                ':' => ("Semicolon", true),
                '\'' => ("Quote", false),
                '"' => ("Quote", true),
                '`' => ("Backquote", false),
                '~' => ("Backquote", true),
                ',' => ("Comma", false),
                '<' => ("Comma", true),
                '.' => ("Period", false),
                '>' => ("Period", true),
                '/' => ("Slash", false),
                '?' => ("Slash", true),
                _ => return None,
            };
            named(code, shift)
        }
    }
}

/// Reads Exp-Golomb codes from an RBSP (emulation prevention removed).
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.at / 8)?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Some(bit as u32)
    }
    fn bits(&mut self, n: usize) -> Option<u32> {
        (0..n).try_fold(0u32, |v, _| Some(v << 1 | self.bit()?))
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.at += n;
        (self.at <= self.data.len() * 8).then_some(())
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
    fn se(&mut self) -> Option<i32> {
        let v = self.ue()? as i64;
        Some(if v % 2 == 1 {
            ((v + 1) / 2) as i32
        } else {
            -(v / 2) as i32
        })
    }
}

fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// The NAL units of an Annex B access unit.
fn nal_units(au: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| {
            let mut end = starts.get(k + 1).map_or(au.len(), |&n| n - 3);
            while end > s && au[end - 1] == 0 {
                end -= 1;
            }
            &au[s..end]
        })
        .collect()
}

fn h264_size(sps: &[u8]) -> Option<(u32, u32)> {
    let data = rbsp(&sps[1..]);
    let mut b = Bits { data: &data, at: 0 };
    let profile = b.bits(8)?;
    b.skip(16)?;
    b.ue()?;
    let mut chroma = 1;
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        chroma = b.ue()?;
        if chroma == 3 {
            b.skip(1)?;
        }
        b.ue()?;
        b.ue()?;
        b.skip(1)?;
        if b.bit()? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if b.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + b.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    b.ue()?;
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.skip(1)?;
            b.se()?;
            b.se()?;
            for _ in 0..b.ue()? {
                b.se()?;
            }
        }
        _ => {}
    }
    b.ue()?;
    b.skip(1)?;
    let width_mbs = b.ue()? + 1;
    let height_units = b.ue()? + 1;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 {
        b.skip(1)?;
    }
    b.skip(1)?;
    let mut width = width_mbs * 16;
    let mut height = (2 - frame_mbs_only) * height_units * 16;
    if b.bit()? == 1 {
        let (l, r, t, bottom) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        let (cx, cy) = match chroma {
            0 => (1, 2 - frame_mbs_only),
            1 => (2, 2 * (2 - frame_mbs_only)),
            2 => (2, 2 - frame_mbs_only),
            _ => (1, 2 - frame_mbs_only),
        };
        width = width.checked_sub(cx * (l + r))?;
        height = height.checked_sub(cy * (t + bottom))?;
    }
    Some((width, height))
}

fn hevc_size(sps: &[u8]) -> Option<(u32, u32)> {
    let data = rbsp(sps.get(2..)?);
    let mut b = Bits { data: &data, at: 0 };
    b.skip(4)?;
    let sub_layers = b.bits(3)? as usize;
    b.skip(1)?;
    b.skip(96)?;
    let mut present = Vec::new();
    for _ in 0..sub_layers {
        present.push((b.bit()?, b.bit()?));
    }
    if sub_layers > 0 {
        b.skip(2 * (8 - sub_layers))?;
    }
    for (profile, level) in present {
        b.skip(if profile == 1 { 88 } else { 0 } + if level == 1 { 8 } else { 0 })?;
    }
    b.ue()?;
    let chroma = b.ue()?;
    if chroma == 3 {
        b.skip(1)?;
    }
    let (mut width, mut height) = (b.ue()?, b.ue()?);
    if b.bit()? == 1 {
        let (l, r, t, bottom) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        let (cx, cy) = match chroma {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        width = width.checked_sub(cx * (l + r))?;
        height = height.checked_sub(cy * (t + bottom))?;
    }
    Some((width, height))
}

/// The picture size an access unit's sequence parameter set declares.
pub fn picture_size(au: &[u8], hevc: bool) -> Option<(u32, u32)> {
    nal_units(au)
        .into_iter()
        .find_map(|nal| match (hevc, nal.first()) {
            (false, Some(h)) if h & 0x1f == 7 => h264_size(nal),
            (true, Some(h)) if (h >> 1) & 0x3f == 33 => hevc_size(nal),
            _ => None,
        })
}

/// One endpoint message: kind, length (u32 BE), payload.
pub fn message(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// What the endpoint does with the viewer's input.
#[derive(Debug, PartialEq)]
pub enum Action {
    Events(Vec<Value>),
    KeyFrame,
    Bitrate(u64),
    Codec(bool),
    None,
}

/// Translates viewer input messages for the desktop relay.
#[derive(Default)]
pub struct Input {
    buttons: u8,
    /// The picture's size, for pointer positions.
    pub size: Option<(u32, u32)>,
}

impl Input {
    pub fn act(&mut self, m: &[u8]) -> Action {
        let be16 = |at: usize| u16::from_be_bytes([m[at], m[at + 1]]);
        match m[0] {
            1 => {
                let Some((w, h)) = self.size else {
                    return Action::None;
                };
                let (x, y, mask) = (be16(1) as u32, be16(3) as u32, m[5]);
                let mut events =
                    vec![json!({"t":"move","x":x.min(w - 1),"y":y.min(h - 1),"w":w,"h":h})];
                for (bit, button) in [(1u8, "left"), (2, "middle"), (4, "right")] {
                    if (mask ^ self.buttons) & bit != 0 {
                        events.push(json!({"t":"button","button":button,"down":mask & bit != 0}));
                    }
                }
                let pressed = mask & !self.buttons;
                for (bit, dx, dy) in [(8u8, 0, -1), (16, 0, 1), (32, -1, 0), (64, 1, 0)] {
                    if pressed & bit != 0 {
                        events.push(json!({"t":"wheel","dx":dx,"dy":dy}));
                    }
                }
                self.buttons = mask;
                Action::Events(events)
            }
            2 => match mac_key(be16(1)) {
                Some(code) => Action::Events(vec![json!({"t":"key","code":code,"down":m[3] != 0})]),
                None => Action::None,
            },
            3 => {
                let scalar = u32::from_be_bytes([m[1], m[2], m[3], m[4]]);
                let down = m[5] != 0;
                match char::from_u32(scalar).and_then(us_key) {
                    Some((code, shift)) => {
                        let key = json!({"t":"key","code":code,"down":down});
                        let shift_key = json!({"t":"key","code":"ShiftLeft","down":down});
                        Action::Events(match (shift, down) {
                            (false, _) => vec![key],
                            (true, true) => vec![shift_key, key],
                            (true, false) => vec![key, shift_key],
                        })
                    }
                    None => Action::None,
                }
            }
            4 => Action::KeyFrame,
            7 => {
                let kbps = u32::from_be_bytes([m[1], m[2], m[3], m[4]]) as u64;
                Action::Bitrate((kbps * 1000).clamp(250_000, 40_000_000))
            }
            19 => Action::Codec(m[1] == 1),
            _ => Action::None,
        }
    }
}

type Socket = Arc<Mutex<TcpStream>>;

/// One running `vm-control desktop-stream` and the codec it encodes.
struct Pipeline {
    child: Child,
    stdin: ChildStdin,
}

impl Pipeline {
    fn start(
        name: &str,
        hevc: bool,
        socket: Socket,
        size: Arc<Mutex<Option<(u32, u32)>>>,
        sent_codec: Arc<Mutex<bool>>,
    ) -> Result<Self> {
        let mut child = Command::new(std::env::current_exe()?)
            .args([
                "desktop-stream",
                "--codec",
                if hevc { "hevc" } else { "h264" },
                name,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting the desktop encoder")?;
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        thread::spawn(move || {
            if let Err(error) = relay(stdout, &socket, hevc, &size, &sent_codec) {
                progress(&format!("display relay: {error:#}"));
            }
        });
        Ok(Self { child, stdin })
    }

    fn send(&mut self, line: &Value) -> bool {
        let mut bytes = serde_json::to_vec(line).unwrap_or_default();
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).is_ok()
    }

    /// Ends the encoder by closing its stdin (it then releases held input
    /// and the Apple Virtualization bridge), killing it after 3 s.
    fn stop(self) {
        let Pipeline { mut child, stdin } = self;
        drop(stdin);
        for _ in 0..150 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Encoder records (`u32 length, u8 kind, payload`) to endpoint messages.
fn relay(
    mut from: impl Read,
    socket: &Socket,
    hevc: bool,
    size: &Mutex<Option<(u32, u32)>>,
    sent_codec: &Mutex<bool>,
) -> Result<()> {
    let mut header = [0u8; 5];
    let mut current: Option<(u32, u32)> = None;
    loop {
        from.read_exact(&mut header)?;
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        if length == 0 || length > 8 << 20 {
            bail!("encoder record of {length} bytes")
        }
        let mut payload = vec![0u8; length - 1];
        from.read_exact(&mut payload)?;
        match header[4] {
            1 => {
                let event: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
                if event["event"] == "error" {
                    bail!("{}", event["message"].as_str().unwrap_or("encoder error"))
                }
            }
            2 if payload.len() > 9 => {
                let key = payload[8] != 0;
                let au = &payload[9..];
                if key {
                    current = picture_size(au, hevc).or(current);
                }
                let Some((w, h)) = current else {
                    continue;
                };
                *size.lock().unwrap() = Some((w, h));
                let mut body = Vec::with_capacity(5 + au.len());
                body.extend_from_slice(&(w as u16).to_be_bytes());
                body.extend_from_slice(&(h as u16).to_be_bytes());
                body.push(key as u8);
                body.extend_from_slice(au);
                let mut out = socket.lock().unwrap();
                let mut codec = sent_codec.lock().unwrap();
                if *codec != hevc {
                    out.write_all(&message(12, &[hevc as u8]))?;
                    *codec = hevc;
                }
                out.write_all(&message(1, &body))?;
            }
            _ => {}
        }
    }
}

/// Serves one authorized viewer until it disconnects or is replaced.
fn serve(name: &str, stream: TcpStream, desktop: &Mutex<()>) -> Result<()> {
    let socket: Socket = Arc::new(Mutex::new(stream.try_clone()?));
    // The inputs beyond the oldest viewers' that this endpoint takes.
    socket.lock().unwrap().write_all(&message(11, &[19]))?;
    let size = Arc::new(Mutex::new(None));
    let sent_codec = Arc::new(Mutex::new(false));
    let mut hevc = false;
    // A replaced viewer's encoder holds the desktop until it has stopped.
    let _desktop = desktop.lock().unwrap_or_else(|e| e.into_inner());
    let mut pipeline = Some(Pipeline::start(
        name,
        hevc,
        socket.clone(),
        size.clone(),
        sent_codec.clone(),
    )?);
    let mut input = Input::default();
    let mut reader = BufReader::new(stream);
    let result = (|| -> Result<()> {
        loop {
            let mut kind = [0u8; 1];
            reader.read_exact(&mut kind)?;
            let total =
                input_size(kind[0]).with_context(|| format!("unknown input {}", kind[0]))?;
            let mut m = vec![0u8; total];
            m[0] = kind[0];
            reader.read_exact(&mut m[1..])?;
            input.size = *size.lock().unwrap();
            let mut send = |line: Value| pipeline.as_mut().is_some_and(|p| p.send(&line));
            let ok = match input.act(&m) {
                Action::Events(events) => send(json!({"events":events})),
                Action::KeyFrame => send(json!({"keyframe":true})),
                Action::Bitrate(bits) => send(json!({"encoder":format!("bitrate {bits}")})),
                Action::Codec(wanted) if wanted != hevc => {
                    hevc = wanted;
                    // One encoder per desktop: the old one lets go first.
                    if let Some(old) = pipeline.take() {
                        old.stop();
                    }
                    pipeline = Some(Pipeline::start(
                        name,
                        hevc,
                        socket.clone(),
                        size.clone(),
                        sent_codec.clone(),
                    )?);
                    true
                }
                Action::Codec(_) | Action::None => true,
            };
            if !ok {
                bail!("the desktop encoder ended")
            }
        }
    })();
    if let Some(pipeline) = pipeline {
        pipeline.stop();
    }
    let _ = socket.lock().unwrap().shutdown(Shutdown::Both);
    result
}

/// Accepts viewers until the endpoint is stopped or the machine stops.
fn listen(
    name: &str,
    stream_name: &'static str,
    listener: TcpListener,
    token: &str,
    stopped: &AtomicBool,
    active: &Arc<Mutex<Option<TcpStream>>>,
    viewer: &Viewer,
) {
    let mut checked = std::time::Instant::now();
    while !stopped.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let (name, token, active, viewer) = (
                    name.to_owned(),
                    token.to_owned(),
                    active.clone(),
                    viewer.clone(),
                );
                thread::spawn(move || {
                    let _ = stream.set_nonblocking(false);
                    if !authorized(&stream, &token) {
                        let _ = stream.shutdown(Shutdown::Both);
                        return;
                    }
                    let _ = stream.set_read_timeout(None);
                    let _ = stream.set_nodelay(true);
                    // The newest viewer wins: the one before it is cut.
                    if let Some(previous) =
                        active.lock().unwrap().replace(stream.try_clone().unwrap())
                    {
                        let _ = previous.shutdown(Shutdown::Both);
                    }
                    if let Err(error) = viewer(&name, stream) {
                        progress(&format!("{stream_name} viewer of {name} ended: {error:#}"));
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50))
            }
            Err(_) => thread::sleep(Duration::from_millis(200)),
        }
        if checked.elapsed() > Duration::from_secs(2) {
            checked = std::time::Instant::now();
            if !super::running(&super::state(name)) {
                let mut endpoints = ENDPOINTS.lock().unwrap();
                if let Some(map) = endpoints.as_mut() {
                    let key = (name.to_owned(), stream_name);
                    if map
                        .get(&key)
                        .is_some_and(|e| std::ptr::eq(&*e.stopped, stopped))
                    {
                        map.remove(&key);
                    }
                }
                return;
            }
        }
    }
}

fn authorized(stream: &TcpStream, token: &str) -> bool {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    let mut reader = stream;
    while line.len() <= 128 {
        match reader.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => {
                let given = String::from_utf8_lossy(&line);
                let given = given.trim_end_matches('\r');
                // Constant time over the token's length.
                return given.len() == token.len()
                    && given
                        .bytes()
                        .zip(token.bytes())
                        .fold(0, |d, (a, b)| d | (a ^ b))
                        == 0;
            }
            Ok(1) => line.push(byte[0]),
            _ => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes the Exp-Golomb bitstreams the parsers read.
    #[derive(Default)]
    struct Writer(Vec<bool>);
    impl Writer {
        fn bits(&mut self, value: u64, n: usize) -> &mut Self {
            for i in (0..n).rev() {
                self.0.push(value >> i & 1 == 1);
            }
            self
        }
        fn ue(&mut self, value: u32) -> &mut Self {
            let v = value as u64 + 1;
            let n = 64 - v.leading_zeros() as usize;
            self.bits(0, n - 1).bits(v, n)
        }
        /// The NAL unit after a start code, with emulation prevention bytes.
        fn nal(&mut self, header: &[u8]) -> Vec<u8> {
            self.bits(1, 1);
            while !self.0.len().is_multiple_of(8) {
                self.0.push(false);
            }
            let bytes: Vec<u8> = self
                .0
                .chunks(8)
                .map(|c| c.iter().fold(0, |b, &bit| b << 1 | bit as u8))
                .collect();
            let mut out = vec![0, 0, 0, 1];
            out.extend_from_slice(header);
            let mut zeros = 0;
            for b in bytes {
                if zeros >= 2 && b <= 3 {
                    out.push(3);
                    zeros = 0;
                }
                zeros = if b == 0 { zeros + 1 } else { 0 };
                out.push(b);
            }
            out
        }
    }

    #[test]
    fn h264_sequence_parameter_sets_give_the_picture_size() {
        // High profile, 4:2:0, 1920x1088 coded, cropped to 1080 (4 x 2 rows).
        let mut w = Writer::default();
        w.bits(100, 8)
            .bits(0, 8)
            .bits(40, 8)
            .ue(0)
            .ue(1)
            .ue(0)
            .ue(0)
            .bits(0, 1)
            .bits(0, 1);
        w.ue(4)
            .ue(0)
            .ue(2)
            .ue(4)
            .bits(0, 1)
            .ue(119)
            .ue(67)
            .bits(1, 1)
            .bits(1, 1);
        w.bits(1, 1).ue(0).ue(0).ue(0).ue(4).bits(0, 1);
        let mut au = w.nal(&[0x67]);
        au.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x80]);
        assert_eq!(picture_size(&au, false), Some((1920, 1080)));
        // Baseline with picture order type 1 and no cropping.
        let mut w = Writer::default();
        w.bits(66, 8)
            .bits(0, 8)
            .bits(30, 8)
            .ue(0)
            .ue(0)
            .ue(1)
            .bits(1, 1)
            .ue(0)
            .ue(0)
            .ue(2)
            .ue(1)
            .ue(1);
        w.ue(1)
            .bits(0, 1)
            .ue(79)
            .ue(44)
            .bits(1, 1)
            .bits(1, 1)
            .bits(0, 1)
            .bits(0, 1);
        assert_eq!(picture_size(&w.nal(&[0x67]), false), Some((1280, 720)));
        assert_eq!(picture_size(&[0, 0, 1, 0x65, 1, 2], false), None);
    }

    #[test]
    fn hevc_sequence_parameter_sets_give_the_picture_size() {
        // One sub-layer, 4:2:0, 1920x1088 cropped by 4 x 2 rows.
        let mut w = Writer::default();
        w.bits(0, 4)
            .bits(0, 3)
            .bits(1, 1)
            .bits(0, 64)
            .bits(0, 32)
            .ue(0)
            .ue(1)
            .ue(1920)
            .ue(1088);
        w.bits(1, 1).ue(0).ue(0).ue(0).ue(4).ue(0);
        assert_eq!(
            picture_size(&w.nal(&[0x42, 0x01]), true),
            Some((1920, 1080))
        );
        // Two sub-layers with profile and level present.
        let mut w = Writer::default();
        w.bits(0, 4)
            .bits(1, 3)
            .bits(1, 1)
            .bits(0, 64)
            .bits(0, 32)
            .bits(1, 1)
            .bits(1, 1)
            .bits(0, 14);
        w.bits(0, 64)
            .bits(0, 32)
            .ue(0)
            .ue(1)
            .ue(1280)
            .ue(720)
            .bits(0, 1);
        assert_eq!(picture_size(&w.nal(&[0x42, 0x01]), true), Some((1280, 720)));
    }

    #[test]
    fn viewer_input_becomes_desktop_events() {
        let mut input = Input::default();
        assert_eq!(
            input.act(&[1, 0, 10, 0, 20, 1]),
            Action::None,
            "no picture yet"
        );
        input.size = Some((1280, 720));
        let Action::Events(e) = input.act(&[1, 0, 10, 0, 20, 1]) else {
            panic!()
        };
        assert_eq!(e[0], json!({"t":"move","x":10,"y":20,"w":1280,"h":720}));
        assert_eq!(e[1], json!({"t":"button","button":"left","down":true}));
        let Action::Events(e) = input.act(&[1, 0xff, 0xff, 0, 20, 8]) else {
            panic!()
        };
        assert_eq!(e[0]["x"], 1279);
        assert!(e.contains(&json!({"t":"button","button":"left","down":false})));
        assert!(e.contains(&json!({"t":"wheel","dx":0,"dy":-1})));
        // A held wheel bit is one tick, not one per message.
        let Action::Events(e) = input.act(&[1, 0, 0, 0, 0, 8]) else {
            panic!()
        };
        assert_eq!(e.len(), 1);
        assert_eq!(
            input.act(&[2, 0, 0, 1]),
            Action::Events(vec![json!({"t":"key","code":"KeyA","down":true})])
        );
        assert_eq!(input.act(&[2, 0, 200, 1]), Action::None);
        let Action::Events(e) = input.act(&[3, 0, 0, 0, b'?', 1]) else {
            panic!()
        };
        assert_eq!(
            e,
            vec![
                json!({"t":"key","code":"ShiftLeft","down":true}),
                json!({"t":"key","code":"Slash","down":true})
            ]
        );
        assert_eq!(input.act(&[4]), Action::KeyFrame);
        assert_eq!(
            input.act(&[7, 0, 0, 0x1f, 0x40]),
            Action::Bitrate(8_000_000)
        );
        assert_eq!(input.act(&[7, 0, 0, 0, 1]), Action::Bitrate(250_000));
        assert_eq!(input.act(&[19, 1]), Action::Codec(true));
        assert_eq!(input.act(&[17, 0, 1, 0, 1]), Action::None);
    }

    #[test]
    fn input_sizes_and_framing_match_screen_stream() {
        // screen-stream.swift's inputSizes.
        let table = [
            (1, 6),
            (2, 4),
            (3, 6),
            (4, 1),
            (5, 2),
            (6, 2),
            (7, 5),
            (8, 2),
            (11, 5),
            (12, 2),
            (13, 2),
            (14, 5),
            (15, 2),
            (16, 5),
            (17, 5),
            (18, 5),
            (19, 2),
            (20, 9),
            (21, 10),
            (22, 7),
            (23, 7),
        ];
        for (kind, size) in table {
            assert_eq!(input_size(kind), Some(size), "{kind}");
        }
        assert_eq!(input_size(9), None);
        assert_eq!(message(12, &[1]), vec![12, 0, 0, 0, 1, 1]);
        for c in (' '..='~').chain(['\n', '\t']) {
            assert!(us_key(c).is_some(), "{c:?}");
        }
        assert_eq!(us_key('A'), Some(("KeyA".into(), true)));
        assert_eq!(us_key(')'), Some(("Digit0".into(), true)));
        for code in 0..=126u16 {
            if let Some(name) = mac_key(code) {
                assert!(
                    crate::desktop_input::parse(&json!([{"t":"key","code":name,"down":true}]))
                        .is_ok(),
                    "{name}"
                );
            }
        }
    }
}
