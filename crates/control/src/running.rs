//! A running VM's worker process and its runtime state.
use super::*;

pub(crate) struct GuestAsyncTimeout {
    pub(crate) maximum: u64,
    pub(crate) generation: Option<u64>,
}
impl Default for GuestAsyncTimeout {
    fn default() -> Self {
        Self {
            maximum: SYNC_EXEC_MAX_TIMEOUT_MS,
            generation: Some(0),
        }
    }
}
impl GuestAsyncTimeout {
    pub(crate) fn invalidate(&mut self) {
        self.maximum = SYNC_EXEC_MAX_TIMEOUT_MS;
        // Exhaustion leaves the policy unknown permanently, never reuses a
        // generation still held by a pending info completion.
        self.generation = self.generation.and_then(|value| value.checked_add(1));
    }
    pub(crate) fn observe_worker_state(&mut self, state: &Value) {
        // Ordinary running/paused updates do not renegotiate a no-reconnect
        // serial lifetime or invalidate a just-completed info observation.
        if matches!(state.as_str(), Some("stopped" | "error")) {
            self.invalidate();
        }
    }
}
pub(crate) struct Running {
    pub(crate) bridge: Arc<Mutex<bridge::State>>,
    pub(crate) child: Child,
    pub(crate) assigned_memory_mib: u64,
    pub(crate) expected_exit: bool,
    pub(crate) execution_quiescing: bool,
    // A new worker gets a new cell. Old info completions retain only the old
    // cell, so neither VM-id reuse nor a late reply can elevate its successor.
    pub(crate) guest_async_timeout: Arc<Mutex<GuestAsyncTimeout>>,
    pub(crate) exit_status: Option<std::process::ExitStatus>,
    pub(crate) disk_identity: Option<DiskIdentity>,
    pub(crate) stdin: ChildStdin,
    pub(crate) state: Arc<Mutex<Value>>,
    pub(crate) pending: Arc<Mutex<BTreeMap<u64, PendingReply>>>,
    pub(crate) next: u64,
}
// These values describe the original launch or its last Guest observation and remain
// historical facts after terminal/error events. This function only receives one Running's state;
// a replacement worker starts with a fresh state object and inherits nothing.
pub(crate) fn replace_runtime_state(current: &mut Value, mut next: Value) {
    // A Windows guest's reboot is a new QEMU under the same worker: how far an
    // unattended install is, for the host that watches it.
    let resets =
        current["guest_resets"].as_u64().unwrap_or(0) + u64::from(next["reason"] == "guest-reset");
    next["guest_resets"] = resets.into();
    for field in [
        "disk_provenance",
        "graphics_profile",
        "execution_guardian",
        "backend_pid",
        "vm_id",
        "serial_log",
        "stderr_log",
        "guest_metal_device_alert",
        "guest_metal_device_observation",
    ] {
        if let Some(value) = current.get(field) {
            next[field] = value.clone();
        }
    }
    *current = next;
}
// A missing or malformed graphics reply is not evidence that a previously lost
// device recovered. Keep the Guest envelope unchanged; this is only a worker-local
// health summary, and no query compiles a pipeline or proves application support.
pub(crate) fn update_guest_metal_health(state: &mut Value, info: &Value) {
    let graphics = &info["graphics"];
    let missing = graphics["status"] == "unavailable" && graphics["reason"] == "no_metal_device";
    let device = &graphics["device"];
    let device_name = device["name"]
        .as_str()
        .filter(|name| !name.trim().is_empty());
    let valid_registry = device["registry_id"].as_str().is_some_and(|value| {
        !value.is_empty()
            && value.bytes().all(|byte| byte.is_ascii_digit())
            && value.parse::<u64>().is_ok()
    });
    let reported = graphics["schema"] == 1
        && graphics["api"] == "metal"
        && graphics["status"] == "reported"
        && graphics["device_selection"] == "system-default"
        && graphics["evidence"] == "public-device-properties"
        && graphics["queried_at_unix_ms"].as_u64().is_some()
        && device_name.is_some()
        && valid_registry
        && info["metal_device"].as_str() == device_name;
    let mut observation = json!({
        "status":if missing { "unavailable" } else if reported { "reported" } else { "unknown" },
        "source":"guest_info",
        "scope":"last successful info reply for this worker; not polled; device presence does not validate rendering or performance",
    });
    if let Some(queried_at) = graphics["queried_at_unix_ms"].as_u64() {
        observation["queried_at_unix_ms"] = json!(queried_at);
    }
    if missing {
        observation["reason"] = json!("no_metal_device");
        let mut alert = json!({
            "reason":"no_metal_device",
            "source":"guest_info",
            "scope":"last explicit no-Metal-device observation for this worker; not polled; retained until a validated device report or a new worker",
            "operator_guidance":{
                "action":"save Guest work, request normal Guest shutdown, wait for the stopped state, start it again, then query vm_guest_info",
                "evidence":"normal Guest restarts recovered the device in the retained decision 0023 observations; the loss trigger remains unestablished",
                "decision":"docs/decisions/0023-macos-guest-metal-compute-limit.md",
                "automatic_recovery":false,
                "verification":"a reported device clears this loss alert; rendering, large compute pipelines and performance still require separate tests",
            },
        });
        if let Some(queried_at) = graphics["queried_at_unix_ms"].as_u64() {
            alert["queried_at_unix_ms"] = json!(queried_at);
        }
        state["guest_metal_device_alert"] = alert;
    } else if reported {
        state
            .as_object_mut()
            .unwrap()
            .remove("guest_metal_device_alert");
    } else {
        observation["reason"] = json!(if graphics.is_null() {
            "graphics_not_reported"
        } else {
            "invalid_or_unrecognized_graphics_report"
        });
    }
    state["guest_metal_device_observation"] = observation;
}
impl Running {
    // Keep the worker PID reserved until its private process group is killed.
    // try_wait would reap first and permit PID/PGID reuse before cleanup.
    pub(crate) fn poll_exit(&mut self) -> Result<Option<std::process::ExitStatus>> {
        if self.exit_status.is_some() {
            return Ok(self.exit_status);
        }
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        #[cfg(target_os = "macos")]
        let pid = info.si_pid;
        #[cfg(not(target_os = "macos"))]
        let pid = unsafe { info.si_pid() };
        // Darwin can report CLD_STOPPED even with WEXITED. Only terminal
        // child events justify group cleanup/reaping; a stopped VM is alive.
        if pid == 0
            || ![libc::CLD_EXITED, libc::CLD_KILLED, libc::CLD_DUMPED].contains(&info.si_code)
        {
            return Ok(None);
        }
        if unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH)
                && !only_waitable_leader(error.raw_os_error(), self.child.id())
            {
                return Err(error.into());
            }
        }
        self.exit_status = Some(self.child.wait()?);
        Ok(self.exit_status)
    }
    /// Publishes this worker's exit once it is known; repeated calls are no-ops.
    pub(crate) fn publish_exit(&self, id: &str, events: &events::Log) {
        if let Some(status) = self.exit_status {
            events.exited(id, self.child.id(), status, self.expected_exit);
        }
    }
    pub(crate) fn force_stop(&mut self) -> Result<()> {
        if self.poll_exit()?.is_none() {
            if unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            self.expected_exit = true;
            self.exit_status = Some(self.child.wait()?);
        }
        Ok(())
    }
}
// Darwin returns EPERM, not ESRCH, when killpg targets only an unreaped zombie.
// Accept that case only while waitid has reserved the leader and libproc reports
// exactly that PID. Never suppress EPERM for a group containing other processes.
#[cfg(target_os = "macos")]
pub(crate) fn only_waitable_leader(error: Option<i32>, pid: u32) -> bool {
    if error != Some(libc::EPERM) {
        return false;
    }
    let mut members = [0i32; 1024];
    let bytes = unsafe {
        libc::proc_listpids(
            2,
            pid,
            members.as_mut_ptr().cast(),
            std::mem::size_of_val(&members) as i32,
        )
    };
    bytes == std::mem::size_of::<i32>() as i32 && members[0] == pid as i32
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn only_waitable_leader(_error: Option<i32>, _pid: u32) -> bool {
    false
}
/// One screenshot through the daemon, in microseconds, printed as one stderr line when its reply has been written.
/// Numbers only: no Guest content, path, identifier or hash. The `broker_*` and `worker_*` stages are the QEMU
/// worker's own report and stay zero for a worker that sends none.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ShotTiming {
    /// Command written to the worker until its reply reached the waiting connection.
    pub(crate) backend_us: u64,
    pub(crate) broker_wait_us: u64,
    pub(crate) broker_read_us: u64,
    pub(crate) worker_parse_us: u64,
    pub(crate) worker_wake_us: u64,
    /// The worker's timing record seen until the reply line that follows it was complete.
    pub(crate) pipe_us: u64,
    pub(crate) parse_us: u64,
    pub(crate) normalize_us: u64,
    pub(crate) bytes: u64,
}
/// The worker's stage names in its `screenshot_timing` record, and the names they get here.
pub(crate) const SHOT_WORKER_STAGES: [(&str, &str); 4] = [
    ("broker_wait_us", "broker_wait_us"),
    ("broker_read_us", "broker_read_us"),
    ("parse_us", "worker_parse_us"),
    ("wake_us", "worker_wake_us"),
];
impl ShotTiming {
    /// From the in-process note the worker's reader attached to a reply; anything but an unsigned integer is zero.
    pub(crate) fn from_reply(reply: &Value, backend_us: u64, normalize_us: u64) -> Self {
        let field = |name: &str| reply["screenshot_timing"][name].as_u64().unwrap_or(0);
        Self {
            backend_us,
            broker_wait_us: field("broker_wait_us"),
            broker_read_us: field("broker_read_us"),
            worker_parse_us: field("worker_parse_us"),
            worker_wake_us: field("worker_wake_us"),
            pipe_us: field("pipe_us"),
            parse_us: field("parse_us"),
            normalize_us,
            bytes: field("bytes"),
        }
    }
    pub(crate) fn line(&self, total_us: u64, write_us: u64) -> String {
        format!(
            "screenshot timing total_us={total_us} backend_us={} broker_wait_us={} broker_read_us={} \
             worker_parse_us={} worker_wake_us={} pipe_us={} parse_us={} normalize_us={} write_us={write_us} bytes={}",
            self.backend_us,
            self.broker_wait_us,
            self.broker_read_us,
            self.worker_parse_us,
            self.worker_wake_us,
            self.pipe_us,
            self.parse_us,
            self.normalize_us,
            self.bytes,
        )
    }
}
thread_local! {
    /// Set by `Pending::wait` on the connection's own thread and taken by `handle_connection` after the reply write.
    pub(crate) static SHOT_TIMING: std::cell::Cell<Option<ShotTiming>> = const { std::cell::Cell::new(None) };
}
