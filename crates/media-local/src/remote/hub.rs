//! One encoder per VM and codec, shared by every remote viewer of it.
//!
//! The Apple Virtualization guest bridge and the QEMU console broker each
//! serve one encoder, so viewers cannot each start their own. The hub opens
//! the daemon's `desktop.stream` (API 1) once and fans its access units
//! out to per-viewer [`Pacer`]s. A viewer that zooms on a phone asks for a
//! crop rectangle; viewers asking for the same rectangle share an encoder
//! track, at most `tracks_per_vm` exist, and a viewer that finds none free
//! (or a QEMU VM, which has one track) keeps the full screen and zooms in
//! the browser.
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, Notify};
use vm_remote_proto::abr;
use vm_remote_proto::pacer::{Frame, KeyframeLimiter, Offer, Pacer};
use vm_remote_proto::record::{VideoHeader, View};

/// The encoders' track ids are 0..8; track 0 is the full screen.
pub const MAX_TRACKS: usize = 8;
/// After the last viewer leaves, the encoder keeps running this long so a
/// reload or reconnect does not pay for a new session and a keyframe.
pub const LINGER: Duration = Duration::from_secs(10);
const MAX_AU: usize = 4 * 1024 * 1024;
/// Offsets inside a VIDEO_V2 header patched when the frame is sent.
pub const HOST_QUEUE_AT: usize = 21;
pub const INPUT_ACK_AT: usize = 25;

#[derive(Debug)]
pub struct EncodedFrame {
    pub track: u8,
    pub pts_us: u64,
    pub keyframe: bool,
    pub annex_b: Vec<u8>,
}

#[derive(Debug)]
pub enum SourceEvent {
    Frame(EncodedFrame),
    /// A JSON event from the relay: `ready`, `encoder` (with the encoder's
    /// `view`/`rate` diagnostics in `detail`), `input_error`, `error`.
    Event(Value),
    Ended(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum SourceCommand {
    /// `desktop_input` events, browser wire shape.
    Input(Value),
    /// One encoder stdin line (`@T keyframe`, `track add T`, ...).
    Encoder(String),
}

pub struct SourceHandle {
    pub events: mpsc::Receiver<SourceEvent>,
    pub commands: mpsc::UnboundedSender<SourceCommand>,
}

/// Starts the encoder for one VM; the process ends when `commands` closes.
pub trait Source: Send + Sync {
    fn start(&self, vm: &str, codec: &str) -> Result<SourceHandle>;
}

/// The production source: the daemon's `desktop.stream {name, codec,
/// tracks: true}` (docs/api.md). Its `stdout` frames are the relay records
/// [`read_records`] parses; commands go out as `control` messages.
pub struct DesktopStream {
    pub client: veynodevm_api::Client,
}

impl Source for DesktopStream {
    fn start(&self, vm: &str, codec: &str) -> Result<SourceHandle> {
        let client = self.client.clone();
        let args = json!({ "name": vm, "codec": codec, "tracks": true });
        // A blocking API client on threads; the records cross to the async
        // parser through a socket pair, so they keep their order and the
        // stream's end reaches the parser after its last record.
        let (writer, reader) = std::os::unix::net::UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        let mut reader = tokio::net::UnixStream::from_std(reader)?;
        let ended = Arc::new(Mutex::new(None::<String>));
        let (sender_tx, sender_rx) = std::sync::mpsc::channel();
        let (event_tx, events) = mpsc::channel(64);
        let (commands, mut command_rx) = mpsc::unbounded_channel::<SourceCommand>();
        let end = ended.clone();
        std::thread::Builder::new()
            .name("desktop-stream".into())
            .spawn(move || {
                let message = relay(&client, args, writer, &sender_tx);
                *end.lock().unwrap() = Some(message);
            })?;
        std::thread::Builder::new()
            .name("desktop-commands".into())
            .spawn(move || {
                let Ok(sender) = sender_rx.recv() else {
                    return;
                };
                let sender: veynodevm_api::StreamSender = sender;
                while let Some(c) = command_rx.blocking_recv() {
                    let message = match c {
                        SourceCommand::Input(events) => json!({ "events": events }),
                        SourceCommand::Encoder(line) => json!({ "encoder": line }),
                    };
                    if sender.control(&message).is_err() {
                        break;
                    }
                }
                // Leaving the stream stops the relay, which releases held
                // input and the guest bridge.
                sender.cancel();
            })?;
        tokio::spawn(async move {
            let parsed = read_records(&mut reader, &event_tx).await;
            let ended = ended.lock().unwrap().take();
            let message = match parsed {
                Err(e) => format!("{e:#}"),
                Ok(()) => ended.unwrap_or_else(|| "desktop relay ended".to_string()),
            };
            let _ = event_tx.send(SourceEvent::Ended(message)).await;
        });
        Ok(SourceHandle { events, commands })
    }
}

/// `desktop.stream` to its end, its `stdout` copied to `to`; what ended it.
fn relay(
    client: &veynodevm_api::Client,
    args: Value,
    mut to: std::os::unix::net::UnixStream,
    sender: &std::sync::mpsc::Sender<veynodevm_api::StreamSender>,
) -> String {
    use std::io::Write;
    let mut stream = match client.stream("desktop.stream", args) {
        Ok(stream) => stream,
        Err(e) => return format!("desktop.stream: {e:#}"),
    };
    let _ = sender.send(stream.sender());
    loop {
        match stream.next_event() {
            Ok(veynodevm_api::Event::Stdout(data)) => {
                if to.write_all(&data).is_err() {
                    return "desktop relay ended".to_string();
                }
            }
            Ok(veynodevm_api::Event::End(Ok(_))) => return "desktop relay ended".to_string(),
            Ok(veynodevm_api::Event::End(Err(e))) => return format!("desktop.stream: {e}"),
            Ok(_) => {}
            // Cancelled when the hub left, or the daemon went away.
            Err(e) => return format!("desktop relay ended: {e:#}"),
        }
    }
}

/// The relay's stdout: `u32 len | u8 kind | payload`; kind 1 JSON, kind 2
/// `u64 pts | u8 key | Annex B` (track 0), kind 10 `u8 track | u64 pts |
/// u8 key | Annex B`.
async fn read_records(
    from: &mut (impl tokio::io::AsyncRead + Unpin),
    to: &mpsc::Sender<SourceEvent>,
) -> Result<()> {
    loop {
        let mut len = [0u8; 4];
        match from.read_exact(&mut len).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_be_bytes(len) as usize;
        anyhow::ensure!((1..=MAX_AU + 16).contains(&len), "relay record size {len}");
        let mut record = vec![0u8; len];
        from.read_exact(&mut record).await?;
        let event = match record[0] {
            1 => SourceEvent::Event(serde_json::from_slice(&record[1..])?),
            2 => frame(0, &record[1..])?,
            10 => {
                anyhow::ensure!(record.len() >= 2, "short track record");
                frame(record[1], &record[2..])?
            }
            k => bail!("unknown relay record kind {k}"),
        };
        if to.send(event).await.is_err() {
            return Ok(());
        }
    }
}

fn frame(track: u8, p: &[u8]) -> Result<SourceEvent> {
    anyhow::ensure!(
        p.len() > 9 && (track as usize) < MAX_TRACKS,
        "bad video record"
    );
    Ok(SourceEvent::Frame(EncodedFrame {
        track,
        pts_us: u64::from_be_bytes(p[..8].try_into().unwrap()),
        keyframe: p[8] != 0,
        annex_b: p[9..].to_vec(),
    }))
}

/// A crop request: source rectangle and the viewer's pixel size for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub out_w: u16,
    pub out_h: u16,
}

impl Crop {
    pub fn from_control(v: &Value) -> Option<Option<Self>> {
        if v.is_null() {
            return Some(None);
        }
        let f = |k: &str| v.get(k)?.as_u64().and_then(|n| u16::try_from(n).ok());
        Some(Some(Self {
            x: f("x")?,
            y: f("y")?,
            w: f("w")?,
            h: f("h")?,
            out_w: f("out_w")?,
            out_h: f("out_h")?,
        }))
    }
}

/// What one viewer pulls frames from.
pub struct ViewerHandle {
    pub id: u64,
    pub pacer: Arc<Mutex<Pacer>>,
    pub ready: Arc<Notify>,
    /// Hub events for this viewer (ready, view, viewers, error).
    pub events: mpsc::Receiver<Value>,
}

struct Viewer {
    track: u8,
    crop: Option<Crop>,
    pacer: Arc<Mutex<Pacer>>,
    ready: Arc<Notify>,
    events: mpsc::Sender<Value>,
    bps: Option<u64>,
}

#[derive(Default)]
struct Track {
    /// What the encoder last reported for this track.
    view: Option<View>,
    /// What viewers on it asked for (track 0: always the full screen).
    crop: Option<Crop>,
    frame_seq: u32,
    last_sps: Vec<u8>,
    keyframes: KeyframeLimiter,
    keyframes_since: Option<Instant>,
    bitrate: Option<u64>,
}

/// The keyframe budget is per session; a hub outlives sessions, so it
/// starts a fresh budget this often.
const KEYFRAME_BUDGET_PERIOD: Duration = Duration::from_secs(600);

struct State {
    viewers: HashMap<u64, Viewer>,
    tracks: [Option<Track>; MAX_TRACKS],
    multi_track: bool,
    src: Option<(u16, u16)>,
    ready: Option<Value>,
    epoch: u32,
    next_viewer: u64,
    idle_since: Option<Instant>,
    ended: Option<String>,
}

pub struct Hub {
    pub codec: String,
    tracks_max: usize,
    commands: mpsc::UnboundedSender<SourceCommand>,
    state: Mutex<State>,
}

impl Hub {
    /// Starts the encoder and the task that fans its output out.
    pub fn start(
        source: &dyn Source,
        vm: &str,
        codec: &str,
        tracks_max: usize,
    ) -> Result<Arc<Self>> {
        let SourceHandle {
            mut events,
            commands,
        } = source.start(vm, codec)?;
        let mut tracks: [Option<Track>; MAX_TRACKS] = Default::default();
        tracks[0] = Some(Track::default());
        let hub = Arc::new(Self {
            codec: codec.into(),
            tracks_max: tracks_max.clamp(1, MAX_TRACKS),
            commands,
            state: Mutex::new(State {
                viewers: HashMap::new(),
                tracks,
                multi_track: false,
                src: None,
                ready: None,
                epoch: 1,
                next_viewer: 0,
                idle_since: Some(Instant::now()),
                ended: None,
            }),
        });
        let weak = Arc::downgrade(&hub);
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let Some(hub) = weak.upgrade() else { return };
                hub.handle(event, Instant::now());
            }
        });
        Ok(hub)
    }

    pub fn ended(&self) -> Option<String> {
        self.state.lock().unwrap().ended.clone()
    }

    /// True once no viewer has been attached for [`LINGER`].
    pub fn idle(&self, now: Instant) -> bool {
        let s = self.state.lock().unwrap();
        s.ended.is_some()
            || s.idle_since
                .is_some_and(|t| now.duration_since(t) >= LINGER)
    }

    pub fn viewers(&self) -> usize {
        self.state.lock().unwrap().viewers.len()
    }

    pub fn join(&self) -> ViewerHandle {
        let mut s = self.state.lock().unwrap();
        s.next_viewer += 1;
        let id = s.next_viewer;
        let (tx, events) = mpsc::channel(32);
        let pacer = Arc::new(Mutex::new(Pacer::new()));
        let ready = Arc::new(Notify::new());
        if let Some(r) = &s.ready {
            let _ = tx.try_send(r.clone());
        }
        if let Some(reason) = &s.ended {
            let _ = tx.try_send(json!({"event":"error","message":reason}));
        }
        s.viewers.insert(
            id,
            Viewer {
                track: 0,
                crop: None,
                pacer: pacer.clone(),
                ready: ready.clone(),
                events: tx,
                bps: None,
            },
        );
        s.idle_since = None;
        broadcast_count(&s);
        // A new viewer starts on a keyframe.
        self.keyframe_locked(&mut s, 0, Instant::now());
        ViewerHandle {
            id,
            pacer,
            ready,
            events,
        }
    }

    pub fn leave(&self, id: u64) {
        let mut s = self.state.lock().unwrap();
        if let Some(v) = s.viewers.remove(&id) {
            self.release_track(&mut s, v.track);
        }
        if s.viewers.is_empty() {
            s.idle_since = Some(Instant::now());
        }
        broadcast_count(&s);
    }

    pub fn input(&self, events: Value) {
        let _ = self.commands.send(SourceCommand::Input(events));
    }

    /// The viewer lost a frame (or just joined a track): ask for a keyframe,
    /// rate-limited per track.
    pub fn keyframe(&self, viewer: u64, now: Instant) {
        let mut s = self.state.lock().unwrap();
        if let Some(track) = s.viewers.get(&viewer).map(|v| v.track) {
            self.keyframe_locked(&mut s, track, now);
        }
    }

    /// Pending keyframe requests held back by the rate limit.
    pub fn poll(&self, now: Instant) {
        let mut s = self.state.lock().unwrap();
        for t in 0..MAX_TRACKS {
            let due = s.tracks[t]
                .as_mut()
                .is_some_and(|tr| tr.keyframes.poll(now));
            if due {
                self.send_encoder(t as u8, "keyframe");
            }
        }
    }

    /// The viewer's bitrate target; a track runs at the median of its viewers'.
    pub fn bitrate(&self, viewer: u64, bps: u64) {
        let mut s = self.state.lock().unwrap();
        let Some(v) = s.viewers.get_mut(&viewer) else {
            return;
        };
        v.bps = Some(bps);
        let track = v.track;
        let mut wanted: Vec<u64> = s
            .viewers
            .values()
            .filter(|v| v.track == track)
            .filter_map(|v| v.bps)
            .collect();
        wanted.sort_unstable();
        let Some(&median) = wanted.get(wanted.len() / 2) else {
            return;
        };
        let Some(t) = s.tracks[track as usize].as_mut() else {
            return;
        };
        // Encoder rebuild-free, but still not worth a line per 1% change.
        if t.bitrate.is_some_and(|b| b.abs_diff(median) * 20 < b) {
            return;
        }
        t.bitrate = Some(median);
        self.send_encoder(track, &format!("bitrate {median}"));
    }

    /// `{"view":...}` from a viewer: move it to a matching crop track.
    pub fn view(&self, viewer: u64, crop: Option<Crop>, now: Instant) {
        let mut s = self.state.lock().unwrap();
        let Some(current) = s.viewers.get(&viewer).map(|v| v.track) else {
            return;
        };
        let crop = crop.and_then(|c| normalize(c, s.src));
        let target = match crop {
            None => 0,
            Some(c) => match self.track_for(&mut s, c) {
                Some(t) => t,
                None => {
                    if let Some(v) = s.viewers.get(&viewer) {
                        let _ = v.events.try_send(json!({"event":"view","fallback":"full",
                            "message":"no crop track free; zoom in the browser"}));
                    }
                    0
                }
            },
        };
        if let Some(v) = s.viewers.get_mut(&viewer) {
            v.crop = crop;
            v.track = target;
            // Frames of the old track are useless to the decoder now.
            let mut p = v.pacer.lock().unwrap();
            p.lost();
        }
        if target != current {
            self.release_track(&mut s, current);
        }
        self.keyframe_locked(&mut s, target, now);
    }

    fn track_for(&self, s: &mut State, c: Crop) -> Option<u8> {
        if !s.multi_track {
            return None;
        }
        if let Some(t) =
            (1..MAX_TRACKS).find(|&t| s.tracks[t].as_ref().is_some_and(|tr| tr.crop == Some(c)))
        {
            return Some(t as u8);
        }
        let used = s.tracks.iter().filter(|t| t.is_some()).count();
        if used >= self.tracks_max {
            return None;
        }
        let t = (1..MAX_TRACKS).find(|&t| s.tracks[t].is_none())?;
        let (src_w, src_h) = s.src?;
        let (out_w, out_h) = abr::crop_output(
            (c.w as u32, c.h as u32),
            (c.out_w as u32, c.out_h as u32),
            src_w as u64 * src_h as u64,
        );
        s.tracks[t] = Some(Track {
            crop: Some(c),
            ..Default::default()
        });
        let _ = self
            .commands
            .send(SourceCommand::Encoder(format!("track add {t}")));
        self.send_encoder(t as u8, &format!("crop {} {} {} {}", c.x, c.y, c.w, c.h));
        self.send_encoder(t as u8, &format!("scale {out_w} {out_h}"));
        Some(t as u8)
    }

    fn release_track(&self, s: &mut State, track: u8) {
        if track == 0 || s.viewers.values().any(|v| v.track == track) {
            return;
        }
        s.tracks[track as usize] = None;
        let _ = self
            .commands
            .send(SourceCommand::Encoder(format!("track remove {track}")));
    }

    fn keyframe_locked(&self, s: &mut State, track: u8, now: Instant) {
        let due = s.tracks[track as usize].as_mut().is_some_and(|t| {
            if t.keyframes_since
                .is_none_or(|since| now.duration_since(since) >= KEYFRAME_BUDGET_PERIOD)
            {
                t.keyframes = KeyframeLimiter::default();
                t.keyframes_since = Some(now);
            }
            t.keyframes.request(now)
        });
        if due {
            self.send_encoder(track, "keyframe");
        }
    }

    fn send_encoder(&self, track: u8, line: &str) {
        let line = if track == 0 {
            line.to_string()
        } else {
            format!("@{track} {line}")
        };
        let _ = self.commands.send(SourceCommand::Encoder(line));
    }

    fn handle(&self, event: SourceEvent, now: Instant) {
        let mut s = self.state.lock().unwrap();
        match event {
            SourceEvent::Frame(f) => self.fan_out(&mut s, f, now),
            SourceEvent::Event(v) => {
                match v["event"].as_str() {
                    Some("ready") => {
                        s.multi_track = v["backend"] == "apple-virtualization";
                        s.ready = Some(v.clone());
                    }
                    Some("encoder") if v["detail"]["event"] == "view" => {
                        self.encoder_view(&mut s, &v["detail"]);
                    }
                    Some("error") => s.ended = v["message"].as_str().map(str::to_string),
                    _ => {}
                }
                // Encoder diagnostics stay on the host; viewers get what
                // they act on.
                if v["event"] != "encoder" {
                    for viewer in s.viewers.values() {
                        let _ = viewer.events.try_send(v.clone());
                    }
                }
            }
            SourceEvent::Ended(reason) => {
                s.ended = Some(reason.clone());
                for viewer in s.viewers.values() {
                    let _ = viewer
                        .events
                        .try_send(json!({"event":"error","message":reason}));
                }
            }
        }
    }

    fn encoder_view(&self, s: &mut State, d: &Value) {
        if d["removed"] == true {
            return;
        }
        let n = |k: &str| d[k].as_u64().and_then(|v| u16::try_from(v).ok());
        let track = d["track"].as_u64().unwrap_or(0) as usize;
        let (Some(src_w), Some(src_h)) = (n("src_w"), n("src_h")) else {
            return;
        };
        let view = View {
            x: n("x").unwrap_or(0),
            y: n("y").unwrap_or(0),
            w: n("w").unwrap_or(src_w),
            h: n("h").unwrap_or(src_h),
            src_w,
            src_h,
        };
        if view.validate().is_err() || track >= MAX_TRACKS {
            return;
        }
        if s.src != Some((src_w, src_h)) {
            // A guest mode change: a new epoch tells browsers to reconfigure.
            if s.src.is_some() {
                s.epoch = s.epoch.wrapping_add(1);
            }
            s.src = Some((src_w, src_h));
        }
        if let Some(t) = s.tracks[track].as_mut() {
            t.view = Some(view);
        }
    }

    fn fan_out(&self, s: &mut State, f: EncodedFrame, now: Instant) {
        let epoch = s.epoch;
        let Some(track) = s.tracks[f.track as usize].as_mut() else {
            return;
        };
        track.frame_seq = track.frame_seq.wrapping_add(1);
        let hevc = self.codec == "hevc";
        let sps = if f.keyframe {
            first_sps(&f.annex_b, hevc)
        } else {
            None
        };
        let sps_changed = sps.is_some_and(|sps| {
            let changed = track.last_sps != sps;
            track.last_sps = sps.to_vec();
            changed
        });
        if f.keyframe {
            track.keyframes.satisfied();
        }
        let view = track.view.unwrap_or_else(|| {
            let (w, h) = s.src.unwrap_or((0, 0));
            View::full(w, h)
        });
        let header = VideoHeader {
            epoch,
            frame_seq: track.frame_seq,
            keyframe: f.keyframe,
            sps_changed,
            pts_us: f.pts_us,
            encode_us: 0,
            host_queue_us: 0,
            input_ack_seq: 0,
            view,
        };
        let payload = header.encode(&f.annex_b);
        let mut need_key = false;
        for v in s.viewers.values().filter(|v| v.track == f.track) {
            let offered = v.pacer.lock().unwrap().offer(Frame {
                keyframe: f.keyframe,
                enqueued: now,
                bytes: payload.clone(),
            });
            need_key |= offered == Offer::DroppedNeedKeyframe;
            v.ready.notify_one();
        }
        if need_key {
            self.keyframe_locked(s, f.track, now);
        }
    }
}

fn broadcast_count(s: &State) {
    let n = s.viewers.len();
    for v in s.viewers.values() {
        let _ = v.events.try_send(json!({"event":"viewers","n":n}));
    }
}

/// A crop inside the source with even sizes, or `None` for the full screen.
fn normalize(c: Crop, src: Option<(u16, u16)>) -> Option<Crop> {
    let (sw, sh) = src?;
    let x = c.x.min(sw.saturating_sub(2));
    let y = c.y.min(sh.saturating_sub(2));
    let w = c.w.min(sw - x) & !1;
    let h = c.h.min(sh - y) & !1;
    if w < 2 || h < 2 || (x == 0 && y == 0 && w >= sw & !1 && h >= sh & !1) {
        return None;
    }
    Some(Crop {
        x,
        y,
        w,
        h,
        out_w: c.out_w.max(2),
        out_h: c.out_h.max(2),
    })
}

/// The first SPS NAL unit of an Annex B access unit (H.264 type 7, HEVC
/// type 33), to notice a resolution change without parsing it.
pub fn first_sps(au: &[u8], hevc: bool) -> Option<&[u8]> {
    let starts: Vec<usize> = au
        .windows(3)
        .enumerate()
        .filter(|(_, w)| *w == [0, 0, 1])
        .map(|(i, _)| i + 3)
        .collect();
    for (n, &start) in starts.iter().enumerate() {
        let &b = au.get(start)?;
        let is_sps = if hevc {
            (b >> 1) & 0x3f == 33
        } else {
            b & 0x1f == 7
        };
        if is_sps {
            let end = starts
                .get(n + 1)
                .map(|&next| {
                    let mut e = next - 3;
                    while e > start && au[e - 1] == 0 {
                        e -= 1;
                    }
                    e
                })
                .unwrap_or(au.len());
            return Some(&au[start..end]);
        }
    }
    None
}

#[cfg(test)]
#[path = "hub_tests.rs"]
pub(crate) mod tests;
