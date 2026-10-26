//! One browser session over one gateway route: the handshake, then secure
//! records both ways. Video leaves as one tunnel stream per access unit
//! (`u64 route | secure record`), reset when it misses its deadline, so a
//! late frame is dropped instead of delaying the next one; everything else
//! rides the route's reliable stream as `u32 len | secure record`.
use super::handshake::{self, Handshake, Next};
use super::hub::{Crop, Hub, ViewerHandle, HOST_QUEUE_AT, INPUT_ACK_AT};
use super::Remote;
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{mpsc, oneshot};
use vm_control::desktop_input;
use vm_remote_proto::abr::{Controller, Sample};
use vm_remote_proto::config::VideoMode;
use vm_remote_proto::frag;
use vm_remote_proto::record::{self, close, Pointer};
use vm_remote_proto::secure::{Direction, Opener, Sealer};
use vm_remote_proto::tunnel::{self, RouteOpen};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Video streams in flight per viewer; the pacer holds the rest. A stream
/// counts until the gateway acknowledges it, about one tunnel RTT.
const VIDEO_IN_FLIGHT: usize = 4;
const TICK: Duration = Duration::from_millis(250);
const ABR_WINDOW: Duration = Duration::from_millis(500);
/// Records that fail to open (tampered, replayed) before the session ends.
const MAX_BAD_RECORDS: u32 = 64;

/// The tunnel half of a route as the host sees it.
pub struct Route {
    pub conn: quinn::Connection,
    pub open: RouteOpen,
    pub send: quinn::SendStream,
    pub recv: quinn::RecvStream,
    pub datagrams: mpsc::Receiver<Bytes>,
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    let n = r.read(&mut header).await?;
    if n == 0 {
        return Ok(None);
    }
    r.read_exact(&mut header[n..]).await?;
    let mut message = vec![0u8; tunnel::frame_len(header)?];
    r.read_exact(&mut message).await?;
    Ok(Some(message))
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `clamp(max(2 x frame interval, 1.5 x RTT), 33, 150)` ms.
pub fn video_deadline(frame_interval: Duration, rtt: Duration) -> Duration {
    (frame_interval * 2)
        .max(rtt.mul_f64(1.5))
        .clamp(Duration::from_millis(33), Duration::from_millis(150))
}

pub async fn serve(mut route: Route, remote: Arc<Remote>) -> Result<()> {
    let established =
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, admit(&mut route, &remote)).await {
            Ok(Ok(e)) => e,
            Ok(Err(e)) => {
                let refusal = handshake::encode(&handshake::refusal(&e))?;
                let _ = route.send.write_all(&tunnel::frame(&refusal)?).await;
                let _ = route.send.finish();
                return Err(e.context("handshake"));
            }
            Err(_) => {
                let _ = route.send.finish();
                bail!("handshake timed out")
            }
        };
    let session_id = established.session_id.clone();
    let (closed_tx, closed_rx) = oneshot::channel();
    remote.register(&session_id, closed_tx);
    let result = run(route, &remote, established, closed_rx).await;
    remote.unregister(&session_id);
    remote.gate.close(&session_id);
    result
}

async fn admit(route: &mut Route, remote: &Remote) -> Result<handshake::Established> {
    let mut hs = Handshake::new(
        &remote.gate,
        remote.identity.as_ref(),
        &route.open.transport,
        &route.open.origin,
        vm_session_auth::remote::MAX_SESSION_SECONDS,
        handshake::session_id(),
    );
    loop {
        let unit = read_frame(&mut route.recv)
            .await?
            .context("browser left during the handshake")?;
        let message = handshake::decode(&unit)?;
        if let handshake::BrowserMessage::Hello { device_id, .. } = &message {
            anyhow::ensure!(
                remote.handshake_allowed(device_id),
                "handshake rate exceeded"
            );
        }
        match hs.step(message, unix_now())? {
            Next::Send(reply) => {
                route
                    .send
                    .write_all(&tunnel::frame(&handshake::encode(&reply)?)?)
                    .await?;
            }
            Next::Established(e) => return Ok(e),
        }
    }
}

/// Browser-reported link figures, for the bitrate controller.
#[derive(Default)]
struct LinkStats {
    rtt_ms: Option<f64>,
    loss: f64,
    fps: f64,
}

struct Viewer {
    session_id: String,
    route_id: u64,
    hub: Arc<Hub>,
    handle: ViewerHandle,
    sealer: Arc<Mutex<Sealer>>,
    reliable: mpsc::Sender<Vec<u8>>,
    conn: quinn::Connection,
    datagram_video: bool,
    input: bool,
    last_input_seq: u64,
    last_pointer_seq: u32,
    held: desktop_input::Held,
    stats: LinkStats,
    in_flight: usize,
    in_flight_bytes: u64,
}

async fn run(
    route: Route,
    remote: &Arc<Remote>,
    e: handshake::Established,
    mut closed: oneshot::Receiver<u16>,
) -> Result<()> {
    let Route {
        conn,
        open,
        send,
        mut recv,
        mut datagrams,
    } = route;
    let hub = remote.hub(&e.vm_id, handshake::encoder_codec(&e.codec))?;
    let handle = hub.join();
    let sealer = Arc::new(Mutex::new(Sealer::new(&e.keys, Direction::HostToBrowser)?));
    let mut opener = Opener::new(&e.keys, Direction::BrowserToHost)?;
    let (reliable, mut reliable_rx) = mpsc::channel::<Vec<u8>>(64);
    let writer = tokio::spawn(async move {
        let mut send = send;
        while let Some(record) = reliable_rx.recv().await {
            let Ok(framed) = tunnel::frame(&record) else {
                continue;
            };
            if send.write_all(&framed).await.is_err() {
                return;
            }
        }
        let _ = send.finish();
    });
    let mut v = Viewer {
        session_id: e.session_id.clone(),
        route_id: open.route_id,
        hub: hub.clone(),
        handle,
        sealer,
        reliable,
        conn: conn.clone(),
        datagram_video: open.transport == "wt" && remote.config.video.mode == VideoMode::Datagram,
        input: e.scopes.iter().any(|s| s == "input"),
        last_input_seq: 0,
        last_pointer_seq: 0,
        held: Default::default(),
        stats: Default::default(),
        in_flight: 0,
        in_flight_bytes: 0,
    };
    v.event(
        json!({"event":"session","session_id":e.session_id,"vm":e.vm_id,
        "codec":e.codec,"expires_at":e.expires_at,"input":v.input}),
    )
    .await;
    let mut abr = Controller::new(remote.config.video.start_bps);
    let (video_done_tx, mut video_done) = mpsc::unbounded_channel::<(u64, bool)>();
    let mut tick = tokio::time::interval(TICK);
    let mut last_abr = Instant::now();
    let mut bad = 0u32;
    let mut frame_interval = Duration::from_millis(16);
    let mut last_frame_at: Option<Instant> = None;
    let result: Result<u16> = async {
        loop {
            tokio::select! {
                code = &mut closed => return Ok(code.unwrap_or(close::RETRY)),
                frame = read_frame(&mut recv) => {
                    let Some(record) = frame? else { return Ok(0) };
                    if !v.received(&mut opener, &record, remote).await {
                        bad += 1;
                    }
                }
                Some(d) = datagrams.recv() => {
                    if !v.received(&mut opener, &d, remote).await {
                        bad += 1;
                    }
                }
                Some(event) = v.handle.events.recv() => v.event(event).await,
                _ = v.handle.ready.notified(), if v.in_flight < VIDEO_IN_FLIGHT => {
                    let deadline = video_deadline(
                        frame_interval,
                        Duration::from_secs_f64(v.stats.rtt_ms.unwrap_or(0.0) / 1000.0)
                            .max(conn.rtt()),
                    );
                    while v.in_flight < VIDEO_IN_FLIGHT {
                        let Some(frame) = v.handle.pacer.lock().unwrap().pop() else { break };
                        let now = Instant::now();
                        if let Some(last) = last_frame_at {
                            frame_interval = (frame_interval * 7 + now.duration_since(last)) / 8;
                        }
                        last_frame_at = Some(now);
                        v.send_video(frame, deadline, video_done_tx.clone())?;
                    }
                    // More may be waiting once a slot frees.
                    if v.in_flight >= VIDEO_IN_FLIGHT {
                        v.handle.ready.notify_one();
                    }
                }
                Some((bytes, ok)) = video_done.recv() => {
                    v.in_flight -= 1;
                    v.in_flight_bytes = v.in_flight_bytes.saturating_sub(bytes);
                    if !ok {
                        v.handle.pacer.lock().unwrap().lost();
                        hub.keyframe(v.handle.id, Instant::now());
                    }
                    v.handle.ready.notify_one();
                }
                _ = tick.tick() => {
                    let now = Instant::now();
                    if remote.gate.authorize_remote(&v.session_id, "observe", unix_now()).is_err() {
                        return Ok(close::REVOKED);
                    }
                    if bad > MAX_BAD_RECORDS {
                        return Ok(close::INVALID);
                    }
                    hub.poll(now);
                    if now.duration_since(last_abr) >= ABR_WINDOW {
                        last_abr = now;
                        let queue_ms = v.handle.pacer.lock().unwrap().queued_for(now)
                            .map_or(0.0, |d| d.as_secs_f64() * 1000.0);
                        let sample = Sample {
                            rtt_ms: v.stats.rtt_ms.unwrap_or(conn.rtt().as_secs_f64() * 1000.0),
                            loss: v.stats.loss,
                            in_flight: v.in_flight_bytes,
                            queue_ms,
                            fps: v.stats.fps,
                        };
                        if let Some(target) = abr.update(sample, now) {
                            hub.bitrate(v.handle.id, target.bps);
                            v.event(json!({"event":"rate","bps":target.bps,"scale":target.scale})).await;
                        }
                    }
                }
            }
        }
    }
    .await;
    // Release whatever this viewer still holds down, then leave.
    let releases = v.held.releases();
    if !releases.is_empty() {
        if let Ok(raw) = serde_json::to_value(releases.iter().map(wire).collect::<Vec<_>>()) {
            hub.input(raw);
        }
    }
    hub.leave(v.handle.id);
    if let Ok(code) = &result {
        if *code != 0 {
            v.event(json!({"event":"close","code":code})).await;
        }
    }
    drop(v);
    let _ = writer.await;
    result.map(|_| ())
}

/// Browser wire shape of a parsed input event.
fn wire(e: &desktop_input::Event) -> Value {
    use desktop_input::Event::*;
    match e {
        Move { x, y, w, h } => json!({"t":"move","x":x,"y":y,"w":w,"h":h}),
        Button { button, down } => json!({"t":"button","button":button,"down":down}),
        Wheel { dx, dy } => json!({"t":"wheel","dx":dx,"dy":dy}),
        Key { code, down } => json!({"t":"key","code":code,"down":down}),
    }
}

impl Viewer {
    async fn event(&self, value: Value) {
        let Ok(bytes) = serde_json::to_vec(&value) else {
            return;
        };
        let Ok(sealed) = self.sealer.lock().unwrap().seal(record::EVENT, &bytes) else {
            return;
        };
        let _ = self.reliable.send(sealed).await;
    }

    async fn reply(&self, kind: u8, value: Value) {
        let Ok(bytes) = serde_json::to_vec(&value) else {
            return;
        };
        let Ok(sealed) = self.sealer.lock().unwrap().seal(kind, &bytes) else {
            return;
        };
        let _ = self.reliable.send(sealed).await;
    }

    /// Opens and handles one browser record; false when it did not open.
    async fn received(&mut self, opener: &mut Opener, sealed: &[u8], remote: &Remote) -> bool {
        let Ok((kind, _seq, plain)) = opener.open(sealed) else {
            return false;
        };
        if let Err(e) = self.dispatch(kind, &plain, remote).await {
            // Bad input is reported, at most a few times; the session goes on.
            self.event(json!({"event":"input_error","message":format!("{e:#}")}))
                .await;
        }
        true
    }

    fn may_input(&self, remote: &Remote) -> Result<()> {
        anyhow::ensure!(self.input, "this session is view-only");
        remote
            .gate
            .authorize_remote(&self.session_id, "input", unix_now())
    }

    async fn dispatch(&mut self, kind: u8, plain: &[u8], remote: &Remote) -> Result<()> {
        match kind {
            record::INPUT => {
                self.may_input(remote)?;
                anyhow::ensure!(plain.len() <= record::MAX_JSON, "input too large");
                let v: Value = serde_json::from_slice(plain)?;
                let seq = v["seq"].as_u64().context("input without seq")?;
                anyhow::ensure!(seq > self.last_input_seq, "input seq not increasing");
                let events = desktop_input::parse(&v["events"])?;
                self.last_input_seq = seq;
                self.held.update(&events);
                self.hub.input(v["events"].clone());
            }
            record::POINTER => {
                self.may_input(remote)?;
                let p = Pointer::decode(plain)?;
                if p.seq <= self.last_pointer_seq {
                    return Ok(());
                }
                self.last_pointer_seq = p.seq;
                let (x, y) = (p.x.max(0) as u64, p.y.max(0) as u64);
                let (w, h) = (p.w.max(1) as u64, p.h.max(1) as u64);
                let events = json!([{"t":"move","x":x.min(w - 1),"y":y.min(h - 1),"w":w,"h":h}]);
                desktop_input::parse(&events)?;
                self.hub.input(events);
            }
            record::CONTROL => {
                let v: Value = serde_json::from_slice(plain)?;
                if v["keyframe_req"] == true {
                    self.hub.keyframe(self.handle.id, Instant::now());
                } else if let Some(view) = v.get("view") {
                    let crop = Crop::from_control(view).context("invalid view")?;
                    self.hub.view(self.handle.id, crop, Instant::now());
                } else if v.get("takeover").is_some()
                    || v.get("release").is_some()
                    || v.get("activity").is_some()
                {
                    // Control leases live in the daemon (computer use phase
                    // 3); until then every input-scoped viewer may operate.
                    self.event(json!({"event":"lease","mode":"shared","epoch":0}))
                        .await;
                }
            }
            record::PING => {
                let v: Value = serde_json::from_slice(plain)?;
                let now_us = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_micros() as u64;
                self.reply(
                    record::PING,
                    json!({"id":v["id"],"t_send_us":v["t_send_us"],"t_peer_us":now_us}),
                )
                .await;
            }
            record::STATS => {
                let v: Value = serde_json::from_slice(plain)?;
                self.stats.rtt_ms = v["rtt"].as_f64().or(self.stats.rtt_ms);
                self.stats.loss = v["loss"].as_f64().unwrap_or(0.0).clamp(0.0, 1.0);
                self.stats.fps = v["fps"].as_f64().unwrap_or(self.stats.fps);
            }
            _ => {}
        }
        Ok(())
    }

    fn send_video(
        &mut self,
        mut frame: vm_remote_proto::pacer::Frame,
        deadline: Duration,
        done: mpsc::UnboundedSender<(u64, bool)>,
    ) -> Result<()> {
        let queued = frame.enqueued.elapsed().as_micros().min(u32::MAX as u128) as u32;
        frame.bytes[HOST_QUEUE_AT..HOST_QUEUE_AT + 4].copy_from_slice(&queued.to_be_bytes());
        let ack = self.last_input_seq.min(u32::MAX as u64) as u32;
        frame.bytes[INPUT_ACK_AT..INPUT_ACK_AT + 4].copy_from_slice(&ack.to_be_bytes());
        let size = frame.bytes.len() as u64;
        self.in_flight += 1;
        self.in_flight_bytes += size;
        if self.datagram_video {
            let ok = self.send_fragments(&frame.bytes).is_ok();
            let _ = done.send((size, ok));
            return Ok(());
        }
        let sealed = self
            .sealer
            .lock()
            .unwrap()
            .seal(record::VIDEO_V2, &frame.bytes)?;
        let (conn, route_id) = (self.conn.clone(), self.route_id);
        tokio::spawn(async move {
            let ok = send_unit(&conn, route_id, &sealed, deadline).await;
            let _ = done.send((size, ok));
        });
        Ok(())
    }

    fn send_fragments(&self, payload: &[u8]) -> Result<()> {
        let epoch = u32::from_be_bytes(payload[..4].try_into()?);
        let frame_seq = u32::from_be_bytes(payload[4..8].try_into()?);
        for fragment in frag::split(epoch, frame_seq, payload)? {
            let sealed = self
                .sealer
                .lock()
                .unwrap()
                .seal(record::VIDEO_V2, &fragment)?;
            self.conn
                .send_datagram(tunnel::routed(self.route_id, &sealed).into())?;
        }
        Ok(())
    }
}

/// One access unit on its own stream, reset if it misses `deadline`.
async fn send_unit(
    conn: &quinn::Connection,
    route_id: u64,
    sealed: &[u8],
    deadline: Duration,
) -> bool {
    let Ok(mut stream) = conn.open_uni().await else {
        return false;
    };
    let sent = tokio::time::timeout(deadline, async {
        stream.write_all(&route_id.to_be_bytes()).await?;
        stream.write_all(sealed).await?;
        stream.finish()?;
        // Done when the gateway has every byte; a stop means it dropped
        // the unit (its browser side is behind).
        match stream.stopped().await? {
            None => anyhow::Ok(()),
            Some(code) => bail!("gateway stopped the unit ({code})"),
        }
    })
    .await;
    match sent {
        Ok(Ok(())) => true,
        _ => {
            let _ = stream.reset(1u32.into());
            false
        }
    }
}
