pub mod backend;
pub mod bridge;
pub mod container_tools;
pub mod desktop_input;
pub mod desktop_tools;
pub mod home;
pub mod input;
mod jobs;
mod native_control;
pub mod native_input;
pub mod qemu_disk;
pub mod qemu_extras;
pub mod qemu_graphics;
pub mod qemu_uefi;
mod resources;
pub mod screen_text;
pub mod screen_wait;
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
    /// `kernel` (default): direct kernel boot of `kernel`/`initrd`. `uefi`:
    /// firmware boot of the distribution's own bootloader from `disk` or `cdrom`.
    #[serde(default, skip_serializing_if = "BootMode::is_kernel")]
    pub boot: BootMode,
    /// Per-VM UEFI variable store (boot entries). Created on first start; the
    /// file format is backend specific (QEMU pflash image, VZ variable store).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub efi_vars: Option<PathBuf>,
    /// Read-only installer medium (ISO 9660 image) for UEFI boot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdrom: Option<PathBuf>,
    /// QEMU only: also expose a private QMP monitor socket (published in the VM
    /// state as `monitor_socket`) for scripted installer automation.
    #[serde(default, skip_serializing_if = "is_false")]
    pub qemu_monitor: bool,
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
    /// Additional read-only block devices, attached after `disk` in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_disks: Vec<ExtraDisk>,
    /// Host directories exposed by tag: virtiofs on Apple, 9p on QEMU.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shares: Vec<Share>,
    /// Host listener → Guest port forwards (QEMU user networking only; the
    /// Apple NAT Guest address is reachable from the host directly).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortForward>,
    /// Fixed NIC address (Apple Virtualization), so the host can find the
    /// Guest's DHCP lease.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac_address: Option<String>,
    /// macOS Guest login file ("user\npassword"): the worker serves
    /// screenshots and input through the Guest's Screen Sharing (RFB) server
    /// instead of the in-Guest service, so no Guest TCC grants are needed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_sharing_login: Option<PathBuf>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BootMode {
    #[default]
    Kernel,
    Uefi,
}
impl BootMode {
    pub fn is_kernel(&self) -> bool {
        *self == Self::Kernel
    }
}
/// Largest accepted installer image (DVD images are well below this).
pub const MAX_CDROM_BYTES: u64 = 64 << 30;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtraDisk {
    pub path: PathBuf,
    /// Must be true: writable disks need the cooperating-worker lock that only
    /// the primary `disk` carries.
    pub read_only: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Share {
    pub tag: String,
    pub path: PathBuf,
    #[serde(default)]
    pub read_only: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortForward {
    #[serde(default = "loopback")]
    pub host_address: std::net::Ipv4Addr,
    pub host_port: u16,
    pub guest_port: u16,
    #[serde(default = "tcp")]
    pub protocol: String,
}
fn loopback() -> std::net::Ipv4Addr {
    std::net::Ipv4Addr::LOCALHOST
}
fn tcp() -> String {
    "tcp".into()
}
fn valid_local_mac(mac: &str) -> bool {
    let octets: Vec<_> = mac.split(':').collect();
    octets.len() == 6
        && octets
            .iter()
            .all(|o| o.len() == 2 && o.bytes().all(|b| b.is_ascii_hexdigit()))
        && u8::from_str_radix(octets[0], 16).is_ok_and(|first| first & 0b11 == 0b10)
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
            bail!("unsupported QEMU configuration: this revision supports Linux ARM direct or UEFI boot, optional explicitly raw disk, and SPICE; host sharing and other Guest backends are not integrated")
        }
        if self.qemu_monitor && self.backend != Backend::QemuHvf {
            bail!("qemu_monitor requires QEMU/HVF")
        }
        self.validate_boot()?;
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
        if self.guest == "linux" && self.boot == BootMode::Kernel && self.kernel.is_none() {
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
        if let Some(mac) = &self.mac_address {
            if self.backend != Backend::AppleVirtualization || !valid_local_mac(mac) {
                bail!("mac_address must be a locally administered unicast address (Apple Virtualization)")
            }
        }
        if let Some(login) = &self.screen_sharing_login {
            if self.backend != Backend::AppleVirtualization
                || self.guest != "macos"
                || self.mac_address.is_none()
                || !login.is_absolute()
                || !login.is_file()
            {
                bail!("screen_sharing_login needs an Apple macOS Guest with mac_address and an existing absolute login file")
            }
        }
        if self.extra_disks.len() > 4 || self.shares.len() > 8 || self.ports.len() > 32 {
            bail!("too many extra disks, shares or ports")
        }
        for disk in &self.extra_disks {
            if !disk.read_only {
                bail!("extra disks must be read-only; only the primary disk is writable")
            }
            if !disk.path.is_absolute() || !disk.path.is_file() {
                bail!("extra disk must exist at an absolute path")
            }
        }
        let mut tags = BTreeSet::new();
        for share in &self.shares {
            if share.tag.is_empty()
                || share.tag.len() > 32
                || !share
                    .tag
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
                || share.tag == "veynode"
                || !tags.insert(share.tag.as_str())
            {
                bail!("share tags must be unique [a-z0-9_-]{{1,32}} and not \"veynode\"")
            }
            if !share.path.is_absolute() || !share.path.is_dir() {
                bail!("share path must be an existing absolute directory")
            }
        }
        let mut listeners = BTreeSet::new();
        for port in &self.ports {
            if self.backend != Backend::QemuHvf {
                bail!("port forwards are QEMU user-network options; Apple NAT Guests are reachable directly")
            }
            if port.host_port == 0
                || port.guest_port == 0
                || !["tcp", "udp"].contains(&port.protocol.as_str())
            {
                bail!("port forwards need nonzero ports and tcp or udp")
            }
            if !listeners.insert((port.host_address, port.host_port, port.protocol.clone())) {
                bail!("duplicate host port forward")
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
impl Config {
    fn validate_boot(&self) -> Result<()> {
        if self.boot == BootMode::Kernel {
            if self.efi_vars.is_some() || self.cdrom.is_some() {
                bail!("efi_vars and cdrom require boot \"uefi\"")
            }
            return Ok(());
        }
        if self.guest != "linux" {
            bail!("UEFI boot is supported for Linux guests only")
        }
        if self.kernel.is_some() || self.initrd.is_some() || self.command_line.is_some() {
            bail!("UEFI boot uses the guest's own bootloader; kernel, initrd and command_line are not allowed")
        }
        if self.qemu_guest_exec {
            bail!("qemu_guest_exec needs the VeynodeVM guest agent and is unavailable for UEFI-booted distributions")
        }
        if self.disk.is_none() && self.cdrom.is_none() {
            bail!("UEFI boot requires a disk or a cdrom")
        }
        let vars = self
            .efi_vars
            .as_deref()
            .context("UEFI boot requires efi_vars (per-VM firmware variable store)")?;
        validate_private_output(vars, "efi_vars")?;
        if let Some(cdrom) = &self.cdrom {
            if !cdrom.is_absolute() {
                bail!("cdrom must be an absolute path")
            }
            let metadata = fs::symlink_metadata(cdrom)
                .with_context(|| format!("cdrom image missing: {}", cdrom.display()))?;
            if !metadata.is_file() {
                bail!("cdrom must be a regular file, not a symbolic link or directory")
            }
            if metadata.len() == 0 || metadata.len() > MAX_CDROM_BYTES {
                bail!("cdrom image must be 1 byte through 64 GiB")
            }
        }
        if [&self.disk, &self.cdrom, &self.efi_vars]
            .into_iter()
            .flatten()
            .any(|p| p.to_str().is_none_or(|s| s.contains(',')))
        {
            bail!("UEFI disk, cdrom and efi_vars paths must be UTF-8 without commas")
        }
        if [&self.disk, &self.cdrom]
            .into_iter()
            .flatten()
            .any(|p| Some(p) == self.efi_vars.as_ref())
        {
            bail!("efi_vars must be a separate file")
        }
        Ok(())
    }
}
/// A file a worker may create: absolute and normalized, in an existing
/// directory owned by the user that others cannot write, and, when it already
/// exists, a private regular file (never a symbolic link).
pub fn validate_private_output(path: &Path, name: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;
    if !path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
    {
        bail!("{name} must be an absolute normalized path")
    }
    let parent = path
        .parent()
        .with_context(|| format!("{name} needs a parent directory"))?;
    let directory = fs::metadata(parent)
        .with_context(|| format!("{name} directory missing: {}", parent.display()))?;
    let uid = unsafe { libc::geteuid() };
    if !directory.is_dir() || directory.uid() != uid || directory.mode() & 0o022 != 0 {
        bail!("{name} directory must be an existing directory owned by you and not group/other writable")
    }
    match fs::symlink_metadata(path) {
        Ok(existing) => {
            if !existing.is_file() || existing.uid() != uid || existing.mode() & 0o077 != 0 {
                bail!("{name} must be a private (0600) regular file owned by you")
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
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
    },
    Screenshot {
        guest_wrapped: bool,
        baselines: Arc<Mutex<screen_wait::Baselines>>,
        vm: String,
    },
    /// A guest agent reply `{"id","result"}` returned as its result.
    GuestResult,
    Start {
        reservation: jobs::Reservation,
        vm: String,
        owner: String,
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
        evdev: bool,
    },
    ScreenWait {
        dir: PathBuf,
        vm: String,
        probe: screen_wait::Probe,
        arguments: Value,
        baseline: Option<Value>,
    },
    Synchronous {
        dir: PathBuf,
        arguments: Value,
    },
    MacDesktop {
        dir: PathBuf,
        vm: String,
        method: String,
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
                evdev,
            } => desktop_input(&dir, &method, arguments, evdev, peer),
            Self::ScreenWait {
                dir,
                vm,
                probe,
                arguments,
                baseline,
            } => screen_wait::run(&dir, &vm, probe, &arguments, baseline, peer),
            Self::Synchronous { dir, arguments } => synchronous_exec(&dir, arguments, peer),
            Self::MacDesktop {
                dir,
                vm,
                method,
                arguments,
            } => container_tools::desktop(&dir, &vm, &method, &arguments),
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
                        let result =
                            call(&dir, "exec_cancel", json!({"id":job.vm,"job_id":job.id}));
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
    evdev: bool,
    peer: &UnixStream,
) -> Result<Value> {
    let mut steps = input::plan(method, &arguments)?;
    let first_gap = input::first_gap_ms(method, &arguments)?;
    if evdev {
        // Apple Virtualization Linux: the guest agent drives uinput devices.
        steps = steps
            .iter()
            .map(|events| desktop_input::planned_evdev(events))
            .collect::<Result<_>>()?;
    }
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
            if events.is_empty() {
                continue; // a wheel release has no evdev event
            }
            arguments["begin"] = json!(index == 0);
            arguments["events"] = json!(events);
            call(dir, "input_step", arguments.clone())?;
            sent += 1;
            if index + 1 < steps.len() {
                let gap = if index == 0 { first_gap } else { 20 };
                thread::sleep(Duration::from_millis(gap));
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
    // Every command uses the same registered Guest job path. Never fall back to
    // legacy uncancellable exec when an older Guest lacks asynchronous integration.
    let started = call(dir, "exec_start", arguments.clone()).map_err(|error| anyhow::anyhow!(
        "synchronous command start failed (Guest exec_start/exec_poll/exec_cancel support required): {error}"
    ))?;
    let job_id = started["result"]["job_id"]
        .as_str()
        .context("guest returned no job identity")?;
    let job = json!({"id":arguments["id"],"job_id":job_id});
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
                    }) => {
                        result = normalize_backend_input(result, &method, guest_wrapped)?;
                    }
                    Some(JobPost::Start {
                        reservation,
                        vm,
                        owner,
                    }) => {
                        if result["result"]["state"] != "running" {
                            bail!("guest returned invalid asynchronous start state")
                        }
                        let job_id = result["result"]["job_id"]
                            .as_str()
                            .context("guest returned no asynchronous job id")?;
                        reservation.register(&vm, job_id, &owner, std::time::Instant::now())?;
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
                    Some(JobPost::Screenshot {
                        guest_wrapped,
                        baselines,
                        vm,
                    }) => {
                        result = normalize_screenshot(result, guest_wrapped)?;
                        baselines.lock().unwrap().record(&vm, &result);
                    }
                    Some(JobPost::GuestResult) => {
                        result = result
                            .get("result")
                            .cloned()
                            .context("guest reply missing result")?;
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
fn normalize_backend_input(value: Value, method: &str, guest_wrapped: bool) -> Result<Value> {
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
    )
}
fn normalize_guest_input(value: Value, method: &str) -> Result<Value> {
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
    jobs: Arc<Mutex<jobs::Jobs>>,
    file_sessions: FileSessionRoutes,
    baselines: Arc<Mutex<screen_wait::Baselines>>,
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
            qemu_window: json!({"native_window":false,"display_only":true,"input_enabled":false}),
            native_connections: BTreeMap::new(),
            configs,
            jobs: Arc::new(Mutex::new(jobs::Jobs::default())),
            file_sessions: BTreeMap::new(),
            baselines: Arc::new(Mutex::new(screen_wait::Baselines::default())),
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
        let exit = self
            .running
            .get_mut(id)
            .map(Running::poll_exit)
            .transpose()?
            .flatten();
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
            result["capabilities"]["input_scope"] = json!("absolute click/drag/discrete wheel/explicit qcode only; snapshot <=5s and matching generation required; no text/layout translation");
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
                json!({"resource_admission":{"physical_memory_mib":self.memory.physical_memory_mib,"vm_memory_budget_mib":self.memory.vm_memory_budget_mib,"active_assigned_memory_mib":assigned,"remaining_budget_mib":self.memory.vm_memory_budget_mib.saturating_sub(assigned),"explicitly_configured":self.memory.explicitly_configured,"scope":"assigned guest RAM in this daemon; excludes VM overhead, other daemons and unrelated host workloads; not a host pressure guarantee"},"backends":{"apple-virtualization":backend::capabilities(Backend::AppleVirtualization),"qemu-hvf":{"registered":self.qemu.is_some(),"capabilities":qemu_capabilities}},"backend":"apple-virtualization","guests":["linux","macos"],"windows":false,"window_enumeration":"macOS Guest --allow-observe with existing Screen Recording permission and eligible console; no titles/AX tree/input authority; other backends unsupported","lifecycle":true,"native_window":true,"screenshot":true,"screenshot_requirements":"macOS Guest --allow-observe and existing Guest Screen Recording permission, registered QEMU capture, or an Apple Virtualization Linux desktop container","input":true,"input_requirements":"registered QEMU persistent console, macOS Guest --allow-observe --allow-input with existing Guest Screen Recording/Accessibility permissions, or an Apple Virtualization Linux desktop container; independent MCP input permission and a recent matching screenshot","screen_wait":"every screenshot backend; changed regions on Apple Virtualization Linux desktops","screen_text":"every screenshot backend; host Apple Vision through the registered vm-vision helper","desktop_windows":"Apple Virtualization Linux desktop containers through the guest agent (X11/EWMH), and veynodevm macOS containers through the Accessibility API over SSH: state and apps observe; window actions and app launch need input permission","accessibility":"Apple Virtualization Linux desktop containers whose image runs at-spi2 (AT-SPI over the desktop's accessibility bus), and veynodevm macOS containers (the macOS Accessibility API over SSH, granted at first boot): tree and find observe; element actions need input permission","command_execution":"Apple Linux backend requires enabled async Guest jobs; macOS legacy exec is not exposed; QEMU requires explicit qemu_guest_exec v2 and rootless cgroup-v2 Guest service","async_commands":"Apple Linux backend requires exec_start/exec_poll/exec_cancel support; jobs are bound to their VM, run under a guest timeout and are killed with their process group when they time out, are cancelled or their MCP session ends; QEMU default observer transport has no command capability","file_transfer":"Apple backend requires enabled Guest files integration: Linux dedicated non-root files account or macOS opt-in same-UID private root; QEMU private serial transports have no file capability; query vm_guest_info for current capabilities and file_policy; single filenames, 8MiB max","remote_desktop":false,"guest_integration":"Apple backend uses enabled Linux/macOS host-only vsock service; QEMU private virtio-serial defaults to observer-only v1; explicit qemu_guest_exec selects v2 requiring rootless cgroup-v2 Guest execution; query vm_guest_info for actual availability and capabilities","guest_shutdown":"Apple Linux integration requires explicit shutdown enablement; QEMU guest-service shutdown is unsupported and QMP power-button requests remain separate; request acceptance is not shutdown completion","gpu":"requires guest measurements; no performance claim"}),
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
        if method == "delete" {
            if args["confirm"].as_str() != Some(id) {
                bail!("delete requires confirm equal to the VM id")
            }
            if let Some(run) = self.running.get_mut(id) {
                if run.poll_exit()?.is_none() {
                    bail!("VM is running; stop it before delete")
                }
            }
            let mut next = self.configs.clone();
            let removed = next.remove(id).context("VM not found")?;
            atomic_json(&self.dir.join("registry.json"), &next)?;
            self.configs = next;
            self.running.remove(id);
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
        let mut desktop_events = None;
        if method == "desktop_input" {
            let apple_linux = self.configs[id].backend == Backend::AppleVirtualization
                && self.configs[id].guest == "linux";
            if self.configs[id].backend != Backend::QemuHvf && !apple_linux {
                bail!("unsupported: desktop_input needs a Linux desktop on QEMU/HVF or Apple Virtualization")
            }
            if args
                .as_object()
                .is_none_or(|a| a.len() != 2 || !a.contains_key("id") || !a.contains_key("events"))
            {
                bail!("desktop_input accepts only id and events")
            }
            let events = desktop_input::parse(&args["events"])?;
            if apple_linux {
                // The guest agent's uinput devices are reached through the
                // caller's desktop stream.
                return Ok(json!({"sent":false,"inject":"stream"}));
            }
            desktop_events = Some(desktop_input::qmp(&events)?);
        }
        let now = std::time::Instant::now();
        if self
            .running
            .get(id)
            .is_some_and(|run| run.execution_quiescing)
            && (method.starts_with("exec") || method == "input_step" || desktop_tools::acts(method))
        {
            bail!("VM execution is quiescing for shutdown")
        }
        if method == "exec" {
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
            if !guest_input
                && (self.configs[id].backend != Backend::QemuHvf
                    || self
                        .qemu
                        .as_ref()
                        .is_none_or(|q| q.console_broker.is_none()))
            {
                bail!("unsupported: desktop input requires QEMU persistent console, a macOS Guest input service or an Apple Virtualization Linux desktop")
            }
            if ["input_click", "input_drag", "input_scroll", "input_key"].contains(&method) {
                input::plan(method, args)?;
                *deferred = Some(Completion::Input {
                    dir: self.dir.clone(),
                    arguments: args.clone(),
                    method: method.into(),
                    evdev: apple_guest && self.configs[id].guest == "linux",
                });
                return Ok(Value::Null);
            }
        }
        let mut desktop_arguments = None;
        if desktop_tools::METHODS.contains(&method) {
            let macos = self.configs[id].guest == "macos";
            if self.configs[id].backend != Backend::AppleVirtualization
                || !(macos || self.configs[id].guest == "linux")
            {
                bail!("unsupported: desktop window and application tools require an Apple Virtualization Linux desktop or macOS container")
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
        if method == "status" {
            return self.view(id);
        }
        if method == "start" {
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
                } else if method == "screenshot" {
                    17
                } else if method == "screen_wait" {
                    30
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
    // request. Other option failures are fatal.
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
            // Include socket reads and queue/manager-lock delay.
            if admitted_at.elapsed() > Duration::from_secs(30) {
                bail!("controller request expired before dispatch")
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
        json!({"name":"vm_screenshot","description":"Capture the same console at full source resolution: QEMU PNG, Apple Virtualization Linux desktop PNG (JPEG when large) or macOS Guest JPEG. Returns actual MIME, frame id, dimensions, scale and capture timing. veynodevm macOS containers are captured through the Guest's Screen Sharing server (no Guest permission grants); other macOS Guests need the opted-in Guest service with existing Screen Recording permission, never requested automatically.","inputSchema":id}),
        json!({"name":"vm_screen_wait","description":"Wait until the VM screen changes, then optionally until it has been still for settle_ms (for example after a click, until the app has finished redrawing). Pass the frame_id of the screenshot you acted on so a change that already happened counts. Returns changed, settled, timing and, on Apple Virtualization Linux desktops, the changed regions in screenshot pixels; region limits the wait to one area there. QEMU detects console draws and macOS Guests compare screenshots, both whole-screen. Take a new screenshot afterwards before input.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"frame_id":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1,"maximum":screen_wait::MAX_TIMEOUT_MS,"default":10000},"settle_ms":{"type":"integer","minimum":0,"maximum":screen_wait::MAX_SETTLE_MS,"default":0},"region":{"type":"object","properties":{"x":{"type":"integer","minimum":0},"y":{"type":"integer","minimum":0},"width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1}},"required":["x","y","width","height"],"additionalProperties":false}},"required":["id"],"additionalProperties":false}}),
        json!({"name":"vm_screen_text","description":"Read the text on the VM screen without an image: takes a screenshot and runs host text recognition (Apple Vision) on it, for every backend with screenshots and nothing installed in the Guest. Returns lines in reading order with boxes and center points in screenshot pixels, plus the screenshot's frame_id, source_generation, width and height, so a recognized word can be clicked directly with the input tools. find returns each case-insensitive occurrence of a string with its own box. fast trades accuracy for speed; languages are BCP 47 codes (default automatic).","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"find":{"type":"string","minLength":1,"maxLength":256},"fast":{"type":"boolean","default":false},"languages":{"type":"array","items":{"type":"string"},"maxItems":8}},"required":["id"],"additionalProperties":false}}),
        json!({"name":"vm_windows","description":"Read macOS Guest on-screen application window metadata with existing Screen Recording permission and --allow-observe. No titles, focus, stacking order, Accessibility tree or input authority. Coordinates are Guest desktop-global points; this enumeration is not an atomic screenshot. Other Guest/backend combinations are unsupported.","inputSchema":id}),
        json!({"name":"vm_guest_info","description":"Query the real Guest service: Apple host-only vsock or QEMU private virtio-serial (default observer-only; explicit v2 permits rootless cgroup execution). Reports actual capabilities; fails if unavailable.","inputSchema":id}),
        json!({"name":"vm_status","description":"Get VM configuration and process state; running does not assert guest health.","inputSchema":id}),
        json!({"name":"vm_capabilities","description":"Report implemented operations and explicitly unsupported integrations.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
    ];
    // After the screen tools, so observe-only listings keep their order.
    tools.splice(4..4, desktop_tools::tools(input));
    if lifecycle {
        for (name, description) in [
            ("vm_start", "Start a VM in its own native backend process."),
            (
                "vm_shutdown",
                "Request normal shutdown via backend power button (default) or opt-in installed guest service (mode guest). Request acceptance is not completion; poll status. Never implicitly force-stops.",
            ),
            ("vm_show_window", "Show native VM console; QEMU input requires a verified v2 helper and its bound private controller channel, and flows only while the window has focus."),
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
            "efi_vars",
            "cdrom",
        ] {
            properties[p] = json!({"type":"string"});
        }
        properties["boot"] = json!({"type":"string","enum":["kernel","uefi"],"default":"kernel","description":"uefi boots the Linux distribution's own bootloader from disk or cdrom (installer ISO) and requires efi_vars, a per-VM firmware variable store created on first start."});
        properties["qemu_monitor"] = json!({"type":"boolean","description":"QEMU only: expose a private QMP monitor socket in the runtime state for installer automation."});
        tools.push(json!({"name":"vm_create","description":"Register a Linux direct-boot, Linux UEFI (distribution ISO/installed disk) or installed macOS VM configuration; does not create storage.","inputSchema":{"type":"object","properties":properties,"required":["id","guest","cpus","memory_mib"],"additionalProperties":false}}));
    }
    if execute {
        tools.push(json!({"name":"vm_exec","description":"Execute argv directly inside the VM through its guest service. No implicit shell. Output bounded by guest service; timeout enforced in guest.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"argv":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":128},"timeout_ms":{"type":"integer","minimum":1,"maximum":30000}},"required":["id","argv"],"additionalProperties":false}}));
    }
    if execute {
        tools.push(json!({"name":"vm_exec_start","description":"Start a bounded asynchronous guest command. Returns an opaque job id; poll for completion. The guest kills the command's process group at timeout_ms (default 5000), on cancel, or when this MCP session ends.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"argv":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":128},"timeout_ms":{"type":"integer","minimum":1,"maximum":30000}},"required":["id","argv"],"additionalProperties":false}}));
        for (name,description) in [("vm_exec_poll","Poll a job started on this VM. Inspect terminal result, exit code, cancelled and timed_out."),("vm_exec_cancel","Kill a guest job's process group. Cancellation is not rollback; poll to verify the actual terminal state.")] {tools.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":{"id":{"type":"string"},"job_id":{"type":"string","pattern":"^[0-9a-f]{32}$"}},"required":["id","job_id"],"additionalProperties":false}}));}
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
                json!({"keys":{"type":"array","items":{"type":"string","enum":input::key_names()},"minItems":1,"maxItems":8},"hold_ms":{"type":"integer","minimum":0,"maximum":input::MAX_HOLD_MS,"description":"Hold the keys this long before release (default a 20 ms tap); games that steer or accelerate while a key is down need a hold."}}),
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
            tools.push(json!({"name":format!("vm_{name}"),"description":"Bounded physical input to a QEMU console, an Apple Virtualization Linux desktop or an opted-in macOS Guest. Requires a screenshot younger than 5 seconds with matching VM, generation and dimensions. Backend dispatch does not prove app acceptance; no Unicode/layout translation; query Guest supported physical keys.","inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}));
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
    tools.extend(container_tools::tools(lifecycle, execute));
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
    // The first cleanup owns the daemon's response. A second call must not
    // replace a failed result with an empty, apparently successful response.
    outcome: Mutex<Option<bool>>,
}
impl SessionCleanupState {
    fn finish(&self) -> Result<()> {
        self.stopped.store(true, Ordering::Release);
        let mut outcome = self.outcome.lock().unwrap();
        let clean = *outcome.get_or_insert_with(|| cleanup_session(&self.dir, &self.owner).is_ok());
        if !clean {
            bail!("session cleanup unconfirmed; a Guest cleanup request failed")
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
    if result["closed"] != true {
        bail!("session cleanup unconfirmed")
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
    // A valid cancel_requested reply is not evidence that a job has reached its
    // terminal state. This receipt covers the cleanup RPC outcomes.
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
                    json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":VERSION,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"veynode-vm","version":env!("CARGO_PKG_VERSION")},"instructions":"Local trusted-user daemon. Observe is default; lifecycle access requires server --allow-lifecycle. Command execution requires --allow-execute and enabled Linux Guest async jobs; macOS legacy execution is not exposed by vm_exec, but container_exec runs commands in veynodevm macOS containers over SSH. File transfer independently requires --allow-files and an enabled Guest files root: a dedicated Linux files account, or macOS --allow-files --files-root with a private directory owned by the service UID. macOS files are not isolated from other same-UID programs. Query vm_guest_info for actual files capability and file_policy; Guest denial is an error, not permission to use another path. Screenshot observation requires QEMU capture, a veynodevm macOS container (Guest Screen Sharing; no Guest permission grants) or macOS Guest --allow-observe with existing Guest Screen Recording permission. vm_windows uses macOS Guest observation only and returns no titles or input authority; other backends are unsupported. Linux desktop containers (--desktop) on Apple Virtualization are captured and driven by the guest agent with no Guest setup. vm_screen_wait waits for the screen to change or settle; vm_screen_text reads its text with boxes in screenshot pixels. On those Linux desktops and on veynodevm macOS containers vm_desktop_state and vm_desktop_apps report windows, focus, keyboard layout and installed applications without a screenshot; with input permission vm_window_action and vm_app_launch act on them. vm_accessibility_tree and vm_accessibility_find read their accessibility tree (AT-SPI on Linux, the Accessibility API on macOS) with roles, names, states and boxes; vm_accessibility_action (input permission) clicks, focuses or fills an element by ref. Physical desktop input always requires server --allow-input. Its backend must provide QEMU persistent capture, a veynodevm macOS container (Guest Screen Sharing), an Apple Virtualization Linux desktop container, or macOS Guest --allow-observe --allow-input with existing Guest Screen Recording and Accessibility permissions. Query vm_guest_info for macOS permission availability and supported physical keys; Unicode text and keyboard-layout translation are unsupported. Input requires a matching screenshot younger than 5 seconds. Commands run under a guest timeout; the guest kills a command's process group when it times out, is cancelled or this MCP session ends. Backend input acknowledgement does not prove application acceptance. VM start returns before guest readiness."}})
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
                        // The session owner lets session_end cancel this session's
                        // jobs and file transfers when the client goes away.
                        if name.starts_with("vm_file_")
                            || ["vm_exec", "vm_exec_start"].contains(&name)
                        {
                            args["owner"] = json!(owner);
                        }
                        let result = if name.starts_with("container_") || name.starts_with("image_")
                        {
                            container_tools::call(dir, name, &args)
                        } else if name == "vm_screen_text" {
                            screen_text::run(dir, &args)
                        } else {
                            call(dir, name.strip_prefix("vm_").unwrap(), args)
                        };
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
    fn qemu_input_cleanup_acknowledgements_cannot_be_empty_or_negative() {
        for result in [
            json!({}),
            json!({"released":false}),
            json!({"released":true}),
        ] {
            assert!(normalize_backend_input(result, "input_cleanup", false).is_err());
        }
        let valid = json!({"released":true,"synthetic_release_dispatched":true});
        assert_eq!(
            normalize_backend_input(valid.clone(), "input_cleanup", false).unwrap(),
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
            assert!(normalize_guest_input(json!({"result":result}), "input_step").is_err());
        }
        let step = json!({"dispatched":true,"guest_delivery_confirmed":false});
        assert_eq!(
            normalize_guest_input(json!({"result":step}), "input_step").unwrap(),
            step
        );
        assert!(
            normalize_guest_input(json!({"result":{"released":false}}), "input_cleanup").is_ok()
        );
        assert!(
            normalize_guest_input(json!({"result":{"released":true}}), "input_cleanup").is_err()
        );
        assert!(normalize_guest_input(json!({"result":{}}), "input_revoke").is_err());
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
        let args = json!({"id":"mac","operation_id":"b".repeat(32),"begin":true,"events":[]});
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
        pending.lock().unwrap().remove(&1).unwrap().send(json!({"id":1,"result":{"id":"1","error":{"code":"accessibility_permission_required","message":"denied"}}})).unwrap();
        let (peer, _) = UnixStream::pair().unwrap();
        assert!(completion
            .unwrap()
            .wait(&peer)
            .unwrap_err()
            .to_string()
            .contains("accessibility_permission_required"));
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
        assert_eq!(linux["screenshot"], true);
        assert_eq!(linux["input"], true);
        assert!(linux["input_requirements"]
            .as_str()
            .unwrap()
            .contains("desktop container"));
        assert_eq!(linux["command_execution"], true);
        assert_eq!(linux["async_commands"], true);
    }
    #[test]
    fn mac_address_must_be_local_unicast() {
        assert!(valid_local_mac("72:76:f4:5e:d8:14"));
        assert!(!valid_local_mac("73:76:f4:5e:d8:14"), "multicast");
        assert!(
            !valid_local_mac("00:1c:42:00:00:01"),
            "universally administered"
        );
        assert!(!valid_local_mac("72:76:f4:5e:d8"));
        assert!(!valid_local_mac("72:76:f4:5e:d8:1g"));
    }
    #[test]
    fn guest_stream_bridge_is_strict_local_apple_only() {
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
        .is_ok());
        assert!(validate_local_stream_bridge(
            "start_guest_stream_bridge",
            &start,
            Backend::AppleVirtualization,
            "windows"
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
    fn uefi_machines_register_on_both_backends_without_a_kernel() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let iso = root.join("installer.iso");
        fs::write(&iso, vec![0u8; 2048]).unwrap();
        let disk = root.join("disk.raw");
        fs::write(&disk, vec![0u8; 4096]).unwrap();
        fs::set_permissions(&disk, fs::Permissions::from_mode(0o600)).unwrap();
        fs::create_dir(root.join("state")).unwrap();
        fs::set_permissions(root.join("state"), fs::Permissions::from_mode(0o700)).unwrap();
        let mut manager = Manager::load(
            root.join("state"),
            "/unused".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        manager.qemu = Some(QemuRegistration {
            worker: iso.clone(),
            launcher: iso.clone(),
            library: iso.clone(),
            resources: root.clone(),
            spice_library: None,
            screenshot_helper: None,
            console_broker: None,
            console_helper: None,
            venus_runtime: None,
        });
        for (id, backend) in [("q", "qemu-hvf"), ("a", "apple-virtualization")] {
            let request = json!({"method":"create","arguments":{"backend":backend,"id":id,"guest":"linux","cpus":2,"memory_mib":2048,"boot":"uefi","disk":disk,"cdrom":iso,"efi_vars":root.join(format!("{id}-vars.fd"))}});
            let view = manager.dispatch(request, &mut None).unwrap();
            assert_eq!(view["config"]["boot"], "uefi");
            assert_eq!(view["capabilities"]["boot"], json!(["kernel", "uefi"]));
            assert!(view["config"].get("kernel").is_none());
        }
        // The firmware store is created by the worker, never by registration.
        assert!(!root.join("q-vars.fd").exists());
        let bad = json!({"method":"create","arguments":{"id":"x","guest":"linux","cpus":1,"memory_mib":512,"boot":"uefi","cdrom":iso}});
        assert!(manager.dispatch(bad, &mut None).is_err());
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
    fn stale_requests_expire_but_disconnected_cleanup_still_runs() {
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
        let owner = "ab".repeat(32);
        manager
            .file_sessions
            .insert(owner.clone(), (std::time::Instant::now(), BTreeSet::new()));
        let manager = Arc::new(Mutex::new(manager));
        let (mut client, server) = UnixStream::pair().unwrap();
        write_json(
            &mut client,
            &json!({"method":"status","arguments":{"id":"test"}}),
        )
        .unwrap();
        handle_connection(
            server,
            manager.clone(),
            std::time::Instant::now() - Duration::from_secs(31),
        )
        .unwrap();
        let reply = read_frame(&mut BufReader::new(client)).unwrap().unwrap();
        assert!(String::from_utf8(reply)
            .unwrap()
            .contains("expired before dispatch"));
        // Cleanup is deliberately processed despite caller disconnect.
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
        assert!(manager.lock().unwrap().file_sessions.is_empty());
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
            let error = fixture
                .manager
                .dispatch(
                    json!({"method":method,"arguments":{"id":"mac","owner":"a".repeat(64)}}),
                    &mut deferred,
                )
                .unwrap_err();
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
        assert!(only.contains("vm_input_click") && !only.contains("vm_acquire_control"));
        assert!(
            only.contains("vm_window_action")
                && only.contains("vm_app_launch")
                && only.contains("vm_accessibility_action")
        );
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
        // 12 observe vm_* tools plus 4 read-only container/image tools.
        assert_eq!(list["tools"].as_array().unwrap().len(), 16);
        assert!(
            list.to_string().contains("vm_screen_wait")
                && list.to_string().contains("vm_screen_text")
                && list.to_string().contains("vm_desktop_state")
                && list.to_string().contains("vm_desktop_apps")
        );
        assert!(
            !list.to_string().contains("vm_window_action")
                && !list.to_string().contains("vm_app_launch")
                && !list.to_string().contains("vm_accessibility_action")
        );
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
