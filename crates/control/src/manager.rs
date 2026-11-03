//! The `Manager`: every VM, its worker and its requests, behind one lock.
use super::*;

pub(crate) fn validate_local_stream_bridge(
    method: &str,
    args: &Value,
    backend: Backend,
    guest: &str,
) -> Result<()> {
    // macOS guests stream through Screen Sharing capture; Linux guests
    // through the agent's desktop stream (browser desktop).
    if backend != Backend::AppleVirtualization || !matches!(guest, "macos" | "linux") {
        bail!("unsupported: Guest stream bridge requires Apple Virtualization")
    }
    let allowed = if method == "guest_stream_bridge_binding" {
        vec!["id", "worker_instance", "bridge_instance"]
    } else if method == "start_guest_stream_bridge" {
        vec!["id", "socket_path"]
    } else {
        vec!["id"]
    };
    if args.as_object().is_none_or(|fields| {
        fields
            .keys()
            .any(|field| !allowed.contains(&field.as_str()))
    }) {
        bail!("invalid local bridge fields")
    }
    if method == "start_guest_stream_bridge" {
        let path = args["socket_path"]
            .as_str()
            .context("socket_path required")?;
        if path.len() > 1024 || path.contains('\0') || !Path::new(path).is_absolute() {
            bail!("bridge socket_path must be absolute and bounded")
        }
    }
    Ok(())
}
pub(crate) fn shutdown_mode(args: &Value) -> Result<&str> {
    let mode = args
        .get("mode")
        .map(|value| value.as_str().context("shutdown mode must be a string"))
        .transpose()?
        .unwrap_or("backend");
    if !["backend", "guest"].contains(&mode) {
        bail!("unsupported shutdown mode; expected backend or guest")
    }
    Ok(mode)
}
pub(crate) struct Manager {
    pub(crate) snapshot_ops: snapshot_control::Operations,
    pub(crate) restore_starts: Arc<Mutex<snapshot_restore::Starts>>,
    pub(crate) memory: MemoryBudget,
    pub(crate) dir: PathBuf,
    pub(crate) backend: PathBuf,
    pub(crate) qemu: Option<QemuRegistration>,
    pub(crate) worker_key_event_log_dir: Option<PathBuf>,
    pub(crate) worker_key_event_logs: BTreeMap<String, (String, PathBuf)>,
    pub(crate) qemu_window: Value,
    pub(crate) native_connections: BTreeMap<String, (String, String)>,
    pub(crate) configs: BTreeMap<String, Config>,
    /// Registered VMs that failed the creation checks at registry load, with the reason. Such an entry stays in the
    /// registry as it is, is listed as `invalid` and is refused by `start`; nothing here ever removes it.
    pub(crate) invalid: BTreeMap<String, String>,
    /// What the daemon turned off or set aside at its start so that it could start (a missing runtime piece, a
    /// damaged registry), in plain words; `hello` reports them as `runtime.notices`.
    pub(crate) notices: Vec<String>,
    pub(crate) jobs: Arc<Mutex<jobs::Jobs>>,
    pub(crate) file_sessions: FileSessionRoutes,
    /// Staged transfers of Windows guests, which go over SSH, not a worker.
    pub(crate) windows_files: Arc<Mutex<windows_guest::Files>>,
    /// Each VM's cua-driver `tools/list` result, with the worker pid it came from.
    pub(crate) driver_tools: DriverTools,
    pub(crate) driver_links: DriverLinks,
    /// Who controls each VM's input: a person at a viewer, or the agent (`control_lease`).
    pub(crate) leases: Arc<control_lease::Leases>,
    pub(crate) baselines: Arc<Mutex<screen_wait::Baselines>>,
    pub(crate) running: BTreeMap<String, Running>,
    /// Lifecycle events for MCP adapters (`events`).
    pub(crate) events: Arc<events::Log>,
    /// The daemon's background exit monitor, parked while no worker is alive.
    pub(crate) exit_monitor: Option<thread::Thread>,
    /// Each VM's idle policy and the pause the daemon asked for (`idle`).
    pub(crate) idle: BTreeMap<String, idle::Idle>,
}
/// Background reaping cadence while at least one worker is alive (unchanged).
pub(crate) const EXIT_MONITOR_LIVE: Duration = Duration::from_millis(100);
/// With no live worker there is nothing to observe: stay parked until `Manager::started`
/// wakes the monitor. The bound only lets the thread notice a dropped manager.
pub(crate) const EXIT_MONITOR_IDLE: Duration = Duration::from_secs(5);
#[cfg(test)]
pub(crate) static EXIT_MONITOR_PASSES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// One background observation of every worker; returns how long the monitor may park.
pub(crate) fn exit_monitor_pass(manager: &Mutex<Manager>) -> Duration {
    #[cfg(test)]
    EXIT_MONITOR_PASSES.fetch_add(1, Ordering::Relaxed);
    let mut live = false;
    let manager = &mut *manager.lock().unwrap();
    for (id, runtime) in manager.running.iter_mut() {
        // An observation error is not proof of exit; keep the live cadence.
        if !matches!(runtime.poll_exit(), Ok(Some(_))) {
            live = true;
        }
        runtime.publish_exit(id, &manager.events);
    }
    if live {
        EXIT_MONITOR_LIVE
    } else {
        EXIT_MONITOR_IDLE
    }
}
pub(crate) fn spawn_exit_monitor(manager: &Arc<Mutex<Manager>>) {
    let monitor = Arc::downgrade(manager);
    let handle = thread::spawn(move || {
        while let Some(manager) = monitor.upgrade() {
            let park = exit_monitor_pass(&manager);
            drop(manager);
            // An unpark delivered at any point after the pass above is retained as a token,
            // so a worker registered while this thread was not yet parked still wakes it.
            thread::park_timeout(park);
        }
    });
    manager.lock().unwrap().exit_monitor = Some(handle.thread().clone());
}
impl Manager {
    /// One cua-driver request (`cua_bridge`). Refusals and the cached tool list are
    /// answered here; anything else goes to the worker, and its reply is awaited off the
    /// manager lock: Apple Virtualization through the guest agent's `driver_rpc` method
    /// (vsock), QEMU through the worker's `veynode.driver` port. Windows and macOS Guests
    /// use a persistent link instead (`driver_link`): the driver's own socket proxy over
    /// SSH on Windows, the embedded driver over SSH in a macOS container.
    pub(crate) fn driver_rpc(
        &mut self,
        id: &str,
        args: &Value,
        deferred: &mut Option<Completion>,
    ) -> Result<Value> {
        let config = &self.configs[id];
        let linked = match (config.backend, config.guest.as_str()) {
            (Backend::QemuHvf, "windows") | (Backend::AppleVirtualization, "macos") => true,
            (_, "linux") => false,
            _ => bail!("unsupported: the cua-driver bridge serves Linux Guests, Windows machines and macOS containers"),
        };
        let wrapped = config.backend == Backend::AppleVirtualization;
        let request = cua_bridge::Request::from_args(args)?;
        if let cua_bridge::Request::Call { tool, .. } = &request {
            let acts = matches!(
                cua_bridge::scope(tool),
                Some(cua_bridge::Scope::Input | cua_bridge::Scope::Browser)
            );
            if acts {
                if let Err(wait) = self.leases.acquire(id, std::time::Instant::now()) {
                    return Ok(cua_bridge::refusal(
                        "person_in_control",
                        &lease_refusal(wait),
                    ));
                }
            }
        }
        let scopes = cua_bridge::Scopes::from_value(&args["scopes"]);
        let desktop = cua_bridge::Desktop {
            person_attached: self.native_connections.contains_key(id) || self.leases.watched(id),
        };
        let run = self.running.get_mut(id).context("VM process not active")?;
        if run.poll_exit()?.is_some() {
            bail!("VM process has exited")
        }
        let pid = run.child.id();
        if linked {
            let link = self.driver_link(id, pid)?;
            *deferred = Some(Completion::Driver {
                link,
                request,
                scopes,
                desktop,
            });
            return Ok(Value::Null);
        }
        let cached = self
            .driver_tools
            .lock()
            .unwrap()
            .get(id)
            .filter(|(from, _)| *from == pid)
            .map(|(_, list)| list.clone());
        let outgoing = match cua_bridge::prepare(request, scopes, desktop, cached.as_ref()) {
            cua_bridge::Prepared::Reply(reply) => return Ok(reply),
            cua_bridge::Prepared::Send(outgoing) => outgoing,
        };
        let number = run.next;
        run.next += 1;
        let mut frame = outgoing.frame;
        frame["id"] = json!(number);
        let deadline_ms = outgoing.deadline.as_millis() as u64;
        let command = if wrapped {
            json!({"id":number,"method":"guest_request","request":{"id":number.to_string(),"method":"driver_rpc","arguments":{"frame":frame,"deadline_ms":deadline_ms}}})
        } else {
            json!({"id":number,"method":"driver_rpc","frame":frame,"deadline_ms":deadline_ms})
        };
        let (tx, rx) = mpsc::channel();
        run.pending.lock().unwrap().insert(
            number,
            PendingReply {
                sender: tx,
                limit: response_limit("driver_rpc"),
            },
        );
        if let Err(e) = write_json(&mut run.stdin, &command) {
            run.pending.lock().unwrap().remove(&number);
            return Err(e);
        }
        *deferred = Some(Completion::Backend(Pending {
            receiver: rx,
            number,
            pending: run.pending.clone(),
            // Past the worker's own deadline, so its timeout reply is the one seen.
            timeout: outgoing.deadline + Duration::from_secs(5),
            post: Some(JobPost::Driver {
                wrapped,
                finish: outgoing.finish,
                cache: self.driver_tools.clone(),
                vm: id.into(),
                pid,
            }),
        }));
        Ok(Value::Null)
    }
    /// `control_lease {action}`: `status` (the default) reports who controls input;
    /// `release` gives the agent's lease back until its next input.
    pub(crate) fn control_lease(&mut self, id: &str, args: &Value) -> Result<Value> {
        let now = std::time::Instant::now();
        match args["action"].as_str().unwrap_or("status") {
            "status" => {}
            "release" => self.leases.release(id, now),
            other => bail!("control_lease action must be status or release, not {other}"),
        }
        Ok(self.leases.status(id, now))
    }
    /// The VM's driver link for this worker, made on first use; the driver is installed
    /// and started when the link first connects, off the manager lock.
    pub(crate) fn driver_link(&mut self, id: &str, pid: u32) -> Result<Arc<DriverLink>> {
        let mut links = self.driver_links.lock().unwrap();
        if let Some((from, link)) = links.get(id) {
            if *from == pid {
                return Ok(link.clone());
            }
        }
        let link = if self.configs[id].guest == "windows" {
            windows_driver::link(windows_guest::Ssh::locate(&self.dir, &self.configs[id])?)
        } else {
            container_tools::macos_driver(&self.dir, id)
        };
        let link = Arc::new(DriverLink {
            link,
            tools: Mutex::new(None),
        });
        // Replacing an old worker's link drops it, which closes its connection.
        links.insert(id.into(), (pid, link.clone()));
        Ok(link)
    }
    /// A deferred call to a running Windows guest over its SSH server.
    pub(crate) fn windows_call(&mut self, id: &str, call: WindowsCall) -> Result<Completion> {
        let run = self.running.get_mut(id).context("VM process not active")?;
        if run.poll_exit()?.is_some() {
            bail!("VM process has exited")
        }
        let ssh = windows_guest::Ssh::locate(&self.dir, &self.configs[id])?;
        Ok(Completion::Windows { ssh, call })
    }
    /// File tools of a Windows guest: chunks are staged here, and the file
    /// crosses SSH once (commit, download begin).
    pub(crate) fn windows_file(
        &mut self,
        id: &str,
        method: &str,
        args: &Value,
        deferred: &mut Option<Completion>,
    ) -> Result<Value> {
        let owner = args["owner"].as_str().unwrap_or_default().to_owned();
        let files = self.windows_files.clone();
        let mut store = files.lock().unwrap();
        match method {
            "file_session_end" => Ok(store.session_end(id, &owner)),
            "file_upload_begin" => store.upload_begin(&self.dir, id, args),
            "file_upload_write" => store.upload_write(id, args),
            "file_download_read" => store.download_read(id, args),
            "file_cancel" => store.cancel(id, args),
            "file_upload_commit" => {
                let (name, bytes, sha256) = store.upload_take(id, args)?;
                drop(store);
                *deferred = Some(self.windows_call(
                    id,
                    WindowsCall::Put {
                        name,
                        bytes,
                        sha256,
                    },
                )?);
                Ok(Value::Null)
            }
            "file_download_begin" => {
                drop(store);
                let name =
                    windows_guest::file_name(args["path"].as_str().context("missing path")?)?
                        .to_owned();
                *deferred = Some(self.windows_call(
                    id,
                    WindowsCall::Get {
                        name,
                        owner,
                        vm: id.into(),
                        dir: self.dir.clone(),
                        files,
                    },
                )?);
                Ok(Value::Null)
            }
            _ => bail!("unknown file method"),
        }
    }
    /// Registers a freshly started worker and resumes background exit observation at once.
    pub(crate) fn started(&mut self, id: String, running: Running) {
        if let Some(previous) = self.running.get(&id) {
            previous.publish_exit(&id, &self.events);
        }
        self.events.started(&id);
        self.running.insert(id, running);
        if let Some(monitor) = &self.exit_monitor {
            monitor.unpark();
        }
    }
    pub(crate) fn load(dir: PathBuf, backend: PathBuf, memory: MemoryBudget) -> Result<Self> {
        check_registry_schema(&dir)?;
        let (configs, damaged) = load_registry(&dir)?;
        if let Some(note) = &damaged {
            log_line(&format!("vm-control: {note}"));
        }
        let snapshot_ops = snapshot_control::Operations::open(&dir)?;
        Ok(Self {
            snapshot_ops,
            restore_starts: Arc::new(Mutex::new(snapshot_restore::Starts::default())),
            memory,
            dir,
            backend,
            qemu: None,
            worker_key_event_log_dir: None,
            worker_key_event_logs: BTreeMap::new(),
            qemu_window: json!({"native_window":false,"display_only":true,"input_enabled":false}),
            native_connections: BTreeMap::new(),
            configs,
            invalid: BTreeMap::new(),
            notices: damaged.into_iter().collect(),
            jobs: Arc::new(Mutex::new(jobs::Jobs::default())),
            file_sessions: BTreeMap::new(),
            windows_files: Arc::default(),
            driver_tools: Arc::default(),
            driver_links: Arc::default(),
            leases: Arc::default(),
            baselines: Arc::new(Mutex::new(screen_wait::Baselines::default())),
            running: BTreeMap::new(),
            events: Arc::new(events::Log::default()),
            exit_monitor: None,
            idle: BTreeMap::new(),
        })
    }
    /// Everything `create` requires of a configuration beyond parsing it. `create`, `start` and the registry load use
    /// this one function, so a definition is judged the same way wherever it enters.
    pub(crate) fn admissible(&self, config: &Config) -> Result<()> {
        config.validate()?;
        if config.backend == Backend::QemuHvf {
            backend::require_registration(&self.qemu)?
                .require_profile(config.qemu_graphics_profile)?;
            if config.window && !backend::window_supported(&self.qemu_window) {
                bail!("native display-only window unavailable for registered QEMU helper")
            }
        }
        Ok(())
    }
    /// Registry load, once the daemon's QEMU registration is known: every entry goes through the creation checks. A
    /// failure is recorded per VM and stops neither the daemon nor the other VMs; the registry file is not written.
    /// The verdict holds until the next daemon start.
    pub(crate) fn validate_registry(&mut self) {
        self.invalid = self
            .configs
            .iter()
            .filter_map(|(id, config)| {
                let reason = format!("{:#}", validate_new_vm_id(id).and_then(|()| self.admissible(config)).err()?);
                log_line(&format!("vm-control: registered VM {id} failed validation at registry load and cannot be started: {reason}"));
                Some((id.clone(), reason))
            })
            .collect();
    }
    pub(crate) fn assigned_memory_mib(&mut self) -> Result<u64> {
        self.running.values_mut().try_fold(0u64, |sum, runtime| {
            if runtime.poll_exit()?.is_none() {
                sum.checked_add(runtime.assigned_memory_mib)
                    .context("active assigned memory arithmetic overflow")
            } else {
                Ok(sum)
            }
        })
    }
    /// `log_read {id, stream, max_bytes}`: the redacted tail of the VM's serial
    /// console log or its worker's diagnostic log (see `events::read_tail`).
    pub(crate) fn log_read(&self, id: &str, args: &Value) -> Result<Value> {
        let max_bytes = args
            .get("max_bytes")
            .map_or(Some(events::MAX_LOG_BYTES), Value::as_u64)
            .context("max_bytes must be an integer")?;
        let stream = args["stream"].as_str().unwrap_or("");
        let runtime = self
            .running
            .get(id)
            .map(|r| r.state.lock().unwrap().clone());
        let path = match stream {
            "serial" => self.configs[id]
                .serial_log
                .clone()
                .or_else(|| runtime.as_ref()?["serial_log"].as_str().map(PathBuf::from)),
            "worker" => runtime
                .as_ref()
                .and_then(|r| r["stderr_log"].as_str().map(PathBuf::from)),
            _ => bail!("stream must be serial or worker"),
        };
        let Some(path) = path else {
            bail!("unavailable: this VM has no {stream} log (serial needs serial_log; worker logs are QEMU only and appear once its worker has started)")
        };
        let mut result = events::read_tail(&path, max_bytes, stream == "worker")?;
        result["stream"] = json!(stream);
        Ok(result)
    }
    pub(crate) fn view(&mut self, id: &str) -> Result<Value> {
        let exit = self
            .running
            .get_mut(id)
            .map(Running::poll_exit)
            .transpose()?
            .flatten();
        let config = self.configs.get(id).context("VM not found")?;
        let mut result = json!({"config":config,"state":"stopped","capabilities":backend::guest_capabilities(config.backend,&config.guest)});
        result["key_event_log"] = json!({"enabled":false});
        if config.backend == Backend::AppleVirtualization && exit.is_none() {
            if let Some((instance, path)) = self.worker_key_event_logs.get(id) {
                if self.running.get(id).is_some_and(|runtime| {
                    runtime.bridge.lock().unwrap().worker_instance == *instance
                }) {
                    result["key_event_log"] = json!({"enabled":true,"path":path});
                }
            }
        }
        result["capabilities"]["product_capabilities"] =
            product_capabilities(config.backend, &config.guest);
        if config.backend == Backend::QemuHvf && !config.qemu_guest_exec {
            // This VM cannot reach the Guest service's execution transport.
            result["capabilities"]["product_capabilities"]["unicode_text_input"] = json!({"status":"unsupported","reason":"this VM is registered without qemu_guest_exec; vm_input_text needs the Guest execution transport and is refused"});
        }
        // The same narrowing by this VM's registration and this daemon's registered QEMU helpers.
        let mut features = control_features(config.backend, &config.guest);
        if config.backend == Backend::QemuHvf {
            if config.guest == "linux" {
                features["gpu_3d"] = qemu_gpu_3d(config.qemu_graphics_profile);
            }
            let mut narrow = |names: &[&str], reason: &str| {
                for name in names {
                    features[*name] = json!({"status":"unsupported","reason":reason});
                }
            };
            if !config.qemu_guest_exec {
                narrow(&["command_execution", "async_jobs", "unicode_text_input"], "this VM is registered without qemu_guest_exec; the observer-only transport carries no execution or text input and the calls are refused");
            }
            if self.qemu.as_ref().is_none_or(|q| !q.has_screenshot()) {
                narrow(
                    &["screenshot"],
                    "this daemon has no registered QEMU capture; vm_screenshot is refused",
                );
            }
            if self
                .qemu
                .as_ref()
                .is_none_or(|q| q.console_broker.is_none())
            {
                narrow(&["screenshot_scaled_jpeg"], "this daemon has no registered QEMU console broker; the scaled JPEG screenshot form is refused");
                narrow(&["pointer_key_input", "unicode_text_input"], "this daemon has no registered QEMU persistent console; input calls are refused");
            }
            if self.qemu.is_none() {
                narrow(&["gpu_3d"], "this daemon has no registered QEMU runtime; no Guest 3D acceleration is available");
            } else if config.qemu_graphics_profile.venus()
                && self.qemu.as_ref().is_none_or(|q| q.venus_runtime.is_none())
            {
                narrow(&["gpu_3d"], "this VM selects experimental Venus but the daemon has no registered Venus runtime; no Guest 3D acceleration is available");
            }
        }
        if config.backend == Backend::AppleVirtualization && config.guest == "linux" {
            if config.machine_identity_dir.is_none() {
                features["snapshot"] = json!({"status":"unsupported","reason":"This VM has no explicit persistent Linux machine_identity_dir. Snapshot capture refuses it; existing VMs are never silently assigned a new snapshot identity."});
            }
            result["capabilities"]["snapshot_kinds"] = json!({
                "full_state":features["snapshot"],
                "disk_rollback":{"named_create":false,"restore":false,"automatic_restore_protection":true,"reason":"Full-state restore retains an independently verified protective disk copy; disk-only rollback is not exposed as full-state restore."},
                "suspend":{"save":false,"resume":false,"reason":"A resumable named full-state snapshot is implemented separately; a standalone suspend lifecycle is not exposed."},
                "operation_fenced":self.snapshot_ops.active_vm(id)
            });
        }
        result["capabilities"]["control_features"] = features;
        // Added key, macOS Guests only: Linux and Windows status objects keep their exact previous key set.
        let limits = known_limits(config.backend, &config.guest);
        if limits.as_array().is_some_and(|limits| !limits.is_empty()) {
            result["capabilities"]["known_limits"] = limits;
        }
        if config.backend == Backend::AppleVirtualization && config.guest == "macos" {
            result["capabilities"]["screenshot"] = json!(true);
            result["capabilities"]["screenshot_scaled_jpeg"] = json!(false);
            result["capabilities"]["screenshot_availability"]=json!("requires running macOS Guest service --allow-observe and Guest Screen Recording permission; query guest_info for current availability");
        }
        if config.backend == Backend::QemuHvf {
            result["capabilities"]["graphics_profile"] = json!({"configured":config.qemu_graphics_profile,"local_runtime_registered":self.qemu.as_ref().is_some_and(|q|q.venus_runtime.is_some()),"host_supported":self.running.get(id).is_some_and(|r|r.state.lock().unwrap()["graphics_profile"]["host_supported"]==true),"guest_verified":false,"scope":"explicit experimental host profile; actual Guest application Vulkan driver/pixels require independent validation"});
            if !config.qemu_graphics_profile.venus() {
                result["capabilities"]["graphics_profile"]["local_runtime_registered"] =
                    json!(self.qemu.is_some());
                result["capabilities"]["graphics_profile"]["scope"] = json!(if config
                    .qemu_graphics_profile
                    .requires_gl()
                {
                    "configured Virgl profile; Guest driver, rendered pixels and performance require independent validation"
                } else {
                    "explicit unaccelerated virtio-2d console; no Guest 3D or snapshot acceptance implied"
                });
            }
            if config.qemu_graphics_profile.venus() {
                result["capabilities"]["graphics_profile"]["host_api_constraints"] =
                    qemu_graphics::host_api_constraints(config.qemu_graphics_profile);
            }
            result["capabilities"]["command_execution"] = json!(config.qemu_guest_exec);
            result["capabilities"]["async_commands"] = json!(config.qemu_guest_exec);
            result["capabilities"]["guest_integration"] = json!(if config.qemu_guest_exec {
                "opted-in private virtio-serial exec v2; requires rootless cgroup-v2 Guest service; query actual guest_info"
            } else {
                "observer-only private virtio-serial v1"
            });
            result["capabilities"]["input"] = json!(self
                .qemu
                .as_ref()
                .is_some_and(|q| q.console_broker.is_some()));
            result["capabilities"]["input_scope"] = json!(if config.qemu_guest_exec {
                "absolute click/drag/discrete wheel/explicit qcode; Unicode text only through the Guest service (vm_input_text, see product_capabilities.unicode_text_input); snapshot <=5s and matching generation required; no layout translation for keys"
            } else {
                "absolute click/drag/discrete wheel/explicit qcode only; snapshot <=5s and matching generation required; no text/layout translation (vm_input_text requires qemu_guest_exec)"
            });
            result["capabilities"]["screenshot"] =
                json!(self.qemu.as_ref().is_some_and(|q| q.has_screenshot()));
            // The scaled-JPEG request form exists only behind the console broker; whether the running helper
            // has it is known only from its reply (an older helper answers with a PNG).
            result["capabilities"]["screenshot_scaled_jpeg"] = json!(self
                .qemu
                .as_ref()
                .is_some_and(|q| q.console_broker.is_some()));
        }
        if config.backend == Backend::QemuHvf {
            result["capabilities"]["native_window"] =
                json!(backend::window_supported(&self.qemu_window));
            result["capabilities"]["native_window_details"] = self.qemu_window.clone();
        }
        if let Some(runtime) = self.running.get_mut(id) {
            runtime.publish_exit(id, &self.events);
            result["runtime"] = runtime.state.lock().unwrap().clone();
            result["runtime"]["execution_quiescing"] = json!(runtime.execution_quiescing);
            if let Some(exit) = exit {
                result["state"] = json!(if exit.success() || runtime.expected_exit {
                    "stopped"
                } else {
                    "crashed"
                });
                result["exit_code"] = json!(exit.code());
            } else {
                // Backend events are progress, not proof that the owned worker
                // exited. Keep terminal status exclusively tied to waitid/wait.
                result["state"] = if matches!(
                    result["runtime"]["state"].as_str(),
                    Some("stopped" | "crashed" | "error")
                ) {
                    json!("stopping")
                } else {
                    result["runtime"]["state"].clone()
                };
                result["pid"] = json!(runtime.child.id());
                result["idle"] = self.idle_view(id, &result["state"]);
            }
        }
        if result["state"] == "stopped" {
            if let Some(path) = self.hibernated(id) {
                result["state"] = json!("hibernated");
                let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                result["hibernation"] = json!({"path":path,"bytes":bytes});
            }
        }
        if let Some(reason) = self.invalid.get(id) {
            // Never running: `start` refuses it and nothing was running when the registry was loaded.
            result["state"] = json!("invalid");
            result["invalid_reason"] = json!(reason);
        }
        Ok(result)
    }
    pub(crate) fn dispatch(
        &mut self,
        request: Value,
        deferred: &mut Option<Completion>,
    ) -> Result<Value> {
        let method = request["method"].as_str().context("missing method")?;
        let args = &request["arguments"];
        if let Some(value) = self.snapshot_restore_dispatch(method, args, deferred)? {
            return Ok(value);
        }
        if let Some(value) = self.snapshot_dispatch(method, args, deferred)? {
            return Ok(value);
        }
        if let Some(id) = args["id"].as_str() {
            if self.snapshot_ops.active_vm(id)
                && !matches!(
                    method,
                    "status" | "capabilities" | "snapshot_restore_start" | "log_read"
                )
            {
                bail!("VM is fenced by an active or unresolved snapshot operation")
            }
        }
        if let Some(value) = self.idle_dispatch(method, args, deferred)? {
            return Ok(value);
        }
        let guest_shutdown = method == "shutdown" && shutdown_mode(args)? == "guest";
        if method == "session_end" {
            let owner = args["owner"].as_str().context("missing session owner")?;
            if owner.len() != 64 || !owner.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("invalid session owner")
            }
            *deferred = Some(Completion::CancelJobs {
                dir: self.dir.clone(),
                file_sessions: self
                    .file_sessions
                    .remove(owner)
                    .map(|(_, vms)| vms)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|vm| json!({"id":vm,"owner":owner}))
                    .collect(),
                response: json!({"closed":true}),
                jobs: self.jobs.lock().unwrap().owned_running(
                    owner,
                    None,
                    std::time::Instant::now(),
                ),
            });
            return Ok(Value::Null);
        }
        if method == "events" {
            return self.events.since(args);
        }
        if method == "list" {
            return Ok(Value::Array(
                self.configs
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .iter()
                    .map(|id| self.view(id))
                    .collect::<Result<Vec<_>>>()?,
            ));
        }
        if method == "capabilities" {
            let assigned = self.assigned_memory_mib()?;
            let mut qemu_capabilities = backend::capabilities(Backend::QemuHvf);
            qemu_capabilities["native_window"] =
                json!(backend::window_supported(&self.qemu_window));
            qemu_capabilities["native_window_details"] = self.qemu_window.clone();
            qemu_capabilities["input"] = json!(self
                .qemu
                .as_ref()
                .is_some_and(|q| q.console_broker.is_some()));
            qemu_capabilities["screenshot"] =
                json!(self.qemu.as_ref().is_some_and(|q| q.has_screenshot()));
            let mut report = json!({"resource_admission":{"physical_memory_mib":self.memory.physical_memory_mib,"vm_memory_budget_mib":self.memory.vm_memory_budget_mib,"active_assigned_memory_mib":assigned,"remaining_budget_mib":self.memory.vm_memory_budget_mib.saturating_sub(assigned),"explicitly_configured":self.memory.explicitly_configured,"scope":"assigned guest RAM in this daemon; excludes VM overhead, other daemons and unrelated host workloads; not a host pressure guarantee"},"backends":{"apple-virtualization":backend::capabilities(Backend::AppleVirtualization),"qemu-hvf":{"registered":self.qemu.is_some(),"capabilities":qemu_capabilities}},"backend":"apple-virtualization","guests":["linux","macos"],"windows":false,"window_enumeration":"macOS Guest --allow-observe with existing Screen Recording permission and eligible console; no titles/AX tree/input authority; other backends unsupported","lifecycle":true,"native_window":true,"screenshot":true,"screenshot_requirements":"macOS Guest --allow-observe and existing Guest Screen Recording permission, registered QEMU capture, or an Apple Virtualization Linux desktop container","input":true,"input_requirements":"registered QEMU persistent console, macOS Guest --allow-observe --allow-input with existing Guest Screen Recording/Accessibility permissions, or an Apple Virtualization Linux desktop container; independent MCP input permission and a recent matching screenshot","screen_wait":"every screenshot backend; changed regions on Apple Virtualization Linux desktops","screen_text":"every screenshot backend; host Apple Vision through the registered vm-vision helper","desktop_windows":"Apple Virtualization Linux desktop containers through the guest agent (X11/EWMH), veynodevm macOS containers through the Accessibility API over SSH, and Windows machines through Win32 in the desktop user's session over SSH: state and apps observe; window actions and app launch need input permission","accessibility":"Apple Virtualization Linux desktop containers whose image runs at-spi2 (AT-SPI over the desktop's accessibility bus), veynodevm macOS containers (the macOS Accessibility API over SSH, granted at first boot) and Windows machines (UI Automation over SSH): tree and find observe; element actions need input permission","command_execution":"Apple Linux backend requires enabled async Guest jobs; macOS legacy exec is not exposed; QEMU requires explicit qemu_guest_exec v2 and rootless cgroup-v2 Guest service; Windows machines run vm_exec over their OpenSSH Server (machine ssh-key)","async_commands":"Apple Linux backend requires exec_start/exec_poll/exec_cancel support; jobs are bound to their VM, run under a guest timeout and are killed with their process group when they time out, are cancelled or their MCP session ends; QEMU default observer transport has no command capability","file_transfer":"Apple backend requires enabled Guest files integration: Linux dedicated non-root files account or macOS opt-in same-UID private root; QEMU private serial transports have no file capability, Windows machines transfer over their OpenSSH Server; query vm_guest_info for current capabilities and file_policy; single filenames, 8MiB max","remote_desktop":false,"guest_integration":"Apple backend uses enabled Linux/macOS host-only vsock service; QEMU private virtio-serial defaults to observer-only v1; explicit qemu_guest_exec selects v2 requiring rootless cgroup-v2 Guest execution; query vm_guest_info for actual availability and capabilities","guest_shutdown":"Apple Linux integration requires explicit shutdown enablement; QEMU guest-service shutdown is unsupported and QMP power-button requests remain separate; request acceptance is not shutdown completion","gpu":"requires guest measurements; no performance claim"});
            // Added fields only: every pre-existing key keeps its name, type and meaning.
            report["backends"]["apple-virtualization"]["product_capabilities"] = json!({
                "macos":product_capabilities(Backend::AppleVirtualization, "macos"),
                "linux":product_capabilities(Backend::AppleVirtualization, "linux"),
            });
            report["backends"]["qemu-hvf"]["product_capabilities"] = json!({
                "linux":product_capabilities(Backend::QemuHvf, "linux"),
                "windows":product_capabilities(Backend::QemuHvf, "windows"),
            });
            report["backends"]["apple-virtualization"]["control_features"] = json!({
                "macos":control_features(Backend::AppleVirtualization, "macos"),
                "linux":control_features(Backend::AppleVirtualization, "linux"),
            });
            // Added key under the Apple backend only, and only for the macOS Guest.
            report["backends"]["apple-virtualization"]["known_limits"] = json!({
                "macos":known_limits(Backend::AppleVirtualization, "macos"),
            });
            report["backends"]["qemu-hvf"]["control_features"] = json!({
                "linux":control_features(Backend::QemuHvf, "linux"),
                "macos":control_features(Backend::QemuHvf, "macos"),
                "windows":control_features(Backend::QemuHvf, "windows"),
            });
            return Ok(report);
        }
        if method == "create" {
            let config: Config = serde_json::from_value(args.clone())?;
            validate_new_vm_id(&config.id)?;
            self.admissible(&config)?;
            if self.configs.contains_key(&config.id) {
                bail!("VM id already exists")
            }
            let mut next = self.configs.clone();
            next.insert(config.id.clone(), config.clone());
            atomic_json(&self.dir.join("registry.json"), &next)?;
            self.configs = next;
            return self.view(&config.id);
        }
        let id = args["id"].as_str().context("missing VM id")?;
        validate_id(id)?;
        if !self.configs.contains_key(id) {
            bail!("VM not found")
        }
        if method == "log_read" {
            return self.log_read(id, args);
        }
        if method == "control_lease" {
            return self.control_lease(id, args);
        }
        if method.starts_with("input_") || desktop_tools::acts(method) {
            if let Err(wait) = self.leases.acquire(id, std::time::Instant::now()) {
                bail!("{}", lease_refusal(wait))
            }
        }
        if method == "driver_rpc" {
            return self.driver_rpc(id, args, deferred);
        }
        if method == "delete" {
            if args["confirm"].as_str() != Some(id) {
                bail!("delete requires confirm equal to the VM id")
            }
            if let Some(run) = self.running.get_mut(id) {
                if run.poll_exit()?.is_none() {
                    bail!("VM is running; stop it before delete")
                }
                run.publish_exit(id, &self.events);
            }
            let mut next = self.configs.clone();
            let removed = next.remove(id).context("VM not found")?;
            atomic_json(&self.dir.join("registry.json"), &next)?;
            self.configs = next;
            self.running.remove(id);
            self.driver_links.lock().unwrap().remove(id);
            self.leases.forget(id);
            self.idle.remove(id);
            // Disks are owned by the image store, not the registry; report them for collection.
            return Ok(json!({"deleted":true,"id":id,"disk":removed.disk,"disk_removed":false}));
        }
        if method == "windows" {
            if self.configs[id].backend != Backend::AppleVirtualization
                || self.configs[id].guest != "macos"
            {
                bail!("unsupported: window enumeration requires enabled macOS Guest observation service")
            }
            if args
                .as_object()
                .is_none_or(|a| a.len() != 1 || !a.contains_key("id"))
            {
                bail!("windows accepts only VM id; titles and input authority are not available")
            }
        }
        if method == "guest_stream_status" {
            if self.configs[id].backend != Backend::AppleVirtualization
                || self.configs[id].guest != "macos"
            {
                bail!("unsupported: stream probe requires a macOS Guest on Apple Virtualization")
            }
            if args
                .as_object()
                .is_none_or(|a| a.len() != 1 || !a.contains_key("id"))
            {
                bail!("guest_stream_status accepts only VM id")
            }
        }
        // Windows guests run commands and files over their SSH server below.
        let windows_ssh = self.configs[id].guest == "windows"
            && (method.starts_with("exec") || method.starts_with("file_"));
        if self.configs[id].backend == Backend::QemuHvf
            && !windows_ssh
            && (guest_shutdown
                || (method.starts_with("exec") && !self.configs[id].qemu_guest_exec)
                || method.starts_with("file_")
                || method == "serial_write")
        {
            bail!("unsupported: QEMU/HVF guest integration is observer-only ({method})")
        }
        if [
            "start_guest_stream_bridge",
            "stop_guest_stream_bridge",
            "guest_stream_bridge_binding",
        ]
        .contains(&method)
        {
            validate_local_stream_bridge(
                method,
                args,
                self.configs[id].backend,
                &self.configs[id].guest,
            )?;
        }
        let mut desktop_events = None;
        if method == "desktop_input" {
            let apple_stream = self.configs[id].backend == Backend::AppleVirtualization
                && matches!(self.configs[id].guest.as_str(), "linux" | "macos");
            if self.configs[id].backend != Backend::QemuHvf && !apple_stream {
                bail!("unsupported: desktop_input needs a Linux desktop on QEMU/HVF or Apple Virtualization, or a macOS desktop")
            }
            if args
                .as_object()
                .is_none_or(|a| a.len() != 2 || !a.contains_key("id") || !a.contains_key("events"))
            {
                bail!("desktop_input accepts only id and events")
            }
            let events = desktop_input::parse(&args["events"])?;
            if apple_stream {
                // The guest agent's uinput devices (Linux) and the Screen
                // Sharing session (macOS) are reached through the caller's
                // desktop stream.
                return Ok(json!({"sent":false,"inject":"stream"}));
            }
            desktop_events = Some(desktop_input::qmp(&events)?);
        }
        let now = std::time::Instant::now();
        if self
            .running
            .get(id)
            .is_some_and(|run| run.execution_quiescing)
            && (method.starts_with("exec")
                || method == "input_step"
                || method == input::TEXT_STEP_METHOD
                || desktop_tools::acts(method))
        {
            bail!("VM execution is quiescing for shutdown")
        }
        let windows_vm =
            self.configs[id].backend == Backend::QemuHvf && self.configs[id].guest == "windows";
        if windows_vm && ["exec_start", "exec_poll", "exec_cancel"].contains(&method) {
            bail!("unsupported: Windows guests run commands with vm_exec only (over SSH, up to 30 s); there are no asynchronous jobs")
        }
        if method == "exec" && windows_vm {
            exec_timeout_ms(args, SYNC_EXEC_MAX_TIMEOUT_MS)?;
            if !exec_identity(args)?.is_empty() {
                bail!("unsupported: user, env and workdir apply to Linux container commands")
            }
            let argv = args["argv"].as_array().map_or(0, Vec::len);
            if !(1..=128).contains(&argv) {
                bail!("argv must have 1..128 items")
            }
            *deferred = Some(self.windows_call(id, WindowsCall::Exec(args.clone()))?);
            return Ok(Value::Null);
        }
        if method == "exec" {
            exec_timeout_ms(args, SYNC_EXEC_MAX_TIMEOUT_MS)?;
            *deferred = Some(Completion::Synchronous {
                dir: self.dir.clone(),
                arguments: args.clone(),
            });
            return Ok(Value::Null);
        }
        if ["exec_poll", "exec_cancel"].contains(&method) {
            self.jobs.lock().unwrap().known(
                id,
                args["job_id"].as_str().context("missing job_id")?,
                now,
            )?;
        }
        // Apple Virtualization Guests take input through their own service:
        // macOS through Screen Sharing, Linux desktops through the agent.
        let apple_guest = self.configs[id].backend == Backend::AppleVirtualization
            && ["macos", "linux"].contains(&self.configs[id].guest.as_str());
        let guest_input = method.starts_with("input_") && apple_guest;
        if method.starts_with("input_") {
            // Before any console check: the refusal depends only on the registered VM.
            if (method == "input_text" || method == input::TEXT_STEP_METHOD)
                && (self.configs[id].backend != Backend::QemuHvf
                    || self.configs[id].guest != "linux"
                    || !self.configs[id].qemu_guest_exec)
            {
                bail!("{}", input::TEXT_UNSUPPORTED_VM)
            }
            if !guest_input
                && (self.configs[id].backend != Backend::QemuHvf
                    || self
                        .qemu
                        .as_ref()
                        .is_none_or(|q| q.console_broker.is_none()))
            {
                bail!("unsupported: desktop input requires QEMU persistent console, a macOS Guest input service or an Apple Virtualization Linux desktop")
            }
            if method == "input_text" {
                input::plan_text(args)?;
                *deferred = Some(Completion::Input {
                    dir: self.dir.clone(),
                    arguments: args.clone(),
                    method: method.into(),
                    evdev: false,
                });
                return Ok(Value::Null);
            }
            if ["input_click", "input_drag", "input_scroll", "input_key"].contains(&method) {
                let arguments = input::source_pixel_arguments(method, args)?.into_owned();
                input::plan(method, &arguments)?;
                *deferred = Some(Completion::Input {
                    dir: self.dir.clone(),
                    arguments,
                    method: method.into(),
                    evdev: apple_guest && self.configs[id].guest == "linux",
                });
                return Ok(Value::Null);
            }
        }
        let mut desktop_arguments = None;
        let windows_vm =
            self.configs[id].backend == Backend::QemuHvf && self.configs[id].guest == "windows";
        if desktop_tools::METHODS.contains(&method) {
            let macos = self.configs[id].guest == "macos";
            if windows_vm {
                let arguments = desktop_tools::arguments(method, args)?;
                *deferred = Some(self.windows_call(
                    id,
                    WindowsCall::Desktop(desktop_tools::guest_method(method).into(), arguments),
                )?);
                return Ok(Value::Null);
            }
            if self.configs[id].backend != Backend::AppleVirtualization
                || !(macos || self.configs[id].guest == "linux")
            {
                bail!("unsupported: desktop window and application tools require an Apple Virtualization Linux desktop, a macOS container or a Windows machine with SSH; on a QEMU Linux desktop whose image ships cua-driver use the cua tools (cua_list_windows, cua_get_window_state)")
            }
            let arguments = desktop_tools::arguments(method, args)?;
            if macos {
                *deferred = Some(Completion::MacDesktop {
                    dir: self.dir.clone(),
                    vm: id.into(),
                    method: desktop_tools::guest_method(method).into(),
                    arguments,
                });
                return Ok(Value::Null);
            }
            desktop_arguments = Some(arguments);
        }
        let guest_screenshot = method == "screenshot" && apple_guest;
        let qemu_capture = self.configs[id].backend == Backend::QemuHvf
            && self.qemu.as_ref().is_some_and(|q| q.has_screenshot());
        if method == "screenshot" && !guest_screenshot && !qemu_capture {
            bail!("unsupported: screenshot requires QEMU capture, a macOS Guest observation service or an Apple Virtualization Linux desktop")
        }
        if method == "frame_status" && !qemu_capture {
            bail!("unsupported: frame_status requires QEMU console capture")
        }
        let mut guest_wait = None;
        if method == "screen_wait" {
            let arguments = screen_wait::arguments(args)?;
            if self.configs[id].backend == Backend::AppleVirtualization
                && self.configs[id].guest == "linux"
            {
                guest_wait = Some(arguments);
            } else {
                let probe = if qemu_capture {
                    screen_wait::Probe::Sequence
                } else if apple_guest {
                    screen_wait::Probe::Digest
                } else {
                    bail!("unsupported: screen_wait requires a screenshot-capable desktop")
                };
                let baseline = self
                    .baselines
                    .lock()
                    .unwrap()
                    .get(id, &arguments["frame_id"]);
                *deferred = Some(Completion::ScreenWait {
                    dir: self.dir.clone(),
                    vm: id.into(),
                    probe,
                    arguments,
                    baseline,
                });
                return Ok(Value::Null);
            }
        }
        // The scaled-JPEG form (decision 0025 item 2). No such field: `None`, and nothing below differs from before.
        let scaled_screenshot = if method == "screenshot" {
            let scaled = screenshot_scaled::Scaled::parse(args)?;
            if scaled.is_some()
                && (guest_screenshot
                    || self
                        .qemu
                        .as_ref()
                        .is_none_or(|q| q.console_broker.is_none()))
            {
                return Err(screenshot_scaled::Refused::new(
                    screenshot_scaled::Refusal::FormatUnavailable,
                    "unsupported: the scaled JPEG screenshot form requires the QEMU console broker; request the screenshot without format, max_width, max_height and quality",
                )
                .into());
            }
            scaled
        } else {
            None
        };
        if method == "status" {
            return self.view(id);
        }
        if method == "start" || method == snapshot_restore::START {
            if let Some(reason) = self.invalid.get(id) {
                bail!("VM configuration failed validation at registry load and cannot be started: {reason}")
            }
            let restore_plan = if method == snapshot_restore::START {
                Some(snapshot_restore::start_plan(self, args)?)
            } else {
                None
            };
            let config = match &restore_plan {
                Some(plan) => plan.config.clone(),
                None => self.configs.get(id).context("VM not found")?.clone(),
            };
            self.admissible(&config)?;
            if let Some(run) = self.running.get_mut(id) {
                if run.poll_exit()?.is_none() {
                    bail!("VM process already active")
                }
            }
            let idle_after = idle::policy(args)?;
            let hibernate_after = idle::hibernate_policy(args, &config)?;
            let hibernation = self.hibernated(id);
            let assigned = self.assigned_memory_mib()?;
            self.memory.admit(assigned, config.memory_mib)?;
            let disk_identity = config.disk.as_deref().map(DiskIdentity::read).transpose()?;
            if let Some(identity) = disk_identity {
                for (other, runtime) in &mut self.running {
                    if other != id
                        && runtime.poll_exit()?.is_none()
                        && runtime.disk_identity == Some(identity)
                    {
                        bail!("writable disk inode is already attached to another running VM (including hard-link aliases)")
                    }
                }
            }
            // Key-event logging directory: checked before any state of this start is touched, and re-checked on
            // every start because the directory may have changed since the daemon started. The canonical path is
            // what the worker receives.
            if config.backend == Backend::AppleVirtualization
                && self.worker_key_event_log_dir.is_some()
            {
                let canonical = validate_worker_key_event_log_dir(
                    self.worker_key_event_log_dir.as_ref().unwrap(),
                )?;
                self.worker_key_event_log_dir = Some(canonical);
            }
            self.jobs.lock().unwrap().clear_vm(id);
            let config_path = if let Some(plan) = &restore_plan {
                // Prepared privately and durably by the operation thread. The
                // selected registry and ordinary start config stay untouched.
                plan.config_path.clone()
            } else {
                let path = self.dir.join(format!("{id}.json"));
                write_vm_config(&self.dir, &path, &config)?;
                path
            };
            let mut command = match config.backend {
                Backend::AppleVirtualization => Command::new(&self.backend),
                Backend::QemuHvf => backend::require_registration(&self.qemu)?.command(),
            };
            let bridge = Arc::new(Mutex::new(bridge::State::new()?));
            let worker_instance = bridge.lock().unwrap().worker_instance.clone();
            let unix_seconds = if config.backend == Backend::AppleVirtualization
                && self.worker_key_event_log_dir.is_some()
            {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs()
            } else {
                0
            };
            let (start_args, key_event_log_path) = worker_start_args(
                config.backend,
                &config_path,
                self.worker_key_event_log_dir.as_deref(),
                id,
                unix_seconds,
                &worker_instance,
            );
            if config.backend == Backend::QemuHvf
                && self.qemu_window["native_input_supported"] == true
            {
                command
                    .arg("--control-socket")
                    .arg(self.dir.join("control.sock"))
                    .arg("--vm-id")
                    .arg(id)
                    .arg("--worker-instance")
                    .arg(&bridge.lock().unwrap().worker_instance);
            }
            let machine_state = match args.get("restore_machine_state") {
                Some(path) if restore_plan.is_none() => {
                    Some(container_state::restore_path(&config, path)?)
                }
                Some(_) => {
                    bail!("restore_machine_state cannot be combined with a snapshot restore")
                }
                // A hibernated VM resumes where it was.
                None if restore_plan.is_none() => hibernation
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                None => None,
            };
            let restoring = hibernation.is_some()
                && restore_plan.is_none()
                && args.get("restore_machine_state").is_none();
            if let Some(path) = hibernation.as_ref().filter(|_| !restoring) {
                // Another state replaces the disk the hibernated one belongs to.
                fs::remove_file(path).context("discarding the hibernation file")?;
            }
            snapshot_restore::append_start_arguments(
                &mut command,
                &start_args,
                restore_plan
                    .as_ref()
                    .map(|plan| plan.worker_arguments())
                    .or(machine_state
                        .map(|path| vec!["--restore-machine-state".into(), path.into()])),
            );
            let mut child = command
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("starting VM backend")?;
            if let Some(path) = key_event_log_path {
                log_line(&format!(
                    "vm-control: worker key-event logging enabled for {id}: {path:?}"
                ));
                self.worker_key_event_logs
                    .insert(id.into(), (worker_instance, path));
            } else {
                self.worker_key_event_logs.remove(id);
            }
            drop(command);
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            let state = Arc::new(Mutex::new(json!({"state":"starting"})));
            let guest_async_timeout = Arc::new(Mutex::new(GuestAsyncTimeout::default()));
            let pending: Arc<Mutex<BTreeMap<u64, PendingReply>>> =
                Arc::new(Mutex::new(BTreeMap::new()));
            let output_state = state.clone();
            let output_async_timeout = guest_async_timeout.clone();
            let output_pending = pending.clone();
            let output_events = (self.events.clone(), id.to_string());
            let output_leases = self.leases.clone();
            let restore_expected = restore_plan.as_ref().map(|plan| {
                (
                    plan.config.id.clone(),
                    plan.capture_id.clone(),
                    plan.config_sha256.clone(),
                )
            });
            thread::spawn(move || {
                let mut reader = BufReader::with_capacity(LARGE_REPLY_READ_BUFFER, stdout);
                let mut restored_receipt: Option<Value> = None;
                // The worker's timing record for the screenshot reply that follows it: request number, stages,
                // and when it was read. Only unsigned integers are kept from the worker.
                let mut shot: Option<(Option<u64>, [u64; 4], std::time::Instant)> = None;
                loop {
                    let frame = read_frame_bounded(&mut reader, MAX_SCREENSHOT_RESPONSE);
                    let read = std::time::Instant::now();
                    match frame {
                        Ok(Some(bytes)) => match serde_json::from_slice::<Value>(&bytes) {
                            Ok(mut value) => {
                                let parsed = std::time::Instant::now();
                                if validate_backend_response_bound(
                                    &value,
                                    bytes.len() + 1,
                                    &output_pending.lock().unwrap(),
                                )
                                .is_err()
                                {
                                    replace_runtime_state(
                                        &mut output_state.lock().unwrap(),
                                        json!({"state":"error","error":"backend output exceeded operation response bound"}),
                                    );
                                    break;
                                }
                                if value["event"] == "snapshot_restored" {
                                    let valid = restore_expected.as_ref().is_some_and(
                                        |(vm, capture, configuration)| {
                                            value["vm_id"].as_str() == Some(vm.as_str())
                                                && value["capture_id"].as_str()
                                                    == Some(capture.as_str())
                                                && value["configuration_sha256"].as_str()
                                                    == Some(configuration.as_str())
                                                && value["state"] == "paused"
                                                && value["restored"] == true
                                        },
                                    );
                                    if !valid {
                                        replace_runtime_state(
                                            &mut output_state.lock().unwrap(),
                                            json!({"state":"error","error":"unbound saved-state restore receipt"}),
                                        );
                                        break;
                                    }
                                    restored_receipt = Some(value.clone());
                                }
                                if value["event"] == "state" {
                                    output_async_timeout
                                        .lock()
                                        .unwrap()
                                        .observe_worker_state(&value["state"]);
                                    output_events.0.worker_state(&output_events.1, &value);
                                    let mut observed_state = output_state.lock().unwrap();
                                    replace_runtime_state(&mut observed_state, value.clone());
                                    if let Some(receipt) = &restored_receipt {
                                        observed_state["snapshot_restore"] = receipt.clone();
                                    }
                                }
                                if let Some(change) = match value["event"].as_str() {
                                    Some("window_opened" | "window_closed") => output_leases
                                        .viewer(
                                            &output_events.1,
                                            "window",
                                            value["event"] == "window_opened",
                                            std::time::Instant::now(),
                                        ),
                                    Some("person_input") => output_leases
                                        .human_input(&output_events.1, std::time::Instant::now()),
                                    _ => None,
                                } {
                                    let holder = match change {
                                        control_lease::Change::PersonTookControl => "person",
                                        control_lease::Change::AgentMayAct => "agent",
                                    };
                                    output_events.0.publish(
                                        &output_events.1,
                                        "control",
                                        json!({"state":holder}),
                                    );
                                }
                                if value["event"] == "screenshot_timing" {
                                    let stages = SHOT_WORKER_STAGES
                                        .map(|(name, _)| value[name].as_u64().unwrap_or(0));
                                    shot = Some((value["request"].as_u64(), stages, parsed));
                                    continue;
                                }
                                if let Some(id) = value["id"].as_u64() {
                                    // In-process note beside `result`, read by `Pending::wait` and never forwarded.
                                    if let Some((_, stages, seen)) =
                                        shot.take().filter(|(request, ..)| *request == Some(id))
                                    {
                                        let mut note = json!({"pipe_us":read.duration_since(seen).as_micros() as u64,"parse_us":parsed.duration_since(read).as_micros() as u64,"bytes":bytes.len()});
                                        for ((_, name), stage) in
                                            SHOT_WORKER_STAGES.iter().zip(stages)
                                        {
                                            note[*name] = json!(stage);
                                        }
                                        value["screenshot_timing"] = note;
                                    }
                                    if let Some(tx) = output_pending.lock().unwrap().remove(&id) {
                                        let _ = tx.send(value);
                                    }
                                }
                            }
                            Err(_) => {
                                replace_runtime_state(
                                    &mut output_state.lock().unwrap(),
                                    json!({"state":"error","error":"backend emitted invalid JSON"}),
                                );
                                break;
                            }
                        },
                        Ok(None) => break,
                        Err(_) => {
                            replace_runtime_state(
                                &mut output_state.lock().unwrap(),
                                json!({"state":"error","error":"backend output exceeded protocol bound"}),
                            );
                            break;
                        }
                    }
                }
                output_async_timeout.lock().unwrap().invalidate();
                output_pending.lock().unwrap().clear();
            });
            // A new VM process creates a new Guest service lifetime. Do not carry
            // cleanup-required state from the previous, already exited Guest.
            self.started(
                id.into(),
                Running {
                    bridge,
                    child,
                    assigned_memory_mib: config.memory_mib,
                    expected_exit: false,
                    execution_quiescing: false,
                    guest_async_timeout,
                    exit_status: None,
                    disk_identity,
                    stdin,
                    state,
                    pending,
                    next: 1,
                },
            );
            if let Some(plan) = &restore_plan {
                plan.attach_worker(self.running.get_mut(id).unwrap())?;
            }
            self.idle_started(id, args, idle_after, hibernate_after, restoring)?;
            return self.view(id);
        }
        if method == "force_stop" {
            if args["confirm"] != id {
                bail!("force stop risks guest data loss; confirm must exactly equal VM id")
            }
            let run = self.running.get_mut(id).context("VM process not active")?;
            run.force_stop()?;
            replace_runtime_state(
                &mut run.state.lock().unwrap(),
                json!({"state":"stopped","reason":"force_stopped"}),
            );
            return Ok(json!({"state":"stopped","forced":true}));
        }
        let file_method = [
            "file_upload_begin",
            "file_upload_write",
            "file_upload_commit",
            "file_download_begin",
            "file_download_read",
            "file_cancel",
            "file_session_end",
        ]
        .contains(&method);
        if file_method {
            let config = self.configs.get(id).unwrap();
            let windows = config.backend == Backend::QemuHvf && config.guest == "windows";
            if !windows && !["linux", "macos"].contains(&config.guest.as_str()) {
                bail!("unsupported: file integration requires enabled Linux or macOS Guest service, or a Windows machine with SSH")
            }
            let owner = args["owner"]
                .as_str()
                .context("file access requires session owner")?;
            if owner.len() != 64
                || !owner
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                bail!("invalid file session owner")
            }
            if method != "file_session_end" {
                // Guest expiry is not cleanup evidence: a failed/stopped files
                // worker may retain staging indefinitely. Preserve this route
                // until the session's first cleanup owns its result; never evict
                // it to admit another owner. Adapters retain that first result.
                if !self.file_sessions.contains_key(owner) && self.file_sessions.len() >= 256 {
                    bail!("file session capacity reached; outstanding sessions require cleanup")
                }
                let entry = self
                    .file_sessions
                    .entry(owner.into())
                    .or_insert_with(|| (now, BTreeSet::new()));
                entry.0 = now;
                entry.1.insert(id.into());
            }
            let config = &self.configs[id];
            if config.backend == Backend::QemuHvf && config.guest == "windows" {
                return self.windows_file(id, method, args, deferred);
            }
        }
        if file_method
            || [
                "shutdown",
                "show_window",
                "close_window",
                "window_status",
                "serial_write",
                "screenshot",
                "start_guest_stream_bridge",
                "stop_guest_stream_bridge",
                "guest_stream_bridge_binding",
                "input_step",
                input::TEXT_STEP_METHOD,
                "input_cleanup",
                "desktop_input",
                "frame_status",
                "screen_wait",
                "desktop_state",
                "desktop_apps",
                "window_action",
                "app_launch",
                "accessibility_tree",
                "accessibility_find",
                "accessibility_action",
                "guest_info",
                "guest_stream_status",
                "windows",
                "exec_start",
                "exec_poll",
                "exec_cancel",
                container_state::SAVE,
                container_state::CLOCK_SYNC,
            ]
            .contains(&method)
        {
            let run = self.running.get_mut(id).context("VM process not active")?;
            if run.poll_exit()?.is_some() {
                bail!("VM process has exited")
            }
            if method == "exec_start" {
                let timeout = exec_timeout_ms(args, ASYNC_EXEC_MAX_TIMEOUT_MS)?;
                let maximum = if self.configs[id].backend == Backend::QemuHvf
                    && self.configs[id].guest == "linux"
                    && self.configs[id].qemu_guest_exec
                {
                    run.guest_async_timeout.lock().unwrap().maximum
                } else {
                    SYNC_EXEC_MAX_TIMEOUT_MS
                };
                if timeout > maximum {
                    bail!(
                        "timeout_ms exceeds this Guest's observed asynchronous limit {maximum}; query vm_guest_info before a longer start (unknown or reconnecting Guest limit is 30000)"
                    )
                }
            }
            if method == "guest_info" {
                // Failure or a missing/malformed policy leaves the safe legacy
                // ceiling, rather than reusing a prior successful observation.
                run.guest_async_timeout.lock().unwrap().invalidate();
            }
            if method == "shutdown" && self.configs[id].qemu_guest_exec {
                // Under the Manager lock: no exec can start after this.
                run.execution_quiescing = true;
            }
            let number = run.next;
            run.next += 1;
            let (tx, rx) = mpsc::channel();
            let mut command = json!({"id":number,"method":method});
            let mut bridge_post = None;
            if method == "stop_guest_stream_bridge" {
                run.bridge.lock().unwrap().invalidate();
            }
            if method == "start_guest_stream_bridge" || method == "guest_stream_bridge_binding" {
                let mut state = run.bridge.lock().unwrap();
                let attempt = if method == "start_guest_stream_bridge" {
                    command["socket_path"] = args["socket_path"].clone();
                    state.begin(args["socket_path"].as_str().unwrap())?
                } else {
                    state
                        .expected(
                            args["worker_instance"]
                                .as_str()
                                .context("worker_instance required")?,
                            args["bridge_instance"]
                                .as_str()
                                .context("bridge_instance required")?,
                        )?
                        .bridge_instance
                        .clone()
                };
                command["worker_instance"] = json!(state.worker_instance);
                command["bridge_instance"] = json!(attempt);
                bridge_post = Some(JobPost::Bridge {
                    state: run.bridge.clone(),
                    vm: id.into(),
                    pid: run.child.id(),
                    attempt,
                    start: method == "start_guest_stream_bridge",
                });
            }
            if let Some(events) = desktop_events.take() {
                command["arguments"] = json!({"events":events});
            }
            if method.starts_with("input_") {
                if guest_input {
                    command["method"] = json!("guest_request");
                    command["request"] =
                        json!({"id":number.to_string(),"method":method,"arguments":args});
                    if serde_json::to_vec(&command["request"])?.len() >= 65535 {
                        bail!("input request exceeds Guest 64KiB bound")
                    }
                } else {
                    command["arguments"] = args.clone();
                }
            }
            if file_method {
                command["method"] = json!("guest_request");
                let mut request = args.clone();
                request["id"] = json!(number.to_string());
                request["method"] = json!(method);
                command["request"] = request;
                if serde_json::to_vec(&command["request"])?.len() >= 65535 {
                    bail!("file request exceeds Guest 64KiB bound")
                }
            }
            if guest_shutdown {
                command["method"] = json!("guest_request");
                command["request"] = json!({"id":number.to_string(),"method":"shutdown"});
            }
            if guest_screenshot {
                command["method"] = json!("guest_request");
                command["request"] = json!({"id":number.to_string(),"method":"screenshot"});
            }
            if let Some(arguments) = guest_wait.take() {
                command["method"] = json!("guest_request");
                command["request"] =
                    json!({"id":number.to_string(),"method":"screen_wait","arguments":arguments});
            }
            if let Some(arguments) = desktop_arguments.take() {
                command["method"] = json!("guest_request");
                command["request"] = json!({"id":number.to_string(),"method":desktop_tools::guest_method(method),"arguments":arguments});
            }
            if let Some(scaled) = &scaled_screenshot {
                scaled.apply(&mut command);
            }
            if method == "windows" {
                command["method"] = json!("guest_request");
                command["request"] = json!({"id":number.to_string(),"method":"windows"});
            }
            if method == "guest_info" {
                command["method"] = json!("guest_request");
                command["request"] = json!({"id":number.to_string(),"method":"info"});
            }
            if method == "guest_stream_status" {
                command["method"] = json!("guest_stream_probe");
            }
            if method == "exec_start" {
                let argv = args["argv"].as_array().context("argv must be an array")?;
                if argv.is_empty()
                    || argv.len() > 128
                    || argv.iter().any(|v| {
                        v.as_str()
                            .is_none_or(|s| s.len() > 8192 || s.contains('\0'))
                    })
                    || serde_json::to_vec(argv)?.len() > 32768
                {
                    bail!("argv invalid or too large")
                }
                let timeout = exec_timeout_ms(args, ASYNC_EXEC_MAX_TIMEOUT_MS)?;
                command["method"] = json!("guest_request");
                command["request"] = json!({"id":number.to_string(),"method":method,"argv":argv,"timeout_ms":timeout});
                for (key, value) in exec_identity(args)? {
                    command["request"][key] = value;
                }
            }
            if ["exec_poll", "exec_cancel"].contains(&method) {
                command["method"] = json!("guest_request");
                command["request"] =
                    json!({"id":number.to_string(),"method":method,"job_id":args["job_id"]});
            }
            let post = if bridge_post.is_some() {
                bridge_post
            } else if method == "guest_stream_status" {
                Some(JobPost::GuestStreamStatus)
            } else if method == "guest_info" {
                Some(JobPost::GuestInfo {
                    backend: self.configs[id].backend,
                    guest: self.configs[id].guest.clone(),
                    state: run.state.clone(),
                    qemu_exec: self.configs[id].backend == Backend::QemuHvf
                        && self.configs[id].guest == "linux"
                        && self.configs[id].qemu_guest_exec,
                    async_timeout: run.guest_async_timeout.clone(),
                    async_generation: run.guest_async_timeout.lock().unwrap().generation,
                })
            } else if method == input::TEXT_STEP_METHOD {
                Some(JobPost::GuestText)
            } else if method.starts_with("input_") {
                Some(JobPost::GuestInput {
                    guest_wrapped: guest_input,
                    method: method.into(),
                })
            } else if method == "windows" {
                Some(JobPost::Windows)
            } else if method == "screenshot" {
                Some(JobPost::Screenshot {
                    guest_wrapped: guest_screenshot,
                    baselines: self.baselines.clone(),
                    vm: id.into(),
                    scaled: scaled_screenshot,
                })
            } else if method == "screen_wait" || desktop_tools::METHODS.contains(&method) {
                Some(JobPost::GuestResult)
            } else if method == "exec_start" {
                Some(JobPost::Start {
                    reservation: jobs::Jobs::reserve(&self.jobs, now)?,
                    vm: id.into(),
                    owner: args["owner"].as_str().unwrap_or_default().into(),
                })
            } else if method == "exec_poll" {
                Some(JobPost::Poll {
                    jobs: self.jobs.clone(),
                    vm: id.into(),
                    id: args["job_id"].as_str().unwrap().into(),
                })
            } else {
                None
            };
            if method == container_state::SAVE {
                container_state::save_command(&self.configs[id], args, &mut command)?;
            }
            if method == container_state::CLOCK_SYNC {
                command["method"] = json!("guest_request");
                command["request"] = container_state::clock_request(number)?;
            }
            if method == "serial_write" {
                let data = args["data"]
                    .as_str()
                    .context("missing base64 serial data")?;
                if data.len() > 87384 {
                    bail!("serial payload exceeds 64 KiB")
                }
                command["data"] = json!(data);
            }
            if self.configs[id].qemu_guest_exec
                && method == "exec_start"
                && run.state.lock().unwrap()["guest_integration_state"] != "ready"
            {
                bail!("guest execution service is not ready yet; retry after the Guest finishes starting")
            }
            run.pending.lock().unwrap().insert(
                number,
                PendingReply {
                    sender: tx,
                    limit: response_limit(method),
                },
            );
            let sent = write_json(&mut run.stdin, &command);
            if let Err(e) = sent {
                run.pending.lock().unwrap().remove(&number);
                return Err(e);
            }
            *deferred = Some(Completion::Backend(Pending {
                receiver: rx,
                number,
                pending: run.pending.clone(),
                timeout: Duration::from_secs(if guest_input {
                    4
                } else if method == "guest_stream_status" {
                    3
                } else if method == "screenshot" {
                    17
                } else if method == "screen_wait" {
                    30
                } else if method == "windows" {
                    12
                } else if method == container_state::SAVE {
                    container_state::SAVE_TIMEOUT_SECONDS
                } else if method.ends_with("guest_stream_bridge")
                    || method == "guest_stream_bridge_binding"
                {
                    8
                } else {
                    10
                }),
                post,
            }));
            return Ok(Value::Null);
        }
        bail!("unsupported operation: {method}")
    }
}
