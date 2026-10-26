//! Loopback-only integration diagnostic. Not an authenticated internet endpoint.
mod frame_schedule;
#[cfg(test)]
use frame_schedule::frame_header;
mod auth;
mod control_channel;
#[cfg(test)]
mod encoder_budget_tests;
mod encoder_cleanup;
mod encoder_rate;
mod guest_binding;
mod guest_cleanup;
mod ice_config;
#[cfg(test)]
mod media_egress_tests;
mod pinned_client;
mod provenance_evidence;
mod receiver_loss;
mod remote;
mod rtcp_forwarder;
mod shutdown_signal;
mod source_status;
use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rtc::{
    interceptor::Registry,
    media_stream::MediaStreamTrack,
    peer_connection::{
        configuration::{
            interceptor_registry::register_default_interceptors, media_engine::MediaEngine,
        },
        sdp::RTCSessionDescription,
    },
    rtp_transceiver::rtp_sender::{
        RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
        RtpCodecKind,
    },
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
#[cfg(test)]
use tokio::io::AsyncWriteExt;
use tokio::sync::{watch, Mutex};
use webrtc::{
    data_channel::DataChannel,
    media_stream::{
        track_local::{static_rtp::TrackLocalStaticRTP, TrackLocal},
        Track,
    },
    peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
        RTCPeerConnectionState,
    },
};

// Measured 2026-10-12 (M1 Pro, n = 1): aggregate hardware encode
// throughput stops growing at 2 concurrent sessions and a third only adds latency; other chips are unmeasured.
// This per-service ceiling does not relax the existing one-session diagnostic
// guard in SessionBook, or its refusal while cleanup remains unconfirmed.
const MAX_CONCURRENT_MEDIA_SESSIONS: usize = 2;

#[derive(Debug)]
struct EncoderBudgetExceeded;
impl IntoResponse for EncoderBudgetExceeded {
    fn into_response(self) -> Response {
        (
            StatusCode::CONFLICT,
            Json(json!({"error":"encoder_budget_exceeded"})),
        )
            .into_response()
    }
}

fn reserve_encoder(
    budget: &Arc<tokio::sync::Semaphore>,
) -> Result<tokio::sync::OwnedSemaphorePermit, EncoderBudgetExceeded> {
    budget
        .clone()
        .try_acquire_owned()
        .map_err(|_| EncoderBudgetExceeded)
}

#[derive(Debug, PartialEq, Eq)]
enum EncoderFrameFailure {
    ConsecutiveOversizedUnits,
    KeyframeRecoveryExhausted,
}
impl std::fmt::Display for EncoderFrameFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ConsecutiveOversizedUnits => "encoder_frame_size_limit_exceeded",
            Self::KeyframeRecoveryExhausted => "encoder_frame_size_recovery_exhausted",
        })
    }
}
impl std::error::Error for EncoderFrameFailure {}

#[derive(Default)]
struct EncoderFrameSchedule {
    schedule: frame_schedule::Schedule,
    oversized_dropped: u64,
    consecutive_oversized: u64,
    dependency_dropped: u64,
    recoveries: u64,
    recovery: Option<(Instant, u64)>,
    last_request: Option<Instant>,
}
impl EncoderFrameSchedule {
    fn recovery_deadline(&self) -> Option<Instant> {
        self.schedule
            .recovery_deadline()
            .into_iter()
            .chain(
                self.recovery
                    .map(|(at, _)| at + frame_schedule::RECOVERY_LIMIT),
            )
            .min()
    }
    fn classify(
        &mut self,
        pts: u64,
        key: bool,
        bytes: usize,
        now: Instant,
    ) -> Result<frame_schedule::Decision> {
        use vm_media_local::rate_control::{
            MAX_CONSECUTIVE_OVERSIZED_UNITS, MAX_ENCODED_ACCESS_UNIT_BYTES,
        };
        let oversized = bytes > MAX_ENCODED_ACCESS_UNIT_BYTES;
        if oversized {
            self.oversized_dropped += 1;
            self.consecutive_oversized += 1;
            if self.consecutive_oversized >= MAX_CONSECUTIVE_OVERSIZED_UNITS {
                return Err(EncoderFrameFailure::ConsecutiveOversizedUnits.into());
            }
            self.recovery.get_or_insert((now, 0));
        } else {
            self.consecutive_oversized = 0;
        }
        if let Some((began, drops)) = self.recovery {
            if now >= began + frame_schedule::RECOVERY_LIMIT
                || drops >= frame_schedule::MAX_RECOVERY_DROPS
            {
                return Err(EncoderFrameFailure::KeyframeRecoveryExhausted.into());
            }
        }
        // Never admit a dependent AU after discarding an oversized reference,
        // even if its timestamp would satisfy the relative-lag scheduler.
        if oversized || (self.recovery.is_some() && !key) {
            if !oversized {
                self.dependency_dropped += 1;
            }
            self.recovery.as_mut().expect("size recovery started").1 += 1;
            let request_keyframe = self
                .last_request
                .is_none_or(|at| now.duration_since(at) >= frame_schedule::KEYFRAME_INTERVAL);
            if request_keyframe {
                self.last_request = Some(now);
            }
            return Ok(frame_schedule::Decision::Drop { request_keyframe });
        }
        let prior_recoveries = self.schedule.stats().recoveries;
        let decision = self.schedule.classify(pts, key, now)?;
        if matches!(decision, frame_schedule::Decision::Send { .. })
            && self.recovery.take().is_some()
        {
            // One keyframe may repair both lag and size loss; count it once.
            if self.schedule.stats().recoveries == prior_recoveries {
                self.recoveries += 1;
            }
            self.last_request = None;
        }
        Ok(decision)
    }
}

#[derive(Clone)]
struct App {
    origin: String,
    host: String,
    encoder: PathBuf,
    socket: PathBuf,
    web: PathBuf,
    capabilities: Value,
    fault_au_egress_hold: bool,
    fault_drop_first_keyframe: bool,
    fault_suppress_hevc_frames: bool,
    active: Arc<Mutex<SessionBook>>,
    auth: Option<Arc<auth::Auth>>,
    cleanup_failure_exit: Option<i32>,
    guest_binding: Option<Arc<guest_binding::Source>>,
    encoder_rate: Option<encoder_rate::Options>,
}
impl App {
    async fn encoder_budget(&self) -> Arc<tokio::sync::Semaphore> {
        self.active.lock().await.encoder_budget.clone()
    }
}
struct SessionBook {
    encoder_budget: Arc<tokio::sync::Semaphore>,
    stopping: bool,
    active: Option<Session>,
    cancelled: BTreeMap<String, Instant>,
    completed: BTreeMap<String, (Instant, bool, String)>,
    recoveries: BTreeMap<String, (String, guest_cleanup::Ticket)>,
    source_failures: BTreeMap<String, (String, source_status::SourceFailure)>,
    retained_encoders: BTreeMap<String, tokio::sync::OwnedSemaphorePermit>,
}
impl Default for SessionBook {
    fn default() -> Self {
        Self {
            encoder_budget: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_MEDIA_SESSIONS)),
            stopping: false,
            active: None,
            cancelled: BTreeMap::new(),
            completed: BTreeMap::new(),
            recoveries: BTreeMap::new(),
            source_failures: BTreeMap::new(),
            retained_encoders: BTreeMap::new(),
        }
    }
}
impl SessionBook {
    fn finish_encoder_slot(
        &mut self,
        session: &str,
        slot: tokio::sync::OwnedSemaphorePermit,
        clean: bool,
    ) {
        // Failed cleanup may still own hardware; only its confirmed recovery
        // receipt (or process exit) may release the retained capacity.
        if !clean {
            self.retained_encoders.insert(session.into(), slot);
        }
    }
    fn close_receipt(&self, request: &str) -> Value {
        let mut receipt = json!({"closed":true});
        if let Some((_, true, session)) = self.completed.get(request) {
            if let Some((source_session, failure)) = self.source_failures.get(request) {
                if source_session == session {
                    receipt["media_failure"] = json!(failure);
                }
            }
        }
        receipt
    }
    fn prune(&mut self) {
        self.cancelled
            .retain(|_, at| at.elapsed() < Duration::from_secs(60));
        self.completed
            .retain(|_, (at, clean, _)| !*clean || at.elapsed() < Duration::from_secs(60));
        self.recoveries.retain(|request, _| {
            self.completed
                .get(request)
                .is_some_and(|(_, clean, _)| !*clean)
        });
        self.source_failures
            .retain(|request, _| self.completed.contains_key(request));
    }
    fn cancel_id(&mut self, id: &str) -> bool {
        self.prune();
        if self.cancelled.len() >= 256 && !self.cancelled.contains_key(id) {
            return false;
        }
        self.cancelled.insert(id.into(), Instant::now());
        true
    }
}
struct Session {
    request: String,
    id: String,
    cancel: watch::Sender<bool>,
    finished: watch::Receiver<Option<bool>>,
    confirmed: watch::Sender<bool>,
    input: Option<Arc<auth::browser_input::SessionInput>>,
    files: Option<Arc<auth::browser_files::SessionFiles>>,
}
struct SessionSlotGuard {
    book: Arc<Mutex<SessionBook>>,
    request: String,
    session: String,
    encoder: Option<tokio::sync::OwnedSemaphorePermit>,
}
impl SessionSlotGuard {
    fn finish(&mut self, book: &mut SessionBook, clean: bool) {
        if let Some(encoder) = self.encoder.take() {
            Self::finish_slot(book, &self.request, &self.session, encoder, clean);
        }
    }
    fn finish_slot(
        book: &mut SessionBook,
        request: &str,
        session: &str,
        encoder: tokio::sync::OwnedSemaphorePermit,
        clean: bool,
    ) {
        book.finish_encoder_slot(session, encoder, clean);
        if book.active.as_ref().is_some_and(|s| s.id == session) {
            book.active = None;
        }
        // Clearing active alone would admit new capture after a panic. Record
        // unconfirmed cleanup under the same lock so admission and the restart
        // watchdog both see the failed session, including on the Drop path.
        book.completed
            .insert(request.into(), (Instant::now(), clean, session.into()));
    }
}
impl Drop for SessionSlotGuard {
    fn drop(&mut self) {
        if self.encoder.is_none() {
            return;
        }
        let book = self.book.clone();
        if let Ok(mut book) = book.try_lock() {
            self.finish(&mut book, false);
            return;
        }
        // Drop cannot await the session book. Keep the slot and permit owned
        // until this short reconciliation can acquire the admission lock.
        let request = self.request.clone();
        let session = self.session.clone();
        let encoder = self.encoder.take().unwrap();
        tokio::spawn(async move {
            Self::finish_slot(&mut *book.lock().await, &request, &session, encoder, false);
        });
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Offer {
    request_id: String,
    sdp: String,
    r#type: String,
    codec: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Close {
    request_id: String,
    session_id: Option<String>,
}
struct Handler {
    gathered: watch::Sender<bool>,
    connected: watch::Sender<bool>,
    lost: watch::Sender<bool>,
    control: Option<Arc<control_channel::ControlChannel>>,
    disconnect_grace: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}
// A transient ICE `Disconnected` may recover. It becomes terminal only if the
// peer has not returned to `Connected` within this fixed grace. The grace never
// extends a session cap or an authorization check: those run independently.
const DISCONNECT_GRACE: Duration = Duration::from_secs(5);
impl Handler {
    fn cancel_disconnect_grace(&self) {
        let pending = self
            .disconnect_grace
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(pending) = pending {
            pending.abort();
        }
    }
    fn start_disconnect_grace(&self) {
        let mut pending = self
            .disconnect_grace
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A repeated Disconnected must never restart, and so extend, the grace.
        if pending.is_some() {
            return;
        }
        let lost = self.lost.clone();
        *pending = Some(tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(DISCONNECT_GRACE) => { let _ = lost.send(true); }
                // The session already ended; nothing is left to signal.
                _ = lost.closed() => {}
            }
        }));
    }
}
impl Drop for Handler {
    fn drop(&mut self) {
        self.cancel_disconnect_grace();
    }
}
#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        if let Some(control) = &self.control {
            control.accept(channel);
        } else {
            let _ = self.lost.send(true);
            // The peer driver awaits callbacks. Signal session teardown and let
            // peer.close collect every unsolicited channel, with no per-channel task.
            drop(channel);
        }
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.send(true);
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if state == RTCPeerConnectionState::Connected {
            self.cancel_disconnect_grace();
            let _ = self.connected.send(true);
        }
        if state == RTCPeerConnectionState::Disconnected {
            self.start_disconnect_grace();
        }
        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        ) {
            self.cancel_disconnect_grace();
            let _ = self.lost.send(true);
        }
    }
}
struct ConsoleControlDispatch {
    app: App,
    id: String,
}
#[async_trait::async_trait]
impl control_channel::ControlDispatch for ConsoleControlDispatch {
    async fn handle(&self, signed: vm_session_auth::control::SignedControl) -> Result<Value> {
        anyhow::ensure!(
            signed.request.session_id == self.id,
            "control session mismatch"
        );
        let (input, files) = {
            let book = self.app.active.lock().await;
            anyhow::ensure!(!book.stopping, "service stopping");
            let session = book
                .active
                .as_ref()
                .filter(|s| s.id == self.id && *s.confirmed.borrow())
                .context("confirmed session unavailable")?;
            (session.input.clone(), session.files.clone())
        };
        if matches!(
            signed.request.action,
            vm_session_auth::control::ControlAction::File { .. }
        ) {
            files
                .context("file session unavailable")?
                .handle(signed)
                .await
        } else {
            input
                .context("console control unavailable")?
                .handle(signed)
                .await
        }
    }
    async fn replied(&self, sequence: u64, timing: control_channel::ReplyTiming) {
        let input = {
            let book = self.app.active.lock().await;
            book.active
                .as_ref()
                .filter(|s| s.id == self.id)
                .and_then(|s| s.input.clone())
        };
        if let Some(input) = input {
            input.replied(sequence, timing);
        }
    }
    async fn end(&self) -> Result<()> {
        if let Some(auth) = &self.app.auth {
            auth.gate.close(&self.id);
        }
        anyhow::ensure!(
            end_session_input(&self.app, &self.id).await,
            "input retirement unconfirmed"
        );
        Ok(())
    }
}
async fn flag(mut value: watch::Receiver<bool>) {
    loop {
        if *value.borrow_and_update() {
            return;
        }
        if value.changed().await.is_err() {
            return;
        }
    }
}
async fn completion(mut value: watch::Receiver<Option<bool>>) -> bool {
    loop {
        if let Some(clean) = *value.borrow_and_update() {
            return clean;
        }
        if value.changed().await.is_err() {
            return false;
        }
    }
}
fn check(app: &App, headers: &HeaderMap, mutation: bool) -> std::result::Result<(), &'static str> {
    if headers.get(header::HOST).and_then(|v| v.to_str().ok()) != Some(&app.host) {
        return Err("invalid Host");
    }
    if (mutation || headers.contains_key(header::ORIGIN))
        && headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(&app.origin)
    {
        return Err("same-origin required");
    }
    Ok(())
}
async fn get_capabilities(State(app): State<App>, headers: HeaderMap) -> Response {
    if let Err(e) = check(&app, &headers, false) {
        return (StatusCode::FORBIDDEN, e).into_response();
    }
    let mut value = app.capabilities.clone();
    value["authentication"] = json!(app
        .auth
        .as_ref()
        .map(|a| json!({"required":true,"host_public_key":a.host_public_key,"vm_id":a.vm_id,"control_enabled":a.control_state.is_some(),"files_enabled":a.files_enabled,"ice_config_enabled":a.ice_config.is_some(),"control_transport":"webrtc-datachannel-v1"}))
        .unwrap_or(json!({"required":false})));
    value["guest_observation_binding"] = json!({"enabled":app.guest_binding.is_some(),"input_authorized":false,"lock_state":"unknown","atomic_transition_guarantee":false});
    value["encoder_budget"] = json!({"max_concurrent_media_sessions":MAX_CONCURRENT_MEDIA_SESSIONS,"max_encoded_access_unit_bytes":vm_media_local::rate_control::MAX_ENCODED_ACCESS_UNIT_BYTES});
    Json(value).into_response()
}
async fn asset(app: App, headers: HeaderMap, name: &str, mime: &str) -> Response {
    if let Err(e) = check(&app, &headers, false) {
        return (StatusCode::FORBIDDEN, e).into_response();
    }
    match tokio::fs::read(app.web.join(name)).await { Ok(body)=>([(header::CONTENT_TYPE,mime),(header::CACHE_CONTROL,"no-store"),(header::CONTENT_SECURITY_POLICY,"default-src 'self'; connect-src 'self'; media-src 'self' blob:; img-src 'self' data:; object-src 'none'; frame-ancestors 'none'")],body).into_response(), Err(_)=>(StatusCode::NOT_FOUND,"asset unavailable").into_response() }
}
async fn close(State(app): State<App>, headers: HeaderMap, Json(req): Json<Close>) -> Response {
    if let Err(e) = check(&app, &headers, true) {
        return (StatusCode::FORBIDDEN, e).into_response();
    }
    if app.auth.is_some() {
        return (StatusCode::FORBIDDEN, "signed close required").into_response();
    }
    close_inner(&app, req).await
}
async fn close_inner(app: &App, req: Close) -> Response {
    let mut recovery = None;
    let finished = {
        let mut book = app.active.lock().await;
        if uuid::Uuid::parse_str(&req.request_id).is_err() {
            return (StatusCode::BAD_REQUEST, "invalid request id").into_response();
        }
        if !book.cancel_id(&req.request_id) {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "cancellation capacity reached",
            )
                .into_response();
        }
        match book.active.as_ref() {
            Some(session)
                if session.request == req.request_id
                    && req.session_id.as_ref().is_none_or(|id| id == &session.id) =>
            {
                let _ = session.cancel.send(true);
                Some(session.finished.clone())
            }
            Some(_) => return (StatusCode::FORBIDDEN, "session mismatch").into_response(),
            None => {
                if book
                    .completed
                    .get(&req.request_id)
                    .is_some_and(|(_, _, id)| {
                        req.session_id
                            .as_ref()
                            .is_some_and(|requested| requested != id)
                    })
                {
                    return (StatusCode::FORBIDDEN, "session mismatch").into_response();
                }
                if book
                    .completed
                    .get(&req.request_id)
                    .is_some_and(|(_, clean, _)| !*clean)
                {
                    recovery = book.recoveries.get(&req.request_id).cloned();
                    if recovery.is_none() {
                        return (StatusCode::GATEWAY_TIMEOUT, "cleanup unconfirmed")
                            .into_response();
                    }
                }
                None
            }
        }
    };
    if let Some((session, ticket)) = recovery {
        let clean = match &app.guest_binding {
            Some(source) => source.recover_cleanup(&ticket).await.is_ok(),
            None => false,
        };
        let mut book = app.active.lock().await;
        // Only the original failed session can consume its original receipt.
        if let Some((at, confirmed, id)) = book.completed.get_mut(&req.request_id) {
            if id == &session && clean {
                *confirmed = true;
                *at = Instant::now();
            }
        }
        if !book
            .completed
            .get(&req.request_id)
            .is_some_and(|(_, clean, id)| *clean && id == &session)
        {
            return (StatusCode::GATEWAY_TIMEOUT, "cleanup unconfirmed").into_response();
        }
        book.recoveries.remove(&req.request_id);
        book.retained_encoders.remove(&session);
    }
    if let Some(finished) = finished {
        if !matches!(
            tokio::time::timeout(Duration::from_secs(12), completion(finished)).await,
            Ok(true)
        ) {
            return (StatusCode::GATEWAY_TIMEOUT, "cleanup unconfirmed").into_response();
        }
    }
    Json(app.active.lock().await.close_receipt(&req.request_id)).into_response()
}
async fn offer(State(app): State<App>, headers: HeaderMap, Json(req): Json<Offer>) -> Response {
    if let Err(e) = check(&app, &headers, true) {
        return (StatusCode::FORBIDDEN, e).into_response();
    }
    if app.auth.is_some() {
        return (StatusCode::FORBIDDEN, "signed offer required").into_response();
    }
    offer_inner(app, req, None).await
}
async fn offer_inner(
    app: App,
    req: Offer,
    accepted: Option<vm_session_auth::AcceptedOffer>,
) -> Response {
    let budget = app.encoder_budget().await;
    offer_with_encoder_budget(app, req, accepted, &budget).await
}
async fn offer_with_encoder_budget(
    app: App,
    req: Offer,
    accepted: Option<vm_session_auth::AcceptedOffer>,
    budget: &Arc<tokio::sync::Semaphore>,
) -> Response {
    if uuid::Uuid::parse_str(&req.request_id).is_err()
        || req.r#type != "offer"
        || req.sdp.len() > 200_000
        || !matches!(req.codec.as_str(), "h264" | "h265")
    {
        return (StatusCode::BAD_REQUEST, "invalid offer").into_response();
    }
    if app.capabilities[&req.codec] != "verified-hardware" {
        return (
            StatusCode::CONFLICT,
            "required hardware encoder unavailable",
        )
            .into_response();
    }
    let (cancel, cancel_rx) = watch::channel(false);
    let (confirmed, confirmed_rx) = watch::channel(accepted.is_none());
    let cancel_on_timeout = cancel.clone();
    let (finished_tx, finished) = watch::channel(None);
    let cleanup_confirmed = Arc::new(AtomicBool::new(true));
    let id = uuid::Uuid::new_v4().to_string();
    let request = req.request_id.clone();
    let encoder_slot;
    {
        let mut book = app.active.lock().await;
        if book.stopping {
            return (StatusCode::SERVICE_UNAVAILABLE, "service stopping").into_response();
        }
        book.prune();
        if book.cancelled.contains_key(&request) || book.completed.contains_key(&request) {
            return (StatusCode::CONFLICT, "request already cancelled").into_response();
        }
        if book.cancelled.len() >= 256 || book.completed.len() >= 256 {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "cancellation capacity reached",
            )
                .into_response();
        }
        if book.completed.values().any(|(_, clean, _)| !*clean) {
            return (StatusCode::CONFLICT, "previous session cleanup unconfirmed").into_response();
        }
        if book.active.is_some() {
            return (
                StatusCode::CONFLICT,
                "one diagnostic session already active",
            )
                .into_response();
        }
        encoder_slot = match reserve_encoder(budget) {
            Ok(slot) => slot,
            Err(refusal) => return refusal.into_response(),
        };
        book.active = Some(Session {
            request: request.clone(),
            id: id.clone(),
            cancel,
            finished,
            confirmed,
            input: None,
            files: None,
        });
    }
    let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();
    let session_id = id.clone();
    let mut slot = SessionSlotGuard {
        book: app.active.clone(),
        request: request.clone(),
        session: session_id.clone(),
        encoder: Some(encoder_slot),
    };
    tokio::spawn(async move {
        let mut evidence = SessionEvidence::default();
        let outcome = run_session(
            &app,
            &req,
            cancel_rx,
            answer_tx,
            cleanup_confirmed.clone(),
            &mut evidence,
            auth::SessionSetup {
                id: session_id.clone(),
                accepted,
                confirmed: confirmed_rx,
            },
        )
        .await;
        if let Some(auth) = &app.auth {
            auth.teardown_session(&request, &session_id);
        }
        let clean = cleanup_confirmed.load(Ordering::Acquire);
        // No SDP, ICE credentials or frame data in logs.
        if let Err(error) = outcome {
            eprintln!("media session failed: {error:#}");
        }
        {
            let mut active = app.active.lock().await;
            if let Some(failure) = evidence.source_failure {
                active
                    .source_failures
                    .insert(request.clone(), (session_id.clone(), failure));
            }
            if !clean {
                if let Some(ticket) = evidence.recovery {
                    active
                        .recoveries
                        .insert(request.clone(), (session_id.clone(), ticket));
                }
            }
            slot.finish(&mut active, clean);
        }
        let _ = finished_tx.send(Some(clean));
    });
    match tokio::time::timeout(Duration::from_secs(8), answer_rx).await {
        Ok(Ok(Ok(mut answer))) => {
            answer["session_id"] = json!(id);
            Json(answer).into_response()
        }
        Ok(Ok(Err(error))) => {
            (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response()
        }
        _ => {
            let _ = cancel_on_timeout.send(true);
            (StatusCode::GATEWAY_TIMEOUT, "offer failed or timed out").into_response()
        }
    }
}
#[derive(Default)]
struct SessionEvidence {
    source_failure: Option<source_status::SourceFailure>,
    recovery: Option<guest_cleanup::Ticket>,
}
async fn run_session(
    app: &App,
    req: &Offer,
    cancel: watch::Receiver<bool>,
    answer: tokio::sync::oneshot::Sender<Result<Value, String>>,
    cleanup_confirmed: Arc<AtomicBool>,
    evidence: &mut SessionEvidence,
    authorization: auth::SessionSetup,
) -> Result<()> {
    let session_started = Instant::now();
    let SessionEvidence {
        source_failure,
        recovery,
    } = evidence;
    if let Some(binding) = &app.guest_binding {
        tokio::select! { result=binding.validate()=>result.inspect_err(|error| { *source_failure = Some(guest_binding::source_failure(error)); })?, _=flag(cancel.clone())=>return Ok(()) };
    }
    let (gathered_tx, gathered) = watch::channel(false);
    let (connected_tx, connected) = watch::channel(false);
    let (lost_tx, lost) = watch::channel(false);
    let codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: if req.codec == "h265" {
                "video/H265"
            } else {
                "video/H264"
            }
            .into(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: if req.codec == "h265" {
                "profile-id=1;tier-flag=0;level-id=120;tx-mode=SRST"
            } else {
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
            }
            .into(),
            rtcp_feedback: vec![],
        },
        payload_type: if req.codec == "h265" { 98 } else { 102 },
    };
    let mut engine = MediaEngine::default();
    engine.register_codec(codec.clone(), RtpCodecKind::Video)?;
    let (nack_responder, nack_monitor) = webrtc::bounded_nack::BoundedNack::new();
    let mut registry = register_default_interceptors(Registry::new(), &mut engine)?
        .with(rtc::interceptor::Slot::NackResponder, nack_responder)
        .with(
            rtc::interceptor::Slot::from(14_000),
            rtcp_forwarder::RtcpForwarder::new(12345678),
        );
    let loss_feedback = if app.encoder_rate.as_ref().is_some_and(|v| v.loss_response) {
        let (observer, feedback) = receiver_loss::ReceiverLoss::new(
            12345678,
            app.encoder_rate.as_ref().is_some_and(|v| v.fault_rtp_loss),
        );
        registry = registry.with(rtc::interceptor::Slot::from(3500), observer);
        Some(feedback)
    } else {
        None
    };
    anyhow::ensure!(
        registry.slots().iter().any(
            |(slot, name)| *slot == rtc::interceptor::Slot::NackResponder && name == "BoundedNack"
        ),
        "bounded NACK responder missing from registry"
    );
    let control = app
        .auth
        .as_ref()
        .filter(|auth| auth.control_state.is_some() || auth.files_enabled)
        .map(|_| {
            control_channel::ControlChannel::new(
                Arc::new(ConsoleControlDispatch {
                    app: app.clone(),
                    id: authorization.id.clone(),
                }),
                lost_tx.clone(),
            )
        });
    let peer = PeerConnectionBuilder::new()
        .with_setting_engine(control_channel::bounded_settings())
        .with_data_channel_send_buffer_limit(control_channel::SEND_BUFFER_BYTES)
        .with_configuration(ice_config::configuration(
            ice_config::peer_credential(
                app.auth.as_ref().and_then(|a| a.ice_config.as_deref()),
                &req.request_id,
                auth::unix_now()?,
            )?
            .as_ref(),
            auth::unix_now()?,
        )?)
        .with_media_engine(engine)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Handler {
            gathered: gathered_tx,
            connected: connected_tx,
            lost: lost_tx,
            control: control.clone(),
            disconnect_grace: Default::default(),
        }))
        .with_runtime(webrtc::runtime::default_runtime().context("Tokio runtime unavailable")?)
        .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
        .build()
        .await?;
    let media_egress = peer.media_egress();
    nack_monitor.attach(media_egress.clone())?;
    let fence_id = uuid::Uuid::new_v4().to_string();
    let mut media_fenced = false;
    let mut input_cleanup_done = false;
    let result=async {
        let track=Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new("vm-console".into(),"video".into(),"VM desktop".into(),RtpCodecKind::Video,vec![RTCRtpEncodingParameters{rtp_coding_parameters:RTCRtpCodingParameters{ssrc:Some(12345678),..Default::default()},codec:codec.rtp_codec.clone(),..Default::default()}])));
        let sender=peer.add_track(track.clone() as Arc<dyn TrackLocal>).await?;
        let setup=async {
            // The SDP parser's and serde's error texts quote the rejected line, which can carry ICE
            // credentials and client-chosen line breaks. Only a fixed label may reach the log.
            let offer=serde_json::from_value::<RTCSessionDescription>(json!({"type":"offer","sdp":req.sdp})).map_err(|_| anyhow!("offer description rejected"))?;
            peer.set_remote_description(offer).await.map_err(|_| anyhow!("remote description rejected"))?;
            let answer=peer.create_answer(None).await.map_err(|_| anyhow!("answer creation failed"))?;peer.set_local_description(answer).await.map_err(|_| anyhow!("local description rejected"))?;
            tokio::time::timeout(Duration::from_secs(4),flag(gathered)).await?;
            let answer=peer.local_description().await.context("missing answer")?;
            let mut value = serde_json::to_value(answer)?;
            if let Some(accepted) = authorization.accepted {
                let auth = app.auth.as_ref().context("authentication unavailable")?;
                auth.reload()?;
                let signed = auth.sign_answer(accepted, &authorization.id, value["sdp"].as_str().context("answer SDP missing")?)?;
                value["signed_answer"] = serde_json::to_value(signed)?;
            }
            Ok::<Value,anyhow::Error>(value)
        };
        let setup=tokio::select!{result=setup=>result,_=flag(cancel.clone())=>Err(anyhow!("cancelled during negotiation"))};
        match setup {Ok(description)=>{let _=answer.send(Ok(description));},Err(error)=>{let _=answer.send(Err("WebRTC negotiation failed".into()));return Err(error);}}
        let ready=async { tokio::try_join!(tokio::time::timeout(Duration::from_secs(12),flag(connected)),tokio::time::timeout(Duration::from_secs(12),flag(authorization.confirmed)))?; Ok::<(),anyhow::Error>(()) };
        tokio::select!{result=ready=>result?,_=flag(cancel.clone())=>return Ok(()),_=flag(lost.clone())=>bail!("connection failed before streaming"),result=auth::watch_authorization(app.auth.clone(),&authorization.id,false)=>return result};
        if let Some(auth) = &app.auth { auth.reload()?; auth.gate.authorize_active(&authorization.id,"observe")?; }
        anyhow::ensure!(!nack_monitor.snapshot().counters.failed,"bounded NACK responder failed before streaming");
        let parameters=sender.get_parameters().await?;
        let selected=parameters.rtp_parameters.codecs.iter().find(|p|native_codec_matches(&req.codec,&p.rtp_codec.mime_type,&p.rtp_codec.sdp_fmtp_line)).context("negotiated codec profile does not match native encoder; refusing MIME-only fallback")?;
        let payload=selected.payload_type;
        media_egress.enable(payload)?;
        eprintln!("media negotiated codec={} payload={} fmtp={}",req.codec,payload,selected.rtp_codec.sdp_fmtp_line);
        let ssrc=*track.ssrcs().await.first().context("missing SSRC")?;
        let mut au_deadlines=sender.enable_au_deadlines(ssrc,payload).await?;
        if app.fault_au_egress_hold {media_egress.arm_au_hold_diagnostic()?;}
        let expected_identity = if let Some(binding) = &app.guest_binding {
            Some(tokio::select! { result=binding.prepare()=>result.inspect_err(|error| { *source_failure = Some(guest_binding::source_failure(error)); })?, _=flag(cancel.clone())=>return Ok(()), _=flag(lost.clone())=>bail!("connection lost while verifying Guest binding") })
        } else { None };
        // Recheck authority after potentially slow local identity RPCs and before any encoder starts.
        if let Some(auth) = &app.auth { auth.reload()?; auth.gate.authorize_active(&authorization.id,"observe")?; }
        let mut command=tokio::process::Command::new(&app.encoder);
        command.arg(&app.socket).args(["--codec",if req.codec=="h265"{"hevc"}else{"h264"}]);
        if let Some(expected) = &expected_identity { command.arg("--expected-console-identity").arg(expected.path()); }
        let receipt_path = expected_identity.as_ref().filter(|_| app.capabilities["cleanup_receipt_version"] == 1).map(|file| file.receipt_path());
        if let Some(path) = &receipt_path { command.arg("--cleanup-receipt").arg(path); }
        let rate_channel = if app.encoder_rate.is_some() { Some(encoder_rate::SpawnChannel::configure(&mut command)?) } else { None };
        let mut child=command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(if app.cleanup_failure_exit.is_some() { Stdio::piped() } else { Stdio::inherit() }).kill_on_drop(true).spawn()?;
        let mut rate_client = rate_channel.map(encoder_rate::SpawnChannel::spawned);
        let (rate_first_tx, rate_first_rx) = watch::channel(false);
        let diagnostics = child.stderr.take().map(|stderr| tokio::spawn(source_status::collect_report(stderr)));
        cleanup_confirmed.store(false,Ordering::Release);
        let mut input=child.stdin.take().context("native stdin")?;let mut output=child.stdout.take().context("native stdout")?;
        let mut packetizer=rtc::rtp::packetizer::new_packetizer(Instant::now(),1200,payload,ssrc,codec.rtp_codec.payloader()?,Box::new(rtc::rtp::sequence::new_random_sequencer()),90_000);
        let timestamp_base=uuid::Uuid::new_v4().as_u128() as u32;
        let (keyframe_tx,mut keyframe_rx)=tokio::sync::mpsc::channel::<()>(1);
        let mut scheduling=EncoderFrameSchedule::default();
        let streaming=async {
            let mut previous=None; let mut frames=0_u64; let mut queued_frames=0_u64;
            loop {
                let frame=frame_schedule::read_frame(&mut output,previous,scheduling.recovery_deadline()).await?;
                let frame_schedule::Frame{pts,key,data}=frame;
                if frames == 0 { let _ = rate_first_tx.send(true); }
                if frames==0 && !key {bail!("first native frame must be keyframe")};
                previous=Some(pts);
                if req.codec=="h265" && app.fault_suppress_hevc_frames {
                    if frames==0 {eprintln!("FAULT: suppressing actual hardware HEVC access units before RTP to exercise first-frame fallback");}
                    frames+=1;continue;
                }
                if frames==0 && app.fault_drop_first_keyframe { frames+=1;eprintln!("FAULT: deliberately dropped first complete keyframe before RTP; awaiting actual receiver recovery feedback");continue; }
                let deadline=match scheduling.classify(pts,key,data.len(),Instant::now())? {
                    frame_schedule::Decision::Send{deadline}=>deadline,
                    frame_schedule::Decision::Drop{request_keyframe}=>{
                        if request_keyframe { match keyframe_tx.try_send(()) { Ok(())|Err(tokio::sync::mpsc::error::TrySendError::Full(()))=>{},Err(_)=>bail!("native keyframe command channel closed") } }
                        frames+=1;
                        // Let feedback write the queued keyframe command even if
                        // many old AUs are immediately readable from the pipe.
                        tokio::task::yield_now().await;continue;
                    }
                };
                if key {eprintln!("media native keyframe index={frames}");}
                use rtc::rtp::packetizer::Packetizer;
                let mut packets=packetizer.packetize(Instant::now(),&data.into(),0)?;
                let timestamp=timestamp_base.wrapping_add(((pts as u128*90_000+500_000)/1_000_000) as u32);
                for packet in &mut packets { packet.header.timestamp=timestamp; }
                if packets.is_empty() {bail!("native AU produced no RTP packets")};
                au_deadlines.register(timestamp,deadline)?;
                for packet in packets { frame_schedule::enqueue_before(deadline,track.write_rtp(packet)).await?; }
                frames+=1;queued_frames+=1;if queued_frames==1 || queued_frames.is_multiple_of(300) {eprintln!("media encoded access units submitted to RTP queue: {queued_frames}");}
            }
            #[allow(unreachable_code)] Ok::<(),anyhow::Error>(())
        };
        let feedback=async {
            let mut keyframe_writer=frame_schedule::KeyframeWriter::new(&mut input);
            loop {
                let request=tokio::select! {
                    requested=keyframe_rx.recv()=>{if requested.is_none(){return Ok::<(),anyhow::Error>(())} true},
                    event=track.poll()=>{match event {
                        Some(webrtc::media_stream::track_local::TrackLocalEvent::OnRtcpPacket(packets))=>rtcp_forwarder::requests_keyframe(&packets,ssrc),
                        None=>return Ok(()),
                        Some(_)=>false,
                    }},
                };
                if request {
                    keyframe_writer.request().await?;
                    eprintln!("media native keyframe command written (receiver feedback, relative sender lag or oversized AU)");
                }
            }
        };
        let result=tokio::select!{result=streaming=>result,result=feedback=>result,result=watch_media_egress(&media_egress)=>result,_=nack_monitor.failed()=>Err(anyhow!("bounded NACK responder failed")),result=encoder_rate::drive(&mut rate_client,app.encoder_rate.as_ref(),rate_first_rx,loss_feedback.as_ref(),session_started)=>result.map_err(|_| anyhow!("encoder rate channel failed")),result=guest_binding::watch(app.guest_binding.as_deref())=>result.inspect_err(|error| { let failure=guest_binding::source_failure(error);eprintln!("media failure category={}",failure.code);*source_failure = Some(failure); }),result=auth::watch_authorization(app.auth.clone(),&authorization.id,true)=>result.inspect_err(|_|eprintln!("media failure category=session_authorization")),_=flag(cancel.clone())=>Ok(()),_=flag(lost)=>Ok(()),_=ice_config::media_session_cap_elapsed()=>Ok(())};
        eprintln!("media relative_sender_lag stale_dropped={} dependency_dropped={} recovery_keyframes_admitted={} oversized_dropped={} consecutive_oversized={}",scheduling.schedule.stats().stale_dropped,scheduling.schedule.stats().dependency_dropped+scheduling.dependency_dropped,scheduling.schedule.stats().recoveries+scheduling.recoveries,scheduling.oversized_dropped,scheduling.consecutive_oversized);
        log_media_egress(&fence_id, "before_cleanup", session_started, media_egress.fence());
        log_nack_budget(&fence_id,"before_cleanup",session_started,nack_monitor.snapshot());
        provenance_evidence::log(&fence_id,"before_cleanup",session_started,media_egress.provenance_snapshot());
        media_fenced = true;
        // Revoke input authority before waiting for native encoder teardown.
        if let Some(auth) = &app.auth { auth.gate.close(&authorization.id); }
        let control_cleanup = end_session_input(app, &authorization.id);
        let video_cleanup = async {
            let outcome = encoder_cleanup::stop_and_wait(&mut child, input, &mut output).await;
            let clean = outcome.graceful_status.is_some_and(|status| cleanup_attested_exit(status.code(), app.cleanup_failure_exit));
            (clean, outcome.child_reaped)
        };
        let (control_clean, (video_clean, child_reaped)) = tokio::join!(control_cleanup, video_cleanup);
        drop(rate_client);
        if !video_clean && child_reaped && control_clean {
            *recovery = receipt_path.as_ref().and_then(|path| guest_cleanup::Ticket::read(path).ok());
        }
        if let Some(mut diagnostics) = diagnostics {
            let diagnostic = match tokio::time::timeout(Duration::from_millis(250), &mut diagnostics).await {
                Ok(Ok(report)) => { if source_failure.is_none() { *source_failure = report.failure; } report.diagnostic },
                _ => { diagnostics.abort(); let _ = diagnostics.await; source_status::Diagnostic::CollectionUnavailable }
            };
            eprintln!("{}", serde_json::json!({"version":1,"event":"native_source_diagnostic","fence_id":fence_id,"phase":"after_encoder_cleanup","elapsed_ms":session_started.elapsed().as_millis(),"diagnostic":diagnostic}));
        }
        if !control_clean || !video_clean { eprintln!("media cleanup unconfirmed: controller_retired={control_clean} encoder_stopped={video_clean}"); }
        cleanup_confirmed.store(control_clean && video_clean,Ordering::Release);
        input_cleanup_done = true;
        drop(expected_identity);
        result
    }.await;
    if !media_fenced {
        log_media_egress(
            &fence_id,
            "before_cleanup",
            session_started,
            media_egress.fence(),
        );
    }
    if !media_fenced {
        log_nack_budget(
            &fence_id,
            "before_cleanup",
            session_started,
            nack_monitor.snapshot(),
        );
        provenance_evidence::log(
            &fence_id,
            "before_cleanup",
            session_started,
            media_egress.provenance_snapshot(),
        );
    }
    if let Some(auth) = &app.auth {
        auth.gate.close(&authorization.id);
    }
    if !input_cleanup_done && !end_session_input(app, &authorization.id).await {
        cleanup_confirmed.store(false, Ordering::Release);
        *recovery = None;
    }
    if let Some(control) = control {
        if !control.shutdown().await {
            *recovery = None;
            eprintln!("media cleanup unconfirmed: control_channel_stopped=false");
            cleanup_confirmed.store(false, Ordering::Release);
        }
    }
    // Keep library error text private: it can contain transport details. The
    // fixed outcome distinguishes an actual close error from our deadline.
    let peer_close_outcome = match tokio::time::timeout(Duration::from_secs(2), peer.close()).await
    {
        Ok(Ok(())) => "closed",
        Ok(Err(_)) => "close_error",
        Err(_) => "timeout",
    };
    eprintln!(
        "{}",
        serde_json::json!({"version":1,"event":"peer_cleanup_outcome","fence_id":fence_id,"elapsed_ms":session_started.elapsed().as_millis(),"outcome":peer_close_outcome})
    );
    if peer_close_outcome != "closed" {
        *recovery = None;
        eprintln!("media cleanup unconfirmed: peer_closed=false");
        cleanup_confirmed.store(false, Ordering::Release);
    }
    let final_nack = nack_monitor.snapshot();
    log_nack_budget(
        &fence_id,
        "after_peer_close",
        session_started,
        final_nack.clone(),
    );
    let final_egress = media_egress.snapshot();
    log_media_egress(&fence_id, "after_peer_close", session_started, final_egress);
    let final_provenance = media_egress.provenance_snapshot();
    provenance_evidence::log(
        &fence_id,
        "after_peer_close",
        session_started,
        final_provenance,
    );
    if !provenance_evidence::cleanup_attested(&final_provenance) {
        cleanup_confirmed.store(false, Ordering::Release);
        *recovery = None;
        bail!("AU provenance ownership cleanup unconfirmed");
    }
    if final_egress.failed || final_egress.pending_media_send {
        cleanup_confirmed.store(false, Ordering::Release);
        *recovery = None;
        bail!("media egress fence unconfirmed");
    }
    if !nack_cleanup_attested(&final_nack) {
        cleanup_confirmed.store(false, Ordering::Release);
        *recovery = None;
        bail!("bounded NACK responder cleanup unconfirmed");
    }
    if final_nack.counters.failed {
        bail!("bounded NACK responder failed");
    }
    if final_egress.policy_stop_reason.is_some() {
        bail!("local AU submission deadline expired");
    }
    result
}
fn nack_budget_value(s: &webrtc::bounded_nack::Snapshot) -> Value {
    let mut counters = serde_json::Map::from_iter([
        ("bound".into(), json!(s.counters.bound)),
        ("closed".into(), json!(s.counters.closed)),
        ("failed".into(), json!(s.counters.failed)),
        ("failure_reason".into(), json!(s.counters.failure_reason)),
        ("cache_packets".into(), json!(s.counters.cache_packets)),
        ("cache_bytes".into(), json!(s.counters.cache_bytes)),
        (
            "cache_packets_high_water".into(),
            json!(s.counters.cache_packets_high_water),
        ),
        (
            "cache_bytes_high_water".into(),
            json!(s.counters.cache_bytes_high_water),
        ),
        (
            "pending_repair_packets".into(),
            json!(s.counters.pending_repair_packets),
        ),
        (
            "pending_repair_bytes".into(),
            json!(s.counters.pending_repair_bytes),
        ),
        (
            "pending_repair_packets_high_water".into(),
            json!(s.counters.pending_repair_packets_high_water),
        ),
        (
            "pending_repair_bytes_high_water".into(),
            json!(s.counters.pending_repair_bytes_high_water),
        ),
        (
            "pending_forward_packets".into(),
            json!(s.counters.pending_forward_packets),
        ),
        (
            "pending_forward_bytes".into(),
            json!(s.counters.pending_forward_bytes),
        ),
        (
            "pending_forward_packets_high_water".into(),
            json!(s.counters.pending_forward_packets_high_water),
        ),
        (
            "pending_forward_bytes_high_water".into(),
            json!(s.counters.pending_forward_bytes_high_water),
        ),
        (
            "pending_read_packets".into(),
            json!(s.counters.pending_read_packets),
        ),
        (
            "pending_read_bytes".into(),
            json!(s.counters.pending_read_bytes),
        ),
        (
            "pending_read_packets_high_water".into(),
            json!(s.counters.pending_read_packets_high_water),
        ),
        (
            "pending_read_bytes_high_water".into(),
            json!(s.counters.pending_read_bytes_high_water),
        ),
        ("cached_packets".into(), json!(s.counters.cached_packets)),
        ("cache_evictions".into(), json!(s.counters.cache_evictions)),
        (
            "cache_skipped_oversize".into(),
            json!(s.counters.cache_skipped_oversize),
        ),
        (
            "nack_pairs_processed".into(),
            json!(s.counters.nack_pairs_processed),
        ),
        (
            "duplicate_requests".into(),
            json!(s.counters.duplicate_requests),
        ),
        (
            "unknown_stream_nacks".into(),
            json!(s.counters.unknown_stream_nacks),
        ),
        ("cache_misses".into(), json!(s.counters.cache_misses)),
        (
            "repair_enqueued_packets".into(),
            json!(s.counters.repair_enqueued_packets),
        ),
        (
            "repair_enqueued_bytes".into(),
            json!(s.counters.repair_enqueued_bytes),
        ),
        (
            "repair_suppressed_packets".into(),
            json!(s.counters.repair_suppressed_packets),
        ),
        (
            "repair_expired_before_enqueue".into(),
            json!(s.counters.repair_expired_before_enqueue),
        ),
        (
            "repair_expired_before_dequeue".into(),
            json!(s.counters.repair_expired_before_dequeue),
        ),
        (
            "nack_budget_exhaustions".into(),
            json!(s.counters.nack_budget_exhaustions),
        ),
        (
            "repair_dequeued_packets".into(),
            json!(s.counters.repair_dequeued_packets),
        ),
        (
            "repair_discarded_on_unbind".into(),
            json!(s.counters.repair_discarded_on_unbind),
        ),
        ("unbinds".into(), json!(s.counters.unbinds)),
        ("closes".into(), json!(s.counters.closes)),
    ]);
    counters.insert("caps".into(),json!({"cache_packets":s.caps.cache_packets,"cache_bytes":s.caps.cache_bytes,"cached_packet_bytes":s.caps.cached_packet_bytes,"repair_packets":s.caps.repair_packets,"repair_bytes":s.caps.repair_bytes,"forward_packets":s.caps.forward_packets,"forward_bytes":s.caps.forward_bytes,"read_packets":s.caps.read_packets,"read_bytes":s.caps.read_bytes,"nack_pairs_per_compound":s.caps.nack_pairs_per_compound,"rtcp_messages_inspected":s.caps.rtcp_messages_inspected}));
    Value::Object(counters)
}
fn nack_cleanup_attested(s: &webrtc::bounded_nack::Snapshot) -> bool {
    let c = &s.counters;
    c.closed
        && !c.bound
        && c.cache_packets == 0
        && c.cache_bytes == 0
        && c.pending_repair_packets == 0
        && c.pending_repair_bytes == 0
        && c.pending_forward_packets == 0
        && c.pending_forward_bytes == 0
        && c.pending_read_packets == 0
        && c.pending_read_bytes == 0
}
fn log_nack_budget(
    fence_id: &str,
    phase: &str,
    started: Instant,
    s: webrtc::bounded_nack::Snapshot,
) {
    eprintln!(
        "{}",
        json!({"version":2,"event":"nack_budget_snapshot","fence_id":fence_id,"phase":phase,"elapsed_ms":started.elapsed().as_millis(),"counters":nack_budget_value(&s)})
    );
}
fn log_media_egress(
    fence_id: &str,
    phase: &str,
    started: Instant,
    counters: webrtc::peer_connection::media_egress::Snapshot,
) {
    eprintln!(
        "{}",
        json!({"version":1,"event":"media_egress_fence","fence_id":fence_id,"phase":phase,"elapsed_ms":started.elapsed().as_millis(),"counters":{"enabled":counters.enabled,"fenced":counters.fenced,"failed":counters.failed,"media_submitted_datagrams":counters.media_submitted_datagrams,"control_submitted_datagrams":counters.control_submitted_datagrams,"mdns_send_errors":counters.mdns_send_errors,"blocked_media_datagrams":counters.blocked_media_datagrams,"rejected_datagrams":counters.rejected_datagrams,"rejection_reason":counters.rejection_reason,"pending_media_send":counters.pending_media_send,"au_deadlines_enabled":counters.au_deadlines_enabled,"registered_access_units":counters.registered_access_units,"au_records":counters.au_records,"au_records_high_water":counters.au_records_high_water,"expired_datagrams":counters.expired_datagrams,"completion_overrun_datagrams":counters.completion_overrun_datagrams,"unknown_au_datagrams":counters.unknown_au_datagrams,"policy_stop_reason":counters.policy_stop_reason,"first_expiry_stage":counters.first_expiry_stage,"first_expiry_turn":counters.first_expiry_turn,"first_expiry_first_poll_remaining_us":counters.first_expiry_first_poll_remaining_us,"first_expiry_socket_pending_polls":counters.first_expiry_socket_pending_polls,"first_expiry_send_elapsed_us":counters.first_expiry_send_elapsed_us,"fault_au_hold_armed":counters.fault_au_hold_armed,"fault_au_hold_triggered":counters.fault_au_hold_triggered,"fault_au_hold_after_first_submit_ms":counters.fault_au_hold_after_first_submit_ms}})
    );
}
async fn watch_media_egress(
    gate: &webrtc::peer_connection::media_egress::MediaEgress,
) -> Result<()> {
    loop {
        let snapshot = gate.snapshot();
        if snapshot.policy_stop_reason.is_some() {
            bail!("local AU submission deadline expired");
        }
        if snapshot.failed {
            bail!("media egress framing or transport unsupported");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
async fn end_session_input(app: &App, id: &str) -> bool {
    let (input, files) = app
        .active
        .lock()
        .await
        .active
        .as_ref()
        .filter(|s| s.id == id)
        .map(|s| (s.input.clone(), s.files.clone()))
        .unwrap_or((None, None));
    if let Some(input) = input {
        input.end();
    }
    match files {
        Some(files) => files.end().await.is_ok(),
        None => true,
    }
}
fn cleanup_attested_exit(code: Option<i32>, failure_exit: Option<i32>) -> bool {
    code == Some(0) || (code.is_some() && code == failure_exit)
}

// RFC6184 Table5 subprofile matching; never accept a library's MIME-only fallback.
fn native_codec_matches(codec: &str, mime: &str, fmtp: &str) -> bool {
    let mut fields = BTreeMap::new();
    for field in fmtp.split(';').map(str::trim).filter(|v| !v.is_empty()) {
        let Some((k, v)) = field.split_once('=') else {
            return false;
        };
        let key = k.trim().to_ascii_lowercase();
        if key.is_empty() || v.trim().is_empty() || fields.insert(key, v.trim()).is_some() {
            return false;
        }
    }
    if codec == "h265" {
        return mime.eq_ignore_ascii_case("video/H265")
            && fields.get("profile-space").copied().unwrap_or("0") == "0"
            && fields.get("profile-id").copied().unwrap_or("1") == "1"
            && fields.get("tier-flag").copied().unwrap_or("0") == "0"
            && fields.get("tx-mode").copied().unwrap_or("SRST") == "SRST"
            && fields
                .get("level-id")
                .filter(|v| v.len() <= 3 && v.bytes().all(|c| c.is_ascii_digit()))
                .and_then(|v| v.parse::<u16>().ok())
                .is_some_and(|v| [120, 123, 150, 153, 156, 180, 183, 186].contains(&v));
    }
    if !mime.eq_ignore_ascii_case("video/H264") || fields.get("packetization-mode") != Some(&"1") {
        return false;
    }
    let Some(profile) = fields.get("profile-level-id") else {
        return false;
    };
    if profile.len() != 6 {
        return false;
    }
    let Ok(n) = u32::from_str_radix(profile, 16) else {
        return false;
    };
    let id = (n >> 16) as u8;
    let flags = (n >> 8) as u8;
    let level = n as u8;
    let cb = match id {
        0x42 => flags & 0x4f == 0x40,
        0x4d => flags & 0x8f == 0x80,
        0x58 => flags & 0xcf == 0xc0,
        _ => false,
    };
    cb && [31, 32, 40, 41, 42, 50, 51, 52, 60, 61, 62].contains(&level)
}
fn parse_au_hold_option(args: &[String]) -> Result<(bool, Vec<String>)> {
    let mut enabled = false;
    let mut remaining = Vec::new();
    for arg in args {
        if arg == "--fault-au-egress-hold" {
            anyhow::ensure!(!enabled, "duplicate AU egress fault flag");
            enabled = true;
        } else {
            remaining.push(arg.clone());
        }
    }
    Ok((enabled, remaining))
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            if error.is::<CleanupUnconfirmedExit>() {
                eprintln!(
                    "{}",
                    json!({"version":1,"event":"media_cleanup_unconfirmed_exit","grace_ms":CLEANUP_CONFIRMATION_GRACE.as_millis(),"exit_code":CLEANUP_UNCONFIRMED_EXIT_CODE})
                );
            } else {
                eprintln!("Error: {error:?}");
            }
            std::process::ExitCode::from(service_error_exit_code(&error))
        }
    }
}
async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|v| v == "--pair") {
        return vm_session_auth::cli::run_cli(&args[2..]);
    }
    if args.get(1).is_some_and(|v| v == "remote") {
        return remote::run(&args[2..]).await;
    }
    if args
        .get(1)
        .is_some_and(|v| v == "--print-pinned-client-manifest")
    {
        anyhow::ensure!(
            args.len() == 3,
            "usage: vm-media-local --print-pinned-client-manifest BROWSER_DIR"
        );
        print!(
            "{}",
            pinned_client::manifest_for(&PathBuf::from(&args[2]).canonicalize()?)?
        );
        return Ok(());
    }
    if args.len() < 5 {
        bail!(
            "usage: vm-media-local ENCODER SOCKET BROWSER_DIR PORT [--guest-stream [--guest-binding-state DIR --guest-binding-vm ID --guest-worker-instance HEX --guest-bridge-instance HEX [--guest-cleanup-bridge PRIVATE_JSON]]] [--auth-state DIR --auth-namespace NAME --vm ID [--control-state DIR] [--ice-config PRIVATE_JSON]] [--fault-drop-first-keyframe] [--fault-suppress-hevc-frames] [--fault-au-egress-hold] [--pinned-client MANIFEST --pinned-client-sha256 HEX] [--encoder-rate-control | --encoder-rate-trial HIGH,LOW,HIGH | --encoder-loss-response [--fault-rtp-loss]]"
        )
    }
    let (pinned_options, pinned_remaining) = pinned_client::parse_options(&args[5..])?;
    let (encoder_rate, rate_remaining) = encoder_rate::parse_options(&pinned_remaining)?;
    let (fault_au_egress_hold, rate_remaining) = parse_au_hold_option(&rate_remaining)?;
    let (binding_options, remaining_options) = guest_binding::parse_options(&rate_remaining)?;
    let authentication = auth::from_args(&remaining_options)?;
    let broker_mode = args[1] == "--console";
    anyhow::ensure!(
        broker_mode
            || authentication
                .as_ref()
                .is_none_or(|a| a.control_state.is_none()),
        "--control-state requires production --console mode"
    );
    let encoder = if broker_mode {
        std::env::current_exe()?
            .with_file_name("vm-console-stream")
            .canonicalize()?
    } else {
        PathBuf::from(&args[1]).canonicalize()?
    };
    let socket = PathBuf::from(&args[2]).canonicalize()?;
    let web = PathBuf::from(&args[3]).canonicalize()?;
    // Verified before the encoder probe and before any socket is bound: a client
    // that does not match its pins must not reach a listening state.
    let pinned_bundle = pinned_options
        .map(|options| pinned_client::Bundle::load(&web, &options).map(Arc::new))
        .transpose()?;
    let mut probe = tokio::process::Command::new(&encoder);
    probe.arg("--probe-codecs").kill_on_drop(true);
    if broker_mode {
        probe.arg(&socket);
    }
    let output = tokio::time::timeout(Duration::from_secs(8), probe.output()).await??;
    if !output.status.success() || output.stdout.len() > 8192 {
        bail!("native hardware capability probe failed")
    }
    let capabilities: Value = serde_json::from_slice(&output.stdout)?;
    if let Some(options) = &encoder_rate {
        options.check_capabilities(&capabilities)?;
    }
    let cleanup_failure_exit = if args[5..].iter().any(|a| a == "--guest-stream") {
        anyhow::ensure!(
            capabilities["protocol"] == "veynode-guest-video-stream-v1"
                && capabilities["cleanup_contract_version"] == 1
                && capabilities["cleanup_confirmed_failure_exit"] == 78,
            "Guest encoder cleanup contract missing or incompatible"
        );
        Some(78)
    } else {
        None
    };
    let guest_binding = if let Some(options) = binding_options {
        let auth = authentication
            .as_ref()
            .context("Guest binding requires signed authentication")?;
        anyhow::ensure!(!broker_mode, "Guest binding cannot use QEMU console broker");
        Some(Arc::new(
            guest_binding::Source::open(
                options,
                &socket,
                &auth.vm_id,
                auth.control_state.is_some(),
                cleanup_failure_exit.is_some(),
                &capabilities,
            )
            .await?,
        ))
    } else {
        None
    };
    anyhow::ensure!(
        !authentication.as_ref().is_some_and(|a| a.files_enabled) || guest_binding.is_some(),
        "--allow-files requires authenticated strict Guest binding"
    );
    let shutdown = shutdown_signal::install()?;
    let listener =
        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, args[4].parse::<u16>()?))
            .await?;
    let host = listener.local_addr()?.to_string();
    let origin = format!("http://{host}");
    let app = App {
        host,
        origin: origin.clone(),
        encoder,
        socket,
        web,
        capabilities,
        fault_au_egress_hold,
        fault_drop_first_keyframe: args[5..].iter().any(|a| a == "--fault-drop-first-keyframe"),
        fault_suppress_hevc_frames: args[5..]
            .iter()
            .any(|a| a == "--fault-suppress-hevc-frames"),
        active: Arc::new(Mutex::new(SessionBook::default())),
        auth: authentication,
        cleanup_failure_exit,
        guest_binding,
        encoder_rate,
    };
    let shutdown_app = app.clone();
    let router = Router::new()
        .route("/capabilities", get(get_capabilities))
        .route("/auth/challenge", post(auth::challenge))
        .route("/auth/ice-config", post(auth::ice_configuration))
        .route("/auth/offer", post(auth::offer))
        .route("/auth/confirm", post(auth::confirm))
        .route("/auth/close", post(auth::close))
        .route("/offer", post(offer))
        .route("/close", post(close));
    let pinned_digest = pinned_bundle
        .as_ref()
        .map(|b| b.manifest_sha256().to_owned());
    let router = if let Some(bundle) = pinned_bundle {
        // Pinned mode: no route reads BROWSER_DIR; see pinned_client.rs.
        pinned_client::mount(router, bundle)
    } else {
        router
            .route(
                "/",
                get(|State(a), h| async {
                    asset(a, h, "native.html", "text/html; charset=utf-8").await
                }),
            )
            .route(
                "/dist/native.js",
                get(|State(a), h| async { asset(a, h, "dist/native.js", "text/javascript").await }),
            )
            .route(
                "/dist/codec-policy.js",
                get(|State(a), h| async {
                    asset(a, h, "dist/codec-policy.js", "text/javascript").await
                }),
            )
            .route(
                "/dist/native-auth.js",
                get(|State(a), h| async {
                    asset(a, h, "dist/native-auth.js", "text/javascript").await
                }),
            )
            .route(
                "/dist/session-auth.js",
                get(|State(a), h| async {
                    asset(a, h, "dist/session-auth.js", "text/javascript").await
                }),
            )
            .route(
                "/dist/control-channel.js",
                get(|State(a): State<App>, h| async move {
                    asset(a, h, "dist/control-channel.js", "text/javascript").await
                }),
            )
            .route(
                "/dist/ice-config.js",
                get(|State(a), h| async {
                    asset(a, h, "dist/ice-config.js", "text/javascript").await
                }),
            )
            .route(
                "/dist/feedback-evidence.js",
                get(|State(a), h| async {
                    asset(a, h, "dist/feedback-evidence.js", "text/javascript").await
                }),
            )
            .route(
                "/dist/session-files.js",
                get(|State(a), h| async {
                    asset(a, h, "dist/session-files.js", "text/javascript").await
                }),
            )
            .route(
                "/dist/session-control.js",
                get(|State(a), h| async {
                    asset(a, h, "dist/session-control.js", "text/javascript").await
                }),
            )
            .route(
                "/probe.css",
                get(|State(a), h| async { asset(a, h, "probe.css", "text/css").await }),
            )
    };
    let router = router
        .layer(DefaultBodyLimit::max(256 * 1024))
        .with_state(app);
    let mut ready = json!({"url":origin,"scope":"localhost-native-VM-WebRTC-diagnostic","internet_authentication":false,"fault_drop_first_keyframe":args[5..].iter().any(|a| a == "--fault-drop-first-keyframe"),"fault_suppress_hevc_frames":args[5..].iter().any(|a| a == "--fault-suppress-hevc-frames")});
    if let Some(digest) = pinned_digest {
        ready["pinned_client_manifest_sha256"] = json!(digest);
    }
    println!("{ready}");
    serve_with_shutdown(
        listener,
        router,
        &shutdown_app,
        shutdown,
        Duration::from_secs(12),
    )
    .await?;
    Ok(())
}

// Start only after the session (including its bounded teardown) has finished.
// Allow another full 12 s close/shutdown budget for a late cleanup confirmation.
// Polling adds at most 100 ms before the grace starts and before it is observed.
const CLEANUP_CONFIRMATION_GRACE: Duration = Duration::from_secs(12);
const CLEANUP_CHECK_INTERVAL: Duration = Duration::from_millis(100);
const CLEANUP_UNCONFIRMED_EXIT_CODE: u8 = 75;

#[derive(Debug)]
struct CleanupUnconfirmedExit;
impl std::fmt::Display for CleanupUnconfirmedExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("idle session cleanup unconfirmed after grace")
    }
}
impl std::error::Error for CleanupUnconfirmedExit {}
fn service_error_exit_code(error: &anyhow::Error) -> u8 {
    if error.is::<CleanupUnconfirmedExit>() {
        CLEANUP_UNCONFIRMED_EXIT_CODE
    } else {
        1
    }
}

async fn wait_for_unconfirmed_cleanup(app: &App, grace: Duration) -> bool {
    let mut pending_since: Option<(String, tokio::time::Instant)> = None;
    loop {
        {
            let mut book = app.active.lock().await;
            if book.stopping {
                return false;
            }
            let pending = book.completed.iter().find(|(_, (_, clean, _))| !*clean);
            if let Some((request, _)) = pending.filter(|_| book.active.is_none()) {
                // A different failure gets its own full grace even if a clean
                // receipt and another short session both fell between polls.
                if pending_since.as_ref().is_none_or(|(id, _)| id != request) {
                    pending_since = Some((request.clone(), tokio::time::Instant::now()));
                }
                let (_, since) = pending_since.as_ref().unwrap();
                if since.elapsed() >= grace {
                    // Recheck and fence admission under the same lock. A confirmed
                    // cleanup or a new active session can never race this decision.
                    book.stopping = true;
                    return true;
                }
            } else {
                pending_since = None;
            }
        }
        tokio::time::sleep(CLEANUP_CHECK_INTERVAL).await;
    }
}

// Mark stopping and capture the only session under the same lock as admission.
// Nothing can occupy the just-vacated slot after this transition.
async fn begin_shutdown(app: &App) -> Option<watch::Receiver<Option<bool>>> {
    let mut book = app.active.lock().await;
    book.stopping = true;
    book.active.as_ref().map(|session| {
        let _ = session.cancel.send(true);
        session.finished.clone()
    })
}
async fn await_shutdown_cleanup(
    finished: Option<watch::Receiver<Option<bool>>>,
    deadline: Duration,
) -> Result<()> {
    if let Some(finished) = finished {
        let clean = tokio::time::timeout(deadline, completion(finished))
            .await
            .context("shutdown cleanup deadline exceeded")?;
        anyhow::ensure!(clean, "shutdown cleanup unconfirmed");
    }
    Ok(())
}
async fn serve_with_shutdown<F>(
    listener: tokio::net::TcpListener,
    router: Router,
    app: &App,
    signal: F,
    deadline: Duration,
) -> Result<()>
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    use std::future::IntoFuture;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = stop_rx.await;
        })
        .into_future();
    drain_with_shutdown(server, stop_tx, app, signal, deadline).await
}
async fn drain_with_shutdown<S, F>(
    server: S,
    stop_tx: tokio::sync::oneshot::Sender<()>,
    app: &App,
    signal: F,
    deadline: Duration,
) -> Result<()>
where
    S: std::future::Future<Output = std::io::Result<()>>,
    F: std::future::Future<Output = std::io::Result<()>>,
{
    tokio::pin!(server);
    let (early_result, signal_result, cleanup_expired) = tokio::select! {
        biased;
        result = signal => (None, result, false),
        result = &mut server => (Some(result), Ok(()), false),
        expired = wait_for_unconfirmed_cleanup(app, CLEANUP_CONFIRMATION_GRACE) => (None, Ok(()), expired),
    };
    let finished = begin_shutdown(app).await;
    // Resolve Axum's shutdown future immediately. Never wait for Guest cleanup
    // while leaving the listening socket open to new requests.
    let _ = stop_tx.send(());
    let drain = async {
        if let Some(result) = early_result {
            result?;
            bail!("HTTP server ended before shutdown signal");
        }
        tokio::time::timeout(deadline, &mut server)
            .await
            .context("HTTP shutdown drain deadline exceeded")??;
        Ok::<(), anyhow::Error>(())
    };
    // Existing partial-body/keepalive handlers cannot hold process shutdown open.
    // Both paths get time to complete even if the other fails immediately.
    let (cleanup, drained) = tokio::join!(await_shutdown_cleanup(finished, deadline), drain);
    let cleanup_unconfirmed = app
        .active
        .lock()
        .await
        .completed
        .values()
        .any(|(_, clean, _)| !*clean);
    if cleanup_expired && cleanup_unconfirmed {
        // Recovery can finish during HTTP draining. Only unresolved cleanup at
        // exit warrants the distinct restart status, including a drain timeout.
        return Err(CleanupUnconfirmedExit.into());
    }
    signal_result.context("shutdown signal failed")?;
    cleanup?;
    drained?;
    anyhow::ensure!(
        !cleanup_unconfirmed,
        "shutdown has unresolved earlier cleanup"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn nack_cleanup_requires_actual_closed_and_empty_all_resources() {
        use rtc::sansio::Protocol;
        let (mut responder, monitor) = webrtc::bounded_nack::BoundedNack::new();
        assert!(!super::nack_cleanup_attested(&monitor.snapshot()));
        responder.close().unwrap();
        let clean = monitor.snapshot();
        assert!(super::nack_cleanup_attested(&clean));
        for index in 0..10 {
            let mut dirty = clean.clone();
            let c = &mut dirty.counters;
            match index {
                0 => c.closed = false,
                1 => c.bound = true,
                2 => c.cache_packets = 1,
                3 => c.cache_bytes = 1,
                4 => c.pending_repair_packets = 1,
                5 => c.pending_repair_bytes = 1,
                6 => c.pending_forward_packets = 1,
                7 => c.pending_forward_bytes = 1,
                8 => c.pending_read_packets = 1,
                _ => c.pending_read_bytes = 1,
            }
            assert!(!super::nack_cleanup_attested(&dirty));
        }
    }
    #[test]
    fn nack_diagnostics_use_fixed_budget_fields() {
        let (_, monitor) = webrtc::bounded_nack::BoundedNack::new();
        let value = super::nack_budget_value(&monitor.snapshot());
        assert_eq!(value["caps"]["cache_bytes"], 1048576);
        assert_eq!(value["caps"]["repair_bytes"], 131072);
        assert_eq!(value["caps"]["cached_packet_bytes"], 2048);
        assert_eq!(value["caps"].as_object().unwrap().len(), 11);
        assert!(value["failure_reason"].is_null());
        assert_eq!(value["closed"], false);
        assert_eq!(value["repair_expired_before_enqueue"], 0);
        assert_eq!(value["repair_expired_before_dequeue"], 0);
    }

    use super::*;
    fn grace_fixture() -> (Handler, watch::Receiver<bool>, watch::Receiver<bool>) {
        let (gathered, _) = watch::channel(false);
        let (connected, connected_rx) = watch::channel(false);
        let (lost, lost_rx) = watch::channel(false);
        let handler = Handler {
            gathered,
            connected,
            lost,
            control: None,
            disconnect_grace: Default::default(),
        };
        (handler, connected_rx, lost_rx)
    }
    async fn elapse(ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        tokio::task::yield_now().await;
    }
    #[tokio::test(start_paused = true)]
    async fn disconnected_is_terminal_only_after_the_five_second_grace() {
        assert_eq!(DISCONNECT_GRACE, Duration::from_secs(5));
        let (handler, _connected, lost) = grace_fixture();
        handler
            .on_connection_state_change(RTCPeerConnectionState::Connected)
            .await;
        handler
            .on_connection_state_change(RTCPeerConnectionState::Disconnected)
            .await;
        assert!(!*lost.borrow(), "Disconnected is not terminal at once");
        elapse(4_999).await;
        assert!(!*lost.borrow(), "not terminal before 5 s");
        elapse(2).await;
        assert!(*lost.borrow(), "terminal once 5 s passed without Connected");
    }
    #[tokio::test(start_paused = true)]
    async fn reconnect_within_grace_cancels_and_a_later_disconnect_gets_a_fresh_grace() {
        let (handler, connected, lost) = grace_fixture();
        handler
            .on_connection_state_change(RTCPeerConnectionState::Disconnected)
            .await;
        elapse(4_999).await;
        handler
            .on_connection_state_change(RTCPeerConnectionState::Connected)
            .await;
        assert!(*connected.borrow());
        elapse(60_000).await;
        assert!(!*lost.borrow(), "reconnect cancelled the grace timer");
        handler
            .on_connection_state_change(RTCPeerConnectionState::Disconnected)
            .await;
        elapse(4_999).await;
        assert!(!*lost.borrow());
        elapse(2).await;
        assert!(*lost.borrow());
    }
    #[tokio::test(start_paused = true)]
    async fn repeated_disconnected_or_non_connected_states_never_extend_the_grace() {
        let (handler, _connected, lost) = grace_fixture();
        handler
            .on_connection_state_change(RTCPeerConnectionState::Disconnected)
            .await;
        elapse(3_000).await;
        handler
            .on_connection_state_change(RTCPeerConnectionState::Disconnected)
            .await;
        handler
            .on_connection_state_change(RTCPeerConnectionState::Connecting)
            .await;
        elapse(1_999).await;
        assert!(!*lost.borrow());
        elapse(2).await;
        assert!(
            *lost.borrow(),
            "grace is measured from the first Disconnected"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn failed_and_closed_remain_immediately_terminal_also_during_grace() {
        for state in [
            RTCPeerConnectionState::Failed,
            RTCPeerConnectionState::Closed,
        ] {
            let (handler, _connected, lost) = grace_fixture();
            handler.on_connection_state_change(state).await;
            assert!(*lost.borrow());
            let (handler, _connected, lost) = grace_fixture();
            handler
                .on_connection_state_change(RTCPeerConnectionState::Disconnected)
                .await;
            elapse(1_000).await;
            assert!(!*lost.borrow());
            handler.on_connection_state_change(state).await;
            assert!(*lost.borrow());
        }
    }
    fn header(n: u32, pts: u64, key: u8) -> [u8; 13] {
        let mut h = [0; 13];
        h[..4].copy_from_slice(&n.to_be_bytes());
        h[4..12].copy_from_slice(&pts.to_be_bytes());
        h[12] = key;
        h
    }
    #[tokio::test]
    async fn close_requires_cleanup_attestation_even_after_session_exit() {
        let request = uuid::Uuid::new_v4().to_string();
        let app = App {
            origin: "http://127.0.0.1:8787".into(),
            host: "127.0.0.1:8787".into(),
            encoder: PathBuf::new(),
            socket: PathBuf::new(),
            web: PathBuf::new(),
            capabilities: json!({}),
            fault_au_egress_hold: false,
            fault_drop_first_keyframe: false,
            fault_suppress_hevc_frames: false,
            active: Arc::new(Mutex::new(SessionBook::default())),
            auth: None,
            cleanup_failure_exit: None,
            guest_binding: None,
            encoder_rate: None,
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, app.host.parse().unwrap());
        headers.insert(header::ORIGIN, app.origin.parse().unwrap());
        let (cancel, _) = watch::channel(false);
        let (done, finished) = watch::channel(None);
        app.active.lock().await.active = Some(Session {
            request: request.clone(),
            id: "owned".into(),
            cancel,
            finished,
            confirmed: watch::channel(false).0,
            input: None,
            files: None,
        });
        // A task disappearing without a final value is not proof of cleanup.
        drop(done);
        let reply = close(
            State(app.clone()),
            headers.clone(),
            Json(Close {
                request_id: request.clone(),
                session_id: Some("owned".into()),
            }),
        )
        .await;
        assert_eq!(reply.status(), StatusCode::GATEWAY_TIMEOUT);
        {
            let mut book = app.active.lock().await;
            book.active = None;
            book.completed
                .insert(request.clone(), (Instant::now(), false, "owned".into()));
        }
        let reply = close(
            State(app.clone()),
            headers.clone(),
            Json(Close {
                request_id: request.clone(),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(reply.status(), StatusCode::GATEWAY_TIMEOUT);
        app.active
            .lock()
            .await
            .completed
            .insert(request.clone(), (Instant::now(), true, "owned".into()));
        let reply = close(
            State(app),
            headers,
            Json(Close {
                request_id: request,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(reply.status(), StatusCode::OK);
    }
    #[tokio::test]
    async fn failed_cleanup_never_ages_into_success_or_new_admission() {
        let mut app = shutdown_fixture();
        app.capabilities = json!({"h264":"verified-hardware"});
        let request = uuid::Uuid::new_v4().to_string();
        app.active.lock().await.completed.insert(
            request.clone(),
            (
                Instant::now() - Duration::from_secs(600),
                false,
                "old".into(),
            ),
        );
        let response = close_inner(
            &app,
            Close {
                request_id: request.clone(),
                session_id: Some("old".into()),
            },
        )
        .await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(app.active.lock().await.completed.contains_key(&request));
        let response = offer_inner(
            app.clone(),
            Offer {
                request_id: uuid::Uuid::new_v4().to_string(),
                sdp: String::new(),
                r#type: "offer".into(),
                codec: "h264".into(),
            },
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        assert!(serve_with_shutdown(
            listener,
            Router::new(),
            &app,
            async { Ok(()) },
            Duration::from_millis(100)
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("unresolved earlier cleanup"));
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }
    #[test]
    fn cleanup_failure_exit_is_opt_in_and_not_a_media_success() {
        assert!(cleanup_attested_exit(Some(0), None));
        assert!(cleanup_attested_exit(Some(78), Some(78)));
        for (code, contract) in [
            (None, None),
            (None, Some(78)),
            (Some(1), Some(78)),
            (Some(78), None),
        ] {
            assert!(!cleanup_attested_exit(code, contract));
        }
    }
    #[test]
    fn cancellation_before_offer_is_retained_and_bounded() {
        let mut book = SessionBook::default();
        assert!(book.cancel_id("cancelled-before-offer"));
        assert!(book.cancelled.contains_key("cancelled-before-offer"));
        for i in 0..255 {
            assert!(book.cancel_id(&i.to_string()));
        }
        assert!(!book.cancel_id("overflow"));
        assert!(book.cancel_id("cancelled-before-offer"));
        book.cancelled.insert(
            "cancelled-before-offer".into(),
            Instant::now() - Duration::from_secs(61),
        );
        book.prune();
        assert!(!book.cancelled.contains_key("cancelled-before-offer"));
    }
    #[test]
    fn negotiated_profile_requires_encoder_compatible_subprofile_and_level() {
        for id in ["42e01f", "42c01f", "4d801f", "58c01f"] {
            assert!(native_codec_matches(
                "h264",
                "video/H264",
                &format!("packetization-mode=1;profile-level-id={id}")
            ));
        }
        for id in ["4d001f", "640c1f", "42e01e", "42e0ff", "zzzzzz"] {
            assert!(!native_codec_matches(
                "h264",
                "video/H264",
                &format!("packetization-mode=1;profile-level-id={id}")
            ));
        }
        assert!(!native_codec_matches(
            "h264",
            "video/H264",
            "packetization-mode=0;profile-level-id=42e01f"
        ));
        assert!(!native_codec_matches(
            "h264",
            "video/H264",
            "packetization-mode=1;profile-level-id=42e01f;profile-level-id=640c1f"
        ));
        assert!(native_codec_matches(
            "h265",
            "video/H265",
            "profile-id=1;tier-flag=0;level-id=120;tx-mode=SRST"
        ));
        assert!(!native_codec_matches(
            "h265",
            "video/H265",
            "profile-id=2;tier-flag=0;level-id=120;tx-mode=SRST"
        ));
        assert!(native_codec_matches("h265", "video/H265", "level-id=120"));
        for fmtp in [
            "level-id=93",
            "level-id=121",
            "level-id=+120",
            "level-id=120;profile-space=1",
            "level-id=120;tier-flag=1",
            "level-id=120;tx-mode=MRST",
            "level-id=120;PROFILE-ID=1;profile-id=1",
            "level-id=120;broken",
        ] {
            assert!(!native_codec_matches("h265", "video/H265", fmtp));
        }
    }
    #[test]
    fn malformed_frame_is_rejected_before_allocation() {
        for h in [
            header(0, 0, 1),
            header(4 * 1024 * 1024 + 1, 0, 1),
            header(99, 0, 2),
            header(99, 1, 1),
        ] {
            assert!(frame_header(h, None).is_err());
        }
        assert_eq!(
            frame_header(header(123, 0, 1), None).unwrap(),
            (123, 0, true)
        );
        assert!(frame_header(header(123, 42, 0), Some(42)).is_err());
        assert!(frame_header(header(123, 41, 0), Some(42)).is_err());
        assert!(frame_header(header(123, 10_000_043, 0), Some(42)).is_err());
        assert_eq!(
            frame_header(header(123, 33_333, 0), Some(0)).unwrap().1,
            33_333
        );
    }
    #[test]
    fn loopback_mutations_require_exact_host_and_origin() {
        let app = App {
            origin: "http://127.0.0.1:1234".into(),
            host: "127.0.0.1:1234".into(),
            encoder: PathBuf::new(),
            socket: PathBuf::new(),
            web: PathBuf::new(),
            capabilities: Value::Null,
            fault_au_egress_hold: false,
            fault_drop_first_keyframe: false,
            fault_suppress_hevc_frames: false,
            active: Arc::new(Mutex::new(SessionBook::default())),
            auth: None,
            cleanup_failure_exit: None,
            guest_binding: None,
            encoder_rate: None,
        };
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "127.0.0.1:1234".parse().unwrap());
        assert!(check(&app, &h, false).is_ok());
        assert!(check(&app, &h, true).is_err());
        h.insert(header::ORIGIN, "https://attacker.example".parse().unwrap());
        assert!(check(&app, &h, true).is_err());
        h.insert(header::ORIGIN, app.origin.parse().unwrap());
        assert!(check(&app, &h, true).is_ok());
        h.insert(header::HOST, "attacker.example".parse().unwrap());
        assert!(check(&app, &h, false).is_err());
    }
    fn shutdown_fixture() -> App {
        App {
            origin: "http://127.0.0.1:1".into(),
            host: "127.0.0.1:1".into(),
            encoder: PathBuf::from("must-not-start"),
            socket: PathBuf::new(),
            web: PathBuf::new(),
            capabilities: json!({"h264":"verified-hardware"}),
            fault_au_egress_hold: false,
            fault_drop_first_keyframe: false,
            fault_suppress_hevc_frames: false,
            active: Arc::new(Mutex::new(SessionBook::default())),
            auth: None,
            cleanup_failure_exit: None,
            guest_binding: None,
            encoder_rate: None,
        }
    }
    async fn cleanup_watchdog_fixture() -> (App, String) {
        let app = shutdown_fixture();
        let request = uuid::Uuid::new_v4().to_string();
        app.active
            .lock()
            .await
            .completed
            .insert(request.clone(), (Instant::now(), false, "ended".into()));
        (app, request)
    }
    fn spawn_cleanup_watchdog(app: &App) -> tokio::task::JoinHandle<()> {
        let app = app.clone();
        tokio::spawn(async move {
            wait_for_unconfirmed_cleanup(&app, Duration::from_millis(300)).await;
        })
    }
    #[tokio::test(start_paused = true)]
    async fn cleanup_watchdog_exits_only_after_idle_grace() {
        assert_eq!(CLEANUP_CONFIRMATION_GRACE, Duration::from_secs(12));
        let (app, _) = cleanup_watchdog_fixture().await;
        let watchdog = spawn_cleanup_watchdog(&app);
        tokio::task::yield_now().await;
        elapse(299).await;
        assert!(!watchdog.is_finished());
        assert!(!app.active.lock().await.stopping);
        elapse(101).await;
        assert!(watchdog.is_finished());
        watchdog.await.unwrap();
        let book = app.active.lock().await;
        assert!(book.stopping);
        assert!(book.active.is_none());
        assert!(book.completed.values().any(|(_, clean, _)| !*clean));
    }
    #[tokio::test(start_paused = true)]
    async fn cleanup_watchdog_active_session_prevents_exit_and_resets_grace() {
        let (app, _) = cleanup_watchdog_fixture().await;
        let watchdog = spawn_cleanup_watchdog(&app);
        tokio::task::yield_now().await;
        elapse(100).await;
        let (cancel, cancelled) = watch::channel(false);
        app.active.lock().await.active = Some(Session {
            request: "active".into(),
            id: "active".into(),
            cancel,
            finished: watch::channel(None).1,
            confirmed: watch::channel(true).0,
            input: None,
            files: None,
        });
        elapse(1_000).await;
        assert!(!watchdog.is_finished());
        assert!(!app.active.lock().await.stopping);
        assert!(!*cancelled.borrow());
        app.active.lock().await.active = None;
        elapse(299).await;
        assert!(
            !watchdog.is_finished(),
            "active time cannot consume idle grace"
        );
        elapse(201).await;
        assert!(watchdog.is_finished());
        watchdog.await.unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn cleanup_watchdog_confirmation_inside_grace_cancels_exit() {
        let (app, request) = cleanup_watchdog_fixture().await;
        let watchdog = spawn_cleanup_watchdog(&app);
        tokio::task::yield_now().await;
        elapse(299).await;
        app.active
            .lock()
            .await
            .completed
            .get_mut(&request)
            .unwrap()
            .1 = true;
        elapse(1_000).await;
        assert!(!watchdog.is_finished());
        assert!(!app.active.lock().await.stopping);
        watchdog.abort();
        assert!(watchdog.await.unwrap_err().is_cancelled());
    }
    #[tokio::test(start_paused = true)]
    async fn cleanup_watchdog_confirmed_cleanup_never_arms_exit() {
        let (app, request) = cleanup_watchdog_fixture().await;
        app.active
            .lock()
            .await
            .completed
            .get_mut(&request)
            .unwrap()
            .1 = true;
        let watchdog = spawn_cleanup_watchdog(&app);
        elapse(1_000).await;
        assert!(!watchdog.is_finished());
        assert!(!app.active.lock().await.stopping);
        watchdog.abort();
        assert!(watchdog.await.unwrap_err().is_cancelled());
    }
    #[tokio::test(start_paused = true)]
    async fn cleanup_watchdog_different_failure_gets_full_grace() {
        let (app, request) = cleanup_watchdog_fixture().await;
        let watchdog = spawn_cleanup_watchdog(&app);
        tokio::task::yield_now().await;
        elapse(299).await;
        {
            let mut book = app.active.lock().await;
            book.completed.get_mut(&request).unwrap().1 = true;
            book.completed
                .insert("another".into(), (Instant::now(), false, "another".into()));
        }
        elapse(299).await;
        assert!(!watchdog.is_finished());
        elapse(201).await;
        assert!(watchdog.is_finished());
        watchdog.await.unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn cleanup_watchdog_pending_still_refuses_new_sessions() {
        let (app, _) = cleanup_watchdog_fixture().await;
        let watchdog = spawn_cleanup_watchdog(&app);
        tokio::task::yield_now().await;
        let response = offer_inner(
            app.clone(),
            Offer {
                request_id: uuid::Uuid::new_v4().to_string(),
                sdp: String::new(),
                r#type: "offer".into(),
                codec: "h264".into(),
            },
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"previous session cleanup unconfirmed");
        assert!(app.active.lock().await.active.is_none());
        assert!(!watchdog.is_finished());
        watchdog.abort();
        assert!(watchdog.await.unwrap_err().is_cancelled());
    }
    #[test]
    fn cleanup_watchdog_exit_status_is_distinct_from_other_errors() {
        assert_eq!(service_error_exit_code(&CleanupUnconfirmedExit.into()), 75);
        assert_eq!(service_error_exit_code(&anyhow!("ordinary error")), 1);
        assert_eq!(
            service_error_exit_code(&anyhow::Error::new(CleanupUnconfirmedExit).context("drain")),
            75
        );
    }
    #[tokio::test]
    async fn shutdown_requires_actual_cleanup_and_bounds_missing_completion() {
        let deadline = Duration::from_millis(20);
        await_shutdown_cleanup(None, deadline).await.unwrap();
        let (done, finished) = watch::channel(Some(true));
        await_shutdown_cleanup(Some(finished), deadline)
            .await
            .unwrap();
        drop(done);
        let (done, finished) = watch::channel(Some(false));
        assert!(await_shutdown_cleanup(Some(finished), deadline)
            .await
            .unwrap_err()
            .to_string()
            .contains("unconfirmed"));
        drop(done);
        let (done, finished) = watch::channel(None);
        drop(done);
        assert!(await_shutdown_cleanup(Some(finished), deadline)
            .await
            .unwrap_err()
            .to_string()
            .contains("unconfirmed"));
        let (_done, finished) = watch::channel(None);
        assert!(await_shutdown_cleanup(Some(finished), deadline)
            .await
            .unwrap_err()
            .to_string()
            .contains("deadline"));
    }
    #[tokio::test]
    async fn shutdown_stops_real_listener_and_bounds_partial_body_handler() {
        let app = shutdown_fixture();
        let entered = Arc::new(tokio::sync::Notify::new());
        let notification = entered.clone();
        let router = Router::new().route(
            "/slow",
            post(move |request: axum::http::Request<axum::body::Body>| {
                let notification = notification.clone();
                async move {
                    notification.notify_one();
                    let _ = axum::body::to_bytes(request.into_body(), 1024).await;
                    StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, signal) = tokio::sync::oneshot::channel::<()>();
        let running_app = app.clone();
        let task = tokio::spawn(async move {
            serve_with_shutdown(
                listener,
                router,
                &running_app,
                async {
                    let _ = signal.await;
                    Ok(())
                },
                Duration::from_millis(50),
            )
            .await
        });
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"POST /slow HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\nx")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        shutdown.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("HTTP shutdown drain deadline"));
        assert!(app.active.lock().await.stopping);
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
        drop(client);
    }
    #[tokio::test]
    async fn shutdown_http_returns_error_for_failed_guest_cleanup() {
        let app = shutdown_fixture();
        let (cancel, cancelled) = watch::channel(false);
        let (_done, finished) = watch::channel(Some(false));
        app.active.lock().await.active = Some(Session {
            request: uuid::Uuid::new_v4().to_string(),
            id: uuid::Uuid::new_v4().to_string(),
            cancel,
            finished,
            confirmed: watch::channel(false).0,
            input: None,
            files: None,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let result = serve_with_shutdown(
            listener,
            Router::new(),
            &app,
            async { Ok(()) },
            Duration::from_millis(50),
        )
        .await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("cleanup unconfirmed"));
        assert!(*cancelled.borrow());
    }

    #[tokio::test]
    async fn shutdown_http_clean_idle_service_exits_successfully() {
        let app = shutdown_fixture();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        serve_with_shutdown(
            listener,
            Router::new(),
            &app,
            async { Ok(()) },
            Duration::from_millis(100),
        )
        .await
        .unwrap();
        assert!(app.active.lock().await.stopping);
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }
}

#[cfg(test)]
mod au_cli_tests {
    #[test]
    fn finite_au_fault_is_explicit_unique_and_does_not_consume_auth_options() {
        let args = vec!["--auth-state".into(), "private".into()];
        assert_eq!(
            super::parse_au_hold_option(&args).unwrap(),
            (false, args.clone())
        );
        let mut configured = args.clone();
        configured.push("--fault-au-egress-hold".into());
        assert_eq!(
            super::parse_au_hold_option(&configured).unwrap(),
            (true, args)
        );
        configured.push("--fault-au-egress-hold".into());
        assert!(super::parse_au_hold_option(&configured).is_err());
    }
}
