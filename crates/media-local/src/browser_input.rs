//! Host-only adapter from signed control requests to the existing QEMU controller.
//! HTTP exposure remains loopback-only. Signatures are not payload encryption.
use super::{now, Auth};
use crate::control_channel::FailureCode;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{watch, Mutex},
};
use vm_control::screenshot_scaled::{Refusal, REFUSAL_FIELD};
use vm_session_auth::{
    self as protocol,
    control::{ControlAction, FrameAnchor, SignedControl},
};
#[path = "control_frame.rs"]
pub(crate) mod control_frame;
const RPC_LIMIT: u64 = 1024 * 1024;
/// The controller screenshot reply is a full-resolution PNG in base64 JSON (decision 0024). Only that method may
/// exceed `RPC_LIMIT`; the frame sent on to the browser is still bounded by `CONTROL_FRAME_MAX_BYTES`.
const SCREENSHOT_RPC_LIMIT: u64 = vm_control::MAX_SCREENSHOT_RESPONSE as u64;
const OBSERVATION_TTL: Duration = Duration::from_secs(5);
/// Error text of the typed "no picture for this request" reply. Distinct from `"control rejected"`, which fences.
pub const FRAME_UNAVAILABLE: &str = "frame unavailable";

/// What one dispatched action produced.
enum Outcome {
    Result(Value),
    /// `observe_frame` only: the request was authentic, authorised and in sequence, but no picture could be
    /// produced. Nothing was changed: no anchor recorded. The session and its control stay up.
    FrameUnavailable(FailureCode),
}
/// The picture failures that are an ordinary outcome instead of a control violation. Each one means "the
/// controller had no usable picture this time": it refused the capture (frame not ready, capture busy, PNG above
/// its own bound), it did not answer in time (a screenshot changes nothing in the Guest, so an unknown completion
/// is harmless), the PNG is a valid form this service does not decode, or the picture cannot be brought under the
/// 700 KiB frame bound. The `screenshot` call carries no authority of its own, so none of these is a denial
/// of authority. Everything else still fences: a reply above the 12 MiB / 8 MiB contract, a reply that
/// disagrees with itself (size, hash, geometry, corrupt PNG), and any failed identity, status, authorisation,
/// signature, sequence or expiry check.
fn frame_unavailable(error: &anyhow::Error) -> Option<FailureCode> {
    FailureCode::of(error).filter(|code| {
        matches!(
            code,
            FailureCode::ControllerRefused
                | FailureCode::ControllerDeadline
                | FailureCode::PngDecodeUnsupported
                | FailureCode::ScaledFrameOverCap
        )
    })
}

#[derive(Clone)]
struct SocketIdentity {
    path: PathBuf,
    dev: u64,
    ino: u64,
}
impl SocketIdentity {
    fn read(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "private socket path must be absolute");
        let meta = std::fs::symlink_metadata(path)?;
        ensure!(
            meta.file_type().is_socket()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o077 == 0,
            "socket must be private and owner-held"
        );
        Ok(Self {
            path: path.canonicalize()?,
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
    fn check(&self) -> Result<()> {
        let current = Self::read(&self.path)?;
        ensure!(
            current.dev == self.dev && current.ino == self.ino,
            "private socket incarnation changed"
        );
        Ok(())
    }
}
struct Observation {
    anchor: FrameAnchor,
    received: Instant,
}
struct InputState {
    sequence: u64,
    frames: VecDeque<Observation>,
    /// Stage times of the `observe_frame` being handled, if that is the action in flight.
    timing: Option<ObserveTiming>,
    /// The controller has no scaled-JPEG screenshot form (it answered a request for it with a PNG, or refused it
    /// as unavailable or invalid). Remembered for the session: every later picture asks for the PNG only.
    png_only: bool,
    /// Guest geometry of the last picture, for sizing a request above divisor 1 (`control_frame::helper_steps`).
    source: Option<(u64, u64)>,
    /// The last PNG of this session was above the control-frame bound and had to be scaled and re-encoded here, so
    /// the following pictures ask the helper for that JPEG. A session starts with the PNG: a screen that fits is
    /// still delivered losslessly, exactly as before, and its geometry sizes the helper requests.
    helper_mode: bool,
    /// Pictures taken from the helper since the last accepted PNG or accepted fallback from a failed PNG probe;
    /// after `PNG_PROBE_EVERY` the next one is a PNG again, so a screen that fits returns to the lossless picture.
    helper_pictures: u32,
    /// Index into `control_frame::helper_steps` of the pair that fitted last time: the next picture starts there
    /// instead of paying one refused capture for every pair above it.
    helper_hint: usize,
    /// Helper pictures still to come before the hint may move to a better pair again. Set by a size refusal: a
    /// screen whose picture is just under half the bound would otherwise pay a refused capture every second time.
    helper_hold: u32,
}
/// See `InputState::helper_pictures`.
const PNG_PROBE_EVERY: u32 = 64;
/// See `InputState::helper_hold`.
const HINT_HOLD: u32 = 8;
/// The controller's fixed word for a refused scaled screenshot. It travels with the usual `controller_refused`
/// code and shows the controller's text, exactly as an unmarked refusal does.
#[derive(Debug)]
struct ScaledRefused {
    refusal: Refusal,
    message: String,
}
impl std::fmt::Display for ScaledRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ScaledRefused {}
/// One captured picture before it becomes a control frame.
enum Shot {
    /// The helper's scaled JPEG and the request it answers.
    Helper(Value, control_frame::HelperStep),
    /// A PNG, to be passed through or transformed as before.
    Png(Value),
}
/// Where the time of one controller exchange went.
#[derive(Clone, Copy, Debug, Default)]
struct RpcTiming {
    // For an `observe_frame` that made several screenshot requests, each field is the sum over them.
    /// Connect and request written, until the first reply byte: the controller's own work.
    wait_us: u64,
    /// First reply byte until the whole reply line was read.
    read_us: u64,
    parse_us: u64,
    bytes: usize,
}
/// Stage times and sizes of one `observe_frame`. Numbers only; see `timing_line`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ObserveTiming {
    entered: Option<Instant>,
    /// Request checks before the capture: identity, authority, signature, sequence, VM status.
    admit_us: u64,
    rpc: RpcTiming,
    /// The frame transform on the blocking pool, including the wait for a pool thread.
    worker_us: u64,
    frame: control_frame::FrameTiming,
    verify_us: u64,
    /// `handle` entered until its reply value was ready.
    total_us: u64,
}
/// The one log line of an `observe_frame`: stage names with whole milliseconds, byte counts and the scale that
/// was chosen. `helper_jpeg=1` is a picture the console helper scaled and encoded itself (`helper_quality` is its
/// ImageIO number, `quality` the matching step of our own scale); `helper_jpeg=0` is the PNG path, transformed
/// here or, with `passthrough=1`, forwarded. `requests` counts the screenshot requests the picture cost. Nothing here derives from picture content, a hash, an identifier or controller text.
/// `reply` is absent when the reply was never handed to the transport.
pub(crate) fn timing_line(
    timing: &ObserveTiming,
    reply: Option<&crate::control_channel::ReplyTiming>,
) -> String {
    let ms = |us: u64| (us + 500) / 1000;
    let frame = &timing.frame;
    let transform = frame.base64_decode_us
        + frame.png_chunks_us
        + frame.inflate_us
        + frame.unfilter_us
        + frame.scale_us
        + frame.jpeg_us
        + frame.hash_us
        + frame.base64_encode_us;
    format!(
        "control frame timing total_ms={} admit_ms={} rpc_wait_ms={} rpc_read_ms={} rpc_parse_ms={} \
         worker_wait_ms={} base64_decode_ms={} png_chunks_ms={} inflate_ms={} unfilter_ms={} scale_ms={} jpeg_ms={} \
         hash_ms={} base64_encode_ms={} verify_ms={} serialise_ms={} send_ms={} rpc_bytes={} source_bytes={} \
         raw_bytes={} frame_bytes={} reply_bytes={} fragments={} width={} height={} attempts={} divisor={} \
         quality={} passthrough={} helper_jpeg={} helper_quality={} requests={} sent={}",
        ms(timing.total_us),
        ms(timing.admit_us),
        ms(timing.rpc.wait_us),
        ms(timing.rpc.read_us),
        ms(timing.rpc.parse_us),
        ms(timing.worker_us.saturating_sub(transform)),
        ms(frame.base64_decode_us),
        ms(frame.png_chunks_us),
        ms(frame.inflate_us),
        ms(frame.unfilter_us),
        ms(frame.scale_us),
        ms(frame.jpeg_us),
        ms(frame.hash_us),
        ms(frame.base64_encode_us),
        ms(timing.verify_us),
        ms(reply.map_or(0, |reply| reply.serialise_us)),
        ms(reply.map_or(0, |reply| reply.send_us)),
        timing.rpc.bytes,
        frame.source_bytes,
        frame.raw_bytes,
        frame.frame_bytes,
        reply.map_or(0, |reply| reply.bytes),
        reply.map_or(0, |reply| reply.fragments),
        frame.width,
        frame.height,
        frame.attempts,
        frame.divisor,
        frame.quality,
        frame.passthrough as u8,
        frame.helper_jpeg as u8,
        frame.helper_quality,
        frame.requests,
        reply.is_some() as u8,
    )
}
pub struct SessionInput {
    auth: Arc<Auth>,
    session_id: String,
    transcript_sha256: String,
    device_public_key: String,
    vm_id: String,
    control: SocketIdentity,
    console: SocketIdentity,
    worker_pid: u64,
    state: Mutex<InputState>,
    closing: AtomicBool,
    cancel: watch::Sender<bool>,
    /// Timing of the last `observe_frame` whose reply has not been reported as sent yet, with its sequence.
    unsent: std::sync::Mutex<Option<(u64, ObserveTiming)>>,
}
impl SessionInput {
    /// Call only on explicit control use. Failure must not tear down valid observe-only media.
    pub async fn attach(
        auth: Arc<Auth>,
        session_id: &str,
        state_dir: PathBuf,
        media_console: PathBuf,
    ) -> Result<Arc<Self>> {
        auth.reload()?;
        auth.gate.authorize_active(session_id, "observe")?;
        let (transcript_sha256, device_public_key) = {
            let proofs = auth
                .close_proofs
                .lock()
                .map_err(|_| anyhow::anyhow!("session proof unavailable"))?;
            let proof = proofs
                .get(session_id)
                .context("session transcript unavailable")?;
            ensure!(
                proof.transcript.intent.challenge.vm_id == auth.vm_id,
                "session VM mismatch"
            );
            (proof.transcript.digest()?, proof.device_public_key.clone())
        };
        let meta = std::fs::symlink_metadata(&state_dir)?;
        ensure!(
            meta.is_dir()
                && !meta.file_type().is_symlink()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o077 == 0,
            "controller state must be private and owner-held"
        );
        let control = SocketIdentity::read(&state_dir.join("control.sock"))?;
        let console = SocketIdentity::read(&media_console)?;
        let status = rpc(
            &control,
            "status",
            json!({"id":auth.vm_id}),
            Duration::from_secs(3),
        )
        .await?;
        let worker_pid = validate_status(&status, &auth.vm_id, &console, None)?;
        auth.reload()?;
        auth.gate.authorize_active(session_id, "observe")?;
        let (cancel, _) = watch::channel(false);
        Ok(Arc::new(Self {
            vm_id: auth.vm_id.clone(),
            auth,
            session_id: session_id.into(),
            transcript_sha256,
            device_public_key,
            control,
            console,
            worker_pid,
            state: Mutex::new(InputState {
                sequence: 0,
                frames: VecDeque::new(),
                timing: None,
                png_only: false,
                helper_mode: false,
                helper_pictures: 0,
                helper_hint: 0,
                helper_hold: 0,
                source: None,
            }),
            closing: AtomicBool::new(false),
            cancel,
            unsent: std::sync::Mutex::new(None),
        }))
    }
    /// One action in flight, zero queued actions. A consumed sequence is never retried.
    pub async fn handle(&self, signed: SignedControl) -> Result<Value> {
        let entered = Instant::now();
        self.open()?;
        let mut state = FailureCode::Busy.on(self
            .state
            .try_lock()
            .context("input busy; no queued actions"))?;
        let request = &signed.request;
        let bytes = FailureCode::Malformed.on(request.canonical())?;
        FailureCode::RequestExpired.on(request.check_time(now()?))?;
        FailureCode::RequestBinding.on((|| {
            ensure!(
                request.session_id == self.session_id
                    && request.transcript_sha256 == self.transcript_sha256,
                "control transcript mismatch"
            );
            Ok(())
        })())?;
        self.authorize(request.action.required_scope())?;
        FailureCode::Signature.on(protocol::verify(
            &self.device_public_key,
            &bytes,
            &signed.signature,
        ))?;
        FailureCode::Replay.on((|| {
            ensure!(
                request.sequence > state.sequence,
                "control sequence replay or reordering"
            );
            Ok(())
        })())?;
        state.sequence = request.sequence;
        state.timing =
            matches!(request.action, ControlAction::ObserveFrame).then(|| ObserveTiming {
                entered: Some(entered),
                ..Default::default()
            });
        let mut cancelled = self.cancel.subscribe();
        let work = self.dispatch(&request.action, &mut state, request.expires_at);
        let result = tokio::select! {result=work=>result,_=async{if *cancelled.borrow(){return}let _=cancelled.changed().await;}=>Err(FailureCode::SessionEnded.tag(anyhow::anyhow!("input session ended; in-flight RPC cancelled")))};
        let outcome = result?;
        FailureCode::SessionEnded.on((|| {
            ensure!(
                !self.closing.load(Ordering::Acquire),
                "input session ended during operation"
            );
            Ok(())
        })())?;
        // The same closing and authority rechecks guard the typed "frame unavailable" reply: it is sent only
        // to a session that would also have been given the picture.
        self.authorize(request.action.required_scope())?;
        let timing = state.timing.take().map(|mut timing| {
            timing.total_us = entered.elapsed().as_micros() as u64;
            timing
        });
        Ok(match outcome {
            Outcome::Result(result) => {
                // The line is printed by `replied`, once the transport has the reply; a reply that was never
                // reported (the channel failed first) is printed without its send stage by the next one.
                if let Some(timing) = timing {
                    let previous = self
                        .unsent
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .replace((request.sequence, timing));
                    if let Some((_, previous)) = previous {
                        eprintln!("{}", timing_line(&previous, None));
                    }
                }
                json!({"sequence":request.sequence,"result":result})
            }
            Outcome::FrameUnavailable(reason) => {
                if let Some(timing) = timing {
                    eprintln!("{}", timing_line(&timing, None));
                }
                // Fixed words only; the controller's own error text is never printed.
                eprintln!("control frame unavailable reason={}", reason.name());
                json!({"sequence":request.sequence,"error":FRAME_UNAVAILABLE,"reason":reason.name()})
            }
        })
    }
    /// The reply to `sequence` was handed to the transport: print the timing line of that `observe_frame`.
    pub fn replied(&self, sequence: u64, reply: crate::control_channel::ReplyTiming) {
        let mut unsent = self
            .unsent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if unsent
            .as_ref()
            .is_some_and(|(pending, _)| *pending == sequence)
        {
            if let Some((_, timing)) = unsent.take() {
                eprintln!("{}", timing_line(&timing, Some(&reply)));
            }
        }
    }
    fn open(&self) -> Result<()> {
        if self.closing.load(Ordering::Acquire) {
            return Err(FailureCode::SessionEnded.tag(anyhow::anyhow!("input session ended")));
        }
        Ok(())
    }
    fn authorize(&self, scope: &str) -> Result<()> {
        FailureCode::InputAuthorization.on(self.auth.reload())?;
        FailureCode::InputAuthorization
            .on(self.auth.gate.authorize_active(&self.session_id, scope))?;
        Ok(())
    }
    async fn dispatch(
        &self,
        action: &ControlAction,
        state: &mut InputState,
        expires_at: u64,
    ) -> Result<Outcome> {
        if matches!(action, ControlAction::ObserveFrame) {
            return self.observe_frame(state, expires_at).await;
        }
        self.dispatch_action(action, state, expires_at)
            .await
            .map(Outcome::Result)
    }
    /// Identity, VM incarnation, authority and expiry checks shared by every action. Any failure here fences.
    async fn admit(&self, scope: &str, expires_at: u64) -> Result<()> {
        FailureCode::VmStatus.on(self.control.check())?;
        FailureCode::VmStatus.on(self.console.check())?;
        let status = rpc(
            &self.control,
            "status",
            json!({"id":self.vm_id}),
            Duration::from_secs(3),
        )
        .await?;
        FailureCode::VmStatus.on(validate_status(
            &status,
            &self.vm_id,
            &self.console,
            Some(self.worker_pid),
        ))?;
        self.open()?;
        self.authorize(scope)?;
        if expires_at <= now()? {
            return Err(FailureCode::RequestExpired
                .tag(anyhow::anyhow!("control request expired before dispatch")));
        }
        Ok(())
    }
    async fn observe_frame(&self, state: &mut InputState, expires_at: u64) -> Result<Outcome> {
        self.admit(ControlAction::ObserveFrame.required_scope(), expires_at)
            .await?;
        let mut timing = state.timing.take().unwrap_or_default();
        timing.admit_us = timing
            .entered
            .map_or(0, |entered| entered.elapsed().as_micros() as u64);
        // Only the capture and its transform may end as "frame unavailable"; `admit` above may not.
        let picture = capture_picture(
            &self.vm_id,
            state,
            &mut timing,
            |arguments, deadline| async move {
                let mut rpc = RpcTiming::default();
                let reply = self.screenshot(arguments, deadline, &mut rpc).await;
                (reply, rpc)
            },
        )
        .await;
        state.timing = Some(timing);
        let (result, anchor) = match picture {
            Ok(value) => value,
            Err(error) => {
                return match frame_unavailable(&error) {
                    Some(reason) => Ok(Outcome::FrameUnavailable(reason)),
                    None => Err(error),
                }
            }
        };
        state.source = result["width"].as_u64().zip(result["height"].as_u64());
        state.frames.push_back(Observation {
            anchor,
            received: Instant::now(),
        });
        while state.frames.len() > 16 {
            state.frames.pop_front();
        }
        Ok(Outcome::Result(result))
    }
    async fn dispatch_action(
        &self,
        action: &ControlAction,
        state: &mut InputState,
        expires_at: u64,
    ) -> Result<Value> {
        self.admit(action.required_scope(), expires_at).await?;
        ensure!(
            !matches!(action, ControlAction::ObserveFrame),
            "observe_frame is dispatched separately"
        );
        FailureCode::FrameAnchor.on((|| {
            let frame = action.frame().context("screenshot anchor required")?;
            let observation = state
                .frames
                .iter()
                .find(|v| v.anchor.frame_id == frame.frame_id)
                .context("screenshot does not belong to this input session")?;
            ensure!(
                &observation.anchor == frame,
                "screenshot geometry/generation substituted"
            );
            ensure!(
                observation.received.elapsed() <= OBSERVATION_TTL,
                "screenshot observation expired; obtain a new visible anchor"
            );
            Ok(())
        })())?;
        let (method, mut args) = FailureCode::Malformed.on(input_arguments(action))?;
        args["id"] = json!(self.vm_id);
        FailureCode::Malformed.on(vm_control::input::plan(method, &args))?;
        // The daemon rechecks the source frame generation at EVERY step.
        // QMP acknowledgement remains distinct from actual application acceptance.
        rpc(&self.control, method, args, Duration::from_secs(8)).await
    }
    /// One `screenshot` request within the picture's `deadline`; its times are added to `total`.
    async fn screenshot(
        &self,
        arguments: Value,
        deadline: Instant,
        total: &mut RpcTiming,
    ) -> Result<Value> {
        let mut timing = RpcTiming::default();
        let result = rpc_timed(
            &self.control,
            "screenshot",
            arguments,
            deadline.saturating_duration_since(Instant::now()),
            Some(&mut timing),
        )
        .await;
        total.wait_us += timing.wait_us;
        total.read_us += timing.read_us;
        total.parse_us += timing.parse_us;
        total.bytes += timing.bytes;
        result
    }
    /// Called on media close/revocation/disconnect. Cancels any in-flight RPC;
    /// the daemon releases held input at the end of every input operation.
    pub fn end(&self) {
        self.closing.store(true, Ordering::Release);
        let _ = self.cancel.send(true);
    }
}
fn validate_status(
    status: &Value,
    vm: &str,
    console: &SocketIdentity,
    pid: Option<u64>,
) -> Result<u64> {
    ensure!(status["config"]["id"]==vm&&status["config"]["backend"]=="qemu-hvf"&&status["state"]=="running"&&status["capabilities"]["input"]==true,"unsupported: browser input requires this running QEMU persistent console; macOS input is not implemented");
    let actual = SocketIdentity::read(Path::new(
        status["runtime"]["console_socket"]
            .as_str()
            .context("VM console socket missing")?,
    ))?;
    ensure!(
        actual.path == console.path && actual.dev == console.dev && actual.ino == console.ino,
        "media/VM console mismatch"
    );
    let current = status["pid"]
        .as_u64()
        .context("VM worker identity unavailable")?;
    ensure!(
        pid.is_none_or(|old| old == current),
        "VM worker incarnation changed"
    );
    Ok(current)
}
/// Capture policy shared with the scripted controller tests. All policy writes are committed together after
/// the transform and anchor verification, so refusals, timeouts and cancellation leave it unchanged.
async fn capture_picture<F, Fut>(
    vm_id: &str,
    state: &mut InputState,
    timing: &mut ObserveTiming,
    screenshot: F,
) -> Result<(Value, FrameAnchor)>
where
    F: FnMut(Value, Instant) -> Fut,
    Fut: std::future::Future<Output = (Result<Value>, RpcTiming)>,
{
    capture_picture_with_transform(
        vm_id,
        state,
        timing,
        screenshot,
        tokio::time::Instant::now() + Duration::from_secs(16),
        transform_picture,
    )
    .await
}
async fn capture_picture_with_transform<F, Fut, T>(
    vm_id: &str,
    state: &mut InputState,
    timing: &mut ObserveTiming,
    mut screenshot: F,
    mut deadline: tokio::time::Instant,
    transform: T,
) -> Result<(Value, FrameAnchor)>
where
    F: FnMut(Value, Instant) -> Fut,
    Fut: std::future::Future<Output = (Result<Value>, RpcTiming)>,
    T: FnOnce(Shot) -> (Result<Value>, control_frame::FrameTiming) + Send + 'static,
{
    let mut png_only = state.png_only;
    let mut helper_mode = state.helper_mode;
    let mut helper_pictures = state.helper_pictures;
    let mut helper_hint = state.helper_hint;
    let mut helper_hold = state.helper_hold;
    let started = tokio::time::Instant::now();
    let fresh_until = timing
        .entered
        .map(tokio::time::Instant::from_std)
        .unwrap_or(started)
        + Duration::from_secs(4);
    let mut requests = 0u32;
    let mut shot = None;
    let mut png_failure = None;
    let helper_first = !png_only && helper_mode && helper_pictures < PNG_PROBE_EVERY;
    if !helper_first {
        requests += 1;
        // A timed-out RPC does not cancel the capture: only a definite refusal may try the helper.
        match capture_request(
            &mut screenshot,
            json!({"id":vm_id}),
            deadline,
            &mut timing.rpc,
        )
        .await
        {
            Ok(reply) => shot = Some(Shot::Png(reply)),
            Err(error)
                if !png_only
                    && matches!(
                        FailureCode::of(&error),
                        Some(FailureCode::ControllerRefused)
                    ) =>
            {
                png_failure = Some(error);
                deadline = deadline.min(fresh_until);
            }
            Err(error) => return Err(error),
        }
    }
    // The pair of the helper's picture and whether it may fit one pair better; applied once it is verified.
    let mut fitted = None;
    // The helper's own scaled JPEG (decision 0025 item 2), in the order the transform would try its divisor
    // and quality pairs; each `encoded_size_limit` refusal moves one pair on.
    if shot.is_none() && !png_only {
        // At session start a refused PNG provides no source geometry. The helper accepts size bounds,
        // so use its maximum display size to budget three full-size and three smaller requests.
        let steps = control_frame::helper_steps(state.source.or(Some((
            vm_control::screenshot_scaled::MAX_WIDTH,
            vm_control::screenshot_scaled::MAX_HEIGHT,
        ))));
        let last = steps.len().saturating_sub(1);
        let first = helper_hint.min(last);
        for (index, step) in steps.into_iter().enumerate().skip(first) {
            let mut arguments = json!({"id":vm_id});
            step.scaled.apply(&mut arguments);
            requests += 1;
            match capture_request(&mut screenshot, arguments, deadline, &mut timing.rpc).await {
                Ok(reply) if reply["mime_type"] == "image/jpeg" => {
                    // A picture under half the bound may fit one pair better: try that one next time.
                    let small = reply["byte_length"].as_u64().is_some_and(|bytes| {
                        bytes * 2 < control_frame::CONTROL_FRAME_MAX_BYTES as u64
                    });
                    fitted = Some((index, small));
                    shot = Some(Shot::Helper(reply, step));
                    break;
                }
                // A controller or helper without the scaled form ignores the fields and sends the PNG:
                // use it for this picture and do not ask again in this session.
                Ok(reply) => {
                    png_only = true;
                    shot = Some(Shot::Png(reply));
                    break;
                }
                Err(error) => match error.downcast_ref::<ScaledRefused>() {
                    Some(refused) => match refused.refusal {
                        // Also when every pair is refused: the next picture starts at the last one.
                        Refusal::EncodedSizeLimit => {
                            helper_hint = (index + 1).min(last);
                            helper_hold = HINT_HOLD;
                        }
                        Refusal::InvalidRequest | Refusal::FormatUnavailable => {
                            png_only = true;
                            break;
                        }
                        // This geometry has no frame of the size asked for: the PNG path for this picture.
                        Refusal::ScaleUnavailable => break,
                    },
                    None => return Err(error),
                },
            }
        }
    }
    let shot = match shot {
        Some(shot) => shot,
        None => {
            // The PNG was already tried before the helper: never loop between the two forms.
            if let Some(error) = png_failure {
                return Err(error);
            }
            requests += 1;
            Shot::Png(
                capture_request(
                    &mut screenshot,
                    json!({"id":vm_id}),
                    deadline,
                    &mut timing.rpc,
                )
                .await?,
            )
        }
    };
    // Decode and re-encode off the async workers; the media pipeline shares this runtime.
    let started = Instant::now();
    // Dropping the handle on expiry leaves the blocking job to finish without committing its result.
    let worker = tokio::task::spawn_blocking(move || transform(shot));
    let completed =
        FailureCode::ControllerDeadline.on(tokio::time::timeout_at(deadline, worker)
            .await
            .context("control picture freshness deadline exceeded"))?;
    let (result, frame) =
        FailureCode::FrameWorker.on(completed.context("control frame worker failed"))?;
    timing.worker_us = started.elapsed().as_micros() as u64;
    timing.frame = frame;
    timing.frame.requests = requests;
    // Only a picture that was accepted changes what the next one asks for.
    let result = result?;
    match fitted {
        Some((index, small)) => {
            let better = small && helper_hold == 0;
            helper_hint = index.saturating_sub(better as usize);
            helper_hold = helper_hold.saturating_sub(1);
            helper_mode = true;
            helper_pictures = if png_failure.is_some() {
                0
            } else {
                helper_pictures + 1
            };
        }
        // The PNG decided: above the bound, the helper makes the following pictures; within it, they stay
        // PNG. The hint is kept: a PNG taken because the helper refused says nothing against it.
        None => {
            helper_mode = !timing.frame.passthrough;
            helper_pictures = 0;
        }
    }
    let started = Instant::now();
    let anchor = FailureCode::FrameVerification.on(frame_from_screenshot(&result))?;
    timing.verify_us = started.elapsed().as_micros() as u64;
    if tokio::time::Instant::now() >= deadline {
        return Err(FailureCode::ControllerDeadline.tag(anyhow::anyhow!(
            "control picture freshness deadline exceeded"
        )));
    }
    state.png_only = png_only;
    state.helper_mode = helper_mode;
    state.helper_pictures = helper_pictures;
    state.helper_hint = helper_hint;
    state.helper_hold = helper_hold;
    Ok((result, anchor))
}
fn transform_picture(shot: Shot) -> (Result<Value>, control_frame::FrameTiming) {
    let mut frame = control_frame::FrameTiming::default();
    let result = match shot {
        Shot::Helper(reply, step) => {
            control_frame::control_frame_from_helper_jpeg(reply, &step, &mut frame)
        }
        Shot::Png(reply) => control_frame::control_frame_from_screenshot_timed(reply, &mut frame),
    };
    (result, frame)
}
async fn capture_request<F, Fut>(
    screenshot: &mut F,
    arguments: Value,
    deadline: tokio::time::Instant,
    total: &mut RpcTiming,
) -> Result<Value>
where
    F: FnMut(Value, Instant) -> Fut,
    Fut: std::future::Future<Output = (Result<Value>, RpcTiming)>,
{
    if tokio::time::Instant::now() >= deadline {
        return Err(
            FailureCode::ControllerDeadline.tag(anyhow::anyhow!("controller deadline exceeded"))
        );
    }
    let (result, timing) = FailureCode::ControllerDeadline.on(tokio::time::timeout_at(
        deadline,
        screenshot(arguments, deadline.into_std()),
    )
    .await
    .context("controller deadline exceeded"))?;
    total.wait_us += timing.wait_us;
    total.read_us += timing.read_us;
    total.parse_us += timing.parse_us;
    total.bytes += timing.bytes;
    result
}
fn frame_from_screenshot(value: &Value) -> Result<FrameAnchor> {
    // The 700 KiB transport bound is unchanged. A larger controller PNG has already been re-encoded as a scaled
    // JPEG by `control_frame_from_screenshot`; that form is accepted only when its declared scale, its real image
    // dimensions, its hash and the source geometry all agree. `width`/`height` stay the SOURCE geometry.
    ensure!(
        (value["mime_type"] == "image/png" || value["mime_type"] == "image/jpeg")
            && value["byte_length"]
                .as_u64()
                .is_some_and(|v| v > 0 && v <= control_frame::CONTROL_FRAME_MAX_BYTES as u64)
            && value["data_base64"]
                .as_str()
                .is_some_and(|s| s.len() <= control_frame::CONTROL_FRAME_MAX_BASE64),
        "controller screenshot is not a bounded QEMU PNG"
    );
    control_frame::verify_control_frame(value)?;
    let frame: FrameAnchor = serde_json::from_value(
        json!({"frame_id":value["frame_id"],"source_generation":value["source_generation"],"width":value["width"],"height":value["height"]}),
    )?;
    let probe = protocol::control::ControlRequest {
        session_id: "validate".into(),
        transcript_sha256: "00".repeat(32),
        sequence: 1,
        expires_at: 0,
        action: ControlAction::Key {
            frame: frame.clone(),
            keys: vec!["a".into()],
        },
    };
    probe.canonical()?;
    Ok(frame)
}
fn input_arguments(action: &ControlAction) -> Result<(&'static str, Value)> {
    let frame = action.frame().context("missing screenshot")?;
    let mut args = serde_json::to_value(frame)?;
    let method = match action {
        ControlAction::Click { x, y, button, .. } => {
            args["x"] = json!(x);
            args["y"] = json!(y);
            args["button"] = json!(button);
            "input_click"
        }
        ControlAction::Drag {
            x,
            y,
            end_x,
            end_y,
            button,
            ..
        } => {
            args["x"] = json!(x);
            args["y"] = json!(y);
            args["end_x"] = json!(end_x);
            args["end_y"] = json!(end_y);
            args["button"] = json!(button);
            "input_drag"
        }
        ControlAction::Scroll {
            x,
            y,
            direction,
            ticks,
            ..
        } => {
            args["x"] = json!(x);
            args["y"] = json!(y);
            args["direction"] = json!(direction);
            args["ticks"] = json!(ticks);
            "input_scroll"
        }
        ControlAction::Key { keys, .. } => {
            args["keys"] = json!(keys);
            "input_key"
        }
        _ => bail!("not a physical input action"),
    };
    Ok((method, args))
}
async fn rpc(
    socket: &SocketIdentity,
    method: &str,
    args: Value,
    deadline: Duration,
) -> Result<Value> {
    rpc_timed(socket, method, args, deadline, None).await
}
/// `rpc`, also recording where the time of the exchange went.
async fn rpc_timed(
    socket: &SocketIdentity,
    method: &str,
    args: Value,
    deadline: Duration,
    mut timing: Option<&mut RpcTiming>,
) -> Result<Value> {
    let started = Instant::now();
    FailureCode::ControllerUnavailable.on(socket.check())?;
    let bytes = serde_json::to_vec(&json!({"method":method,"arguments":args}))?;
    ensure!(bytes.len() < 64 * 1024, "controller request limit exceeded");
    let screenshot = method == "screenshot";
    let exchange = tokio::time::timeout(deadline, async {
        let frame = FailureCode::ControllerUnavailable.on(async {
            let mut stream = UnixStream::connect(&socket.path)
                .await
                .context("private controller unavailable")?;
            stream.write_all(&bytes).await?;
            stream.write_all(b"\n").await?;
            let mut frame = Vec::new();
            let limit = if screenshot {
                SCREENSHOT_RPC_LIMIT
            } else {
                RPC_LIMIT
            };
            // At most `limit + 1` bytes are ever read, whatever the controller sends.
            let mut input = BufReader::new(stream).take(limit + 1);
            // Wait for the first reply byte separately: until then the time is the controller's.
            input.fill_buf().await?;
            let waited = started.elapsed();
            let count = input.read_until(b'\n', &mut frame).await?;
            if let Some(timing) = timing.as_deref_mut() {
                timing.wait_us = waited.as_micros() as u64;
                timing.read_us = (started.elapsed() - waited).as_micros() as u64;
                timing.bytes = count;
            }
            if count > limit as usize {
                let code = if screenshot {
                    FailureCode::ScreenshotReplyTooLarge
                } else {
                    FailureCode::ControllerReplyInvalid
                };
                return Err(code.tag(anyhow::anyhow!(
                    "controller response missing or exceeds limit"
                )));
            }
            if count == 0 || frame.last() != Some(&b'\n') {
                return Err(FailureCode::ControllerReplyInvalid.tag(anyhow::anyhow!(
                    "controller response missing or exceeds limit"
                )));
            }
            Ok::<_, anyhow::Error>(frame)
        }
        .await)?;
        let parsing = Instant::now();
        let mut value: Value =
            FailureCode::ControllerReplyInvalid.on(serde_json::from_slice(&frame))?;
        drop(frame);
        if let Some(timing) = timing.as_deref_mut() {
            timing.parse_us = parsing.elapsed().as_micros() as u64;
        }
        if value.get("error").is_some() {
            // A refused scaled screenshot: the same code and text, with the controller's fixed word attached.
            if let Some(refusal) = Refusal::from_name(&value[REFUSAL_FIELD]).filter(|_| screenshot)
            {
                return Err(anyhow::Error::new(FailureCode::ControllerRefused).context(
                    ScaledRefused {
                        refusal,
                        message: format!("controller rejected operation: {}", value["error"]),
                    },
                ));
            }
            // The text is the controller's; it stays in the error for callers and is never logged.
            return Err(FailureCode::ControllerRefused.tag(anyhow::anyhow!(
                "controller rejected operation: {}",
                value["error"]
            )));
        };
        FailureCode::ControllerReplyInvalid.on(value
            .get_mut("result")
            .map(Value::take)
            .context("controller response missing result"))
    })
    .await;
    FailureCode::ControllerDeadline
        .on(exchange.context("controller deadline exceeded; command completion unknown"))?
}

impl Drop for SessionInput {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
    }
}

#[cfg(test)]
#[path = "browser_input_tests.rs"]
mod tests;
