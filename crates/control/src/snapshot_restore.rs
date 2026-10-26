//! Stopped Linux/Apple full-state restore transaction. The public API never
//! overwrites the selected disk: a protected original and a new private clone
//! survive every failed/ambiguous attempt. Large I/O and all backend waits run
//! on the operation thread, outside Manager's mutex.
use super::*;
use crate::snapshot::{
    self, storage::Dir, CaptureProof, Compatibility, Kind, Metadata, RestoreTarget, Role,
    SnapshotStore, VmSnapshots,
};
use crate::snapshot_control::{Operation, WorkerChannel};
use anyhow::ensure;
use sha2::{Digest, Sha256};
use std::time::Instant;

pub(crate) const START: &str = "snapshot_restore_start";
const COMMIT: &str = "__snapshot_restore_commit";

/// The Apple executable dispatches on argv[1] (`run`), before parsing any
/// options. Keep the normal start action/config first; never prepend restore
/// flags. Used at the real spawn site, including ordinary non-restore starts.
pub(crate) fn append_start_arguments(
    command: &mut Command,
    start: &[OsString],
    restore: Option<Vec<OsString>>,
) {
    command.args(start);
    if let Some(arguments) = restore {
        command.args(arguments);
    }
}

#[derive(Default)]
pub(crate) struct Starts {
    plans: BTreeMap<String, Arc<Prepared>>,
}
struct Runtime {
    channel: Option<WorkerChannel>,
    state: Arc<Mutex<Value>>,
    worker_instance: String,
    pid: u32,
}
pub(crate) struct Prepared {
    pub(crate) config: Config,
    pub(crate) config_path: PathBuf,
    pub(crate) state_path: PathBuf,
    pub(crate) state_sha256: String,
    pub(crate) config_sha256: String,
    pub(crate) capture_id: String,
    original: Config,
    operation: Operation,
    journal: Dir,
    metadata: Metadata,
    protection: Option<String>,
    _original_guard: Option<qemu_disk::DiskGuard>,
    _snapshots: VmSnapshots,
    claimed: AtomicBool,
    verified: AtomicBool,
    selected: AtomicBool,
    runtime: Mutex<Option<Runtime>>,
}
fn same_config(a: &Config, b: &Config) -> Result<bool> {
    Ok(serde_json::to_vec(a)? == serde_json::to_vec(b)?)
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
fn open_bound(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let m = file.metadata()?;
    let edge = fs::symlink_metadata(path)?;
    ensure!(
        m.is_file()
            && m.nlink() == 1
            && m.uid() == unsafe { libc::geteuid() }
            && m.dev() == edge.dev()
            && m.ino() == edge.ino(),
        "restore input is not an owned unaliased regular file"
    );
    Ok(file)
}
#[cfg(target_os = "macos")]
fn host_version() -> Result<String> {
    let mut bytes = [0u8; 256];
    let mut n = bytes.len();
    ensure!(
        unsafe {
            libc::sysctlbyname(
                c"kern.osversion".as_ptr(),
                bytes.as_mut_ptr().cast(),
                &mut n,
                std::ptr::null_mut(),
                0,
            )
        } == 0
            && n > 1
            && n <= bytes.len(),
        "cannot identify current Apple host OS build"
    );
    Ok(format!("macos-{}", std::str::from_utf8(&bytes[..n - 1])?))
}
#[cfg(not(target_os = "macos"))]
fn host_version() -> Result<String> {
    bail!("Apple full-state restore requires macOS")
}

impl Prepared {
    pub(crate) fn worker_arguments(&self) -> Vec<OsString> {
        vec![
            "--restore-state".into(),
            self.state_path.as_os_str().into(),
            "--restore-state-sha256".into(),
            self.state_sha256.clone().into(),
            "--restore-config-sha256".into(),
            self.config_sha256.clone().into(),
            "--restore-capture-id".into(),
            self.capture_id.clone().into(),
        ]
    }
    /// Called only inside the ordinary start branch, after its fresh Running
    /// exists. The operation's admission fence remains held until commit.
    pub(crate) fn attach_worker(&self, run: &mut Running) -> Result<()> {
        let mut runtime = self.runtime.lock().unwrap();
        ensure!(runtime.is_none(), "restore worker already attached");
        let channel = WorkerChannel::from_running(run)?;
        *runtime = Some(Runtime {
            channel: Some(channel),
            state: run.state.clone(),
            worker_instance: run.bridge.lock().unwrap().worker_instance.clone(),
            pid: run.child.id(),
        });
        Ok(())
    }
    fn note(&self, name: &str, phase: &str, extra: Value) -> Result<()> {
        self.journal.write_json(name,&json!({"schema":1,"operation_id":self.operation.id(),"id":self.config.id,"phase":phase,
            "capture_id":self.capture_id,"selected_disk":self.config.disk,"original_disk":self.original.disk,"details":extra}))
    }
}

/// Opaque in-memory plan lookup. No public request can supply restore paths,
/// backend flags, a disk replacement or a compatibility digest.
pub(crate) fn start_plan(manager: &mut Manager, args: &Value) -> Result<Arc<Prepared>> {
    ensure!(
        args.as_object().is_some_and(|o| o.len() == 2
            && o.contains_key("id")
            && o.contains_key("operation_id")),
        "internal restore start accepts only VM and operation identity"
    );
    let id = args["id"].as_str().context("restore VM identity missing")?;
    let op = args["operation_id"]
        .as_str()
        .context("restore operation identity missing")?;
    jobs::validate_job_id(op)?;
    let plan = manager
        .restore_starts
        .lock()
        .unwrap()
        .plans
        .get(op)
        .filter(|p| p.config.id == id)
        .cloned()
        .context("no prepared restore transaction for this VM/operation")?;
    ensure!(
        manager.snapshot_ops.active_vm(id),
        "restore operation fence absent"
    );
    ensure!(
        same_config(
            manager.configs.get(id).context("VM missing")?,
            &plan.original
        )?,
        "selected VM configuration changed before restore start"
    );
    ensure!(
        !plan.claimed.swap(true, Ordering::AcqRel),
        "restore start is already claimed; query the original operation"
    );
    Ok(plan)
}

fn prepare(
    operation: Operation,
    dir: &Path,
    backend: &Path,
    original: Config,
    name: &str,
) -> Result<Arc<Prepared>> {
    operation.phase("verifying_restore")?;
    let store = SnapshotStore::open(dir)?;
    let mut snapshots = store.lock_vm(&original.id)?;
    let metadata = snapshots
        .list()?
        .into_iter()
        .find(|m| m.name == name)
        .context("named snapshot not found")?;
    ensure!(
        metadata.kind == Kind::FullState,
        "restore requires full_state; disk rollback and suspend are distinct"
    );
    let mut compatible: Compatibility = metadata.compatibility.clone();
    ensure!(
        compatible.backend == "apple_virtualization"
            && compatible.machine == "Apple-VZGeneric-v1"
            && compatible.architecture == "aarch64"
            && compatible.cpu_model == "Apple-VZGeneric-host",
        "unsupported snapshot backend/machine/CPU"
    );
    ensure!(
        compatible.cpus == original.cpus && compatible.memory_mib == original.memory_mib,
        "snapshot CPU/RAM configuration mismatch"
    );
    ensure!(
        compatible.backend_version == host_version()?,
        "snapshot host OS build differs; same-host Apple state compatibility required"
    );
    let backend_file = open_bound(backend)?;
    let (_, current_backend) = snapshot::storage::hash(&backend_file)?;
    ensure!(
        current_backend == compatible.backend_sha256,
        "snapshot backend executable version/hash mismatch"
    );
    compatible.backend_sha256 = current_backend;
    // These hashes validate immutable source artifacts. Remaining semantic
    // inputs (actual boot bytes, persistent Generic ID/MAC, devices/OS/share)
    // are recomputed by the candidate worker before ANY restore or cold boot.
    snapshots.verify(name, &compatible)?;
    let guard = original
        .disk
        .as_deref()
        .map(qemu_disk::DiskGuard::open_apple_raw)
        .transpose()?;
    let current = original.disk.as_deref().map(open_bound).transpose()?;
    ensure!(
        current
            .as_ref()
            .map(|f| f.metadata().map(|m| m.len()))
            .transpose()?
            == compatible.disk_bytes,
        "selected disk extent differs from snapshot"
    );
    if let Some(guard) = &guard {
        guard.verify_paths()?;
    }
    let state = Dir::open(dir)?;
    let (generations, _) = state.child("restore-generations", true)?;
    let (vm, _) = generations.child(&original.id, true)?;
    let (journal, created) = vm.child(operation.id(), true)?;
    ensure!(
        created,
        "restore generation already exists; original operation requires recovery"
    );
    let bytes = compatible.disk_bytes.unwrap_or(0);
    journal.reserve(
        bytes
            .checked_mul(2)
            .context("restore generation space overflow")?,
    )?;
    let protection = if let Some(file) = &current {
        operation.phase("protecting_disk")?;
        let protect = format!("protect-{}", operation.id());
        let capture = snapshots.begin(&protect, Kind::DiskRollback, compatible.clone(), bytes)?;
        let receipt = json!({"schema":1,"operation_id":operation.id(),"id":original.id,"source_capture_id":metadata.proof.capture_id,"original_disk":original.disk,"original_retired":true});
        let proof = CaptureProof {
            capture_id: capture.capture_id().into(),
            backend_receipt_sha256: format!("{:x}", Sha256::digest(serde_json::to_vec(&receipt)?)),
            source_worker_instance: None,
            backend_completed: true,
            source_quiesced: true,
            ram_cpu_devices_saved: false,
            disk_in_same_quiesced_interval: true,
            protects_restore_of: Some(metadata.proof.capture_id.clone()),
        };
        capture.clone_disk(file)?;
        capture.commit(proof)?;
        Some(protect)
    } else {
        None
    };
    let plan = snapshots.prepare_restore(
        name,
        &compatible,
        protection.as_deref(),
        RestoreTarget {
            worker_retired: true,
            disk: guard.as_ref().zip(current.as_ref()),
        },
    )?;
    let mut candidate = original.clone();
    if let Some(disk) = &plan.disk {
        let cloned = journal.clone_disk(disk, "disk.raw", bytes)?;
        let expected = metadata
            .artifacts
            .iter()
            .find(|a| a.role == Role::Disk)
            .context("snapshot disk identity missing")?;
        ensure!(
            snapshot::storage::hash(&cloned)? == (expected.bytes, expected.sha256.clone()),
            "restored disk clone integrity mismatch"
        );
        candidate.disk = Some(journal.path.join("disk.raw"));
    }
    let saved_state = plan
        .machine_state
        .as_ref()
        .context("full snapshot state artifact missing")?;
    let state_artifact = metadata
        .artifacts
        .iter()
        .find(|a| a.role == Role::MachineState)
        .context("snapshot machine state identity missing")?;
    ensure!(
        snapshot::storage::hash(saved_state)?
            == (state_artifact.bytes, state_artifact.sha256.clone()),
        "machine-state integrity mismatch"
    );
    let state_path = dir
        .join("snapshots")
        .join(&original.id)
        .join(name)
        .join("machine.vmstate");
    drop(plan);
    candidate.validate()?;
    journal.write_json("candidate.json", &candidate)?;
    journal.write_json("intent.json",&json!({"schema":1,"operation_id":operation.id(),"id":original.id,"name":name,"capture_id":metadata.proof.capture_id,
        "original":original,"candidate":candidate,"compatibility":compatible,"protection":protection,"state_sha256":state_artifact.sha256,"state_bytes":state_artifact.bytes,
        "phase":"prepared","selected":false,"native_preload_configuration_check_required":true}))?;
    let result = Arc::new(Prepared {
        config: candidate,
        config_path: journal.path.join("candidate.json"),
        state_path,
        state_sha256: state_artifact.sha256.clone(),
        config_sha256: compatible.config_sha256,
        capture_id: metadata.proof.capture_id.clone(),
        original,
        operation,
        journal,
        metadata,
        protection,
        _original_guard: guard,
        _snapshots: snapshots,
        claimed: AtomicBool::new(false),
        verified: AtomicBool::new(false),
        selected: AtomicBool::new(false),
        runtime: Mutex::new(None),
    });
    Ok(result)
}

fn continue_candidate(plan: &Arc<Prepared>) -> Result<()> {
    let (mut channel, state, worker, pid) = {
        let mut slot = plan.runtime.lock().unwrap();
        let runtime = slot.as_mut().context("restore worker was not attached")?;
        (
            runtime
                .channel
                .take()
                .context("restore worker channel already claimed")?,
            runtime.state.clone(),
            runtime.worker_instance.clone(),
            runtime.pid,
        )
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let value = state.lock().unwrap().clone();
        ensure!(
            !matches!(
                value["state"].as_str(),
                Some("stopped" | "crashed" | "error")
            ),
            "restore candidate failed before verified pause"
        );
        if value["state"] == "paused" {
            let receipt = &value["snapshot_restore"];
            ensure!(
                receipt["restored"] == true
                    && receipt["capture_id"] == plan.capture_id
                    && receipt["configuration_sha256"] == plan.config_sha256
                    && receipt["vm_id"] == plan.config.id,
                "candidate paused without bound completed restore receipt"
            );
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "restore candidate pause/receipt deadline; original disk remains selected"
        );
        thread::sleep(Duration::from_millis(25));
    }
    plan.note(
        "0001-restored.json",
        "restored_paused",
        json!({"worker_instance":worker,"pid":pid}),
    )?;
    plan.operation.phase("checking_restored_configuration")?;
    let support = channel.request("snapshot_support", json!({}), Duration::from_secs(60))?;
    let compatible = crate::snapshot_control::apple_compatibility(
        &plan.config,
        &support,
        plan.metadata.compatibility.backend_sha256.clone(),
    )?;
    ensure!(
        compatible == plan.metadata.compatibility,
        "restored worker actual semantic configuration differs from snapshot"
    );
    plan.operation.phase("resuming_restore")?;
    let resume = channel.request(
        "snapshot_resume",
        json!({"capture_id":plan.capture_id}),
        Duration::from_secs(60),
    )?;
    ensure!(
        resume["capture_id"] == plan.capture_id
            && resume["state"] == "running"
            && resume["resumed"] == true,
        "restored candidate resume unconfirmed"
    );
    // This is a new vsock connection and a new host request identity. It tests
    // actual service continuity; it does not claim the application's RAM oracle.
    let info = channel.request(
        "guest_request",
        json!({"request":{"id":format!("restore-{}",plan.operation.id()),"method":"info"}}),
        Duration::from_secs(15),
    )?;
    ensure!(
        info.get("error").is_none() && info["result"]["os"] == "linux",
        "fresh restored Guest integration connection unavailable"
    );
    plan.note("0002-continued.json","continued",json!({"worker_instance":worker,"pid":pid,"fresh_guest_info":true,"application_memory_oracle_verified":false}))?;
    plan.verified.store(true, Ordering::Release);
    Ok(())
}

fn run_restore(
    operation: Operation,
    dir: PathBuf,
    backend: PathBuf,
    config: Config,
    name: String,
    starts: Arc<Mutex<Starts>>,
) -> Result<()> {
    let plan = prepare(operation.clone(), &dir, &backend, config, &name)?;
    starts
        .lock()
        .unwrap()
        .plans
        .insert(operation.id().into(), plan.clone());
    operation.phase("starting_restore")?;
    let args = json!({"id":plan.config.id,"operation_id":operation.id()});
    crate::call(&dir, START, args.clone())?;
    continue_candidate(&plan)?;
    operation.phase("selecting_generation")?;
    plan.note(
        "0003-selection-intent.json",
        "selection_intent",
        json!({"fresh_guest_info":true}),
    )?;
    crate::call(&dir, COMMIT, args)?;
    plan.note(
        "0004-selected.json",
        "selected",
        json!({"registry_commit_confirmed":true}),
    )?;
    operation.finish(json!({"id":plan.config.id,"name":name,"kind":"full_state","restored":true,"guest_continuation_verified":true,
        "application_memory_oracle_verified":false,"capture_id":plan.capture_id,"protective_snapshot":plan.protection,"original_disk_retained":true}))?;
    starts.lock().unwrap().plans.remove(operation.id());
    Ok(())
}

impl Manager {
    /// Root calls before the generic snapshot admission fence. Private commit
    /// and start are bound to the same opaque, already admitted operation.
    pub(crate) fn snapshot_restore_dispatch(
        &mut self,
        method: &str,
        args: &Value,
        _deferred: &mut Option<Completion>,
    ) -> Result<Option<Value>> {
        if method == COMMIT {
            ensure!(
                args.as_object().is_some_and(|o| o.len() == 2
                    && o.contains_key("id")
                    && o.contains_key("operation_id")),
                "invalid internal restore commit"
            );
            let id = args["id"].as_str().context("restore VM identity missing")?;
            let op = args["operation_id"]
                .as_str()
                .context("restore operation identity missing")?;
            let plan = self
                .restore_starts
                .lock()
                .unwrap()
                .plans
                .get(op)
                .filter(|p| p.config.id == id)
                .cloned()
                .context("no admitted restore transaction")?;
            ensure!(
                self.snapshot_ops.active_vm(id)
                    && plan.verified.load(Ordering::Acquire)
                    && !plan.selected.load(Ordering::Acquire),
                "restore not verified or already selected"
            );
            ensure!(
                same_config(self.configs.get(id).context("VM missing")?, &plan.original)?,
                "registry configuration changed during restore"
            );
            let run = self.running.get_mut(id).context("restore worker missing")?;
            let runtime = plan.runtime.lock().unwrap();
            let runtime = runtime.as_ref().context("restore worker identity absent")?;
            ensure!(
                run.child.id() == runtime.pid
                    && run.bridge.lock().unwrap().worker_instance == runtime.worker_instance
                    && run.poll_exit()?.is_none()
                    && run.state.lock().unwrap()["state"] == "running",
                "verified restore worker changed or exited before registry selection"
            );
            let mut next = self.configs.clone();
            next.insert(id.into(), plan.config.clone());
            // The small durable registry selection is serialized with create;
            // large disk/state I/O and all native waits occurred outside here.
            atomic_json(&self.dir.join("registry.json"), &next)?;
            self.configs = next;
            plan.selected.store(true, Ordering::Release);
            return Ok(Some(json!({"selected":true,"id":id,"operation_id":op})));
        }
        if method != "snapshot_restore" {
            return Ok(None);
        }
        ensure!(
            args.as_object()
                .is_some_and(|o| o.len() == 3
                    && ["id", "name", "confirm"].iter().all(|k| o.contains_key(*k))),
            "snapshot_restore accepts exactly id, name and confirm"
        );
        let id = args["id"].as_str().context("snapshot VM id missing")?;
        validate_id(id)?;
        ensure!(
            args["confirm"] == id,
            "restore confirm must exactly equal the target VM id"
        );
        let name = args["name"]
            .as_str()
            .filter(|n| valid_name(n))
            .context("invalid snapshot name")?;
        let config = self.configs.get(id).context("VM not found")?.clone();
        ensure!(config.backend==Backend::AppleVirtualization&&config.guest=="linux"&&config.machine_identity_dir.is_some(),"full-state restore requires explicit Linux Apple persistent machine identity; QEMU GPU migration is not integrated");
        if let Some(run) = self.running.get_mut(id) {
            ensure!(run.poll_exit()?.is_some(),"normal shutdown required before snapshot restore; existing VM is not stopped or killed implicitly");
        }
        ensure!(
            self.jobs.lock().unwrap().snapshot_idle(id),
            "Guest jobs require terminal confirmation before restore"
        );
        ensure!(
            !self.file_sessions.values().any(|(_, vms)| vms.contains(id)),
            "Guest file sessions must be explicitly closed before restore"
        );
        ensure!(
            !self.native_connections.contains_key(id),
            "native input connection must be closed before restore"
        );
        let operation = self.snapshot_ops.begin(id, name, "restore", None)?;
        let task = operation.clone();
        let dir = self.dir.clone();
        let backend = self.backend.clone();
        let name = name.to_owned();
        let starts = self.restore_starts.clone();
        if let Err(error) = thread::Builder::new()
            .name("snapshot-restore".into())
            .spawn(move || {
                if let Err(error) =
                    run_restore(task.clone(), dir, backend, config, name, starts.clone())
                {
                    log_line(&format!(
                        "vm-control: snapshot restore {} failed: {error:#}",
                        task.id()
                    ));
                    let plan = starts.lock().unwrap().plans.get(task.id()).cloned();
                    let recovery = plan
                        .as_ref()
                        .is_some_and(|p| p.claimed.load(Ordering::Acquire));
                    let _ = task.fail(
                        if recovery {
                            "restore_outcome_unconfirmed"
                        } else {
                            "restore_preflight_failed"
                        },
                        recovery,
                    );
                    if !recovery {
                        starts.lock().unwrap().plans.remove(task.id());
                    }
                }
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
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    struct Fixture {
        root: tempfile::TempDir,
        config: Config,
        backend: PathBuf,
        ops: crate::snapshot_control::Operations,
    }
    fn temporary() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("veynode-restore-test-")
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }
    fn private(path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
    #[test]
    fn actual_spawn_keeps_run_action_before_restore_options() {
        // Exercise actual process argv using the same worker_start_args and
        // append helper as the production spawn. This is an OS argument test,
        // not a simulated VZ save/restore or backend acceptance.
        let config = Path::new("/private/test path/恢复/candidate.json");
        for (restore, logging) in [(false, false), (true, false), (true, true)] {
            let (start, _) = worker_start_args(
                Backend::AppleVirtualization,
                config,
                logging.then_some(Path::new("/private/log dir")),
                "linux",
                123,
                &"a".repeat(64),
            );
            let options = restore.then(|| {
                vec![
                    OsString::from("--restore-state"),
                    OsString::from("/private/saved state/恢复/machine.vmstate"),
                    OsString::from("--restore-state-sha256"),
                    OsString::from("b".repeat(64)),
                    OsString::from("--restore-config-sha256"),
                    OsString::from("c".repeat(64)),
                    OsString::from("--restore-capture-id"),
                    OsString::from("d".repeat(32)),
                ]
            });
            let mut expected = start.clone();
            if let Some(args) = &options {
                expected.extend_from_slice(args);
            }
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "printf '%s\\0' \"$@\"", "argv-recorder"]);
            append_start_arguments(&mut command, &start, options);
            let output = command.stdin(Stdio::null()).output().unwrap();
            assert!(output.status.success());
            assert!(output.stderr.is_empty());
            let actual: Vec<_> = output
                .stdout
                .split(|b| *b == 0)
                .take(expected.len())
                .map(|b| b.to_vec())
                .collect();
            let expected: Vec<_> = expected
                .iter()
                .map(|s| s.as_encoded_bytes().to_vec())
                .collect();
            assert_eq!(
                actual, expected,
                "spawned process received incorrect arguments"
            );
            assert_eq!(actual[0], b"run");
            assert_eq!(actual[1], b"--config");
            if restore {
                assert_eq!(
                    actual.iter().position(|v| v == b"--restore-state").unwrap(),
                    if logging { 5 } else { 3 }
                );
            }
            assert_eq!(
                output.stdout.iter().filter(|b| **b == 0).count(),
                expected.len(),
                "extra or lost argument boundaries"
            );
        }
    }
    #[cfg(target_os = "macos")]
    fn fixture() -> Fixture {
        let root = temporary();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let backend = root.path().join("fixture-backend");
        private(
            &backend,
            b"offline restore transaction fixture; never executed",
        );
        let kernel = root.path().join("kernel");
        private(
            &kernel,
            b"not a bootable kernel; offline filesystem fixture",
        );
        let disk = root.path().join("original.raw");
        private(&disk, &vec![0; 4096]);
        let identity = root.path().join("identity");
        fs::create_dir(&identity).unwrap();
        fs::set_permissions(&identity, fs::Permissions::from_mode(0o700)).unwrap();
        let config:Config=serde_json::from_value(json!({"id":"linux","backend":"apple-virtualization","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel,"disk":disk,"machine_identity_dir":identity})).unwrap();
        let (_, backend_sha256) = snapshot::storage::hash(&File::open(&backend).unwrap()).unwrap();
        let compatible = Compatibility {
            backend: "apple_virtualization".into(),
            backend_version: host_version().unwrap(),
            backend_sha256,
            machine: "Apple-VZGeneric-v1".into(),
            architecture: "aarch64".into(),
            cpu_model: "Apple-VZGeneric-host".into(),
            cpus: 1,
            memory_mib: 512,
            config_sha256: "a".repeat(64),
            devices_sha256: "b".repeat(64),
            disk_bytes: Some(4096),
        };
        let store = SnapshotStore::open(root.path()).unwrap();
        let mut vm = store.lock_vm("linux").unwrap();
        let capture = vm
            .begin(
                "point",
                Kind::FullState,
                compatible.clone(),
                compatible.required_reservation(Kind::FullState).unwrap(),
            )
            .unwrap();
        capture
            .clone_disk(&File::open(config.disk.as_ref().unwrap()).unwrap())
            .unwrap();
        // Fixture bytes test storage/transactions only; no VZ state or Guest
        // save/restore is claimed and no backend process is executed.
        capture
            .create_artifact(Role::MachineState)
            .unwrap()
            .write_all(b"offline opaque machine-state fixture")
            .unwrap();
        let capture_id = capture.capture_id().to_owned();
        capture
            .commit(CaptureProof {
                capture_id,
                backend_receipt_sha256: "c".repeat(64),
                source_worker_instance: Some("d".repeat(64)),
                backend_completed: true,
                source_quiesced: true,
                ram_cpu_devices_saved: true,
                disk_in_same_quiesced_interval: true,
                protects_restore_of: None,
            })
            .unwrap();
        drop(vm);
        let ops = crate::snapshot_control::Operations::open(root.path()).unwrap();
        Fixture {
            root,
            config,
            backend,
            ops,
        }
    }
    #[test]
    fn public_restore_requires_exact_confirmation_and_shape_without_work() {
        let dir = temporary();
        let mut manager = Manager::load(
            dir.path().into(),
            "/unused".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        let config: Config =
            serde_json::from_value(json!({"id":"linux","guest":"linux","cpus":1,"memory_mib":512}))
                .unwrap();
        manager.configs.insert("linux".into(), config);
        for args in [
            json!({"id":"linux","name":"point","confirm":"other"}),
            json!({"id":"linux","name":"point","confirm":"linux","path":"/unsafe"}),
        ] {
            assert!(manager
                .snapshot_restore_dispatch("snapshot_restore", &args, &mut None)
                .is_err());
        }
        assert!(!manager.snapshot_ops.active_vm("linux"));
        assert!(manager.restore_starts.lock().unwrap().plans.is_empty());
    }
    #[test]
    fn opaque_start_rejects_unprepared_or_path_injected_requests() {
        let dir = temporary();
        let mut manager = Manager::load(
            dir.path().into(),
            "/unused".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        for args in [
            json!({"id":"linux","operation_id":"e".repeat(32)}),
            json!({"id":"linux","operation_id":"e".repeat(32),"restore_state":"/unsafe"}),
        ] {
            assert!(start_plan(&mut manager, &args).is_err());
        }
        assert!(manager.running.is_empty());
    }
    #[test]
    #[cfg(target_os = "macos")]
    fn restore_preparation_protects_original_and_clones_a_fresh_generation() {
        let f = fixture();
        let original_path = f.config.disk.as_ref().unwrap();
        fs::write(original_path, vec![7; 4096]).unwrap();
        let original_identity = fs::metadata(original_path).unwrap();
        let operation = f.ops.begin("linux", "point", "restore", None).unwrap();
        let plan = prepare(
            operation.clone(),
            f.root.path(),
            &f.backend,
            f.config.clone(),
            "point",
        )
        .unwrap();
        assert_eq!(fs::read(original_path).unwrap(), vec![7; 4096]);
        assert_eq!(
            fs::read(plan.config.disk.as_ref().unwrap()).unwrap(),
            vec![0; 4096]
        );
        let candidate_identity = fs::metadata(plan.config.disk.as_ref().unwrap()).unwrap();
        assert_ne!(
            (original_identity.dev(), original_identity.ino()),
            (candidate_identity.dev(), candidate_identity.ino())
        );
        let protection = f
            .root
            .path()
            .join("snapshots/linux")
            .join(plan.protection.as_ref().unwrap())
            .join("disk.raw");
        assert_eq!(fs::read(protection).unwrap(), vec![7; 4096]);
        assert!(plan.journal.path.join("intent.json").is_file());
        assert!(!f.root.path().join("registry.json").exists());
        assert!(!plan.claimed.load(Ordering::Acquire));
        assert!(!plan.verified.load(Ordering::Acquire));
        assert!(
            qemu_disk::DiskGuard::open(original_path).is_err(),
            "original disk must remain fenced throughout unselected candidate ownership"
        );
        drop(plan);
        operation.fail("offline_test_prepared_only", false).unwrap();
        // A second restore clones the immutable saved disk again, after the
        // previous offline transaction was safely retired without a worker.
        let second = f.ops.begin("linux", "point", "restore", None).unwrap();
        let plan2 = prepare(
            second.clone(),
            f.root.path(),
            &f.backend,
            f.config.clone(),
            "point",
        )
        .unwrap();
        assert_eq!(
            fs::read(plan2.config.disk.as_ref().unwrap()).unwrap(),
            vec![0; 4096]
        );
        assert_eq!(fs::read(original_path).unwrap(), vec![7; 4096]);
        assert_eq!(
            fs::read(f.root.path().join("snapshots/linux/point/disk.raw")).unwrap(),
            vec![0; 4096]
        );
        drop(plan2);
        second.fail("offline_test_prepared_only", false).unwrap();
    }
    #[test]
    #[cfg(target_os = "macos")]
    fn changed_backend_refuses_before_generation_or_disk_changes() {
        let f = fixture();
        fs::write(&f.backend, b"different backend bytes").unwrap();
        let before = fs::read(f.config.disk.as_ref().unwrap()).unwrap();
        let op = f.ops.begin("linux", "point", "restore", None).unwrap();
        let error = match prepare(
            op.clone(),
            f.root.path(),
            &f.backend,
            f.config.clone(),
            "point",
        ) {
            Ok(_) => panic!("changed backend accepted"),
            Err(e) => e,
        };
        assert!(error.to_string().contains("backend executable"));
        assert_eq!(fs::read(f.config.disk.as_ref().unwrap()).unwrap(), before);
        assert!(!f.root.path().join("restore-generations").exists());
        op.fail("offline_test_incompatible", false).unwrap();
    }
}
