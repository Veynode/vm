pub mod backend;
pub mod bridge;
mod exec_journal;
pub mod exec_scope;
pub mod exec_witness;
mod input_retirement;
use input_retirement::InputRetirement;
pub mod input;
mod jobs;
mod lease;
mod native_control;
pub mod native_input;
pub mod qemu_disk;
pub mod qemu_graphics;
mod resources;
mod windows;
use anyhow::{bail, Context, Result};
use backend::{Backend, QemuRegistration};
use resources::{DiskIdentity, MemoryBudget};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::Duration,
};

pub const MAX_MESSAGE: usize = 1024 * 1024;
pub const MAX_SCREENSHOT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_SCREENSHOT_RESPONSE: usize = 3 * 1024 * 1024;
pub fn response_limit(method: &str) -> usize {
    if method == "screenshot" {
        MAX_SCREENSHOT_RESPONSE
    } else {
        MAX_MESSAGE
    }
}
pub fn mcp_response_limit(request: &Value) -> usize {
    if request["method"] == "tools/call" && request["params"]["name"] == "vm_screenshot" {
        MAX_SCREENSHOT_RESPONSE
    } else {
        MAX_MESSAGE
    }
}
pub const VERSION: &str = "2025-06-18";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub backend: Backend,
    pub id: String,
    pub guest: String,
    pub cpus: u32,
    pub memory_mib: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initrd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore_image: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_identity_dir: Option<PathBuf>,
    #[serde(default)]
    pub window: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub qemu_guest_exec: bool,
    #[serde(default, skip_serializing_if = "qemu_graphics::Profile::is_default")]
    pub qemu_graphics_profile: qemu_graphics::Profile,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_log: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_dir: Option<PathBuf>,
}
fn is_false(value: &bool) -> bool {
    !*value
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.id)?;
        self.qemu_graphics_profile
            .validate(self.backend, &self.guest)?;
        if self.qemu_guest_exec && self.backend != Backend::QemuHvf {
            bail!("qemu_guest_exec requires QEMU/HVF")
        }
        if self.backend == Backend::QemuHvf
            && (self.guest != "linux"
                || self.shared_dir.is_some()
                || self.restore_image.is_some()
                || self.machine_identity_dir.is_some())
        {
            bail!("unsupported QEMU configuration: this revision supports Linux ARM direct boot, optional explicitly raw disk, and SPICE; host sharing and other Guest backends are not integrated")
        }
        if self.backend == Backend::QemuHvf {
            backend::qemu_display_size(self.width, self.height)?;
            if let Some(disk) = &self.disk {
                qemu_disk::validate_path(disk)?;
            }
        }
        if !["linux", "macos"].contains(&self.guest.as_str()) {
            bail!("unsupported guest: Windows ARM requires a separately validated backend and GPU path")
        }
        if !(1..=128).contains(&self.cpus) || !(256..=1048576).contains(&self.memory_mib) {
            bail!("invalid CPU or memory allocation")
        }
        if self.guest == "linux" && self.kernel.is_none() {
            bail!("Linux direct boot requires kernel")
        }
        if self.guest == "macos" && (self.disk.is_none() || self.machine_identity_dir.is_none()) {
            bail!("macOS requires installed disk and machine_identity_dir")
        }
        for p in [&self.kernel, &self.initrd, &self.disk, &self.restore_image]
            .into_iter()
            .flatten()
        {
            if !p.is_absolute() || !p.is_file() {
                bail!("input files must exist at absolute paths: {}", p.display())
            }
        }
        for p in [&self.machine_identity_dir, &self.serial_log]
            .into_iter()
            .flatten()
        {
            if !p.is_absolute() {
                bail!("output paths must be absolute")
            }
        }
        if let Some(shared) = &self.shared_dir {
            if !shared.is_absolute() || !shared.is_dir() {
                bail!("shared_dir must exist at an absolute path")
            }
        }
        for dimension in [self.width, self.height].into_iter().flatten() {
            if !(320..=8192).contains(&dimension) {
                bail!("display dimension must be 320..8192")
            }
        }
        if self
            .command_line
            .as_ref()
            .is_some_and(|s| s.len() > 16384 || s.contains('\0'))
        {
            bail!("kernel command line too large or contains NUL")
        }
        Ok(())
    }
}
pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        bail!("id must be 1–64 ASCII letters, digits, '-' or '_'")
    }
    Ok(())
}
pub fn read_frame(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    read_frame_bounded(reader, MAX_MESSAGE)
}
pub fn read_frame_bounded(reader: &mut impl BufRead, limit: usize) -> Result<Option<Vec<u8>>> {
    if limit != MAX_MESSAGE && limit != MAX_SCREENSHOT_RESPONSE {
        bail!("invalid response bound")
    }
    let mut out = Vec::new();
    let n = reader
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut out)?;
    if n == 0 {
        return Ok(None);
    }
    if n > limit || out.last() != Some(&b'\n') {
        bail!("oversized or unterminated JSON frame")
    }
    out.pop();
    Ok(Some(out))
}
fn validate_local_stream_bridge(
    method: &str,
    args: &Value,
    backend: Backend,
    guest: &str,
) -> Result<()> {
    if backend != Backend::AppleVirtualization || guest != "macos" {
        bail!("unsupported: Guest stream bridge requires Apple macOS")
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
fn shutdown_mode(args: &Value) -> Result<&str> {
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
fn write_json(writer: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}
fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        bail!("state directory must be a real directory")
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let tmp = path.with_extension(format!("{nonce}.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let result = (|| {
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(path.parent().unwrap())?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
struct Running {
    bridge: Arc<Mutex<bridge::State>>,
    child: Child,
    assigned_memory_mib: u64,
    expected_exit: bool,
    execution_quiescing: bool,
    exit_status: Option<std::process::ExitStatus>,
    disk_identity: Option<DiskIdentity>,
    stdin: ChildStdin,
    state: Arc<Mutex<Value>>,
    pending: Arc<Mutex<BTreeMap<u64, PendingReply>>>,
    next: u64,
}
// These values describe the original launch and remain valid historical facts
// after terminal/error events. This function only receives one Running's state;
// a replacement worker starts with a fresh state object and inherits nothing.
fn replace_runtime_state(current: &mut Value, mut next: Value) {
    for field in [
        "disk_provenance",
        "graphics_profile",
        "execution_witness",
        "execution_guardian",
        "backend_pid",
        "vm_id",
    ] {
        if let Some(value) = current.get(field) {
            next[field] = value.clone();
        }
    }
    *current = next;
}
impl Running {
    // Keep the worker PID reserved until its private process group is killed.
    // try_wait would reap first and permit PID/PGID reuse before cleanup.
    fn poll_exit(&mut self) -> Result<Option<std::process::ExitStatus>> {
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
    fn force_stop(&mut self) -> Result<()> {
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
fn only_waitable_leader(error: Option<i32>, pid: u32) -> bool {
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
fn only_waitable_leader(_error: Option<i32>, _pid: u32) -> bool {
    false
}
struct PendingReply {
    sender: mpsc::Sender<Value>,
    limit: usize,
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
    fn send(self, value: Value) -> std::result::Result<(), mpsc::SendError<Value>> {
        self.sender.send(value)
    }
}
fn validate_backend_response_bound(
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
struct Pending {
    receiver: mpsc::Receiver<Value>,
    number: u64,
    pending: Arc<Mutex<BTreeMap<u64, PendingReply>>>,
    timeout: Duration,
    post: Option<JobPost>,
}
enum JobPost {
    Windows,
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
        through_epoch: Option<u64>,
    },
    Screenshot {
        guest_wrapped: bool,
    },
    Start {
        reservation: jobs::Reservation,
        vm: String,
        owner: String,
        epoch: u64,
        dir: PathBuf,
        leases: Arc<Mutex<lease::Leases>>,
    },
    Poll {
        jobs: Arc<Mutex<jobs::Jobs>>,
        vm: String,
        id: String,
    },
}
enum Completion {
    RecoveryFence(Box<Completion>),
    Backend(Pending),
    HumanRelease {
        cleanup: Box<Completion>,
        leases: Arc<Mutex<lease::Leases>>,
        vm: String,
        epoch: u64,
    },
    Input {
        dir: PathBuf,
        arguments: Value,
        method: String,
    },
    Synchronous {
        dir: PathBuf,
        arguments: Value,
    },
    CancelJobs {
        dir: PathBuf,
        response: Value,
        jobs: Vec<jobs::Job>,
        file_sessions: Vec<Value>,
        input_revocations: Vec<InputRetirement>,
        input_retirements: Arc<Mutex<input_retirement::Retirements>>,
        exec_retirements: Arc<Mutex<exec_scope::Ledger>>,
        exec_revocations: Vec<exec_scope::Scope>,
    },
}
impl Completion {
    fn wait(self, peer: &UnixStream) -> Result<Value> {
        match self {
            Self::RecoveryFence(cleanup) => {
                cleanup.wait(peer)?;
                bail!("execution cleanup unconfirmed: durable prior-daemon scope requires reconciliation")
            }
            Self::Backend(p) => p.wait(),
            Self::HumanRelease {
                cleanup,
                leases,
                vm,
                epoch,
            } => {
                let mut response = cleanup.wait(peer)?;
                let next = leases.lock().unwrap().human_release_after_cleanup(
                    &vm,
                    epoch,
                    std::time::Instant::now(),
                )?;
                response["epoch"] = json!(next);
                response["human_control"] = json!(false);
                Ok(response)
            }
            Self::Input {
                dir,
                arguments,
                method,
            } => desktop_input(&dir, &method, arguments, peer),
            Self::Synchronous { dir, arguments } => synchronous_exec(&dir, arguments, peer),
            Self::CancelJobs {
                dir,
                mut response,
                jobs,
                file_sessions,
                input_revocations,
                input_retirements,
                exec_retirements,
                exec_revocations,
            } => {
                let jobs = jobs
                    .into_iter()
                    .filter(|job| {
                        !exec_revocations.iter().any(|scope| {
                            scope.vm == job.vm
                                && scope.owner == job.owner
                                && scope.epoch == job.epoch
                        })
                    })
                    .collect::<Vec<_>>();
                let mut cleanup_error =
                    retire_exec_scopes(&dir, exec_revocations, &exec_retirements).err();
                // Leases are already fenced while holding Manager's lock. Guest ACK is
                // separate evidence that queued old epochs and synthetic downs are retired.
                let input_count = input_revocations.len();
                let input_vm_count = input_revocations
                    .iter()
                    .map(|record| &record.vm)
                    .collect::<BTreeSet<_>>()
                    .len();
                let (input_tx, input_rx) = mpsc::channel();
                let input_deadline = std::time::Instant::now() + Duration::from_secs(5);
                let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(
                    input_revocations,
                )));
                for _ in 0..input_count.min(8) {
                    let tx = input_tx.clone();
                    let dir = dir.clone();
                    let ledger = input_retirements.clone();
                    let queue = queue.clone();
                    thread::spawn(move || {
                        loop {
                            let record = {
                                let mut queue = queue.lock().unwrap();
                                if std::time::Instant::now() >= input_deadline {
                                    break;
                                }
                                queue.pop_front()
                            };
                            let Some(record) = record else {
                                break;
                            };
                            if std::time::Instant::now() >= input_deadline {
                                break;
                            }
                            let result = retire_input(&dir, &record, &ledger);
                            if tx.send(result).is_err() {
                                break;
                            }
                        }
                        // Undispatched obligations remain in the independent ledger.
                    });
                }
                drop(input_tx);
                let count = jobs.len();
                let (tx, rx) = mpsc::channel();
                for job in jobs {
                    let tx = tx.clone();
                    let dir = dir.clone();
                    thread::spawn(move || {
                        let result = call(
                            &dir,
                            "exec_cancel",
                            json!({"id":job.vm,"job_id":job.id,"owner":job.owner,"epoch":job.epoch}),
                        );
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
                if input_count > 0 {
                    let mut input_confirmed = true;
                    for _ in 0..input_count {
                        if let Err(error) = input_rx
                            .recv_timeout(
                                input_deadline.saturating_duration_since(std::time::Instant::now()),
                            )
                            .map_err(anyhow::Error::from)
                            .and_then(|result| result)
                            .context(
                                "control is fenced, but synthetic input release is unconfirmed",
                            )
                        {
                            input_confirmed = false;
                            if cleanup_error.is_none() {
                                cleanup_error = Some(error)
                            }
                        }
                    }
                    if input_confirmed {
                        response["input_release"] = json!({"confirmed":true,"vm_count":input_vm_count,"guest_delivery_confirmed":false});
                    }
                }
                if let Some(error) = cleanup_error {
                    return Err(error.context("session cleanup unconfirmed"));
                }
                response["note"]=json!("Cancellation is best effort, not rollback. Poll asynchronous jobs or inspect the synchronous command's terminal result to confirm termination.");
                Ok(response)
            }
        }
    }
}
fn retire_input(
    dir: &Path,
    record: &InputRetirement,
    ledger: &Arc<Mutex<input_retirement::Retirements>>,
) -> Result<Value> {
    if !ledger.lock().unwrap().contains(record) {
        return Ok(json!({"confirmed":true,"already_confirmed":true}));
    }
    let result = call(
        dir,
        "input_revoke",
        json!({"id":record.vm,"through_epoch":record.epoch,"worker_instance":record.worker_instance}),
    ).and_then(|value| {
        if value["released"] != true || value["synthetic_release_dispatched"] != true || value["through_epoch"].as_u64() != Some(record.epoch) {
            bail!("input release acknowledgement is unconfirmed")
        }
        Ok(value)
    });
    if result.is_ok() {
        ledger.lock().unwrap().complete(record);
    } else if !ledger.lock().unwrap().contains(record) {
        // Another exact-tuple release ACK or verified process reap completed it.
        return Ok(json!({"confirmed":true,"concurrently_confirmed":true}));
    }
    result
}
fn peer_disconnected(fds: &[std::os::fd::RawFd]) -> bool {
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
fn retire_exec_scopes(
    dir: &Path,
    records: Vec<exec_scope::Scope>,
    ledger: &Arc<Mutex<exec_scope::Ledger>>,
) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(7);
    let count = records.len();
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(records)));
    let (tx, rx) = mpsc::channel();
    for _ in 0..count.min(8) {
        let tx = tx.clone();
        let queue = queue.clone();
        let dir = dir.to_owned();
        let ledger = ledger.clone();
        thread::spawn(move || loop {
            if std::time::Instant::now() >= deadline {
                break;
            }
            let Some(scope) = queue.lock().unwrap().pop_front() else {
                break;
            };
            if std::time::Instant::now() >= deadline {
                break;
            }
            let args = json!({"id":scope.vm,"worker_instance":scope.worker_instance,"owner":scope.owner,"epoch":scope.epoch});
            let result = call(&dir, "exec_scope_revoke", args.clone()).and_then(|r| {
                if r == json!({"incarnation_terminated":true})
                    || r["result"] == json!({"revoked":true,"pending_starts":0,"cgroup_empty":true})
                {
                    Ok(())
                } else {
                    bail!("execution scope cleanup unconfirmed")
                }
            });
            let result = match result {
                Ok(()) => ledger.lock().unwrap().complete(&scope),
                Err(error) => {
                    let _ = call(&dir, "exec_scope_abort", args);
                    Err(error)
                }
            };
            let _ = tx.send(result);
        });
    }
    drop(tx);
    for _ in 0..count {
        rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .context("execution scope cleanup unconfirmed")??;
    }
    Ok(())
}
fn completed_exec_result(reply: &Value) -> Result<Option<Value>> {
    match reply["result"]["state"].as_str() {
        Some("running") => Ok(None),
        Some("finished") => {
            if let Some(error) = reply["result"].get("error") {
                bail!("guest command failed: {error}")
            }
            let result = &reply["result"]["result"];
            if !result.is_object()
                || !result["exit_code"].is_i64()
                || !result["cancelled"].is_boolean()
            {
                bail!("guest returned invalid terminal command result")
            }
            Ok(Some(json!({"id":reply["id"],"result":result})))
        }
        _ => bail!("guest returned invalid job state"),
    }
}
fn desktop_input(
    dir: &Path,
    method: &str,
    mut arguments: Value,
    peer: &UnixStream,
) -> Result<Value> {
    let steps = input::plan(method, &arguments)?;
    let mut random = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    arguments["operation_id"] = json!(random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>());
    let mut sent = 0;
    let result = (|| {
        for (index, events) in steps.iter().enumerate() {
            if peer_disconnected(&[peer.as_raw_fd()]) {
                bail!("input client disconnected")
            }
            arguments["begin"] = json!(index == 0);
            arguments["events"] = json!(events);
            call(dir, "input_step", arguments.clone())?;
            sent += 1;
            if index + 1 < steps.len() {
                thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    })();
    // Releases are safety cleanup, not new user input. An opaque operation token prevents
    // delayed cleanup from releasing a later client's buttons or keys.
    let cleanup = call(dir, "input_cleanup", arguments);
    if let Err(error) = cleanup {
        bail!("input cleanup unconfirmed after {sent} steps: {error}; operation result: {result:?}")
    }
    result?;
    Ok(
        json!({"dispatched":true,"steps":sent,"guest_delivery_confirmed":false,"note":"Backend dispatch acknowledgement is not application acceptance; physical keys use Guest layout."}),
    )
}
fn synchronous_exec(dir: &Path, arguments: Value, peer: &UnixStream) -> Result<Value> {
    // Every command uses the same registered, epoch-bound Guest job path. Never fall back
    // to legacy uncancellable exec when an older Guest lacks asynchronous integration.
    let started = call(dir, "exec_start", arguments.clone()).map_err(|error| anyhow::anyhow!(
        "synchronous command start failed (Guest exec_start/exec_poll/exec_cancel support required): {error}"
    ))?;
    let job_id = started["result"]["job_id"]
        .as_str()
        .context("guest returned no job identity")?;
    let job = json!({"id":arguments["id"],"owner":arguments["owner"],"epoch":arguments["epoch"],"job_id":job_id});
    let deadline = std::time::Instant::now() + Duration::from_secs(35);
    let result = (|| loop {
        if peer_disconnected(&[peer.as_raw_fd()]) {
            bail!("command client disconnected")
        }
        if std::time::Instant::now() >= deadline {
            bail!("synchronous command result deadline exceeded")
        }
        let reply = call(dir, "exec_poll", job.clone())?;
        if let Some(result) = completed_exec_result(&reply)? {
            return Ok(result);
        }
        thread::sleep(Duration::from_millis(50));
    })();
    if let Err(error) = result {
        return match call(dir, "exec_cancel", job) {
            Ok(_) => Err(anyhow::anyhow!(
                "command wait failed; cancellation requested, not confirmed: {error}"
            )),
            Err(cancel) => Err(anyhow::anyhow!(
                "command wait failed: {error}; cancellation also failed: {cancel}"
            )),
        };
    }
    result
}
impl Pending {
    fn wait(self) -> Result<Value> {
        match self.receiver.recv_timeout(self.timeout) {
            Ok(reply) => {
                if let Some(error) = reply.get("error") {
                    bail!("backend: {error}")
                }
                if let Some(error) = reply["result"].get("error") {
                    bail!("guest: {error}")
                }
                let mut result = reply["result"].clone();
                match self.post {
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
                        through_epoch,
                    }) => {
                        result =
                            normalize_backend_input(result, &method, through_epoch, guest_wrapped)?;
                    }
                    Some(JobPost::Start {
                        reservation,
                        vm,
                        owner,
                        epoch,
                        dir,
                        leases,
                    }) => {
                        if result["result"]["state"] != "running" {
                            bail!("guest returned invalid asynchronous start state")
                        }
                        let job_id = result["result"]["job_id"]
                            .as_str()
                            .context("guest returned no asynchronous job id")?
                            .to_owned();
                        reservation.register(
                            &vm,
                            &job_id,
                            &owner,
                            epoch,
                            std::time::Instant::now(),
                        )?;
                        let revoked = leases
                            .lock()
                            .unwrap()
                            .check(&vm, &owner, epoch, std::time::Instant::now())
                            .is_err();
                        if revoked {
                            result["result"]["control_revoked"] = json!(true);
                            result["result"]["cancellation"] = match call(
                                &dir,
                                "exec_cancel",
                                json!({"id":vm,"job_id":job_id,"owner":owner,"epoch":epoch}),
                            ) {
                                Ok(v) => v,
                                Err(e) => json!({"error":e.to_string()}),
                            };
                        }
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
                    Some(JobPost::Screenshot { guest_wrapped }) => {
                        result = normalize_screenshot(result, guest_wrapped)?;
                    }
                    None => {}
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
fn normalize_backend_input(
    value: Value,
    method: &str,
    through_epoch: Option<u64>,
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
        through_epoch,
    )
}
fn normalize_guest_input(value: Value, method: &str, through_epoch: Option<u64>) -> Result<Value> {
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
    } else if method == "input_revoke" {
        if through_epoch.is_none()
            || result["released"] != true
            || result["synthetic_release_dispatched"] != true
            || result["through_epoch"].as_u64() != through_epoch
        {
            bail!("Guest epoch fence/synthetic release is unconfirmed")
        }
    } else {
        bail!("unexpected Guest input reply")
    }
    Ok(result)
}
fn normalize_screenshot(mut value: Value, guest_wrapped: bool) -> Result<Value> {
    use base64::Engine;
    if guest_wrapped {
        value = value
            .get("result")
            .cloned()
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
fn screenshot_content(mut value: Value) -> Result<Value> {
    let data = value
        .as_object_mut()
        .and_then(|v| v.remove("data_base64"))
        .context("screenshot missing image data")?;
    Ok(
        json!({"content":[{"type":"image","mimeType":value["mime_type"],"data":data},{"type":"text","text":serde_json::to_string(&value)?}],"isError":false}),
    )
}
type FileSessionRoutes = BTreeMap<String, (std::time::Instant, BTreeSet<String>)>;
struct Manager {
    memory: MemoryBudget,
    dir: PathBuf,
    backend: PathBuf,
    qemu: Option<QemuRegistration>,
    qemu_window: Value,
    native_connections: BTreeMap<String, (String, String)>,
    configs: BTreeMap<String, Config>,
    leases: Arc<Mutex<lease::Leases>>,
    jobs: Arc<Mutex<jobs::Jobs>>,
    file_sessions: FileSessionRoutes,
    input_retirements: Arc<Mutex<input_retirement::Retirements>>,
    exec_retirements: Arc<Mutex<exec_scope::Ledger>>,
    running: BTreeMap<String, Running>,
}
impl Manager {
    fn load(dir: PathBuf, backend: PathBuf, memory: MemoryBudget) -> Result<Self> {
        let registry = dir.join("registry.json");
        let configs: BTreeMap<String, Config> = if registry.exists() {
            let bytes = fs::read(&registry)?;
            if bytes.len() > 16 * MAX_MESSAGE {
                bail!("registry exceeds size limit")
            }
            serde_json::from_slice(&bytes).context("invalid registry; preserving original")?
        } else {
            BTreeMap::new()
        };
        for (id, cfg) in &configs {
            validate_id(id)?;
            if *id != cfg.id {
                bail!("registry identity mismatch")
            }
        }
        let exec_retirements =
            exec_scope::Ledger::open(&dir, configs.values().any(|c| c.qemu_guest_exec))?;
        Ok(Self {
            memory,
            dir,
            backend,
            qemu: None,
            qemu_window: json!({"native_window":false,"display_only":true,"input_enabled":false}),
            native_connections: BTreeMap::new(),
            configs,
            leases: Arc::new(Mutex::new(lease::Leases::default())),
            jobs: Arc::new(Mutex::new(jobs::Jobs::default())),
            file_sessions: BTreeMap::new(),
            input_retirements: Arc::new(Mutex::new(input_retirement::Retirements::default())),
            exec_retirements: Arc::new(Mutex::new(exec_retirements)),
            running: BTreeMap::new(),
        })
    }
    fn prune_input_retirements(&mut self) -> Result<()> {
        let records = self.input_retirements.lock().unwrap().records(None, None);
        for record in records {
            if let Some(run) = self.running.get_mut(&record.vm) {
                // Replacing a Running is allowed only after its old process was reaped.
                let replaced = run.bridge.lock().unwrap().worker_instance != record.worker_instance;
                if replaced || run.poll_exit()?.is_some() {
                    self.input_retirements.lock().unwrap().complete(&record);
                }
            }
        }
        Ok(())
    }
    fn input_revocations(
        &mut self,
        owner: Option<&str>,
        vm: Option<&str>,
    ) -> Result<Vec<InputRetirement>> {
        self.prune_input_retirements()?;
        Ok(self.input_retirements.lock().unwrap().records(owner, vm))
    }
    fn assigned_memory_mib(&mut self) -> Result<u64> {
        self.running.values_mut().try_fold(0u64, |sum, runtime| {
            if runtime.poll_exit()?.is_none() {
                sum.checked_add(runtime.assigned_memory_mib)
                    .context("active assigned memory arithmetic overflow")
            } else {
                Ok(sum)
            }
        })
    }
    fn reconcile_normal_execution_exit(&mut self, id: &str) -> Result<()> {
        let exit = self
            .running
            .get_mut(id)
            .map(Running::poll_exit)
            .transpose()?
            .flatten();
        self.reconcile_observed_normal_execution_exit(id, exit)
    }
    fn reconcile_observed_normal_execution_exit(
        &mut self,
        id: &str,
        exit: Option<std::process::ExitStatus>,
    ) -> Result<()> {
        if !exit.is_some_and(|s| s.success())
            || !self.configs.get(id).is_some_and(|c| c.qemu_guest_exec)
        {
            return Ok(());
        }
        let Some(run) = self.running.get(id) else {
            return Ok(());
        };
        let instance = run.bridge.lock().unwrap().worker_instance.clone();
        if !self
            .exec_retirements
            .lock()
            .unwrap()
            .current_launch(id, &instance)
        {
            return Ok(());
        }
        // Directly owned successful child exit is eligibility only. The exact original
        // witness must still become exclusively lockable and removal durably commit.
        self.exec_retirements
            .lock()
            .unwrap()
            .reconcile(&self.dir, id, &instance)
    }
    fn view(&mut self, id: &str) -> Result<Value> {
        // Sample once: a later poll must not render a terminal state that skipped
        // reconciliation because an earlier poll still observed a live child.
        let exit = self
            .running
            .get_mut(id)
            .map(Running::poll_exit)
            .transpose()?
            .flatten();
        let automatic_cleanup_failed = self
            .reconcile_observed_normal_execution_exit(id, exit)
            .is_err();
        let config = self.configs.get(id).context("VM not found")?;
        let mut result = json!({"config":config,"state":"stopped","capabilities":backend::guest_capabilities(config.backend,&config.guest)});
        if config.backend == Backend::AppleVirtualization && config.guest == "macos" {
            result["capabilities"]["screenshot"] = json!(true);
            result["capabilities"]["screenshot_availability"]=json!("requires running macOS Guest service --allow-observe and Guest Screen Recording permission; query guest_info for current availability");
        }
        if config.backend == Backend::QemuHvf {
            result["capabilities"]["graphics_profile"] = json!({"configured":config.qemu_graphics_profile,"local_runtime_registered":self.qemu.as_ref().is_some_and(|q|q.venus_runtime.is_some()),"host_supported":self.running.get(id).is_some_and(|r|r.state.lock().unwrap()["graphics_profile"]["host_supported"]==true),"guest_verified":false,"scope":"explicit experimental host profile; actual Guest application Vulkan driver/pixels require independent validation"});
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
            result["capabilities"]["input_scope"] = json!("absolute click/drag/discrete wheel/explicit qcode only; snapshot <=5s, generation and current lease required; no text/layout translation");
            result["capabilities"]["screenshot"] =
                json!(self.qemu.as_ref().is_some_and(|q| q.has_screenshot()));
        }
        if config.backend == Backend::QemuHvf {
            result["capabilities"]["native_window"] =
                json!(backend::window_supported(&self.qemu_window));
            result["capabilities"]["native_window_details"] = self.qemu_window.clone();
        }
        if let Some(runtime) = self.running.get_mut(id) {
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
            }
        }
        if automatic_cleanup_failed {
            result["execution_reconciliation_status"] = json!("unconfirmed");
        }
        let incarnations = self.exec_retirements.lock().unwrap().incarnations(id);
        if !incarnations.is_empty() {
            result["execution_incarnations"] = serde_json::to_value(&incarnations)?;
            result["execution_reconciliation_required"] = json!(result.get("pid").is_none());
        }
        if self
            .exec_retirements
            .lock()
            .unwrap()
            .recovery_required(None, Some(id))
        {
            result["state"] = json!("recovery_required");
            result["execution_recovery_required"] = json!(true);
        }
        Ok(result)
    }
    fn dispatch(&mut self, request: Value, deferred: &mut Option<Completion>) -> Result<Value> {
        let method = request["method"].as_str().context("missing method")?;
        let args = &request["arguments"];
        let guest_shutdown = method == "shutdown" && shutdown_mode(args)? == "guest";
        if method == "session_end" {
            let owner = args["owner"].as_str().context("missing session owner")?;
            if owner.len() != 64 || !owner.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("invalid session owner")
            }
            let released = self
                .leases
                .lock()
                .unwrap()
                .retire_owner(owner, std::time::Instant::now());
            *deferred = Some(Completion::CancelJobs {
                dir: self.dir.clone(),
                input_revocations: self.input_revocations(Some(owner), None)?,
                exec_revocations: self
                    .exec_retirements
                    .lock()
                    .unwrap()
                    .begin(Some(owner), None),
                exec_retirements: self.exec_retirements.clone(),
                input_retirements: self.input_retirements.clone(),
                file_sessions: self
                    .file_sessions
                    .remove(owner)
                    .map(|(_, vms)| vms)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|vm| json!({"id":vm,"owner":owner}))
                    .collect(),
                response: json!({"closed":true,"retired_owner":true,"retired_owner_ttl_seconds":120,"released_vm_ids":released}),
                jobs: self.jobs.lock().unwrap().owned_running(
                    owner,
                    None,
                    std::time::Instant::now(),
                ),
            });
            if self
                .exec_retirements
                .lock()
                .unwrap()
                .recovery_required(Some(owner), None)
            {
                *deferred = Some(Completion::RecoveryFence(Box::new(
                    deferred.take().unwrap(),
                )));
            }
            return Ok(Value::Null);
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
            return Ok(
                json!({"resource_admission":{"physical_memory_mib":self.memory.physical_memory_mib,"vm_memory_budget_mib":self.memory.vm_memory_budget_mib,"active_assigned_memory_mib":assigned,"remaining_budget_mib":self.memory.vm_memory_budget_mib.saturating_sub(assigned),"explicitly_configured":self.memory.explicitly_configured,"scope":"assigned guest RAM in this daemon; excludes VM overhead, other daemons and unrelated host workloads; not a host pressure guarantee"},"backends":{"apple-virtualization":backend::capabilities(Backend::AppleVirtualization),"qemu-hvf":{"registered":self.qemu.is_some(),"capabilities":qemu_capabilities}},"backend":"apple-virtualization","guests":["linux","macos"],"windows":false,"window_enumeration":"macOS Guest --allow-observe with existing Screen Recording permission and eligible console; no titles/AX tree/input authority; other backends unsupported","lifecycle":true,"native_window":true,"screenshot":true,"screenshot_requirements":"macOS Guest --allow-observe and existing Guest Screen Recording permission, or registered QEMU capture; Apple Linux screenshot unsupported","input":true,"input_requirements":"registered QEMU persistent console or macOS Guest --allow-observe --allow-input with existing Guest Screen Recording/Accessibility permissions; independent MCP input permission, current lease and recent matching screenshot; Apple Linux input unsupported","command_execution":"Apple Linux backend requires enabled async Guest jobs; macOS legacy exec is not exposed; QEMU requires explicit qemu_guest_exec v2 and rootless cgroup-v2 Guest service","async_commands":"Apple Linux backend requires exec_start/exec_poll/exec_cancel support; owned jobs are VM/client/original-epoch bound; QEMU default observer transport has no command capability; explicit exec v2 uses registered original-worker scopes and confirmed scope revocation","file_transfer":"Apple backend requires enabled Guest files integration: Linux dedicated non-root files account or macOS opt-in same-UID private root; QEMU private serial transports have no file capability; query vm_guest_info for current capabilities and file_policy; single filenames, 8MiB max","control_session_fencing":{"retired_owner_ttl_seconds":120,"request_dispatch_deadline_seconds":30},"remote_desktop":false,"guest_integration":"Apple backend uses enabled Linux/macOS host-only vsock service; QEMU private virtio-serial defaults to observer-only v1; explicit qemu_guest_exec selects v2 requiring rootless cgroup-v2 Guest execution; query vm_guest_info for actual availability and capabilities","guest_shutdown":"Apple Linux integration requires explicit shutdown enablement; QEMU guest-service shutdown is unsupported and QMP power-button requests remain separate; request acceptance is not shutdown completion","gpu":"requires guest measurements; no performance claim"}),
            );
        }
        if method == "create" {
            let config: Config = serde_json::from_value(args.clone())?;
            config.validate()?;
            if config.backend == Backend::QemuHvf {
                backend::require_registration(&self.qemu)?
                    .require_profile(config.qemu_graphics_profile)?;
                if config.window && !backend::window_supported(&self.qemu_window) {
                    bail!("native display-only window unavailable for registered QEMU helper")
                }
            }
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
        if self.configs[id].backend == Backend::QemuHvf
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
        if method == "reconcile_execution" {
            let instance = args["worker_instance"]
                .as_str()
                .context("original worker instance required")?;
            if !native_input::hex(instance, 64) {
                bail!("invalid original worker instance")
            }
            self.exec_retirements
                .lock()
                .unwrap()
                .reconcile(&self.dir, id, instance)?;
            return Ok(
                json!({"reconciled":true,"basis":"original_incarnation_witness_exclusive","id":id,"worker_instance":instance}),
            );
        }
        let now = std::time::Instant::now();
        if self
            .exec_retirements
            .lock()
            .unwrap()
            .recovery_required(None, Some(id))
            && (matches!(
                method,
                "start"
                    | "acquire_control"
                    | "release_control"
                    | "shutdown"
                    | "human_takeover"
                    | "human_release"
                    | "input_step"
            ) || method.starts_with("exec")
                && !matches!(method, "exec_scope_revoke" | "exec_scope_abort"))
        {
            bail!("execution recovery required; original scope cleanup unconfirmed")
        }
        if self
            .running
            .get(id)
            .is_some_and(|run| run.execution_quiescing)
            && (method == "acquire_control"
                || method.starts_with("exec_") && method != "exec_scope_abort"
                || method == "input_step")
        {
            bail!("VM execution is quiescing for shutdown; new authority denied")
        }
        if method == "acquire_control" {
            let owner = args["owner"].as_str().context("missing control owner")?;
            if !self
                .exec_retirements
                .lock()
                .unwrap()
                .acquire_allowed(id, owner)
            {
                bail!("previous execution scope cleanup unconfirmed; acquire denied")
            }
            for scope in self
                .exec_retirements
                .lock()
                .unwrap()
                .records(Some(owner), Some(id))
            {
                self.leases.lock().unwrap().check(id,owner,scope.epoch,now).context("previous execution epoch expired; scope cleanup unconfirmed, acquire denied")?;
            }
            let epoch = self.leases.lock().unwrap().acquire(id, owner, now)?;
            return Ok(json!({"epoch":epoch,"expires_in_seconds":60}));
        }
        if method == "release_control" {
            self.leases.lock().unwrap().release(
                id,
                args["owner"].as_str().context("missing control owner")?,
                args["epoch"].as_u64().context("missing control epoch")?,
                now,
            )?;
            *deferred = Some(Completion::CancelJobs {
                dir: self.dir.clone(),
                file_sessions: Vec::new(),
                input_revocations: self.input_revocations(args["owner"].as_str(), Some(id))?,
                exec_revocations: self
                    .exec_retirements
                    .lock()
                    .unwrap()
                    .begin(args["owner"].as_str(), Some(id)),
                exec_retirements: self.exec_retirements.clone(),
                input_retirements: self.input_retirements.clone(),
                response: json!({"released":true}),
                jobs: self.jobs.lock().unwrap().owned_running(
                    args["owner"].as_str().unwrap(),
                    Some(id),
                    now,
                ),
            });
            return Ok(Value::Null);
        }
        if method == "human_takeover" || method == "human_release" {
            let epoch = self.leases.lock().unwrap().human(id, true, now)?;
            if method == "human_takeover" {
                *deferred = Some(Completion::CancelJobs {
                    dir: self.dir.clone(),
                    file_sessions: Vec::new(),
                    input_revocations: self.input_revocations(None, Some(id))?,
                    exec_revocations: self.exec_retirements.lock().unwrap().begin(None, Some(id)),
                    exec_retirements: self.exec_retirements.clone(),
                    input_retirements: self.input_retirements.clone(),
                    response: json!({"epoch":epoch,"human_control":true}),
                    jobs: self.jobs.lock().unwrap().running(id, now),
                });
                return Ok(Value::Null);
            }
            *deferred = Some(Completion::HumanRelease {
                cleanup: Box::new(Completion::CancelJobs {
                    dir: self.dir.clone(),
                    file_sessions: Vec::new(),
                    input_revocations: self.input_revocations(None, Some(id))?,
                    exec_revocations: self.exec_retirements.lock().unwrap().begin(None, Some(id)),
                    exec_retirements: self.exec_retirements.clone(),
                    input_retirements: self.input_retirements.clone(),
                    response: json!({}),
                    jobs: Vec::new(),
                }),
                leases: self.leases.clone(),
                vm: id.into(),
                epoch,
            });
            return Ok(Value::Null);
        }
        if ["exec", "exec_start"].contains(&method) {
            self.leases.lock().unwrap().check(
                id,
                args["owner"]
                    .as_str()
                    .context("acquire control before executing commands")?,
                args["epoch"].as_u64().context("missing control epoch")?,
                now,
            )?;
        }
        if method == "exec" {
            *deferred = Some(Completion::Synchronous {
                dir: self.dir.clone(),
                arguments: args.clone(),
            });
            return Ok(Value::Null);
        }
        if ["exec_poll", "exec_cancel"].contains(&method) {
            self.jobs.lock().unwrap().owned(
                id,
                args["job_id"].as_str().context("missing job_id")?,
                args["owner"].as_str().context("missing job owner")?,
                args["epoch"].as_u64().context("missing job epoch")?,
                now,
            )?;
        }
        if method == "input_revoke" {
            if let Some(expected) = args.get("worker_instance") {
                self.prune_input_retirements()?;
                let run = self
                    .running
                    .get(id)
                    .context("input cleanup worker unavailable")?;
                if expected.as_str() != Some(run.bridge.lock().unwrap().worker_instance.as_str()) {
                    bail!("input cleanup worker instance changed")
                }
            }
        }
        let mac_input = method.starts_with("input_")
            && self.configs[id].backend == Backend::AppleVirtualization
            && self.configs[id].guest == "macos";
        if method.starts_with("input_") {
            if !mac_input
                && (self.configs[id].backend != Backend::QemuHvf
                    || self
                        .qemu
                        .as_ref()
                        .is_none_or(|q| q.console_broker.is_none()))
            {
                bail!("unsupported: desktop input requires QEMU persistent console or enabled macOS Guest input service")
            }
            if !["input_cleanup", "input_revoke"].contains(&method) {
                self.leases.lock().unwrap().check(
                    id,
                    args["owner"]
                        .as_str()
                        .context("acquire control before input")?,
                    args["epoch"].as_u64().context("missing control epoch")?,
                    now,
                )?;
            }
            if ["input_click", "input_drag", "input_scroll", "input_key"].contains(&method) {
                input::plan(method, args)?;
                *deferred = Some(Completion::Input {
                    dir: self.dir.clone(),
                    arguments: args.clone(),
                    method: method.into(),
                });
                return Ok(Value::Null);
            }
        }
        let guest_screenshot = method == "screenshot"
            && self.configs[id].backend == Backend::AppleVirtualization
            && self.configs[id].guest == "macos";
        if method == "screenshot"
            && !guest_screenshot
            && (self.configs[id].backend != Backend::QemuHvf
                || self.qemu.as_ref().is_none_or(|q| !q.has_screenshot()))
        {
            bail!("unsupported: screenshot requires QEMU capture or enabled macOS Guest observation service")
        }
        if method == "status" {
            return self.view(id);
        }
        if method == "start" {
            self.reconcile_normal_execution_exit(id)?;
            let config = self.configs.get(id).context("VM not found")?.clone();
            config.validate()?;
            if config.backend == Backend::QemuHvf {
                backend::require_registration(&self.qemu)?
                    .require_profile(config.qemu_graphics_profile)?;
            }
            if config.backend == Backend::QemuHvf
                && config.window
                && !backend::window_supported(&self.qemu_window)
            {
                bail!("native display-only window unavailable for registered QEMU helper")
            }
            if let Some(run) = self.running.get_mut(id) {
                if run.poll_exit()?.is_none() {
                    bail!("VM process already active")
                }
            }
            if config.qemu_guest_exec && self.exec_retirements.lock().unwrap().pending_launch(id) {
                bail!("original incarnation requires explicit reconciliation before restart")
            }
            if config.qemu_guest_exec {
                self.leases.lock().unwrap().fence_for_shutdown(id, now)?;
            }
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
            self.jobs.lock().unwrap().clear_vm(id);
            let config_path = self.dir.join(format!("{id}.json"));
            atomic_json(&config_path, &config)?;
            let mut command = match config.backend {
                Backend::AppleVirtualization => {
                    let mut command = Command::new(&self.backend);
                    command.arg("run");
                    command
                }
                Backend::QemuHvf => backend::require_registration(&self.qemu)?.command(),
            };
            let bridge = Arc::new(Mutex::new(bridge::State::new()?));
            let witness = if config.qemu_guest_exec {
                self.exec_retirements
                    .lock()
                    .unwrap()
                    .admit_launch(id, &bridge.lock().unwrap().worker_instance)?;
                let witness = exec_witness::Witness::create(
                    &self.dir.join("exec-journal"),
                    id,
                    &bridge.lock().unwrap().worker_instance,
                )?;
                self.exec_retirements
                    .lock()
                    .unwrap()
                    .launch(witness.identity.clone())?;
                witness.configure_worker(&mut command)?;
                Some(witness)
            } else {
                None
            };
            if config.qemu_guest_exec {
                command
                    .arg("--guest-worker-instance")
                    .arg(&bridge.lock().unwrap().worker_instance);
            }
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
            let mut child = command
                .arg("--config")
                .arg(config_path)
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("starting VM backend")?;
            drop(command);
            drop(witness);
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            let state = Arc::new(Mutex::new(json!({"state":"starting"})));
            let pending: Arc<Mutex<BTreeMap<u64, PendingReply>>> =
                Arc::new(Mutex::new(BTreeMap::new()));
            let output_state = state.clone();
            let output_pending = pending.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_frame_bounded(&mut reader, MAX_SCREENSHOT_RESPONSE) {
                        Ok(Some(bytes)) => match serde_json::from_slice::<Value>(&bytes) {
                            Ok(value) => {
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
                                if value["event"] == "state" {
                                    replace_runtime_state(
                                        &mut output_state.lock().unwrap(),
                                        value.clone(),
                                    );
                                }
                                if let Some(id) = value["id"].as_u64() {
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
                output_pending.lock().unwrap().clear();
            });
            // A new VM process creates a new Guest service lifetime. Do not carry
            // cleanup-required state from the previous, already exited Guest.
            self.running.insert(
                id.into(),
                Running {
                    bridge,
                    child,
                    assigned_memory_mib: config.memory_mib,
                    expected_exit: false,
                    execution_quiescing: false,
                    exit_status: None,
                    disk_identity,
                    stdin,
                    state,
                    pending,
                    next: 1,
                },
            );
            return self.view(id);
        }
        if ["exec_scope_revoke", "exec_scope_abort"].contains(&method) {
            if !self.configs[id].qemu_guest_exec {
                bail!("unsupported execution scope transport")
            }
            let expected = args["worker_instance"]
                .as_str()
                .context("scope worker identity required")?;
            let run = self
                .running
                .get_mut(id)
                .context("execution scope worker unavailable")?;
            if run.bridge.lock().unwrap().worker_instance != expected || run.poll_exit()?.is_some()
            {
                bail!("execution scope cleanup unconfirmed: original worker no longer available")
            }
            if method == "exec_scope_abort" {
                run.force_stop()?;
                bail!("execution scope cleanup unconfirmed: forced termination requested")
            }
            let number = run.next;
            run.next += 1;
            let (tx, rx) = mpsc::channel();
            run.pending.lock().unwrap().insert(number, tx.into());
            write_json(
                &mut run.stdin,
                &json!({"id":number,"method":"guest_scope_revoke","qemu_exec_deadline_ns":exec_scope::monotonic_ns()?+5_000_000_000,"request":{"id":number.to_string(),"method":"scope_revoke","scope":{"owner":args["owner"],"epoch":args["epoch"]}}}),
            )?;
            *deferred = Some(Completion::Backend(Pending {
                receiver: rx,
                number,
                pending: run.pending.clone(),
                timeout: Duration::from_secs(6),
                post: None,
            }));
            return Ok(Value::Null);
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
            if !["linux", "macos"].contains(&self.configs.get(id).unwrap().guest.as_str()) {
                bail!("unsupported: file integration requires enabled Linux or macOS Guest service")
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
                "input_cleanup",
                "input_revoke",
                "guest_info",
                "windows",
                "exec_start",
                "exec_poll",
                "exec_cancel",
            ]
            .contains(&method)
        {
            let run = self.running.get_mut(id).context("VM process not active")?;
            if run.poll_exit()?.is_some() {
                bail!("VM process has exited")
            }
            if method == "shutdown" && self.configs[id].qemu_guest_exec {
                if !self
                    .exec_retirements
                    .lock()
                    .unwrap()
                    .records(None, Some(id))
                    .is_empty()
                {
                    bail!("shutdown refused: execution scope cleanup unconfirmed; release control or end sessions first")
                }
                // Under the Manager lock: no start can register between this check and fencing.
                run.execution_quiescing = true;
                self.leases.lock().unwrap().fence_for_shutdown(id, now)?;
            }
            let number = run.next;
            run.next += 1;
            let (tx, rx) = mpsc::channel();
            let mut command = json!({"id":number,"method":method});
            if method == "shutdown" && self.configs[id].qemu_guest_exec {
                command["execution_scopes_empty"] = json!(true);
            }
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
            if method.starts_with("input_") {
                if mac_input {
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
            if method == "windows" {
                command["method"] = json!("guest_request");
                command["request"] = json!({"id":number.to_string(),"method":"windows"});
            }
            if method == "guest_info" {
                command["method"] = json!("guest_request");
                command["request"] = json!({"id":number.to_string(),"method":"info"});
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
                let timeout = args
                    .get("timeout_ms")
                    .map(|v| v.as_u64().context("invalid timeout_ms"))
                    .transpose()?
                    .unwrap_or(5000);
                if !(1..=30000).contains(&timeout) {
                    bail!("timeout_ms must be 1..30000")
                }
                command["method"] = json!("guest_request");
                command["request"] = json!({"id":number.to_string(),"method":method,"argv":argv,"timeout_ms":timeout});
            }
            if ["exec_poll", "exec_cancel"].contains(&method) {
                command["method"] = json!("guest_request");
                command["request"] =
                    json!({"id":number.to_string(),"method":method,"job_id":args["job_id"]});
            }
            let post = if bridge_post.is_some() {
                bridge_post
            } else if method.starts_with("input_") {
                Some(JobPost::GuestInput {
                    guest_wrapped: mac_input,
                    method: method.into(),
                    through_epoch: args["through_epoch"].as_u64(),
                })
            } else if method == "windows" {
                Some(JobPost::Windows)
            } else if method == "screenshot" {
                Some(JobPost::Screenshot {
                    guest_wrapped: guest_screenshot,
                })
            } else if method == "exec_start" {
                Some(JobPost::Start {
                    reservation: jobs::Jobs::reserve(&self.jobs, now)?,
                    vm: id.into(),
                    owner: args["owner"].as_str().unwrap().into(),
                    epoch: args["epoch"].as_u64().unwrap(),
                    dir: self.dir.clone(),
                    leases: self.leases.clone(),
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
            if method == "serial_write" {
                let data = args["data"]
                    .as_str()
                    .context("missing base64 serial data")?;
                if data.len() > 87384 {
                    bail!("serial payload exceeds 64 KiB")
                }
                command["data"] = json!(data);
            }
            if method == "input_step" {
                self.input_retirements
                    .lock()
                    .unwrap()
                    .record(InputRetirement {
                        owner: args["owner"]
                            .as_str()
                            .context("input owner required")?
                            .into(),
                        vm: id.into(),
                        epoch: args["epoch"].as_u64().context("input epoch required")?,
                        worker_instance: run.bridge.lock().unwrap().worker_instance.clone(),
                    })?;
            }
            if self.configs[id].qemu_guest_exec
                && ["exec_start", "exec_poll", "exec_cancel"].contains(&method)
            {
                let owner = args["owner"]
                    .as_str()
                    .context("execution scope owner required")?;
                if !native_input::hex(owner, 64) {
                    bail!("invalid execution scope owner")
                }
                let epoch = args["epoch"]
                    .as_u64()
                    .filter(|e| *e > 0 && *e <= 9007199254740991)
                    .context("execution scope epoch invalid")?;
                if method == "exec_start" {
                    self.exec_retirements
                        .lock()
                        .unwrap()
                        .record(exec_scope::Scope {
                            vm: id.into(),
                            worker_instance: run.bridge.lock().unwrap().worker_instance.clone(),
                            owner: owner.into(),
                            epoch,
                        })?;
                }
                command["qemu_exec_deadline_ns"] =
                    json!(exec_scope::monotonic_ns()? + 1_000_000_000);
                command["request"]["scope"] = json!({"owner":owner,"epoch":epoch});
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
                timeout: Duration::from_secs(if mac_input {
                    4
                } else if method == "screenshot" {
                    17
                } else if method == "windows" {
                    12
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
pub fn serve(dir: PathBuf, backend: PathBuf, memory_budget_mib: Option<u64>) -> Result<()> {
    serve_with_qemu(dir, backend, memory_budget_mib, None)
}
pub fn serve_with_qemu(
    dir: PathBuf,
    backend: PathBuf,
    memory_budget_mib: Option<u64>,
    qemu: Option<QemuRegistration>,
) -> Result<()> {
    if let Some(registration) = &qemu {
        registration.validate()?;
    }
    let memory = MemoryBudget::detect(memory_budget_mib)?;
    private_dir(&dir)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join("daemon.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another daemon holds this state directory")
    }
    let socket = dir.join("control.sock");
    if socket.exists() {
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let mut manager = Manager::load(dir, backend, memory)?;
    manager.qemu_window = match qemu
        .as_ref()
        .and_then(|q| q.console_broker.as_ref().zip(q.console_helper.as_ref()))
    {
        Some((broker, helper)) => backend::probe_broker_window(broker, helper)?,
        None => json!({"native_window":false,"display_only":true,"input_enabled":false}),
    };
    manager.qemu = qemu;
    let manager = Arc::new(Mutex::new(manager));
    let monitor = Arc::downgrade(&manager);
    thread::spawn(move || {
        while let Some(manager) = monitor.upgrade() {
            for runtime in manager.lock().unwrap().running.values_mut() {
                let _ = runtime.poll_exit();
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for connection in listener.incoming() {
        let stream = connection?;
        let admitted_at = std::time::Instant::now();
        if active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 32 {
            active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            drop(stream);
            continue;
        }
        let manager = manager.clone();
        let active = active.clone();
        thread::spawn(move || {
            let _ = handle_connection(stream, manager, admitted_at);
            active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
    }
    Ok(())
}
fn handle_connection(
    mut stream: UnixStream,
    manager: Arc<Mutex<Manager>>,
    admitted_at: std::time::Instant,
) -> Result<()> {
    // Darwin may return EINVAL when setting socket options after its peer closed.
    // A disconnected socket cannot block reads: drain its already-buffered cleanup
    // request, but continue to reject acquire below. Other option failures are fatal.
    for result in [
        stream.set_read_timeout(Some(Duration::from_secs(15))),
        stream.set_write_timeout(Some(Duration::from_secs(15))),
    ] {
        if let Err(error) = result {
            if !peer_disconnected(&[stream.as_raw_fd()]) {
                return Err(error.into());
            }
        }
    }
    let frame = read_frame(&mut BufReader::new(stream.try_clone()?))?.context("empty request")?;
    let request: Value = serde_json::from_slice(&frame)?;
    if request["method"] == "native_attach" {
        return native_control::serve(stream, manager, request, admitted_at);
    }
    let mut deferred = None;
    let result = serde_json::from_slice(&frame)
        .map_err(anyhow::Error::from)
        .and_then(|req: Value| {
            let mut manager = manager.lock().unwrap();
            // Include socket reads and queue/manager-lock delay. A late request
            // cannot outlive the 120s closed-owner fence and acquire a fresh lease.
            if admitted_at.elapsed() > Duration::from_secs(30) {
                bail!("controller request expired before dispatch")
            }
            if req["method"] == "acquire_control" && peer_disconnected(&[stream.as_raw_fd()]) {
                bail!("control acquisition client disconnected before dispatch")
            }
            manager.dispatch(req, &mut deferred)
        })
        .and_then(|value| match deferred {
            Some(p) => p.wait(&stream),
            None => Ok(value),
        });
    write_json(
        &mut stream,
        &match result {
            Ok(value) => json!({"result":value}),
            Err(e) => json!({"error":e.to_string()}),
        },
    )
}
pub fn call(dir: &Path, method: &str, args: Value) -> Result<Value> {
    let mut socket = UnixStream::connect(dir.join("control.sock"))
        .context("connect daemon; run vm-control daemon first")?;
    socket.set_read_timeout(Some(Duration::from_secs(40)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    write_json(&mut socket, &json!({"method":method,"arguments":args}))?;
    let bytes = read_frame_bounded(&mut BufReader::new(socket), response_limit(method))?
        .context("daemon disconnected")?;
    let reply: Value = serde_json::from_slice(&bytes)?;
    if let Some(error) = reply.get("error") {
        if let Some(message) = error.as_str() {
            bail!("{message}")
        }
        bail!("{error}")
    }
    Ok(reply["result"].clone())
}

pub fn tools_list(lifecycle: bool, execute: bool) -> Value {
    tools_with_files(lifecycle, execute, false)
}
fn tools_with_files(lifecycle: bool, execute: bool, files: bool) -> Value {
    tools_with_input(lifecycle, execute, files, false)
}
fn tools_with_input(lifecycle: bool, execute: bool, files: bool, input: bool) -> Value {
    let id = json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false});
    let mut tools = vec![
        json!({"name":"vm_list","description":"List registered VMs and actual backend process state.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
        json!({"name":"vm_screenshot","description":"Capture the same console at full source resolution: QEMU PNG or opted-in macOS Guest JPEG. Returns actual MIME, frame id, dimensions, scale and capture timing. macOS requires existing Guest Screen Recording permission; never requests it automatically.","inputSchema":id}),
        json!({"name":"vm_windows","description":"Read macOS Guest on-screen application window metadata with existing Screen Recording permission and --allow-observe. No titles, focus, stacking order, Accessibility tree or input authority. Coordinates are Guest desktop-global points; this enumeration is not an atomic screenshot. Other Guest/backend combinations are unsupported.","inputSchema":id}),
        json!({"name":"vm_guest_info","description":"Query the real Guest service: Apple host-only vsock or QEMU private virtio-serial (default observer-only; explicit v2 permits rootless cgroup execution). Reports actual capabilities; fails if unavailable.","inputSchema":id}),
        json!({"name":"vm_status","description":"Get VM configuration and process state; running does not assert guest health.","inputSchema":id}),
        json!({"name":"vm_capabilities","description":"Report implemented operations and explicitly unsupported integrations.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
    ];
    if lifecycle {
        for (name, description) in [
            ("vm_start", "Start a VM in its own native backend process."),
            (
                "vm_shutdown",
                "Request normal shutdown via backend power button (default) or opt-in installed guest service (mode guest). Request acceptance is not completion; poll status. Never implicitly force-stops.",
            ),
            ("vm_show_window", "Show native VM console; QEMU input requires a verified v2 helper, its bound private controller channel, and explicit local Take Control. Showing a window alone grants no input authority."),
            (
                "vm_close_window",
                "Close native console while keeping VM running.",
            ),
        ] {
            let schema=if name=="vm_shutdown" {json!({"type":"object","properties":{"id":{"type":"string"},"mode":{"type":"string","enum":["backend","guest"],"default":"backend"}},"required":["id"],"additionalProperties":false})}else{id.clone()};
            tools.push(json!({"name":name,"description":description,"inputSchema":schema}));
        }
        tools.push(json!({"name":"vm_force_stop","description":"Kill VM process. Can lose guest data. Requires explicit confirmation equal to VM id.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"confirm":{"type":"string"}},"required":["id","confirm"],"additionalProperties":false}}));
        let mut properties = json!({"backend":{"type":"string","enum":["apple-virtualization","qemu-hvf"],"default":"apple-virtualization"},"id":{"type":"string"},"guest":{"type":"string","enum":["linux","macos"]},"cpus":{"type":"integer","minimum":1,"maximum":128},"memory_mib":{"type":"integer","minimum":256,"maximum":1048576},"window":{"type":"boolean"}});
        properties["qemu_graphics_profile"] = json!({"type":"string","enum":["virgl","venus-moltenvk-experimental"],"default":"virgl","description":"Linux QEMU graphics profile. Experimental Venus requires a trusted runtime pre-registered by the local daemon operator; this option grants no binary paths, environment overrides or Guest execution capability."});
        for p in ["width", "height"] {
            properties[p] = json!({"type":"integer","minimum":320,"maximum":8192,"description":"QEMU requires both initial width and height, even values, max3840x2160; Guest actual mode may differ. No dynamic resizing."});
        }
        for p in [
            "kernel",
            "initrd",
            "command_line",
            "disk",
            "restore_image",
            "machine_identity_dir",
            "serial_log",
            "shared_dir",
        ] {
            properties[p] = json!({"type":"string"});
        }
        tools.push(json!({"name":"vm_create","description":"Register an existing Linux direct-boot or installed macOS VM configuration; does not install an OS or create storage.","inputSchema":{"type":"object","properties":properties,"required":["id","guest","cpus","memory_mib"],"additionalProperties":false}}));
    }
    if execute || input {
        for (name,description) in [("vm_acquire_control","Acquire or renew 60-second VM control lease. Human takeover revokes it immediately for subsequent commands."),("vm_release_control","Release this MCP session's control lease.")] {tools.push(json!({"name":name,"description":description,"inputSchema":id}));}
    }
    if execute {
        tools.push(json!({"name":"vm_exec","description":"Execute argv directly inside the VM through its guest service. No implicit shell. Output bounded by guest service; timeout enforced in guest.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"argv":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":128},"timeout_ms":{"type":"integer","minimum":1,"maximum":30000}},"required":["id","argv"],"additionalProperties":false}}));
    }
    if execute {
        tools.push(json!({"name":"vm_exec_start","description":"Start a bounded asynchronous guest command under the current VM control lease. Returns an opaque owned job id; poll for completion.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"argv":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":128},"timeout_ms":{"type":"integer","minimum":1,"maximum":30000}},"required":["id","argv"],"additionalProperties":false}}));
        for (name,description) in [("vm_exec_poll","Poll only a job started by this MCP connection on this VM and original lease epoch. Inspect terminal result, exit code, cancelled and timed_out."),("vm_exec_cancel","Request cancellation of this connection's guest job. Cancellation is not rollback; poll to verify the actual terminal state.")] {tools.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":{"id":{"type":"string"},"job_id":{"type":"string","pattern":"^[0-9a-f]{32}$"}},"required":["id","job_id"],"additionalProperties":false}}));}
    }
    if input {
        for (name, extra, required) in [
            (
                "input_click",
                json!({"x":{"type":"integer"},"y":{"type":"integer"},"button":{"type":"string","enum":["left","middle","right"]}}),
                vec!["x", "y"],
            ),
            (
                "input_drag",
                json!({"x":{"type":"integer"},"y":{"type":"integer"},"end_x":{"type":"integer"},"end_y":{"type":"integer"},"button":{"type":"string","enum":["left","middle","right"]}}),
                vec!["x", "y", "end_x", "end_y"],
            ),
            (
                "input_scroll",
                json!({"x":{"type":"integer"},"y":{"type":"integer"},"direction":{"type":"string","enum":["up","down","left","right"]},"ticks":{"type":"integer","minimum":1,"maximum":20}}),
                vec!["x", "y", "direction", "ticks"],
            ),
            (
                "input_key",
                json!({"keys":{"type":"array","items":{"type":"string","enum":input::key_names()},"minItems":1,"maxItems":8}}),
                vec!["keys"],
            ),
        ] {
            let mut properties = extra;
            for k in ["id", "frame_id"] {
                properties[k] = json!({"type":"string"});
            }
            for k in ["width", "height", "source_generation"] {
                properties[k] = json!({"type":"integer"});
            }
            let mut required = required;
            required.extend(["id", "frame_id", "width", "height", "source_generation"]);
            tools.push(json!({"name":format!("vm_{name}"),"description":"Bounded physical QEMU console or opted-in macOS Guest input. Requires current control and a screenshot younger than 5 seconds with matching VM, generation and dimensions. Backend dispatch does not prove app acceptance; no Unicode/layout translation; query Guest supported physical keys.","inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}));
        }
    }
    if files {
        for (name, description, extra, required) in [
            ("file_upload_begin", "Begin <=8MiB upload of a single filename in the enabled Guest files root. Does not overwrite. Linux uses a dedicated files account; macOS uses an opt-in private root with the service UID, not isolation from same-user programs. Query vm_guest_info for availability and file_policy.", json!({"path":{"type":"string"},"size":{"type":"integer","minimum":0,"maximum":8388608},"sha256":{"type":"string","pattern":"^[0-9a-f]{64}$"}}),vec!["path","size","sha256"]),
            ("file_upload_write", "Append one base64 chunk <=32KiB at the exact sequential offset.",json!({"token":{"type":"string"},"offset":{"type":"integer","minimum":0},"data":{"type":"string","maxLength":43692}}),vec!["token","offset","data"]),
            ("file_upload_commit", "Verify size and SHA256 then atomically publish without replacing existing files.",json!({"token":{"type":"string"}}),vec!["token"]),
            ("file_download_begin", "Open an owned single-link regular file in the configured files directory, max8MiB.",json!({"path":{"type":"string"}}),vec!["path"]),
            ("file_download_read", "Read next <=32KiB base64 chunk; verify complete download against SHA256. Final chunk closes transfer.",json!({"token":{"type":"string"},"offset":{"type":"integer","minimum":0}}),vec!["token","offset"]),
            ("file_cancel", "Cancel this session's transfer and remove any unfinished upload. Published files are not deleted.",json!({"token":{"type":"string"}}),vec!["token"]),
        ] {
            let mut properties=extra;properties["id"]=json!({"type":"string"});let mut required=required;required.push("id");
            tools.push(json!({"name":format!("vm_{name}"),"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}));
        }
    }
    json!({"tools":tools})
}
fn rpc_error(id: Value, code: i64, message: impl ToString) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message.to_string()}})
}
struct SessionCleanup {
    state: Arc<SessionCleanupState>,
}
struct SessionCleanupState {
    dir: PathBuf,
    owner: String,
    stopped: AtomicBool,
    // The first cleanup owns the daemon retirement response. A second call must
    // not replace a failed result with an empty, apparently successful response.
    outcome: Mutex<Option<bool>>,
}
impl SessionCleanupState {
    fn finish(&self) -> Result<()> {
        self.stopped.store(true, Ordering::Release);
        let mut outcome = self.outcome.lock().unwrap();
        let clean = *outcome.get_or_insert_with(|| cleanup_session(&self.dir, &self.owner).is_ok());
        if !clean {
            bail!(
                "session cleanup unconfirmed; authority retirement or Guest cleanup request failed"
            )
        }
        Ok(())
    }
}
impl Drop for SessionCleanup {
    fn drop(&mut self) {
        if self.state.finish().is_err() {
            eprintln!("vm-control: session cleanup unconfirmed");
        }
    }
}
fn cleanup_session(dir: &Path, owner: &str) -> Result<()> {
    let result = call(dir, "session_end", json!({"owner":owner}))?;
    validate_session_cleanup(&result)
}
fn validate_session_cleanup(result: &Value) -> Result<()> {
    if result["closed"] != true || result["retired_owner"] != true {
        bail!("session authority retirement unconfirmed")
    }
    for key in ["job_cancellation", "file_cancellation"] {
        let cancellation = &result[key];
        let attempted = cancellation["attempted"]
            .as_u64()
            .context("missing cleanup count")?;
        let outcomes = cancellation["outcomes"]
            .as_array()
            .context("missing cleanup outcomes")?;
        if cancellation["unconfirmed_requests"].as_u64() != Some(0)
            || outcomes.len() as u64 != attempted
            || outcomes.iter().any(|outcome| {
                outcome.get("error").is_some()
                    || outcome.get("result").is_none()
                    || outcome["result"].get("error").is_some()
                    || if key == "file_cancellation" {
                        outcome["result"]["result"]["cancelled_transfers"]
                            .as_u64()
                            .is_none()
                    } else {
                        outcome["result"]["result"]["cancel_requested"]
                            .as_bool()
                            .is_none()
                    }
            })
        {
            bail!("Guest cleanup request failed or unconfirmed")
        }
    }
    if result.get("input_release").is_some() && result["input_release"]["confirmed"] != true {
        bail!("Guest input release unconfirmed")
    }
    // A valid cancel_requested reply is not evidence that a job has reached its
    // terminal state. This receipt covers retirement and cleanup RPC outcomes.
    Ok(())
}

pub fn mcp(dir: &Path, lifecycle: bool, execute: bool, files: bool) -> Result<()> {
    mcp_with_input(dir, lifecycle, execute, files, false)
}
pub fn mcp_with_input(
    dir: &Path,
    lifecycle: bool,
    execute: bool,
    files: bool,
    input: bool,
) -> Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut random = [0u8; 32];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let owner = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let disconnected = Arc::new(AtomicBool::new(false));
    let cleanup = SessionCleanup {
        state: Arc::new(SessionCleanupState {
            dir: dir.to_path_buf(),
            owner: owner.clone(),
            stopped: AtomicBool::new(false),
            outcome: Mutex::new(None),
        }),
    };
    let monitor_cleanup = cleanup.state.clone();
    let monitor_disconnected = disconnected.clone();
    thread::spawn(move || {
        while !monitor_cleanup.stopped.load(Ordering::Acquire) {
            if peer_disconnected(&[libc::STDIN_FILENO, libc::STDOUT_FILENO]) {
                monitor_disconnected.store(true, Ordering::Release);
                let _ = monitor_cleanup.finish();
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
    let protocol_result = (|| -> Result<()> {
        let mut epochs: BTreeMap<String, u64> = BTreeMap::new();
        let mut job_epochs: BTreeMap<(String, String), (u64, std::time::Instant, bool)> =
            BTreeMap::new();
        let mut initialized = false;
        let mut ready = false;
        while let Some(bytes) = read_frame(&mut reader)? {
            // A blocked synchronous tool must not defer cleanup until the next stdin read.
            // Also discard queued requests after a disconnected transport was observed.
            if disconnected.load(Ordering::Acquire) {
                break;
            }
            let request: Value = match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(_) => {
                    write_json(&mut writer, &rpc_error(Value::Null, -32700, "Parse error"))?;
                    continue;
                }
            };
            let id = request.get("id").cloned();
            if request["jsonrpc"] != "2.0"
                || !request["method"].is_string()
                || id
                    .as_ref()
                    .is_some_and(|v| !v.is_string() && !v.is_i64() && !v.is_u64())
            {
                write_json(
                    &mut writer,
                    &rpc_error(Value::Null, -32600, "Invalid request"),
                )?;
                continue;
            }
            let method = request["method"].as_str().unwrap();
            if id.is_none() {
                if method == "notifications/initialized" && initialized {
                    ready = true;
                }
                // Disconnection/cancellation does not terminate independently running VM operations.
                continue;
            }
            let id = id.unwrap();
            let response = if method == "initialize" {
                if initialized {
                    rpc_error(id, -32600, "Already initialized")
                } else if !request["params"]["protocolVersion"].is_string()
                    || !request["params"]["capabilities"].is_object()
                    || !request["params"]["clientInfo"]["name"].is_string()
                    || !request["params"]["clientInfo"]["version"].is_string()
                {
                    rpc_error(
                        id,
                        -32602,
                        "Initialization requires protocolVersion, capabilities and clientInfo",
                    )
                } else {
                    initialized = true;
                    json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":VERSION,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"veynode-vm","version":env!("CARGO_PKG_VERSION")},"instructions":"Local trusted-user daemon. Observe is default; lifecycle access requires server --allow-lifecycle. Command execution requires --allow-execute and enabled Linux Guest async jobs; macOS legacy execution is not exposed by this MCP API. File transfer independently requires --allow-files and an enabled Guest files root: a dedicated Linux files account, or macOS --allow-files --files-root with a private directory owned by the service UID. macOS files are not isolated from other same-UID programs. Query vm_guest_info for actual files capability and file_policy; Guest denial is an error, not permission to use another path. Screenshot observation requires QEMU capture or macOS Guest --allow-observe with existing Guest Screen Recording permission. vm_windows uses macOS Guest observation only and returns no titles or input authority; other backends are unsupported. Physical desktop input always requires server --allow-input. Its backend must provide QEMU persistent capture, or macOS Guest --allow-observe --allow-input with existing Guest Screen Recording and Accessibility permissions. Apple Linux desktop input is unsupported. Query vm_guest_info for macOS permission availability and supported physical keys; Unicode text and keyboard-layout translation are unsupported. Input requires a matching screenshot younger than 5 seconds and current control. Acquire control before input or command execution; leases last 60 seconds and must be renewed. Backend input acknowledgement does not prove application acceptance. VM start returns before guest readiness."}})
                }
            } else if method == "ping" {
                json!({"jsonrpc":"2.0","id":id,"result":{}})
            } else if !ready {
                rpc_error(
                    id,
                    -32000,
                    "Initialize and send notifications/initialized first",
                )
            } else if method == "tools/list" {
                json!({"jsonrpc":"2.0","id":id,"result":tools_with_input(lifecycle,execute,files,input)})
            } else if method == "tools/call" {
                let name = request["params"]["name"].as_str().unwrap_or("");
                let known = tools_with_input(lifecycle, execute, files, input)["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["name"] == name);
                if !known {
                    rpc_error(id, -32602, "Tool unknown or permission not granted")
                } else {
                    let mut args = request["params"]
                        .get("arguments")
                        .cloned()
                        .unwrap_or(json!({}));
                    let schema = tools_with_input(lifecycle, execute, files, input)["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|t| t["name"] == name)
                        .unwrap()["inputSchema"]
                        .clone();
                    let valid = args
                        .as_object()
                        .is_some_and(|a| a.keys().all(|k| schema["properties"].get(k).is_some()))
                        && schema["required"].as_array().is_none_or(|r| {
                            r.iter().all(|k| args.get(k.as_str().unwrap()).is_some())
                        });
                    if !valid {
                        rpc_error(id, -32602, "Invalid tool arguments")
                    } else {
                        let vm_id = args["id"].as_str().unwrap_or("").to_owned();
                        if name.starts_with("vm_file_") {
                            args["owner"] = json!(owner);
                        }
                        if [
                            "vm_acquire_control",
                            "vm_release_control",
                            "vm_exec",
                            "vm_exec_start",
                            "vm_exec_poll",
                            "vm_exec_cancel",
                        ]
                        .contains(&name)
                            || name.starts_with("vm_input_")
                        {
                            args["owner"] = json!(owner);
                            args["epoch"] = json!(epochs.get(&vm_id).copied().unwrap_or(0));
                        }
                        jobs::prune_terminal(
                            &mut job_epochs,
                            std::time::Instant::now(),
                            |(_, created, terminal)| (*created, *terminal),
                        );
                        let requested_job_id = args["job_id"].as_str().unwrap_or("").to_owned();
                        if ["vm_exec_poll", "vm_exec_cancel"].contains(&name) {
                            let job_id = args["job_id"].as_str().unwrap_or("").to_owned();
                            args["epoch"] = json!(job_epochs
                                .get(&(vm_id.clone(), job_id))
                                .map(|(epoch, _, _)| *epoch)
                                .unwrap_or(0));
                        }
                        let job_epoch = args["epoch"].as_u64().unwrap_or(0);
                        let result = if name == "vm_exec_start"
                            && !jobs::make_room(&mut job_epochs, 0, |(_, created, finished)| {
                                (*created, *finished)
                            }) {
                            Err(anyhow::anyhow!(
                                "MCP session job capacity reached; unresolved jobs require terminal proof"
                            ))
                        } else {
                            call(dir, name.strip_prefix("vm_").unwrap(), args)
                        };
                        if name == "vm_exec_start" {
                            if let Ok(value) = &result {
                                if let Some(job_id) = value["result"]["job_id"].as_str() {
                                    job_epochs.insert(
                                        (vm_id.clone(), job_id.into()),
                                        (job_epoch, std::time::Instant::now(), false),
                                    );
                                }
                            }
                        }
                        if name == "vm_exec_poll"
                            && result.as_ref().is_ok_and(|value| {
                                matches!(completed_exec_result(value), Ok(Some(_)))
                            })
                        {
                            if let Some((_, _, finished)) =
                                job_epochs.get_mut(&(vm_id.clone(), requested_job_id))
                            {
                                *finished = true;
                            }
                        }

                        if name == "vm_acquire_control" {
                            if let Ok(value) = &result {
                                if let Some(epoch) = value["epoch"].as_u64() {
                                    epochs.insert(vm_id.clone(), epoch);
                                }
                            }
                        }
                        if name == "vm_release_control" && result.is_ok() {
                            epochs.remove(&vm_id);
                        }
                        let content = match result {
                            Ok(v) if name == "vm_screenshot" => match screenshot_content(v) {
                                Ok(content) => content,
                                Err(error) => {
                                    json!({"content":[{"type":"text","text":error.to_string()}],"isError":true})
                                }
                            },
                            Ok(v) => {
                                json!({"content":[{"type":"text","text":serde_json::to_string(&v)?}],"isError":false})
                            }
                            Err(e) => {
                                json!({"content":[{"type":"text","text":e.to_string()}],"isError":true})
                            }
                        };
                        json!({"jsonrpc":"2.0","id":id,"result":content})
                    }
                }
            } else {
                rpc_error(id, -32601, "Method not found")
            };
            write_json(&mut writer, &response)?;
        }
        Ok(())
    })();
    let cleanup_result = cleanup.state.finish();
    protocol_result.and(cleanup_result)
}
#[cfg(test)]
mod input_retirement_tests;
#[cfg(test)]
mod mcp_cleanup_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stopped_owned_worker_is_not_reaped_and_can_continue_to_real_exit() {
        struct Owned(Running);
        impl Drop for Owned {
            fn drop(&mut self) {
                let _ = self.0.force_stop();
            }
        }
        let mut child=Command::new("python3").args(["-c","import os,signal,sys; os.kill(os.getpid(),signal.SIGSTOP); sys.stdin.readline(); sys.exit(7)"])
            .process_group(0).stdin(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut owned = Owned(Running {
            bridge: Arc::new(Mutex::new(bridge::State::new().unwrap())),
            child,
            stdin,
            assigned_memory_mib: 0,
            expected_exit: false,
            execution_quiescing: false,
            exit_status: None,
            disk_identity: None,
            state: Arc::new(Mutex::new(json!({}))),
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            next: 1,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe {
                    libc::waitid(
                        libc::P_PID,
                        owned.0.child.id() as libc::id_t,
                        &mut info,
                        libc::WSTOPPED | libc::WNOHANG | libc::WNOWAIT,
                    )
                },
                0
            );
            if info.si_code == libc::CLD_STOPPED {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "owned fixture did not stop"
            );
            thread::sleep(Duration::from_millis(5));
        }
        for _ in 0..3 {
            assert!(
                owned.0.poll_exit().unwrap().is_none(),
                "stopped worker must remain alive"
            );
        }
        assert_eq!(
            unsafe { libc::kill(owned.0.child.id() as i32, libc::SIGCONT) },
            0
        );
        owned.0.stdin.write_all(b"continue\n").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = owned.0.poll_exit().unwrap() {
                assert_eq!(status.code(), Some(7));
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "owned fixture did not exit"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn qemu_input_cleanup_and_fence_acknowledgements_cannot_be_empty_or_negative() {
        for result in [
            json!({}),
            json!({"released":false}),
            json!({"released":true}),
        ] {
            assert!(normalize_backend_input(result, "input_cleanup", None, false).is_err());
        }
        let valid = json!({"released":true,"synthetic_release_dispatched":true});
        assert_eq!(
            normalize_backend_input(valid.clone(), "input_cleanup", None, false).unwrap(),
            valid
        );
        let valid = json!({"released":true,"synthetic_release_dispatched":true,"through_epoch":7});
        assert!(normalize_backend_input(valid.clone(), "input_revoke", Some(6), false).is_err());
        assert_eq!(
            normalize_backend_input(valid.clone(), "input_revoke", Some(7), false).unwrap(),
            valid
        );
    }
    #[test]
    fn mac_guest_input_acknowledgements_are_explicit() {
        for result in [
            json!({}),
            json!({"dispatched":true}),
            json!({"dispatched":false,"guest_delivery_confirmed":false}),
        ] {
            assert!(normalize_guest_input(json!({"result":result}), "input_step", None).is_err());
        }
        let step = json!({"dispatched":true,"guest_delivery_confirmed":false});
        assert_eq!(
            normalize_guest_input(json!({"result":step}), "input_step", None).unwrap(),
            step
        );
        assert!(
            normalize_guest_input(json!({"result":{"released":false}}), "input_cleanup", None)
                .is_ok()
        );
        assert!(
            normalize_guest_input(json!({"result":{"released":true}}), "input_cleanup", None)
                .is_err()
        );
        let fenced = json!({"result":{"released":true,"synthetic_release_dispatched":true,"through_epoch":3}});
        assert!(normalize_guest_input(fenced.clone(), "input_revoke", Some(3)).is_ok());
        assert!(normalize_guest_input(fenced.clone(), "input_revoke", Some(4)).is_err());
        assert!(normalize_guest_input(fenced, "input_revoke", None).is_err());
    }
    #[test]
    fn mac_input_routes_through_guest_transport_and_preserves_guest_denial() {
        // This is a pipe/protocol fixture, not VM acceptance. The real child is
        // only cat: inspect the exact request and supply a controlled Guest reply.
        let temp = tempfile::tempdir().unwrap();
        let mut manager = Manager::load(
            temp.path().into(),
            "/unused".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        manager.configs.insert(
            "mac".into(),
            serde_json::from_value(json!({"id":"mac","guest":"macos","cpus":1,"memory_mib":512}))
                .unwrap(),
        );
        let mut child = Command::new("/bin/cat")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        manager.running.insert(
            "mac".into(),
            Running {
                bridge: Arc::new(Mutex::new(bridge::State::new().unwrap())),
                child,
                stdin,
                pending: pending.clone(),
                assigned_memory_mib: 512,
                expected_exit: false,
                execution_quiescing: false,
                exit_status: None,
                disk_identity: None,
                state: Arc::new(Mutex::new(json!({"state":"running"}))),
                next: 1,
            },
        );
        let owner = "a".repeat(64);
        let now = std::time::Instant::now();
        let epoch = manager
            .leases
            .lock()
            .unwrap()
            .acquire("mac", &owner, now)
            .unwrap();
        let args = json!({"id":"mac","owner":owner,"epoch":epoch,"operation_id":"b".repeat(32),"begin":true,"events":[]});
        let mut completion = None;
        let dispatched = manager.dispatch(
            json!({"method":"input_step","arguments":args}),
            &mut completion,
        );
        // Reap our fixture before assertions so even a protocol assertion fails cleanly.
        let mut descriptor = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, 1000) };
        unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
        let wire = if ready > 0 {
            read_frame(&mut BufReader::new(stdout))
        } else {
            Err(anyhow::anyhow!("fixture output deadline"))
        };
        manager
            .running
            .get_mut("mac")
            .unwrap()
            .force_stop()
            .unwrap();
        dispatched.unwrap();
        let wire: Value = serde_json::from_slice(&wire.unwrap().unwrap()).unwrap();
        assert_eq!(wire["method"], "guest_request");
        assert_eq!(wire["request"]["method"], "input_step");
        assert_eq!(wire["request"]["arguments"], args);
        assert!(!manager
            .input_retirements
            .lock()
            .unwrap()
            .records(None, Some("mac"))
            .is_empty());
        pending.lock().unwrap().remove(&1).unwrap().send(json!({"id":1,"result":{"id":"1","error":{"code":"accessibility_permission_required","message":"denied"}}})).unwrap();
        let (peer, _) = UnixStream::pair().unwrap();
        assert!(completion
            .unwrap()
            .wait(&peer)
            .unwrap_err()
            .to_string()
            .contains("accessibility_permission_required"));
        assert!(
            !manager
                .input_retirements
                .lock()
                .unwrap()
                .records(None, Some("mac"))
                .is_empty(),
            "permission denial must not erase attempted input cleanup tracking"
        );
        assert!(
            manager
                .input_revocations(None, Some("mac"))
                .unwrap()
                .is_empty(),
            "the explicitly reaped fixture needs no Guest cleanup RPC"
        );
        let mut after = None;
        manager
            .leases
            .lock()
            .unwrap()
            .human("mac", true, now)
            .unwrap();
        assert!(manager
            .dispatch(json!({"method":"input_step","arguments":args}), &mut after)
            .unwrap_err()
            .to_string()
            .contains("revoked"));
    }
    #[test]
    fn backend_attempt_retirement_fences_before_ack_for_eof_release_and_takeover() {
        // State-machine test, not Guest/TCC acceptance. The preceding real pipe
        // unit verifies that a denied reply retains the exact input cleanup tuple.
        for (backend, guest) in [("apple-virtualization", "macos"), ("qemu-hvf", "linux")] {
            for method in ["session_end", "release_control", "human_takeover"] {
                let temp = tempfile::tempdir().unwrap();
                let mut manager = Manager::load(
                    temp.path().into(),
                    PathBuf::from("/unused"),
                    MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
                )
                .unwrap();
                let id = "mac";
                manager.configs.insert(
                    id.into(),
                    serde_json::from_value(
                        json!({"id":id,"backend":backend,"guest":guest,"cpus":1,"memory_mib":512}),
                    )
                    .unwrap(),
                );
                let owner = "a".repeat(64);
                let now = std::time::Instant::now();
                let epoch = manager
                    .leases
                    .lock()
                    .unwrap()
                    .acquire(id, &owner, now)
                    .unwrap();
                assert!(
                    manager
                        .input_revocations(None, Some(id))
                        .unwrap()
                        .is_empty(),
                    "observe-only VM needs no input cleanup"
                );
                let attempted = InputRetirement {
                    owner: owner.clone(),
                    vm: id.into(),
                    epoch,
                    worker_instance: "c".repeat(64),
                };
                manager
                    .input_retirements
                    .lock()
                    .unwrap()
                    .record(attempted.clone())
                    .unwrap();
                let mut completion = None;
                manager
                    .dispatch(
                        json!({"method":method,"arguments":{"id":id,"owner":owner,"epoch":epoch}}),
                        &mut completion,
                    )
                    .unwrap();
                assert!(
                    manager
                        .leases
                        .lock()
                        .unwrap()
                        .check(id, &owner, epoch, now)
                        .is_err(),
                    "authority must be fenced before waiting for Guest acknowledgement"
                );
                match completion.unwrap() {
                    Completion::CancelJobs {
                        input_revocations,
                        response,
                        ..
                    } => {
                        assert_eq!(input_revocations, vec![attempted]);
                        assert!(
                            response.get("input_release").is_none(),
                            "no release attestation before Guest reply"
                        );
                    }
                    _ => panic!("missing deferred cleanup"),
                }
                if method != "release_control" {
                    assert!(manager
                        .leases
                        .lock()
                        .unwrap()
                        .acquire(id, &owner, now)
                        .is_err());
                }
            }
        }
    }
    #[test]
    fn apple_guest_capabilities_do_not_conflate_screenshot_and_async_execution() {
        let mac = backend::guest_capabilities(Backend::AppleVirtualization, "macos");
        assert_eq!(mac["screenshot"], true);
        assert_eq!(mac["input"], true);
        assert_eq!(mac["command_execution"], false);
        assert_eq!(mac["async_commands"], false);
        assert!(mac["guest_integration"].as_str().unwrap().contains("macos"));
        let linux = backend::guest_capabilities(Backend::AppleVirtualization, "linux");
        assert_eq!(linux["screenshot"], false);
        assert_eq!(linux["input"], false);
        assert_eq!(linux["command_execution"], true);
        assert_eq!(linux["async_commands"], true);
    }
    #[test]
    fn guest_stream_bridge_is_strict_local_macos_only() {
        let start = json!({"id":"mac","socket_path":"/private/owner/stream.sock"});
        assert!(validate_local_stream_bridge(
            "start_guest_stream_bridge",
            &start,
            Backend::AppleVirtualization,
            "macos"
        )
        .is_ok());
        assert!(validate_local_stream_bridge(
            "start_guest_stream_bridge",
            &start,
            Backend::QemuHvf,
            "macos"
        )
        .is_err());
        assert!(validate_local_stream_bridge(
            "start_guest_stream_bridge",
            &start,
            Backend::AppleVirtualization,
            "linux"
        )
        .is_err());
        assert!(validate_local_stream_bridge(
            "stop_guest_stream_bridge",
            &start,
            Backend::AppleVirtualization,
            "macos"
        )
        .is_err());
        assert!(validate_local_stream_bridge(
            "start_guest_stream_bridge",
            &json!({"id":"mac","socket_path":"relative"}),
            Backend::AppleVirtualization,
            "macos"
        )
        .is_err());
        assert!(!tools_with_input(true, true, true, true)
            .to_string()
            .contains("guest_stream_bridge"));
    }
    #[test]
    fn screenshots_preserve_real_mime_and_reject_malformed_images() {
        use base64::Engine;
        for (mime, bytes) in [
            ("image/png", b"\x89PNG\r\n\x1a\n".as_slice()),
            ("image/jpeg", [0xff, 0xd8, 0xff, 0xd9].as_slice()),
        ] {
            let value = json!({"mime_type":mime,"data_base64":base64::engine::general_purpose::STANDARD.encode(bytes),"byte_length":bytes.len(),"width":1280,"height":800,"frame_id":"00000000-0000-0000-0000-000000000000"});
            let normalized = normalize_screenshot(json!({"id":"1","result":value}), true).unwrap();
            let content = screenshot_content(normalized).unwrap();
            assert_eq!(content["content"][0]["mimeType"], mime);
            let mut bad = value.clone();
            bad["mime_type"] = json!("image/svg+xml");
            assert!(normalize_screenshot(bad, false).is_err());
            let mut bad = value.clone();
            bad["byte_length"] = json!(1);
            assert!(normalize_screenshot(bad, false).is_err());
            let mut bad = value;
            bad["data_base64"] = json!("not base64");
            assert!(normalize_screenshot(bad, false).is_err());
        }
    }
    include!("file_routes_tests.rs");
    include!("job_retention_tests.rs");
    include!("exec_scope_tests.rs");
    #[test]
    fn synchronous_terminal_result_preserves_exit_and_cancellation() {
        assert!(
            completed_exec_result(&json!({"result":{"state":"running"}}))
                .unwrap()
                .is_none()
        );
        let result =
            json!({"exit_code":-1,"stdout":"你好","stderr":"","cancelled":true,"timed_out":false});
        let reply = completed_exec_result(
            &json!({"id":"42","result":{"state":"finished","result":result}}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(reply, json!({"id":"42","result":result}));
        assert!(completed_exec_result(
            &json!({"result":{"state":"finished","error":{"code":"internal"}}})
        )
        .is_err());
        assert!(completed_exec_result(&json!({"result":{"state":"finished"}})).is_err());
        assert!(completed_exec_result(&json!({"result":{"state":"unknown"}})).is_err());
    }
    #[test]
    fn unix_peer_disconnect_is_detected_without_consuming_data() {
        let (local, mut remote) = UnixStream::pair().unwrap();
        remote.write_all(b"pending bytes").unwrap();
        assert!(!peer_disconnected(&[local.as_raw_fd()]));
        drop(remote);
        // Kernel readiness is asynchronous; concurrent process creation may also
        // briefly retain a CLOEXEC descriptor until exec. Do not consume bytes.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !peer_disconnected(&[local.as_raw_fd()]) {
            assert!(
                std::time::Instant::now() < deadline,
                "peer EOF readiness deadline"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let mut bytes = String::new();
        (&local).read_to_string(&mut bytes).unwrap();
        assert_eq!(bytes, "pending bytes");
    }
    #[test]
    fn shutdown_rejects_unsupported_mode() {
        assert!(shutdown_mode(&json!({"mode":"force"})).is_err());
        assert_eq!(shutdown_mode(&json!({})).unwrap(), "backend");
        assert_eq!(shutdown_mode(&json!({"mode":"guest"})).unwrap(), "guest");
    }
    #[test]
    fn shutdown_rejects_nonstring_mode() {
        for value in [Value::Null, json!(false), json!(12), json!(["guest"])] {
            assert!(shutdown_mode(&json!({"mode":value})).is_err());
        }
    }
    #[test]
    fn frame_bounds() {
        assert!(read_frame(&mut &b"{}\n"[..]).unwrap().is_some());
        assert!(read_frame(&mut &b"{}"[..]).is_err());
        assert!(read_frame(&mut vec![b'a'; MAX_MESSAGE + 1].as_slice()).is_err());
    }
    #[test]
    fn ids_reject_traversal() {
        for id in ["../x", "", "a/b", "x.y", "é"] {
            assert!(validate_id(id).is_err())
        }
        assert!(validate_id("linux-01_test").is_ok());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_zombie_only_group_eperm_is_reaped_without_hiding_live_group_errors() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            },
            0
        );
        assert_eq!(
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        assert!(only_waitable_leader(Some(libc::EPERM), child.id()));
        assert!(!only_waitable_leader(Some(libc::EACCES), child.id()));
        assert!(!only_waitable_leader(
            Some(libc::EPERM),
            unsafe { libc::getpgrp() } as u32
        ));
        let stdin = child.stdin.take().unwrap();
        let mut run = Running {
            bridge: Arc::new(Mutex::new(bridge::State::new().unwrap())),
            child,
            assigned_memory_mib: 0,
            expected_exit: false,
            execution_quiescing: false,
            exit_status: None,
            disk_identity: None,
            stdin,
            state: Arc::new(Mutex::new(json!({}))),
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            next: 1,
        };
        assert!(run.poll_exit().unwrap().unwrap().success());
        assert!(run.poll_exit().unwrap().unwrap().success());
    }
    #[test]
    fn qemu_window_creation_requires_verified_registered_display_capability() {
        let temp = tempfile::tempdir().unwrap();
        let kernel = temp.path().join("kernel");
        fs::write(&kernel, b"protocol fixture").unwrap();
        let mut manager = Manager::load(
            temp.path().into(),
            "/unused".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        manager.qemu = Some(QemuRegistration {
            worker: kernel.clone(),
            launcher: kernel.clone(),
            library: kernel.clone(),
            resources: temp.path().into(),
            spice_library: None,
            screenshot_helper: None,
            console_broker: Some(kernel.clone()),
            console_helper: Some(kernel.clone()),
            venus_runtime: None,
        });
        let request = json!({"method":"create","arguments":{"backend":"qemu-hvf","id":"window-test","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel,"window":true}});
        assert!(manager.dispatch(request.clone(), &mut None).is_err());
        assert!(!temp.path().join("registry.json").exists());
        manager.qemu_window = json!({"native_window":true,"native_window_protocol":1,"display_only":true,"input_enabled":false});
        let result = manager.dispatch(request, &mut None).unwrap();
        assert_eq!(result["capabilities"]["native_window"], true);
        assert_eq!(
            result["capabilities"]["native_window_details"]["input_enabled"],
            false
        );
        assert!(
            manager.running.is_empty(),
            "capability fixture does not start a VM"
        );
    }
    #[test]
    fn backend_selection_is_explicit_and_unsupported_qemu_features_reject() {
        let temp = tempfile::tempdir().unwrap();
        let kernel = temp.path().join("Image");
        fs::write(&kernel, b"fixture").unwrap();
        let value = json!({"id":"test","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel});
        let apple: Config = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(apple.backend, Backend::AppleVirtualization);
        let mut value = value;
        value["backend"] = json!("qemu-hvf");
        let qemu: Config = serde_json::from_value(value.clone()).unwrap();
        assert!(qemu.validate().is_ok());
        let mut window_config = value.clone();
        window_config["window"] = json!(true);
        assert!(serde_json::from_value::<Config>(window_config)
            .unwrap()
            .validate()
            .is_ok());
        let disk = temp.path().canonicalize().unwrap().join("disk.raw");
        fs::write(&disk, vec![0u8; 4096]).unwrap();
        let mut disk_config = value.clone();
        disk_config["disk"] = json!(disk);
        assert!(serde_json::from_value::<Config>(disk_config)
            .unwrap()
            .validate()
            .is_ok());
        // Syntax can request a window; actual create/start must separately have
        // a registered helper with a validated display-only capability.
        assert!(!backend::window_supported(&Value::Null));
        for (field, unsupported) in [
            ("disk", json!(kernel)),
            ("shared_dir", json!(temp.path())),
            ("width", json!(1280)),
        ] {
            let mut bad = value.clone();
            bad[field] = unsupported;
            assert!(
                serde_json::from_value::<Config>(bad)
                    .unwrap()
                    .validate()
                    .is_err(),
                "{field}"
            );
        }
        value["backend"] = json!("qemu-software");
        assert!(serde_json::from_value::<Config>(value).is_err());
    }
    #[test]
    fn queued_or_disconnected_acquire_never_mutates_lease_but_cleanup_still_runs() {
        fn await_disconnection(server: &UnixStream) {
            // Other parallel tests spawn real processes. Between fork and exec,
            // their child can briefly retain our CLOEXEC test-client socket.
            // Establish actual EOF before asserting disconnected-client behavior.
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while !peer_disconnected(&[server.as_raw_fd()]) && std::time::Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(1));
            }
            assert!(
                peer_disconnected(&[server.as_raw_fd()]),
                "test client EOF deadline"
            );
        }
        let temp = tempfile::tempdir().unwrap();
        let mut manager = Manager::load(
            temp.path().into(),
            "/unused/apple".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        manager.configs.insert(
            "test".into(),
            serde_json::from_value(json!({"id":"test","guest":"linux","cpus":1,"memory_mib":512}))
                .unwrap(),
        );
        let manager = Arc::new(Mutex::new(manager));
        let owner = "ab".repeat(32);
        for stale in [false, true] {
            let (mut client, server) = UnixStream::pair().unwrap();
            write_json(
                &mut client,
                &json!({"method":"acquire_control","arguments":{"id":"test","owner":owner}}),
            )
            .unwrap();
            let admitted =
                std::time::Instant::now() - Duration::from_secs(if stale { 31 } else { 0 });
            if stale {
                handle_connection(server, manager.clone(), admitted).unwrap();
                let reply = read_frame(&mut BufReader::new(client)).unwrap().unwrap();
                assert!(String::from_utf8(reply)
                    .unwrap()
                    .contains("expired before dispatch"));
            } else {
                drop(client);
                await_disconnection(&server);
                assert!(handle_connection(server, manager.clone(), admitted).is_err());
            }
            assert!(manager
                .lock()
                .unwrap()
                .leases
                .lock()
                .unwrap()
                .check("test", &owner, 1, std::time::Instant::now())
                .is_err());
        }
        // Cleanup is deliberately processed despite caller disconnect: no late acquire
        // can then resurrect the retired owner, and a fresh owner remains usable.
        let (mut client, server) = UnixStream::pair().unwrap();
        write_json(
            &mut client,
            &json!({"method":"session_end","arguments":{"owner":owner}}),
        )
        .unwrap();
        drop(client);
        await_disconnection(&server);
        let cleanup = handle_connection(server, manager.clone(), std::time::Instant::now());
        assert!(cleanup.is_err()); // Response write fails, but cleanup is already committed.
        let manager = manager.lock().unwrap();
        let mut leases = manager.leases.lock().unwrap();
        assert!(leases
            .acquire("test", &owner, std::time::Instant::now())
            .is_err());
        assert!(leases
            .acquire("test", &"cd".repeat(32), std::time::Instant::now())
            .is_ok());
    }
    #[test]
    fn qemu_observer_rejects_mutation_before_retaining_cleanup_routes() {
        let mut fixture = FilePipeFixture::macos();
        fixture.manager.configs.get_mut("mac").unwrap().backend = Backend::QemuHvf;
        for method in [
            "file_upload_begin",
            "file_session_end",
            "exec_start",
            "exec_poll",
            "exec_cancel",
            "exec",
        ] {
            let mut deferred = None;
            let error=fixture.manager.dispatch(json!({"method":method,"arguments":{"id":"mac","owner":"a".repeat(64),"epoch":1}}),&mut deferred).unwrap_err();
            assert!(error.to_string().contains("observer-only"), "{error}");
            assert!(deferred.is_none());
            assert!(fixture.manager.file_sessions.is_empty());
        }
        let mut deferred = None;
        let result = fixture
            .manager
            .dispatch(
                json!({"method":"session_end","arguments":{"owner":"a".repeat(64)}}),
                &mut deferred,
            )
            .unwrap();
        assert!(result.is_null());
        let (peer, _other) = UnixStream::pair().unwrap();
        let reply = deferred.unwrap().wait(&peer).unwrap();
        assert_eq!(reply["closed"], true);
        assert_eq!(reply["file_cancellation"]["attempted"], 0);
    }
    #[test]
    fn unregistered_qemu_cannot_be_created() {
        let temp = tempfile::tempdir().unwrap();
        let kernel = temp.path().join("Image");
        fs::write(&kernel, b"fixture").unwrap();
        let mut manager = Manager::load(
            temp.path().into(),
            "/unused/apple".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        let mut deferred = None;
        let error=manager.dispatch(json!({"method":"create","arguments":{"id":"test","backend":"qemu-hvf","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel}}),&mut deferred).unwrap_err();
        assert!(error.to_string().contains("not registered"));
        assert!(!temp.path().join("registry.json").exists());
    }
    #[test]
    fn input_permission_is_independent() {
        let only = tools_with_input(false, false, false, true).to_string();
        assert!(only.contains("vm_input_click") && only.contains("vm_acquire_control"));
        assert!(
            !only.contains("vm_exec")
                && !only.contains("vm_file_")
                && !only.contains("vm_force_stop")
        );
        assert!(!tools_with_input(true, true, true, false)
            .to_string()
            .contains("vm_input_"));
    }
    #[test]
    fn tool_permissions() {
        let list = tools_list(false, false);
        assert_eq!(list["tools"].as_array().unwrap().len(), 6);
        assert!(list.to_string().contains("vm_screenshot"));
        assert!(!list.to_string().contains("vm_start"));
        assert!(tools_list(true, false).to_string().contains("vm_start"));
        assert!(!tools_list(true, true)
            .to_string()
            .contains("vm_file_upload_begin"));
        let files = tools_with_files(false, false, true).to_string();
        assert!(files.contains("vm_file_upload_begin") && files.contains("vm_file_download_read"));
        assert!(!files.contains("vm_exec") && !files.contains("vm_acquire_control"));
    }

    // Real child pipes exercise transport framing only; cat is not a Guest and
    // these controlled replies do not establish macOS file-service acceptance.
    struct FilePipeFixture {
        manager: Manager,
        output: BufReader<std::process::ChildStdout>,
        _directory: tempfile::TempDir,
    }
    impl FilePipeFixture {
        fn macos() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let mut manager = Manager::load(
                directory.path().into(),
                "/unused".into(),
                MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
            )
            .unwrap();
            manager.configs.insert(
                "mac".into(),
                serde_json::from_value(
                    json!({"id":"mac","guest":"macos","cpus":1,"memory_mib":512}),
                )
                .unwrap(),
            );
            let mut child = Command::new("/bin/cat")
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let stdin = child.stdin.take().unwrap();
            let output = child.stdout.take().unwrap();
            assert!(
                unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } >= 0
            );
            manager.running.insert(
                "mac".into(),
                Running {
                    bridge: Arc::new(Mutex::new(bridge::State::new().unwrap())),
                    child,
                    stdin,
                    pending: Arc::new(Mutex::new(BTreeMap::new())),
                    assigned_memory_mib: 512,
                    expected_exit: false,
                    execution_quiescing: false,
                    exit_status: None,
                    disk_identity: None,
                    state: Arc::new(Mutex::new(json!({"state":"running"}))),
                    next: 1,
                },
            );
            Self {
                manager,
                output: BufReader::new(output),
                _directory: directory,
            }
        }
        fn forward(&mut self, method: &str, args: Value, reply: Value) -> (Value, Result<Value>) {
            let mut completion = None;
            self.manager
                .dispatch(json!({"method":method,"arguments":args}), &mut completion)
                .unwrap();
            // serde_json::to_writer emits separate writes; a readable pipe may
            // contain only a prefix. Retain it across EAGAIN under one deadline.
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            let mut bytes = Vec::new();
            while bytes.last() != Some(&b'\n') {
                assert!(
                    std::time::Instant::now() < deadline && bytes.len() <= MAX_MESSAGE,
                    "fixture transport deadline or size bound"
                );
                let mut byte = [0];
                match self.output.read(&mut byte) {
                    Ok(1) => bytes.push(byte[0]),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        let mut descriptor = libc::pollfd {
                            fd: self.output.get_ref().as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        assert!(unsafe { libc::poll(&mut descriptor, 1, 20) } >= 0);
                    }
                    other => panic!("fixture pipe ended unexpectedly: {other:?}"),
                }
            }
            let wire: Value = serde_json::from_slice(
                &read_frame(&mut std::io::Cursor::new(bytes))
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            let number = wire["id"].as_u64().unwrap();
            self.manager.running["mac"]
                .pending
                .lock()
                .unwrap()
                .remove(&number)
                .unwrap()
                .send(json!({"id":number,"result":reply}))
                .unwrap();
            let (peer, _other) = UnixStream::pair().unwrap();
            (wire, completion.unwrap().wait(&peer))
        }
    }
    impl Drop for FilePipeFixture {
        fn drop(&mut self) {
            let _ = self.manager.running.get_mut("mac").unwrap().force_stop();
        }
    }
    #[test]
    fn mac_windows_observe_forwards_only_guest_method_and_validates_result() {
        let mut fixture = FilePipeFixture::macos();
        let (wire, result) =
            fixture.forward("windows", json!({"id":"mac"}), windows::tests::fixture());
        assert_eq!(
            wire,
            json!({"id":1,"method":"guest_request","request":{"id":"1","method":"windows"}})
        );
        assert_eq!(result.unwrap()["titles_included"], false);
        assert!(fixture.manager.file_sessions.is_empty());
        let (_, denied) = fixture.forward(
            "windows",
            json!({"id":"mac"}),
            json!({"id":"2","error":{"code":"screen_recording_permission_required"}}),
        );
        assert!(denied
            .unwrap_err()
            .to_string()
            .contains("screen_recording_permission_required"));
        let mut bad = windows::tests::fixture();
        bad["result"]["windows"][0]["title"] = json!("must not surface");
        let (_, bad) = fixture.forward("windows", json!({"id":"mac"}), bad);
        assert!(bad.is_err());
        for (backend, guest, args) in [
            (Backend::AppleVirtualization, "linux", json!({"id":"mac"})),
            (Backend::QemuHvf, "linux", json!({"id":"mac"})),
            (
                Backend::AppleVirtualization,
                "macos",
                json!({"id":"mac","include_titles":true}),
            ),
        ] {
            let config = fixture.manager.configs.get_mut("mac").unwrap();
            config.backend = backend;
            config.guest = guest.into();
            let mut pending = None;
            assert!(fixture
                .manager
                .dispatch(json!({"method":"windows","arguments":args}), &mut pending)
                .is_err());
            assert!(pending.is_none());
        }
        assert!(tools_with_input(false, false, false, false)["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "vm_windows"));
    }

    #[test]
    fn mac_files_forward_flat_owner_bound_wire_and_route_session_cleanup() {
        let mut fixture = FilePipeFixture::macos();
        let owner = "a".repeat(64);
        for (method, fields) in [
            (
                "file_upload_begin",
                json!({"path":"artifact.txt","size":3,"sha256":"b".repeat(64)}),
            ),
            (
                "file_upload_write",
                json!({"token":"c".repeat(32),"offset":0,"data":"YWJj"}),
            ),
            ("file_upload_commit", json!({"token":"c".repeat(32)})),
            ("file_download_begin", json!({"path":"artifact.txt"})),
            (
                "file_download_read",
                json!({"token":"d".repeat(32),"offset":0}),
            ),
            ("file_cancel", json!({"token":"d".repeat(32)})),
        ] {
            let mut args = fields;
            args["id"] = json!("mac");
            args["owner"] = json!(owner);
            let reply = json!({"id":"fixture","result":{"fixture_reply":true}});
            let (wire, result) = fixture.forward(method, args.clone(), reply.clone());
            assert_eq!(wire["method"], "guest_request");
            args["id"] = json!(wire["id"].as_u64().unwrap().to_string());
            args["method"] = json!(method);
            assert_eq!(wire["request"], args);
            assert_eq!(result.unwrap(), reply);
        }
        assert!(fixture.manager.file_sessions[&owner].1.contains("mac"));
        assert!(
            fixture
                .manager
                .leases
                .lock()
                .unwrap()
                .acquire("mac", "b".repeat(64).as_str(), std::time::Instant::now())
                .is_ok(),
            "files do not acquire desktop control"
        );
        let mut completion = None;
        fixture
            .manager
            .dispatch(
                json!({"method":"session_end","arguments":{"owner":owner}}),
                &mut completion,
            )
            .unwrap();
        match completion.unwrap() {
            Completion::CancelJobs { file_sessions, .. } => {
                assert_eq!(file_sessions, vec![json!({"id":"mac","owner":owner})])
            }
            _ => panic!("missing file session cleanup"),
        }
        assert!(!fixture.manager.file_sessions.contains_key(&owner));
        let (wire, result) = fixture.forward(
            "file_session_end",
            json!({"id":"mac","owner":owner}),
            json!({"id":"cleanup","result":{"cancelled":0}}),
        );
        assert_eq!(wire["request"]["method"], "file_session_end");
        assert!(result.is_ok());
        assert!(
            fixture.manager.file_sessions.is_empty(),
            "cleanup must not resurrect routing"
        );
    }
    #[test]
    fn mac_file_availability_is_guest_authority_and_denial_is_not_success() {
        let mut fixture = FilePipeFixture::macos();
        let info = json!({"id":"info","result":{"capabilities":{"files":false}}});
        let (_, result) = fixture.forward("guest_info", json!({"id":"mac"}), info.clone());
        assert_eq!(result.unwrap(), info);
        for code in [
            "unsupported",
            "files_disabled",
            "files_worker_unavailable",
            "session_retired",
            "owner_mismatch",
        ] {
            let owner = "a".repeat(64);
            let (wire, result) = fixture.forward(
                "file_download_begin",
                json!({"id":"mac","owner":owner,"path":"artifact.txt"}),
                json!({"id":"request","error":{"code":code,"message":"denied by Guest"}}),
            );
            assert_eq!(wire["request"]["owner"], owner);
            assert!(result.unwrap_err().to_string().contains(code));
        }
        // Failed requests still need EOF cleanup: a lost/denied reply is not
        // proof that the Guest created no temporary transfer state.
        assert!(fixture.manager.file_sessions.contains_key(&"a".repeat(64)));
    }
    #[test]
    fn mac_file_owner_size_and_backend_guards_fail_before_worker_write() {
        let mut fixture = FilePipeFixture::macos();
        for owner in [
            Value::Null,
            json!("A".repeat(64)),
            json!("a".repeat(63)),
            json!("g".repeat(64)),
        ] {
            let mut completion = None;
            assert!(fixture.manager.dispatch(json!({"method":"file_upload_begin","arguments":{"id":"mac","owner":owner,"path":"x","size":0,"sha256":"b".repeat(64)}}),&mut completion).is_err());
            assert!(completion.is_none());
        }
        assert!(fixture.manager.file_sessions.is_empty());
        let mut completion = None;
        let error=fixture.manager.dispatch(json!({"method":"file_upload_write","arguments":{"id":"mac","owner":"a".repeat(64),"token":"b".repeat(32),"offset":0,"data":"A".repeat(65536)}}),&mut completion).unwrap_err();
        assert!(error.to_string().contains("64KiB"));
        assert!(completion.is_none());
        fixture.manager.configs.get_mut("mac").unwrap().backend = Backend::QemuHvf;
        let error=fixture.manager.dispatch(json!({"method":"file_download_begin","arguments":{"id":"mac","owner":"a".repeat(64),"path":"x"}}),&mut completion).unwrap_err();
        assert!(error.to_string().contains("QEMU/HVF"));
        assert_eq!(
            fixture.manager.running["mac"].next, 2,
            "only bounded rejected payload allocates a request number"
        );
        assert!(fixture.manager.running["mac"]
            .pending
            .lock()
            .unwrap()
            .is_empty());
        let mut descriptor = libc::pollfd {
            fd: fixture.output.get_ref().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut descriptor, 1, 40) },
            0,
            "rejected operation reached worker"
        );
    }
    #[test]
    fn file_tools_keep_independent_permission_and_state_macos_boundary() {
        assert!(!tools_with_input(true, true, false, true)
            .to_string()
            .contains("vm_file_"));
        let tools = tools_with_files(false, false, true);
        let begin = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "vm_file_upload_begin")
            .unwrap();
        let description = begin["description"].as_str().unwrap();
        assert!(
            description.contains("Linux")
                && description.contains("macOS")
                && description.contains("same-user")
        );
        assert!(begin["inputSchema"]["properties"].get("owner").is_none());
        assert_eq!(
            begin["inputSchema"]["properties"]["size"]["maximum"],
            8388608
        );
        assert!(
            !tools.to_string().contains("vm_exec")
                && !tools.to_string().contains("vm_acquire_control")
        );
    }
}

#[cfg(test)]
mod screenshot_limits_tests;
#[cfg(test)]
mod screenshot_test_support;
