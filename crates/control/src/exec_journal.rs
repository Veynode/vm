//! Recovery-only obligations. No argv, output, raw owner credential, or reusable authority.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    path::Path,
};
const FILE: &str = "exec-scopes.json";
const REQUIRED: &str = "exec-scopes.required";
const MAX_BYTES: usize = 262144;
const MAX_COMPLETED: usize = 64;
const MAX_LAUNCHES: usize = 256;
const MAX_SCOPES: usize = 256;
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub vm: String,
    pub worker_instance: String,
    pub owner_sha256: String,
    pub epoch: u64,
}
impl Record {
    fn from_scope(s: &crate::exec_scope::Scope) -> Self {
        Self {
            vm: s.vm.clone(),
            worker_instance: s.worker_instance.clone(),
            owner_sha256: owner_hash(&s.owner),
            epoch: s.epoch,
        }
    }
    fn validate(&self) -> Result<()> {
        crate::validate_id(&self.vm)?;
        if !crate::native_input::hex(&self.worker_instance, 64)
            || !crate::native_input::hex(&self.owner_sha256, 64)
            || self.epoch == 0
            || self.epoch > 9007199254740991
        {
            bail!("invalid execution journal record")
        }
        Ok(())
    }
}
fn owner_hash(owner: &str) -> String {
    format!("{:x}", Sha256::digest(owner.as_bytes()))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    scopes: Vec<Record>,
    launches: Vec<crate::exec_witness::Identity>,
    completed: Vec<crate::exec_witness::Identity>,
}
impl Snapshot {
    fn validate_links(&self) -> Result<()> {
        if ![2, 3].contains(&self.version) {
            return Ok(());
        }
        let mut instances = BTreeSet::new();
        let mut vms = BTreeSet::new();
        for l in &self.launches {
            l.validate()?;
            if !instances.insert(l.worker_instance.clone()) || !vms.insert(l.vm.clone()) {
                bail!("inconsistent duplicate launch")
            }
        }
        for c in &self.completed {
            c.validate()?;
            if !instances.insert(c.worker_instance.clone()) {
                bail!("inconsistent completed incarnation")
            }
        }
        for scope in &self.scopes {
            scope.validate()?;
            if !self
                .launches
                .iter()
                .any(|l| l.vm == scope.vm && l.worker_instance == scope.worker_instance)
            {
                bail!("execution scope lacks original launch witness")
            }
        }
        Ok(())
    }
}
pub(crate) struct Journal {
    directory: File,
    records: BTreeSet<Record>,
    launches: BTreeSet<crate::exec_witness::Identity>,
    recovered_launches: BTreeSet<crate::exec_witness::Identity>,
    completed: Vec<crate::exec_witness::Identity>,
    path: std::path::PathBuf,
    legacy: bool,
    recovered: BTreeSet<Record>,
    poisoned: bool,
    published: Option<Vec<u8>>,
    #[cfg(test)]
    pub fail_before_sync: bool,
    #[cfg(test)]
    stop_after_receipt_sync: bool,
}
fn c(value: &str) -> CString {
    CString::new(value).expect("fixed/generated file name")
}
fn owned(fd: i32) -> Result<File> {
    if fd < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
impl Journal {
    pub fn open(path: &Path, require_existing: bool) -> Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        let witness_path = path.to_path_buf();
        let path = CString::new(path.as_os_str().as_bytes())?;
        let directory = owned(unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        let m = directory.metadata()?;
        if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o777 != 0o700 {
            bail!("execution journal directory must be owner-private")
        }
        let mut journal = Self {
            directory,
            records: BTreeSet::new(),
            launches: BTreeSet::new(),
            recovered_launches: BTreeSet::new(),
            completed: Vec::new(),
            path: witness_path,
            legacy: false,
            recovered: BTreeSet::new(),
            poisoned: false,
            published: None,
            #[cfg(test)]
            fail_before_sync: false,
            #[cfg(test)]
            stop_after_receipt_sync: false,
        };
        let required = journal.read(REQUIRED)?;
        if required.as_deref().is_some_and(|v| v != b"v1\n") {
            bail!("unknown execution journal marker")
        }
        if let Some(bytes) = journal.read(FILE)? {
            if required.is_none() {
                bail!("execution journal initialization marker missing; recovery unconfirmed")
            }
            // Typed parsing rejects duplicate struct fields, unknown fields and trailing JSON.
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Legacy {
                version: u32,
                scopes: Vec<Record>,
            }
            let version = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v["version"].as_u64());
            let snapshot: Snapshot = if version == Some(1) {
                let old: Legacy = serde_json::from_slice(&bytes)
                    .map_err(|_| anyhow::anyhow!("invalid legacy journal schema"))?;
                Snapshot {
                    version: old.version,
                    scopes: old.scopes,
                    launches: Vec::new(),
                    completed: Vec::new(),
                }
            } else {
                serde_json::from_slice(&bytes)
                    .map_err(|_| anyhow::anyhow!("invalid execution journal schema"))?
            };
            if ![1, 2, 3].contains(&snapshot.version)
                || snapshot.scopes.len() > MAX_SCOPES
                || snapshot.version == 2 && snapshot.launches.len() + snapshot.completed.len() > 256
                || snapshot.launches.len() > MAX_LAUNCHES
                || snapshot.completed.len()
                    > if snapshot.version == 3 {
                        MAX_COMPLETED
                    } else {
                        256
                    }
            {
                bail!("unknown execution journal version or capacity")
            }
            snapshot.validate_links()?;
            if snapshot.version == 1 {
                if !snapshot.launches.is_empty() || !snapshot.completed.is_empty() {
                    bail!("legacy journal cannot contain witness proof")
                }
                journal.legacy = true;
            }
            let mut instances = BTreeSet::new();
            for launch in snapshot.launches {
                launch.validate()?;
                if !instances.insert(launch.worker_instance.clone())
                    || !journal.launches.insert(launch)
                {
                    bail!("duplicate incarnation witness")
                }
            }
            for receipt in snapshot.completed {
                receipt.validate()?;
                if !instances.insert(receipt.worker_instance.clone()) {
                    bail!("duplicate incarnation completion")
                }
                journal.completed.push(receipt);
            }
            journal.recovered_launches = journal.launches.clone();
            for record in snapshot.scopes {
                record.validate()?;
                if !journal.records.insert(record) {
                    bail!("duplicate execution journal scope")
                }
            }
            journal.recovered = journal.records.clone();
            journal.published = Some(bytes);
        } else {
            if required.is_some() || require_existing {
                bail!("required execution journal is missing; legacy state recovery unconfirmed")
            }
            journal.persist(&BTreeSet::new())?;
        }
        if required.is_none() {
            journal.replace(REQUIRED, b"v1\n")?;
        }
        if !journal.legacy && journal.completed.len() > MAX_COMPLETED {
            // v2 receipts are ordered as stored (historically sorted), not wall time.
            // Collect only durable completed identities before forgetting their metadata.
            let excess = journal.completed.len() - MAX_COMPLETED;
            for receipt in &journal.completed[..excess] {
                crate::exec_witness::collect_completed(&journal.path, receipt)?;
            }
            let original = journal.completed.clone();
            journal.completed.drain(..excess);
            if let Err(e) = journal.persist(&journal.records.clone()) {
                journal.completed = original;
                return Err(e);
            }
        }
        Ok(journal)
    }
    fn read(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                c(name).as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        let mut file = owned(fd).context("execution journal open failed")?;
        let before = file.metadata()?;
        if !before.is_file()
            || before.uid() != unsafe { libc::geteuid() }
            || before.nlink() != 1
            || before.mode() & 0o777 != 0o600
            || before.len() > MAX_BYTES as u64
        {
            bail!("unsafe execution journal file")
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        let mut at: libc::stat = unsafe { std::mem::zeroed() };
        if bytes.len() > MAX_BYTES
            || before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.len() != after.len()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
            || unsafe {
                libc::fstatat(
                    self.directory.as_raw_fd(),
                    c(name).as_ptr(),
                    &mut at,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            || at.st_ino != after.ino()
            || at.st_dev as u64 != after.dev()
        {
            bail!("execution journal changed while reading")
        }
        Ok(Some(bytes))
    }
    fn replace(&self, name: &str, bytes: &[u8]) -> Result<()> {
        // Refuse unsafe pre-existing targets, rather than silently replacing them.
        self.read(name)?;
        let mut random = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let temp = format!(
            ".exec-journal-{}.tmp",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let mut file = owned(unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                c(&temp).as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        })?;
        let result = (|| -> Result<()> {
            file.write_all(bytes)?;
            #[cfg(test)]
            if self.fail_before_sync {
                bail!("injected journal sync failure")
            }
            file.sync_all()?;
            if unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    c(&temp).as_ptr(),
                    self.directory.as_raw_fd(),
                    c(name).as_ptr(),
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            self.directory.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            unsafe { libc::unlinkat(self.directory.as_raw_fd(), c(&temp).as_ptr(), 0) };
        }
        result.context("execution journal durability unconfirmed")
    }
    fn persist(&mut self, records: &BTreeSet<Record>) -> Result<()> {
        let snapshot = Snapshot {
            version: 3,
            scopes: records.iter().cloned().collect(),
            launches: self.launches.iter().cloned().collect(),
            completed: self.completed.clone(),
        };
        snapshot.validate_links()?;
        let bytes = serde_json::to_vec(&snapshot)?;
        if bytes.len() > MAX_BYTES {
            bail!("execution journal exceeds bound")
        }
        if self.read(FILE)? != self.published {
            bail!("execution journal changed or missing; recovery unconfirmed")
        }
        self.replace(FILE, &bytes)?;
        self.published = Some(bytes);
        Ok(())
    }
    fn verify_current(&mut self) -> Result<()> {
        let result = (|| -> Result<()> {
            if self.read(FILE)? != self.published
                || self.read(REQUIRED)?.as_deref() != Some(b"v1\n")
            {
                bail!("execution journal changed or missing; recovery unconfirmed")
            }
            Ok(())
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result.context("execution journal durability unconfirmed")
    }
    pub fn record(&mut self, scope: &crate::exec_scope::Scope) -> Result<()> {
        if self.poisoned || self.legacy {
            bail!("execution journal failed; cleanup unconfirmed")
        }
        self.verify_current()?;
        let record = Record::from_scope(scope);
        record.validate()?;
        if !self
            .launches
            .iter()
            .any(|l| l.vm == record.vm && l.worker_instance == record.worker_instance)
        {
            bail!("execution scope lacks original launch witness")
        }
        if self.records.contains(&record) {
            return Ok(());
        }
        if self.records.len() >= MAX_SCOPES {
            bail!("execution journal capacity reached")
        }
        let mut next = self.records.clone();
        next.insert(record);
        if let Err(error) = self.persist(&next) {
            self.poisoned = true;
            return Err(error.context("execution journal durability unconfirmed"));
        }
        self.records = next;
        Ok(())
    }
    pub fn complete(&mut self, scope: &crate::exec_scope::Scope) -> Result<()> {
        if self.poisoned || self.legacy {
            bail!("execution journal failed; cleanup unconfirmed")
        }
        self.verify_current()?;
        let record = Record::from_scope(scope);
        if self.recovered.contains(&record) {
            bail!("recovered execution scope requires reconciliation; cleanup unconfirmed")
        }
        let mut next = self.records.clone();
        next.remove(&record);
        if let Err(error) = self.persist(&next) {
            self.poisoned = true;
            return Err(error.context("execution journal durability unconfirmed"));
        }
        self.records = next;
        Ok(())
    }
    pub fn admit_launch(&mut self, vm: &str, instance: &str) -> Result<()> {
        if self.poisoned || self.legacy {
            bail!("execution journal recovery required")
        }
        self.verify_current()?;
        if self.launches.len() >= MAX_LAUNCHES {
            bail!("pending incarnation capacity reached; unresolved records retained")
        }
        if self
            .launches
            .iter()
            .any(|l| l.vm == vm || l.worker_instance == instance)
            || self.completed.iter().any(|l| l.worker_instance == instance)
        {
            bail!("original incarnation requires explicit reconciliation")
        }
        Ok(())
    }
    pub fn launch(&mut self, identity: crate::exec_witness::Identity) -> Result<()> {
        identity.validate()?;
        self.admit_launch(&identity.vm, &identity.worker_instance)?;
        self.launches.insert(identity.clone());
        if let Err(e) = self.persist(&self.records.clone()) {
            self.launches.remove(&identity);
            self.poisoned = true;
            return Err(e);
        }
        Ok(())
    }
    pub fn current_launch(&self, vm: &str, instance: &str) -> bool {
        !self.poisoned
            && !self.legacy
            && self.launches.iter().any(|l| {
                l.vm == vm && l.worker_instance == instance && !self.recovered_launches.contains(l)
            })
    }
    pub fn incarnations(&self, vm: &str) -> Vec<crate::exec_witness::Identity> {
        self.launches
            .iter()
            .filter(|l| l.vm == vm)
            .cloned()
            .collect()
    }
    pub fn pending_launch(&self, vm: &str) -> bool {
        self.launches.iter().any(|l| l.vm == vm)
    }
    pub fn reconcile(&mut self, path: &Path, vm: &str, instance: &str) -> Result<()> {
        if self.poisoned || self.legacy {
            bail!("execution journal recovery unconfirmed; no usable witness proof")
        }
        self.verify_current()?;
        if self
            .completed
            .iter()
            .any(|l| l.vm == vm && l.worker_instance == instance)
        {
            if let Err(e) = self.directory.sync_all() {
                self.poisoned = true;
                return Err(e.into());
            }
            if let Some(receipt) = self
                .completed
                .iter()
                .find(|l| l.vm == vm && l.worker_instance == instance)
            {
                if crate::exec_witness::collect_completed(&self.path, receipt).is_err() {
                    eprintln!("execution witness metadata cleanup deferred");
                }
            }
            return Ok(());
        }
        let identity = self
            .launches
            .iter()
            .find(|l| l.vm == vm && l.worker_instance == instance)
            .cloned()
            .context("unknown or expired original incarnation; recovery unconfirmed")?;
        let guard = crate::exec_witness::ReconciliationGuard::acquire(path, &identity)?;
        guard.verify()?;
        let next: BTreeSet<_> = self
            .records
            .iter()
            .filter(|s| !(s.vm == vm && s.worker_instance == instance))
            .cloned()
            .collect();
        let previous_completed = self.completed.clone();
        if self.completed.len() == MAX_COMPLETED {
            crate::exec_witness::collect_completed(&self.path, &self.completed[0])?;
            self.completed.remove(0);
        }
        self.launches.remove(&identity);
        self.completed.push(identity.clone());
        if let Err(e) = self.persist(&next) {
            self.completed = previous_completed;
            self.launches.insert(identity);
            self.poisoned = true;
            return Err(e);
        }
        self.records = next;
        self.recovered
            .retain(|s| !(s.vm == vm && s.worker_instance == instance));
        self.recovered_launches.remove(&identity);
        drop(guard);
        #[cfg(test)]
        if self.stop_after_receipt_sync {
            bail!("injected interruption after durable receipt before metadata GC")
        }
        if crate::exec_witness::collect_completed(&self.path, &identity).is_err() {
            eprintln!("execution witness metadata cleanup deferred");
        }
        Ok(())
    }
    pub fn recovery_required(&self, owner: Option<&str>, vm: Option<&str>) -> bool {
        self.poisoned
            || self.legacy
            || owner.is_none()
                && self
                    .recovered_launches
                    .iter()
                    .any(|l| vm.is_none_or(|v| l.vm == v))
            || self.recovered.iter().any(|r| {
                owner.is_none_or(|o| r.owner_sha256 == owner_hash(o))
                    && vm.is_none_or(|v| r.vm == v)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{symlink, PermissionsExt},
    };
    fn private_tempdir() -> tempfile::TempDir {
        let dir =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }
    fn scope() -> crate::exec_scope::Scope {
        crate::exec_scope::Scope {
            vm: "test".into(),
            worker_instance: "a".repeat(64),
            owner: "b".repeat(64),
            epoch: 1,
        }
    }
    fn with_scope_launch(dir: &Path) -> (Journal, crate::exec_witness::Witness) {
        let mut journal = Journal::open(dir, false).unwrap();
        let scope = scope();
        let w =
            crate::exec_witness::Witness::create(dir, &scope.vm, &scope.worker_instance).unwrap();
        journal.launch(w.identity.clone()).unwrap();
        (journal, w)
    }
    #[test]
    fn inconsistent_v2_and_v3_links_are_rejected_not_completed() {
        let dir = private_tempdir();
        let (mut journal, w) = with_scope_launch(dir.path());
        journal.record(&scope()).unwrap();
        let original: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join(FILE)).unwrap()).unwrap();
        for version in [2, 3] {
            for mode in [
                "missing_launch",
                "wrong_vm",
                "completed_scope",
                "duplicate_instance",
                "duplicate_vm",
            ] {
                let mut v = original.clone();
                v["version"] = serde_json::json!(version);
                match mode {
                    "missing_launch" => v["launches"] = serde_json::json!([]),
                    "wrong_vm" => v["launches"][0]["vm"] = serde_json::json!("other"),
                    "completed_scope" => {
                        v["completed"] = v["launches"].clone();
                        v["launches"] = serde_json::json!([])
                    }
                    "duplicate_instance" => {
                        let mut x = v["launches"][0].clone();
                        x["vm"] = serde_json::json!("other");
                        v["completed"] = serde_json::json!([x]);
                    }
                    "duplicate_vm" => {
                        let mut x = v["launches"][0].clone();
                        x["worker_instance"] = serde_json::json!("c".repeat(64));
                        v["launches"].as_array_mut().unwrap().push(x)
                    }
                    _ => unreachable!(),
                }
                fs::write(dir.path().join(FILE), serde_json::to_vec(&v).unwrap()).unwrap();
                assert!(Journal::open(dir.path(), true).is_err(), "{mode}");
            }
        }
        drop(w);
    }
    fn finish_number(journal: &mut Journal, dir: &Path, n: u64) -> crate::exec_witness::Identity {
        let vm = format!("vm{}", n % 3);
        let instance = format!("{n:064x}");
        let w = crate::exec_witness::Witness::create(dir, &vm, &instance).unwrap();
        let id = w.identity.clone();
        journal.launch(id.clone()).unwrap();
        journal
            .record(&crate::exec_scope::Scope {
                vm: vm.clone(),
                worker_instance: instance.clone(),
                owner: "e".repeat(64),
                epoch: n,
            })
            .unwrap();
        drop(w);
        // A concurrently forking test can briefly inherit the witness OFD before exec.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while let Err(error) = journal.reconcile(dir, &vm, &instance) {
            assert!(
                std::time::Instant::now() < deadline,
                "witness stayed locked: {error}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        id
    }
    #[test]
    fn more_than_256_completions_preserve_active_other_vm_and_expire_only_old_receipts() {
        let dir = private_tempdir();
        let mut j = Journal::open(dir.path(), false).unwrap();
        let active =
            crate::exec_witness::Witness::create(dir.path(), "active", &"f".repeat(64)).unwrap();
        j.launch(active.identity.clone()).unwrap();
        let active_scope = crate::exec_scope::Scope {
            vm: "active".into(),
            worker_instance: "f".repeat(64),
            owner: "d".repeat(64),
            epoch: 1,
        };
        j.record(&active_scope).unwrap();
        let mut first = None;
        let mut latest = None;
        for n in 1..=300 {
            let id = finish_number(&mut j, dir.path(), n);
            if first.is_none() {
                first = Some(id.clone())
            }
            latest = Some(id);
        }
        assert_eq!(j.completed.len(), 64);
        assert_eq!(j.launches.len(), 1);
        assert!(j.records.contains(&Record::from_scope(&active_scope)));
        assert!(dir
            .path()
            .join(format!("{}.witness", active.identity.worker_instance))
            .exists());
        let count = fs::read_dir(dir.path())
            .unwrap()
            .filter(|p| {
                p.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|e| e == "witness")
            })
            .count();
        assert_eq!(count, 1);
        let before = fs::read(dir.path().join(FILE)).unwrap();
        let old = first.unwrap();
        assert!(j
            .reconcile(dir.path(), &old.vm, &old.worker_instance)
            .unwrap_err()
            .to_string()
            .contains("unknown or expired"));
        assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), before);
        let latest = latest.unwrap();
        assert!(!dir
            .path()
            .join(format!("{}.witness", latest.worker_instance))
            .exists());
        j.reconcile(dir.path(), &latest.vm, &latest.worker_instance)
            .unwrap();
        drop(j);
        let mut restored = Journal::open(dir.path(), true).unwrap();
        assert!(restored.recovery_required(None, Some("active")));
        assert_eq!(restored.completed.len(), 64);
        restored
            .reconcile(dir.path(), &latest.vm, &latest.worker_instance)
            .unwrap();
        assert!(restored
            .reconcile(dir.path(), &old.vm, &old.worker_instance)
            .is_err());
    }
    #[test]
    fn interruption_after_receipt_sync_retains_retryable_gc_and_eviction_failure_keeps_obligations()
    {
        let dir = private_tempdir();
        let mut j = Journal::open(dir.path(), false).unwrap();
        let first =
            crate::exec_witness::Witness::create(dir.path(), "first", &format!("{:064x}", 1000))
                .unwrap();
        let id = first.identity.clone();
        j.launch(id.clone()).unwrap();
        drop(first);
        j.stop_after_receipt_sync = true;
        assert!(j
            .reconcile(dir.path(), &id.vm, &id.worker_instance)
            .is_err());
        let path = dir.path().join(format!("{}.witness", id.worker_instance));
        assert!(path.exists());
        drop(j);
        let mut j = Journal::open(dir.path(), true).unwrap();
        // Keep the oldest completed witness until eviction; other completions GC normally.
        for n in 1..64 {
            finish_number(&mut j, dir.path(), n);
        }
        let w = crate::exec_witness::Witness::create(dir.path(), "next", &format!("{:064x}", 2000))
            .unwrap();
        let next = w.identity.clone();
        j.launch(next.clone()).unwrap();
        drop(w);
        let before = fs::read(dir.path().join(FILE)).unwrap();
        j.fail_before_sync = true;
        assert!(j
            .reconcile(dir.path(), &next.vm, &next.worker_instance)
            .is_err());
        assert!(!path.exists());
        assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), before);
        assert!(j.pending_launch("next"));
        drop(j);
        let mut restored = Journal::open(dir.path(), true).unwrap();
        restored
            .reconcile(dir.path(), &id.vm, &id.worker_instance)
            .unwrap();
        assert!(restored.pending_launch("next"));
        restored
            .reconcile(dir.path(), &next.vm, &next.worker_instance)
            .unwrap();
        assert!(restored
            .reconcile(dir.path(), &id.vm, &id.worker_instance)
            .is_err());
        assert!(!restored.pending_launch("next"));
    }
    #[test]
    fn unsafe_completed_witness_blocks_eviction_without_touching_pending_or_replacement() {
        let dir = private_tempdir();
        let mut j = Journal::open(dir.path(), false).unwrap();
        let w =
            crate::exec_witness::Witness::create(dir.path(), "first", &format!("{:064x}", 1000))
                .unwrap();
        let id = w.identity.clone();
        j.launch(id.clone()).unwrap();
        drop(w);
        j.stop_after_receipt_sync = true;
        assert!(j
            .reconcile(dir.path(), &id.vm, &id.worker_instance)
            .is_err());
        j.stop_after_receipt_sync = false;
        let path = dir.path().join(format!("{}.witness", id.worker_instance));
        fs::rename(&path, dir.path().join("original-preserved")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        // Existing cleanup proof remains valid, but replacement is never removed.
        j.reconcile(dir.path(), &id.vm, &id.worker_instance)
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        for n in 1..64 {
            finish_number(&mut j, dir.path(), n);
        }
        let w = crate::exec_witness::Witness::create(dir.path(), "next", &format!("{:064x}", 2000))
            .unwrap();
        let next = w.identity.clone();
        j.launch(next.clone()).unwrap();
        drop(w);
        let before = fs::read(dir.path().join(FILE)).unwrap();
        assert!(j
            .reconcile(dir.path(), &next.vm, &next.worker_instance)
            .is_err());
        assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), before);
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        assert!(j.pending_launch("next"));
        assert_eq!(j.completed.len(), 64);
    }
    #[test]
    fn completed_retry_and_eviction_do_not_touch_new_instance_with_same_inode() {
        let dir = private_tempdir();
        let mut j = Journal::open(dir.path(), false).unwrap();
        let old = crate::exec_witness::Witness::create(dir.path(), "vm", &"a".repeat(64)).unwrap();
        let old_id = old.identity.clone();
        j.launch(old_id.clone()).unwrap();
        drop(old);
        j.stop_after_receipt_sync = true;
        assert!(j
            .reconcile(dir.path(), "vm", &old_id.worker_instance)
            .is_err());
        j.stop_after_receipt_sync = false;
        // Deterministically model inode reuse: same inode now has a distinct
        // instance-specific pathname and exact new immutable content. No PID or
        // inode-only search may cause an old receipt to remove that new file.
        let new_id = crate::exec_witness::Identity {
            vm: "vm".into(),
            worker_instance: "b".repeat(64),
            device: old_id.device,
            inode: old_id.inode,
        };
        let old_path = dir
            .path()
            .join(format!("{}.witness", old_id.worker_instance));
        let new_path = dir
            .path()
            .join(format!("{}.witness", new_id.worker_instance));
        fs::rename(&old_path, &new_path).unwrap();
        let content = serde_json::to_vec(
            &serde_json::json!({"version":1,"vm":"vm","worker_instance":new_id.worker_instance}),
        )
        .unwrap();
        fs::write(&new_path, &content).unwrap();
        let holder = File::options()
            .read(true)
            .write(true)
            .open(&new_path)
            .unwrap();
        assert_eq!(holder.metadata().unwrap().ino(), old_id.inode);
        assert_eq!(
            unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        j.launch(new_id.clone()).unwrap();
        let active = crate::exec_scope::Scope {
            vm: "vm".into(),
            worker_instance: new_id.worker_instance.clone(),
            owner: "c".repeat(64),
            epoch: 2,
        };
        j.record(&active).unwrap();
        let before = fs::read(dir.path().join(FILE)).unwrap();
        j.reconcile(dir.path(), "vm", &old_id.worker_instance)
            .unwrap();
        assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), before);
        for n in 1..=64 {
            finish_number(&mut j, dir.path(), n);
        }
        assert!(j
            .reconcile(dir.path(), "vm", &old_id.worker_instance)
            .unwrap_err()
            .to_string()
            .contains("unknown or expired"));
        assert_eq!(fs::read(&new_path).unwrap(), content);
        assert!(j.launches.contains(&new_id));
        assert!(j.records.contains(&Record::from_scope(&active)));
    }
    #[test]
    fn bounded_v2_migration_preserves_pending_and_safely_collects_only_evicted_completed() {
        let dir = private_tempdir();
        let mut j = Journal::open(dir.path(), false).unwrap();
        let mut completed = Vec::new();
        for n in 1..=70 {
            completed.push(finish_number(&mut j, dir.path(), n));
        }
        let active =
            crate::exec_witness::Witness::create(dir.path(), "active", &"f".repeat(64)).unwrap();
        j.launch(active.identity.clone()).unwrap();
        let bytes = serde_json::to_vec(&Snapshot {
            version: 2,
            scopes: Vec::new(),
            launches: vec![active.identity.clone()],
            completed,
        })
        .unwrap();
        fs::write(dir.path().join(FILE), bytes).unwrap();
        drop(j);
        let migrated = Journal::open(dir.path(), true).unwrap();
        assert_eq!(migrated.completed.len(), 64);
        assert!(migrated.recovery_required(None, Some("active")));
        assert!(dir
            .path()
            .join(format!("{}.witness", active.identity.worker_instance))
            .exists());
        let v: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join(FILE)).unwrap()).unwrap();
        assert_eq!(v["version"], 3);
    }
    #[test]
    fn completion_history_does_not_consume_pending_capacity() {
        let dir = private_tempdir();
        let mut journal = Journal::open(dir.path(), false).unwrap();
        for n in 1..=MAX_COMPLETED {
            journal.completed.push(crate::exec_witness::Identity {
                vm: format!("done{n}"),
                worker_instance: format!("{n:064x}"),
                device: 1,
                inode: n as u64,
            });
        }
        for n in 1..=MAX_LAUNCHES {
            journal.launches.insert(crate::exec_witness::Identity {
                vm: format!("active{n}"),
                worker_instance: format!("{:064x}", n + 1000),
                device: 1,
                inode: n as u64 + 1000,
            });
        }
        journal.persist(&BTreeSet::new()).unwrap();
        let before = fs::read(dir.path().join(FILE)).unwrap();
        assert!(journal.admit_launch("extra", &"f".repeat(64)).is_err());
        assert_eq!(journal.launches.len(), 256);
        assert_eq!(journal.completed.len(), 64);
        assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), before);
        journal.launches.pop_first();
        journal.persist(&BTreeSet::new()).unwrap();
        journal.admit_launch("extra", &"f".repeat(64)).unwrap();
    }
    #[test]
    fn witness_reconciliation_locked_restart_idempotency_and_new_incarnation() {
        let dir = private_tempdir();
        let s = scope();
        let mut journal = Journal::open(dir.path(), false).unwrap();
        let witness =
            crate::exec_witness::Witness::create(dir.path(), &s.vm, &s.worker_instance).unwrap();
        journal.launch(witness.identity.clone()).unwrap();
        journal.record(&s).unwrap();
        let original = fs::read(dir.path().join(FILE)).unwrap();
        assert!(journal
            .reconcile(dir.path(), &s.vm, &s.worker_instance)
            .is_err());
        assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), original);
        drop(journal);
        let mut restarted = Journal::open(dir.path(), true).unwrap();
        assert!(restarted.recovery_required(None, Some(&s.vm)));
        assert!(restarted
            .reconcile(dir.path(), &s.vm, &s.worker_instance)
            .is_err());
        drop(witness);
        restarted
            .reconcile(dir.path(), &s.vm, &s.worker_instance)
            .unwrap();
        assert!(!restarted.recovery_required(None, Some(&s.vm)));
        let new = crate::exec_witness::Witness::create(dir.path(), &s.vm, &"c".repeat(64)).unwrap();
        restarted.launch(new.identity.clone()).unwrap();
        let mut next = s.clone();
        next.worker_instance = new.identity.worker_instance.clone();
        restarted.record(&next).unwrap();
        let before = fs::read(dir.path().join(FILE)).unwrap();
        restarted
            .reconcile(dir.path(), &s.vm, &s.worker_instance)
            .unwrap();
        assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), before);
        assert!(restarted.records.contains(&Record::from_scope(&next)));
        assert!(restarted
            .reconcile(dir.path(), "other", &s.worker_instance)
            .is_err());
        assert!(restarted
            .reconcile(dir.path(), &s.vm, &"d".repeat(64))
            .is_err());
    }
    #[test]
    fn reconciliation_sync_failure_keeps_fence_and_legacy_cannot_be_backfilled() {
        let dir = private_tempdir();
        let s = scope();
        let mut journal = Journal::open(dir.path(), false).unwrap();
        let w =
            crate::exec_witness::Witness::create(dir.path(), &s.vm, &s.worker_instance).unwrap();
        journal.launch(w.identity.clone()).unwrap();
        journal.record(&s).unwrap();
        drop(w);
        let before = fs::read(dir.path().join(FILE)).unwrap();
        journal.fail_before_sync = true;
        assert!(journal
            .reconcile(dir.path(), &s.vm, &s.worker_instance)
            .is_err());
        assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), before);
        assert!(journal.recovery_required(None, None));
        let old = private_tempdir();
        fs::write(old.path().join(FILE), b"{\"version\":1,\"scopes\":[]}").unwrap();
        fs::set_permissions(old.path().join(FILE), fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(old.path().join(REQUIRED), b"v1\n").unwrap();
        fs::set_permissions(old.path().join(REQUIRED), fs::Permissions::from_mode(0o600)).unwrap();
        let mut legacy = Journal::open(old.path(), true).unwrap();
        assert!(legacy.recovery_required(None, None));
        assert!(legacy
            .reconcile(old.path(), &s.vm, &s.worker_instance)
            .is_err());
    }
    #[test]
    fn durable_scope_is_recovery_only_without_raw_owner_or_command_data() {
        let dir = private_tempdir();
        let (mut journal, _witness) = with_scope_launch(dir.path());
        journal.record(&scope()).unwrap();
        let bytes = fs::read(dir.path().join(FILE)).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(&scope().owner));
        assert_eq!(
            fs::metadata(dir.path().join(FILE)).unwrap().mode() & 0o777,
            0o600
        );
        let mut restarted = Journal::open(dir.path(), true).unwrap();
        assert!(restarted.recovery_required(Some(&scope().owner), Some("test")));
        assert!(!restarted.recovery_required(Some(&"c".repeat(64)), Some("test")));
        assert!(restarted.complete(&scope()).is_err());
        journal.complete(&scope()).unwrap();
        assert!(!Journal::open(dir.path(), true)
            .unwrap()
            .recovery_required(Some(&scope().owner), None));
        drop(_witness);
        journal
            .reconcile(dir.path(), "test", &scope().worker_instance)
            .unwrap();
        assert!(!Journal::open(dir.path(), true)
            .unwrap()
            .recovery_required(None, None));
    }
    #[test]
    fn missing_required_legacy_unknown_and_corrupt_journals_never_become_empty() {
        let legacy = private_tempdir();
        assert!(Journal::open(legacy.path(), true).is_err());
        let dir = private_tempdir();
        Journal::open(dir.path(), false).unwrap();
        fs::remove_file(dir.path().join(FILE)).unwrap();
        assert!(Journal::open(dir.path(), false).is_err());
        for bytes in [
            br#"{"version":2,"scopes":[]}"#.as_slice(),
            br#"{"version":1,"version":1,"scopes":[]}"#,
            br#"{"version":1,"scopes":[],"extra":true}"#,
            b"{",
            b"null",
        ] {
            fs::write(dir.path().join(FILE), bytes).unwrap();
            fs::set_permissions(dir.path().join(FILE), fs::Permissions::from_mode(0o600)).unwrap();
            assert!(Journal::open(dir.path(), false).is_err());
        }
    }
    #[test]
    fn unsafe_files_and_oversize_fail_closed_without_fifo_block() {
        for kind in ["symlink", "hardlink", "fifo", "mode", "size"] {
            let dir = private_tempdir();
            Journal::open(dir.path(), false).unwrap();
            let path = dir.path().join(FILE);
            match kind {
                "symlink" => {
                    fs::rename(&path, dir.path().join("original")).unwrap();
                    symlink("original", &path).unwrap();
                }
                "hardlink" => fs::hard_link(&path, dir.path().join("alias")).unwrap(),
                "fifo" => {
                    fs::remove_file(&path).unwrap();
                    use std::os::unix::ffi::OsStrExt;
                    assert_eq!(
                        unsafe {
                            libc::mkfifo(
                                CString::new(path.as_os_str().as_bytes()).unwrap().as_ptr(),
                                0o600,
                            )
                        },
                        0
                    );
                }
                "mode" => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
                "size" => {
                    File::options()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_len((MAX_BYTES + 1) as u64)
                        .unwrap();
                }
                _ => unreachable!(),
            }
            assert!(Journal::open(dir.path(), false).is_err(), "{kind}");
        }
    }
    #[test]
    fn actual_filesystem_write_failure_preserves_scope_and_poison_is_sticky() {
        let dir = private_tempdir();
        let (mut journal, _witness) = with_scope_launch(dir.path());
        journal.record(&scope()).unwrap();
        let before = fs::read(dir.path().join(FILE)).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let result = journal.complete(&scope());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), before);
        assert!(journal.complete(&scope()).is_err());
        assert!(Journal::open(dir.path(), true)
            .unwrap()
            .recovery_required(None, Some("test")));
    }
    #[test]
    fn changed_live_journal_is_not_overwritten_as_empty() {
        let dir = private_tempdir();
        let mut journal = Journal::open(dir.path(), false).unwrap();
        fs::write(dir.path().join(FILE), b"invalid").unwrap();
        assert!(journal.record(&scope()).is_err());
        assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), b"invalid");
    }
    #[test]
    fn journal_capacity_and_invalid_records_are_not_silently_discarded() {
        let dir = private_tempdir();
        let (mut journal, _witness) = with_scope_launch(dir.path());
        let records = (1..=MAX_SCOPES)
            .map(|epoch| {
                let mut r = Record::from_scope(&scope());
                r.epoch = epoch as u64;
                r
            })
            .collect::<BTreeSet<_>>();
        journal.persist(&records).unwrap();
        journal.records = records.clone();
        let mut next = scope();
        next.epoch = (MAX_SCOPES + 1) as u64;
        assert!(journal.record(&next).is_err());
        assert_eq!(journal.records, records);
        let bytes = journal.read(FILE).unwrap().unwrap();
        let snapshot: Snapshot = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(snapshot.scopes.len(), MAX_SCOPES);
        for change in ["duplicate", "epoch", "vm", "worker", "owner"] {
            let mut record = Record::from_scope(&scope());
            match change {
                "epoch" => record.epoch = 0,
                "vm" => record.vm = "../outside".into(),
                "worker" => record.worker_instance = "invalid".into(),
                "owner" => record.owner_sha256 = "secret".into(),
                _ => (),
            }
            let mut scopes = vec![record.clone()];
            if change == "duplicate" {
                scopes.push(record);
            }
            fs::write(
                dir.path().join(FILE),
                serde_json::to_vec(&Snapshot {
                    version: 2,
                    scopes,
                    launches: Vec::new(),
                    completed: Vec::new(),
                })
                .unwrap(),
            )
            .unwrap();
            assert!(Journal::open(dir.path(), true).is_err(), "{change}");
        }
    }
    #[test]
    fn repeating_scope_checks_required_marker_and_does_not_recreate_deleted_state() {
        for name in [FILE, REQUIRED] {
            let dir = private_tempdir();
            let (mut journal, _witness) = with_scope_launch(dir.path());
            journal.record(&scope()).unwrap();
            fs::remove_file(dir.path().join(name)).unwrap();
            assert!(journal.record(&scope()).is_err());
            assert!(!dir.path().join(name).exists());
            assert!(Journal::open(dir.path(), true).is_err());
            assert!(journal.recovery_required(None, None));
        }
    }
}
