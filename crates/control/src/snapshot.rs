//! Named Linux snapshots. Storage integrity is separate from backend capture.
//!
//! Manager must fence lifecycle/input/exec admission for the complete operation.
//! In particular a paused VM is not a retired disk owner. Full-state creation
//! needs a completed backend RAM/CPU/device save plus a disk captured within the
//! SAME quiesced interval. This module neither claims that disk copy supplies
//! that proof nor restores into a live VM. Failed hidden staging is retained.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;
use std::{
    collections::BTreeSet,
    fs::File,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
#[path = "snapshot_fs.rs"]
pub(crate) mod storage;
use storage::Dir;

const SCHEMA: u32 = 1;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    DiskRollback,
    FullState,
    Suspend,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Disk,
    MachineState,
}
impl Role {
    fn file(self) -> &'static str {
        match self {
            Self::Disk => "disk.raw",
            Self::MachineState => "machine.vmstate",
        }
    }
}
/// A read snapshot: its metadata, its directory and its opened artifacts.
type SnapshotRead = (Metadata, Dir, Vec<(Role, File)>);
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compatibility {
    pub backend: String,
    pub backend_version: String,
    pub backend_sha256: String,
    pub machine: String,
    pub architecture: String,
    pub cpu_model: String,
    pub cpus: u32,
    pub memory_mib: u64,
    pub config_sha256: String,
    pub devices_sha256: String,
    pub disk_bytes: Option<u64>,
}
impl Compatibility {
    pub fn config_hash(config: &crate::Config) -> Result<String> {
        ensure!(
            config.guest == "linux",
            "snapshot store currently supports Linux only"
        );
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(config)?)))
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.backend.as_str(), "qemu_hvf" | "apple_virtualization")
                && self.architecture == "aarch64",
            "unsupported snapshot backend/architecture"
        );
        for value in [&self.backend_version, &self.machine, &self.cpu_model] {
            ensure!(
                !value.is_empty() && value.len() <= 128 && value.is_ascii(),
                "invalid snapshot compatibility identity"
            );
        }
        for value in [
            &self.backend_sha256,
            &self.config_sha256,
            &self.devices_sha256,
        ] {
            hex(value, 64)?;
        }
        ensure!(
            (1..=128).contains(&self.cpus) && (256..=1048576).contains(&self.memory_mib),
            "invalid snapshot CPU/RAM"
        );
        if let Some(bytes) = self.disk_bytes {
            ensure!(
                (512..=storage::MAX_ARTIFACT).contains(&bytes) && bytes % 512 == 0,
                "snapshot raw disk size invalid"
            );
        }
        Ok(())
    }
    pub fn required_reservation(&self, kind: Kind) -> Result<u64> {
        self.validate()?;
        let disk = self.disk_bytes.unwrap_or(0);
        if kind == Kind::DiskRollback {
            return Ok(disk);
        }
        disk.checked_add(
            self.memory_mib
                .checked_mul(1024 * 1024)
                .context("snapshot RAM overflow")?,
        )
        .and_then(|n| n.checked_add(64 * 1024 * 1024))
        .context("snapshot reservation overflow")
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub role: Role,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureProof {
    pub capture_id: String,
    pub backend_receipt_sha256: String,
    pub source_worker_instance: Option<String>,
    pub backend_completed: bool,
    pub source_quiesced: bool,
    pub ram_cpu_devices_saved: bool,
    pub disk_in_same_quiesced_interval: bool,
    /// Present only for a protective disk_rollback, naming the restore source's capture ID.
    pub protects_restore_of: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub schema: u32,
    pub vm: String,
    pub name: String,
    pub kind: Kind,
    pub created_unix_ms: u64,
    pub compatibility: Compatibility,
    pub artifacts: Vec<Artifact>,
    pub proof: CaptureProof,
    pub capture_verified: bool,
    /// Capture integrity is not an executed restore round trip.
    pub restore_verified: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    schema: u32,
    vm: String,
}
fn hex(value: &str, len: usize) -> Result<()> {
    ensure!(
        value.len() == len
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid snapshot digest/identity"
    );
    Ok(())
}
fn name(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 64
            && value.as_bytes()[0].is_ascii_alphanumeric()
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
        "invalid snapshot name"
    );
    Ok(())
}
fn roles(kind: Kind, disk: bool) -> BTreeSet<Role> {
    let mut result = BTreeSet::new();
    if disk {
        result.insert(Role::Disk);
    }
    if kind != Kind::DiskRollback {
        result.insert(Role::MachineState);
    }
    result
}
fn check_protection(
    source: &Metadata,
    protection: &Metadata,
    compatible: &Compatibility,
    current: (u64, String),
) -> Result<()> {
    ensure!(
        protection.vm == source.vm
            && protection.kind == Kind::DiskRollback
            && protection.compatibility == *compatible
            && protection.proof.protects_restore_of.as_deref()
                == Some(source.proof.capture_id.as_str()),
        "protective snapshot is not bound to this VM/restore"
    );
    let disk = protection
        .artifacts
        .iter()
        .find(|a| a.role == Role::Disk)
        .context("protective disk absent")?;
    ensure!(
        current == (disk.bytes, disk.sha256.clone()),
        "current disk differs from protective rollback; refuse restore"
    );
    Ok(())
}
impl Metadata {
    fn validate(&self, vm: &str, wanted: &str) -> Result<()> {
        ensure!(
            self.schema == SCHEMA && self.vm == vm && self.name == wanted && self.capture_verified,
            "snapshot format, VM ownership, name or capture state mismatch"
        );
        name(&self.name)?;
        hex(&self.proof.capture_id, 32)?;
        hex(&self.proof.backend_receipt_sha256, 64)?;
        self.compatibility.validate()?;
        ensure!(
            self.proof.backend_completed && self.proof.source_quiesced,
            "snapshot capture was not completed/quiesced"
        );
        if let Some(id) = &self.proof.source_worker_instance {
            hex(id, 64)?;
        }
        let full = self.kind != Kind::DiskRollback;
        ensure!(
            self.proof.ram_cpu_devices_saved == full,
            "disk rollback is not RAM/CPU/device state"
        );
        if full {
            ensure!(
                self.proof.source_worker_instance.is_some(),
                "full-state capture lacks source incarnation"
            );
            ensure!(
                self.proof.protects_restore_of.is_none(),
                "full state cannot impersonate protective disk rollback"
            );
        } else {
            ensure!(
                self.compatibility.disk_bytes.is_some(),
                "disk rollback needs a disk"
            );
        }
        if self.compatibility.disk_bytes.is_some() {
            ensure!(
                self.proof.disk_in_same_quiesced_interval,
                "disk capture consistency unproven"
            );
        }
        if let Some(id) = &self.proof.protects_restore_of {
            hex(id, 32)?;
        }
        let mut observed = BTreeSet::new();
        for artifact in &self.artifacts {
            ensure!(
                observed.insert(artifact.role),
                "duplicate snapshot artifact role"
            );
            hex(&artifact.sha256, 64)?;
            ensure!(
                (1..=storage::MAX_ARTIFACT).contains(&artifact.bytes),
                "invalid snapshot artifact size"
            );
            if artifact.role == Role::Disk {
                ensure!(
                    Some(artifact.bytes) == self.compatibility.disk_bytes,
                    "snapshot disk extent mismatch"
                );
            }
        }
        ensure!(
            observed == roles(self.kind, self.compatibility.disk_bytes.is_some()),
            "snapshot artifact set incomplete"
        );
        Ok(())
    }
}

pub struct SnapshotStore {
    directory: Dir,
}
pub struct VmSnapshots {
    directory: Dir,
    vm: String,
    lock: storage::Lock,
}
pub struct Capture<'a> {
    owner: &'a mut VmSnapshots,
    directory: Dir,
    staging: String,
    name: String,
    kind: Kind,
    compatibility: Compatibility,
    capture_id: String,
}
/// Root must hold its lifecycle fence and verify exact worker retirement before
/// setting worker_retired. The borrowed DiskGuard fences cooperating VZ/QEMU/image
/// users and is retained throughout the returned restore plan.
pub struct RestoreTarget<'a> {
    pub worker_retired: bool,
    pub disk: Option<(&'a crate::qemu_disk::DiskGuard, &'a File)>,
}
pub struct RestorePlan<'a> {
    pub metadata: Metadata,
    pub disk: Option<File>,
    pub machine_state: Option<File>,
    pub protection: Option<Metadata>,
    _owner: &'a mut VmSnapshots,
    _target: RestoreTarget<'a>,
}
impl SnapshotStore {
    pub fn open(state_dir: &Path) -> Result<Self> {
        let state = Dir::open(state_dir)?;
        let (directory, _) = state.child("snapshots", true)?;
        Ok(Self { directory })
    }
    pub fn lock_vm(&self, vm: &str) -> Result<VmSnapshots> {
        crate::validate_id(vm)?;
        let (directory, created) = self.directory.child(vm, true)?;
        if !created {
            // Do not even create a lock file inside an unrecognized directory.
            let owner: Owner = directory.json(".owner.json")?;
            ensure!(
                owner.schema == SCHEMA && owner.vm == vm,
                "snapshot VM directory ownership mismatch"
            );
        }
        let lock = directory.lock()?;
        if created {
            directory.write_json(
                ".owner.json",
                &Owner {
                    schema: SCHEMA,
                    vm: vm.into(),
                },
            )?;
        }
        let owner: Owner = directory.json(".owner.json")?;
        ensure!(
            owner.schema == SCHEMA && owner.vm == vm,
            "snapshot VM directory ownership mismatch"
        );
        lock.verify(&directory)?;
        Ok(VmSnapshots {
            directory,
            vm: vm.into(),
            lock,
        })
    }
}
impl VmSnapshots {
    fn verify_owner(&self) -> Result<()> {
        self.directory.verify()?;
        self.lock.verify(&self.directory)?;
        let owner: Owner = self.directory.json(".owner.json")?;
        ensure!(
            owner.schema == SCHEMA && owner.vm == self.vm,
            "snapshot VM ownership changed"
        );
        Ok(())
    }
    pub fn begin(
        &mut self,
        wanted: &str,
        kind: Kind,
        compatibility: Compatibility,
        reserve_bytes: u64,
    ) -> Result<Capture<'_>> {
        self.verify_owner()?;
        name(wanted)?;
        compatibility.validate()?;
        ensure!(
            kind != Kind::DiskRollback || compatibility.disk_bytes.is_some(),
            "disk rollback requires a disk"
        );
        ensure!(
            !self
                .directory
                .names()?
                .iter()
                .any(|n| n == wanted || n == &format!(".deleting-{wanted}")),
            "snapshot name already exists or has unfinished deletion"
        );
        ensure!(
            reserve_bytes >= compatibility.required_reservation(kind)?,
            "snapshot reservation too small"
        );
        // Free-space admission, not an OS guarantee against unrelated writers.
        // A backend/write failure retains staging and cannot publish a snapshot.
        self.directory.reserve(reserve_bytes)?;
        let capture_id = storage::nonce()?;
        let staging = format!(".pending-{capture_id}");
        let (directory, created) = self.directory.child(&staging, true)?;
        ensure!(created, "snapshot staging nonce collision");
        Ok(Capture {
            owner: self,
            directory,
            staging,
            name: wanted.into(),
            kind,
            compatibility,
            capture_id,
        })
    }
    fn read(&self, wanted: &str, integrity: bool) -> Result<SnapshotRead> {
        self.verify_owner()?;
        name(wanted)?;
        let (directory, _) = self.directory.child(wanted, false)?;
        let metadata: Metadata = directory.json("manifest.json")?;
        metadata.validate(&self.vm, wanted)?;
        let expected: BTreeSet<String> = metadata
            .artifacts
            .iter()
            .map(|a| a.role.file().into())
            .chain(std::iter::once("manifest.json".into()))
            .collect();
        ensure!(
            directory.names()?.into_iter().collect::<BTreeSet<_>>() == expected,
            "unexpected files in snapshot; refuse traversal/removal"
        );
        let mut files = Vec::new();
        for artifact in &metadata.artifacts {
            let file = directory.file(artifact.role.file(), false)?;
            ensure!(
                file.metadata()?.len() == artifact.bytes,
                "snapshot artifact size mismatch"
            );
            if integrity {
                ensure!(
                    storage::hash(&file)? == (artifact.bytes, artifact.sha256.clone()),
                    "snapshot artifact SHA mismatch"
                );
            }
            files.push((artifact.role, file));
        }
        self.verify_owner()?;
        directory.verify()?;
        Ok((metadata, directory, files))
    }
    pub fn list(&self) -> Result<Vec<Metadata>> {
        self.verify_owner()?;
        let mut result = Vec::new();
        for entry in self.directory.names()? {
            if entry.starts_with('.') {
                continue;
            } // partial/failed captures are never listed as complete
            result.push(self.read(&entry, false)?.0);
        }
        Ok(result)
    }
    pub fn verify(&self, wanted: &str, compatible: &Compatibility) -> Result<Metadata> {
        compatible.validate()?;
        let metadata = self.read(wanted, true)?.0;
        ensure!(
            metadata.compatibility == *compatible,
            "snapshot backend/config/CPU/device compatibility mismatch"
        );
        Ok(metadata)
    }
    pub fn delete(&mut self, wanted: &str) -> Result<()> {
        self.verify_owner()?;
        name(wanted)?;
        // A deterministic private tomb reserves this exact name across restart.
        // A retry must never search arbitrary tombs or select another snapshot.
        let tomb = format!(".deleting-{wanted}");
        let names = self.directory.names()?;
        let directory = if names.contains(&tomb) {
            ensure!(
                !names.iter().any(|n| n == wanted),
                "ambiguous snapshot deletion; preserve both generations"
            );
            self.directory.child(&tomb, false)?.0
        } else {
            let (_, directory, _) = self.read(wanted, false)?;
            self.directory.rename_child_new(wanted, &tomb, &directory)?
        };
        let entries = directory.names()?;
        if entries.is_empty() {
            // Crash after the last file unlink: only remove this checked empty
            // private directory; no artifact/path is inferred without metadata.
            self.directory.unlink_child(&tomb, &directory)?;
            return Ok(());
        }
        let metadata: Metadata = directory.json("manifest.json")?;
        metadata.validate(&self.vm, wanted)?;
        let allowed: BTreeSet<String> = metadata
            .artifacts
            .iter()
            .map(|a| a.role.file().into())
            .chain(std::iter::once("manifest.json".into()))
            .collect();
        ensure!(
            entries.iter().all(|n| allowed.contains(n)),
            "unexpected files in deleting snapshot; preserve contents"
        );
        // Validate every remaining descriptor before deleting any. Missing
        // artifacts are permitted only inside this already hidden tomb.
        let mut files = Vec::new();
        for artifact in &metadata.artifacts {
            if entries.iter().any(|n| n == artifact.role.file()) {
                let file = directory.file(artifact.role.file(), false)?;
                ensure!(
                    file.metadata()?.len() == artifact.bytes,
                    "deleting snapshot artifact size mismatch"
                );
                files.push((artifact.role, file));
            }
        }
        let manifest = directory.file("manifest.json", false)?;
        self.verify_owner()?;
        directory.verify()?;
        // Open directory descriptor continues to identify exactly this owned
        // snapshot. No remove_dir_all, arbitrary metadata path, or link follow.
        for (role, file) in files {
            directory.unlink_file(role.file(), &file)?;
        }
        // Keep the manifest until all artifacts are durably gone, so interrupted
        // partial deletion can validate the same VM/name and explicit file set.
        directory.unlink_file("manifest.json", &manifest)?;
        self.directory.unlink_child(&tomb, &directory)?;
        Ok(())
    }
    pub fn prepare_restore<'a>(
        &'a mut self,
        wanted: &str,
        compatible: &Compatibility,
        protective_name: Option<&str>,
        target: RestoreTarget<'a>,
    ) -> Result<RestorePlan<'a>> {
        ensure!(
            target.worker_retired,
            "restore requires confirmed worker/backend retirement, not pause"
        );
        compatible.validate()?;
        let (metadata, _, files) = self.read(wanted, true)?;
        ensure!(
            metadata.compatibility == *compatible,
            "snapshot backend/config/CPU/device compatibility mismatch"
        );
        let protection = if let Some((guard, file)) = target.disk {
            guard.verify_paths()?;
            let identity = guard.identity();
            let actual = file.metadata()?;
            ensure!(
                actual.dev() == identity.device && actual.ino() == identity.inode,
                "restore target is not the locked disk inode"
            );
            ensure!(
                Some(actual.len()) == compatible.disk_bytes,
                "restore target disk extent mismatch"
            );
            let protective_name = protective_name
                .context("create a protective disk_rollback snapshot before restore")?;
            ensure!(
                protective_name != wanted,
                "restore source cannot be its own protection"
            );
            let protection = self.read(protective_name, true)?.0;
            check_protection(&metadata, &protection, compatible, storage::hash(file)?)?;
            guard.verify_paths()?;
            Some(protection)
        } else {
            ensure!(
                compatible.disk_bytes.is_none() && protective_name.is_none(),
                "restore disk/lock/protection missing"
            );
            None
        };
        let mut disk = None;
        let mut machine_state = None;
        for (role, file) in files {
            match role {
                Role::Disk => disk = Some(file),
                Role::MachineState => machine_state = Some(file),
            }
        }
        Ok(RestorePlan {
            metadata,
            disk,
            machine_state,
            protection,
            _owner: self,
            _target: target,
        })
    }
}
impl Capture<'_> {
    pub fn capture_id(&self) -> &str {
        &self.capture_id
    }
    pub fn artifact_path(&self, role: Role) -> Result<PathBuf> {
        ensure!(
            roles(self.kind, self.compatibility.disk_bytes.is_some()).contains(&role),
            "artifact role not allowed"
        );
        self.owner.verify_owner()?;
        self.directory.verify()?;
        Ok(self.directory.path.join(role.file()))
    }
    pub fn create_artifact(&self, role: Role) -> Result<File> {
        self.artifact_path(role)?;
        self.directory.file(role.file(), true)
    }
    /// Fresh write-only descriptor for QMP file:/dev/fdset/N migration output.
    /// QEMU file migration requires O_WRONLY, not an O_RDWR descriptor.
    pub fn create_artifact_writeonly(&self, role: Role) -> Result<File> {
        self.artifact_path(role)?;
        self.directory.create_writeonly(role.file())
    }
    /// Caller retains the existing backend DiskGuard and proves the source is
    /// quiesced. The clone is a disk artifact only; it supplies no RAM/device or
    /// full-state capture proof by itself. Errors retain hidden staging.
    pub fn clone_disk(&self, source: &File) -> Result<File> {
        self.artifact_path(Role::Disk)?;
        let bytes = self
            .compatibility
            .disk_bytes
            .context("snapshot has no disk artifact")?;
        self.directory.clone_disk(source, Role::Disk.file(), bytes)
    }
    pub fn commit(self, proof: CaptureProof) -> Result<Metadata> {
        self.commit_inner(proof, None)
    }
    pub fn commit_expected(self, proof: CaptureProof, expected: &[Artifact]) -> Result<Metadata> {
        self.commit_inner(proof, Some(expected))
    }
    fn commit_inner(
        self,
        proof: CaptureProof,
        expected_receipt: Option<&[Artifact]>,
    ) -> Result<Metadata> {
        self.owner.verify_owner()?;
        ensure!(
            proof.capture_id == self.capture_id,
            "backend receipt belongs to another capture"
        );
        let expected = roles(self.kind, self.compatibility.disk_bytes.is_some());
        let names: BTreeSet<String> = expected.iter().map(|r| r.file().into()).collect();
        ensure!(
            self.directory.names()?.into_iter().collect::<BTreeSet<_>>() == names,
            "capture incomplete or unexpected files"
        );
        let mut artifacts = Vec::new();
        for role in expected {
            let file = self.directory.file(role.file(), false)?;
            file.sync_all()?;
            let (bytes, sha256) = storage::hash(&file)?;
            artifacts.push(Artifact {
                role,
                bytes,
                sha256,
            });
        }
        if let Some(expected) = expected_receipt {
            ensure!(
                expected.len() == artifacts.len(),
                "backend receipt artifact count mismatch"
            );
            for artifact in &artifacts {
                let matches: Vec<_> = expected
                    .iter()
                    .filter(|a| a.role == artifact.role)
                    .collect();
                ensure!(
                    matches.len() == 1
                        && matches[0].bytes == artifact.bytes
                        && matches[0].sha256 == artifact.sha256,
                    "backend receipt artifact digest/extent mismatch"
                );
            }
        }
        let metadata = Metadata {
            schema: SCHEMA,
            vm: self.owner.vm.clone(),
            name: self.name,
            kind: self.kind,
            created_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)?
                .as_millis()
                .try_into()?,
            compatibility: self.compatibility,
            artifacts,
            proof,
            capture_verified: true,
            restore_verified: false,
        };
        metadata.validate(&self.owner.vm, &metadata.name)?;
        self.directory.write_json("manifest.json", &metadata)?;
        self.owner
            .directory
            .rename_child_new(&self.staging, &metadata.name, &self.directory)
            .context(
                "snapshot commit uncertain; inspect retained staging/final name before retry",
            )?;
        Ok(metadata)
    }
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;
