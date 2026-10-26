//! Immutable admission-time witnesses for interrupted Apple Linux capture.
//! Retirement never implies that a snapshot was committed or that RAM was restored.
use super::*;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Node {
    device: u64,
    inode: u64,
    bytes: u64,
}
impl Node {
    fn of(file: &File) -> Result<Self> {
        let m = file.metadata()?;
        Ok(Self {
            device: m.dev(),
            inode: m.ino(),
            bytes: m.len(),
        })
    }
    fn check(&self, file: &File) -> Result<()> {
        ensure!(
            *self == Self::of(file)?,
            "create retirement witness inode/extent changed"
        );
        snapshot::storage::reject_extended_acl(file)?;
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    schema: u32,
    operation_id: String,
    id: String,
    name: String,
    worker_instance: String,
    config: Config,
    identity_directory: Node,
    identity_file: Node,
    identity_sha256: String,
    identity_lock: Node,
    disk: Option<Node>,
}
pub(super) struct Prepared {
    config: Config,
    worker: String,
    identity: Dir,
    file: File,
    lock: File,
    disk: Option<File>,
    nodes: (Node, Node, Node, Option<Node>),
    digest: String,
}
impl Prepared {
    pub(super) fn observe(config: &Config, worker: &str) -> Result<Self> {
        ensure!(
            config.backend == Backend::AppleVirtualization
                && config.guest == "linux"
                && native_input::hex(worker, 64),
            "create retirement witness requires bound Apple Linux worker"
        );
        let identity = Dir::open(
            config
                .machine_identity_dir
                .as_deref()
                .context("persistent identity required")?,
        )?;
        let file = identity.file("linux-identity-v1.json", false)?;
        ensure!(
            file.metadata()?.len() <= 4096,
            "create identity extent too large"
        );
        let lock = identity.file(".linux-snapshot.lock", false)?;
        let disk = config
            .disk
            .as_deref()
            .map(|p| qemu_disk::observe_raw_identity(p).map(|v| v.0))
            .transpose()?;
        if let Some(disk) = &disk {
            snapshot::storage::reject_extended_acl(disk)?;
        }
        let nodes = (
            Node::of(&identity.file)?,
            Node::of(&file)?,
            Node::of(&lock)?,
            disk.as_ref().map(Node::of).transpose()?,
        );
        let digest = snapshot::storage::hash(&file)?.1;
        Ok(Self {
            config: config.clone(),
            worker: worker.into(),
            identity,
            file,
            lock,
            disk,
            nodes,
            digest,
        })
    }
    pub(super) fn publish(&self, directory: &Dir, record: &Record) -> Result<String> {
        ensure!(
            record.action == "create"
                && record.id == self.config.id
                && record.worker_instance.as_deref() == Some(self.worker.as_str()),
            "create admission witness binding mismatch"
        );
        self.identity.verify()?;
        self.nodes.0.check(&self.identity.file)?;
        self.nodes.1.check(&self.file)?;
        self.nodes.2.check(&self.lock)?;
        // Reopen path edges; retaining the original FD alone cannot detect replacement.
        self.nodes
            .1
            .check(&self.identity.file("linux-identity-v1.json", false)?)?;
        self.nodes
            .2
            .check(&self.identity.file(".linux-snapshot.lock", false)?)?;
        ensure!(
            snapshot::storage::hash(&self.file)?.1 == self.digest,
            "create identity changed during admission"
        );
        if let (Some(node), Some(file), Some(path)) = (&self.nodes.3, &self.disk, &self.config.disk)
        {
            node.check(file)?;
            node.check(&qemu_disk::observe_raw_identity(path)?.0)?;
        }
        let intent = Intent {
            schema: 1,
            operation_id: record.operation_id.clone(),
            id: record.id.clone(),
            name: record.name.clone(),
            worker_instance: self.worker.clone(),
            config: self.config.clone(),
            identity_directory: self.nodes.0.clone(),
            identity_file: self.nodes.1.clone(),
            identity_sha256: self.digest.clone(),
            identity_lock: self.nodes.2.clone(),
            disk: self.nodes.3.clone(),
        };
        directory.write_json("create-intent.json", &intent)?;
        Ok(snapshot::storage::hash(&directory.file("create-intent.json", false)?)?.1)
    }
}
pub(super) fn validate_live_intent(dir: &Path, registered: &Config, record: &Record) -> Result<()> {
    let expected = record
        .create_intent_sha256
        .as_deref()
        .context("live source has no admission witness")?;
    let root = Dir::open(dir)?;
    let (operations, _) = root.child("snapshot-operations", false)?;
    let (journal, _) = operations.child(&record.operation_id, false)?;
    ensure!(
        snapshot::storage::hash(&journal.file("create-intent.json", false)?)?.1 == expected,
        "live source intent digest mismatch"
    );
    let intent: Intent = journal.json("create-intent.json")?;
    ensure!(
        intent.schema == 1
            && intent.operation_id == record.operation_id
            && intent.id == record.id
            && intent.name == record.name
            && record.worker_instance.as_deref() == Some(intent.worker_instance.as_str())
            && serde_json::to_vec(&intent.config)? == serde_json::to_vec(registered)?,
        "live source configuration/operation changed"
    );
    let current = Prepared::observe(registered, &intent.worker_instance)?;
    ensure!(
        current.nodes
            == (
                intent.identity_directory,
                intent.identity_file,
                intent.identity_lock,
                intent.disk
            )
            && current.digest == intent.identity_sha256,
        "live source disk or persistent identity changed"
    );
    Ok(())
}

pub(super) struct Proof {
    identity: Dir,
    file: File,
    lock: File,
    disk: Option<qemu_disk::DiskGuard>,
    disk_file: Option<File>,
    journal: Dir,
    intent_file: File,
    intent: Intent,
    intent_sha: String,
    native_retirement: Option<(File, Node, String)>,
    pub(super) receipt: Value,
}
impl Proof {
    pub(super) fn verify(&self) -> Result<()> {
        self.identity.verify()?;
        self.journal.verify()?;
        self.intent.identity_directory.check(&self.identity.file)?;
        self.intent.identity_file.check(&self.file)?;
        self.intent.identity_lock.check(&self.lock)?;
        self.intent
            .identity_file
            .check(&self.identity.file("linux-identity-v1.json", false)?)?;
        self.intent
            .identity_lock
            .check(&self.identity.file(".linux-snapshot.lock", false)?)?;
        ensure!(
            snapshot::storage::hash(&self.file)?.1 == self.intent.identity_sha256,
            "create persistent identity content changed"
        );
        ensure!(
            snapshot::storage::hash(&self.intent_file)?.1 == self.intent_sha
                && snapshot::storage::hash(&self.journal.file("create-intent.json", false)?)?.1
                    == self.intent_sha,
            "create retirement intent changed"
        );
        if let Some((file, node, digest)) = &self.native_retirement {
            node.check(file)?;
            let current = self.journal.file("native-retired.json", false)?;
            node.check(&current)?;
            ensure!(
                snapshot::storage::hash(file)?.1 == *digest
                    && snapshot::storage::hash(&current)?.1 == *digest,
                "native retirement receipt changed"
            );
        }
        if let (Some(disk), Some(file), Some(node)) =
            (&self.disk, &self.disk_file, &self.intent.disk)
        {
            disk.verify_paths()?;
            node.check(file)?;
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeRetirement {
    schema: u32,
    operation_id: String,
    id: String,
    worker_instance: String,
    capture_id: String,
    configuration_sha256: String,
    capture_io_drained: bool,
    backend_stopped: bool,
    normal_guest_shutdown: bool,
}
pub(super) fn prove(dir: &Path, registered: &Config, record: &Record) -> Result<Proof> {
    ensure!(
        record.action == "create"
            && record.state == "recovery_required"
            && record.id == registered.id,
        "create retirement requires exact unresolved operation"
    );
    let expected = record
        .create_intent_sha256
        .as_deref()
        .context("legacy create has no admission-time retirement witness")?;
    let root = Dir::open(dir)?;
    let (operations, _) = root.child("snapshot-operations", false)?;
    let (journal, _) = operations.child(&record.operation_id, false)?;
    // Losing the native process does not prove an asynchronous framework save
    // has stopped writing. Revalidate the immutable history, and retain the
    // fence for every potentially dispatched capture without a verified native
    // I/O-drain retirement receipt. Time or stable hashes
    // alone must never substitute for that ownership proof.
    let history: Vec<_> = journal
        .names()?
        .into_iter()
        .filter(|n| {
            n.len() == 9 && n.ends_with(".json") && n.as_bytes()[..4].iter().all(u8::is_ascii_digit)
        })
        .collect();
    ensure!(
        history.len() == record.sequence as usize + 1,
        "create retirement history changed"
    );
    let mut previous: Option<Record> = None;
    let mut save_may_have_started = false;
    for (index, name) in history.iter().enumerate() {
        ensure!(
            name == &format!("{index:04}.json"),
            "create retirement history gap"
        );
        let current: Record = journal.json(name)?;
        current.validate()?;
        ensure!(
            current.operation_id == record.operation_id
                && current.id == record.id
                && current.action == "create"
                && current.name == record.name
                && current.worker_instance == record.worker_instance
                && current.create_intent_sha256 == record.create_intent_sha256
                && current.sequence == index as u32,
            "create retirement history binding changed"
        );
        if let Some(old) = &previous {
            ensure!(
                current.previous_sha256.as_deref() == Some(old.digest()?.as_str()),
                "create retirement history chain changed"
            );
        }
        save_may_have_started |= matches!(
            current.phase.as_str(),
            "capturing" | "committing" | "resuming"
        );
        previous = Some(current);
    }
    ensure!(
        previous.context("create history missing")?.digest()? == record.digest()?,
        "create retirement latest receipt changed"
    );
    let native_retirement = if save_may_have_started {
        let binding = record.capture_binding.as_ref().context(
            "native asynchronous capture retirement is unconfirmed: no sealed capture binding",
        )?;
        let file = journal.file("native-retired.json", false).context(
            "native asynchronous capture retirement is unconfirmed: drain receipt missing",
        )?;
        ensure!(
            file.metadata()?.len() <= 4096,
            "native retirement receipt too large"
        );
        let receipt: NativeRetirement = journal.json("native-retired.json")?;
        ensure!(
            receipt.schema == 1
                && receipt.operation_id == record.operation_id
                && receipt.id == record.id
                && record.worker_instance.as_deref() == Some(receipt.worker_instance.as_str())
                && receipt.capture_id == binding.capture_id
                && receipt.configuration_sha256 == binding.configuration_sha256
                && receipt.capture_io_drained
                && receipt.backend_stopped
                && !receipt.normal_guest_shutdown,
            "native retirement receipt is not bound to this drained/stopped capture"
        );
        Some((
            file.try_clone()?,
            Node::of(&file)?,
            snapshot::storage::hash(&file)?.1,
        ))
    } else {
        None
    };
    let intent_file = journal.file("create-intent.json", false)?;
    ensure!(
        snapshot::storage::hash(&intent_file)?.1 == expected,
        "create retirement intent digest mismatch"
    );
    let intent: Intent = journal.json("create-intent.json")?;
    ensure!(
        intent.schema == 1
            && intent.operation_id == record.operation_id
            && intent.id == record.id
            && intent.name == record.name
            && record.worker_instance.as_deref() == Some(intent.worker_instance.as_str())
            && serde_json::to_vec(&intent.config)? == serde_json::to_vec(registered)?,
        "create retirement configuration/operation mismatch"
    );
    ensure!(
        registered.backend == Backend::AppleVirtualization
            && registered.guest == "linux"
            && intent.disk.is_some() == registered.disk.is_some(),
        "unsupported create retirement backend/disk binding"
    );
    let identity = Dir::open(
        registered
            .machine_identity_dir
            .as_deref()
            .context("create identity missing")?,
    )?;
    intent.identity_directory.check(&identity.file)?;
    let file = identity.file("linux-identity-v1.json", false)?;
    let lock = identity.file(".linux-snapshot.lock", false)?;
    intent.identity_file.check(&file)?;
    intent.identity_lock.check(&lock)?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "original Apple machine identity is still owned"
    );
    let disk = registered
        .disk
        .as_deref()
        .map(qemu_disk::DiskGuard::open_apple_raw)
        .transpose()?;
    let disk_file = registered
        .disk
        .as_deref()
        .map(|p| qemu_disk::observe_raw_identity(p).map(|v| v.0))
        .transpose()?;
    if let (Some(node), Some(file)) = (&intent.disk, &disk_file) {
        node.check(file)?;
    }
    if let Some(file) = &disk_file {
        lock_retired_image(file)?;
    }
    let receipt = json!({"reconciled":true,"restored":false,"snapshot_result_verified":false,
        "basis":if native_retirement.is_some(){"admission_and_native_io_drain_retirement"}else{"admission_bound_disk_and_persistent_identity_retirement"},
        "native_io_retirement_verified":native_retirement.is_some(),
        "operation_failed_history_retained":true,"registry_selection_preserved":"original","disks_unchanged":true,
        "snapshot_artifacts_preserved":true});
    let proof = Proof {
        identity,
        file,
        lock,
        disk,
        disk_file,
        journal,
        intent_file,
        intent,
        intent_sha: expected.into(),
        native_retirement,
        receipt,
    };
    proof.verify()?;
    Ok(proof)
}

#[cfg(test)]
#[path = "snapshot_create_recovery_tests.rs"]
mod tests;
