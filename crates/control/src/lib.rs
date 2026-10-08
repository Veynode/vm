pub mod backend;
pub mod input;
mod jobs;
mod lease;
mod resources;
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_log: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_dir: Option<PathBuf>,
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.id)?;
        if self.backend == Backend::QemuHvf
            && (self.guest != "linux"
                || self.window
                || self.disk.is_some()
                || self.shared_dir.is_some()
                || self.restore_image.is_some()
                || self.machine_identity_dir.is_some()
                || self.width.is_some()
                || self.height.is_some())
        {
            bail!("unsupported QEMU configuration: this revision supports Linux ARM RAM direct boot with headless SPICE; disk, host sharing, native windows and display resizing are not yet integrated")
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
    let mut out = Vec::new();
    let n = reader
        .take((MAX_MESSAGE + 1) as u64)
        .read_until(b'\n', &mut out)?;
    if n == 0 {
        return Ok(None);
    }
    if n > MAX_MESSAGE || out.last() != Some(&b'\n') {
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
    let allowed = if method == "start_guest_stream_bridge" {
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
    child: Child,
    assigned_memory_mib: u64,
    expected_exit: bool,
    exit_status: Option<std::process::ExitStatus>,
    disk_identity: Option<DiskIdentity>,
    stdin: ChildStdin,
    state: Arc<Mutex<Value>>,
    pending: Arc<Mutex<BTreeMap<u64, mpsc::Sender<Value>>>>,
    next: u64,
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
        if pid == 0 {
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
struct Pending {
    receiver: mpsc::Receiver<Value>,
    number: u64,
    pending: Arc<Mutex<BTreeMap<u64, mpsc::Sender<Value>>>>,
    timeout: Duration,
    post: Option<JobPost>,
}
enum JobPost {
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
    Backend(Pending),
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
    },
}
impl Completion {
    fn wait(self, peer: &UnixStream) -> Result<Value> {
        match self {
            Self::Backend(p) => p.wait(),
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
            } => {
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
                response["note"]=json!("Cancellation is best effort, not rollback. Poll asynchronous jobs or inspect the synchronous command's terminal result to confirm termination.");
                Ok(response)
            }
        }
    }
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
        json!({"dispatched":true,"steps":sent,"guest_delivery_confirmed":false,"note":"QMP acknowledgement is not application acceptance; physical keys use Guest layout."}),
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
                            jobs.lock().unwrap().finish(&vm, &id);
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
    if encoded.len() > (700usize * 1024).div_ceil(3) * 4 {
        bail!("screenshot exceeds 700 KiB")
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("invalid screenshot base64")?;
    if bytes.len() > 700 * 1024 || value["byte_length"].as_u64() != Some(bytes.len() as u64) {
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
fn prune_file_routes(routes: &mut FileSessionRoutes, now: std::time::Instant) {
    routes.retain(|_, (touched, _)| {
        now.saturating_duration_since(*touched) < Duration::from_secs(330)
    });
}
struct Manager {
    memory: MemoryBudget,
    dir: PathBuf,
    backend: PathBuf,
    qemu: Option<QemuRegistration>,
    configs: BTreeMap<String, Config>,
    leases: Arc<Mutex<lease::Leases>>,
    jobs: Arc<Mutex<jobs::Jobs>>,
    file_sessions: FileSessionRoutes,
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
        Ok(Self {
            memory,
            dir,
            backend,
            qemu: None,
            configs,
            leases: Arc::new(Mutex::new(lease::Leases::default())),
            jobs: Arc::new(Mutex::new(jobs::Jobs::default())),
            file_sessions: BTreeMap::new(),
            running: BTreeMap::new(),
        })
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
    fn view(&mut self, id: &str) -> Result<Value> {
        let config = self.configs.get(id).context("VM not found")?;
        let mut result = json!({"config":config,"state":"stopped","capabilities":backend::guest_capabilities(config.backend,&config.guest)});
        if config.backend == Backend::AppleVirtualization && config.guest == "macos" {
            result["capabilities"]["screenshot"] = json!(true);
            result["capabilities"]["screenshot_availability"]=json!("requires running macOS Guest service --allow-observe and Guest Screen Recording permission; query guest_info for current availability");
        }
        if config.backend == Backend::QemuHvf {
            result["capabilities"]["input"] = json!(self
                .qemu
                .as_ref()
                .is_some_and(|q| q.console_broker.is_some()));
            result["capabilities"]["input_scope"] = json!("absolute click/drag/discrete wheel/explicit qcode only; snapshot <=5s, generation and current lease required; no text/layout translation");
            result["capabilities"]["screenshot"] =
                json!(self.qemu.as_ref().is_some_and(|q| q.has_screenshot()));
        }
        if let Some(runtime) = self.running.get_mut(id) {
            result["runtime"] = runtime.state.lock().unwrap().clone();
            if let Some(exit) = runtime.poll_exit()? {
                result["state"] = json!(if exit.success() || runtime.expected_exit {
                    "stopped"
                } else {
                    "crashed"
                });
                result["exit_code"] = json!(exit.code());
            } else {
                result["state"] = result["runtime"]["state"].clone();
                result["pid"] = json!(runtime.child.id());
            }
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
            qemu_capabilities["input"] = json!(self
                .qemu
                .as_ref()
                .is_some_and(|q| q.console_broker.is_some()));
            qemu_capabilities["screenshot"] =
                json!(self.qemu.as_ref().is_some_and(|q| q.has_screenshot()));
            return Ok(
                json!({"resource_admission":{"physical_memory_mib":self.memory.physical_memory_mib,"vm_memory_budget_mib":self.memory.vm_memory_budget_mib,"active_assigned_memory_mib":assigned,"remaining_budget_mib":self.memory.vm_memory_budget_mib.saturating_sub(assigned),"explicitly_configured":self.memory.explicitly_configured,"scope":"assigned guest RAM in this daemon; excludes VM overhead, other daemons and unrelated host workloads; not a host pressure guarantee"},"backends":{"apple-virtualization":backend::capabilities(Backend::AppleVirtualization),"qemu-hvf":{"registered":self.qemu.is_some(),"capabilities":qemu_capabilities}},"backend":"apple-virtualization","guests":["linux","macos"],"windows":false,"lifecycle":true,"native_window":true,"screenshot":true,"screenshot_requirements":"macOS Guest --allow-observe and existing Guest Screen Recording permission, or registered QEMU capture; Apple Linux screenshot unsupported","input":self.qemu.as_ref().is_some_and(|q|q.console_broker.is_some()),"input_requirements":"registered QEMU persistent console, independent input permission, current lease and recent matching screenshot; macOS/Apple Linux input unsupported","command_execution":"MCP requires enabled Linux async Guest jobs; macOS legacy exec is not exposed because async cancellation is unsupported","async_commands":"requires exec_start/exec_poll/exec_cancel support in the enabled guest service; owned jobs are VM/client/original-epoch bound","file_transfer":"requires opt-in Linux non-root files worker; single filenames under configured directory; 8MiB max","control_session_fencing":{"retired_owner_ttl_seconds":120,"request_dispatch_deadline_seconds":30},"remote_desktop":false,"guest_integration":"enabled Linux or macOS Guest service over host-only vsock; query vm_guest_info for actual availability","guest_shutdown":"requires installed Linux integration service explicitly enabled for shutdown; request acceptance is not shutdown completion","gpu":"requires guest measurements; no performance claim"}),
            );
        }
        if method == "create" {
            let config: Config = serde_json::from_value(args.clone())?;
            config.validate()?;
            if config.backend == Backend::QemuHvf {
                backend::require_registration(&self.qemu)?;
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
        if self.configs[id].backend == Backend::QemuHvf
            && (guest_shutdown
                || method.starts_with("exec")
                || method.starts_with("file_")
                || ["guest_info", "show_window", "close_window", "serial_write"].contains(&method))
        {
            bail!("unsupported operation on QEMU/HVF backend: {method}; Guest integration and native window are not connected")
        }
        if ["start_guest_stream_bridge", "stop_guest_stream_bridge"].contains(&method) {
            validate_local_stream_bridge(
                method,
                args,
                self.configs[id].backend,
                &self.configs[id].guest,
            )?;
        }
        let now = std::time::Instant::now();
        if method == "acquire_control" {
            let owner = args["owner"].as_str().context("missing control owner")?;
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
            let epoch = self
                .leases
                .lock()
                .unwrap()
                .human(id, method == "human_takeover", now)?;
            if method == "human_takeover" {
                *deferred = Some(Completion::CancelJobs {
                    dir: self.dir.clone(),
                    file_sessions: Vec::new(),
                    response: json!({"epoch":epoch,"human_control":true}),
                    jobs: self.jobs.lock().unwrap().running(id, now),
                });
                return Ok(Value::Null);
            }
            return Ok(json!({"epoch":epoch,"human_control":false}));
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
        if method.starts_with("input_") {
            if self.configs[id].backend != Backend::QemuHvf
                || self
                    .qemu
                    .as_ref()
                    .is_none_or(|q| q.console_broker.is_none())
            {
                bail!("unsupported: desktop input requires QEMU persistent console")
            }
            if method != "input_cleanup" {
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
            let config = self.configs.get(id).context("VM not found")?.clone();
            config.validate()?;
            if let Some(run) = self.running.get_mut(id) {
                if run.poll_exit()?.is_none() {
                    bail!("VM process already active")
                }
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
            let mut child = command
                .arg("--config")
                .arg(config_path)
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("starting VM backend")?;
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            let state = Arc::new(Mutex::new(json!({"state":"starting"})));
            let pending: Arc<Mutex<BTreeMap<u64, mpsc::Sender<Value>>>> =
                Arc::new(Mutex::new(BTreeMap::new()));
            let output_state = state.clone();
            let output_pending = pending.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_frame(&mut reader) {
                        Ok(Some(bytes)) => match serde_json::from_slice::<Value>(&bytes) {
                            Ok(value) => {
                                if value["event"] == "state" {
                                    *output_state.lock().unwrap() = value.clone();
                                }
                                if let Some(id) = value["id"].as_u64() {
                                    if let Some(tx) = output_pending.lock().unwrap().remove(&id) {
                                        let _ = tx.send(value);
                                    }
                                }
                            }
                            Err(_) => {
                                *output_state.lock().unwrap() =
                                    json!({"state":"error","error":"backend emitted invalid JSON"});
                                break;
                            }
                        },
                        Ok(None) => break,
                        Err(_) => {
                            *output_state.lock().unwrap() = json!({"state":"error","error":"backend output exceeded protocol bound"});
                            break;
                        }
                    }
                }
                output_pending.lock().unwrap().clear();
            });
            self.running.insert(
                id.into(),
                Running {
                    child,
                    assigned_memory_mib: config.memory_mib,
                    expected_exit: false,
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
        if method == "force_stop" {
            if args["confirm"] != id {
                bail!("force stop risks guest data loss; confirm must exactly equal VM id")
            }
            let run = self.running.get_mut(id).context("VM process not active")?;
            run.force_stop()?;
            *run.state.lock().unwrap() = json!({"state":"stopped","reason":"force_stopped"});
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
            if self.configs.get(id).unwrap().guest != "linux" {
                bail!("unsupported: file integration currently Linux-only")
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
                // Guest transfers expire after at most 300 seconds. Do not retain
                // abruptly killed adapters forever in the bounded host routing table.
                prune_file_routes(&mut self.file_sessions, now);
                if !self.file_sessions.contains_key(owner) && self.file_sessions.len() >= 256 {
                    bail!("file session capacity reached")
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
                "serial_write",
                "screenshot",
                "start_guest_stream_bridge",
                "stop_guest_stream_bridge",
                "input_step",
                "input_cleanup",
                "guest_info",
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
            let number = run.next;
            run.next += 1;
            let (tx, rx) = mpsc::channel();
            let mut command = json!({"id":number,"method":method});
            if method == "start_guest_stream_bridge" {
                command["socket_path"] = args["socket_path"].clone();
            }
            if method.starts_with("input_") {
                command["arguments"] = args.clone();
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
            let post = if method == "screenshot" {
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
            run.pending.lock().unwrap().insert(number, tx);
            let sent = write_json(&mut run.stdin, &command);
            if let Err(e) = sent {
                run.pending.lock().unwrap().remove(&number);
                return Err(e);
            }
            *deferred = Some(Completion::Backend(Pending {
                receiver: rx,
                number,
                pending: run.pending.clone(),
                timeout: Duration::from_secs(if method == "screenshot" {
                    17
                } else if method.ends_with("guest_stream_bridge") {
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
    let bytes = read_frame(&mut BufReader::new(socket))?.context("daemon disconnected")?;
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
        json!({"name":"vm_guest_info","description":"Query the real guest integration service over vsock; fails if unavailable.","inputSchema":id}),
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
            ("vm_show_window", "Show native VM console."),
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
        for p in ["width", "height"] {
            properties[p] = json!({"type":"integer","minimum":320,"maximum":8192});
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
            tools.push(json!({"name":format!("vm_{name}"),"description":"Bounded physical QEMU console input. Requires current control and a screenshot observation younger than 5 seconds with matching VM, generation and dimensions. QMP ack does not prove app acceptance; no Unicode/layout translation.","inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}));
        }
    }
    if files {
        for (name, description, extra, required) in [
            ("file_upload_begin", "Begin <=8MiB upload of a single filename in the explicitly configured Linux files directory. Does not overwrite; files account owns published files, execute account has no implicit access.", json!({"path":{"type":"string"},"size":{"type":"integer","minimum":0,"maximum":8388608},"sha256":{"type":"string","pattern":"^[0-9a-f]{64}$"}}),vec!["path","size","sha256"]),
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
    dir: PathBuf,
    owner: String,
    stopped: Arc<AtomicBool>,
}
impl Drop for SessionCleanup {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        cleanup_session(&self.dir, &self.owner);
    }
}
fn cleanup_session(dir: &Path, owner: &str) {
    match call(dir, "session_end", json!({"owner":owner})) {
        Err(_) => eprintln!(
            "vm-control: session cleanup unavailable; lease TTL and Guest deadlines still apply"
        ),
        Ok(result) => {
            for key in ["job_cancellation", "file_cancellation"] {
                let cancellation = &result[key];
                if cancellation["unconfirmed_requests"].as_u64().unwrap_or(0) > 0
                    || cancellation["outcomes"].as_array().is_some_and(|outcomes| {
                        outcomes
                            .iter()
                            .any(|outcome| outcome.get("error").is_some())
                    })
                {
                    eprintln!(
                        "vm-control: some session cleanup requests failed or remain unconfirmed"
                    );
                }
            }
        }
    }
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
    let stopped = Arc::new(AtomicBool::new(false));
    let disconnected = Arc::new(AtomicBool::new(false));
    let _cleanup = SessionCleanup {
        dir: dir.to_path_buf(),
        owner: owner.clone(),
        stopped: stopped.clone(),
    };
    let monitor_dir = dir.to_path_buf();
    let monitor_owner = owner.clone();
    let monitor_disconnected = disconnected.clone();
    thread::spawn(move || {
        while !stopped.load(Ordering::Acquire) {
            if peer_disconnected(&[libc::STDIN_FILENO, libc::STDOUT_FILENO]) {
                monitor_disconnected.store(true, Ordering::Release);
                cleanup_session(&monitor_dir, &monitor_owner);
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
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
                json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":VERSION,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"veynode-vm","version":env!("CARGO_PKG_VERSION")},"instructions":"Local trusted-user daemon. Observe is default; lifecycle access requires server --allow-lifecycle; command execution requires --allow-execute. File transfer requires --allow-files and explicitly configured Linux Guest files root. Screenshot observation requires QEMU capture or opted-in macOS Guest service with existing Guest permission; desktop input requires --allow-input and QEMU persistent capture. Acquire control before commands; leases last 60 seconds and must be renewed. VM start returns before guest readiness."}})
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
                    && schema["required"]
                        .as_array()
                        .is_none_or(|r| r.iter().all(|k| args.get(k.as_str().unwrap()).is_some()));
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
                    job_epochs
                        .retain(|_, (_, created, _)| created.elapsed() < Duration::from_secs(330));
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
                            "MCP session job capacity reached; wait for old jobs to expire"
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
                        && result
                            .as_ref()
                            .is_ok_and(|value| value["result"]["state"] == "finished")
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
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn apple_guest_capabilities_do_not_conflate_screenshot_and_async_execution() {
        let mac = backend::guest_capabilities(Backend::AppleVirtualization, "macos");
        assert_eq!(mac["screenshot"], true);
        assert_eq!(mac["command_execution"], false);
        assert_eq!(mac["async_commands"], false);
        assert!(mac["guest_integration"].as_str().unwrap().contains("macos"));
        let linux = backend::guest_capabilities(Backend::AppleVirtualization, "linux");
        assert_eq!(linux["screenshot"], false);
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
    #[test]
    fn stale_file_cleanup_routes_expire_after_guest_deadlines_but_live_routes_remain() {
        let now = std::time::Instant::now();
        let mut routes = FileSessionRoutes::new();
        routes.insert(
            "killed-adapter".into(),
            (
                now - Duration::from_secs(331),
                BTreeSet::from(["old-vm".into()]),
            ),
        );
        routes.insert(
            "live-adapter".into(),
            (
                now - Duration::from_secs(300),
                BTreeSet::from(["live-vm".into()]),
            ),
        );
        prune_file_routes(&mut routes, now);
        assert!(!routes.contains_key("killed-adapter"));
        assert!(routes["live-adapter"].1.contains("live-vm"));
    }
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
        assert!(peer_disconnected(&[local.as_raw_fd()]));
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
            child,
            assigned_memory_mib: 0,
            expected_exit: false,
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
        for (field, unsupported) in [
            ("window", json!(true)),
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
        assert_eq!(list["tools"].as_array().unwrap().len(), 5);
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
}
