//! Requests in flight to a worker, a guest or a driver, and how their replies complete.
use super::*;

pub(crate) struct PendingReply {
    pub(crate) sender: mpsc::Sender<Value>,
    pub(crate) limit: usize,
}
impl From<mpsc::Sender<Value>> for PendingReply {
    fn from(sender: mpsc::Sender<Value>) -> Self {
        Self {
            sender,
            limit: MAX_MESSAGE,
        }
    }
}
impl PendingReply {
    pub(crate) fn send(self, value: Value) -> std::result::Result<(), mpsc::SendError<Value>> {
        self.sender.send(value)
    }
}
pub(crate) fn validate_backend_response_bound(
    value: &Value,
    bytes: usize,
    pending: &BTreeMap<u64, PendingReply>,
) -> Result<()> {
    let limit = if value.get("event").is_some() {
        MAX_MESSAGE
    } else {
        value["id"]
            .as_u64()
            .and_then(|id| pending.get(&id))
            .map_or(MAX_MESSAGE, |p| p.limit)
    };
    if bytes > limit {
        bail!("backend output exceeded operation response bound")
    }
    Ok(())
}
pub(crate) struct Pending {
    pub(crate) receiver: mpsc::Receiver<Value>,
    pub(crate) number: u64,
    pub(crate) pending: Arc<Mutex<BTreeMap<u64, PendingReply>>>,
    pub(crate) timeout: Duration,
    pub(crate) post: Option<JobPost>,
}
pub(crate) type DriverTools = Arc<Mutex<BTreeMap<String, (u32, Value)>>>;
/// A Windows or macOS Guest's persistent cua-driver link (phase 2) and its cached
/// `tools/list`. Kept per VM with the worker pid it belongs to; a new worker gets a new one.
pub(crate) struct DriverLink {
    pub(crate) link: driver_link::Link,
    pub(crate) tools: Mutex<Option<Value>>,
}
pub(crate) type DriverLinks = Arc<Mutex<BTreeMap<String, (u32, Arc<DriverLink>)>>>;
/// The error agent input gets while a person controls the VM. The MCP adapter reads
/// the `person_in_control: <ms>` prefix back into an `ActionResult` refusal.
pub(crate) fn lease_refusal(wait: Duration) -> String {
    format!(
        "person_in_control: {} ms until agent input resumes; a person is using this VM and their input always wins",
        wait.as_millis()
    )
}
pub(crate) enum JobPost {
    Windows,
    GuestStreamStatus,
    GuestInfo {
        backend: Backend,
        guest: String,
        state: Arc<Mutex<Value>>,
        qemu_exec: bool,
        async_timeout: Arc<Mutex<GuestAsyncTimeout>>,
        async_generation: Option<u64>,
    },
    Bridge {
        state: Arc<Mutex<bridge::State>>,
        vm: String,
        pid: u32,
        attempt: String,
        start: bool,
    },
    GuestInput {
        guest_wrapped: bool,
        method: String,
    },
    /// One text chunk: the Guest reply, error replies included, goes to `input::deliver_text`.
    GuestText,
    Screenshot {
        guest_wrapped: bool,
        baselines: Arc<Mutex<screen_wait::Baselines>>,
        vm: String,
        /// The scaled-JPEG form was asked for (QEMU console broker only); `None` is the PNG form.
        scaled: Option<screenshot_scaled::Scaled>,
    },
    /// A guest agent reply `{"id","result"}` returned as its result.
    GuestResult,
    /// A cua-driver reply frame: inside the guest agent's envelope on Apple Virtualization
    /// (`wrapped`), as the QEMU worker's `{"frame"}` result otherwise.
    Driver {
        wrapped: bool,
        finish: cua_bridge::Finish,
        cache: DriverTools,
        vm: String,
        pid: u32,
    },
    Start {
        reservation: jobs::Reservation,
        vm: String,
        owner: String,
    },
    Poll {
        jobs: Arc<Mutex<jobs::Jobs>>,
        vm: String,
        id: String,
    },
}
pub(crate) enum Completion {
    Backend(Pending),
    Input {
        dir: PathBuf,
        arguments: Value,
        method: String,
        evdev: bool,
    },
    ScreenWait {
        dir: PathBuf,
        vm: String,
        probe: screen_wait::Probe,
        arguments: Value,
        baseline: Option<Value>,
    },
    Synchronous {
        dir: PathBuf,
        arguments: Value,
    },
    MacDesktop {
        dir: PathBuf,
        vm: String,
        method: String,
        arguments: Value,
    },
    /// A cua-driver request over a Windows or macOS Guest's driver link.
    Driver {
        link: Arc<DriverLink>,
        request: cua_bridge::Request,
        scopes: cua_bridge::Scopes,
        desktop: cua_bridge::Desktop,
    },
    /// A Windows guest's command, file or desktop call over its SSH server.
    Windows {
        ssh: windows_guest::Ssh,
        call: WindowsCall,
    },
    CancelJobs {
        dir: PathBuf,
        response: Value,
        jobs: Vec<jobs::Job>,
        file_sessions: Vec<Value>,
    },
}
pub(crate) enum WindowsCall {
    Exec(Value),
    Desktop(String, Value),
    Put {
        name: String,
        bytes: Vec<u8>,
        sha256: String,
    },
    Get {
        name: String,
        owner: String,
        vm: String,
        dir: PathBuf,
        files: Arc<Mutex<windows_guest::Files>>,
    },
}
impl WindowsCall {
    pub(crate) fn run(self, ssh: &windows_guest::Ssh) -> Result<Value> {
        match self {
            Self::Exec(arguments) => windows_guest::exec(ssh, &arguments),
            Self::Desktop(method, arguments) => windows_guest::desktop(ssh, &method, &arguments),
            Self::Put {
                name,
                bytes,
                sha256,
            } => windows_guest::put(ssh, &name, &bytes, &sha256),
            Self::Get {
                name,
                owner,
                vm,
                dir,
                files,
            } => {
                let bytes = windows_guest::get(ssh, &name)?;
                files
                    .lock()
                    .unwrap()
                    .download_insert(&dir, &vm, &owner, &bytes)
            }
        }
    }
}
impl Completion {
    pub(crate) fn wait(self, peer: &UnixStream) -> Result<Value> {
        match self {
            Self::Backend(p) => p.wait(),
            Self::Input {
                dir,
                arguments,
                method,
                evdev,
            } => desktop_input(&dir, &method, arguments, evdev, peer),
            Self::ScreenWait {
                dir,
                vm,
                probe,
                arguments,
                baseline,
            } => screen_wait::run(&dir, &vm, probe, &arguments, baseline, peer),
            Self::Synchronous { dir, arguments } => synchronous_exec(&dir, arguments, peer),
            Self::MacDesktop {
                dir,
                vm,
                method,
                arguments,
            } => container_tools::desktop(&dir, &vm, &method, &arguments),
            Self::Driver {
                link,
                request,
                scopes,
                desktop,
            } => {
                let mut cache = link.tools.lock().unwrap().clone();
                let reply = cua_bridge::run(&link.link, request, scopes, desktop, &mut cache);
                *link.tools.lock().unwrap() = cache;
                reply
            }
            Self::Windows { ssh, call } => call.run(&ssh),
            Self::CancelJobs {
                dir,
                mut response,
                jobs,
                file_sessions,
            } => {
                let count = jobs.len();
                let (tx, rx) = mpsc::channel();
                for job in jobs {
                    let tx = tx.clone();
                    let dir = dir.clone();
                    thread::spawn(move || {
                        let result =
                            call(&dir, "exec_cancel", json!({"id":job.vm,"job_id":job.id}));
                        let outcome = match result {
                            Ok(result) => json!({"job_id":job.id,"result":result}),
                            Err(error) => json!({"job_id":job.id,"error":error.to_string()}),
                        };
                        let _ = tx.send(outcome);
                    });
                }
                drop(tx);
                let deadline = std::time::Instant::now() + Duration::from_secs(12);
                let mut outcomes = Vec::new();
                for _ in 0..count {
                    match rx
                        .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    {
                        Ok(value) => outcomes.push(value),
                        Err(_) => break,
                    }
                }
                response["job_cancellation"] = json!({"attempted":count,"completed_requests":outcomes.len(),"unconfirmed_requests":count-outcomes.len(),"outcomes":outcomes});
                let file_count = file_sessions.len();
                let (tx, rx) = mpsc::channel();
                for args in file_sessions {
                    let tx = tx.clone();
                    let dir = dir.clone();
                    thread::spawn(move || {
                        let outcome = match call(&dir, "file_session_end", args) {
                            Ok(result) => json!({"result":result}),
                            Err(error) => json!({"error":error.to_string()}),
                        };
                        let _ = tx.send(outcome);
                    });
                }
                drop(tx);
                let deadline = std::time::Instant::now() + Duration::from_secs(8);
                let mut file_outcomes = Vec::new();
                for _ in 0..file_count {
                    match rx
                        .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    {
                        Ok(value) => file_outcomes.push(value),
                        Err(_) => break,
                    }
                }
                response["file_cancellation"] = json!({"attempted":file_count,"unconfirmed_requests":file_count-file_outcomes.len(),"outcomes":file_outcomes});
                response["note"]=json!("Cancellation is best effort, not rollback. Poll asynchronous jobs or inspect the synchronous command's terminal result to confirm termination.");
                Ok(response)
            }
        }
    }
}
pub(crate) fn peer_disconnected(fds: &[std::os::fd::RawFd]) -> bool {
    let mut descriptors: Vec<libc::pollfd> = fds
        .iter()
        .map(|fd| libc::pollfd {
            fd: *fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    // Darwin requires a read filter to report socket EOF reliably; polling never consumes bytes.
    let ready = unsafe {
        libc::poll(
            descriptors.as_mut_ptr(),
            descriptors.len() as libc::nfds_t,
            0,
        )
    };
    ready > 0
        && descriptors
            .iter()
            .any(|fd| fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0)
}
/// Original observation step of the MCP transport monitor.
pub(crate) const PEER_MONITOR_STEP: Duration = Duration::from_millis(50);
/// Upper bound on one blocking observation, so a kernel that does not wake a blocked poll for
/// some descriptor type still re-evaluates it, and the monitor still notices `stopped`.
pub(crate) const PEER_MONITOR_WAIT: Duration = Duration::from_millis(250);
/// `peer_disconnected`, but sleeping in the kernel for up to `limit` until a hang-up arrives.
/// Same descriptors, same read filter, same hang-up test. Descriptors that are merely readable
/// (a queued request, a regular file, a terminal) cannot block, so that case keeps the
/// original 50 ms step instead of spinning.
pub(crate) fn await_peer_disconnect(fds: &[std::os::fd::RawFd], limit: Duration) -> bool {
    let mut descriptors: Vec<libc::pollfd> = fds
        .iter()
        .map(|fd| libc::pollfd {
            fd: *fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let ready = unsafe {
        libc::poll(
            descriptors.as_mut_ptr(),
            descriptors.len() as libc::nfds_t,
            limit.as_millis().min(i32::MAX as u128) as i32,
        )
    };
    if ready == 0 {
        return false;
    }
    if ready > 0
        && descriptors
            .iter()
            .any(|fd| fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0)
    {
        return true;
    }
    thread::sleep(PEER_MONITOR_STEP.min(limit));
    false
}
impl Pending {
    pub(crate) fn wait(self) -> Result<Value> {
        let waiting = std::time::Instant::now();
        match self.receiver.recv_timeout(self.timeout) {
            Ok(mut reply) => {
                let backend_us = waiting.elapsed().as_micros() as u64;
                if matches!(self.post, Some(JobPost::GuestStreamStatus)) {
                    if let Some(error) = reply.get("error") {
                        if matches!(
                            error["code"].as_str(),
                            Some("unsupported" | "unknown_command")
                        ) || error.as_str() == Some("unknown command")
                        {
                            return Ok(
                                json!({"reachable":null,"reason":"worker_does_not_support_probe"}),
                            );
                        }
                        bail!("backend: {error}")
                    }
                    // The probe's nullable typed `error` is status data, not a Guest envelope.
                    return reply
                        .get_mut("result")
                        .map(Value::take)
                        .context("missing probe result");
                }
                if let Some(error) = reply.get("error") {
                    // A refusal of the scaled screenshot form keeps its fixed word beside the same text.
                    if let Some(JobPost::Screenshot {
                        scaled: Some(_), ..
                    }) = &self.post
                    {
                        if let Some(refusal) = screenshot_scaled::Refusal::from_name(
                            &reply[screenshot_scaled::REFUSAL_FIELD],
                        ) {
                            return Err(screenshot_scaled::Refused::new(
                                refusal,
                                format!("backend: {error}"),
                            )
                            .into());
                        }
                    }
                    bail!("backend: {error}")
                }
                // A Guest refusal of a text chunk carries delivery accounting that only
                // `input::deliver_text` interprets; every other Guest error ends the call here.
                if !matches!(self.post, Some(JobPost::GuestText)) {
                    if let Some(error) = reply["result"].get("error") {
                        bail!("guest: {error}")
                    }
                }
                let mut result = reply.get_mut("result").map_or(Value::Null, Value::take);
                match self.post {
                    Some(JobPost::GuestInfo {
                        backend,
                        guest,
                        state,
                        qemu_exec,
                        async_timeout,
                        async_generation,
                    }) => {
                        let mut policy = async_timeout.lock().unwrap();
                        if async_generation.is_some() && policy.generation == async_generation {
                            policy.maximum = advertised_async_timeout(&result["result"], qemu_exec);
                        }
                        drop(policy);
                        if backend == Backend::AppleVirtualization && guest == "macos" {
                            update_guest_metal_health(
                                &mut state.lock().unwrap(),
                                &result["result"],
                            );
                        }
                    }
                    Some(JobPost::Windows) => {
                        result = windows::normalize(result)?;
                    }
                    Some(JobPost::Bridge {
                        state,
                        vm,
                        pid,
                        attempt,
                        start,
                    }) => {
                        let proof = state
                            .lock()
                            .unwrap()
                            .accept(&result, &vm, pid, &attempt, start)?;
                        result["binding"] = serde_json::to_value(proof)?;
                    }
                    Some(JobPost::GuestInput {
                        guest_wrapped,
                        method,
                    }) => {
                        result = normalize_backend_input(result, &method, guest_wrapped)?;
                    }
                    Some(JobPost::Start {
                        reservation,
                        vm,
                        owner,
                    }) => {
                        if result["result"]["state"] != "running" {
                            bail!("guest returned invalid asynchronous start state")
                        }
                        let job_id = result["result"]["job_id"]
                            .as_str()
                            .context("guest returned no asynchronous job id")?;
                        reservation.register(&vm, job_id, &owner, std::time::Instant::now())?;
                    }
                    Some(JobPost::Poll { jobs, vm, id }) => {
                        let state = result["result"]["state"]
                            .as_str()
                            .context("guest returned no job state")?;
                        if state == "finished" {
                            // A stopped command waiter may still report containment_failed.
                            // Only the normal validated terminal result is cleanup evidence.
                            if result["result"].get("error").is_none() {
                                completed_exec_result(&result)?;
                                jobs.lock().unwrap().finish(&vm, &id);
                            }
                        } else if state != "running" {
                            bail!("invalid guest job state")
                        }
                    }
                    Some(JobPost::Screenshot {
                        guest_wrapped,
                        baselines,
                        vm,
                        scaled,
                    }) => {
                        let started = std::time::Instant::now();
                        result = normalize_screenshot(result, guest_wrapped)?;
                        let normalize_us = started.elapsed().as_micros() as u64;
                        SHOT_TIMING.set(Some(ShotTiming::from_reply(
                            &reply,
                            backend_us,
                            normalize_us,
                        )));
                        if let Some(scaled) = scaled {
                            verify_scaled_screenshot(&result, &scaled)?;
                        } else {
                            // A scaled control picture is not the full-size frame screen_wait compares against.
                            baselines.lock().unwrap().record(&vm, &result);
                        }
                    }
                    Some(JobPost::GuestResult) => {
                        result = result
                            .get("result")
                            .cloned()
                            .context("guest reply missing result")?;
                    }
                    Some(JobPost::Driver {
                        wrapped,
                        finish,
                        cache,
                        vm,
                        pid,
                    }) => {
                        let frame = if wrapped {
                            result["result"].get("frame")
                        } else {
                            result.get("frame")
                        }
                        .context("driver reply missing frame")?;
                        if finish.list {
                            if let Some(list) = frame.get("result") {
                                cache.lock().unwrap().insert(vm, (pid, list.clone()));
                            }
                        }
                        result = finish.result(frame)?;
                    }
                    Some(JobPost::GuestText) => {
                        // Counts and codes only: the Guest's per-key diagnostics never leave the daemon.
                        result = input::guest_text_summary(&result);
                    }
                    Some(JobPost::GuestStreamStatus) | None => {}
                }
                Ok(result)
            }
            Err(e) => {
                self.pending.lock().unwrap().remove(&self.number);
                bail!("backend response unavailable ({e}); query status before retrying")
            }
        }
    }
}
pub(crate) fn normalize_backend_input(
    value: Value,
    method: &str,
    guest_wrapped: bool,
) -> Result<Value> {
    if !guest_wrapped && method == "input_cleanup" && value["released"] != true {
        bail!("QEMU input operation cleanup is unconfirmed")
    }
    normalize_guest_input(
        if guest_wrapped {
            value
        } else {
            json!({"result":value})
        },
        method,
    )
}
pub(crate) fn normalize_guest_input(value: Value, method: &str) -> Result<Value> {
    let result = value
        .get("result")
        .cloned()
        .context("Guest input missing result")?;
    if method == "input_step" {
        if result["dispatched"] != true || result["guest_delivery_confirmed"] != false {
            bail!("Guest input dispatch acknowledgement invalid")
        }
    } else if method == "input_cleanup" {
        if !result["released"].is_boolean() {
            bail!("Guest input cleanup acknowledgement invalid")
        }
        if result["released"] == true && result["synthetic_release_dispatched"] != true {
            bail!("Guest synthetic release is unconfirmed")
        }
    } else {
        bail!("unexpected Guest input reply")
    }
    Ok(result)
}
pub(crate) fn normalize_screenshot(mut value: Value, guest_wrapped: bool) -> Result<Value> {
    use base64::Engine;
    if guest_wrapped {
        value = value
            .get_mut("result")
            .map(Value::take)
            .context("Guest screenshot missing result")?;
    }
    let mime = value["mime_type"]
        .as_str()
        .context("screenshot missing MIME type")?;
    if !["image/jpeg", "image/png"].contains(&mime) {
        bail!("unsupported screenshot MIME type")
    }
    let encoded = value["data_base64"]
        .as_str()
        .context("screenshot missing image data")?;
    let max_bytes = if mime == "image/png" {
        MAX_SCREENSHOT_BYTES
    } else {
        700 * 1024
    };
    if encoded.len() > max_bytes.div_ceil(3) * 4 {
        bail!("screenshot exceeds MIME byte bound")
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("invalid screenshot base64")?;
    if bytes.len() > max_bytes || value["byte_length"].as_u64() != Some(bytes.len() as u64) {
        bail!("screenshot size mismatch")
    }
    if (mime == "image/png" && !bytes.starts_with(b"\x89PNG\r\n\x1a\n"))
        || (mime == "image/jpeg"
            && (!bytes.starts_with(&[0xff, 0xd8, 0xff]) || !bytes.ends_with(&[0xff, 0xd9])))
    {
        bail!("screenshot signature does not match MIME type")
    }
    for field in ["width", "height"] {
        if value[field].as_u64().is_none_or(|n| n == 0 || n > 8192) {
            bail!("invalid screenshot dimensions")
        }
    }
    if value["frame_id"].as_str().is_none_or(|id| id.len() != 36) {
        bail!("invalid screenshot frame id")
    }
    Ok(value)
}
/// The reply to a request of the scaled form, after `normalize_screenshot`: the helper's JPEG, checked against
/// what was asked for, or a PNG from a helper without the form (already checked as any PNG reply; the caller
/// reads `mime_type`).
pub(crate) fn verify_scaled_screenshot(
    value: &Value,
    scaled: &screenshot_scaled::Scaled,
) -> Result<()> {
    if value["mime_type"] == "image/png" {
        return Ok(());
    }
    scaled.verify(value).map(drop)
}
pub(crate) fn screenshot_content(mut value: Value) -> Result<Value> {
    let data = value
        .as_object_mut()
        .and_then(|v| v.remove("data_base64"))
        .context("screenshot missing image data")?;
    Ok(
        json!({"content":[{"type":"image","mimeType":value["mime_type"],"data":data},{"type":"text","text":serde_json::to_string(&value)?}],"isError":false}),
    )
}
