//! Pause, resume and the idle pause.
//!
//! `pause` and `resume` stop and restart a running VM's vCPUs (Apple
//! `VZVirtualMachine.pause`, QEMU `stop`/`cont`): the Guest's memory stays
//! resident, so this saves CPU and power, not memory. A VM given an idle
//! policy (`idle_pause_s`, at `start` or with `idle_policy`) is paused by the
//! daemon after that long without activity, and resumed by the next request
//! that needs its Guest. Activity is what the host sees: every VM method that
//! is not [`passive`], an open console or desktop viewer, running Guest jobs,
//! and the `wake` heartbeats long-lived streams and the port publisher send.
//! Work inside the Guest that nobody on the host is waiting for is not seen.
//!
//! A VM paused with `pause` stays paused until `resume`: other methods are
//! refused, except `shutdown`, which resumes it first.
//!
//! The idle hibernation (`idle_hibernate_s`, Apple Linux VMs with a fixed
//! MAC and machine identifier) goes further: the worker saves the machine
//! state to [`container_state::hibernation_path`] and exits, which frees the
//! Guest's memory. While that file exists and no worker runs, the VM reports
//! `hibernated`; `start`, `resume`, `wake` and every method that needs the
//! Guest restore it from the file, which goes once the restored VM runs (or
//! its restore fails). `shutdown` and `force_stop` discard it.
use super::*;
use std::time::Instant;

/// How often the monitor looks for VMs to pause.
pub(crate) const PASS: Duration = Duration::from_secs(10);
pub(crate) const MIN_SECONDS: u64 = 60;
pub(crate) const MAX_SECONDS: u64 = 7 * 24 * 3600;
/// A pause or resume the worker has not reported within this long did not happen.
const SETTLE: Duration = Duration::from_secs(15);
/// The longest `keep_awake`.
pub(crate) const MAX_HOLD_SECONDS: u64 = 24 * 3600;
/// A Guest whose one-minute load is at least this is busy (`activity`).
const GUEST_BUSY_LOAD: f64 = 0.5;
/// The refusal while the worker saves; `vm_result` waits it out and retries.
pub(crate) const HIBERNATING: &str = "VM is hibernating; try again once it has stopped";
/// How long `vm_result` waits for a save before it passes the refusal on.
pub(crate) const SAVE_WAIT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PausedBy {
    Idle,
    Request,
}
impl PausedBy {
    fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Request => "request",
        }
    }
}

/// One VM's idle policy and what the daemon last asked of it.
#[derive(Debug)]
pub(crate) struct Idle {
    pub(crate) after: Option<Duration>,
    /// Quiet time before the VM is hibernated.
    pub(crate) hibernate: Option<Duration>,
    pub(crate) last: Instant,
    /// Who asked for the pause, and when.
    pub(crate) paused: Option<(PausedBy, Instant)>,
    /// The worker's reply to `hibernate`, while it saves.
    pub(crate) hibernating: Option<mpsc::Receiver<Value>>,
    /// The worker was started from the hibernation file.
    pub(crate) restoring: bool,
    /// `keep_awake`: the VM counts as busy until then.
    pub(crate) hold: Option<Instant>,
    /// The Guest's answer to `activity`, asked once a pause or hibernation is due.
    pub(crate) probe: Option<(Instant, mpsc::Receiver<Value>)>,
}
impl Idle {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            after: None,
            hibernate: None,
            last: now,
            paused: None,
            hibernating: None,
            restoring: false,
            hold: None,
            probe: None,
        }
    }
    pub(crate) fn held_awake(&self, now: Instant) -> bool {
        self.hold.is_some_and(|until| now < until)
    }
    /// Who holds the VM paused: the daemon asked, and the worker reports it.
    pub(crate) fn held(&self, state: &str) -> Option<PausedBy> {
        (state == "paused").then_some(self.paused?.0)
    }
    /// Whether the monitor pauses the VM now.
    pub(crate) fn due(&self, state: &str, busy: bool, now: Instant) -> bool {
        self.after.is_some_and(|after| {
            state == "running"
                && self.paused.is_none()
                && !busy
                && now.duration_since(self.last) >= after
        })
    }
    /// Whether the monitor hibernates the VM now: quiet for its policy,
    /// running or paused by the idle monitor (not on request).
    pub(crate) fn hibernate_due(&self, state: &str, busy: bool, now: Instant) -> bool {
        self.hibernate.is_some_and(|after| {
            self.hibernating.is_none()
                && !busy
                && match state {
                    "running" => self.paused.is_none(),
                    "paused" => self.held(state) == Some(PausedBy::Idle),
                    _ => false,
                }
                && now.duration_since(self.last) >= after
        })
    }
    /// Forgets a pause the worker never reported, or one undone outside the
    /// daemon (a snapshot resumes its source).
    pub(crate) fn settle(&mut self, state: &str, now: Instant) {
        if self
            .paused
            .is_some_and(|(_, at)| state != "paused" && now.duration_since(at) >= SETTLE)
        {
            self.paused = None;
        }
    }
}

/// Methods that are no activity and work on a paused VM: they read the
/// daemon's records, change the VM's lifecycle, or close a viewer.
pub(crate) fn passive(method: &str) -> bool {
    matches!(
        method,
        "status"
            | "capabilities"
            | "list"
            | "events"
            | "log_read"
            | "control_lease"
            | "create"
            | "start"
            | "force_stop"
            | "delete"
            | "close_window"
            | "window_status"
            | "pause"
            | "resume"
            | "wake"
            | "idle_policy"
            | "sleep"
            | "keep_awake"
    )
}

/// Whether the Guest's `activity` reply shows work in progress.
pub(crate) fn guest_busy(reply: &Value) -> bool {
    reply["result"]["result"]["load1"]
        .as_f64()
        .is_some_and(|load| load >= GUEST_BUSY_LOAD)
}

/// What the monitor knows of the Guest's own work.
enum GuestWork {
    Asked,
    Busy,
    Quiet,
}

/// `idle_pause_s`: absent, null or 0 is no idle pause.
pub(crate) fn policy(args: &Value) -> Result<Option<Duration>> {
    seconds(args, "idle_pause_s")
}

/// `idle_hibernate_s`, the same way, for a VM that can be hibernated.
pub(crate) fn hibernate_policy(args: &Value, config: &Config) -> Result<Option<Duration>> {
    let after = seconds(args, "idle_hibernate_s")?;
    if after.is_some() {
        container_state::hibernation_path(config).context("idle_hibernate_s")?;
    }
    Ok(after)
}

pub(crate) fn seconds(args: &Value, key: &str) -> Result<Option<Duration>> {
    match &args[key] {
        Value::Null => Ok(None),
        value => match value.as_u64() {
            Some(0) => Ok(None),
            Some(seconds) if (MIN_SECONDS..=MAX_SECONDS).contains(&seconds) => {
                Ok(Some(Duration::from_secs(seconds)))
            }
            _ => bail!("{key} must be 0 or {MIN_SECONDS}..{MAX_SECONDS} seconds"),
        },
    }
}

impl Manager {
    /// The worker's reported state, while its process is alive.
    fn live_state(&mut self, id: &str) -> Option<String> {
        let run = self.running.get_mut(id)?;
        if !matches!(run.poll_exit(), Ok(None)) {
            return None;
        }
        let state = run.state.lock().unwrap()["state"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned();
        Some(state)
    }

    fn write_command(
        &mut self,
        id: &str,
        mut command: Value,
    ) -> Result<(u64, mpsc::Receiver<Value>)> {
        let run = self.running.get_mut(id).context("VM process not active")?;
        let number = run.next;
        run.next += 1;
        command["id"] = json!(number);
        let (tx, rx) = mpsc::channel();
        let limit = response_limit(command["method"].as_str().unwrap_or_default());
        run.pending
            .lock()
            .unwrap()
            .insert(number, PendingReply { sender: tx, limit });
        if let Err(error) = write_json(&mut run.stdin, &command) {
            run.pending.lock().unwrap().remove(&number);
            return Err(error);
        }
        Ok((number, rx))
    }

    /// A worker command whose reply the caller awaits.
    fn forward(&mut self, id: &str, method: &str) -> Result<Completion> {
        let (number, receiver) = self.write_command(id, json!({"method":method}))?;
        Ok(Completion::Backend(Pending {
            receiver,
            number,
            pending: self.running[id].pending.clone(),
            timeout: Duration::from_secs(10),
            post: None,
        }))
    }

    /// Sends `resume`, then steps an Apple Linux Guest's wall clock to the
    /// host's (`clock_sync`; its reply is not awaited, and a Guest without the
    /// agent answers an error nobody reads). The Apple worker holds commands
    /// that arrive while it resumes, so whatever follows runs on a running VM.
    fn resume(&mut self, id: &str, reason: &str) -> Result<Completion> {
        if let Some(idle) = self.idle.get_mut(id) {
            idle.paused = None;
            idle.last = Instant::now();
        }
        let completion = self.forward(id, "resume")?;
        let config = &self.configs[id];
        if config.backend == Backend::AppleVirtualization && config.guest == "linux" {
            let number = self.running[id].next;
            let request = container_state::clock_request(number)?;
            let _ = self.write_command(id, json!({"method":"guest_request","request":request}));
        }
        self.events
            .publish(id, "resumed", json!({"state":"running","reason":reason}));
        Ok(completion)
    }

    /// The hibernation file of a VM no worker runs, which its next start restores.
    pub(crate) fn hibernated(&mut self, id: &str) -> Option<PathBuf> {
        self.settle_restore(id);
        if self.live_state(id).is_some() {
            return None;
        }
        let path = container_state::hibernation_path(self.configs.get(id)?).ok()?;
        path.is_file().then_some(path)
    }

    /// The hibernation file is used once: it goes when the VM restored from
    /// it runs, or when its worker exits before that.
    fn settle_restore(&mut self, id: &str) {
        if !self.idle.get(id).is_some_and(|idle| idle.restoring) {
            return;
        }
        let state = self.live_state(id);
        if state.as_deref().is_some_and(|state| state != "running") {
            return;
        }
        self.idle.get_mut(id).unwrap().restoring = false;
        if let Ok(path) = container_state::hibernation_path(&self.configs[id]) {
            let _ = fs::remove_file(path);
        }
        match state {
            Some(_) => self.events.publish(
                id,
                "restored",
                json!({"state":"running","from":"hibernation"}),
            ),
            None => log_line(&format!(
                "vm-control: restore of {id} from hibernation failed; its next start boots afresh"
            )),
        }
    }

    /// Reads the worker's answer to `hibernate`: a refusal leaves the VM as it was.
    fn settle_hibernating(&mut self, id: &str) {
        let Some(idle) = self.idle.get_mut(id) else {
            return;
        };
        let reply = match idle.hibernating.as_ref().map(mpsc::Receiver::try_recv) {
            None | Some(Err(mpsc::TryRecvError::Empty)) => return,
            Some(reply) => reply,
        };
        idle.hibernating = None;
        let Ok(reply) = reply else {
            return;
        };
        if reply.get("error").is_some() {
            if let Some(run) = self.running.get_mut(id) {
                run.expected_exit = false;
            }
            log_line(&format!(
                "vm-control: idle hibernation of {id} failed: {}",
                reply["error"]
            ));
            self.events
                .publish(id, "hibernate_failed", json!({"error":reply["error"]}));
        }
    }

    /// Asks the worker to save the machine state and exit.
    fn hibernate(&mut self, id: &str, reason: &str) -> Result<()> {
        // A restore the monitor has not settled yet: its file is the old state,
        // and the flag would make the new one look like a failed restore.
        self.settle_restore(id);
        let path = container_state::hibernation_path(&self.configs[id])?;
        if self.idle.get(id).is_some_and(|idle| idle.restoring) {
            bail!("VM is busy: it is still restoring")
        }
        // A file no restore removed would make the worker refuse.
        let _ = fs::remove_file(&path);
        let (_, reply) = self.write_command(id, json!({"method":"hibernate","path":path}))?;
        self.running.get_mut(id).unwrap().expected_exit = true;
        let idle = self.idle.get_mut(id).unwrap();
        idle.hibernating = Some(reply);
        idle.paused = None;
        self.events
            .publish(id, "hibernating", json!({ "reason": reason }));
        Ok(())
    }

    /// Starts a hibernated VM: `start` restores it from its file.
    fn restore_hibernated(&mut self, id: &str) -> Result<()> {
        self.dispatch(json!({"method":"start","arguments":{"id":id}}), &mut None)?;
        Ok(())
    }

    fn discard_hibernation(&mut self, id: &str, path: &Path) -> Result<()> {
        fs::remove_file(path).context("discarding the hibernation file")?;
        self.events
            .publish(id, "hibernation_discarded", json!({"state":"stopped"}));
        Ok(())
    }

    /// The idle part of `start`: a fresh activity clock, the policies the
    /// request names and, for a restore from hibernation, a clock step queued
    /// behind the restore.
    pub(crate) fn idle_started(
        &mut self,
        id: &str,
        args: &Value,
        after: Option<Duration>,
        hibernate: Option<Duration>,
        restoring: bool,
    ) -> Result<()> {
        let now = Instant::now();
        let idle = self.idle.entry(id.into()).or_insert_with(|| Idle::new(now));
        idle.paused = None;
        idle.hibernating = None;
        idle.restoring = restoring;
        idle.last = now;
        if args.get("idle_pause_s").is_some() {
            idle.after = after;
        }
        if args.get("idle_hibernate_s").is_some() {
            idle.hibernate = hibernate;
        }
        if restoring {
            let number = self.running[id].next;
            let request = container_state::clock_request(number)?;
            let _ = self.write_command(id, json!({"method":"guest_request","request":request}));
            self.events
                .publish(id, "restoring", json!({"from":"hibernation"}));
        }
        Ok(())
    }

    /// Before any other VM method: it is activity, and a VM the idle monitor
    /// paused is resumed for it, a hibernated one restored. A VM paused on
    /// request refuses it.
    fn idle_gate(&mut self, method: &str, id: &str) -> Result<Option<Value>> {
        if passive(method) || !self.configs.contains_key(id) {
            return Ok(None);
        }
        self.settle_hibernating(id);
        let Some(state) = self.live_state(id) else {
            if let Some(path) = self.hibernated(id) {
                if method == "shutdown" {
                    self.discard_hibernation(id, &path)?;
                    return Ok(Some(json!({"state":"stopped","hibernation":"discarded"})));
                }
                // The worker holds the request until the restored VM runs.
                self.restore_hibernated(id)?;
            }
            return Ok(None);
        };
        if state == "hibernated" || self.idle.get(id).is_some_and(|i| i.hibernating.is_some()) {
            bail!(HIBERNATING)
        }
        let now = Instant::now();
        let idle = self.idle.entry(id.into()).or_insert_with(|| Idle::new(now));
        idle.last = now;
        match idle.held(&state) {
            None => Ok(None),
            Some(PausedBy::Request) if method == "show_window" => Ok(None),
            Some(PausedBy::Request) if method != "shutdown" => {
                bail!("VM is paused; resume it first")
            }
            Some(by) => {
                // The request goes out right after; its reply, not resume's, is awaited.
                drop(self.resume(id, by.name())?);
                Ok(None)
            }
        }
    }

    /// Native console traffic (`native_control`): a focused window's
    /// heartbeats and its input are activity, and input resumes a VM the idle
    /// monitor paused. A VM paused on request takes it as it is.
    pub(crate) fn native_activity(&mut self, id: &str, input: bool) {
        let now = Instant::now();
        let Some(state) = self.live_state(id) else {
            return;
        };
        let Some(idle) = self.idle.get_mut(id) else {
            return;
        };
        idle.last = now;
        if input && idle.held(&state) == Some(PausedBy::Idle) {
            if let Err(error) = self.resume(id, "idle") {
                log_line(&format!(
                    "vm-control: resume of {id} for console input failed: {error:#}"
                ));
            }
        }
    }

    /// `pause`, `resume`, `wake` and `idle_policy`, and the gate in front of
    /// every other method. `Some` answers the method (with `deferred` set
    /// when the worker answers it).
    pub(crate) fn idle_dispatch(
        &mut self,
        method: &str,
        args: &Value,
        deferred: &mut Option<Completion>,
    ) -> Result<Option<Value>> {
        let Some(id) = args["id"].as_str() else {
            return Ok(None);
        };
        if matches!(method, "force_stop" | "delete")
            && args["confirm"] == id
            && self.configs.contains_key(id)
        {
            if let Some(path) = self.hibernated(id) {
                self.discard_hibernation(id, &path)?;
                if method == "force_stop" {
                    return Ok(Some(
                        json!({"state":"stopped","forced":true,"hibernation":"discarded"}),
                    ));
                }
            }
        }
        if !matches!(
            method,
            "pause" | "resume" | "wake" | "idle_policy" | "sleep" | "keep_awake"
        ) {
            return self.idle_gate(method, id);
        }
        validate_id(id)?;
        if !self.configs.contains_key(id) {
            bail!("VM not found")
        }
        let allowed: &[&str] = match method {
            "idle_policy" => &["id", "idle_pause_s", "idle_hibernate_s"],
            "keep_awake" => &["id", "seconds"],
            _ => &["id"],
        };
        if args
            .as_object()
            .is_some_and(|a| a.keys().any(|k| !allowed.contains(&k.as_str())))
        {
            bail!("{method} accepts only {}", allowed.join(" and "))
        }
        let now = Instant::now();
        if method == "idle_policy" {
            if args.get("idle_pause_s").is_none() && args.get("idle_hibernate_s").is_none() {
                bail!("idle_policy requires idle_pause_s or idle_hibernate_s (0 turns it off)")
            }
            let after = policy(args)?;
            let hibernate = hibernate_policy(args, &self.configs[id])?;
            let idle = self.idle.entry(id.into()).or_insert_with(|| Idle::new(now));
            if args.get("idle_pause_s").is_some() {
                idle.after = after;
            }
            if args.get("idle_hibernate_s").is_some() {
                idle.hibernate = hibernate;
            }
            idle.last = now;
            return Ok(Some(json!({
                "idle_pause_s": idle.after.map_or(0, |a| a.as_secs()),
                "idle_hibernate_s": idle.hibernate.map_or(0, |a| a.as_secs()),
            })));
        }
        if method == "keep_awake" {
            let seconds = args["seconds"]
                .as_u64()
                .filter(|s| *s <= MAX_HOLD_SECONDS)
                .with_context(|| format!("keep_awake requires seconds, 0..{MAX_HOLD_SECONDS}"))?;
            let idle = self.idle.entry(id.into()).or_insert_with(|| Idle::new(now));
            idle.hold = (seconds > 0).then(|| now + Duration::from_secs(seconds));
        }
        // `keep_awake` also wakes the VM, like the heartbeat.
        let method = if method == "keep_awake" {
            "wake"
        } else {
            method
        };
        self.settle_hibernating(id);
        let Some(state) = self.live_state(id) else {
            if self.hibernated(id).is_some() {
                match method {
                    "pause" => bail!("VM is hibernated; it is not running"),
                    "sleep" => return Ok(Some(json!({"state":"hibernated"}))),
                    _ => {}
                }
                self.restore_hibernated(id)?;
                return Ok(Some(json!({"state":"restoring"})));
            }
            if method == "wake" {
                return Ok(Some(json!({"state":"stopped"})));
            }
            bail!("VM is not running")
        };
        if state == "hibernated" || self.idle.get(id).is_some_and(|i| i.hibernating.is_some()) {
            if method == "sleep" {
                return Ok(Some(json!({"state":"hibernating"})));
            }
            bail!(HIBERNATING)
        }
        let idle = self.idle.entry(id.into()).or_insert_with(|| Idle::new(now));
        idle.last = now;
        let held = idle.held(&state);
        if method == "sleep" {
            return self.sleep(id, &state, held, deferred);
        }
        match method {
            "pause" => {
                if held.is_some() {
                    idle.paused = Some((PausedBy::Request, now));
                    return Ok(Some(json!({"state":"paused"})));
                }
                idle.paused = Some((PausedBy::Request, now));
                *deferred = Some(self.forward(id, "pause")?);
            }
            "resume" => *deferred = Some(self.resume(id, "request")?),
            // `wake` is a heartbeat: it resumes only what the idle monitor paused.
            _ => match held {
                Some(PausedBy::Idle) => *deferred = Some(self.resume(id, "idle")?),
                Some(PausedBy::Request) => {
                    return Ok(Some(json!({"state":"paused","paused_by":"request"})))
                }
                None => return Ok(Some(json!({ "state": state }))),
            },
        }
        Ok(Some(Value::Null))
    }

    /// `sleep`: what the idle monitor would do now, on request. A VM that can
    /// be hibernated is; any other is paused the way the monitor pauses it,
    /// so the next use resumes it.
    fn sleep(
        &mut self,
        id: &str,
        state: &str,
        held: Option<PausedBy>,
        deferred: &mut Option<Completion>,
    ) -> Result<Option<Value>> {
        if held == Some(PausedBy::Request) {
            bail!("VM is paused on request; resume it first")
        }
        if state != "running" && state != "paused" {
            bail!("VM is not running")
        }
        if self.snapshot_ops.active_vm(id)
            || self.running[id].execution_quiescing
            || !self.jobs.lock().unwrap().snapshot_idle(id)
        {
            bail!("VM is busy: Guest jobs or a snapshot operation are still running")
        }
        let now = Instant::now();
        let idle = self.idle.get_mut(id).unwrap();
        idle.hold = None;
        idle.probe = None;
        if container_state::hibernation_path(&self.configs[id]).is_ok() {
            self.hibernate(id, "request")?;
            return Ok(Some(json!({"state":"hibernating"})));
        }
        if held == Some(PausedBy::Idle) {
            return Ok(Some(json!({"state":"paused"})));
        }
        idle.paused = Some((PausedBy::Idle, now));
        *deferred = Some(self.forward(id, "pause")?);
        self.events.publish(
            id,
            "idle_paused",
            json!({"state":"paused","reason":"request"}),
        );
        Ok(Some(Value::Null))
    }

    /// Before a due pause or hibernation of a running Apple Linux Guest, asks
    /// its agent how busy it is; the answer is read on the next pass. A Guest
    /// that does not answer (no agent, an older one) counts as quiet.
    fn guest_work(&mut self, id: &str, state: &str, now: Instant) -> GuestWork {
        let config = &self.configs[id];
        if state != "running"
            || config.backend != Backend::AppleVirtualization
            || config.guest != "linux"
        {
            return GuestWork::Quiet;
        }
        match self.idle.get_mut(id).and_then(|idle| idle.probe.take()) {
            Some((at, reply)) => match reply.try_recv() {
                Ok(reply) if guest_busy(&reply) => GuestWork::Busy,
                Ok(_) | Err(mpsc::TryRecvError::Disconnected) => GuestWork::Quiet,
                Err(mpsc::TryRecvError::Empty) if now.duration_since(at) < 3 * PASS => {
                    self.idle.get_mut(id).unwrap().probe = Some((at, reply));
                    GuestWork::Asked
                }
                Err(mpsc::TryRecvError::Empty) => GuestWork::Quiet,
            },
            None => {
                let number = self.running[id].next;
                let request = json!({"id":number.to_string(),"method":"activity"});
                match self.write_command(id, json!({"method":"guest_request","request":request})) {
                    Ok((_, reply)) => {
                        self.idle.get_mut(id).unwrap().probe = Some((now, reply));
                        GuestWork::Asked
                    }
                    Err(_) => GuestWork::Quiet,
                }
            }
        }
    }

    /// The `idle` field of `status`: the policy and, while the daemon holds
    /// the VM paused, who asked.
    pub(crate) fn idle_view(&self, id: &str, state: &Value) -> Value {
        let Some(idle) = self.idle.get(id) else {
            return json!({"idle_pause_s":0,"idle_hibernate_s":0,"paused_by":null});
        };
        let held = idle.held(state.as_str().unwrap_or_default());
        json!({
            "idle_pause_s": idle.after.map_or(0, |a| a.as_secs()),
            "idle_hibernate_s": idle.hibernate.map_or(0, |a| a.as_secs()),
            "idle_s": Instant::now().duration_since(idle.last).as_secs(),
            "awake_for_s": idle.hold.map(|until| until.saturating_duration_since(Instant::now()).as_secs()).filter(|s| *s > 0),
            "paused_by": held.map(PausedBy::name),
        })
    }
}

/// One monitor pass: hibernates or pauses each VM whose policy is due.
pub(crate) fn monitor_pass(manager: &Mutex<Manager>) {
    let manager = &mut *manager.lock().unwrap();
    let now = Instant::now();
    let ids: Vec<String> = manager
        .idle
        .iter()
        .filter(|(_, idle)| {
            idle.after.is_some()
                || idle.hibernate.is_some()
                || idle.paused.is_some()
                || idle.restoring
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        manager.settle_restore(&id);
        manager.settle_hibernating(&id);
        let Some(state) = manager.live_state(&id) else {
            continue;
        };
        // A QEMU worker's native console channel stays attached with no window
        // open, so it is not a viewer; its input and focus are activity instead.
        let busy = manager.leases.watched(&id)
            || manager.snapshot_ops.active_vm(&id)
            || manager.running[&id].execution_quiescing
            || !manager.jobs.lock().unwrap().snapshot_idle(&id)
            || manager.idle[&id].held_awake(now);
        let idle = manager.idle.get_mut(&id).unwrap();
        idle.settle(&state, now);
        if busy {
            idle.last = now;
            idle.probe = None;
            continue;
        }
        let hibernate = idle.hibernate_due(&state, busy, now);
        if !hibernate && !idle.due(&state, busy, now) {
            idle.probe = None;
            continue;
        }
        let quiet = now.duration_since(idle.last).as_secs();
        // Work the host cannot see, like a build started in the background.
        match manager.guest_work(&id, &state, now) {
            GuestWork::Asked => continue,
            GuestWork::Busy => {
                manager.idle.get_mut(&id).unwrap().last = now;
                continue;
            }
            GuestWork::Quiet => {}
        }
        if hibernate {
            if let Err(error) = manager.hibernate(&id, "idle") {
                log_line(&format!(
                    "vm-control: idle hibernation of {id} after {quiet} s failed: {error:#}"
                ));
            }
            continue;
        }
        manager.idle.get_mut(&id).unwrap().paused = Some((PausedBy::Idle, now));
        match manager.write_command(&id, json!({"method":"pause"})) {
            Ok(_) => manager.events.publish(
                &id,
                "idle_paused",
                json!({"state":"paused","reason":"idle"}),
            ),
            Err(error) => log_line(&format!(
                "vm-control: idle pause of {id} after {quiet} s failed: {error:#}"
            )),
        }
    }
}

pub(crate) fn spawn_monitor(manager: &Arc<Mutex<Manager>>) {
    let monitor = Arc::downgrade(manager);
    thread::spawn(move || loop {
        thread::sleep(PASS);
        let Some(manager) = monitor.upgrade() else {
            return;
        };
        monitor_pass(&manager);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_bounds() {
        assert_eq!(policy(&json!({})).unwrap(), None);
        assert_eq!(policy(&json!({"idle_pause_s":0})).unwrap(), None);
        assert_eq!(
            policy(&json!({"idle_pause_s":300})).unwrap(),
            Some(Duration::from_secs(300))
        );
        for bad in [json!(59), json!(MAX_SECONDS + 1), json!(-1), json!("5m")] {
            assert!(policy(&json!({ "idle_pause_s": bad })).is_err(), "{bad}");
        }
    }

    #[test]
    fn due_only_after_a_quiet_running_period() {
        let start = Instant::now();
        let mut idle = Idle::new(start);
        let later = start + Duration::from_secs(120);
        assert!(!idle.due("running", false, later), "no policy");
        idle.after = Some(Duration::from_secs(60));
        assert!(idle.due("running", false, later));
        assert!(!idle.due("running", true, later), "busy");
        assert!(!idle.due("paused", false, later), "already paused");
        assert!(!idle.due("starting", false, later));
        assert!(!idle.due("running", false, start + Duration::from_secs(30)));
        idle.paused = Some((PausedBy::Idle, later));
        assert!(!idle.due("running", false, later), "pause in flight");
    }

    #[test]
    fn hibernate_due_when_quiet_running_or_idle_paused() {
        let start = Instant::now();
        let later = start + Duration::from_secs(600);
        let mut idle = Idle::new(start);
        assert!(!idle.hibernate_due("running", false, later), "no policy");
        idle.hibernate = Some(Duration::from_secs(300));
        assert!(idle.hibernate_due("running", false, later));
        assert!(!idle.hibernate_due("running", true, later), "busy");
        assert!(!idle.hibernate_due("running", false, start + Duration::from_secs(60)));
        assert!(
            !idle.hibernate_due("paused", false, later),
            "paused by someone else"
        );
        idle.paused = Some((PausedBy::Idle, start));
        assert!(idle.hibernate_due("paused", false, later));
        idle.paused = Some((PausedBy::Request, start));
        assert!(
            !idle.hibernate_due("paused", false, later),
            "paused on request"
        );
        idle.paused = None;
        let (_tx, rx) = mpsc::channel();
        idle.hibernating = Some(rx);
        assert!(
            !idle.hibernate_due("running", false, later),
            "already saving"
        );
    }

    #[test]
    fn hibernate_policy_needs_a_restorable_vm() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let disk = dir.join("rw.raw");
        fs::write(&disk, b"").unwrap();
        let mut value = json!({
            "backend":"apple-virtualization","id":"c-a","guest":"linux","cpus":1,"memory_mib":512,
            "kernel":"/k","disk":disk,"mac_address":"02:00:00:00:00:01",
            "machine_identifier_file":dir.join("machine-id.bin"),
        });
        let config: Config = serde_json::from_value(value.clone()).unwrap();
        let args = json!({"idle_hibernate_s":600});
        assert_eq!(
            hibernate_policy(&args, &config).unwrap(),
            Some(Duration::from_secs(600))
        );
        assert_eq!(
            hibernate_policy(&json!({"idle_hibernate_s":0}), &config).unwrap(),
            None
        );
        assert!(hibernate_policy(&json!({"idle_hibernate_s":30}), &config).is_err());
        value["mac_address"] = Value::Null;
        let no_mac: Config = serde_json::from_value(value).unwrap();
        assert!(hibernate_policy(&args, &no_mac).is_err());
        assert_eq!(
            hibernate_policy(&json!({}), &no_mac).unwrap(),
            None,
            "no policy asks nothing of the VM"
        );
    }

    #[test]
    fn guest_busy_reads_the_agents_load() {
        let reply =
            |load: Value| json!({"id":3,"result":{"id":"3","result":{"load1":load,"running":0}}});
        assert!(guest_busy(&reply(json!(1.2))));
        assert!(guest_busy(&reply(json!(0.5))));
        assert!(!guest_busy(&reply(json!(0.1))));
        let unknown = json!({"id":3,"result":{"id":"3","error":{"code":"unknown_method"}}});
        assert!(!guest_busy(&unknown), "an older agent counts as quiet");
        assert!(!guest_busy(
            &json!({"id":3,"error":{"code":"invalid_state"}})
        ));
    }

    #[test]
    fn held_needs_the_workers_word_and_settles() {
        let start = Instant::now();
        let mut idle = Idle::new(start);
        assert_eq!(idle.held("paused"), None, "paused by someone else");
        idle.paused = Some((PausedBy::Request, start));
        assert_eq!(idle.held("running"), None, "not paused yet");
        assert_eq!(idle.held("paused"), Some(PausedBy::Request));
        idle.settle("running", start + Duration::from_secs(5));
        assert!(
            idle.paused.is_some(),
            "the worker may not have reported yet"
        );
        idle.settle("paused", start + Duration::from_secs(60));
        assert!(idle.paused.is_some());
        idle.settle("running", start + Duration::from_secs(60));
        assert_eq!(idle.paused, None);
    }

    #[test]
    fn passive_methods_are_no_activity() {
        for method in [
            "status", "log_read", "events", "pause", "resume", "wake", "start",
        ] {
            assert!(passive(method), "{method}");
        }
        for method in [
            "exec_start",
            "screenshot",
            "shutdown",
            "show_window",
            "guest_info",
        ] {
            assert!(!passive(method), "{method}");
        }
    }
}
