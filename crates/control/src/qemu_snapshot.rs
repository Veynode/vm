//! Paused external machine-state transfers for the QEMU/HVF Linux worker.
//!
//! This is not a disk snapshot or a claim of working GPU migration. The worker
//! must hold its lifecycle/input/Guest-job fence throughout capture, disk clone,
//! metadata commit and explicit source resume. Only non-GL virtio-2d is eligible;
//! runtime migration blockers are independently queried. Restore requires a new
//! identically configured QEMU with `-incoming defer -S`, an already protected
//! disk replacement, and a fresh Guest transport incarnation before control is
//! admitted. No helper here starts a process, copies a disk or auto-resumes.
//!
//! Selected exact QEMU 10.0.12 UTM source observations (not Guest acceptance):
//! * hw/display/virtio-gpu-base.c185: Virgl installs a migration blocker.
//! * accel/hvf/hvf-accel-ops.c270..300: register sync/pre-load callbacks.
//! * target/arm/hvf/hvf.c2278..2306: hvf-vtimer VMState and pause/resume offset.
//! * target/arm/machine.c853: CPU/cpreg serialization, including non-KVM/HVF.
//! * hw/display/virtio-gpu.c1714: non-GL GPU VMState including resources/scanouts.
//! * migration/migration.c: stop/flush, block inactivation, completed->postmigrate.
//! * migration/file.c96: file channel can use `/dev/fdset/N` via qemu_create.
//! * monitor/fds.c412: fdset access mode MUST match O_WRONLY/O_RDONLY exactly.
//!
//! Source plumbing makes the route testable; only an actual memory+disk Guest
//! round trip establishes that the chosen complete configuration works.
use crate::qemu_graphics::Profile;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs::File,
    os::fd::AsRawFd,
    os::unix::fs::{FileExt, MetadataExt},
    time::{Duration, Instant},
};

/// The existing worker QMP transport implements framing, SCM_RIGHTS and per-call
/// deadlines. `add_fd` must allocate a fresh, single-FD set >=16 (sets 1..15 are
/// reserved for disk/lock/witness), send this FD over its existing monitor and
/// return the acknowledged set ID. It must never accept a browser-supplied path.
pub trait Qmp {
    fn command(&mut self, method: &str, arguments: Value) -> Result<Value>;
    fn add_fd(&mut self, file: &File) -> Result<i64>;
}

const MAX_TRANSFER: Duration = Duration::from_secs(300);
const CANCEL_BUDGET: Duration = Duration::from_secs(10);
const MAX_POLLS: u32 = 12_000;

/// Run before pausing. Success is permission to attempt this narrowly supported
/// transfer, not a capability claim that a restore has been validated.
pub fn preflight(qmp: &mut impl Qmp, profile: Profile) -> Result<()> {
    ensure!(
        profile == Profile::Virtio2d,
        "full-state snapshots unsupported for Virgl/Venus: QEMU's accelerated GPU state is not migratable"
    );
    let commands = qmp.command("query-commands", json!({}))?;
    let commands = commands.as_array().context("invalid QMP command list")?;
    ensure!(commands.len() <= 1024, "QMP command list bound");
    let names: BTreeSet<_> = commands.iter().filter_map(|v| v["name"].as_str()).collect();
    for name in [
        "add-fd",
        "remove-fd",
        "query-fdsets",
        "query-status",
        "query-migrate",
        "query-migrate-capabilities",
        "query-migrate-parameters",
        "migrate",
        "migrate-incoming",
        "migrate_cancel",
        "cont",
        "stop",
    ] {
        ensure!(
            names.contains(name),
            "required QMP command unavailable: {name}"
        );
    }
    let capabilities = qmp.command("query-migrate-capabilities", json!({}))?;
    let capabilities = capabilities
        .as_array()
        .context("invalid migration capabilities")?;
    ensure!(capabilities.len() <= 64, "migration capability bound");
    let mut seen = BTreeSet::new();
    for cap in capabilities {
        let name = cap["capability"]
            .as_str()
            .context("missing migration capability")?;
        let enabled = cap["state"]
            .as_bool()
            .context("invalid migration capability state")?;
        ensure!(seen.insert(name), "duplicate migration capability");
        // No postcopy, RAM release, multifd, compression, background state,
        // shared-memory omission, switchover handshake or experimental mode.
        ensure!(
            !enabled || name == "events",
            "unsupported enabled migration capability: {name}"
        );
    }
    let parameters = qmp.command("query-migrate-parameters", json!({}))?;
    ensure!(
        parameters["mode"] == "normal",
        "snapshot requires normal QEMU migration mode"
    );
    ensure!(
        parameters
            .get("tls-creds")
            .is_none_or(|v| v.is_null() || v == ""),
        "snapshot file migration cannot use TLS credentials"
    );
    let info = qmp.command("query-migrate", json!({}))?;
    reject_blockers(&info)?;
    ensure!(
        matches!(
            migration_status(&info)?,
            None | Some("none" | "failed" | "cancelled" | "completed")
        ),
        "another migration is active"
    );
    Ok(())
}

fn reject_blockers(info: &Value) -> Result<()> {
    ensure!(info.is_object(), "invalid migration query");
    if let Some(reasons) = info.get("blocked-reasons") {
        let reasons = reasons.as_array().context("invalid migration blockers")?;
        ensure!(
            reasons.is_empty(),
            "backend migration blocked: {}",
            Value::Array(reasons.clone())
        );
    }
    Ok(())
}
fn migration_status(info: &Value) -> Result<Option<&str>> {
    ensure!(info.is_object(), "invalid migration query");
    info.get("status")
        .map(|v| v.as_str().context("invalid migration status"))
        .transpose()
}
fn runstate(qmp: &mut impl Qmp) -> Result<(bool, String)> {
    let value = qmp.command("query-status", json!({}))?;
    Ok((
        value["running"]
            .as_bool()
            .context("QMP running flag absent")?,
        value["status"]
            .as_str()
            .context("QMP runstate absent")?
            .to_owned(),
    ))
}
fn require_paused(qmp: &mut impl Qmp, expected: &str) -> Result<()> {
    let (running, state) = runstate(qmp)?;
    ensure!(
        !running && state == expected,
        "expected stopped QEMU {expected}, observed {state}/running={running}"
    );
    Ok(())
}
fn check_file(file: &File, incoming: bool) -> Result<()> {
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.nlink() == 1
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0,
        "unsafe machine-state file"
    );
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    ensure!(
        flags >= 0
            && flags & libc::O_ACCMODE
                == if incoming {
                    libc::O_RDONLY
                } else {
                    libc::O_WRONLY
                },
        "machine-state FD must have exact read-only/write-only access mode"
    );
    ensure!(
        flags & libc::O_APPEND == 0,
        "machine-state FD cannot append"
    );
    ensure!(
        unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_CUR) } == 0,
        "machine-state FD offset must be zero"
    );
    if incoming {
        let mut header = [0; 8];
        ensure!(
            meta.len() >= 8
                && file.read_at(&mut header, 0)? == 8
                && header == [b'Q', b'E', b'V', b'M', 0, 0, 0, 3],
            "unsupported or corrupt QEMU machine-state header"
        );
    } else {
        ensure!(
            meta.len() == 0,
            "capture target must be a new empty machine-state file"
        );
    }
    Ok(())
}
fn verify_fdset(qmp: &mut impl Qmp, id: i64) -> Result<()> {
    ensure!(
        id >= 16,
        "machine-state FD set collides with reserved descriptors"
    );
    let value = qmp.command("query-fdsets", json!({}))?;
    let sets = value.as_array().context("invalid QMP FD sets")?;
    ensure!(sets.len() <= 128, "QMP FD set bound");
    let found: Vec<_> = sets
        .iter()
        .filter(|v| v["fdset-id"].as_i64() == Some(id))
        .collect();
    ensure!(
        found.len() == 1,
        "machine-state FD set missing or duplicated"
    );
    let fds = found[0]["fds"]
        .as_array()
        .context("machine-state descriptors absent")?;
    ensure!(
        fds.len() == 1 && fds[0]["fd"].as_i64().is_some_and(|fd| fd > 2),
        "machine-state FD set must contain exactly one valid descriptor"
    );
    Ok(())
}
fn file_channel(fdset: i64) -> Value {
    json!({"channels":[{"channel-type":"main","addr":{"transport":"file","filename":format!("/dev/fdset/{fdset}"),"offset":0}}]})
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Progress {
    Pending,
    /// Machine state complete, source postmigrate; disk must be cloned now.
    CapturedPaused,
    /// Incoming completed, destination still paused; Guest/control not admitted.
    RestoredPaused,
    Cancelled,
    Failed {
        reason: String,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Prepared,
    Started,
    Cancelling,
    Complete,
    Cancelled,
    Failed,
    Resumed,
}

struct Transfer {
    fdset: i64,
    phase: Phase,
    deadline: Instant,
    polls: u32,
    incoming: bool,
    released: bool,
    file: File,
    identity: (u64, u64),
}
impl Transfer {
    fn prepare(
        qmp: &mut impl Qmp,
        profile: Profile,
        file: &File,
        timeout: Duration,
        now: Instant,
        incoming: bool,
    ) -> Result<Self> {
        ensure!(
            !timeout.is_zero() && timeout <= MAX_TRANSFER,
            "machine-state transfer timeout must be 1ns..300s"
        );
        preflight(qmp, profile)?;
        require_paused(qmp, if incoming { "inmigrate" } else { "paused" })?;
        check_file(file, incoming)?;
        let owned = file.try_clone()?;
        let meta = owned.metadata()?;
        let fdset = qmp.add_fd(file)?;
        // If registration validation fails, never remove a reserved set or an
        // unproved/duplicated set. The worker retains ownership for diagnosis.
        verify_fdset(qmp, fdset)?;
        Ok(Self {
            fdset,
            phase: Phase::Prepared,
            deadline: now + timeout,
            polls: 0,
            incoming,
            released: false,
            file: owned,
            identity: (meta.dev(), meta.ino()),
        })
    }
    fn start(&mut self, qmp: &mut impl Qmp) -> Result<()> {
        ensure!(
            self.phase == Phase::Prepared && !self.released,
            "machine-state transfer already started or released"
        );
        ensure!(
            Instant::now() < self.deadline,
            "machine-state start deadline expired"
        );
        // Record uncertainty BEFORE sending. An ACK error cannot prove that
        // the backend did not start; caller must retain this object and poll.
        self.phase = Phase::Started;
        let mut args = file_channel(self.fdset);
        if self.incoming {
            args["exit-on-error"] = json!(false);
        }
        qmp.command(
            if self.incoming {
                "migrate-incoming"
            } else {
                "migrate"
            },
            args,
        )?;
        Ok(())
    }
    fn poll(&mut self, qmp: &mut impl Qmp, now: Instant) -> Result<Progress> {
        ensure!(
            matches!(self.phase, Phase::Started | Phase::Cancelling),
            "machine-state transfer is not pending"
        );
        ensure!(now < self.deadline && self.polls < MAX_POLLS, "machine-state transfer deadline/poll bound; retain fence and request cancellation or retire the unadmitted destination normally");
        self.polls += 1;
        let info = qmp.command("query-migrate", json!({}))?;
        reject_blockers(&info)?;
        match migration_status(&info)? {
            Some("completed") => {
                let (running, state) = runstate(qmp)?;
                ensure!(
                    !running,
                    "machine-state completion unexpectedly resumed QEMU; disk capture forbidden"
                );
                let expected = if self.incoming {
                    "paused"
                } else {
                    "postmigrate"
                };
                if state != expected {
                    ensure!(
                        state == "finish-migrate" || state == "inmigrate",
                        "unexpected completed transfer runstate: {state}"
                    );
                    return Ok(Progress::Pending);
                }
                let meta = self.file.metadata()?;
                ensure!(
                    (meta.dev(), meta.ino()) == self.identity && meta.len() >= 8,
                    "machine-state artifact missing or changed"
                );
                self.phase = Phase::Complete;
                Ok(if self.incoming {
                    Progress::RestoredPaused
                } else {
                    Progress::CapturedPaused
                })
            }
            Some("failed" | "cancelled") => {
                let (running, state) = runstate(qmp)?;
                ensure!(!running, "failed transfer unexpectedly resumed QEMU");
                ensure!(
                    if self.incoming {
                        state == "inmigrate" || state == "paused"
                    } else {
                        state == "paused"
                    },
                    "failed transfer has unsafe QEMU runstate: {state}"
                );
                if info["status"] == "cancelled" {
                    self.phase = Phase::Cancelled;
                    Ok(Progress::Cancelled)
                } else {
                    self.phase = Phase::Failed;
                    let reason = info["error-desc"]
                        .as_str()
                        .unwrap_or("QEMU machine-state transfer failed")
                        .chars()
                        .take(1024)
                        .collect();
                    Ok(Progress::Failed { reason })
                }
            }
            Some("setup" | "active" | "device" | "cancelling") => Ok(Progress::Pending),
            None | Some("none") => {
                bail!("machine-state start unconfirmed; retain source fence and artifact")
            }
            Some(other) => bail!("unsupported migration phase {other}; retain source fence"),
        }
    }
    fn release(&mut self, qmp: &mut impl Qmp) -> Result<()> {
        ensure!(
            !self.released
                && matches!(
                    self.phase,
                    Phase::Prepared
                        | Phase::Complete
                        | Phase::Cancelled
                        | Phase::Failed
                        | Phase::Resumed
                ),
            "cannot release a live/uncertain machine-state channel"
        );
        verify_fdset(qmp, self.fdset)?;
        qmp.command("remove-fd", json!({"fdset-id":self.fdset}))?;
        self.released = true;
        Ok(())
    }
}

pub struct Capture(Transfer);
impl Capture {
    pub fn prepare(
        qmp: &mut impl Qmp,
        profile: Profile,
        state: &File,
        timeout: Duration,
        now: Instant,
    ) -> Result<Self> {
        Ok(Self(Transfer::prepare(
            qmp, profile, state, timeout, now, false,
        )?))
    }
    pub fn start(&mut self, qmp: &mut impl Qmp) -> Result<()> {
        self.0.start(qmp)
    }
    /// One bounded observation; worker chooses scheduling and keeps processing
    /// cancellation/safety events. No busy loop or blocking sleep here.
    pub fn poll(&mut self, qmp: &mut impl Qmp, now: Instant) -> Result<Progress> {
        self.0.poll(qmp, now)
    }
    pub fn request_cancel(&mut self, qmp: &mut impl Qmp, now: Instant) -> Result<()> {
        ensure!(
            matches!(self.0.phase, Phase::Started | Phase::Cancelling),
            "capture is not cancellable"
        );
        // Retrying cancellation must not extend its finite observation budget.
        if self.0.phase != Phase::Cancelling {
            self.0.phase = Phase::Cancelling;
            self.0.deadline = now + CANCEL_BUDGET;
            self.0.polls = 0;
        }
        qmp.command("migrate_cancel", json!({}))?;
        Ok(())
    }
    /// Call only AFTER coherent disk clone and durable metadata commit, or after
    /// confirmed cancellation/failure when no snapshot is being published. QEMU
    /// `cont` reactivates its disks; read back running state before admitting jobs.
    pub fn resume_source(&mut self, qmp: &mut impl Qmp) -> Result<()> {
        ensure!(
            matches!(
                self.0.phase,
                Phase::Complete | Phase::Cancelled | Phase::Failed
            ),
            "cannot resume a live or uncertain capture"
        );
        let (running, state) = runstate(qmp)?;
        if !(running && state == "running") {
            ensure!(
                !running && (state == "postmigrate" || state == "paused"),
                "unsafe source resume state: {state}"
            );
            qmp.command("cont", json!({}))?;
            let (running, state) = runstate(qmp)?;
            ensure!(
                running && state == "running",
                "source resume is unconfirmed"
            );
        }
        self.0.phase = Phase::Resumed;
        Ok(())
    }
    pub fn release_channel(&mut self, qmp: &mut impl Qmp) -> Result<()> {
        self.0.release(qmp)
    }
}

pub struct Incoming(Transfer);
impl Incoming {
    pub fn prepare(
        qmp: &mut impl Qmp,
        profile: Profile,
        state: &File,
        timeout: Duration,
        now: Instant,
    ) -> Result<Self> {
        Ok(Self(Transfer::prepare(
            qmp, profile, state, timeout, now, true,
        )?))
    }
    pub fn start(&mut self, qmp: &mut impl Qmp) -> Result<()> {
        self.0.start(qmp)
    }
    pub fn poll(&mut self, qmp: &mut impl Qmp, now: Instant) -> Result<Progress> {
        self.0.poll(qmp, now)
    }
    pub fn release_channel(&mut self, qmp: &mut impl Qmp) -> Result<()> {
        self.0.release(qmp)
    }
    /// Only after caller's disk/config integrity and new transport/control epoch
    /// checks. A restored process is not automatically trusted as a new client.
    pub fn resume_destination(&mut self, qmp: &mut impl Qmp) -> Result<()> {
        ensure!(
            self.0.phase == Phase::Complete,
            "incoming state not complete"
        );
        require_paused(qmp, "paused")?;
        qmp.command("cont", json!({}))?;
        let (running, state) = runstate(qmp)?;
        ensure!(
            running && state == "running",
            "destination resume is unconfirmed"
        );
        self.0.phase = Phase::Resumed;
        Ok(())
    }
}

#[cfg(test)]
#[path = "qemu_snapshot_tests.rs"]
mod tests;
