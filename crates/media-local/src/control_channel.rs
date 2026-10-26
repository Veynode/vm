//! Signed console control over the already authenticated peer's DTLS/SCTP channel.
//! The owner must install `bounded_settings()` and `SEND_BUFFER_BYTES` on the SAME
//! peer whose exact SDP/fingerprint was signed, and await shutdown before peer.close.
//! These are application bounds, not a claim that the RTC library uses <=1 MiB:
//! webrtc 0.21 also retains up to 256 channel events and its bounded core queues.
use anyhow::{bail, ensure, Context, Result};
use bytes::BytesMut;
use rtc::peer_connection::configuration::setting_engine::SctpMaxMessageSize;
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{watch, Mutex};
use tokio::time::{timeout, Instant};
use vm_session_auth::control::SignedControl;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::peer_connection::{SettingEngine, SettingEngineBuilder};

pub const LABEL: &str = "veynode-control-v1";
pub const PROTOCOL: &str = "veynode-control/v1";
pub const CHUNK_BYTES: usize = 16 * 1024;
pub const HEADER_BYTES: usize = 17;
pub const JSON_BYTES: usize = 1024 * 1024;
pub const SEND_BUFFER_BYTES: usize = 64 * 1024;
const MAX_SEQUENCE: u64 = (1u64 << 53) - 1;
const ASSEMBLY_TIMEOUT: Duration = Duration::from_secs(5);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(20);
const SEND_TIMEOUT: Duration = Duration::from_secs(4);
const END_TIMEOUT: Duration = Duration::from_secs(12);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

/// Fixed, payload-free diagnostics. Never format an underlying request/backend error.
#[derive(Clone, Copy, Debug)]
pub(crate) enum FailureCode {
    ChannelValidation,
    Assembly,
    Replay,
    Malformed,
    ConcurrentDispatch,
    RemoteClosed,
    DispatchTimeout,
    DispatchRejected,
    ResponseSend,
    FileAuthorization,
    FileSource,
    FileBackend,
    RequestExpired,
    RequestBinding,
    Signature,
    Busy,
    SessionEnded,
    // Sub-reasons of what used to be the single `dispatch_rejected` catch-all on the console control path.
    // Each is a fixed word chosen by this service; none carries controller, Guest or request text.
    InputAuthorization,
    VmStatus,
    FrameAnchor,
    ControllerUnavailable,
    ControllerDeadline,
    ControllerRefused,
    ControllerReplyInvalid,
    ScreenshotReplyTooLarge,
    ScreenshotReplyInvalid,
    PngDecodeUnsupported,
    PngDecodeCorrupt,
    ScaledFrameOverCap,
    FrameVerification,
    FrameWorker,
}
impl FailureCode {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::ChannelValidation => "channel_validation",
            Self::Assembly => "fragment_assembly",
            Self::Replay => "request_replay",
            Self::Malformed => "malformed_request_or_response",
            Self::ConcurrentDispatch => "concurrent_dispatch",
            Self::RemoteClosed => "remote_channel_closed",
            Self::DispatchTimeout => "dispatch_timeout",
            Self::DispatchRejected => "dispatch_rejected",
            Self::ResponseSend => "response_send",
            Self::FileAuthorization => "file_authorization",
            Self::FileSource => "file_source",
            Self::FileBackend => "file_backend",
            Self::RequestExpired => "request_expired",
            Self::RequestBinding => "request_binding",
            Self::Signature => "request_signature",
            Self::Busy => "adapter_busy",
            Self::SessionEnded => "session_ended",
            Self::InputAuthorization => "input_authorization",
            Self::VmStatus => "vm_status_mismatch",
            Self::FrameAnchor => "frame_anchor",
            Self::ControllerUnavailable => "controller_unavailable",
            Self::ControllerDeadline => "controller_deadline",
            Self::ControllerRefused => "controller_refused",
            Self::ControllerReplyInvalid => "controller_reply_invalid",
            Self::ScreenshotReplyTooLarge => "screenshot_reply_too_large",
            Self::ScreenshotReplyInvalid => "screenshot_reply_invalid",
            Self::PngDecodeUnsupported => "png_decode_unsupported",
            Self::PngDecodeCorrupt => "png_decode_corrupt",
            Self::ScaledFrameOverCap => "scaled_frame_over_cap",
            Self::FrameVerification => "frame_verification",
            Self::FrameWorker => "frame_worker_failed",
        }
    }
}
impl std::fmt::Display for FailureCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
impl std::error::Error for FailureCode {}
impl FailureCode {
    /// The code an error already carries, if any.
    pub(crate) fn of(error: &anyhow::Error) -> Option<Self> {
        error.downcast_ref::<Self>().copied()
    }
    /// Attach this code to an error that carries none. The first (innermost) code wins, so a specific reason is
    /// never relabelled by a caller. The error keeps its own message for callers and tests; only the code is ever
    /// printed (`report_failure`), so the message may contain controller text without reaching a log.
    pub(crate) fn tag(self, error: anyhow::Error) -> anyhow::Error {
        if Self::of(&error).is_some() {
            return error;
        }
        let message = error.to_string();
        anyhow::Error::new(self).context(message)
    }
    /// `result.map_err(|e| code.tag(e))` for any error convertible to `anyhow::Error`.
    pub(crate) fn on<T, E: Into<anyhow::Error>>(self, result: Result<T, E>) -> Result<T> {
        result.map_err(|error| self.tag(error.into()))
    }
}
fn failure_category(error: &anyhow::Error) -> &'static str {
    let code = error
        .downcast_ref::<FailureCode>()
        .copied()
        .unwrap_or(FailureCode::DispatchRejected);
    code.name()
}
fn report_failure(error: &anyhow::Error) {
    eprintln!(
        "control channel failure category={}",
        failure_category(error)
    );
}

pub fn bounded_settings() -> SettingEngine {
    SettingEngineBuilder::new()
        .with_sctp_max_message_size(SctpMaxMessageSize::Bounded(CHUNK_BYTES as u32))
        .with_sctp_max_receive_buffer_size(SEND_BUFFER_BYTES as u32)
        .build()
}

/// The adapter rechecks signed-confirmed session identity and current authority on
/// EVERY request. `end` fences confirmation/dispatch first, then requires the
/// existing daemon session_end retirement ACK. No permissions come from DCEP.
#[async_trait::async_trait]
pub trait ControlDispatch: Send + Sync + 'static {
    async fn handle(&self, request: SignedControl) -> Result<Value>;
    /// The reply to `sequence` was handed to the transport. Timing only; the default ignores it.
    async fn replied(&self, _sequence: u64, _timing: ReplyTiming) {}
    async fn end(&self) -> Result<()>;
}

/// What sending one reply cost. Numbers only.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReplyTiming {
    pub serialise_us: u64,
    /// Every fragment handed to the DataChannel (enqueued; not a browser receipt).
    pub send_us: u64,
    pub bytes: usize,
    pub fragments: usize,
}

pub struct ControlChannel {
    dispatch: Arc<dyn ControlDispatch>,
    failure: watch::Sender<bool>,
    cancel: watch::Sender<bool>,
    done: watch::Sender<Option<bool>>,
    // 0 unused, 1 worker claimed, 2 closed without worker, 3 closing worker.
    // One atomic transition prevents accept racing successful shutdown.
    state: AtomicU8,
    rejecting: AtomicBool,
    end_result: Mutex<Option<bool>>,
    #[cfg(test)]
    response_tail: std::sync::Mutex<Option<watch::Receiver<bool>>>,
}
impl ControlChannel {
    pub fn new(dispatch: Arc<dyn ControlDispatch>, failure: watch::Sender<bool>) -> Arc<Self> {
        let (cancel, _) = watch::channel(false);
        let (done, _) = watch::channel(None);
        Arc::new(Self {
            dispatch,
            failure,
            cancel,
            done,
            state: AtomicU8::new(0),
            rejecting: AtomicBool::new(false),
            end_result: Mutex::new(None),
            #[cfg(test)]
            response_tail: std::sync::Mutex::new(None),
        })
    }
    fn fence(&self) {
        self.state.fetch_or(2, Ordering::AcqRel);
        self.cancel.send_replace(true);
    }
    fn fail(&self) {
        self.fence();
        self.failure.send_replace(true);
    }
    /// Synchronous: safe directly inside on_data_channel. Never await a driver
    /// round-trip (including getters/poll/close) inside that callback.
    pub fn accept(self: &Arc<Self>, channel: Arc<dyn DataChannel>) {
        if self
            .state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            eprintln!("control channel failure category=duplicate_channel");
            self.fail();
            // A hostile peer opening many streams must not spawn an unbounded
            // number of close tasks. The first violation requests full peer
            // teardown; subsequent channels are dropped for peer.close to reap.
            if !self.rejecting.swap(true, Ordering::AcqRel) {
                tokio::spawn(async move {
                    let _ = timeout(CLOSE_TIMEOUT, channel.close()).await;
                });
            }
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let cancel = this.cancel.subscribe();
            let result = tokio::select! {
                biased;
                _ = cancelled(cancel) => Ok(()),
                result = this.run(channel.clone()) => result,
            };
            if let Err(error) = &result {
                report_failure(error);
                this.fail();
            }
            this.fence();
            let (ended, closed) =
                tokio::join!(this.end_once(), timeout(CLOSE_TIMEOUT, channel.close()));
            this.done.send_replace(Some(
                ended && closed.as_ref().is_ok_and(channel_close_is_terminal),
            ));
        });
    }
    async fn end_once(&self) -> bool {
        let mut result = self.end_result.lock().await;
        if let Some(value) = *result {
            return value;
        }
        let ended = matches!(timeout(END_TIMEOUT, self.dispatch.end()).await, Ok(Ok(())));
        if !ended {
            eprintln!("control channel cleanup category=retirement_unconfirmed");
        }
        *result = Some(ended);
        ended
    }
    /// Permanent fence, including when no channel ever arrived. The adapter's
    /// retirement ACK and accepted worker's bounded closure are both required.
    pub async fn shutdown(&self) -> bool {
        self.fence();
        timeout(
            END_TIMEOUT + CLOSE_TIMEOUT + Duration::from_millis(100),
            async {
                let ended = self.end_once().await;
                if self.state.load(Ordering::Acquire) & 1 == 0 {
                    return ended;
                }
                let mut done = self.done.subscribe();
                loop {
                    if let Some(value) = *done.borrow_and_update() {
                        return ended && value;
                    }
                    if done.changed().await.is_err() {
                        return false;
                    }
                }
            },
        )
        .await
        .unwrap_or(false)
    }
    async fn run(&self, channel: Arc<dyn DataChannel>) -> Result<()> {
        timeout(Duration::from_secs(2), async {
            ensure!(
                channel.label().await? == LABEL && channel.protocol().await? == PROTOCOL,
                "control channel identity mismatch"
            );
            ensure!(
                channel.ordered().await?
                    && channel.max_packet_life_time().await?.is_none()
                    && channel.max_retransmits().await?.is_none()
                    && !channel.negotiated().await?,
                "control channel must be in-band reliable ordered"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context(FailureCode::ChannelValidation)?
        .context(FailureCode::ChannelValidation)?;
        let mut assembly = Assembly::default();
        let mut previous = 0;
        loop {
            let event = if let Some(deadline) = assembly.deadline() {
                tokio::time::timeout_at(deadline, channel.poll())
                    .await
                    .context(FailureCode::Assembly)?
            } else {
                channel.poll().await
            };
            match event {
                Some(DataChannelEvent::OnMessage(message)) => {
                    ensure!(!message.is_string, FailureCode::Malformed);
                    if let Some((sequence, bytes)) = assembly
                        .push(&message.data)
                        .context(FailureCode::Assembly)?
                    {
                        ensure!(sequence > previous, FailureCode::Replay);
                        previous = sequence;
                        let request: SignedControl =
                            serde_json::from_slice(&bytes).context(FailureCode::Malformed)?;
                        ensure!(
                            request.request.sequence == sequence,
                            FailureCode::RequestBinding
                        );
                        // Reject overlapping dispatcher work, but do not confuse the
                        // transport send tail with work still executing in the Guest.
                        // DataChannel::send enqueues BEFORE its final cooperative yield:
                        // the peer may receive the response and legally send its next
                        // request before this future returns. While sending, inbound
                        // fragments remain in the transport's bounded queue, never in
                        // an application action queue and never concurrently dispatched.
                        let operation = timeout(OPERATION_TIMEOUT, self.dispatch.handle(request));
                        tokio::pin!(operation);
                        let result = loop {
                            tokio::select! {
                                biased;
                                result=&mut operation=>break result,
                                event=channel.poll()=>match event {
                                    Some(DataChannelEvent::OnMessage(_))=>bail!(FailureCode::ConcurrentDispatch),
                                    Some(DataChannelEvent::OnOpen|DataChannelEvent::OnBufferedAmountLow|DataChannelEvent::OnBufferedAmountHigh)=>{},
                                    _=>bail!(FailureCode::RemoteClosed),
                                }
                            }
                        };
                        let (response, rejected) = match result {
                            Ok(Ok(value)) => {
                                ensure!(
                                    value.get("sequence").and_then(Value::as_u64) == Some(sequence),
                                    FailureCode::Malformed
                                );
                                (value, None)
                            }
                            Ok(Err(error)) => (
                                json!({"sequence":sequence,"error":"control rejected"}),
                                Some(error),
                            ),
                            Err(_) => bail!(FailureCode::DispatchTimeout),
                        };
                        let started = Instant::now();
                        let encoded = bounded_json(&response).context(FailureCode::Malformed)?;
                        drop(response);
                        let mut reply = ReplyTiming {
                            serialise_us: started.elapsed().as_micros() as u64,
                            bytes: encoded.len(),
                            fragments: encoded.len().div_ceil(CHUNK_BYTES - HEADER_BYTES),
                            ..Default::default()
                        };
                        let started = Instant::now();
                        timeout(SEND_TIMEOUT, async {
                            send_response(channel.as_ref(), sequence, &encoded).await?;
                            reply.send_us = started.elapsed().as_micros() as u64;
                            #[cfg(test)]
                            {
                                let barrier = self.response_tail.lock().unwrap().take();
                                if let Some(barrier) = barrier {
                                    cancelled(barrier).await;
                                }
                            }
                            Ok::<_, anyhow::Error>(())
                        })
                        .await
                        .context(FailureCode::ResponseSend)?
                        .context(FailureCode::ResponseSend)?;
                        if let Some(error) = rejected {
                            return Err(error);
                        }
                        self.dispatch.replied(sequence, reply).await;
                    }
                }
                Some(
                    DataChannelEvent::OnOpen
                    | DataChannelEvent::OnBufferedAmountLow
                    | DataChannelEvent::OnBufferedAmountHigh,
                ) => {}
                _ => bail!(FailureCode::RemoteClosed),
            }
        }
    }
}
// rtc 0.21 DataChannel::close documents this precise error as "no channel
// with this id is registered ... any more". Remote SCTP reset can remove it
// before our cleanup runs. That is a terminal owned-channel state, not a missing
// retirement ACK. Other errors/timeouts remain unconfirmed; the owner MUST still
// successfully close the entire PeerConnection after shutdown returns.
fn channel_close_is_terminal(result: &webrtc::error::Result<()>) -> bool {
    matches!(
        result,
        Ok(()) | Err(webrtc::error::Error::ErrDataChannelClosed)
    )
}
async fn cancelled(mut value: watch::Receiver<bool>) {
    loop {
        if *value.borrow_and_update() {
            return;
        }
        if value.changed().await.is_err() {
            return;
        }
    }
}

#[derive(Default)]
struct Assembly {
    sequence: u64,
    total: usize,
    bytes: Vec<u8>,
    started: Option<Instant>,
}
impl Assembly {
    fn deadline(&self) -> Option<Instant> {
        self.started.map(|v| v + ASSEMBLY_TIMEOUT)
    }
    fn push(&mut self, chunk: &[u8]) -> Result<Option<(u64, Vec<u8>)>> {
        ensure!(
            chunk.len() > HEADER_BYTES && chunk.len() <= CHUNK_BYTES,
            "control fragment size invalid"
        );
        ensure!(chunk[0] == 1, "control fragment version invalid");
        let sequence = u64::from_be_bytes(chunk[1..9].try_into()?);
        let total = u32::from_be_bytes(chunk[9..13].try_into()?) as usize;
        let offset = u32::from_be_bytes(chunk[13..17].try_into()?) as usize;
        ensure!(
            (1..=MAX_SEQUENCE).contains(&sequence) && (1..=JSON_BYTES).contains(&total),
            "control fragment bounds invalid"
        );
        if self.started.is_none() {
            ensure!(offset == 0, "first control fragment offset must be zero");
            self.sequence = sequence;
            self.total = total;
            self.started = Some(Instant::now());
            self.bytes = Vec::with_capacity(total);
        }
        ensure!(
            self.deadline().is_some_and(|at| Instant::now() < at),
            "control assembly expired"
        );
        ensure!(
            sequence == self.sequence && total == self.total && offset == self.bytes.len(),
            "control fragment continuity invalid"
        );
        let payload = &chunk[HEADER_BYTES..];
        ensure!(
            payload.len() <= total - self.bytes.len(),
            "control fragment overruns total"
        );
        self.bytes.extend_from_slice(payload);
        if self.bytes.len() == total {
            let bytes = std::mem::take(&mut self.bytes);
            self.started = None;
            Ok(Some((sequence, bytes)))
        } else {
            Ok(None)
        }
    }
}
fn fragment(sequence: u64, total: usize, offset: usize, payload: &[u8]) -> Result<BytesMut> {
    ensure!(
        (1..=MAX_SEQUENCE).contains(&sequence) && (1..=JSON_BYTES).contains(&total),
        "control response bounds invalid"
    );
    ensure!(
        !payload.is_empty()
            && payload.len() <= CHUNK_BYTES - HEADER_BYTES
            && offset <= total
            && payload.len() <= total - offset,
        "control response fragment bounds invalid"
    );
    let mut bytes = BytesMut::with_capacity(HEADER_BYTES + payload.len());
    bytes.extend_from_slice(&[1]);
    bytes.extend_from_slice(&sequence.to_be_bytes());
    bytes.extend_from_slice(&(total as u32).to_be_bytes());
    bytes.extend_from_slice(&(offset as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    Ok(bytes)
}
fn bounded_json(value: &Value) -> Result<Vec<u8>> {
    struct Limited(Vec<u8>);
    impl std::io::Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > JSON_BYTES - self.0.len() {
                return Err(std::io::Error::other("control response exceeds bound"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Limited(Vec::new());
    serde_json::to_writer(&mut output, value)?;
    Ok(output.0)
}
async fn send_response(channel: &dyn DataChannel, sequence: u64, bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= JSON_BYTES,
        "control response exceeds bound"
    );
    for (index, payload) in bytes.chunks(CHUNK_BYTES - HEADER_BYTES).enumerate() {
        channel
            .send(fragment(
                sequence,
                bytes.len(),
                index * (CHUNK_BYTES - HEADER_BYTES),
                payload,
            )?)
            .await?;
    }
    Ok(()) // Enqueued is not a browser receipt or Guest input acknowledgement.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use webrtc::{
        data_channel::RTCDataChannelInit,
        peer_connection::{
            PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
        },
    };

    #[test]
    fn telemetry_uses_only_fixed_categories_never_underlying_error_text() {
        let secret =
            anyhow::anyhow!("SECRET_PATH TOKEN FILE_BODY").context(FailureCode::FileBackend);
        assert_eq!(failure_category(&secret), "file_backend");
        assert_eq!(
            failure_category(&anyhow::anyhow!("private arbitrary text")),
            "dispatch_rejected"
        );
    }
    #[test]
    fn tagged_errors_report_the_innermost_code_and_keep_their_message_out_of_the_category() {
        let inner = FailureCode::ControllerRefused.tag(anyhow::anyhow!("GUEST_TEXT /private/path"));
        assert_eq!(inner.to_string(), "GUEST_TEXT /private/path");
        assert_eq!(failure_category(&inner), "controller_refused");
        // An outer caller cannot relabel it, and the category never contains the message.
        let outer = FailureCode::FrameVerification.tag(inner);
        assert_eq!(failure_category(&outer), "controller_refused");
        assert!(!failure_category(&outer).contains("GUEST"));
        assert_eq!(
            FailureCode::ScaledFrameOverCap
                .on::<(), _>(Err(std::io::Error::other("x")))
                .map_err(|e| failure_category(&e))
                .unwrap_err(),
            "scaled_frame_over_cap"
        );
        // Every category is a short lowercase word list: nothing a log scanner could mistake for data.
        for code in [
            FailureCode::InputAuthorization,
            FailureCode::VmStatus,
            FailureCode::FrameAnchor,
            FailureCode::ControllerUnavailable,
            FailureCode::ControllerDeadline,
            FailureCode::ControllerRefused,
            FailureCode::ControllerReplyInvalid,
            FailureCode::ScreenshotReplyTooLarge,
            FailureCode::ScreenshotReplyInvalid,
            FailureCode::PngDecodeUnsupported,
            FailureCode::PngDecodeCorrupt,
            FailureCode::ScaledFrameOverCap,
            FailureCode::FrameVerification,
            FailureCode::FrameWorker,
        ] {
            let name = code.name();
            assert!(
                name.len() <= 32 && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{name}"
            );
        }
    }
    #[test]
    fn fragment_header_is_network_order_and_safe_integer_bounded() {
        let bytes = fragment(0x0102030405, 0x0304, 0x0102, b"X").unwrap();
        assert_eq!(
            &bytes[..],
            &[1, 0, 0, 0, 1, 2, 3, 4, 5, 0, 0, 3, 4, 0, 0, 1, 2, b'X']
        );
        assert!(fragment(MAX_SEQUENCE + 1, 1, 0, b"x").is_err());
        assert!(fragment(0, 1, 0, b"x").is_err());
        assert!(fragment(1, JSON_BYTES + 1, 0, b"x").is_err());
    }
    #[test]
    fn serialized_response_is_capped_before_writing_excess_bytes() {
        let exact = Value::String("x".repeat(JSON_BYTES - 2));
        assert_eq!(bounded_json(&exact).unwrap().len(), JSON_BYTES);
        assert!(bounded_json(&Value::String("x".repeat(JSON_BYTES - 1))).is_err());
        // JSON escaping expands input, so checking the source string's length
        // would not enforce the actual wire-byte budget.
        assert!(bounded_json(&Value::String("\n".repeat(JSON_BYTES / 2))).is_err());
    }
    #[test]
    fn reassembles_maximum_and_rejects_overlap_gaps_substitution_and_expiry() {
        let source = vec![42; JSON_BYTES];
        let mut assembly = Assembly::default();
        let mut complete = None;
        for (i, payload) in source.chunks(CHUNK_BYTES - HEADER_BYTES).enumerate() {
            complete = assembly
                .push(
                    &fragment(9, source.len(), i * (CHUNK_BYTES - HEADER_BYTES), payload).unwrap(),
                )
                .unwrap();
        }
        assert_eq!(complete.unwrap(), (9, source));
        for malformed in [
            fragment(2, 4, 2, b"cd").unwrap(),
            fragment(1, 5, 2, b"cd").unwrap(),
            fragment(1, 4, 1, b"cd").unwrap(),
            fragment(1, 4, 3, b"d").unwrap(),
        ] {
            let mut a = Assembly::default();
            a.push(&fragment(1, 4, 0, b"ab").unwrap()).unwrap();
            assert!(a.push(&malformed).is_err());
        }
        let mut a = Assembly::default();
        a.push(&fragment(1, 4, 0, b"ab").unwrap()).unwrap();
        a.started = Some(Instant::now() - ASSEMBLY_TIMEOUT);
        assert!(a.push(&fragment(1, 4, 2, b"cd").unwrap()).is_err());
        let mut overrun = fragment(1, 1, 0, b"a").unwrap();
        overrun.extend_from_slice(b"b");
        assert!(Assembly::default().push(&overrun).is_err());
        assert!(Assembly::default().push(&vec![0; CHUNK_BYTES + 1]).is_err());
        assert!(Assembly::default().push(&[1; HEADER_BYTES]).is_err());
    }
    // Real loopback ICE/DTLS/SCTP transport; this dispatcher is a test double,
    // not an actual VM, lease or authorization acceptance test.
    struct Dispatch {
        confirmed: AtomicBool,
        pending: bool,
        dispatched: AtomicUsize,
        ended: AtomicUsize,
        fail_retirement: AtomicBool,
        cancelled: Arc<AtomicUsize>,
        started: watch::Sender<bool>,
    }
    struct PendingGuard(Arc<AtomicUsize>);
    impl Drop for PendingGuard {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[async_trait::async_trait]
    impl ControlDispatch for Dispatch {
        async fn handle(&self, request: SignedControl) -> Result<Value> {
            ensure!(self.confirmed.load(Ordering::SeqCst), "not confirmed");
            let golden: Value =
                serde_json::from_str(include_str!("../../session-auth/tests/control-golden.json"))?;
            vm_session_auth::verify(
                golden["device_public_key"].as_str().unwrap(),
                &request.request.canonical()?,
                &request.signature,
            )?;
            self.dispatched.fetch_add(1, Ordering::SeqCst);
            self.started.send_replace(true);
            if self.pending {
                let _guard = PendingGuard(self.cancelled.clone());
                std::future::pending::<()>().await;
            }
            Ok(
                json!({"sequence":request.request.sequence,"result":{"payload":"x".repeat(900_000)}}),
            )
        }
        async fn end(&self) -> Result<()> {
            self.confirmed.store(false, Ordering::SeqCst);
            self.ended.fetch_add(1, Ordering::SeqCst);
            ensure!(
                !self.fail_retirement.load(Ordering::SeqCst),
                "retirement ACK unavailable"
            );
            Ok(())
        }
    }
    struct Events {
        gathered: watch::Sender<bool>,
        control: Option<Arc<ControlChannel>>,
    }
    #[async_trait::async_trait]
    impl PeerConnectionEventHandler for Events {
        async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
            if state == RTCIceGatheringState::Complete {
                self.gathered.send_replace(true);
            }
        }
        async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
            if let Some(control) = &self.control {
                control.accept(channel);
            }
        }
    }
    struct Pair {
        client: Arc<dyn PeerConnection>,
        server: Arc<dyn PeerConnection>,
        channel: Arc<dyn DataChannel>,
        control: Arc<ControlChannel>,
        dispatch: Arc<Dispatch>,
        failure: watch::Receiver<bool>,
    }
    impl Pair {
        async fn new(confirmed: bool, pending: bool) -> Self {
            timeout(Duration::from_secs(12), Self::connect(confirmed, pending))
                .await
                .expect("RTC pair connection deadline")
        }
        async fn connect(confirmed: bool, pending: bool) -> Self {
            let (failure_tx, failure) = watch::channel(false);
            let (started, _) = watch::channel(false);
            let dispatch = Arc::new(Dispatch {
                confirmed: AtomicBool::new(confirmed),
                pending,
                dispatched: AtomicUsize::new(0),
                ended: AtomicUsize::new(0),
                fail_retirement: AtomicBool::new(false),
                cancelled: Arc::new(AtomicUsize::new(0)),
                started,
            });
            let control = ControlChannel::new(dispatch.clone(), failure_tx);
            let (a_tx, a_rx) = watch::channel(false);
            let (b_tx, b_rx) = watch::channel(false);
            let build = |handler| {
                PeerConnectionBuilder::new()
                    .with_setting_engine(bounded_settings())
                    .with_data_channel_send_buffer_limit(SEND_BUFFER_BYTES)
                    .with_handler(handler)
                    .with_runtime(webrtc::runtime::default_runtime().unwrap())
                    .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
            };
            let client: Arc<dyn PeerConnection> = Arc::new(
                build(Arc::new(Events {
                    gathered: a_tx,
                    control: None,
                }))
                .build()
                .await
                .unwrap(),
            );
            let server: Arc<dyn PeerConnection> = Arc::new(
                build(Arc::new(Events {
                    gathered: b_tx,
                    control: Some(control.clone()),
                }))
                .build()
                .await
                .unwrap(),
            );
            let channel = client
                .create_data_channel(
                    LABEL,
                    Some(RTCDataChannelInit {
                        protocol: PROTOCOL.into(),
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            let offer = client.create_offer(None).await.unwrap();
            client.set_local_description(offer).await.unwrap();
            cancelled(a_rx).await;
            server
                .set_remote_description(client.local_description().await.unwrap())
                .await
                .unwrap();
            let answer = server.create_answer(None).await.unwrap();
            server.set_local_description(answer).await.unwrap();
            cancelled(b_rx).await;
            client
                .set_remote_description(server.local_description().await.unwrap())
                .await
                .unwrap();
            loop {
                match channel.poll().await {
                    Some(DataChannelEvent::OnOpen) => break,
                    Some(DataChannelEvent::OnMessage(_)) => panic!("unexpected early data"),
                    None => panic!("channel closed before open"),
                    _ => {}
                }
            }
            Self {
                client,
                server,
                channel,
                control,
                dispatch,
                failure,
            }
        }
        async fn request(&self) {
            let golden: Value =
                serde_json::from_str(include_str!("../../session-auth/tests/control-golden.json"))
                    .unwrap();
            let bytes = serde_json::to_vec(&golden["cases"][2]["signed"]).unwrap();
            self.channel
                .send(fragment(3, bytes.len(), 0, &bytes).unwrap())
                .await
                .unwrap();
        }
        async fn close(self) {
            assert!(
                self.control.shutdown().await,
                "retirement and channel closure ACK"
            );
            timeout(Duration::from_secs(2), self.client.close())
                .await
                .unwrap()
                .unwrap();
            timeout(Duration::from_secs(2), self.server.close())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                self.dispatch.ended.load(Ordering::SeqCst),
                1,
                "retired exactly once"
            );
        }
    }
    #[tokio::test]
    async fn actual_next_request_after_receipt_survives_pending_send_tail() {
        use p256::ecdsa::{signature::Signer, SigningKey};
        let pair = Pair::new(true, false).await;
        let (release, wait) = watch::channel(false);
        *pair.control.response_tail.lock().unwrap() = Some(wait);
        pair.request().await;
        let receive = |channel: Arc<dyn DataChannel>| async move {
            timeout(Duration::from_secs(8), async move {
                let mut assembly = Assembly::default();
                loop {
                    match channel.poll().await {
                        Some(DataChannelEvent::OnMessage(message)) => {
                            if let Some((sequence, _)) = assembly.push(&message.data).unwrap() {
                                break sequence;
                            }
                        }
                        Some(
                            DataChannelEvent::OnOpen
                            | DataChannelEvent::OnBufferedAmountLow
                            | DataChannelEvent::OnBufferedAmountHigh,
                        ) => {}
                        _ => panic!("channel ended before sequential response"),
                    }
                }
            })
            .await
            .unwrap()
        };
        assert_eq!(receive(pair.channel.clone()).await, 3);
        let golden: Value =
            serde_json::from_str(include_str!("../../session-auth/tests/control-golden.json"))
                .unwrap();
        let mut signed: SignedControl =
            serde_json::from_value(golden["cases"][2]["signed"].clone()).unwrap();
        signed.request.sequence = 4;
        let mut secret = [0; 32];
        secret[31] = 2;
        let key = SigningKey::from_bytes((&secret).into()).unwrap();
        let signature: p256::ecdsa::Signature = key.sign(&signed.request.canonical().unwrap());
        signed.signature = vm_session_auth::hex(&signature.to_bytes());
        let bytes = serde_json::to_vec(&signed).unwrap();
        pair.channel
            .send(fragment(4, bytes.len(), 0, &bytes).unwrap())
            .await
            .unwrap();
        // The first response was already received on the real peer. Keep only
        // the sender's completion tail pending, modelling wake_writes yielding.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !*pair.failure.borrow(),
            "a sequential client must not be labelled concurrent"
        );
        release.send_replace(true);
        assert_eq!(receive(pair.channel.clone()).await, 4);
        assert_eq!(pair.dispatch.dispatched.load(Ordering::SeqCst), 2);
        pair.close().await;
    }
    #[tokio::test]
    async fn actual_dtls_sctp_large_response_and_clean_shutdown() {
        let pair = Pair::new(true, false).await;
        pair.request().await;
        let response = timeout(Duration::from_secs(8), async {
            let mut assembly = Assembly::default();
            loop {
                match pair.channel.poll().await {
                    Some(DataChannelEvent::OnMessage(message)) => {
                        if let Some((sequence, body)) = assembly.push(&message.data).unwrap() {
                            assert_eq!(sequence, 3);
                            break body;
                        }
                    }
                    Some(
                        DataChannelEvent::OnBufferedAmountHigh
                        | DataChannelEvent::OnBufferedAmountLow,
                    ) => {}
                    _ => panic!("channel ended before complete response"),
                }
            }
        })
        .await
        .unwrap();
        let value: Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(value["result"]["payload"].as_str().unwrap().len(), 900_000);
        assert_eq!(pair.dispatch.dispatched.load(Ordering::SeqCst), 1);
        assert!(!*pair.failure.borrow());
        pair.close().await;
    }
    /// Benchmark, ignored by default: wall time of one 900,000-byte reply (about the largest control picture)
    /// from request sent to last fragment reassembled, over real loopback ICE/DTLS/SCTP between two instances of
    /// this stack. A browser peer and a real network path will differ; this isolates the sender's share.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "benchmark; run in release with --ignored --nocapture"]
    async fn budget_of_one_large_reply_over_loopback_datachannel() {
        for run in 0..5 {
            let pair = Pair::new(true, false).await;
            let started = Instant::now();
            pair.request().await;
            let (mut first, mut fragments) = (None, 0);
            let bytes = timeout(Duration::from_secs(8), async {
                let mut assembly = Assembly::default();
                loop {
                    match pair.channel.poll().await {
                        Some(DataChannelEvent::OnMessage(message)) => {
                            first.get_or_insert_with(|| started.elapsed());
                            fragments += 1;
                            if let Some((_, body)) = assembly.push(&message.data).unwrap() {
                                break body.len();
                            }
                        }
                        Some(
                            DataChannelEvent::OnBufferedAmountHigh
                            | DataChannelEvent::OnBufferedAmountLow,
                        ) => {}
                        _ => panic!("channel ended before complete response"),
                    }
                }
            })
            .await
            .unwrap();
            eprintln!(
                "budget datachannel run={run} bytes={bytes} fragments={fragments} first_fragment_ms={:.1} complete_ms={:.1}",
                first.unwrap().as_secs_f64() * 1000.0,
                started.elapsed().as_secs_f64() * 1000.0
            );
            pair.close().await;
        }
    }
    #[tokio::test]
    async fn actual_unconfirmed_request_is_rejected_and_retired() {
        let pair = Pair::new(false, false).await;
        pair.request().await;
        timeout(Duration::from_secs(3), cancelled(pair.failure.clone()))
            .await
            .unwrap();
        assert_eq!(pair.dispatch.dispatched.load(Ordering::SeqCst), 0);
        pair.close().await;
    }
    #[tokio::test]
    async fn actual_duplicate_channel_fences_session() {
        let pair = Pair::new(true, false).await;
        let _duplicate = pair
            .client
            .create_data_channel(
                LABEL,
                Some(RTCDataChannelInit {
                    protocol: PROTOCOL.into(),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        timeout(Duration::from_secs(3), cancelled(pair.failure.clone()))
            .await
            .unwrap();
        assert_eq!(pair.dispatch.dispatched.load(Ordering::SeqCst), 0);
        pair.close().await;
    }
    #[tokio::test]
    async fn actual_concurrent_request_cancels_inflight_without_dispatching_second() {
        let pair = Pair::new(true, true).await;
        pair.request().await;
        timeout(
            Duration::from_secs(3),
            cancelled(pair.dispatch.started.subscribe()),
        )
        .await
        .unwrap();
        pair.request().await;
        timeout(Duration::from_secs(3), cancelled(pair.failure.clone()))
            .await
            .unwrap();
        assert_eq!(pair.dispatch.dispatched.load(Ordering::SeqCst), 1);
        assert_eq!(pair.dispatch.cancelled.load(Ordering::SeqCst), 1);
        pair.close().await;
    }
    #[tokio::test]
    async fn actual_partial_request_timeout_retires_without_dispatch() {
        let pair = Pair::new(true, false).await;
        pair.channel
            .send(fragment(1, 100, 0, b"{").unwrap())
            .await
            .unwrap();
        timeout(
            ASSEMBLY_TIMEOUT + Duration::from_secs(2),
            cancelled(pair.failure.clone()),
        )
        .await
        .unwrap();
        assert_eq!(pair.dispatch.dispatched.load(Ordering::SeqCst), 0);
        pair.close().await;
    }
    #[tokio::test]
    async fn actual_shutdown_cancels_inflight_and_permanently_fences() {
        let pair = Pair::new(true, true).await;
        pair.request().await;
        timeout(
            Duration::from_secs(3),
            cancelled(pair.dispatch.started.subscribe()),
        )
        .await
        .unwrap();
        assert!(timeout(Duration::from_secs(2), pair.control.shutdown())
            .await
            .unwrap());
        assert_eq!(pair.dispatch.cancelled.load(Ordering::SeqCst), 1);
        assert_eq!(pair.dispatch.dispatched.load(Ordering::SeqCst), 1);
        assert_eq!(pair.control.state.load(Ordering::Acquire), 3);
        pair.close().await;
    }
    #[tokio::test]
    async fn actual_remote_channel_close_then_shutdown_attests_cleanup() {
        let pair = Pair::new(true, true).await;
        pair.request().await;
        timeout(
            Duration::from_secs(3),
            cancelled(pair.dispatch.started.subscribe()),
        )
        .await
        .unwrap();
        pair.channel.close().await.unwrap();
        timeout(Duration::from_secs(3), cancelled(pair.failure.clone()))
            .await
            .unwrap();
        assert!(
            pair.control.shutdown().await,
            "remote-first channel closure must retain cleanup proof"
        );
        assert_eq!(pair.dispatch.cancelled.load(Ordering::SeqCst), 1);
        pair.close().await;
    }
    #[tokio::test]
    async fn actual_remote_closed_channel_never_hides_missing_retirement_ack() {
        let pair = Pair::new(true, true).await;
        pair.request().await;
        timeout(
            Duration::from_secs(3),
            cancelled(pair.dispatch.started.subscribe()),
        )
        .await
        .unwrap();
        pair.dispatch.fail_retirement.store(true, Ordering::SeqCst);
        pair.channel.close().await.unwrap();
        timeout(Duration::from_secs(3), cancelled(pair.failure.clone()))
            .await
            .unwrap();
        assert!(
            !pair.control.shutdown().await,
            "removed channel is not a lease retirement ACK"
        );
        assert_eq!(pair.dispatch.cancelled.load(Ordering::SeqCst), 1);
        timeout(Duration::from_secs(2), pair.client.close())
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(2), pair.server.close())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pair.dispatch.ended.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn close_only_accepts_success_or_exact_removed_channel_state() {
        assert!(channel_close_is_terminal(&Ok(())));
        assert!(channel_close_is_terminal(&Err(
            webrtc::error::Error::ErrDataChannelClosed
        )));
        assert!(!channel_close_is_terminal(&Err(
            webrtc::error::Error::ErrSendBufferFull
        )));
    }
    #[tokio::test]
    async fn actual_remote_channel_then_peer_close_cancels_active_request() {
        let pair = Pair::new(true, true).await;
        pair.request().await;
        timeout(
            Duration::from_secs(3),
            cancelled(pair.dispatch.started.subscribe()),
        )
        .await
        .unwrap();
        pair.channel.close().await.unwrap();
        pair.client.close().await.unwrap();
        assert!(pair.control.shutdown().await);
        assert_eq!(pair.dispatch.cancelled.load(Ordering::SeqCst), 1);
        assert_eq!(pair.dispatch.dispatched.load(Ordering::SeqCst), 1);
        pair.close().await;
    }
    #[tokio::test]
    async fn actual_remote_peer_close_then_shutdown_attests_cleanup() {
        let pair = Pair::new(true, true).await;
        pair.request().await;
        timeout(
            Duration::from_secs(3),
            cancelled(pair.dispatch.started.subscribe()),
        )
        .await
        .unwrap();
        pair.client.close().await.unwrap();
        assert!(
            pair.control.shutdown().await,
            "remote-first peer closure must allow bounded local cleanup"
        );
        assert_eq!(pair.dispatch.cancelled.load(Ordering::SeqCst), 1);
        pair.close().await;
    }
    #[tokio::test]
    async fn actual_malformed_fragment_retires_without_dispatch() {
        let pair = Pair::new(true, false).await;
        let mut bytes = fragment(1, 1, 0, b"x").unwrap();
        bytes[0] = 2;
        pair.channel.send(bytes).await.unwrap();
        timeout(Duration::from_secs(3), cancelled(pair.failure.clone()))
            .await
            .unwrap();
        assert_eq!(pair.dispatch.dispatched.load(Ordering::SeqCst), 0);
        pair.close().await;
    }
    #[tokio::test]
    async fn shutdown_before_channel_arrives_still_retires_and_is_idempotent() {
        let (failure, _) = watch::channel(false);
        let (started, _) = watch::channel(false);
        let dispatch = Arc::new(Dispatch {
            confirmed: AtomicBool::new(true),
            pending: false,
            dispatched: AtomicUsize::new(0),
            ended: AtomicUsize::new(0),
            fail_retirement: AtomicBool::new(false),
            cancelled: Arc::new(AtomicUsize::new(0)),
            started,
        });
        let control = ControlChannel::new(dispatch.clone(), failure);
        assert!(control.shutdown().await);
        assert!(control.shutdown().await);
        assert_eq!(dispatch.ended.load(Ordering::SeqCst), 1);
    }
}
