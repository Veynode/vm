//! Host-only adapter from signed control requests to the existing QEMU controller.
//! HTTP exposure remains loopback-only. Signatures are not payload encryption.
use super::{now, Auth};
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
use vm_session_auth::{
    self as protocol,
    control::{ControlAction, FrameAnchor, SignedControl},
};
const RPC_LIMIT: u64 = 1024 * 1024;
const OBSERVATION_TTL: Duration = Duration::from_secs(5);

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
            }),
            closing: AtomicBool::new(false),
            cancel,
        }))
    }
    /// One action in flight, zero queued actions. A consumed sequence is never retried.
    pub async fn handle(&self, signed: SignedControl) -> Result<Value> {
        ensure!(!self.closing.load(Ordering::Acquire), "input session ended");
        let mut state = self
            .state
            .try_lock()
            .context("input busy; no queued actions")?;
        let request = &signed.request;
        let bytes = request.canonical()?;
        request.check_time(now()?)?;
        ensure!(
            request.session_id == self.session_id
                && request.transcript_sha256 == self.transcript_sha256,
            "control transcript mismatch"
        );
        self.auth.reload()?;
        self.auth
            .gate
            .authorize_active(&self.session_id, request.action.required_scope())?;
        protocol::verify(&self.device_public_key, &bytes, &signed.signature)?;
        ensure!(
            request.sequence > state.sequence,
            "control sequence replay or reordering"
        );
        state.sequence = request.sequence;
        let mut cancelled = self.cancel.subscribe();
        let work = self.dispatch(&request.action, &mut state, request.expires_at);
        let result = tokio::select! {result=work=>result,_=async{if *cancelled.borrow(){return}let _=cancelled.changed().await;}=>Err(anyhow::anyhow!("input session ended; in-flight RPC cancelled"))};
        let result = result?;
        ensure!(
            !self.closing.load(Ordering::Acquire),
            "input session ended during operation"
        );
        self.auth.reload()?;
        self.auth
            .gate
            .authorize_active(&self.session_id, request.action.required_scope())?;
        Ok(json!({"sequence":request.sequence,"result":result}))
    }
    async fn dispatch(
        &self,
        action: &ControlAction,
        state: &mut InputState,
        expires_at: u64,
    ) -> Result<Value> {
        self.control.check()?;
        self.console.check()?;
        let status = rpc(
            &self.control,
            "status",
            json!({"id":self.vm_id}),
            Duration::from_secs(3),
        )
        .await?;
        validate_status(&status, &self.vm_id, &self.console, Some(self.worker_pid))?;
        ensure!(!self.closing.load(Ordering::Acquire), "input session ended");
        self.auth.reload()?;
        self.auth
            .gate
            .authorize_active(&self.session_id, action.required_scope())?;
        ensure!(
            expires_at > now()?,
            "control request expired before dispatch"
        );
        match action {
            ControlAction::ObserveFrame => {
                let result = rpc(
                    &self.control,
                    "screenshot",
                    json!({"id":self.vm_id}),
                    Duration::from_secs(16),
                )
                .await?;
                let anchor = frame_from_screenshot(&result)?;
                state.frames.push_back(Observation {
                    anchor,
                    received: Instant::now(),
                });
                while state.frames.len() > 16 {
                    state.frames.pop_front();
                }
                Ok(result)
            }
            _ => {
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
                let (method, mut args) = input_arguments(action)?;
                args["id"] = json!(self.vm_id);
                vm_control::input::plan(method, &args)?;
                // The daemon rechecks the source frame generation at EVERY step.
                // QMP acknowledgement remains distinct from actual application acceptance.
                rpc(&self.control, method, args, Duration::from_secs(8)).await
            }
        }
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
fn frame_from_screenshot(value: &Value) -> Result<FrameAnchor> {
    ensure!(
        value["mime_type"] == "image/png"
            && value["byte_length"]
                .as_u64()
                .is_some_and(|v| v > 0 && v <= 700 * 1024)
            && value["data_base64"]
                .as_str()
                .is_some_and(|s| s.len() <= 956_000),
        "controller screenshot is not a bounded QEMU PNG"
    );
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
    socket.check()?;
    let bytes = serde_json::to_vec(&json!({"method":method,"arguments":args}))?;
    ensure!(bytes.len() < 64 * 1024, "controller request limit exceeded");
    tokio::time::timeout(deadline, async {
        let mut stream = UnixStream::connect(&socket.path)
            .await
            .context("private controller unavailable")?;
        stream.write_all(&bytes).await?;
        stream.write_all(b"\n").await?;
        let mut frame = Vec::new();
        let mut input = BufReader::new(stream).take(RPC_LIMIT + 1);
        let count = input.read_until(b'\n', &mut frame).await?;
        ensure!(
            count > 0 && count <= RPC_LIMIT as usize && frame.last() == Some(&b'\n'),
            "controller response missing or exceeds limit"
        );
        let value: Value = serde_json::from_slice(&frame)?;
        if value.get("error").is_some() {
            bail!("controller rejected operation: {}", value["error"])
        };
        value
            .get("result")
            .cloned()
            .context("controller response missing result")
    })
    .await
    .context("controller deadline exceeded; command completion unknown")?
}

impl Drop for SessionInput {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
    }
}

#[cfg(test)]
#[path = "browser_input_tests.rs"]
mod tests;
