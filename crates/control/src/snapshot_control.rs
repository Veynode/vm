//! Background named snapshot operations. The manager serializes admission only;
//! save, hashing and filesystem publication run on owned operation threads.
use super::*;
use crate::snapshot::{
    self, storage::Dir, Artifact, CaptureProof, Compatibility, Kind, Role, SnapshotStore,
};
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::fd::{AsRawFd, FromRawFd};

const MAX_OPERATIONS: usize = 256;
const MAX_STEPS: u32 = 32;
#[derive(Clone)]
struct UnpersistedFailure {
    code: String,
    diagnostic: String,
}
fn bounded_diagnostic(message: &str) -> String {
    let mut end = message.len().min(4096);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].into()
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureBinding {
    capture_id: String,
    configuration_sha256: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    sequence: u32,
    previous_sha256: Option<String>,
    operation_id: String,
    id: String,
    name: String,
    action: String,
    worker_instance: Option<String>,
    state: String,
    phase: String,
    result: Value,
    error_code: Option<String>,
    local_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    create_intent_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capture_binding: Option<CaptureBinding>,
    #[serde(skip)]
    source_channel_released: bool,
    #[serde(skip)]
    source_resume_pending: bool,
    #[serde(skip)]
    unpersisted_failure: Option<UnpersistedFailure>,
}
impl Record {
    fn active(&self) -> bool {
        !matches!(self.state.as_str(), "completed" | "failed")
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1 && self.sequence < MAX_STEPS,
            "invalid snapshot operation version/sequence"
        );
        validate_id(&self.id)?;
        jobs::validate_job_id(&self.operation_id)?;
        ensure!(
            matches!(self.action.as_str(), "create" | "delete" | "restore"),
            "invalid snapshot action"
        );
        ensure!(
            matches!(
                self.state.as_str(),
                "running" | "completed" | "failed" | "recovery_required"
            ),
            "invalid snapshot operation state"
        );
        ensure!(
            !self.name.is_empty()
                && self.name.len() <= 64
                && self.name.as_bytes()[0].is_ascii_alphanumeric()
                && self
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
            "invalid snapshot name"
        );
        ensure!(
            self.phase.len() <= 64
                && self
                    .phase
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "invalid snapshot phase"
        );
        if let Some(worker) = &self.worker_instance {
            ensure!(
                native_input::hex(worker, 64),
                "invalid snapshot worker instance"
            );
        }
        if let Some(digest) = &self.create_intent_sha256 {
            ensure!(
                self.action == "create"
                    && self.worker_instance.is_some()
                    && native_input::hex(digest, 64),
                "invalid snapshot create retirement binding"
            );
        }
        if let Some(binding) = &self.capture_binding {
            ensure!(
                self.action == "create"
                    && self.create_intent_sha256.is_some()
                    && jobs::validate_job_id(&binding.capture_id).is_ok()
                    && native_input::hex(&binding.configuration_sha256, 64),
                "invalid capture retirement binding"
            );
        }
        if let Some(previous) = &self.previous_sha256 {
            ensure!(
                native_input::hex(previous, 64),
                "invalid snapshot operation chain"
            );
        }
        ensure!(
            (self.sequence == 0) == self.previous_sha256.is_none(),
            "invalid snapshot operation chain origin"
        );
        ensure!(
            self.local_error.as_ref().is_none_or(|s| s.len() <= 4096),
            "snapshot local diagnostic too large"
        );
        if let Some(error) = &self.error_code {
            ensure!(
                error.len() <= 128
                    && error
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "invalid snapshot error code"
            );
        }
        Ok(())
    }
    fn digest(&self) -> Result<String> {
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }
    fn public(&self) -> Value {
        let mut value = json!({"operation_id":self.operation_id,"id":self.id,"name":self.name,"action":self.action,
            "state":self.state,"phase":self.phase,"result":self.result,"error_code":self.error_code});
        if self.source_resume_pending {
            value["source_resume_pending"] = json!(true);
        }
        if self.active() && self.source_channel_released {
            value["source_channel_released"] = json!(true);
        }
        if self.unpersisted_failure.is_some() {
            // This is explicitly a live projection, never an invented durable
            // receipt. The original record/chain and recovery fence stay intact.
            value["durable_state"] = json!(self.state);
            value["durable_phase"] = json!(self.phase);
            value["state"] = json!("recovery_required");
            value["phase"] = json!("failure_persistence_unconfirmed");
            value["error_code"] = json!("snapshot_failure_persistence_unconfirmed");
            value["failure_persisted"] = json!(false);
        }
        value
    }
}
#[derive(Clone)]
pub(crate) struct Operations {
    directory: Arc<Dir>,
    records: Arc<Mutex<BTreeMap<String, Record>>>,
}
#[derive(Clone)]
pub(crate) struct Operation {
    operations: Operations,
    id: String,
}
impl Operations {
    pub(crate) fn open(state_dir: &Path) -> Result<Self> {
        let root = Dir::open(state_dir)?;
        let (directory, _) = root.child("snapshot-operations", true)?;
        let mut records = BTreeMap::new();
        for entry in directory.names()? {
            if let Some(nonce) = entry.strip_prefix(".pending-admission-") {
                jobs::validate_job_id(nonce)?;
                // No operation ID is returned and no backend work is admitted
                // before this directory is published. Preserve interrupted
                // staging bytes for diagnosis; never reinterpret them as a VM.
                let (staging, _) = directory.child(&entry, false)?;
                staging.verify()?;
                continue;
            }
            jobs::validate_job_id(&entry)?;
            ensure!(
                records.len() < MAX_OPERATIONS,
                "snapshot operation capacity reached"
            );
            let (dir, _) = directory.child(&entry, false)?;
            let names: Vec<_> = dir
                .names()?
                .into_iter()
                .filter(|n| {
                    !n.starts_with(".pending-")
                        && !matches!(n.as_str(), "create-intent.json" | "native-retired.json")
                })
                .collect();
            ensure!(
                !names.is_empty() && names.len() <= MAX_STEPS as usize,
                "incomplete snapshot operation record"
            );
            let mut previous: Option<Record> = None;
            for (index, name) in names.iter().enumerate() {
                ensure!(
                    name == &format!("{index:04}.json"),
                    "snapshot operation receipt sequence gap"
                );
                let record: Record = dir.json(name)?;
                record.validate()?;
                ensure!(
                    record.operation_id == entry && record.sequence == index as u32,
                    "snapshot operation receipt identity mismatch"
                );
                if let Some(old) = &previous {
                    ensure!(
                        record.id == old.id
                            && record.name == old.name
                            && record.action == old.action
                            && record.worker_instance == old.worker_instance
                            && record.create_intent_sha256 == old.create_intent_sha256
                            && (record.capture_binding == old.capture_binding
                                || (old.capture_binding.is_none()
                                    && record.capture_binding.is_some()
                                    && record.phase == "capturing"))
                            && record.previous_sha256.as_deref() == Some(old.digest()?.as_str()),
                        "snapshot operation receipt chain mismatch"
                    );
                }
                previous = Some(record);
            }
            let mut record = previous.unwrap();
            if record.state == "running" {
                let previous = record.digest()?;
                record.sequence += 1;
                record.previous_sha256 = Some(previous);
                record.state = "recovery_required".into();
                record.phase = "controller_restarted".into();
                record.error_code = Some("original_worker_recovery_required".into());
                Self::write(&dir, &record)?;
            }
            ensure!(
                !records
                    .values()
                    .any(|r: &Record| r.id == record.id && r.active() && record.active()),
                "multiple active snapshot operations for VM"
            );
            records.insert(entry, record);
        }
        Ok(Self {
            directory: Arc::new(directory),
            records: Arc::new(Mutex::new(records)),
        })
    }
    fn write(directory: &Dir, record: &Record) -> Result<()> {
        record.validate()?;
        let tmp = format!(".pending-{}", snapshot::storage::nonce()?);
        directory.write_json(&tmp, record)?;
        directory.rename_new(&tmp, &format!("{:04}.json", record.sequence))?;
        Ok(())
    }
    pub(crate) fn active_vm(&self, vm: &str) -> bool {
        self.records
            .lock()
            .unwrap()
            .values()
            .any(|r| r.id == vm && r.active())
    }
    pub(crate) fn status(&self, vm: &str, id: &str) -> Result<Value> {
        jobs::validate_job_id(id)?;
        let records = self.records.lock().unwrap();
        let record = records
            .get(id)
            .filter(|r| r.id == vm)
            .context("snapshot operation not found for VM")?;
        Ok(record.public())
    }
    pub(crate) fn begin(
        &self,
        vm: &str,
        name: &str,
        action: &str,
        worker: Option<String>,
    ) -> Result<Operation> {
        self.begin_with_intent(vm, name, action, worker, None)
    }
    fn begin_create(&self, config: &Config, name: &str, worker: &str) -> Result<Operation> {
        let intent = create_recovery::Prepared::observe(config, worker)?;
        self.begin_with_intent(
            &config.id,
            name,
            "create",
            Some(worker.into()),
            Some(intent),
        )
    }
    fn begin_with_intent(
        &self,
        vm: &str,
        name: &str,
        action: &str,
        worker: Option<String>,
        intent: Option<create_recovery::Prepared>,
    ) -> Result<Operation> {
        let mut records = self.records.lock().unwrap();
        ensure!(
            records.len() < MAX_OPERATIONS,
            "snapshot operation history capacity reached; preserve and archive history explicitly"
        );
        ensure!(
            !records.values().any(|r| r.id == vm && r.active()),
            "VM has active snapshot operation or recovery obligation"
        );
        ensure!(
            records.values().filter(|r| r.active()).count() < 4,
            "snapshot operation concurrency capacity reached"
        );
        let id = snapshot::storage::nonce()?;
        let mut record = Record {
            version: 1,
            sequence: 0,
            previous_sha256: None,
            operation_id: id.clone(),
            id: vm.into(),
            name: name.into(),
            action: action.into(),
            worker_instance: worker,
            state: "running".into(),
            phase: "admitted".into(),
            result: Value::Null,
            error_code: None,
            local_error: None,
            create_intent_sha256: None,
            capture_binding: None,
            source_channel_released: false,
            source_resume_pending: false,
            unpersisted_failure: None,
        };
        record.validate()?;
        let staging_name = format!(".pending-admission-{id}");
        let (directory, created) = self.directory.child(&staging_name, true)?;
        ensure!(created, "snapshot operation nonce collision");
        if let Some(intent) = intent {
            record.create_intent_sha256 = Some(intent.publish(&directory, &record)?);
        }
        Self::write(&directory, &record)?;
        // Publish only a complete, synced first receipt. A crash before this
        // rename must not strand an empty public operation directory and make
        // every VM's controller fail to reload.
        self.directory
            .rename_child_new(&staging_name, &id, &directory)?;
        records.insert(id.clone(), record);
        Ok(Operation {
            operations: self.clone(),
            id,
        })
    }
}
impl Operation {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    fn update(
        &self,
        phase: &str,
        state: &str,
        result: Option<Value>,
        error: Option<&str>,
        local_error: Option<String>,
    ) -> Result<()> {
        self.update_bound(phase, state, result, error, local_error, None)
    }
    fn update_bound(
        &self,
        phase: &str,
        state: &str,
        result: Option<Value>,
        error: Option<&str>,
        local_error: Option<String>,
        binding: Option<CaptureBinding>,
    ) -> Result<()> {
        let mut records = self.operations.records.lock().unwrap();
        let old = records
            .get(&self.id)
            .context("snapshot operation disappeared")?;
        ensure!(old.active(), "snapshot operation already terminal");
        let mut record = old.clone();
        record.sequence += 1;
        record.previous_sha256 = Some(old.digest()?);
        record.phase = phase.into();
        record.state = state.into();
        record.error_code = error.map(Into::into);
        record.local_error = local_error;
        if phase == "source_resumed_after_failure" && record.local_error.is_none() {
            if let Some(failure) = &old.unpersisted_failure {
                record.local_error = Some(bounded_diagnostic(&format!(
                    "Original failure [{}] was not persisted before source recovery: {}",
                    failure.code, failure.diagnostic
                )));
            }
        }
        // Only a successful durable append below clears the live failure. On
        // any write/fsync/rename error the old record remains fenced and intact.
        record.unpersisted_failure = None;
        if let Some(binding) = binding {
            ensure!(
                record.action == "create"
                    && phase == "capturing"
                    && record.capture_binding.is_none(),
                "capture binding is already sealed or has the wrong phase"
            );
            record.capture_binding = Some(binding);
        }
        if let Some(result) = result {
            record.result = result;
        }
        let (directory, _) = self.operations.directory.child(&self.id, false)?;
        Self::operations_write(&directory, &record)?;
        records.insert(self.id.clone(), record);
        Ok(())
    }
    fn operations_write(directory: &Dir, record: &Record) -> Result<()> {
        Operations::write(directory, record)
    }
    fn release_source_channel(&self) {
        // Original-daemon runtime evidence only. A restart has no stdin owner
        // to resume and must use the separate durable retirement proof instead.
        if let Ok(mut records) = self.operations.records.lock() {
            if let Some(record) = records.get_mut(&self.id) {
                record.source_channel_released = true;
            }
        }
    }
    pub(crate) fn phase(&self, phase: &str) -> Result<()> {
        self.update(phase, "running", None, None, None)
    }
    fn capturing(&self, capture_id: &str, configuration_sha256: &str) -> Result<()> {
        self.update_bound(
            "capturing",
            "running",
            None,
            None,
            None,
            Some(CaptureBinding {
                capture_id: capture_id.into(),
                configuration_sha256: configuration_sha256.into(),
            }),
        )
    }
    pub(crate) fn finish(&self, result: Value) -> Result<()> {
        self.update("finished", "completed", Some(result), None, None)
    }
    pub(crate) fn fail(&self, code: &str, recovery: bool) -> Result<()> {
        self.update(
            if recovery {
                "recovery_required"
            } else {
                "failed"
            },
            if recovery {
                "recovery_required"
            } else {
                "failed"
            },
            None,
            Some(code),
            None,
        )
    }
    fn fail_error(&self, code: &str, recovery: bool, error: &anyhow::Error) -> Result<()> {
        let existing = self
            .operations
            .records
            .lock()
            .unwrap()
            .get(&self.id)
            .and_then(|r| r.unpersisted_failure.clone());
        // Preserve the first backend cause; an outer handler may otherwise
        // overwrite it with the secondary journal ENOSPC/permission error.
        let failure = existing.unwrap_or_else(|| UnpersistedFailure {
            code: code.into(),
            diagnostic: bounded_diagnostic(&format!("{error:#}")),
        });
        let result = self.update(
            if recovery {
                "recovery_required"
            } else {
                "failed"
            },
            if recovery {
                "recovery_required"
            } else {
                "failed"
            },
            None,
            Some(&failure.code),
            Some(failure.diagnostic.clone()),
        );
        if result.is_err() {
            if let Some(record) = self.operations.records.lock().unwrap().get_mut(&self.id) {
                if record.active() && record.unpersisted_failure.is_none() {
                    record.unpersisted_failure = Some(failure);
                }
            }
        }
        result
    }
}

/// The operation owns the only worker stdin writer while the Manager's full VM
/// admission fence is held. The numeric IDs are reserved before that fence opens.
pub(crate) struct WorkerChannel {
    writer: File,
    pending: Arc<Mutex<BTreeMap<u64, PendingReply>>>,
    next: u64,
    end: u64,
}
impl WorkerChannel {
    pub(crate) fn from_running(run: &mut Running) -> Result<Self> {
        ensure!(
            run.pending.lock().unwrap().is_empty(),
            "worker requests are still in flight"
        );
        let fd = unsafe { libc::fcntl(run.stdin.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        ensure!(fd >= 0, "cannot retain snapshot worker input");
        let writer = unsafe { File::from_raw_fd(fd) };
        let next = run.next;
        let end = next
            .checked_add(8)
            .context("worker request identity exhausted")?;
        run.next = end;
        Ok(Self {
            writer,
            pending: run.pending.clone(),
            next,
            end,
        })
    }
    pub(crate) fn request(
        &mut self,
        method: &str,
        mut args: Value,
        timeout: Duration,
    ) -> Result<Value> {
        ensure!(
            self.next < self.end,
            "snapshot worker request capacity exhausted"
        );
        let number = self.next;
        self.next += 1;
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(number, tx.into());
        args["id"] = json!(number);
        args["method"] = json!(method);
        if let Err(error) = write_json(&mut self.writer, &args) {
            self.pending.lock().unwrap().remove(&number);
            return Err(error);
        }
        let pending = Pending {
            receiver: rx,
            number,
            pending: self.pending.clone(),
            timeout,
            post: None,
        };
        let reply = pending.receiver.recv_timeout(pending.timeout);
        pending.pending.lock().unwrap().remove(&number);
        let mut reply =
            reply.context("snapshot backend response unavailable; retain operation fence")?;
        ensure!(
            reply["id"].as_u64() == Some(number),
            "snapshot backend response ID mismatch"
        );
        if reply.get("error").is_some() {
            bail!("snapshot worker rejected operation");
        }
        reply
            .get_mut("result")
            .map(Value::take)
            .filter(|v| v.is_object())
            .context("snapshot worker result absent")
    }
}
fn require_hex(value: &Value, key: &str) -> Result<String> {
    let result = value[key]
        .as_str()
        .context("snapshot support identity missing")?;
    ensure!(
        native_input::hex(result, 64),
        "snapshot support identity invalid"
    );
    Ok(result.into())
}
pub(crate) fn apple_compatibility(
    config: &Config,
    support: &Value,
    backend_sha256: String,
) -> Result<Compatibility> {
    ensure!(
        config.backend == Backend::AppleVirtualization
            && config.guest == "linux"
            && config.machine_identity_dir.is_some(),
        "Linux snapshot requires explicit persistent Apple machine identity"
    );
    ensure!(
        support["can_save_restore"] == true && support["support_query_passed"] == true,
        "backend save/restore support query refused configuration"
    );
    let bytes = support["disk_bytes"]
        .as_u64()
        .context("snapshot support disk extent missing")?;
    ensure!(
        (bytes > 0) == config.disk.is_some(),
        "snapshot support disk presence mismatch"
    );
    require_hex(support, "machine_identity_sha256")?;
    Ok(Compatibility {
        backend: "apple_virtualization".into(),
        backend_version: support["backend_version"]
            .as_str()
            .context("snapshot backend version absent")?
            .into(),
        backend_sha256,
        machine: support["machine"]
            .as_str()
            .context("snapshot machine descriptor absent")?
            .into(),
        architecture: support["host_architecture"]
            .as_str()
            .context("snapshot architecture absent")?
            .into(),
        cpu_model: "Apple-VZGeneric-host".into(),
        cpus: config.cpus,
        memory_mib: config.memory_mib,
        config_sha256: require_hex(support, "configuration_sha256")?,
        devices_sha256: require_hex(support, "device_configuration_sha256")?,
        disk_bytes: (bytes > 0).then_some(bytes),
    })
}
fn verify_resume(value: &Value, capture: &str) -> Result<()> {
    ensure!(
        value["capture_id"] == capture && value["state"] == "running",
        "snapshot resume is unconfirmed"
    );
    Ok(())
}
fn lock_retired_image(file: &File) -> Result<()> {
    // The cooperating sidecar is not the framework's image lock. A VZ helper
    // can outlive its worker while teardown completes; retain both locks.
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "raw disk is still owned by a backend or another image user"
    );
    Ok(())
}
fn create_snapshot(
    operation: &Operation,
    dir: &Path,
    backend: &Path,
    config: &Config,
    worker: &str,
    channel: &mut WorkerChannel,
) -> Result<()> {
    operation.phase("checking_support")?;
    let support = channel.request("snapshot_support", json!({}), Duration::from_secs(60))?;
    ensure!(
        support["capture_retirement_protocol"] == 1,
        "native backend lacks capture I/O-drain retirement protocol; capture refused before pause"
    );
    let backend_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(backend)?;
    let (_, backend_sha) = snapshot::storage::hash(&backend_file)?;
    let compatible = apple_compatibility(config, &support, backend_sha)?;
    let store = SnapshotStore::open(dir)?;
    let mut vm = store.lock_vm(&config.id)?;
    let reserve = compatible.required_reservation(Kind::FullState)?;
    let capture = vm.begin(
        &operation.operations.records.lock().unwrap()[operation.id()].name,
        Kind::FullState,
        compatible.clone(),
        reserve,
    )?;
    let capture_id = capture.capture_id().to_owned();
    let staging = capture
        .artifact_path(Role::MachineState)?
        .parent()
        .context("snapshot staging parent missing")?
        .to_path_buf();
    operation.capturing(&capture_id, &compatible.config_sha256)?;
    let retirement_directory = operation.operations.directory.path.join(operation.id());
    let response=match channel.request("snapshot_capture",json!({"capture_id":capture_id,"worker_instance":worker,"staging_dir":staging,"config_sha256":compatible.config_sha256,
        "operation_id":operation.id(),"retirement_directory":retirement_directory}),Duration::from_secs(600)) {
        Ok(value)=>value,Err(error)=>{let _=operation.fail_error("capture_outcome_unconfirmed",true,&error);return Err(error);}
    };
    if response.get("error").is_some() || response["backend_completed"] != true {
        let not_started = response["capture_id"] == capture_id
            && response["snapshot_capture_state"] == "not_started"
            && response["state"] == "running";
        let detail = response["error"]["message"]
            .as_str()
            .unwrap_or("native capture did not complete");
        operation.fail_error(
            if not_started {
                "capture_rejected_before_pause"
            } else {
                "capture_failed_recovery_required"
            },
            !not_started,
            &anyhow::anyhow!("native capture: {detail}"),
        )?;
        return Ok(());
    }
    ensure!(
        response["capture_id"] == capture_id
            && response["worker_instance"] == worker
            && response["configuration_sha256"] == compatible.config_sha256
            && response["state"] == "paused"
            && response["source_quiesced"] == true
            && response["ram_cpu_devices_saved"] == true
            && (compatible.disk_bytes.is_none()
                || response["disk_in_same_quiesced_interval"] == true),
        "snapshot capture receipt binding/consistency invalid"
    );
    let artifacts: Vec<Artifact> = serde_json::from_value(response["artifacts"].clone())?;
    let proof = CaptureProof {
        capture_id: capture_id.clone(),
        backend_receipt_sha256: require_hex(&response, "backend_receipt_sha256")?,
        source_worker_instance: Some(worker.into()),
        backend_completed: true,
        source_quiesced: true,
        ram_cpu_devices_saved: true,
        disk_in_same_quiesced_interval: response["disk_in_same_quiesced_interval"] == true,
        protects_restore_of: None,
    };
    operation.phase("committing")?;
    let metadata = match capture.commit_expected(proof, &artifacts) {
        Ok(value) => value,
        Err(error) => {
            let resumed = channel
                .request(
                    "snapshot_resume",
                    json!({"capture_id":capture_id}),
                    Duration::from_secs(60),
                )
                .and_then(|v| verify_resume(&v, &capture_id));
            let _ = operation.fail(
                if resumed.is_ok() {
                    "store_commit_failed_vm_resumed"
                } else {
                    "store_commit_failed_resume_unconfirmed"
                },
                resumed.is_err(),
            );
            return Err(error);
        }
    };
    operation.phase("resuming")?;
    let resumed = channel.request(
        "snapshot_resume",
        json!({"capture_id":capture_id}),
        Duration::from_secs(60),
    )?;
    verify_resume(&resumed, &capture_id)?;
    operation.finish(serde_json::to_value(metadata)?)
}
impl Manager {
    pub(crate) fn snapshot_dispatch(
        &mut self,
        method: &str,
        args: &Value,
        deferred: &mut Option<Completion>,
    ) -> Result<Option<Value>> {
        if let Some(value) = self.snapshot_resume_source_dispatch(method, args, deferred)? {
            return Ok(Some(value));
        }
        if let Some(value) = self.snapshot_reconcile_dispatch(method, args, deferred)? {
            return Ok(Some(value));
        }
        if !matches!(
            method,
            "snapshot_create" | "snapshot_list" | "snapshot_delete" | "snapshot_status"
        ) {
            return Ok(None);
        }
        let id = args["id"].as_str().context("snapshot VM id missing")?;
        validate_id(id)?;
        let config = self.configs.get(id).context("VM not found")?.clone();
        if method == "snapshot_status" {
            return Ok(Some(
                self.snapshot_ops.status(
                    id,
                    args["operation_id"]
                        .as_str()
                        .context("operation_id missing")?,
                )?,
            ));
        }
        if method == "snapshot_list" {
            let dir = self.dir.clone();
            let id = id.to_owned();
            let (tx, rx) = mpsc::channel();
            thread::Builder::new()
                .name("snapshot-list".into())
                .spawn(move || {
                    let result = (|| {
                        let store = SnapshotStore::open(&dir)?;
                        let vm = store.lock_vm(&id)?;
                        Ok::<_, anyhow::Error>(serde_json::to_value(vm.list()?)?)
                    })();
                    let _ = tx.send(match result {
                        Ok(value) => json!({"id":0,"result":value}),
                        Err(error) => json!({"id":0,"error":error.to_string()}),
                    });
                })?;
            *deferred = Some(Completion::Backend(Pending {
                receiver: rx,
                number: 0,
                pending: Arc::new(Mutex::new(BTreeMap::new())),
                timeout: Duration::from_secs(30),
                post: None,
            }));
            return Ok(Some(Value::Null));
        }
        let name = args["name"].as_str().context("snapshot name missing")?;
        if method == "snapshot_delete" {
            ensure!(
                args["confirm"].as_str() == Some(name),
                "snapshot delete confirm must exactly equal snapshot name"
            );
            let operation = self.snapshot_ops.begin(id, name, "delete", None)?;
            let dir = self.dir.clone();
            let vm_id = id.to_owned();
            let name = name.to_owned();
            let task = operation.clone();
            if let Err(error) =
                thread::Builder::new()
                    .name("snapshot-delete".into())
                    .spawn(move || {
                        let result = (|| {
                            let store = SnapshotStore::open(&dir)?;
                            let mut vm = store.lock_vm(&vm_id)?;
                            vm.delete(&name)?;
                            task.finish(json!({"deleted":true,"name":name}))
                        })();
                        if result.is_err() {
                            let _ = task.fail("snapshot_delete_failed", false);
                        }
                    })
            {
                let _ = operation.fail("operation_thread_unavailable", false);
                return Err(error.into());
            }
            return Ok(Some(
                json!({"id":id,"operation_id":operation.id(),"state":"running"}),
            ));
        }
        ensure!(
            args["kind"].as_str().unwrap_or("full_state") == "full_state",
            "unsupported snapshot kind; disk rollback/suspend never substitute for full_state"
        );
        ensure!(
            config.guest == "linux",
            "snapshot capture supports Linux only"
        );
        ensure!(config.backend==Backend::AppleVirtualization,"unsupported: QEMU GL devices block full-state migration; virtio-2d production save/restore is not integrated");
        ensure!(
            config.machine_identity_dir.is_some(),
            "snapshot capture requires explicit persistent machine_identity_dir"
        );
        ensure!(self.jobs.lock().unwrap().snapshot_idle(id),"Guest jobs or unregistered start replies require terminal confirmation before snapshot");
        ensure!(
            !self.file_sessions.values().any(|(_, vms)| vms.contains(id)),
            "Guest file session must be explicitly closed before snapshot"
        );
        ensure!(
            !self.native_connections.contains_key(id),
            "native input connection must be closed before snapshot"
        );
        let run = self.running.get_mut(id).context("VM process not active")?;
        ensure!(
            run.poll_exit()?.is_none() && run.state.lock().unwrap()["state"] == "running",
            "snapshot capture requires running VM"
        );
        let worker = run.bridge.lock().unwrap().worker_instance.clone();
        let mut channel = WorkerChannel::from_running(run)?;
        let operation = self.snapshot_ops.begin_create(&config, name, &worker)?;
        let dir = self.dir.clone();
        let backend = self.backend.clone();
        let task = operation.clone();
        if let Err(error) = thread::Builder::new()
            .name("snapshot-create".into())
            .spawn(move || {
                let result = create_snapshot(&task, &dir, &backend, &config, &worker, &mut channel);
                if result.is_err() {
                    let state = task
                        .operations
                        .records
                        .lock()
                        .unwrap()
                        .get(task.id())
                        .cloned();
                    if let Some(record) = state {
                        if record.state == "running" {
                            let uncertain =
                                !matches!(record.phase.as_str(), "admitted" | "checking_support");
                            let _ = task.fail_error(
                                if uncertain {
                                    "snapshot_outcome_unconfirmed"
                                } else {
                                    "snapshot_preflight_failed"
                                },
                                uncertain,
                                result.as_ref().err().unwrap(),
                            );
                        }
                    }
                }
                drop(channel);
                task.release_source_channel();
            })
        {
            let _ = operation.fail("operation_thread_unavailable", false);
            return Err(error.into());
        }
        Ok(Some(
            json!({"id":id,"operation_id":operation.id(),"state":"running"}),
        ))
    }
}

#[cfg(test)]
#[path = "snapshot_control_tests.rs"]
mod tests;

#[path = "snapshot_reconcile.rs"]
mod reconcile;

#[path = "snapshot_create_recovery.rs"]
mod create_recovery;

#[path = "snapshot_source_resume.rs"]
mod source_resume;
