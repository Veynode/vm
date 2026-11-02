//! One independently supervised QEMU/HVF instance. Protocol output is JSON only.
#[path = "../qemu_deadline_io.rs"]
mod deadline_io;
#[path = "../qemu_guardian.rs"]
mod guardian;
#[path = "../qemu_guest_exec.rs"]
mod guest_exec;
#[path = "../qemu_guest_transport.rs"]
mod guest_transport;
#[path = "../qemu_native_input.rs"]
mod native_input_worker;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::OpenOptionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use vm_control::{
    backend::Backend,
    read_frame,
    screenshot_scaled::{Refusal, Refused, Scaled, REFUSAL_FIELD},
    Config,
};
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop_signal(_: i32) {
    STOP.store(true, Ordering::Release);
}
fn emit(value: Value) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &value)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
fn publish_startup_then_resume(
    qmp: &mut Qmp,
    state: &mut Value,
    paused_launch: bool,
    mut publish: impl FnMut(Value) -> Result<()>,
) -> Result<()> {
    // Verified metadata is published before cont. Do not query after cont to
    // establish it: a fast Guest can terminate and close QMP immediately.
    if paused_launch {
        state["state"] = json!("starting");
    }
    publish(state.clone())?;
    if paused_launch {
        qmp.call("cont")?;
        state["state"] = json!("running");
        state["qmp_status"] = json!("running");
        publish(state.clone())?;
    }
    Ok(())
}
struct RuntimeDirectory(PathBuf);
impl RuntimeDirectory {
    fn new() -> Result<Self> {
        let mut random = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = PathBuf::from(format!("/tmp/veynode-qemu-{suffix}"));
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for RuntimeDirectory {
    fn drop(&mut self) {
        // Never recursively remove an externally provided location.
        for name in [
            "qmp.sock",
            "monitor.sock",
            "spice.sock",
            "console.sock",
            "agent.sock",
            "driver.sock",
        ] {
            let _ = fs::remove_file(self.0.join(name));
        }
        let _ = fs::remove_dir(&self.0);
    }
}
struct QemuChild(Child, Option<guardian::Parent>);
impl Drop for QemuChild {
    fn drop(&mut self) {
        if let Some(guardian) = &mut self.1 {
            guardian.close();
            let until = Instant::now() + Duration::from_secs(6);
            while self.0.try_wait().ok().flatten().is_none() && Instant::now() < until {
                thread::sleep(Duration::from_millis(20));
            }
            // Never kill the guardian alone: it owns the unreaped QEMU child.
            // A stalled guardian is unconfirmed; daemon worker-group cleanup remains fail-closed.
            return;
        }
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
struct Qmp {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
    poisoned: bool,
    reply_prefix: Vec<u8>,
    /// A SHUTDOWN event with reason guest-reset was seen (`-action reboot=shutdown`).
    guest_reset: bool,
    /// A run-state event was read (while waiting for a reply or by `drain_events`) and not yet acted on.
    run_state_event: bool,
    /// The same for `SHUTDOWN`.
    shutdown_event: bool,
}
/// QMP events after which `query-status` may answer differently. Run state is only ever taken from `query-status`;
/// an event is a reason to ask now and never a state by itself. Any other event is ignored: the slow safety poll
/// covers a transition that announces itself with none of these. `POWERDOWN` is absent because it only echoes the
/// worker's own `system_powerdown`, which changes no run state and already schedules a query.
const RUN_STATE_EVENTS: [&str; 10] = [
    "STOP",
    "RESUME",
    "RESET",
    "SUSPEND",
    "SUSPEND_DISK",
    "WAKEUP",
    "GUEST_PANICKED",
    "GUEST_CRASHLOADED",
    "WATCHDOG",
    "BLOCK_IO_ERROR",
];
/// Records drained per worker loop pass, so an event storm cannot hold the loop.
const EVENT_DRAIN_BOUND: usize = 64;
/// While the run state may still be moving, `query-status` keeps its original 1 s cadence.
const STATUS_ACTIVE_INTERVAL: Duration = Duration::from_secs(1);
/// How long that cadence lasts after a run-state event, a run-state command, startup, an observed change or a failed
/// query.
const STATUS_ACTIVE_WINDOW: Duration = Duration::from_secs(10);
/// Steady state: the poll that catches a transition for which no listed event was delivered.
const STATUS_SAFETY_INTERVAL: Duration = Duration::from_secs(30);
/// Slots of `EVENTS_SEEN`: one per `RUN_STATE_EVENTS` name, then `SHUTDOWN`, then every other event name together.
const EVENT_SLOTS: usize = RUN_STATE_EVENTS.len() + 2;
thread_local! {
    /// Events read from QMP on this thread, for the run-state summary only. Not a `Qmp` field because the
    /// scripted-peer tests build `Qmp` by literal; the worker reads QMP on one thread.
    static EVENTS_SEEN: std::cell::Cell<[u64; EVENT_SLOTS]> = const { std::cell::Cell::new([0; EVENT_SLOTS]) };
}
fn event_seen(slot: usize) {
    EVENTS_SEEN.with(|seen| {
        let mut counts = seen.get();
        counts[slot] += 1;
        seen.set(counts);
    });
}
/// Why the next `query-status` is asked. Only used to attribute a change that query finds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asked {
    /// A listed event or `SHUTDOWN` was read (or the drain could not judge the stream).
    Event,
    /// Startup `cont` or `system_powerdown`.
    Command,
    /// The 1 s timer inside the active window.
    Cadence,
    /// The 30 s timer.
    Safety,
}
/// Counters for the run-state summary. A change is a successful query that answered a different state than the last
/// successful one; a change to or from `unavailable` is counted as a query error, not as a change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Observed {
    queries: u64,
    query_errors: u64,
    event_changes: u64,
    command_changes: u64,
    /// Found by the 1 s timer with no event read: unannounced, like `safety_changes`, but inside the active window.
    cadence_changes: u64,
    safety_polls: u64,
    /// Found by the 30 s timer with no event read: a transition for which no listed event was delivered.
    safety_changes: u64,
    safety_errors: u64,
}
/// When the worker next asks QEMU for its run state.
struct StatusPoll {
    next: Instant,
    active_until: Instant,
    asked: Asked,
    seen: Observed,
}
impl StatusPoll {
    /// After connect and `cont`: ask on the first loop pass, then at the active cadence.
    fn new(now: Instant) -> Self {
        Self {
            next: now,
            active_until: now + STATUS_ACTIVE_WINDOW,
            asked: Asked::Command,
            seen: Observed::default(),
        }
    }
    /// A run-state event arrived or a run-state command was sent: ask at once and stay at the active cadence.
    fn disturbed(&mut self, now: Instant) {
        self.next = now;
        self.active_until = now + STATUS_ACTIVE_WINDOW;
        self.asked = Asked::Command;
    }
    /// `SHUTDOWN` arrived. QEMU runs without `-no-shutdown`, so it is now exiting and may never answer: do not ask at
    /// once (a query here would usually publish `unavailable` just before the exit is observed and could hold the
    /// loop for the 3 s reply deadline). Ask one active interval later if QEMU is somehow still there, which is the
    /// latest the previous fixed 1 s poll could have asked.
    fn exiting(&mut self, now: Instant) {
        self.next = now + STATUS_ACTIVE_INTERVAL;
        self.active_until = now + STATUS_ACTIVE_WINDOW;
    }
    fn due(&self, now: Instant) -> bool {
        now >= self.next
    }
    /// `settled`: the query succeeded and answered what was already published.
    fn queried(&mut self, now: Instant, settled: bool) {
        if !settled {
            self.active_until = now + STATUS_ACTIVE_WINDOW;
        }
        self.next = now
            + if now < self.active_until {
                STATUS_ACTIVE_INTERVAL
            } else {
                STATUS_SAFETY_INTERVAL
            };
        self.asked = if now < self.active_until {
            Asked::Cadence
        } else {
            Asked::Safety
        };
    }
    /// Counts one query. `raced`: a run-state event was read while waiting for this very reply, so the change was
    /// announced. Answers which timer found an unannounced change, if one did.
    fn count(&mut self, answered: bool, changed: bool, raced: bool) -> Option<&'static str> {
        let safety = self.asked == Asked::Safety;
        self.seen.queries += 1;
        self.seen.safety_polls += safety as u64;
        if !answered {
            self.seen.query_errors += 1;
            self.seen.safety_errors += safety as u64;
            return None;
        }
        if !changed {
            return None;
        }
        match if raced { Asked::Event } else { self.asked } {
            Asked::Event => self.seen.event_changes += 1,
            Asked::Command => self.seen.command_changes += 1,
            Asked::Cadence => {
                self.seen.cadence_changes += 1;
                return Some("cadence_poll");
            }
            Asked::Safety => {
                self.seen.safety_changes += 1;
                return Some("safety_poll");
            }
        }
        None
    }
    /// Counters only: event names are QEMU's own, and nothing of an event's data or of the Guest is kept.
    fn summary(&self) -> Value {
        let counts = EVENTS_SEEN.with(|seen| seen.get());
        let events: serde_json::Map<String, Value> = RUN_STATE_EVENTS
            .iter()
            .chain(&["SHUTDOWN"])
            .zip(counts)
            .filter(|(_, count)| *count > 0)
            .map(|(name, count)| (name.to_string(), json!(count)))
            .collect();
        let seen = &self.seen;
        json!({"queries":seen.queries,"query_errors":seen.query_errors,"changes":{"event":seen.event_changes,"command":seen.command_changes,"cadence_poll":seen.cadence_changes,"safety_poll":seen.safety_changes},"safety_polls":seen.safety_polls,"safety_poll_errors":seen.safety_errors,"events":events,"events_unlisted":counts[EVENT_SLOTS - 1]})
    }
    fn summary_line(&self) -> String {
        let mut line = self.summary();
        line["event"] = json!("qmp_run_state_summary");
        line.to_string()
    }
}
/// The summary reaches stderr however the worker loop ends.
impl Drop for StatusPoll {
    fn drop(&mut self) {
        log_line(&self.summary_line());
    }
}
/// One line on the worker's stderr; a failed write is ignored. Silent in tests, whose output it would bypass capture.
fn log_line(line: &str) {
    if !cfg!(test) {
        let _ = writeln!(std::io::stderr(), "{line}");
    }
}
/// The stderr line for a change found by a timer rather than announced by an event. `from` and `to` are QEMU run
/// state names.
fn poll_change_line(source: &str, from: &Value, to: &Value) -> String {
    json!({"event":"qmp_run_state_poll_change","source":source,"from":from,"to":to}).to_string()
}
/// What the records read since the last pass ask of the status poll.
#[derive(Debug, PartialEq, Eq)]
enum Drained {
    Nothing,
    RunState,
    Exiting,
}
/// One pass of run-state observation: drain delivered QMP events, then `query-status` if an event, a command or a
/// timer asks for it, and publish the state only when it differs. `may_query` is false while a screenshot is in
/// flight, as before; a due query then waits for the capture to finish.
fn observe_status(
    qmp: &mut Qmp,
    poll: &mut StatusPoll,
    state: &mut Value,
    now: Instant,
    may_query: bool,
    mut publish: impl FnMut(Value) -> Result<()>,
) -> Result<()> {
    match qmp.drain_events() {
        Drained::Nothing => {}
        Drained::RunState => {
            poll.disturbed(now);
            poll.asked = Asked::Event;
        }
        Drained::Exiting => {
            poll.exiting(now);
            poll.asked = Asked::Event;
        }
    }
    if !may_query || !poll.due(now) {
        return Ok(());
    }
    let mut updated = state.clone();
    let answered = match qmp.call("query-status") {
        Ok(status) => {
            updated["qmp_status"] = status["status"].clone();
            updated["state"] = json!(if status["running"] == true {
                "running"
            } else {
                "paused"
            });
            updated.as_object_mut().unwrap().remove("qmp_error");
            true
        }
        Err(error) => {
            updated["state"] = json!("unavailable");
            updated["qmp_error"] = json!(error.to_string());
            false
        }
    };
    let changed = updated != *state;
    let raced = qmp.run_state_event || qmp.shutdown_event;
    if let Some(source) = poll.count(answered, changed && state["state"] != "unavailable", raced) {
        log_line(&poll_change_line(
            source,
            &state["qmp_status"],
            &updated["qmp_status"],
        ));
    }
    if changed {
        publish(updated.clone())?;
        *state = updated;
    }
    // An event read while waiting for this very reply preceded the answer or asks for one more query; keep it.
    poll.queried(now, answered && !changed);
    Ok(())
}
impl Qmp {
    fn connect(path: &Path, child: &mut Child) -> Result<Self> {
        let deadline = Instant::now() + Duration::from_secs(12);
        let stream = loop {
            if STOP.load(Ordering::Acquire) {
                bail!("worker interrupted during QEMU startup")
            }
            if let Some(status) = child.try_wait()? {
                bail!("QEMU exited during startup: {status}")
            }
            if let Ok(stream) = UnixStream::connect(path) {
                break stream;
            }
            if Instant::now() >= deadline {
                bail!("QMP startup timed out after 12 seconds")
            }
            // QEMU binds the socket within a few hundred ms; a refused connect is cheap.
            thread::sleep(Duration::from_millis(5));
        };
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let mut qmp = Self {
            reader: BufReader::new(stream.try_clone()?),
            stream,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
            guest_reset: false,
            run_state_event: false,
            shutdown_event: false,
        };
        let greeting: Value =
            serde_json::from_slice(&read_frame(&mut qmp.reader)?.context("missing QMP greeting")?)?;
        if !greeting["QMP"].is_object() {
            bail!("invalid QMP greeting")
        }
        qmp.call("qmp_capabilities")?;
        Ok(qmp)
    }
    fn call(&mut self, method: &str) -> Result<Value> {
        self.arguments(method, json!({}))
    }
    fn arguments(&mut self, method: &str, arguments: Value) -> Result<Value> {
        // Cleanup is also a framed write: a partial release must never permit
        // another JSON record to be appended. Its independent budget remains
        // three seconds and is never inherited from an expired input receipt.
        self.arguments_before(method, arguments, Instant::now() + Duration::from_secs(3))
    }
    fn ensure_usable(&self) -> Result<()> {
        if self.poisoned {
            bail!("QMP framing uncertain; input cleanup unconfirmed")
        }
        Ok(())
    }
    fn arguments_before(
        &mut self,
        method: &str,
        arguments: Value,
        deadline: Instant,
    ) -> Result<Value> {
        self.ensure_usable()?;
        let number = self.next;
        let mut bytes =
            serde_json::to_vec(&json!({"execute":method,"id":number,"arguments":arguments}))?;
        if bytes.len() >= vm_control::MAX_MESSAGE {
            bail!("QMP input record bound")
        }
        bytes.push(b'\n');
        self.next += 1;
        self.write_record_before(&bytes, deadline)?;
        self.reply(number, method)
    }
    fn write_record_before(&mut self, bytes: &[u8], deadline: Instant) -> Result<()> {
        self.ensure_usable()?;
        let write = deadline_io::write(&mut self.stream, bytes, deadline);
        if write.written > 0 && write.written < bytes.len() {
            self.poisoned = true;
        }
        // BufReader shares this descriptor's nonblocking flag. Restore even on failure.
        if let Err(error) = self.stream.set_nonblocking(false) {
            self.poisoned = true;
            return Err(error.into());
        }
        write.result
    }
    fn reply(&mut self, number: u64, method: &str) -> Result<Value> {
        // ACK waiting does not admit new input or renew acquisition eligibility.
        // Event storms cannot extend one request forever.
        let deadline = Instant::now() + Duration::from_secs(3);
        for _ in 0..128 {
            let value: Value = serde_json::from_slice(&self.line_before(deadline)?)?;
            // Before the deadline check: this record is consumed either way, and an event in it must not be lost.
            self.note_event(&value);
            if Instant::now() >= deadline {
                bail!("QMP response deadline expired after decoding")
            }
            if value["id"] == number {
                if let Some(error) = value.get("error") {
                    bail!("QMP {method} failed: {error}")
                }
                return value
                    .get("return")
                    .cloned()
                    .context("QMP response has no return value");
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        bail!("QMP response deadline or event bound exceeded")
    }
    /// A record that is not the awaited reply. Only a record without `id` is an event; a reply (which always
    /// carries the `id` this client sent) is never read as one, and an event is never returned as a reply.
    fn note_event(&mut self, value: &Value) {
        if value.get("id").is_some() {
            return;
        }
        match value["event"].as_str() {
            Some("SHUTDOWN") => {
                self.shutdown_event = true;
                // `-action reboot=shutdown` (Windows): a guest reboot arrives as this SHUTDOWN.
                if value["data"]["reason"] == "guest-reset" {
                    self.guest_reset = true;
                }
                event_seen(EVENT_SLOTS - 2);
            }
            Some(event) => match RUN_STATE_EVENTS.iter().position(|listed| *listed == event) {
                Some(slot) => {
                    self.run_state_event = true;
                    event_seen(slot);
                }
                None => event_seen(EVENT_SLOTS - 1),
            },
            None => {}
        }
    }
    /// After QEMU exited: read the events it wrote before exiting (the socket
    /// keeps them) and report whether the exit was a guest reboot.
    fn exited_for_guest_reset(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_millis(300);
        for _ in 0..64 {
            if self.guest_reset {
                break;
            }
            match self.line_before(deadline) {
                Ok(line) => {
                    if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                        self.note_event(&value);
                    }
                }
                Err(_) => break,
            }
        }
        self.guest_reset
    }
    /// Reads every complete record QEMU has already delivered, without waiting and without sending anything, and
    /// answers what they ask of the status poll. An incomplete record stays in `reply_prefix` for the next
    /// read. End of stream, an oversized record or a read error is not consumed or judged here: it asks for a query,
    /// whose ordinary bounded reply path then reports it exactly as before.
    fn drain_events(&mut self) -> Drained {
        if self.poisoned {
            return Drained::Nothing;
        }
        let mut query = false;
        if self.stream.set_nonblocking(true).is_ok() {
            for _ in 0..EVENT_DRAIN_BOUND {
                match self.delivered_line() {
                    Ok(Some(line)) => match serde_json::from_slice::<Value>(&line) {
                        Ok(value) => self.note_event(&value),
                        Err(_) => query = true,
                    },
                    Ok(None) => break,
                    Err(()) => {
                        query = true;
                        break;
                    }
                }
            }
        } else {
            query = true;
        }
        if self.stream.set_nonblocking(false).is_err() {
            self.poisoned = true;
        }
        let run_state = std::mem::take(&mut self.run_state_event);
        if std::mem::take(&mut self.shutdown_event) {
            Drained::Exiting
        } else if run_state || query {
            Drained::RunState
        } else {
            Drained::Nothing
        }
    }
    /// `Ok(None)`: nothing more is available now. `Err`: the reply path must look at this stream.
    fn delivered_line(&mut self) -> std::result::Result<Option<Vec<u8>>, ()> {
        loop {
            let available = match self.reader.fill_buf() {
                Ok([]) => return Err(()),
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Ok(None)
                }
                Err(_) => return Err(()),
            };
            let end = available.iter().position(|b| *b == b'\n');
            let count = end.map_or(available.len(), |n| n + 1);
            if count > vm_control::MAX_MESSAGE.saturating_sub(self.reply_prefix.len()) {
                return Err(());
            }
            self.reply_prefix.extend_from_slice(&available[..count]);
            self.reader.consume(count);
            if end.is_some() {
                return Ok(Some(std::mem::take(&mut self.reply_prefix)));
            }
        }
    }
    fn line_before(&mut self, deadline: Instant) -> Result<Vec<u8>> {
        self.ensure_usable()?;
        if let Err(error) = self.stream.set_nonblocking(true) {
            self.poisoned = true;
            return Err(error.into());
        }
        let result = self.line_nonblocking_before(deadline);
        if let Err(error) = self.stream.set_nonblocking(false) {
            self.poisoned = true;
            return Err(error.into());
        }
        result
    }
    fn line_nonblocking_before(&mut self, deadline: Instant) -> Result<Vec<u8>> {
        use std::os::fd::AsRawFd;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .context("QMP response deadline expired")?;
            if self.reader.buffer().is_empty() {
                let mut pollfd = libc::pollfd {
                    fd: self.stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe {
                    libc::poll(
                        &mut pollfd,
                        1,
                        remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
                    )
                };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    self.poisoned = true;
                    return Err(error.into());
                }
                if ready == 0 {
                    continue;
                }
                if Instant::now() >= deadline {
                    bail!("QMP response deadline expired")
                }
            }
            let available = match self.reader.fill_buf() {
                Ok(bytes) if !bytes.is_empty() => bytes,
                Ok(_) => {
                    self.poisoned = true;
                    bail!("QMP disconnected; input cleanup unconfirmed")
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    // Readiness can race. Keep the exact prefix and poll again
                    // against the same deadline, never a fresh per-byte timeout.
                    continue;
                }
                Err(error) => {
                    self.poisoned = true;
                    return Err(error.into());
                }
            };
            // If completion is observed late, leave even newly buffered bytes
            // untouched for the next bounded reply operation.
            if Instant::now() >= deadline {
                bail!("QMP response deadline expired")
            }
            let end = available.iter().position(|b| *b == b'\n');
            let count = end.map_or(available.len(), |n| n + 1);
            if count > vm_control::MAX_MESSAGE.saturating_sub(self.reply_prefix.len()) {
                self.poisoned = true;
                bail!("QMP response bound exceeded; input cleanup unconfirmed")
            }
            self.reply_prefix.extend_from_slice(&available[..count]);
            self.reader.consume(count);
            if end.is_some() {
                return Ok(std::mem::take(&mut self.reply_prefix));
            }
        }
    }
}
#[derive(Default)]
struct InputState {
    completed_operations: std::collections::VecDeque<String>,
    operation: Option<String>,
    until: Option<Instant>,
    held: std::collections::BTreeMap<String, Value>,
}
impl InputState {
    fn cleanup(&mut self, qmp: &mut Qmp) -> Result<()> {
        qmp.ensure_usable()?;
        if !self.held.is_empty() {
            qmp.arguments(
                "input-send-event",
                json!({"events":self.held.values().collect::<Vec<_>>()}),
            )?;
        }
        self.held.clear();
        if let Some(operation) = self.operation.take() {
            self.completed_operations.push_back(operation);
            while self.completed_operations.len() > 64 {
                self.completed_operations.pop_front();
            }
        }
        self.until = None;
        Ok(())
    }
    fn cleanup_operation(&mut self, args: &Value, qmp: &mut Qmp) -> Result<Value> {
        let operation = args["operation_id"]
            .as_str()
            .context("missing input operation")?;
        if self.operation.as_deref() == Some(operation) {
            self.cleanup(qmp)?;
            return Ok(json!({"released":true,"synthetic_release_dispatched":true}));
        }
        if self
            .completed_operations
            .iter()
            .any(|known| known == operation)
        {
            return Ok(
                json!({"released":true,"synthetic_release_dispatched":true,"already_released":true}),
            );
        }
        bail!("unknown input operation; cleanup is unconfirmed")
    }
    fn send_events(&mut self, events: &[Value], qmp: &mut Qmp) -> Result<()> {
        self.send_events_before(events, qmp, None)
    }
    fn send_events_before(
        &mut self,
        events: &[Value],
        qmp: &mut Qmp,
        deadline: Option<Instant>,
    ) -> Result<()> {
        let mut releases = Vec::new();
        for event in events {
            if ["key", "btn"].contains(&event["type"].as_str().unwrap_or("")) {
                let identity = if event["type"] == "key" {
                    format!("key:{}", event["data"]["key"])
                } else {
                    format!("btn:{}", event["data"]["button"])
                };
                if event["data"]["down"] == true {
                    let mut release = event.clone();
                    release["data"]["down"] = json!(false);
                    self.held.insert(identity, release);
                } else {
                    releases.push(identity);
                }
            }
        }
        if let Some(deadline) = deadline {
            qmp.arguments_before("input-send-event", json!({"events":events}), deadline)?;
        } else {
            qmp.arguments("input-send-event", json!({"events":events}))?;
        }
        for identity in releases {
            self.held.remove(&identity);
        }
        Ok(())
    }
    fn step(
        &mut self,
        args: &Value,
        frames: &vm_control::input::Frames,
        console: &Path,
        qmp: &mut Qmp,
    ) -> Result<Value> {
        let operation = args["operation_id"]
            .as_str()
            .context("missing input operation")?;
        if operation.len() != 32 || !operation.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!("invalid input operation")
        }
        if args["begin"] == true {
            if self.operation.is_some() {
                bail!("input busy: another operation is active")
            }
        } else if self.operation.as_deref() != Some(operation) {
            bail!("input operation expired or replaced")
        }
        let current = console_frame_status(console)?;
        frames.check(args, &current, Instant::now())?;
        if self.until.is_some_and(|until| Instant::now() >= until) {
            bail!("input operation deadline expired")
        }
        let events = args["events"].as_array().context("events missing")?;
        if events.is_empty() || events.len() > 16 {
            bail!("input event bound exceeded")
        }
        if args["begin"] == true {
            if self
                .completed_operations
                .iter()
                .any(|known| known == operation)
            {
                bail!("input operation was already completed")
            }
            self.operation = Some(operation.into());
            self.until = Some(Instant::now() + Duration::from_secs(4));
        }
        self.send_events(events, qmp)?;
        Ok(
            json!({"dispatched":true,"guest_delivery_confirmed":false,"source_generation":current["source_generation"]}),
        )
    }
}
fn log_pipe(mut pipe: impl Read + Send + 'static, path: Option<PathBuf>) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mut output = if let Some(path) = path {
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            bail!("diagnostic log must be owner-private, regular, and single-link")
        }
        Some(file)
    } else {
        None
    };
    let mut remaining = output
        .as_ref()
        .map(|f| {
            f.metadata()
                .map(|m| (8u64 * 1024 * 1024).saturating_sub(m.len()) as usize)
        })
        .transpose()?
        .unwrap_or(0);
    thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut buffer) {
            if n == 0 {
                break;
            }
            let keep = n.min(remaining);
            if keep > 0 {
                if output.as_mut().unwrap().write_all(&buffer[..keep]).is_err() {
                    remaining = 0;
                } else {
                    remaining -= keep;
                }
            }
            // Guest serial logging is opt-in. Always drain disabled/full/failed logs.
        }
    });
    Ok(())
}
/// Where one broker screenshot spent its time inside the worker, in microseconds. Numbers only.
#[derive(Clone, Copy, Debug)]
struct CaptureTiming {
    /// Request written until the reply's record header arrived: the helper and the broker.
    wait_us: u64,
    read_us: u64,
    parse_us: u64,
    bytes: u64,
    /// When the reader had its result; the loop measures how long it took to notice.
    ready: Instant,
}
impl CaptureTiming {
    /// The worker's diagnostic record for the daemon, sent just before the reply. `request` is not `id`: the daemon
    /// routes any record with an `id` as a reply.
    fn event(&self, request: &Value, noticed: Instant) -> Value {
        json!({"event":"screenshot_timing","request":request,"broker_wait_us":self.wait_us,"broker_read_us":self.read_us,"parse_us":self.parse_us,"wake_us":noticed.saturating_duration_since(self.ready).as_micros() as u64,"bytes":self.bytes})
    }
}
/// An error reply to the daemon; a refusal of the scaled screenshot form also carries its fixed word.
fn refusal_reply(id: &Value, error: &anyhow::Error) -> Value {
    let mut reply = json!({"id":id,"error":error.to_string()});
    Refused::mark(&mut reply, error);
    reply
}
/// What ends the worker loop's wait before its timeout.
enum Wake {
    Request(Result<Value>),
    /// A broker capture has its result, or the supervisor closed stdin: observe now instead of at the next step.
    /// Sent once per event and never while nothing is in flight, so an idle loop wakes exactly as often as before.
    Observe,
}
struct Capture {
    child: Option<QemuChild>,
    socket: Option<UnixStream>,
    receiver: mpsc::Receiver<(Result<Value>, Option<CaptureTiming>)>,
    timing: Option<CaptureTiming>,
    deadline: Instant,
    id: Value,
}
impl Capture {
    fn start(helper: &Path, socket: &Path, id: Value) -> Result<Self> {
        let mut child = QemuChild(
            Command::new(helper)
                .arg(socket)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("starting screenshot helper")?,
            None,
        );
        let mut pipe = child.0.stdout.take().unwrap();
        let (tx, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let result = (|| {
                let mut bytes = Vec::new();
                (&mut pipe)
                    .take((vm_control::MAX_SCREENSHOT_RESPONSE + 1) as u64)
                    .read_to_end(&mut bytes)?;
                if bytes.len() > vm_control::MAX_SCREENSHOT_RESPONSE {
                    bail!("screenshot output exceeds the 12 MiB response bound")
                }
                let mut value: Value = serde_json::from_slice(&bytes)
                    .context("screenshot helper emitted invalid JSON")?;
                if value.get("error").is_some() {
                    bail!("screenshot capture failed: {}", value["error"])
                }
                if value["mime_type"] != "image/png"
                    || value["byte_length"]
                        .as_u64()
                        .is_none_or(|n| n > vm_control::MAX_SCREENSHOT_BYTES as u64)
                    || !value["data_base64"].is_string()
                    || !value["frame_id"].is_string()
                {
                    bail!("invalid screenshot metadata")
                }
                // Internal renderer identity is not an external screenshot coordinate token.
                value
                    .as_object_mut()
                    .context("screenshot object required")?
                    .remove("internal_scanout_id");
                Ok(value)
            })();
            let _ = tx.send((result, None));
        });
        Ok(Self {
            child: Some(child),
            socket: None,
            receiver,
            timing: None,
            deadline: Instant::now() + Duration::from_secs(15),
            id,
        })
    }
    #[cfg(test)]
    fn broker(socket_path: &Path, id: Value, wake: mpsc::SyncSender<Wake>) -> Result<Self> {
        Self::broker_scaled(socket_path, id, wake, None)
    }
    /// `scaled` asks the broker for the scaled-JPEG form; `None` writes the same request as before.
    fn broker_scaled(
        socket_path: &Path,
        id: Value,
        wake: mpsc::SyncSender<Wake>,
        scaled: Option<Scaled>,
    ) -> Result<Self> {
        let mut socket = UnixStream::connect(socket_path).context("console broker unavailable")?;
        socket.set_write_timeout(Some(Duration::from_secs(1)))?;
        let request_id = format!("screenshot-{id}");
        let mut request = json!({"request_id":request_id,"method":"screenshot"});
        if let Some(scaled) = &scaled {
            scaled.apply(&mut request);
        }
        serde_json::to_writer(&mut socket, &request)?;
        socket.write_all(b"\n")?;
        let cancel = socket.try_clone()?;
        let deadline = Instant::now() + Duration::from_secs(15);
        let started = Instant::now();
        let (tx, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = tx.send(
                match read_console_screenshot_scaled(
                    &mut socket,
                    &request_id,
                    deadline,
                    started,
                    scaled,
                ) {
                    Ok((value, timing)) => (Ok(value), Some(timing)),
                    Err(error) => (Err(error), None),
                },
            );
            // A full queue means the loop is awake and polls this capture on every pass anyway.
            let _ = wake.try_send(Wake::Observe);
        });
        Ok(Self {
            child: None,
            socket: Some(cancel),
            receiver,
            timing: None,
            deadline,
            id,
        })
    }
    fn poll(&mut self) -> Option<Result<Value>> {
        if self.child.is_none() {
            match self.receiver.try_recv() {
                Ok((result, timing)) => {
                    self.timing = timing;
                    return Some(result);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Some(Err(anyhow::anyhow!("console request reader disconnected")))
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        match self
            .child
            .as_mut()
            .map(|child| child.0.try_wait())
            .transpose()
        {
            Ok(Some(Some(exit))) => match self.receiver.try_recv() {
                Ok((Err(error), _)) => return Some(Err(error)),
                Ok((Ok(value), _)) => {
                    return Some(if exit.success() {
                        Ok(value)
                    } else {
                        Err(anyhow::anyhow!("screenshot helper failed with {exit}"))
                    })
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Some(Err(anyhow::anyhow!("screenshot reader disconnected")))
                }
                Err(mpsc::TryRecvError::Empty) => {}
            },
            Err(error) => return Some(Err(error.into())),
            Ok(None) | Ok(Some(None)) => {}
        }
        if Instant::now() >= self.deadline {
            return Some(Err(anyhow::anyhow!(
                "screenshot exceeded 15 second deadline"
            )));
        }
        None
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(socket) = &self.socket {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }
}
fn read_console_reply(
    socket: &mut UnixStream,
    request_id: &str,
    deadline: Instant,
) -> Result<Value> {
    read_console_reply_bounded(socket, request_id, deadline, vm_control::MAX_MESSAGE)
}
fn read_console_reply_bounded(
    socket: &mut UnixStream,
    request_id: &str,
    deadline: Instant,
    limit: usize,
) -> Result<Value> {
    read_console_reply_stamped(socket, request_id, deadline, limit).map(|(reply, _)| reply)
}
/// `read_console_reply_bounded`, also returning when the record header arrived, when its body was complete and
/// its length.
fn read_console_reply_stamped(
    socket: &mut UnixStream,
    request_id: &str,
    deadline: Instant,
    limit: usize,
) -> Result<(Value, (Instant, Instant, usize))> {
    fn read_until(socket: &mut UnixStream, mut buffer: &mut [u8], deadline: Instant) -> Result<()> {
        while !buffer.is_empty() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("console screenshot deadline exceeded")?;
            use std::os::fd::AsRawFd;
            socket.set_nonblocking(true)?;
            let mut pollfd = libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe {
                libc::poll(
                    &mut pollfd,
                    1,
                    remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
                )
            };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if ready == 0 {
                bail!("console screenshot deadline exceeded")
            }
            let n = match socket.read(buffer) {
                Ok(n) => n,
                Err(error)
                    if [
                        std::io::ErrorKind::WouldBlock,
                        std::io::ErrorKind::Interrupted,
                    ]
                    .contains(&error.kind()) =>
                {
                    continue
                }
                Err(error) => return Err(error.into()),
            };
            if n == 0 {
                bail!("console disconnected before screenshot completed")
            }
            buffer = &mut buffer[n..];
        }
        Ok(())
    }
    let mut header = [0u8; 13];
    read_until(socket, &mut header, deadline)?;
    let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
    if header[4] != 1 || header[5..13] != [0; 8] || length == 0 || length > limit {
        bail!("invalid or oversized console screenshot frame")
    }
    let header_at = Instant::now();
    let mut body = vec![0u8; length];
    read_until(socket, &mut body, deadline)?;
    let stamps = (header_at, Instant::now(), length);
    let mut reply: Value = serde_json::from_slice(&body)?;
    drop(body);
    if reply["request_id"] != request_id {
        bail!("console screenshot response identity mismatch")
    }
    if let Some(error) = reply.get("error") {
        if request_id == "native-frame" {
            if let Some(stage) = vm_control::native_input::RejectionStage::display_rejection(
                &reply["display_rejection"],
            ) {
                return Err(stage.into());
            }
        }
        // The broker's fixed word for a refused scaled screenshot stays beside the same text.
        if let Some(refusal) = Refusal::from_name(&reply[REFUSAL_FIELD]) {
            return Err(Refused::new(refusal, format!("console operation failed: {error}")).into());
        }
        bail!("console operation failed: {error}")
    }
    reply
        .get_mut("result")
        .map(Value::take)
        .context("missing console result")
        .map(|result| (result, stamps))
}
fn console_window(path: &Path, method: &str) -> Result<Value> {
    if !["show_window", "close_window", "window_status"].contains(&method) {
        bail!("invalid native window operation")
    }
    let mut socket = UnixStream::connect(path)?;
    socket.set_write_timeout(Some(Duration::from_millis(500)))?;
    serde_json::to_writer(
        &mut socket,
        &json!({"request_id":"native-window","method":method}),
    )?;
    socket.write_all(b"\n")?;
    let result = read_console_reply(
        &mut socket,
        "native-window",
        Instant::now() + Duration::from_secs(3),
    )?;
    vm_control::backend::validate_window_capabilities(result.clone())?;
    if !result["window_open"].is_boolean() || !vm_control::backend::window_supported(&result) {
        bail!("invalid native display-only window acknowledgement")
    }
    if (method == "show_window" && result["window_open"] != true)
        || (method == "close_window" && result["window_open"] != false)
    {
        bail!("native window did not acknowledge requested state")
    }
    Ok(result)
}
fn console_frame_status(path: &Path) -> Result<Value> {
    let mut socket = UnixStream::connect(path)?;
    socket.set_write_timeout(Some(Duration::from_millis(500)))?;
    socket.write_all(b"{\"request_id\":\"frame-input\",\"method\":\"frame_status\"}\n")?;
    read_console_reply(
        &mut socket,
        "frame-input",
        Instant::now() + Duration::from_millis(750),
    )
}
#[cfg(test)]
fn read_console_screenshot(
    socket: &mut UnixStream,
    request_id: &str,
    deadline: Instant,
) -> Result<Value> {
    read_console_screenshot_timed(socket, request_id, deadline, Instant::now())
        .map(|(value, _)| value)
}
/// `started` is when the request was written; the timing is relative to it.
#[cfg(test)]
fn read_console_screenshot_timed(
    socket: &mut UnixStream,
    request_id: &str,
    deadline: Instant,
    started: Instant,
) -> Result<(Value, CaptureTiming)> {
    read_console_screenshot_scaled(socket, request_id, deadline, started, None)
}
/// `read_console_screenshot_timed` for a request that may have asked for the scaled form. With `scaled`, the reply
/// may also be the helper's scaled JPEG, which must then agree with the request and with itself
/// (`Scaled::verify`); a PNG reply to a scaled request comes from a helper without that form and is held to the
/// same checks as any PNG.
fn read_console_screenshot_scaled(
    socket: &mut UnixStream,
    request_id: &str,
    deadline: Instant,
    started: Instant,
    scaled: Option<Scaled>,
) -> Result<(Value, CaptureTiming)> {
    let (mut value, (header_at, body_at, bytes)) = read_console_reply_stamped(
        socket,
        request_id,
        deadline,
        vm_control::MAX_SCREENSHOT_RESPONSE,
    )?;
    let jpeg = scaled.filter(|_| value["mime_type"] == "image/jpeg");
    let (mime, limit) = match jpeg {
        Some(_) => ("image/jpeg", vm_control::screenshot_scaled::MAX_BYTES),
        None => ("image/png", vm_control::MAX_SCREENSHOT_BYTES),
    };
    if value["mime_type"] != mime
        || value["byte_length"]
            .as_u64()
            .is_none_or(|n| n > limit as u64)
        || !value["data_base64"].is_string()
        || !value["captured_monotonic_us"].is_u64()
        || !value["width"].is_u64()
        || !value["height"].is_u64()
    {
        bail!("invalid console screenshot metadata")
    }
    if let Some(scaled) = jpeg {
        scaled
            .verify(&value)
            .context("invalid console scaled screenshot")?;
    }
    let mut random = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    random[6] = (random[6] & 0x0f) | 0x40;
    random[8] = (random[8] & 0x3f) | 0x80;
    let hex = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    value["frame_id"] = json!(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ));
    value["display_id"] = json!(0);
    value["guest_ui_scale"] = Value::Null;
    value.as_object_mut().unwrap().remove("internal_scanout_id");
    let ready = Instant::now();
    let micros = |from: Instant, to: Instant| to.saturating_duration_since(from).as_micros() as u64;
    let timing = CaptureTiming {
        wait_us: micros(started, header_at),
        read_us: micros(header_at, body_at),
        parse_us: micros(body_at, ready),
        bytes: bytes as u64,
        ready,
    };
    Ok((value, timing))
}
struct ConsoleBroker {
    child: QemuChild,
    capabilities: Value,
}
impl ConsoleBroker {
    fn start(
        binary: &Path,
        helper: &Path,
        spice: &Path,
        socket: &Path,
        stderr: PathBuf,
        native_args: &[String],
    ) -> Result<Self> {
        let mut owned = Self {
            capabilities: Value::Null,
            child: QemuChild(
                Command::new(binary)
                    .arg(helper)
                    .arg(spice)
                    .arg(socket)
                    .args(native_args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .context("starting console broker")?,
                None,
            ),
        };
        log_pipe(owned.child.0.stderr.take().unwrap(), Some(stderr))?;
        let stdout = owned.child.0.stdout.take().unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let reply = (|| {
                let bytes =
                    read_frame(&mut reader)?.context("console broker closed before ready")?;
                serde_json::from_slice::<Value>(&bytes)
                    .context("invalid console readiness response")
            })();
            let _ = tx.send(reply);
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        });
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            if STOP.load(Ordering::Acquire) {
                bail!("console startup interrupted")
            }
            if let Some(exit) = owned.child.0.try_wait()? {
                bail!("console broker exited before ready: {exit}")
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(reply) => {
                    let reply = reply?;
                    if reply["event"] != "ready"
                        || reply["protocol"] != 1
                        || reply["capture_connected"] != true
                    {
                        bail!("console broker did not confirm capture connection")
                    }
                    owned.capabilities = reply["capabilities"].clone();
                    if !owned.capabilities.is_object() {
                        bail!("invalid console capabilities")
                    }
                    if owned.capabilities["broker_window_protocol"] != 1 {
                        owned.capabilities["native_window"] = json!(false);
                    }
                    return Ok(owned);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("console broker readiness channel closed")
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if Instant::now() >= deadline {
                bail!("console broker readiness exceeded 12 seconds")
            }
        }
    }
}
impl Drop for ConsoleBroker {
    fn drop(&mut self) {
        if self.child.0.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.child.0.id() as i32, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            while self.child.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
        }
        // QemuChild's fallback kills/reaps only this still-owned child.
    }
}
/// Returns true when the guest rebooted and the worker must start a fresh QEMU.
fn run() -> Result<bool> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        bail!("QEMU/HVF backend requires Apple Silicon macOS")
    }
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("run") {
        bail!("usage: vm-qemu run --config FILE --launcher FILE --library FILE --resources DIR")
    }
    let mut options = std::collections::BTreeMap::new();
    while let Some(key) = args.next() {
        if ![
            "--control-socket",
            "--vm-id",
            "--worker-instance",
            "--config",
            "--launcher",
            "--library",
            "--resources",
            "--spice-library",
            "--venus-runtime",
            "--screenshot-helper",
            "--console-broker",
            "--console-helper",
        ]
        .contains(&key.as_str())
            || options.contains_key(&key)
        {
            bail!("unknown or duplicate runtime option")
        }
        options.insert(
            key,
            PathBuf::from(args.next().context("missing option value")?),
        );
    }
    if options.contains_key("--console-broker") != options.contains_key("--console-helper") {
        bail!("console broker and helper must be supplied together")
    }
    let native_fields = ["--control-socket", "--vm-id", "--worker-instance"];
    let native_count = native_fields
        .iter()
        .filter(|key| options.contains_key(**key))
        .count();
    if native_count != 0 && native_count != 3 {
        bail!("native control arguments must be complete")
    }
    let mut native_args = Vec::new();
    if native_count == 3 {
        if !Path::new(&options["--control-socket"]).is_absolute()
            || !vm_control::native_input::hex(
                options["--worker-instance"]
                    .to_str()
                    .context("native nonce must be UTF8")?,
                64,
            )
        {
            bail!("invalid native control binding")
        }
        for key in native_fields {
            native_args.push(key.into());
            native_args.push(
                options[key]
                    .to_str()
                    .context("native argument must be UTF8")?
                    .to_owned(),
            );
        }
    }
    let config_path = options.remove("--config").context("missing config")?;
    let bytes = fs::read(&config_path)?;
    if bytes.len() > 65536 {
        bail!("config exceeds 64KiB")
    }
    let config: Config = serde_json::from_slice(&bytes)?;
    config.validate()?;
    if config.backend != Backend::QemuHvf {
        bail!("wrong backend for QEMU worker")
    }
    if native_count == 3 && options["--vm-id"].to_str() != Some(config.id.as_str()) {
        bail!("native VM binding mismatch")
    }
    if config.window && !options.contains_key("--console-broker") {
        bail!("native window requires persistent console registration")
    }
    let mut launcher = options.remove("--launcher").context("missing launcher")?;
    let library = options.remove("--library").context("missing library")?;
    let resources = options.remove("--resources").context("missing resources")?;
    // Neptune reuses the reviewed Venus registration for its render server.
    let neptune_registration = if config.qemu_graphics_profile.neptune() {
        let registration = vm_control::qemu_graphics::Registration::load(
            options.get("--venus-runtime").context(
                "Neptune needs the locally registered Venus runtime (its render server)",
            )?,
        )?;
        registration.validate(
            &library,
            &resources,
            options.get("--spice-library").map(PathBuf::as_path),
        )?;
        Some(registration)
    } else {
        None
    };
    let (venus, graphics_host) = if config.qemu_graphics_profile.venus() {
        let registration = vm_control::qemu_graphics::Registration::load(
            options
                .get("--venus-runtime")
                .context("Venus runtime not locally registered")?,
        )?;
        registration.validate(
            &library,
            &resources,
            options.get("--spice-library").map(PathBuf::as_path),
        )?;
        let host = vm_control::qemu_graphics::host_admission()?;
        launcher = registration.loader.path.clone();
        (Some(registration), Some(host))
    } else {
        (None, None)
    };
    // Only signal-safe atomic work in handlers. Blocking operations run on the main loop.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            stop_signal as *const () as libc::sighandler_t,
        );
        libc::signal(libc::SIGINT, stop_signal as *const () as libc::sighandler_t);
    }
    let runtime = RuntimeDirectory::new()?;
    let qmp_path = runtime.0.join("qmp.sock");
    let spice_path = runtime.0.join("spice.sock");
    let console_path = runtime.0.join("console.sock");
    let agent_path = runtime.0.join("agent.sock");
    let driver_path = runtime.0.join("driver.sock");
    let uefi = config.boot == vm_control::BootMode::Uefi;
    // Windows ARM hangs in edk2 after an in-process (warm) reset on HVF; QEMU
    // exits on a guest reboot instead and run() returns to start a fresh one.
    let cold_reboot = config.guest == "windows";
    let monitor_path = config.qemu_monitor.then(|| runtime.0.join("monitor.sock"));
    let disk_guard = config
        .disk
        .as_deref()
        .map(vm_control::qemu_disk::DiskGuard::open)
        .transpose()?;
    let guardian = if config.qemu_guest_exec {
        Some(guardian::Parent::new()?)
    } else {
        None
    };
    let mut command = if let Some(watch) = &guardian {
        let mut c = Command::new(std::env::current_exe()?);
        watch.wrap(&mut c)?;
        c.arg(launcher);
        c
    } else {
        Command::new(launcher)
    };
    vm_control::qemu_graphics::clean_environment(&mut command);
    if let Some(registration) = &venus {
        registration.configure(&mut command);
    }
    if let Some(registration) = &neptune_registration {
        vm_control::qemu_graphics::Neptune::locate(&library, registration)?
            .configure(&mut command, &runtime.0);
    }
    command
        .arg(library)
        .arg("-L")
        .arg(&resources)
        .args(config.qemu_graphics_profile.machine_arguments())
        .args([
            "-cpu",
            "host",
            "-nodefaults",
            "-display",
            "none",
            "-monitor",
            "none",
            "-serial",
            "stdio",
            "-nic",
        ])
        .arg(vm_control::qemu_extras::nic(&config))
        .arg("-device")
        .arg(vm_control::qemu_uefi::display_device(&config)?)
        .arg("-m")
        .arg(format!("{}M", config.memory_mib))
        .arg("-smp")
        .arg(config.cpus.to_string())
        .arg("-qmp")
        .arg(format!("unix:{},server=on,wait=off", qmp_path.display()))
        .arg("-spice")
        // agent-mouse=off: a Guest spice-vdagent shares only the clipboard; pointer input
        // keeps going through the SPICE inputs channel.
        .arg(format!(
            "unix=on,addr={},disable-ticketing=on,agent-mouse=off,gl={}",
            spice_path.display(),
            if config.qemu_graphics_profile.requires_gl() {
                "es"
            } else {
                "off"
            }
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if config.qemu_graphics_profile.requires_gl() {
        command.env("ANGLE_DEFAULT_PLATFORM", "metal");
    }
    if options.contains_key("--console-broker") || disk_guard.is_some() || venus.is_some() {
        command.arg("-S");
    }
    if uefi {
        vm_control::qemu_uefi::prepare_vars(
            &resources.join(vm_control::qemu_uefi::VARS_TEMPLATE_FILE),
            config.efi_vars.as_deref().context("efi_vars required")?,
        )?;
        command.args(vm_control::qemu_uefi::arguments(
            &config,
            &resources,
            monitor_path.as_deref(),
        )?);
        if cold_reboot {
            command.args(["-action", "reboot=shutdown"]);
        }
    } else {
        command
            .arg("-bios")
            .arg(resources.join("edk2-aarch64-code.fd"))
            .args([
                "-device",
                "virtio-keyboard-pci",
                "-device",
                "virtio-tablet-pci",
                // The Guest reports the memory it frees; the loader returns it to macOS
                // (native/tools/qemu-ram-reclaim.h).
                "-device",
                vm_control::qemu_uefi::BALLOON_DEVICE,
            ])
            .arg("-kernel")
            .arg(config.kernel.as_ref().context("kernel required")?)
            .arg("-append")
            .arg(config.command_line.as_deref().unwrap_or("console=ttyAMA0"));
        if let Some(initrd) = &config.initrd {
            command.arg("-initrd").arg(initrd);
        }
        if let Some(path) = &monitor_path {
            command
                .arg("-qmp")
                .arg(format!("unix:{},server=on,wait=off", path.display()));
        }
    }
    command
        .args(["-device", "virtio-serial-pci,id=veynode-serial", "-chardev"])
        .arg(format!(
            "socket,id=veynode-agent,path={},server=on,wait=off",
            agent_path.display()
        ))
        .args([
            "-device",
            "virtserialport,bus=veynode-serial.0,chardev=veynode-agent,name=org.veynode.agent.0",
        ]);
    // cua-driver's own port, so its JSON-RPC traffic stays out of the fenced exec channel.
    let driver = config.guest == "linux";
    if driver {
        command
            .arg("-chardev")
            .arg(format!(
                "socket,id=veynode-driver,path={},server=on,wait=off",
                driver_path.display()
            ))
            .args([
                "-device",
                "virtserialport,bus=veynode-serial.0,chardev=veynode-driver,name=veynode.driver",
            ]);
        // The SPICE agent's port: a Guest spice-vdagent shares the clipboard with the viewers.
        command.args([
            "-chardev",
            "spicevmc,id=veynode-vdagent,name=vdagent",
            "-device",
            "virtserialport,bus=veynode-serial.0,chardev=veynode-vdagent,name=com.redhat.spice.0",
        ]);
    }
    if let Some(spice_library) = options.get("--spice-library") {
        command.env("VEYNODE_SPICE_LIBRARY", spice_library);
    }
    if let Some(disk) = &disk_guard {
        disk.configure_command(&mut command, vm_control::qemu_uefi::disk_device(&config))?;
    }
    command.args(vm_control::qemu_extras::arguments(&config)?);
    command.args(vm_control::qemu_audio::arguments(&config));
    // Last, so every device before them keeps the PCI slot a machine's firmware recorded.
    command.args(vm_control::qemu_uefi::trailing_devices(&config));
    let mut child = QemuChild(
        command.spawn().context("starting QEMU/HVF runtime")?,
        guardian,
    );
    let backend_pid = if let Some(watch) = &mut child.1 {
        watch.accept()?
    } else {
        child.0.id()
    };
    let log_prefix = config_path
        .parent()
        .context("config has no parent")?
        .join(format!("{}-{}", config.id, child.0.id()));
    let serial_log = config.serial_log.clone();
    let stderr_log = log_prefix.with_extension("stderr.log");
    log_pipe(child.0.stdout.take().unwrap(), serial_log.clone())?;
    log_pipe(child.0.stderr.take().unwrap(), Some(stderr_log.clone()))?;
    let (tx, rx) = mpsc::sync_channel(16);
    let capture_wake = tx.clone();
    thread::spawn(move || {
        let mut reader = BufReader::new(std::io::stdin());
        loop {
            match read_frame(&mut reader) {
                Ok(Some(bytes)) => {
                    if tx
                        .send(Wake::Request(
                            serde_json::from_slice::<Value>(&bytes).map_err(anyhow::Error::from),
                        ))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(None) => {
                    STOP.store(true, Ordering::Release);
                    // The loop keeps a sender for capture wakes, so the channel no longer disconnects here.
                    let _ = tx.try_send(Wake::Observe);
                    break;
                }
                Err(e) => {
                    STOP.store(true, Ordering::Release);
                    let _ = tx.send(Wake::Request(Err(e)));
                    break;
                }
            }
        }
    });
    let mut qmp = Qmp::connect(&qmp_path, &mut child.0)?;
    if let Some(disk) = &disk_guard {
        disk.verify_fdsets(&qmp.call("query-fdsets")?)?;
    }
    if venus.is_some() {
        vm_control::qemu_graphics::verify_gpu_properties(|method, args| {
            qmp.arguments(method, args)
        })?;
    }
    let guest = if config.qemu_guest_exec {
        None
    } else {
        Some(guest_transport::Transport::start(&agent_path))
    };
    let guest_exec = config
        .qemu_guest_exec
        .then(|| {
            // Readiness and replies wake the loop instead of waiting out its timeout.
            let wake = capture_wake.clone();
            guest_transport::nonce().map(|host| {
                guest_exec::Transport::start_notifying(&agent_path, host, move || {
                    let _ = wake.try_send(Wake::Observe);
                })
            })
        })
        .transpose()?;
    let mut exec_pending: Vec<(Value, guest_exec::Pending)> = Vec::new();
    let driver_channel =
        driver.then(|| vm_control::qemu_driver_channel::Channel::new(&driver_path));
    let mut driver_pending: Vec<(Value, vm_control::qemu_driver_channel::Pending)> = Vec::new();
    let mut guest_pending: Option<(Value, guest_transport::Pending)> = None;
    let mut broker = if let Some(binary) = options.get("--console-broker") {
        let broker = ConsoleBroker::start(
            binary,
            &options["--console-helper"],
            &spice_path,
            &console_path,
            log_prefix.with_extension("console.log"),
            &native_args,
        )?;
        if config.window {
            if !vm_control::backend::window_supported(&broker.capabilities) {
                bail!("native display-only window unavailable")
            }
            console_window(&console_path, "show_window")?;
        }
        Some(broker)
    } else {
        None
    };
    let status = qmp.call("query-status")?;
    let mut state = json!({"event":"state","state":if status["running"]==true {"running"}else{"paused"},"qmp_status":status["status"],"guest_ready":null,"vm_id":config.id,"backend_pid":backend_pid,"spice_socket":spice_path,"serial_log":serial_log,"stderr_log":stderr_log,"log_limit_bytes_per_stream":8388608});
    if let Some(host) = graphics_host {
        state["graphics_profile"] = json!({"configured":config.qemu_graphics_profile,"host_supported":true,"guest_verified":false,"host_admission":host,"runtime_revision":"owned-group-v1","qmp_gpu_properties_verified":true,"ipa_granule_requested_bytes":4096,"hostmem_aperture_bytes":268435456u64,"renderer_ownership":"inherits-worker-process-group"});
    }
    if let Some(disk) = &disk_guard {
        let identity = disk.identity();
        state["disk_provenance"] = json!({"device":identity.device,"inode":identity.inode,"descriptor_backed":true,"qmp_fdsets_verified_at_start":true});
    }
    state["boot"] = json!(config.boot);
    state["monitor_socket"] = monitor_path.as_ref().map_or(Value::Null, |p| json!(p));
    state["console_socket"] = if broker.is_some() {
        json!(console_path)
    } else {
        Value::Null
    };
    state["capture_mode"] = json!(if broker.is_some() {
        "persistent-console"
    } else if options.contains_key("--screenshot-helper") {
        "diagnostic-helper"
    } else {
        "disabled"
    });
    state["capture_connected"] = json!(broker.is_some());
    if config.qemu_guest_exec {
        state["execution_guardian"] = json!({"guardian_pid":child.0.id(),"backend_pid":backend_pid,"heartbeat_timeout_ms":5000,"startup_timeout_ms":30000,"owns_qemu_child":true});
    }
    state["native_window_capabilities"] = broker
        .as_ref()
        .map(|b| b.capabilities.clone())
        .unwrap_or(Value::Null);
    state["native_window_available"] = json!(broker
        .as_ref()
        .is_some_and(|b| vm_control::backend::window_supported(&b.capabilities)));
    publish_startup_then_resume(
        &mut qmp,
        &mut state,
        broker.is_some() || disk_guard.is_some() || venus.is_some(),
        emit,
    )?;
    let mut status_poll = StatusPoll::new(Instant::now());
    let mut capture: Option<Capture> = None;
    let mut frames = vm_control::input::Frames::default();
    let mut input = InputState::default();
    let mut native_input = native_input_worker::NativeInput::default();
    let mut execution_shutdown = ExecutionShutdown::default();
    let mut guardian_heartbeat = Instant::now() - Duration::from_secs(1);
    // Whether the host asked the Guest to power off (`shutdown`).
    let mut host_powerdown = false;
    // The policy, and whether QEMU polls the Guest for statistics yet.
    let mut balloon = vm_control::qemu_uefi::windows(&config).then(|| {
        (
            vm_control::qemu_balloon::Policy::new(config.memory_mib << 20, Instant::now()),
            false,
        )
    });
    loop {
        if config.qemu_guest_exec {
            let mut parent = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut parent, 1, 0) };
            if ready < 0 {
                bail!("execution supervisor pipe observation failed")
            }
            if ready > 0 && parent.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                break;
            }
        }
        // Observe the owned child before reading its guardian socket: normal exit closes both.
        if let Some(status) = child.0.try_wait()? {
            if status.success() && cold_reboot && qmp.exited_for_guest_reset() {
                emit(
                    json!({"event":"state","state":"starting","reason":"guest-reset","guest_ready":null,"vm_id":config.id}),
                )?;
                return Ok(true);
            }
            emit(
                json!({"event":"state","state":if status.success(){"stopped"}else{"crashed"},"reason":exit_reason(status.success(), host_powerdown),"exit_code":status.code(),"guest_ready":null,"vm_id":config.id,"qmp_run_state":status_poll.summary()}),
            )?;
            if !status.success() {
                bail!("QEMU process crashed: {status}")
            }
            return Ok(false);
        }
        execution_shutdown.check(Instant::now())?;
        if guardian_heartbeat.elapsed() >= Duration::from_millis(500) {
            if let Some(watch) = &mut child.1 {
                if let Err(error) = watch.heartbeat() {
                    if !execution_shutdown.active() {
                        return Err(error);
                    }
                    // A terminating guardian may close just before waitable exit. Never
                    // call this success: continue polling the actual child under deadline.
                }
            }
            guardian_heartbeat = Instant::now();
        }
        if !execution_shutdown.active()
            && guest_exec.as_ref().is_some_and(|g| g.state() == "fenced")
        {
            bail!("serial execution incarnation fenced; QEMU will be terminated")
        }
        let mut index = 0;
        while index < exec_pending.len() {
            if let Some(result) = exec_pending[index].1.poll() {
                let (id, _) = exec_pending.remove(index);
                let value = result?;
                emit(json!({"id":id,"result":value}))?;
            } else {
                index += 1
            }
        }
        let mut index = 0;
        while index < driver_pending.len() {
            if let Some(result) = driver_pending[index].1.poll() {
                let (id, _) = driver_pending.remove(index);
                emit(match result {
                    Ok(frame) => json!({"id":id,"result":{"frame":frame}}),
                    Err(error) => json!({"id":id,"error":error.to_string()}),
                })?;
            } else {
                index += 1
            }
        }
        if input.until.is_some_and(|until| Instant::now() >= until) {
            input.cleanup(&mut qmp)?;
        }
        if let Some((id, pending)) = &mut guest_pending {
            if let Some(result) = pending.poll() {
                let id = id.clone();
                guest_pending = None;
                emit(match result {
                    Ok(value) => json!({"id":id,"result":value}),
                    Err(error) => json!({"id":id,"error":error.to_string()}),
                })?;
            }
        }
        if let Some(active) = &mut capture {
            if let Some(result) = active.poll() {
                let id = active.id.clone();
                let timing = active.timing.take().filter(|_| result.is_ok());
                drop(capture.take());
                if let Some(timing) = timing {
                    emit(timing.event(&id, Instant::now()))?;
                }
                emit(match result {
                    Ok(value) => {
                        frames.record(&value, Instant::now());
                        json!({"id":id,"result":value})
                    }
                    Err(error) => refusal_reply(&id, &error),
                })?;
            }
        }
        if STOP.load(Ordering::Acquire) {
            break;
        }
        if let Some(active) = &mut broker {
            if let Some(exit) = active.child.0.try_wait()? {
                // Retain the VM, but fail capture closed. Never reconnect a static source silently.
                state["capture_error"] = json!(format!("console broker exited: {exit}"));
                state["capture_connected"] = json!(false);
                state["native_window_available"] = json!(false);
                emit(state.clone())?;
                drop(broker.take());
            }
        }
        if let Some(status) = child.0.try_wait()? {
            if status.success() && cold_reboot && qmp.exited_for_guest_reset() {
                emit(
                    json!({"event":"state","state":"starting","reason":"guest-reset","guest_ready":null,"vm_id":config.id}),
                )?;
                return Ok(true);
            }
            emit(
                json!({"event":"state","state":if status.success(){"stopped"}else{"crashed"},"reason":exit_reason(status.success(), host_powerdown),"exit_code":status.code(),"guest_ready":null,"vm_id":config.id,"qmp_run_state":status_poll.summary()}),
            )?;
            if !status.success() {
                bail!("QEMU process crashed: {status}")
            }
            return Ok(false);
        }
        observe_status(
            &mut qmp,
            &mut status_poll,
            &mut state,
            Instant::now(),
            capture.is_none(),
            emit,
        )?;
        if let Some((policy, polling)) = &mut balloon {
            if capture.is_none() && policy.due(Instant::now()) {
                balloon_step(&mut qmp, policy, polling, state["state"] == "running");
            }
        }
        if let Some(exec) = &guest_exec {
            // Lets the daemon refuse execution while the transport is not ready. A local read,
            // so it keeps its own cadence instead of waiting for the next QMP status query.
            let integration = json!(exec.state());
            if state["guest_integration_state"] != integration {
                state["guest_integration_state"] = integration;
                emit(state.clone())?;
            }
        }
        // A request or a finished broker capture wakes this at once. The timeout only bounds how stale the polled
        // state above may get, so it stays at 50 ms while anything in flight is being polled for.
        let request = match rx.recv_timeout(request_wait(
            !exec_pending.is_empty()
                || !driver_pending.is_empty()
                || guest_pending.is_some()
                || capture.is_some()
                || input.until.is_some()
                || execution_shutdown.active(),
        )) {
            Ok(Wake::Request(request)) => request?,
            Ok(Wake::Observe) | Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let id = request["id"].clone();
        if execution_shutdown.active()
            && matches!(
                request["method"].as_str(),
                Some(
                    "guest_request"
                        | "driver_rpc"
                        | "native_begin"
                        | "native_heartbeat"
                        | "native_event"
                        | "input_step"
                        | "desktop_input"
                        | "input_text_step"
                )
            )
        {
            emit(json!({"id":id,"error":"VM execution is quiescing for shutdown"}))?;
            continue;
        }
        let result: Result<Value> = match request["method"].as_str() {
            Some("guest_request") => {
                if let Some(exec)=&guest_exec {
                    match exec.request_before(request["request"].clone(), Instant::now()+Duration::from_secs(1)){Ok(pending)=>{exec_pending.push((id.clone(),pending));continue},Err(error)=>Err(error)}
                } else if guest_pending.is_some() {Err(anyhow::anyhow!("guest integration busy: one request per VM"))}
                else {match guest.as_ref().unwrap().request(request["request"].clone()) {Ok(pending)=>{guest_pending=Some((id.clone(),pending));continue},Err(error)=>Err(error)}}
            },
            // One cua-driver JSON-RPC frame over the veynode.driver port; replies arrive in any order.
            Some("driver_rpc") => match &driver_channel {
                None => Err(anyhow::anyhow!("unsupported: no cua-driver port for this Guest")),
                Some(_) if driver_pending.len() >= 16 => Err(anyhow::anyhow!("busy: too many driver calls in flight")),
                Some(channel) => {
                    let deadline = Duration::from_millis(request["deadline_ms"].as_u64().unwrap_or(30_000));
                    match channel.request(request["frame"].clone(), deadline) {
                        Ok(pending) => { driver_pending.push((id.clone(), pending)); continue }
                        Err(error) => Err(error),
                    }
                }
            },
            Some(method @ ("show_window" | "close_window" | "window_status")) => {
                if !broker.as_ref().is_some_and(|b|vm_control::backend::window_supported(&b.capabilities)) {
                    Err(anyhow::anyhow!("native display-only window unavailable"))
                } else { console_window(&console_path,method) }
            },
            Some("screenshot") => {
                // The four fields of the scaled-JPEG form, all or none; `None` is the request as it always was.
                let scaled = match Scaled::parse(&request) { Ok(scaled) => scaled, Err(error) => { emit(refusal_reply(&id, &error))?; continue } };
                if capture.is_some() { Err(anyhow::anyhow!("screenshot busy: one capture per VM")) }
                else if options.contains_key("--console-broker") {
                    if broker.is_none() { Err(anyhow::anyhow!("console broker unavailable; VM restart required to restore capture")) }
                    // A broker from before the scaled form would drop the connection on the unknown fields.
                    else if scaled.is_some() && broker.as_ref().is_some_and(|b| b.capabilities["broker_screenshot_scaled_protocol"] != 1) { Err(Refused::new(Refusal::FormatUnavailable, "unsupported: this console broker has no scaled JPEG screenshot form").into()) }
                    else { match Capture::broker_scaled(&console_path,id.clone(),capture_wake.clone(),scaled) { Ok(started)=>{capture=Some(started);continue},Err(error)=>Err(error) } }
                }
                // The one-shot capture helper has no scaled form.
                else if scaled.is_some() { Err(Refused::new(Refusal::FormatUnavailable, "unsupported: the scaled JPEG screenshot form requires the console broker").into()) }
                else if let Some(helper) = options.get("--screenshot-helper") {
                    match Capture::start(helper, &spice_path, id.clone()) { Ok(started) => { capture = Some(started); continue; }, Err(error) => Err(error) }
                } else { Err(anyhow::anyhow!("unsupported: screenshot helper is not registered")) }
            }
            Some("frame_status") => {
                if broker.is_none() { Err(anyhow::anyhow!("console broker unavailable")) }
                else { console_frame_status(&console_path) }
            },
            Some(method @ ("native_begin" | "native_heartbeat" | "native_event" | "native_end")) => native_input.handle(method,&request["arguments"],&mut input,&console_path,&mut qmp),
            Some("input_step") => {
                if broker.is_none() {Err(anyhow::anyhow!("persistent console unavailable"))}
                else {input.step(&request["arguments"],&frames,&console_path,&mut qmp)}
            },
            Some("input_text_step") => {
                if broker.is_none() {Err(anyhow::anyhow!("persistent console unavailable"))}
                else {
                    let submit = guest_exec.as_ref().map(|exec| move |request: Value, deadline: Instant| exec.request_before(request, deadline));
                    match native_input_worker::text_step(&request["arguments"], &mut input, &frames, &console_path, submit.as_ref().map(|f| f as &dyn Fn(Value, Instant) -> Result<guest_exec::Pending>)) {
                        Ok(pending) => { exec_pending.push((id.clone(), pending)); continue },
                        Err(error) => Err(error),
                    }
                }
            },
            Some("input_cleanup") => {
                input.cleanup_operation(&request["arguments"], &mut qmp)
            },
            Some("desktop_input") => {
                // The daemon validated and translated the batch; MCP or native
                // window operations in flight keep exclusive use of the devices.
                if input.operation.is_some() || native_input.active(&input) {
                    Err(anyhow::anyhow!("desktop input busy: another input operation is active"))
                } else {
                    let events = request["arguments"]["events"].clone();
                    if events.as_array().is_none_or(|e| e.is_empty() || e.len() > 4 * vm_control::desktop_input::MAX_EVENTS) {
                        Err(anyhow::anyhow!("invalid desktop input batch"))
                    } else {
                        // One sync per command: keep a click's down and up from collapsing.
                        vm_control::desktop_input::sync_groups(events.as_array().unwrap())
                            .into_iter()
                            .try_for_each(|group| qmp.arguments("input-send-event", json!({"events":group})).map(|_| ()))
                            .map(|_| json!({"sent":true}))
                    }
                }
            }
            Some("status") => qmp.call("query-status").map(|status|json!({"qmp_status":status,"qmp_run_state":status_poll.summary(),"guest_ready":null,"vm_id":config.id,"disk_provenance":state.get("disk_provenance"),"guest_integration_state":guest.as_ref().map(|g|g.state()).unwrap_or_else(||guest_exec.as_ref().unwrap().state())})),
            Some("shutdown") => {
                host_powerdown = true;
                if guest_exec.is_some() {
                    execution_shutdown.begin(Instant::now());
                }
                // Powerdown is the worker's one run-state command after `cont`: query on the next pass, then at 1 s.
                status_poll.disturbed(Instant::now());
                request_powerdown(&mut qmp, &mut input, guest_exec.is_some()).map(|_|json!({"requested":true,"mechanism":"qmp-system-powerdown","completed":false,"requires_guest_power_button_support":true,"execution_quiescing":execution_shutdown.active(),"shutdown_timeout_seconds":if execution_shutdown.active(){json!(60)}else{Value::Null}}))
            },
            // Idle pause and `veynodevm pause`: vCPUs stop, memory stays. Only a running VM pauses and only a
            // paused one resumes, never one in prelaunch, a migration or a guest-panicked state.
            Some(method @ ("pause" | "resume")) => {
                let (from, command) = if method == "pause" { ("running", "stop") } else { ("paused", "cont") };
                match qmp.call("query-status") {
                    Ok(status) if status["status"] == from => {
                        status_poll.disturbed(Instant::now());
                        let to = if method == "pause" { "paused" } else { "running" };
                        // The serial execution transport stops expecting heartbeats before the vCPUs stop,
                        // and expects them again only once they run: a stopped Guest's silence would fence it.
                        if method == "pause" { if let Some(exec) = &guest_exec { exec.set_paused(true) } }
                        let result = qmp.call(command);
                        if let Some(exec) = &guest_exec { exec.set_paused(if result.is_ok() { method == "pause" } else { method != "pause" }) }
                        match result {
                            // Published now, so the daemon's state is current when the reply arrives.
                            Ok(_) => { state["state"] = json!(to); state["qmp_status"] = json!(to); emit(state.clone())?; Ok(json!({"state":to})) }
                            Err(error) => Err(error),
                        }
                    }
                    Ok(status) => Err(anyhow::anyhow!("invalid_state: QEMU run state is {}", status["status"])),
                    Err(error) => Err(error),
                }
            },
            Some("stop") => { emit(json!({"id":id,"result":{"requested":true,"forced":true}}))?; break },
            _ => Err(anyhow::anyhow!("unsupported QEMU worker operation")),
        };
        emit(match result {
            Ok(value) => json!({"id":id,"result":value}),
            Err(error) => {
                let mut reply = json!({"id":id,"error":error.to_string()});
                if let Some(diagnostic) =
                    error.downcast_ref::<vm_control::native_input::RejectionDiagnostic>()
                {
                    reply["native_input_diagnostic"] = serde_json::to_value(diagnostic)?;
                }
                Refused::mark(&mut reply, &error);
                reply
            }
        })?;
    }
    let _ = input.cleanup(&mut qmp);
    drop(capture);
    drop(broker);
    // EOF/signal are supervisor loss, not a Guest normal shutdown promise.
    let _ = qmp.call("quit");
    let deadline = Instant::now() + Duration::from_secs(2);
    while child.0.try_wait()?.is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    drop(child);
    drop(disk_guard);
    if execution_shutdown.active() {
        bail!(
            "normal shutdown interrupted by supervisor loss or forced stop; completion unconfirmed"
        )
    }
    emit(
        json!({"event":"state","state":"stopped","reason":"supervisor_disconnected_or_stop","vm_id":config.id,"qmp_run_state":status_poll.summary()}),
    )?;
    Ok(false)
}
/// How long the worker loop may block on its request channel before re-observing what it can
/// only poll: QEMU and console broker exit, the stop signal, supervisor pipe loss, a fenced
/// execution transport, delivered QMP events, and the 500 ms guardian and status timers. With a result pending
/// (execution, Guest request, screenshot), held input awaiting timed release, or a normal
/// shutdown in progress, the original 50 ms step is kept. Otherwise nothing is in flight and
/// those observations may be up to 100 ms later than before; all have multi-second deadlines.
/// One step of the Windows balloon policy (`vm_control::qemu_balloon`). Its QMP calls fail only for this step:
/// before the Guest's driver loads the balloon is inactive, and without its service there are no statistics.
fn balloon_step(
    qmp: &mut Qmp,
    policy: &mut vm_control::qemu_balloon::Policy,
    polling: &mut bool,
    running: bool,
) {
    use vm_control::qemu_balloon::{guest_stats, DEVICE_PATH, INTERVAL};
    let stats = running
        .then(|| {
            // Once: setting it again restarts QEMU's timer.
            if !*polling {
                qmp.arguments(
                    "qom-set",
                    json!({"path":DEVICE_PATH,"property":"guest-stats-polling-interval","value":INTERVAL.as_secs()}),
                )
                .ok()?;
                *polling = true;
            }
            qmp.arguments(
                "qom-get",
                json!({"path":DEVICE_PATH,"property":"guest-stats"}),
            )
            .ok()
        })
        .flatten()
        .as_ref()
        .and_then(guest_stats);
    let Some(actual) = qmp
        .call("query-balloon")
        .ok()
        .and_then(|b| b["actual"].as_u64())
    else {
        policy.decide(Instant::now(), false, 0, None);
        return;
    };
    if let Some(target) = policy.decide(Instant::now(), running, actual, stats) {
        let _ = qmp.arguments("balloon", json!({"value":target}));
    }
}
/// Why QEMU exited, in the worker's last state: `guest-shutdown` only when
/// the Guest powered itself off, which an unattended install waits for.
fn exit_reason(success: bool, host_powerdown: bool) -> Value {
    match (success, host_powerdown) {
        (false, _) => Value::Null,
        (true, false) => json!("guest-shutdown"),
        (true, true) => json!("host-powerdown"),
    }
}
fn request_wait(in_flight: bool) -> Duration {
    Duration::from_millis(if in_flight { 50 } else { 150 })
}
fn request_powerdown(qmp: &mut Qmp, input: &mut InputState, retire_input: bool) -> Result<Value> {
    if retire_input {
        input
            .cleanup(qmp)
            .context("shutdown input cleanup unconfirmed")?;
    }
    qmp.call("system_powerdown")
}
/// A guest powering down drops serial execution; the deadline bounds the wait.
#[derive(Default)]
struct ExecutionShutdown {
    deadline: Option<Instant>,
}
impl ExecutionShutdown {
    fn begin(&mut self, now: Instant) {
        self.deadline.get_or_insert(now + Duration::from_secs(60));
    }
    fn active(&self) -> bool {
        self.deadline.is_some()
    }
    fn check(&self, now: Instant) -> Result<()> {
        if self.deadline.is_some_and(|d| now >= d) {
            bail!("normal shutdown deadline exceeded; completion unconfirmed")
        }
        Ok(())
    }
}
fn main() {
    let early = std::env::args_os().skip(1).collect::<Vec<_>>();
    if early.first().is_some_and(|v| v == "--supervise-qemu") {
        if let Err(error) = guardian::run(&early[1..]) {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    match run() {
        Ok(false) => {}
        // Same PID, arguments and pipe to the daemon; every resource of the
        // previous incarnation was dropped when run() returned.
        Ok(true) => {
            use std::os::unix::process::CommandExt;
            let error = Command::new(std::env::current_exe().unwrap_or_else(|_| "vm-qemu".into()))
                .args(&early)
                .exec();
            let _ = emit(
                json!({"event":"state","state":"error","error":format!("restart after guest reboot failed: {error}")}),
            );
            std::process::exit(1);
        }
        Err(error) => {
            let _ = emit(json!({"event":"state","state":"error","error":error.to_string()}));
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    fn broker_reply(reply: Value, kind: u8) -> Result<Value> {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let body = serde_json::to_vec(&reply).unwrap();
        let mut header = [0u8; 13];
        header[..4].copy_from_slice(&(body.len() as u32).to_be_bytes());
        header[4] = kind;
        writer.write_all(&header).unwrap();
        writer.write_all(&body).unwrap();
        drop(writer);
        read_console_screenshot(
            &mut reader,
            "expected",
            Instant::now() + Duration::from_secs(1),
        )
    }
    #[test]
    fn only_a_guest_power_off_is_a_guest_shutdown() {
        assert_eq!(exit_reason(true, false), "guest-shutdown");
        assert_eq!(exit_reason(true, true), "host-powerdown");
        assert!(exit_reason(false, false).is_null());
    }
    #[test]
    fn idle_request_wait_is_longer_but_in_flight_work_keeps_the_original_step() {
        assert_eq!(request_wait(true), Duration::from_millis(50));
        let idle = request_wait(false);
        assert!(idle > request_wait(true));
        // At most 100 ms later than before, and well inside the guardian's 500 ms cadence
        // and 5 s challenge expiry.
        assert!(idle <= Duration::from_millis(150));
        assert!(idle * 2 < Duration::from_millis(500));
        assert!(Duration::from_millis(500) + idle < guardian::ACTIVE_TIMEOUT / 2);
    }
    #[test]
    fn startup_proof_is_published_before_cont_even_when_qmp_closes_after_ack() {
        let (client, mut peer) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let published = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = published.clone();
        let server = thread::spawn(move || {
            let mut reader = BufReader::new(peer.try_clone().unwrap());
            let request: Value =
                serde_json::from_slice(&read_frame(&mut reader).unwrap().unwrap()).unwrap();
            assert_eq!(request["execute"], "cont");
            assert!(observed.load(Ordering::Acquire));
            writeln!(peer, "{}", json!({"id":request["id"],"return":{}})).unwrap();
            // No later query-status is needed to establish the launch identity.
        });
        let mut qmp = Qmp {
            reader: BufReader::new(client.try_clone().unwrap()),
            stream: client,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
            guest_reset: false,
            run_state_event: false,
            shutdown_event: false,
        };
        let mut state = json!({"state":"paused","disk_provenance":{"device":7,"inode":9,"qmp_fdsets_verified_at_start":true}});
        let mut events = Vec::new();
        publish_startup_then_resume(&mut qmp, &mut state, true, |event| {
            assert_eq!(event["disk_provenance"]["inode"], 9);
            published.store(true, Ordering::Release);
            events.push(event);
            Ok(())
        })
        .unwrap();
        server.join().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["state"], "starting");
        assert_eq!(events[1]["state"], "running");
    }
    #[test]
    fn failed_startup_publication_never_resumes_guest() {
        let (client, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut qmp = Qmp {
            reader: BufReader::new(client.try_clone().unwrap()),
            stream: client,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
            guest_reset: false,
            run_state_event: false,
            shutdown_event: false,
        };
        assert!(publish_startup_then_resume(
            &mut qmp,
            &mut json!({"state":"paused"}),
            true,
            |_| bail!("injected stdout failure")
        )
        .is_err());
        let mut bytes = [0; 1];
        assert_eq!(
            peer.read(&mut bytes).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    #[test]
    fn runtime_v2_window_status_requires_json_booleans_not_numeric_flags() {
        let status = json!({"native_window":true,"native_window_protocol":2,"native_input_protocol":1,
            "native_input_supported":true,"native_input_available":false,"input_enabled":false,"display_only":true,
            "window_open":true,"display_instance":"a".repeat(32),"native_input_state":"connecting",
            "native_input_ready":false,"native_release_confirmed":true,"native_input_epoch":null});
        for field in [
            None,
            Some("native_input_available"),
            Some("input_enabled"),
            Some("display_only"),
        ] {
            let mut status = status.clone();
            if let Some(field) = field {
                status[field] = json!(if field == "display_only" { 1 } else { 0 });
            }
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("console.sock");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let peer = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request: Value = serde_json::from_slice(
                    &read_frame(&mut BufReader::new(socket.try_clone().unwrap()))
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(
                    request,
                    json!({"request_id":"native-window","method":"window_status"})
                );
                let body =
                    serde_json::to_vec(&json!({"request_id":"native-window","result":status}))
                        .unwrap();
                socket
                    .write_all(&(body.len() as u32).to_be_bytes())
                    .unwrap();
                socket.write_all(&[1]).unwrap();
                socket.write_all(&[0; 8]).unwrap();
                socket.write_all(&body).unwrap();
            });
            assert_eq!(
                console_window(&path, "window_status").is_ok(),
                field.is_none(),
                "field {field:?}"
            );
            peer.join().unwrap();
        }
    }
    #[test]
    fn window_rpc_requires_exact_epoch_identity_and_display_only_ack() {
        for (epoch, ident, opened, enabled, okay) in [
            (0, "native-window", true, false, true),
            (1, "native-window", true, false, false),
            (0, "other-request", true, false, false),
            (0, "native-window", false, false, false),
            (0, "native-window", true, true, false),
        ] {
            let directory = tempfile::Builder::new()
                .prefix("window-")
                .tempdir_in("/tmp")
                .unwrap();
            let path = directory.path().join("console.sock");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let peer = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request: Value = serde_json::from_slice(
                    &read_frame(&mut BufReader::new(socket.try_clone().unwrap()))
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(
                    request,
                    json!({"request_id":"native-window","method":"show_window"})
                );
                let body=serde_json::to_vec(&json!({"request_id":ident,"result":{"native_window":true,"native_window_protocol":1,"display_only":true,"input_enabled":enabled,"window_open":opened}})).unwrap();
                let mut header = [0u8; 13];
                header[..4].copy_from_slice(&(body.len() as u32).to_be_bytes());
                header[4] = 1;
                header[5..].copy_from_slice(&(epoch as u64).to_be_bytes());
                socket.write_all(&header).unwrap();
                socket.write_all(&body).unwrap();
            });
            assert_eq!(console_window(&path, "show_window").is_ok(), okay);
            peer.join().unwrap();
        }
    }
    #[test]
    fn qmp_process_cleanup_releases_held_state_and_retries_failed_release() {
        // Actual child process + Unix QMP framing; synthetic QMP, not a Guest.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("qmp.sock");
        let program = r#"
import socket,json,sys
s=socket.socket(socket.AF_UNIX);s.bind(sys.argv[1]);s.listen(1);s.settimeout(5)
c,_=s.accept();c.settimeout(5);f=c.makefile('rwb',buffering=0)
def send(v): f.write(json.dumps(v).encode()+b'\n')
send({'QMP':{}})
r=json.loads(f.readline());assert r['execute']=='qmp_capabilities';send({'id':r['id'],'return':{}})
for n,down in enumerate([True,False,False,True,False],1):
 r=json.loads(f.readline());assert r['execute']=='input-send-event'
 assert r['arguments']['events']==[{'type':'btn','data':{'button':'left','down':down}}]
 send({'id':r['id'],'error':{'desc':'injected release failure'}} if n==2 else {'id':r['id'],'return':{}})
f.close();c.close();s.close()
"#;
        struct Owned(std::process::Child);
        impl Drop for Owned {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Owned(
            Command::new("python3")
                .arg("-c")
                .arg(program)
                .arg(&path)
                .spawn()
                .unwrap(),
        );
        let mut qmp = Qmp::connect(&path, &mut child.0).unwrap();
        let console = temp.path().join("console.sock");
        let listener = std::os::unix::net::UnixListener::bind(&console).unwrap();
        listener.set_nonblocking(true).unwrap();
        let console_thread = thread::spawn(move || {
            for _ in 0..2 {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => panic!("console fixture accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let request: Value = serde_json::from_slice(
                    &read_frame(&mut BufReader::new(stream.try_clone().unwrap()))
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                let body = serde_json::to_vec(&json!({"request_id":request["request_id"],"result":{"capture_connected":true,"frame_available":true,"source_generation":7,"width":1280,"height":800}})).unwrap();
                stream
                    .write_all(&(body.len() as u32).to_be_bytes())
                    .unwrap();
                stream.write_all(&[1]).unwrap();
                stream.write_all(&0u64.to_be_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        let anchor = json!({"frame_id":"00000000-0000-0000-0000-000000000001","width":1280,"height":800,"source_generation":7});
        let mut frames = vm_control::input::Frames::default();
        frames.record(&anchor, Instant::now());
        let mut args = anchor.clone();
        args["operation_id"] = json!("a".repeat(32));
        args["begin"] = json!(true);
        args["events"] = json!([vm_control::input::button("left", true)]);
        let mut state = InputState::default();
        assert!(state.step(&args, &frames, &console, &mut qmp).is_ok());
        assert!(state.step(&args, &frames, &console, &mut qmp).is_err());
        assert!(state.cleanup_operation(&args, &mut qmp).is_err());
        assert_eq!(state.held.len(), 1, "a failed release is retried");
        assert_eq!(
            state.cleanup_operation(&args, &mut qmp).unwrap()["released"],
            true
        );
        assert!(state.held.is_empty());
        assert_eq!(
            state.cleanup_operation(&args, &mut qmp).unwrap()["already_released"],
            true
        );
        args["operation_id"] = json!("b".repeat(32));
        assert!(state.step(&args, &frames, &console, &mut qmp).is_ok());
        assert_eq!(state.held.len(), 1);
        assert_eq!(
            state
                .cleanup_operation(&json!({"operation_id":"a".repeat(32)}), &mut qmp)
                .unwrap()["already_released"],
            true
        );
        assert_eq!(
            state.held.len(),
            1,
            "old operation cleanup cannot release newer buttons"
        );
        assert!(state
            .cleanup_operation(&json!({"operation_id":"c".repeat(32)}), &mut qmp)
            .is_err());
        assert_eq!(
            state.cleanup_operation(&args, &mut qmp).unwrap()["released"],
            true
        );
        assert!(state.held.is_empty());
        console_thread.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn failed_input_release_retains_state_for_retry() {
        let (stream, mut server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(server.try_clone().unwrap());
        let handle = thread::spawn(move || {
            for success in [false, true] {
                let request: Value =
                    serde_json::from_slice(&read_frame(&mut reader).unwrap().unwrap()).unwrap();
                assert_eq!(request["execute"], "input-send-event");
                assert_eq!(request["arguments"]["events"][0]["data"]["down"], false);
                let reply = if success {
                    json!({"id":request["id"],"return":{}})
                } else {
                    json!({"id":request["id"],"error":{"desc":"injected QMP failure"}})
                };
                serde_json::to_writer(&mut server, &reply).unwrap();
                server.write_all(b"\n").unwrap();
            }
        });
        let mut qmp = Qmp {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
            guest_reset: false,
            run_state_event: false,
            shutdown_event: false,
        };
        let mut state = InputState {
            operation: Some("a".repeat(32)),
            until: Some(Instant::now()),
            held: std::collections::BTreeMap::from([(
                "left".into(),
                vm_control::input::button("left", false),
            )]),
            ..Default::default()
        };
        assert!(state.cleanup(&mut qmp).is_err());
        assert_eq!(state.held.len(), 1);
        assert!(state.operation.is_some());
        state.cleanup(&mut qmp).unwrap();
        assert!(state.held.is_empty());
        assert!(state.operation.is_none());
        handle.join().unwrap();
    }
    #[test]
    fn broker_screenshot_preserves_frame_time_and_strips_internal_identity() {
        let result=broker_reply(json!({"request_id":"expected","result":{"mime_type":"image/png","byte_length":3,"data_base64":"YWJj","captured_monotonic_us":1234,"captured_at_us":4567,"source_generation":2,"source_sequence":4,"width":1280,"height":800,"internal_scanout_id":999}}),1).unwrap();
        assert_eq!(result["captured_monotonic_us"], 1234);
        assert_eq!(result["captured_at_us"], 4567);
        assert_eq!(result["source_sequence"], 4);
        assert!(result.get("internal_scanout_id").is_none());
        assert_eq!(result["frame_id"].as_str().unwrap().len(), 36);
    }
    #[test]
    fn broker_screenshot_rejects_unrelated_wrong_kind_oversized_and_expired_responses() {
        assert!(broker_reply(json!({"request_id":"wrong","result":{}}), 1).is_err());
        assert!(broker_reply(json!({"request_id":"expected","result":{}}), 2).is_err());
        assert!(broker_reply(
            json!({"request_id":"expected","error":"capture unavailable"}),
            1
        )
        .unwrap_err()
        .to_string()
        .contains("capture unavailable"));
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let mut header = [0u8; 13];
        header[..4]
            .copy_from_slice(&((vm_control::MAX_SCREENSHOT_RESPONSE + 1) as u32).to_be_bytes());
        header[4] = 1;
        writer.write_all(&header).unwrap();
        assert!(read_console_screenshot(
            &mut reader,
            "expected",
            Instant::now() + Duration::from_secs(1)
        )
        .unwrap_err()
        .to_string()
        .contains("oversized"));
        assert!(read_console_screenshot(
            &mut reader,
            "expected",
            Instant::now() - Duration::from_secs(1)
        )
        .unwrap_err()
        .to_string()
        .contains("deadline"));
    }
    #[test]
    fn logging_rejects_symlinks_hardlinks_and_nonprivate_files() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        fs::write(&original, b"preserved").unwrap();
        let link = dir.path().join("symlink");
        symlink(&original, &link).unwrap();
        assert!(log_pipe(std::io::Cursor::new(b"new"), Some(link)).is_err());
        assert!(log_pipe(std::io::Cursor::new(b"new"), Some(original.clone())).is_err());
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&original, fs::Permissions::from_mode(0o600)).unwrap();
        let hardlink = dir.path().join("hardlink");
        fs::hard_link(&original, &hardlink).unwrap();
        assert!(log_pipe(std::io::Cursor::new(b"new"), Some(hardlink)).is_err());
        assert_eq!(fs::read(&original).unwrap(), b"preserved");
    }
    #[test]
    fn log_bound_preserves_existing_bytes_and_drains_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serial");
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(b"original").unwrap();
        file.set_len(8 * 1024 * 1024 - 128).unwrap();
        drop(file);
        log_pipe(
            std::io::Cursor::new(vec![b'x'; 1024 * 1024]),
            Some(path.clone()),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while fs::metadata(&path).unwrap().len() != 8 * 1024 * 1024 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let contents = fs::read(path).unwrap();
        assert_eq!(contents.len(), 8 * 1024 * 1024);
        assert_eq!(&contents[..8], b"original");
        assert_eq!(&contents[contents.len() - 128..], &[b'x'; 128]);
    }
    #[test]
    fn execution_shutdown_never_extends_deadline() {
        let now = Instant::now();
        let mut state = ExecutionShutdown::default();
        assert!(!state.active());
        state.begin(now);
        assert!(state.active());
        state.check(now + Duration::from_secs(59)).unwrap();
        state.begin(now + Duration::from_secs(59));
        assert!(state
            .check(now + Duration::from_secs(60))
            .unwrap_err()
            .to_string()
            .contains("completion unconfirmed"));
    }
    #[test]
    fn execution_shutdown_qmp_waits_for_held_release_ack_before_power_button() {
        for fail_release in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("qmp.sock");
            let script = r#"
import socket,json,sys
s=socket.socket(socket.AF_UNIX);s.bind(sys.argv[1]);s.listen(1);s.settimeout(5)
c,_=s.accept();c.settimeout(5);f=c.makefile('rwb',buffering=0)
def send(v): f.write(json.dumps(v).encode()+b'\n')
send({'QMP':{}})
r=json.loads(f.readline());assert r['execute']=='qmp_capabilities';send({'id':r['id'],'return':{}})
r=json.loads(f.readline());assert r['execute']=='input-send-event'
assert r['arguments']['events']==[{'type':'btn','data':{'button':'left','down':False}}]
if sys.argv[2]=='true':
 send({'id':r['id'],'error':{'desc':'release rejected'}})
 assert f.readline()==b''
else:
 send({'id':r['id'],'return':{}})
 r=json.loads(f.readline());assert r['execute']=='system_powerdown';send({'id':r['id'],'return':{}})
f.close();c.close();s.close()
"#;
            let mut child = Command::new("python3")
                .arg("-c")
                .arg(script)
                .arg(&path)
                .arg(fail_release.to_string())
                .spawn()
                .unwrap();
            let mut qmp = Qmp::connect(&path, &mut child).unwrap();
            let mut input = InputState::default();
            input.held.insert(
                "btn:left".into(),
                json!({"type":"btn","data":{"button":"left","down":false}}),
            );
            let result = request_powerdown(&mut qmp, &mut input, true);
            assert_eq!(result.is_err(), fail_release);
            assert_eq!(input.held.is_empty(), !fail_release);
            if fail_release {
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("cleanup unconfirmed"));
            }
            drop(qmp);
            assert!(child.wait().unwrap().success());
        }
    }
}

#[cfg(test)]
#[path = "../qemu_status_events_tests.rs"]
mod status_events_tests;
/// Attribution counters of the run-state summary, against a scripted QMP peer in the style of
/// `qemu_status_events_tests.rs` (whose helpers are private to it).
#[cfg(test)]
mod status_counter_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    const MS: Duration = Duration::from_millis(1);
    const S: Duration = Duration::from_secs(1);
    struct Worker {
        qmp: Qmp,
        poll: StatusPoll,
        state: Value,
        published: usize,
        start: Instant,
        /// What the peer answers `query-status` with; `null` answers a QMP error.
        status: Arc<Mutex<Value>>,
        before_reply: Arc<Mutex<Vec<u8>>>,
        peer: UnixStream,
    }
    fn record(name: &str) -> Vec<u8> {
        format!(
            "{}\n",
            json!({"event":name,"data":{"secret":"guest-content"}})
        )
        .into_bytes()
    }
    impl Worker {
        fn pass(&mut self, offset: Duration) {
            let published = &mut self.published;
            observe_status(
                &mut self.qmp,
                &mut self.poll,
                &mut self.state,
                self.start + offset,
                true,
                |_| {
                    *published += 1;
                    Ok(())
                },
            )
            .unwrap();
        }
        fn set(&self, running: bool, status: &str) {
            *self.status.lock().unwrap() = json!({"running":running,"status":status});
        }
        fn event(&mut self, name: &str) {
            self.peer.write_all(&record(name)).unwrap();
        }
        fn changes(&self) -> (u64, u64, u64, u64) {
            let seen = self.poll.seen;
            (
                seen.event_changes,
                seen.command_changes,
                seen.cadence_changes,
                seen.safety_changes,
            )
        }
    }
    /// Past the active window: 11 queries, none of them a safety poll, nothing changed.
    fn settled() -> Worker {
        let (client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let status = Arc::new(Mutex::new(json!({"running":true,"status":"running"})));
        let before_reply = Arc::new(Mutex::new(Vec::new()));
        let (answers, interleaved) = (status.clone(), before_reply.clone());
        let peer = server.try_clone().unwrap();
        thread::spawn(move || {
            let mut reader = BufReader::new(server.try_clone().unwrap());
            let mut writer = server;
            while let Ok(Some(line)) = read_frame(&mut reader) {
                let request: Value = serde_json::from_slice(&line).unwrap();
                assert_eq!(request["execute"], "query-status");
                let early = std::mem::take(&mut *interleaved.lock().unwrap());
                writer.write_all(&early).unwrap();
                let status = answers.lock().unwrap().clone();
                let reply = if status.is_null() {
                    json!({"id":request["id"],"error":{"class":"GenericError","desc":"scripted"}})
                } else {
                    json!({"id":request["id"],"return":status})
                };
                writer.write_all(format!("{reply}\n").as_bytes()).unwrap();
            }
        });
        let start = Instant::now();
        let mut worker = Worker {
            qmp: Qmp {
                reader: BufReader::new(client.try_clone().unwrap()),
                stream: client,
                next: 1,
                poisoned: false,
                reply_prefix: Vec::new(),
                run_state_event: false,
                shutdown_event: false,
                guest_reset: false,
            },
            poll: StatusPoll::new(start),
            state: json!({"event":"state","state":"running","qmp_status":"running","vm_id":"test"}),
            published: 0,
            start,
            status,
            before_reply,
            peer,
        };
        assert_eq!(worker.poll.asked, Asked::Command);
        for second in 0..=10 {
            worker.pass(S * second);
        }
        assert_eq!(
            worker.poll.seen,
            Observed {
                queries: 11,
                ..Observed::default()
            }
        );
        assert_eq!(worker.poll.asked, Asked::Safety);
        worker
    }

    #[test]
    fn change_after_an_event_is_counted_as_event_and_not_as_a_poll() {
        let mut worker = settled();
        worker.set(false, "paused");
        worker.event("STOP");
        worker.pass(S * 10 + MS);
        worker.set(true, "running");
        worker.event("RESUME");
        worker.pass(S * 10 + MS * 2);
        assert_eq!(worker.published, 2);
        assert_eq!(worker.changes(), (2, 0, 0, 0));
        // An event that changes nothing is a query and no change; an unlisted event is not even a query.
        worker.event("RESUME");
        worker.event("RTC_CHANGE");
        worker.event("POWERDOWN");
        worker.pass(S * 10 + MS * 3);
        assert_eq!(worker.changes(), (2, 0, 0, 0));
        let seen = worker.poll.seen;
        assert_eq!(
            (seen.queries, seen.safety_polls, seen.query_errors),
            (14, 0, 0)
        );
        let summary = worker.poll.summary();
        assert_eq!(summary["events"], json!({"STOP":1,"RESUME":2}));
        assert_eq!(summary["events_unlisted"], 2);
        assert_eq!(
            summary["changes"],
            json!({"event":2,"command":0,"cadence_poll":0,"safety_poll":0})
        );
    }

    #[test]
    fn change_found_by_the_safety_poll_is_counted_as_a_missed_event() {
        let mut worker = settled();
        // Steady state with nothing happening: one safety poll, no change.
        worker.pass(S * 40);
        assert_eq!(worker.poll.seen.safety_polls, 1);
        assert_eq!(worker.changes(), (0, 0, 0, 0));
        worker.set(false, "paused");
        worker.pass(S * 70 - MS);
        assert_eq!(worker.poll.seen.safety_polls, 1);
        worker.pass(S * 70);
        assert_eq!(worker.published, 1);
        assert_eq!(worker.poll.seen.safety_polls, 2);
        assert_eq!(worker.changes(), (0, 0, 0, 1));
        // The silent change back is found by the 1 s timer of the restarted active window: also unannounced, and
        // counted apart from the safety poll.
        worker.set(true, "running");
        worker.pass(S * 71);
        assert_eq!(worker.published, 2);
        assert_eq!(worker.changes(), (0, 0, 1, 1));
        assert_eq!(worker.poll.seen.safety_polls, 2);
        assert_eq!(worker.poll.seen.queries, 14);
    }

    #[test]
    fn event_read_while_the_safety_poll_waits_for_its_reply_is_not_a_missed_event() {
        let mut worker = settled();
        worker.set(false, "paused");
        *worker.before_reply.lock().unwrap() = record("STOP");
        worker.pass(S * 40);
        assert_eq!(worker.published, 1);
        assert_eq!(worker.poll.seen.safety_polls, 1);
        assert_eq!(worker.changes(), (1, 0, 0, 0));
        // The event still asks for its own query on the next pass, which changes nothing.
        worker.pass(S * 40 + MS);
        assert_eq!(worker.poll.seen.queries, 13);
        assert_eq!(worker.changes(), (1, 0, 0, 0));
    }

    #[test]
    fn failed_queries_are_errors_and_neither_they_nor_the_recovery_are_changes() {
        let mut worker = settled();
        *worker.status.lock().unwrap() = Value::Null;
        worker.pass(S * 40);
        worker.pass(S * 41);
        assert_eq!(worker.published, 1);
        assert_eq!(worker.state["state"], "unavailable");
        let seen = worker.poll.seen;
        assert_eq!(
            (seen.safety_polls, seen.safety_errors, seen.query_errors),
            (1, 1, 2)
        );
        worker.set(true, "running");
        worker.pass(S * 42);
        assert_eq!(worker.published, 2);
        assert_eq!(worker.changes(), (0, 0, 0, 0));
        assert_eq!(worker.poll.seen.query_errors, 2);
    }

    #[test]
    fn command_and_shutdown_attribution() {
        let mut worker = settled();
        // What the loop does around `system_powerdown`; a change the next query finds belongs to the command.
        worker.poll.disturbed(worker.start + S * 12);
        worker.set(false, "paused");
        worker.pass(S * 12);
        assert_eq!(worker.changes(), (0, 1, 0, 0));
        worker.event("SHUTDOWN");
        worker.set(false, "shutdown");
        worker.pass(S * 12 + MS);
        worker.pass(S * 13 + MS);
        assert_eq!(worker.changes(), (1, 1, 0, 0));
        assert_eq!(worker.poll.summary()["events"], json!({"SHUTDOWN":1}));
    }

    #[test]
    fn log_lines_are_single_line_json_with_names_and_counts_only() {
        let mut worker = settled();
        worker.set(false, "paused");
        worker.event("STOP");
        worker.pass(S * 10 + MS);
        let line = worker.poll.summary_line();
        assert_eq!(
            line,
            r#"{"changes":{"cadence_poll":0,"command":0,"event":1,"safety_poll":0},"event":"qmp_run_state_summary","events":{"STOP":1},"events_unlisted":0,"queries":12,"query_errors":0,"safety_poll_errors":0,"safety_polls":0}"#
        );
        assert!(!line.contains('\n') && !line.contains("guest-content"));
        assert_eq!(
            poll_change_line("safety_poll", &json!("running"), &json!("paused")),
            r#"{"event":"qmp_run_state_poll_change","from":"running","source":"safety_poll","to":"paused"}"#
        );
    }
}
#[cfg(test)]
#[path = "../screenshot_test_support.rs"]
mod screenshot_test_support;
#[cfg(test)]
mod screenshot_response_tests {
    use super::*;
    #[test]
    fn actual_console_socket_preserves_large_png_and_rejects_other_reply_bound() {
        let value = screenshot_test_support::value();
        for screenshot in [true, false] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            let payload = serde_json::to_vec(&json!({"request_id":"test","result":value})).unwrap();
            assert!(payload.len() > vm_control::MAX_MESSAGE);
            let writer = std::thread::spawn(move || {
                let mut h = [0u8; 13];
                h[..4].copy_from_slice(&(payload.len() as u32).to_be_bytes());
                h[4] = 1;
                let _ = server.write_all(&h);
                let _ = server.write_all(&payload);
            });
            let deadline = Instant::now() + Duration::from_secs(3);
            if screenshot {
                let reply = read_console_screenshot(&mut client, "test", deadline).unwrap();
                assert_eq!(reply["data_base64"], value["data_base64"]);
            } else {
                assert!(read_console_reply(&mut client, "test", deadline).is_err());
            }
            drop(client);
            writer.join().unwrap();
        }
    }
}

#[cfg(test)]
mod venus_qmp_admission_tests {
    use super::*;
    #[test]
    fn actual_qmp_socket_checks_properties_before_cont_and_denies_mismatch() {
        for wrong_blob in [false, true] {
            let (client, mut server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let peer = std::thread::spawn(move || {
                let mut reader = BufReader::new(server.try_clone().unwrap());
                let mut methods = Vec::new();
                while let Some(line) = read_frame(&mut reader).unwrap() {
                    let request: Value = serde_json::from_slice(&line).unwrap();
                    let method = request["execute"].as_str().unwrap();
                    methods.push(method.to_owned());
                    let result = match method {
                        "qom-get" => {
                            assert_eq!(
                                request["arguments"]["path"],
                                "/machine/peripheral/veynode-gpu"
                            );
                            match request["arguments"]["property"].as_str().unwrap() {
                                "venus" => json!(true),
                                "blob" => json!(!wrong_blob),
                                "hostmem" => json!(268435456u64),
                                other => panic!("unexpected property {other}"),
                            }
                        }
                        "query-status" => json!({"running":false,"status":"prelaunch"}),
                        "cont" => json!({}),
                        other => panic!("unexpected QMP method {other}"),
                    };
                    serde_json::to_writer(
                        &mut server,
                        &json!({"id":request["id"],"return":result}),
                    )
                    .unwrap();
                    server.write_all(b"\n").unwrap();
                }
                methods
            });
            let mut qmp = Qmp {
                reader: BufReader::new(client.try_clone().unwrap()),
                stream: client,
                next: 1,
                poisoned: false,
                reply_prefix: Vec::new(),
                guest_reset: false,
                run_state_event: false,
                shutdown_event: false,
            };
            let admitted = vm_control::qemu_graphics::verify_gpu_properties(|method, args| {
                qmp.arguments(method, args)
            });
            assert_eq!(admitted.is_ok(), !wrong_blob);
            if admitted.is_ok() {
                let mut state = json!({"state":"paused","qmp_status":"prelaunch"});
                let mut states = Vec::new();
                publish_startup_then_resume(&mut qmp, &mut state, true, |value| {
                    states.push(value["state"].clone());
                    Ok(())
                })
                .unwrap();
                assert_eq!(states, vec![json!("starting"), json!("running")]);
            }
            drop(qmp);
            let methods = peer.join().unwrap();
            if wrong_blob {
                assert!(!methods.iter().any(|m| m == "cont"));
            } else {
                assert_eq!(methods.iter().position(|m| m == "cont"), Some(4));
            }
        }
    }
}

#[cfg(test)]
mod capture_wake_tests {
    use super::*;
    /// A console broker that answers one `screenshot` after `delay` and reports when its reply was written.
    fn scripted_broker(
        delay: Duration,
        result: Value,
    ) -> (tempfile::TempDir, PathBuf, mpsc::Receiver<Instant>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let (tx, written) = mpsc::channel();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(socket.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "screenshot");
            let body =
                serde_json::to_vec(&json!({"request_id":request["request_id"],"result":result}))
                    .unwrap();
            let mut record = (body.len() as u32).to_be_bytes().to_vec();
            record.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 0]);
            record.extend_from_slice(&body);
            thread::sleep(delay);
            socket.write_all(&record).unwrap();
            let _ = tx.send(Instant::now());
        });
        (dir, path, written)
    }
    #[test]
    fn finished_broker_capture_wakes_the_loop_at_once_and_exactly_once() {
        let picture = screenshot_test_support::value();
        // Each delay ends 30 to 40 ms before the next 50 ms step of the loop's in-flight wait.
        for delay in [10, 20, 60, 70] {
            let (_dir, path, written) =
                scripted_broker(Duration::from_millis(delay), picture.clone());
            let (wake, rx) = mpsc::sync_channel::<Wake>(16);
            let mut capture = Capture::broker(&path, json!(7), wake.clone()).unwrap();
            let (mut steps, noticed) = (
                0,
                loop {
                    match rx.recv_timeout(request_wait(true)) {
                        Ok(Wake::Observe) => break Instant::now(),
                        Ok(Wake::Request(_)) => panic!("no request was sent"),
                        Err(mpsc::RecvTimeoutError::Timeout) => assert!(capture.poll().is_none()),
                        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("loop keeps a sender"),
                    }
                },
            );
            let lag = noticed.saturating_duration_since(written.recv().unwrap());
            assert!(
                lag < Duration::from_millis(15),
                "woken {lag:?} after a {delay} ms reply"
            );
            let result = capture
                .poll()
                .expect("result is ready when the wake arrives")
                .unwrap();
            assert!(result["data_base64"] == picture["data_base64"]);
            let timing = capture.timing.take().expect("broker capture is timed");
            assert!(timing.wait_us >= delay * 1000 && timing.bytes > 1024 * 1024);
            let event = timing.event(&json!(7), noticed);
            assert!(event.get("id").is_none() && event["request"] == 7);
            assert!(event
                .as_object()
                .unwrap()
                .iter()
                .all(|(key, value)| key == "event" || value.is_u64()));
            // Nothing in flight any more: no second wake, and the idle wait runs its full length.
            let idle = Instant::now();
            while idle.elapsed() < Duration::from_millis(450) {
                assert!(matches!(
                    rx.recv_timeout(request_wait(false)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ));
                steps += 1;
            }
            assert_eq!(steps, 3, "150 ms idle step");
        }
    }
    #[test]
    fn failed_broker_capture_wakes_the_loop_too_and_carries_no_timing() {
        let mut oversized = screenshot_test_support::value();
        oversized["byte_length"] = json!(vm_control::MAX_SCREENSHOT_BYTES + 1);
        let (_dir, path, _written) = scripted_broker(Duration::from_millis(10), oversized);
        let (wake, rx) = mpsc::sync_channel::<Wake>(16);
        let mut capture = Capture::broker(&path, json!(8), wake).unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)),
            Ok(Wake::Observe)
        ));
        let error = capture.poll().unwrap().unwrap_err();
        assert_eq!(error.to_string(), "invalid console screenshot metadata");
        assert!(capture.timing.is_none());
    }
    #[test]
    fn full_request_queue_does_not_block_the_capture_reader() {
        let (_dir, path, written) =
            scripted_broker(Duration::from_millis(5), screenshot_test_support::value());
        let (wake, rx) = mpsc::sync_channel::<Wake>(1);
        wake.send(Wake::Request(Ok(json!({"id":1,"method":"status"}))))
            .unwrap();
        let mut capture = Capture::broker(&path, json!(9), wake.clone()).unwrap();
        written.recv().unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        let result = loop {
            if let Some(result) = capture.poll() {
                break result;
            }
            assert!(Instant::now() < until, "reader blocked on a full queue");
            thread::sleep(Duration::from_millis(1));
        };
        assert!(result.is_ok());
        assert!(matches!(rx.try_recv(), Ok(Wake::Request(_))));
        assert!(rx.try_recv().is_err());
    }
}
#[cfg(test)]
mod scaled_screenshot_tests {
    use super::*;
    use base64::Engine;
    use std::os::unix::net::UnixListener;

    const ASK: Scaled = Scaled {
        max_width: 1280,
        max_height: 720,
        quality: 50,
    };
    /// What `Capture::broker_scaled` writes to the console broker for request number 7.
    fn written(scaled: Option<Scaled>) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (wake, _woken) = mpsc::sync_channel(1);
        let capture = Capture::broker_scaled(&path, json!(7), wake, scaled).unwrap();
        let (peer, _) = listener.accept().unwrap();
        let mut line = Vec::new();
        BufReader::new(peer).read_until(b'\n', &mut line).unwrap();
        drop(capture);
        line
    }
    /// SOI, one frame header, EOI: enough for the dimension check. Not a picture.
    fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8, 0xff, 0xc0, 0, 17, 8];
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1, 0xff, 0xd9]);
        out
    }
    fn scaled_result(frame: (u16, u16)) -> Value {
        let bytes = jpeg(frame.0, frame.1);
        json!({"mime_type":"image/jpeg","byte_length":bytes.len(),
            "data_base64":base64::engine::general_purpose::STANDARD.encode(&bytes),
            "captured_monotonic_us":1234,"source_generation":2,"source_sequence":4,"width":2560,"height":1440,
            "frame_width":1280,"frame_height":720,"scale_divisor":2,"frame_quality":50,"internal_scanout_id":9})
    }
    fn read(reply: Value, scaled: Option<Scaled>) -> Result<Value> {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let body = serde_json::to_vec(&reply).unwrap();
        let mut header = [0u8; 13];
        header[..4].copy_from_slice(&(body.len() as u32).to_be_bytes());
        header[4] = 1;
        writer.write_all(&header).unwrap();
        writer.write_all(&body).unwrap();
        drop(writer);
        let deadline = Instant::now() + Duration::from_secs(1);
        read_console_screenshot_scaled(&mut reader, "expected", deadline, Instant::now(), scaled)
            .map(|(value, _)| value)
    }

    #[test]
    fn broker_request_without_the_scaled_form_is_byte_for_byte_what_it_was() {
        assert_eq!(
            written(None),
            b"{\"method\":\"screenshot\",\"request_id\":\"screenshot-7\"}\n"
        );
        assert_eq!(
            written(Some(ASK)),
            b"{\"format\":\"jpeg\",\"max_height\":720,\"max_width\":1280,\"method\":\"screenshot\",\"quality\":50,\"request_id\":\"screenshot-7\"}\n"
        );
    }
    #[test]
    fn scaled_reply_is_accepted_only_when_asked_for_and_only_when_it_agrees_with_itself() {
        let good = json!({"request_id":"expected","result":scaled_result((1280, 720))});
        let result = read(good.clone(), Some(ASK)).unwrap();
        assert_eq!(result["mime_type"], "image/jpeg");
        assert_eq!(
            (&result["width"], &result["frame_width"]),
            (&json!(2560), &json!(1280))
        );
        assert_eq!(result["frame_id"].as_str().unwrap().len(), 36);
        assert!(result.get("internal_scanout_id").is_none());
        // The same JPEG to a request that did not ask for it: refused exactly as any non-PNG always was.
        assert_eq!(
            read(good, None).unwrap_err().to_string(),
            "invalid console screenshot metadata"
        );
        // Header dimensions that are not the declared frame, and a frame larger than the bound asked for.
        let wrong = json!({"request_id":"expected","result":scaled_result((1280, 722))});
        let error = format!("{:#}", read(wrong, Some(ASK)).unwrap_err());
        assert!(
            error.contains("invalid console scaled screenshot")
                && error.contains("real dimensions"),
            "{error}"
        );
        let small = Scaled {
            max_width: 640,
            ..ASK
        };
        let over = json!({"request_id":"expected","result":scaled_result((1280, 720))});
        assert!(format!("{:#}", read(over, Some(small)).unwrap_err())
            .contains("exceeds the requested size"));
        // A PNG answer to the scaled form (a helper without it) is held to the PNG checks and forwarded.
        let png = json!({"request_id":"expected","result":{"mime_type":"image/png","byte_length":3,"data_base64":"YWJj","captured_monotonic_us":1,"width":1280,"height":800}});
        assert_eq!(read(png, Some(ASK)).unwrap()["mime_type"], "image/png");
    }
    #[test]
    fn broker_refusal_word_travels_beside_the_unchanged_text() {
        for refusal in Refusal::ALL {
            let reply = json!({"request_id":"expected","error":"native operation failed: x","screenshot_refusal":refusal.name()});
            let error = read(reply, Some(ASK)).unwrap_err();
            assert_eq!(Refused::of(&error), Some(refusal));
            assert_eq!(
                error.to_string(),
                "console operation failed: \"native operation failed: x\""
            );
            assert_eq!(
                refusal_reply(&json!(7), &error),
                json!({"id":7,"error":"console operation failed: \"native operation failed: x\"","screenshot_refusal":refusal.name()})
            );
        }
        // No word, or anything but one of the fixed words: an ordinary error with the same text and one field.
        for reply in [
            json!({"request_id":"expected","error":"native operation failed: x"}),
            json!({"request_id":"expected","error":"native operation failed: x","screenshot_refusal":"GUEST_TEXT"}),
        ] {
            let error = read(reply, Some(ASK)).unwrap_err();
            assert_eq!(Refused::of(&error), None);
            assert_eq!(
                refusal_reply(&json!(7), &error),
                json!({"id":7,"error":"console operation failed: \"native operation failed: x\""})
            );
        }
    }
}
