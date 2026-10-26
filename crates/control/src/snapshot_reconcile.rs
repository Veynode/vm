//! Proof-based retirement of an ambiguous restore operation. This never changes
//! the registry, either disk, saved state, or an earlier operation receipt.
use super::*;
use base64::Engine;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    schema: u32,
    operation_id: String,
    id: String,
    name: String,
    capture_id: String,
    original: Config,
    candidate: Config,
    compatibility: Compatibility,
    protection: Option<String>,
    state_sha256: String,
    state_bytes: u64,
    phase: String,
    selected: bool,
    native_preload_configuration_check_required: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    schema: u32,
    vm: String,
    machine_base64: String,
    mac_address: String,
}
struct RetirementProof {
    original: qemu_disk::DiskGuard,
    candidate: qemu_disk::DiskGuard,
    original_file: File,
    candidate_file: File,
    identity_directory: Dir,
    identity_file: File,
    identity_lock: File,
    journal: Dir,
    intent_file: File,
    identity_digest: (u64, String),
    intent_digest: (u64, String),
    receipt: Value,
}
enum ReconciliationProof {
    Restore(Box<RetirementProof>),
    Create(Box<create_recovery::Proof>),
}
impl ReconciliationProof {
    fn verify(&self) -> Result<()> {
        match self {
            Self::Restore(p) => p.verify(),
            Self::Create(p) => p.verify(),
        }
    }
    fn receipt(&self) -> Value {
        match self {
            Self::Restore(p) => p.receipt.clone(),
            Self::Create(p) => p.receipt.clone(),
        }
    }
}
fn same_file(a: &File, b: &File) -> Result<()> {
    let a = a.metadata()?;
    let b = b.metadata()?;
    ensure!(
        a.dev() == b.dev()
            && a.ino() == b.ino()
            && a.len() == b.len()
            && a.mtime() == b.mtime()
            && a.mtime_nsec() == b.mtime_nsec()
            && a.ctime() == b.ctime()
            && a.ctime_nsec() == b.ctime_nsec(),
        "snapshot retirement proof path/content changed"
    );
    Ok(())
}
fn same_config(a: &Config, b: &Config) -> Result<bool> {
    Ok(serde_json::to_vec(a)? == serde_json::to_vec(b)?)
}
impl RetirementProof {
    fn verify(&self) -> Result<()> {
        self.original.verify_paths()?;
        self.candidate.verify_paths()?;
        snapshot::storage::reject_extended_acl(&self.original_file)?;
        snapshot::storage::reject_extended_acl(&self.candidate_file)?;
        self.identity_directory.verify()?;
        self.journal.verify()?;
        same_file(
            &self.identity_file,
            &self
                .identity_directory
                .file("linux-identity-v1.json", false)?,
        )?;
        same_file(
            &self.identity_lock,
            &self
                .identity_directory
                .file(".linux-snapshot.lock", false)?,
        )?;
        same_file(&self.intent_file, &self.journal.file("intent.json", false)?)?;
        ensure!(
            snapshot::storage::hash(&self.identity_file)? == self.identity_digest
                && snapshot::storage::hash(&self.intent_file)? == self.intent_digest,
            "snapshot retirement identity/intent contents changed"
        );
        Ok(())
    }
}
fn open_retired_disk(path: &Path, guard: &qemu_disk::DiskGuard) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let m = file.metadata()?;
    ensure!(
        m.dev() == guard.identity().device
            && m.ino() == guard.identity().inode
            && m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.nlink() == 1
            && m.mode() & 0o022 == 0,
        "restore retirement disk no longer matches its exclusive guard"
    );
    snapshot::storage::reject_extended_acl(&file)?;
    lock_retired_image(&file)?;
    Ok(file)
}
fn prove_retirement(dir: &Path, registered: &Config, record: &Record) -> Result<RetirementProof> {
    ensure!(
        record.state == "recovery_required"
            && record.action == "restore"
            && record.id == registered.id,
        "reconcile requires the exact unresolved restore operation"
    );
    ensure!(
        registered.backend == Backend::AppleVirtualization && registered.guest == "linux",
        "snapshot reconciliation currently supports Apple Linux restore only"
    );
    let root = Dir::open(dir)?;
    let (generations, _) = root.child("restore-generations", false)?;
    let (vm, _) = generations.child(&registered.id, false)?;
    let (journal, _) = vm.child(&record.operation_id, false)?;
    let intent_file = journal.file("intent.json", false)?;
    let intent_digest = snapshot::storage::hash(&intent_file)?;
    let intent: Intent = journal.json("intent.json")?;
    ensure!(
        intent.schema == 1
            && intent.operation_id == record.operation_id
            && intent.id == record.id
            && intent.name == record.name
            && intent.original.id == record.id
            && intent.candidate.id == record.id,
        "restore retirement intent VM/operation identity mismatch"
    );
    ensure!(
        jobs::validate_job_id(&intent.capture_id).is_ok()
            && native_input::hex(&intent.state_sha256, 64)
            && intent.state_bytes > 0
            && intent.phase == "prepared"
            && !intent.selected
            && intent.native_preload_configuration_check_required,
        "restore retirement intent version/state binding invalid"
    );
    ensure!(
        intent.compatibility.backend == "apple_virtualization"
            && intent.compatibility.architecture == "aarch64"
            && intent.compatibility.disk_bytes.is_some(),
        "diskless or unsupported restore retirement lacks independent backend proof"
    );
    let original_path = intent
        .original
        .disk
        .as_deref()
        .context("diskless original requires an independent retirement witness")?;
    let candidate_path = intent
        .candidate
        .disk
        .as_deref()
        .context("restore candidate disk missing")?;
    ensure!(
        candidate_path == journal.path.join("disk.raw"),
        "restore candidate disk is outside its owned generation"
    );
    let mut expected = intent.original.clone();
    expected.disk = Some(journal.path.join("disk.raw"));
    ensure!(
        same_config(&expected, &intent.candidate)?,
        "restore intent changes configuration beyond its private disk generation"
    );
    let selected = if same_config(registered, &intent.original)? {
        "original"
    } else if same_config(registered, &intent.candidate)? {
        "candidate"
    } else {
        bail!("registered VM changed after failed restore; reconciliation refused")
    };
    ensure!(
        intent.original.backend == Backend::AppleVirtualization && intent.original.guest == "linux",
        "restore intent backend/Guest unsupported"
    );
    let identity_path = intent
        .original
        .machine_identity_dir
        .as_deref()
        .context("restore machine identity path missing")?;
    ensure!(
        intent.candidate.machine_identity_dir.as_deref() == Some(identity_path),
        "restore persistent machine identity mismatch"
    );
    let identity_directory = Dir::open(identity_path)?;
    // Existing inode only: never create a replacement witness which could hide
    // an orphan backend holding the original lock. Close-only descriptor Drop.
    let identity_lock = identity_directory.file(".linux-snapshot.lock", false)?;
    ensure!(
        unsafe { libc::flock(identity_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "original/candidate Apple machine identity is still owned"
    );
    let identity_file = identity_directory.file("linux-identity-v1.json", false)?;
    ensure!(
        identity_file.metadata()?.len() <= 4096,
        "persistent machine identity metadata too large"
    );
    let identity_digest = snapshot::storage::hash(&identity_file)?;
    let identity: Identity = identity_directory.json("linux-identity-v1.json")?;
    ensure!(
        identity.schema == 1 && identity.vm == record.id,
        "persistent machine identity belongs to another VM"
    );
    let machine = base64::engine::general_purpose::STANDARD
        .decode(&identity.machine_base64)
        .context("persistent machine identity encoding invalid")?;
    ensure!(
        !machine.is_empty() && machine.len() <= 1024,
        "persistent machine identity extent invalid"
    );
    let mac: Vec<_> = identity.mac_address.split(':').collect();
    ensure!(
        mac.len() == 6
            && mac
                .iter()
                .all(|s| s.len() == 2 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            && u8::from_str_radix(mac[0], 16)? & 3 == 2,
        "persistent machine MAC identity invalid"
    );
    // Both guards are mandatory. In-memory failed Prepared plans, old daemons,
    // aliased disks and a still-running candidate all make this fail closed.
    let original = qemu_disk::DiskGuard::open_apple_raw(original_path)?;
    let candidate = qemu_disk::DiskGuard::open_apple_raw(candidate_path)?;
    ensure!(
        original.identity() != candidate.identity(),
        "restore original/candidate disk alias unsupported"
    );
    let original_file = open_retired_disk(original_path, &original)?;
    let candidate_file = open_retired_disk(candidate_path, &candidate)?;
    let expected_bytes = intent.compatibility.disk_bytes.unwrap();
    ensure!(
        original_file.metadata()?.len() == expected_bytes
            && candidate_file.metadata()?.len() == expected_bytes,
        "restore retired disk extent mismatch"
    );
    if let Some(name) = &intent.protection {
        ensure!(
            name == &format!("protect-{}", record.operation_id),
            "restore protection name binding invalid"
        );
    }
    let receipt = json!({"reconciled":true,"restored":false,"snapshot_result_verified":false,
        "basis":"exclusive_original_candidate_disks_and_persistent_machine_identity",
        "operation_failed_history_retained":true,"registry_selection_preserved":selected,"disks_unchanged":true,
        "proof":{"original_device":original.identity().device,"original_inode":original.identity().inode,
            "candidate_device":candidate.identity().device,"candidate_inode":candidate.identity().inode,
            "identity_lock_device":identity_lock.metadata()?.dev(),"identity_lock_inode":identity_lock.metadata()?.ino(),
            "machine_identity_sha256":format!("{:x}",Sha256::digest(&machine))}});
    let proof = RetirementProof {
        original,
        candidate,
        original_file,
        candidate_file,
        identity_directory,
        identity_file,
        identity_lock,
        journal,
        intent_file,
        identity_digest,
        intent_digest,
        receipt,
    };
    proof.verify()?;
    Ok(proof)
}
impl Manager {
    pub(crate) fn snapshot_reconcile_dispatch(
        &mut self,
        method: &str,
        args: &Value,
        deferred: &mut Option<Completion>,
    ) -> Result<Option<Value>> {
        if method != "snapshot_reconcile" {
            return Ok(None);
        }
        ensure!(
            args.as_object().is_some_and(|o| o.len() == 3
                && ["id", "operation_id", "confirm"]
                    .iter()
                    .all(|k| o.contains_key(*k))),
            "snapshot_reconcile accepts exactly id, operation_id and confirm"
        );
        let id = args["id"].as_str().context("reconcile VM id missing")?;
        validate_id(id)?;
        ensure!(
            args["confirm"] == id,
            "snapshot reconcile confirm must exactly equal VM id"
        );
        let op = args["operation_id"]
            .as_str()
            .context("reconcile operation identity missing")?;
        jobs::validate_job_id(op)?;
        let record = self
            .snapshot_ops
            .records
            .lock()
            .unwrap()
            .get(op)
            .filter(|r| r.id == id)
            .cloned()
            .context("snapshot operation not found for VM")?;
        ensure!(
            record.state == "recovery_required"
                && matches!(record.action.as_str(), "restore" | "create"),
            "only an unresolved create/restore with durable binding can be reconciled"
        );
        ensure!(
            !record.source_resume_pending,
            "source resume is still in flight"
        );
        let registered = self.configs.get(id).context("VM not found")?.clone();
        if let Some(run) = self.running.get_mut(id) {
            ensure!(run.poll_exit()?.is_some(),"snapshot reconciliation requires actual worker retirement; a stopped label is insufficient");
        }
        ensure!(
            self.jobs.lock().unwrap().snapshot_idle(id),
            "Guest job retirement is unconfirmed"
        );
        ensure!(
            !self.file_sessions.values().any(|(_, vms)| vms.contains(id))
                && !self.native_connections.contains_key(id),
            "file or native control connection retirement is unconfirmed"
        );
        let operation = Operation {
            operations: self.snapshot_ops.clone(),
            id: op.into(),
        };
        let dir = self.dir.clone();
        let operations = self.snapshot_ops.clone();
        let (vm_id, op_id) = (id.to_owned(), op.to_owned());
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("snapshot-reconcile".into())
            .spawn(move || {
                let result = (|| {
                    let proof = if record.action == "create" {
                        ReconciliationProof::Create(Box::new(create_recovery::prove(
                            &dir,
                            &registered,
                            &record,
                        )?))
                    } else {
                        ReconciliationProof::Restore(Box::new(prove_retirement(
                            &dir,
                            &registered,
                            &record,
                        )?))
                    };
                    proof.verify()?;
                    operation.update(
                        "reconciled_after_retirement",
                        "failed",
                        Some(proof.receipt()),
                        Some(if record.action == "create" {
                            "create_attempt_retired"
                        } else {
                            "restore_attempt_retired"
                        }),
                        None,
                    )?;
                    // Release all witnesses before returning. Historical failure and
                    // candidate artifacts remain; no registry/disk write occurred.
                    drop(proof);
                    operations.status(&vm_id, &op_id)
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
        Ok(Some(Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    struct Fixture {
        _temp: tempfile::TempDir,
        path: PathBuf,
        config: Config,
        ops: Operations,
        operation: Operation,
        record: Record,
        journal: PathBuf,
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
    fn dir(path: &Path) {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().canonicalize().unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let original = path.join("original.raw");
        private(&original, &[0; 512]);
        let identity = path.join("identity");
        dir(&identity);
        private(&identity.join(".linux-snapshot.lock"), b"");
        private(&identity.join("linux-identity-v1.json"),&serde_json::to_vec(&json!({"schema":1,"vm":"linux1","machine_base64":base64::engine::general_purpose::STANDARD.encode(b"offline fixture, not a VZ identifier"),"mac_address":"02:11:22:33:44:55"})).unwrap());
        let config:Config=serde_json::from_value(json!({"id":"linux1","guest":"linux","cpus":1,"memory_mib":1024,"disk":original,"machine_identity_dir":identity})).unwrap();
        let ops = Operations::open(&path).unwrap();
        let operation = ops.begin("linux1", "before", "restore", None).unwrap();
        operation.phase("starting_restore").unwrap();
        operation.fail("offline_failure_fixture", true).unwrap();
        let record = ops.records.lock().unwrap()[operation.id()].clone();
        let generations = path.join("restore-generations");
        dir(&generations);
        let vm = generations.join("linux1");
        dir(&vm);
        let journal = vm.join(operation.id());
        dir(&journal);
        let candidate_path = journal.join("disk.raw");
        private(&candidate_path, &[1; 512]);
        let mut candidate = config.clone();
        candidate.disk = Some(candidate_path);
        private(&journal.join("intent.json"),&serde_json::to_vec(&json!({"schema":1,"operation_id":operation.id(),"id":"linux1","name":"before","capture_id":"1".repeat(32),
            "original":config,"candidate":candidate,"compatibility":{"backend":"apple_virtualization","backend_version":"fixture","backend_sha256":"2".repeat(64),"machine":"Apple-VZGeneric-v1","architecture":"aarch64","cpu_model":"Apple-VZGeneric-host","cpus":1,"memory_mib":1024,"config_sha256":"3".repeat(64),"devices_sha256":"4".repeat(64),"disk_bytes":512},
            "protection":format!("protect-{}",operation.id()),"state_sha256":"5".repeat(64),"state_bytes":100,"phase":"prepared","selected":false,"native_preload_configuration_check_required":true})).unwrap());
        Fixture {
            _temp: temp,
            path,
            config,
            ops,
            operation,
            record,
            journal,
        }
    }
    #[test]
    fn retirement_proof_appends_terminal_receipt_without_changing_original_history_or_disks() {
        let f = fixture();
        let history = f.path.join("snapshot-operations").join(f.operation.id());
        let before: BTreeMap<_, _> = fs::read_dir(&history)
            .unwrap()
            .map(|e| {
                let p = e.unwrap().path();
                (p.clone(), fs::read(&p).unwrap())
            })
            .collect();
        let original_before = fs::read(f.config.disk.as_ref().unwrap()).unwrap();
        let candidate_before = fs::read(f.journal.join("disk.raw")).unwrap();
        let proof = prove_retirement(&f.path, &f.config, &f.record).unwrap();
        proof.verify().unwrap();
        assert!(qemu_disk::DiskGuard::open(f.config.disk.as_deref().unwrap()).is_err());
        assert!(qemu_disk::DiskGuard::open(&f.journal.join("disk.raw")).is_err());
        f.operation
            .update(
                "reconciled_after_retirement",
                "failed",
                Some(proof.receipt.clone()),
                Some("restore_attempt_retired"),
                None,
            )
            .unwrap();
        drop(proof);
        assert!(!f.ops.active_vm("linux1"));
        assert_eq!(
            f.ops.status("linux1", f.operation.id()).unwrap()["result"]["reconciled"],
            true
        );
        for (path, bytes) in before {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        assert_eq!(
            fs::read(f.config.disk.as_ref().unwrap()).unwrap(),
            original_before
        );
        assert_eq!(
            fs::read(f.journal.join("disk.raw")).unwrap(),
            candidate_before
        );
        let reopened = Operations::open(&f.path).unwrap();
        assert!(!reopened.active_vm("linux1"));
    }
    #[test]
    fn live_identity_or_either_disk_owner_prevents_reconciliation() {
        for owner in 0..3 {
            let f = fixture();
            let identity = File::open(
                f.config
                    .machine_identity_dir
                    .as_ref()
                    .unwrap()
                    .join(".linux-snapshot.lock"),
            )
            .unwrap();
            let candidate_path = f.journal.join("disk.raw");
            let guard = if owner == 0 {
                assert_eq!(
                    unsafe { libc::flock(identity.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    0
                );
                None
            } else {
                Some(
                    qemu_disk::DiskGuard::open(if owner == 1 {
                        f.config.disk.as_deref().unwrap()
                    } else {
                        &candidate_path
                    })
                    .unwrap(),
                )
            };
            assert!(prove_retirement(&f.path, &f.config, &f.record).is_err());
            assert!(f.ops.active_vm("linux1"));
            drop(guard);
            drop(identity);
        }
    }
    #[test]
    fn missing_or_foreign_binding_diskless_and_registry_changes_are_refused() {
        for wrong in 0..5 {
            let f = fixture();
            let mut config = f.config.clone();
            match wrong {
                0 => {
                    fs::remove_file(f.journal.join("intent.json")).unwrap();
                }
                1 => {
                    config.memory_mib += 1;
                }
                2 => {
                    let mut value: Value =
                        serde_json::from_slice(&fs::read(f.journal.join("intent.json")).unwrap())
                            .unwrap();
                    value["id"] = json!("other");
                    fs::write(
                        f.journal.join("intent.json"),
                        serde_json::to_vec(&value).unwrap(),
                    )
                    .unwrap();
                }
                3 => {
                    let mut value: Value =
                        serde_json::from_slice(&fs::read(f.journal.join("intent.json")).unwrap())
                            .unwrap();
                    value["compatibility"]["disk_bytes"] = Value::Null;
                    fs::write(
                        f.journal.join("intent.json"),
                        serde_json::to_vec(&value).unwrap(),
                    )
                    .unwrap();
                }
                _ => {
                    let mut record = f.record.clone();
                    record.action = "create".into();
                    assert!(prove_retirement(&f.path, &config, &record).is_err());
                    continue;
                }
            }
            assert!(prove_retirement(&f.path, &config, &f.record).is_err());
            assert!(f.ops.active_vm("linux1"));
        }
    }
    #[test]
    fn actual_live_child_cannot_be_retired_by_a_stopped_state_label() {
        let f = fixture();
        let mut manager = Manager::load(
            f.path.clone(),
            PathBuf::from("/not-executed"),
            MemoryBudget::new(8 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        manager.configs.insert("linux1".into(), f.config.clone());
        let mut child = Command::new("/bin/cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        manager.running.insert(
            "linux1".into(),
            Running {
                bridge: Arc::new(Mutex::new(bridge::State::new().unwrap())),
                child,
                assigned_memory_mib: 1,
                expected_exit: false,
                execution_quiescing: false,
                guest_async_timeout: Arc::new(Mutex::new(GuestAsyncTimeout::default())),
                exit_status: None,
                disk_identity: None,
                stdin,
                state: Arc::new(Mutex::new(json!({"state":"stopped"}))),
                pending: Arc::new(Mutex::new(BTreeMap::new())),
                next: 1,
            },
        );
        let mut deferred = None;
        let error = manager
            .snapshot_reconcile_dispatch(
                "snapshot_reconcile",
                &json!({"id":"linux1","operation_id":f.operation.id(),"confirm":"linux1"}),
                &mut deferred,
            )
            .unwrap_err();
        assert!(error.to_string().contains("actual worker retirement"));
        assert!(deferred.is_none());
        let runtime = manager.running.remove("linux1").unwrap();
        let mut child = runtime.child;
        drop(runtime.stdin);
        assert!(child.wait().unwrap().success());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn acl_on_identity_lock_or_retired_disks_never_clears_recovery_or_changes_attributes() {
        for target in 0..6 {
            let f = fixture();
            let path = match target {
                0 => f
                    .config
                    .machine_identity_dir
                    .as_ref()
                    .unwrap()
                    .join(".linux-snapshot.lock"),
                1 => f
                    .config
                    .machine_identity_dir
                    .as_ref()
                    .unwrap()
                    .join("linux-identity-v1.json"),
                2 => f.config.machine_identity_dir.clone().unwrap(),
                3 => f.config.disk.clone().unwrap(),
                4 => f.journal.join("disk.raw"),
                _ => f.journal.join("intent.json"),
            };
            let acl = snapshot::storage::acl_test_support::Entry::add(&path);
            let error = match prove_retirement(&f.path, &f.config, &f.record) {
                Ok(_) => panic!("ACL-bearing retirement witness was accepted"),
                Err(error) => error,
            };
            assert!(format!("{error:#}").contains("extended ACL"), "{error:#}");
            assert!(f.ops.active_vm("linux1"));
            acl.assert_unchanged();
        }
    }
}
