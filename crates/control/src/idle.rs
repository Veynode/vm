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
use super::*;
use std::time::Instant;

/// How often the monitor looks for VMs to pause.
pub(crate) const PASS: Duration = Duration::from_secs(10);
pub(crate) const MIN_SECONDS: u64 = 60;
pub(crate) const MAX_SECONDS: u64 = 7 * 24 * 3600;
/// A pause or resume the worker has not reported within this long did not happen.
const SETTLE: Duration = Duration::from_secs(15);

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
#[derive(Clone, Debug)]
pub(crate) struct Idle {
    pub(crate) after: Option<Duration>,
    pub(crate) last: Instant,
    /// Who asked for the pause, and when.
    pub(crate) paused: Option<(PausedBy, Instant)>,
}
impl Idle {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            after: None,
            last: now,
            paused: None,
        }
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
    )
}

/// `idle_pause_s`: absent, null or 0 is no idle pause.
pub(crate) fn policy(args: &Value) -> Result<Option<Duration>> {
    match &args["idle_pause_s"] {
        Value::Null => Ok(None),
        value => match value.as_u64() {
            Some(0) => Ok(None),
            Some(seconds) if (MIN_SECONDS..=MAX_SECONDS).contains(&seconds) => {
                Ok(Some(Duration::from_secs(seconds)))
            }
            _ => bail!("idle_pause_s must be 0 or {MIN_SECONDS}..{MAX_SECONDS} seconds"),
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

    /// Before any other VM method: it is activity, and a VM the idle monitor
    /// paused is resumed for it. A VM paused on request refuses it.
    fn idle_gate(&mut self, method: &str, id: &str) -> Result<()> {
        if passive(method) || !self.configs.contains_key(id) {
            return Ok(());
        }
        let Some(state) = self.live_state(id) else {
            return Ok(());
        };
        let now = Instant::now();
        let idle = self.idle.entry(id.into()).or_insert_with(|| Idle::new(now));
        idle.last = now;
        match idle.held(&state) {
            None => Ok(()),
            Some(PausedBy::Request) if method == "show_window" => Ok(()),
            Some(PausedBy::Request) if method != "shutdown" => {
                bail!("VM is paused; resume it first")
            }
            Some(by) => {
                // The request goes out right after; its reply, not resume's, is awaited.
                drop(self.resume(id, by.name())?);
                Ok(())
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
        if !matches!(method, "pause" | "resume" | "wake" | "idle_policy") {
            self.idle_gate(method, id)?;
            return Ok(None);
        }
        validate_id(id)?;
        if !self.configs.contains_key(id) {
            bail!("VM not found")
        }
        let allowed: &[&str] = if method == "idle_policy" {
            &["id", "idle_pause_s"]
        } else {
            &["id"]
        };
        if args
            .as_object()
            .is_some_and(|a| a.keys().any(|k| !allowed.contains(&k.as_str())))
        {
            bail!("{method} accepts only {}", allowed.join(" and "))
        }
        let now = Instant::now();
        if method == "idle_policy" {
            if args.get("idle_pause_s").is_none() {
                bail!("idle_policy requires idle_pause_s (0 turns the idle pause off)")
            }
            let after = policy(args)?;
            let idle = self.idle.entry(id.into()).or_insert_with(|| Idle::new(now));
            idle.after = after;
            idle.last = now;
            return Ok(Some(
                json!({"idle_pause_s":after.map_or(0, |a| a.as_secs())}),
            ));
        }
        let Some(state) = self.live_state(id) else {
            if method == "wake" {
                return Ok(Some(json!({"state":"stopped"})));
            }
            bail!("VM is not running")
        };
        let idle = self.idle.entry(id.into()).or_insert_with(|| Idle::new(now));
        idle.last = now;
        let held = idle.held(&state);
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

    /// The `idle` field of `status`: the policy and, while the daemon holds
    /// the VM paused, who asked.
    pub(crate) fn idle_view(&self, id: &str, state: &Value) -> Value {
        let Some(idle) = self.idle.get(id) else {
            return json!({"idle_pause_s":0,"paused_by":null});
        };
        let held = idle.held(state.as_str().unwrap_or_default());
        json!({
            "idle_pause_s": idle.after.map_or(0, |a| a.as_secs()),
            "idle_s": Instant::now().duration_since(idle.last).as_secs(),
            "paused_by": held.map(PausedBy::name),
        })
    }
}

/// One monitor pass: pauses each VM whose policy is due.
pub(crate) fn monitor_pass(manager: &Mutex<Manager>) {
    let manager = &mut *manager.lock().unwrap();
    let now = Instant::now();
    let ids: Vec<String> = manager
        .idle
        .iter()
        .filter(|(_, idle)| idle.after.is_some() || idle.paused.is_some())
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        let Some(state) = manager.live_state(&id) else {
            continue;
        };
        // A QEMU worker's native console channel stays attached with no window
        // open, so it is not a viewer; its input and focus are activity instead.
        let busy = manager.leases.watched(&id)
            || manager.snapshot_ops.active_vm(&id)
            || manager.running[&id].execution_quiescing
            || !manager.jobs.lock().unwrap().snapshot_idle(&id);
        let idle = manager.idle.get_mut(&id).unwrap();
        idle.settle(&state, now);
        if busy {
            idle.last = now;
            continue;
        }
        if !idle.due(&state, busy, now) {
            continue;
        }
        idle.paused = Some((PausedBy::Idle, now));
        let quiet = now.duration_since(idle.last).as_secs();
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
        for method in ["status", "log_read", "events", "pause", "resume", "wake"] {
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
