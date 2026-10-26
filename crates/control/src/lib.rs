pub mod action_result;
pub mod agent_view;
pub mod backend;
pub mod bridge;
mod container_state;
pub mod container_tools;
pub mod control_lease;
pub mod cua_bridge;
pub mod desktop_input;
pub mod desktop_tools;
pub mod driver_link;
mod events;
pub mod home;
pub mod input;
mod jobs;
pub mod mcp_events;
mod native_control;
pub mod native_input;
pub mod qemu_disk;
pub mod qemu_driver_channel;
pub mod qemu_extras;
pub mod qemu_graphics;
pub mod qemu_snapshot;
pub mod qemu_uefi;
mod resources;
pub mod screen_text;
pub mod screen_wait;
pub mod screenshot_scaled;
pub mod snapshot;
mod snapshot_control;
mod snapshot_restore;
pub mod tool_groups;
mod windows;
pub mod windows_driver;
pub mod windows_guest;
use anyhow::{bail, Context, Result};
use backend::{Backend, QemuRegistration};
use resources::{DiskIdentity, MemoryBudget};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
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
const SYNC_EXEC_MAX_TIMEOUT_MS: u64 = 30_000;
const ASYNC_EXEC_MAX_TIMEOUT_MS: u64 = 120_000;
/// A lossless 2560x1440 game frame is several MiB, so the PNG bound is 8 MiB (it was 2 MiB, which refused every real
/// scene at that size). The reply is base64 JSON: 8 MiB of PNG is about 10.7 MiB on the wire, hence the 12 MiB response
/// bound. The native console's `VEYNODE_SCREENSHOT_MAX_BYTES` and record writer carry the same pair.
pub const MAX_SCREENSHOT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SCREENSHOT_RESPONSE: usize = 12 * 1024 * 1024;
pub fn response_limit(method: &str) -> usize {
    // A 1 MiB driver frame inside the worker's and the guest agent's envelopes.
    if method == "screenshot" || method == "driver_rpc" {
        MAX_SCREENSHOT_RESPONSE
    } else {
        MAX_MESSAGE
    }
}
pub fn mcp_response_limit(request: &Value) -> usize {
    // An observing input call or a subscribed screen resource may carry a screenshot too.
    let name = request["params"]["name"].as_str().unwrap_or("");
    let driver = name.starts_with(cua_bridge::PREFIX) || cua_bridge::scope(name).is_some();
    if request["method"] == "tools/call"
        && (name == "vm_screenshot"
            || driver
            || request["params"]["arguments"]["observe"]["return"] == "screenshot")
        || request["method"] == "resources/read"
            && request["params"]["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("/screen"))
    {
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
    /// Guest-facing bus of the primary disk (QEMU): an installed Windows
    /// keeps the bus it was installed on.
    #[serde(default, skip_serializing_if = "qemu_uefi::DiskBus::is_default")]
    pub qemu_disk_bus: qemu_uefi::DiskBus,
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
    /// Fixed NIC address, so the host can find an Apple Guest's DHCP lease
    /// and forked containers get distinct addresses on both backends.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac_address: Option<String>,
    /// macOS Guest login file ("user\npassword"): the worker serves
    /// screenshots and input through the Guest's Screen Sharing (RFB) server
    /// instead of the in-Guest service, so no Guest TCC grants are needed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_sharing_login: Option<PathBuf>,
    /// Apple Linux containers: persistent generic machine identifier (created
    /// by the worker on first start), so saved machine state can be restored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_identifier_file: Option<PathBuf>,
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
            && (!["linux", "windows"].contains(&self.guest.as_str())
                || self.shared_dir.is_some()
                || self.restore_image.is_some()
                || self.machine_identity_dir.is_some())
        {
            bail!("unsupported QEMU configuration: this revision supports Linux ARM direct or UEFI boot, Windows ARM UEFI boot, optional explicitly raw disk, and SPICE; host sharing and other Guest backends are not integrated")
        }
        if self.qemu_monitor && self.backend != Backend::QemuHvf {
            bail!("qemu_monitor requires QEMU/HVF")
        }
        if !self.qemu_disk_bus.is_default() && self.backend != Backend::QemuHvf {
            bail!("qemu_disk_bus requires QEMU/HVF")
        }
        self.validate_boot()?;
        if self.backend == Backend::QemuHvf {
            backend::qemu_display_size(self.width, self.height)?;
            if let Some(disk) = &self.disk {
                qemu_disk::validate_path(disk)?;
            }
        }
        if !["linux", "macos", "windows"].contains(&self.guest.as_str()) {
            bail!("unsupported guest: {}", self.guest)
        }
        if self.guest == "windows"
            && (self.backend != Backend::QemuHvf
                || self.boot != BootMode::Uefi
                || !self.shares.is_empty())
        {
            bail!("Windows ARM guests need the QEMU/HVF backend with UEFI boot (Apple Virtualization has no Windows support) and no 9p shares")
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
            if !valid_local_mac(mac) {
                bail!("mac_address must be a locally administered unicast address")
            }
        }
        if let Some(path) = &self.machine_identifier_file {
            if self.backend != Backend::AppleVirtualization
                || self.guest != "linux"
                || self.machine_identity_dir.is_some()
                || !path.is_absolute()
            {
                bail!("machine_identifier_file needs an Apple Linux Guest without machine_identity_dir, at an absolute path")
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
        if !["linux", "windows"].contains(&self.guest.as_str()) {
            bail!("UEFI boot is supported for Linux and Windows guests only")
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
/// The registry format this daemon reads and writes. Version 1 is `registry.json` as a map of VM id to `Config`,
/// with no version inside it: every earlier build parses exactly that, and `schema_version` is itself a valid VM id.
pub const REGISTRY_SCHEMA: u64 = 1;
/// The version lives in a sidecar, `registry.schema` beside `registry.json`: a decimal number, optionally followed by
/// one newline. Absent means version 1, and at version 1 this daemon never writes the file. A higher version, or a
/// file that is not exactly that, refuses the start before the registry is read; nothing is rewritten.
fn check_registry_schema(dir: &Path) -> Result<()> {
    const REFUSED: &str = "daemon not started, nothing rewritten";
    let path = dir.join("registry.schema");
    let malformed = || {
        anyhow::anyhow!("registry.schema is malformed (expected a decimal registry schema version; this daemon supports {REGISTRY_SCHEMA}); {REFUSED}")
    };
    let unreadable = || {
        format!("registry.schema unreadable (this daemon supports registry schema version {REGISTRY_SCHEMA}); {REFUSED}")
    };
    let file = match open_state_file(&path) {
        Ok(None) => return Ok(()),
        Ok(Some(file)) => file,
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Err(malformed()),
        Err(error) => return Err(error).context(unreadable()),
    };
    let bytes = match read_state_file(file, &path, 21) {
        Ok(bytes) => bytes,
        Err(error) if error.is::<StateFileError>() => return Err(malformed()),
        Err(error) => return Err(error).context(unreadable()),
    };
    let digits = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    // Canonical decimal only: no sign, space or leading zero, and versions start at 1.
    let found = std::str::from_utf8(digits)
        .ok()
        .filter(|text| text.bytes().all(|b| b.is_ascii_digit()) && !text.starts_with('0'))
        .and_then(|text| text.parse::<u64>().ok())
        .ok_or_else(malformed)?;
    if found > REGISTRY_SCHEMA {
        bail!("registry schema version {found} is newer than this daemon supports ({REGISTRY_SCHEMA}); {REFUSED}")
    }
    Ok(())
}
#[derive(Debug)]
enum RegistryError {
    ReservedId(String),
    DuplicateKey(String),
    ProtectedConfigPath(PathBuf),
}
impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReservedId(id) => write!(f, "VM id {id:?} is reserved for daemon state"),
            Self::DuplicateKey(key) => {
                write!(f, "duplicate registry key {key:?}; preserving original")
            }
            Self::ProtectedConfigPath(path) => write!(
                f,
                "VM configuration path collides with daemon state: {}",
                path.display()
            ),
        }
    }
}
impl std::error::Error for RegistryError {}

#[derive(Debug)]
enum StateFileError {
    NotRegular(PathBuf),
    TooLarge(PathBuf),
}
impl std::fmt::Display for StateFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRegular(path) => write!(f, "{} is not a regular file", path.display()),
            Self::TooLarge(path) => write!(f, "{} exceeds size limit", path.display()),
        }
    }
}
impl std::error::Error for StateFileError {}

fn open_state_file(path: &Path) -> std::io::Result<Option<File>> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
fn read_state_file(file: File, path: &Path, limit: usize) -> Result<Vec<u8>> {
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(StateFileError::NotRegular(path.into()).into());
    }
    if meta.len() > limit as u64 {
        return Err(StateFileError::TooLarge(path.into()).into());
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    // The opened file can grow after fstat; the descriptor read has the same bound.
    if bytes.len() > limit {
        return Err(StateFileError::TooLarge(path.into()).into());
    }
    Ok(bytes)
}
fn read_registry(path: &Path) -> Result<BTreeMap<String, Config>> {
    let Some(file) = open_state_file(path)? else {
        return Ok(BTreeMap::new());
    };
    let bytes = read_state_file(file, path, 16 * MAX_MESSAGE)?;
    struct RegistryVisitor<'a> {
        duplicate: &'a mut Option<String>,
    }
    impl<'de> serde::de::Visitor<'de> for RegistryVisitor<'_> {
        type Value = BTreeMap<String, Config>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a map of VM ids to configurations")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            let mut configs = BTreeMap::new();
            while let Some(key) = map.next_key::<String>()? {
                if configs.contains_key(&key) {
                    *self.duplicate = Some(key);
                    return Err(serde::de::Error::custom("duplicate registry key"));
                }
                configs.insert(key, map.next_value::<Config>()?);
            }
            Ok(configs)
        }
    }
    let mut duplicate = None;
    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    let result = serde::Deserializer::deserialize_map(
        &mut deserializer,
        RegistryVisitor {
            duplicate: &mut duplicate,
        },
    );
    if let Some(key) = duplicate {
        return Err(RegistryError::DuplicateKey(key).into());
    }
    let configs = result.context("invalid registry; preserving original")?;
    deserializer
        .end()
        .context("invalid registry; preserving original")?;
    Ok(configs)
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
const PROTECTED_STATE_FILES: &[&str] = &["registry.json"];
fn validate_new_vm_id(id: &str) -> Result<()> {
    validate_id(id)?;
    // Only direct JSON state files can collide with <id>.json, including case aliases on macOS.
    if PROTECTED_STATE_FILES.iter().any(|name| {
        !name.contains('/')
            && name
                .strip_suffix(".json")
                .is_some_and(|stem| id.eq_ignore_ascii_case(stem))
    }) {
        return Err(RegistryError::ReservedId(id.into()).into());
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
/// One frame, handed to the descriptor in few writes: the serializer emits every token separately, and an
/// unbuffered socket or pipe would take a system call (and wake its reader) for each. A string larger than the
/// 8 KiB buffer still goes out directly, so no second copy of a screenshot is made.
fn write_json(writer: &mut impl Write, value: &Value) -> Result<()> {
    let mut buffered = std::io::BufWriter::new(&mut *writer);
    let result = (|| {
        serde_json::to_writer(&mut buffered, value)?;
        buffered.write_all(b"\n")?;
        buffered.flush()?;
        Ok(())
    })();
    // Discard, never flush again from the destructor: a failed frame gets no second write attempt.
    let _ = buffered.into_parts();
    result
}
/// Kernel send buffer requested for a connection about to carry a screenshot reply. macOS moves a Unix stream
/// in steps of the sender's buffer, 8 KiB by default: several hundred reader wake-ups for one 4 MiB reply.
pub const LARGE_REPLY_SEND_BUFFER: usize = 1024 * 1024;
/// Best effort; a refusal only leaves the default buffer. Bounds, deadlines and framing are unchanged: the kernel
/// may hold up to this many unread bytes for the peer instead of 8 KiB, for as long as the write deadline allows.
pub fn widen_send_buffer(socket: &UnixStream) {
    let size = LARGE_REPLY_SEND_BUFFER as libc::c_int;
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&size as *const libc::c_int).cast(),
            std::mem::size_of_val(&size) as libc::socklen_t,
        );
    }
}
/// Reader buffer for a stream that may carry a screenshot reply: one read per pipe or socket buffer, not per 8 KiB.
const LARGE_REPLY_READ_BUFFER: usize = 64 * 1024;
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
fn write_vm_config(dir: &Path, path: &Path, config: &Config) -> Result<()> {
    if PROTECTED_STATE_FILES
        .iter()
        .any(|name| path == dir.join(name))
        || (path.parent() == Some(dir)
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("registry.json")))
    {
        return Err(RegistryError::ProtectedConfigPath(path.into()).into());
    }
    atomic_json(path, config)
}
struct GuestAsyncTimeout {
    maximum: u64,
    generation: Option<u64>,
}
impl Default for GuestAsyncTimeout {
    fn default() -> Self {
        Self {
            maximum: SYNC_EXEC_MAX_TIMEOUT_MS,
            generation: Some(0),
        }
    }
}
impl GuestAsyncTimeout {
    fn invalidate(&mut self) {
        self.maximum = SYNC_EXEC_MAX_TIMEOUT_MS;
        // Exhaustion leaves the policy unknown permanently, never reuses a
        // generation still held by a pending info completion.
        self.generation = self.generation.and_then(|value| value.checked_add(1));
    }
    fn observe_worker_state(&mut self, state: &Value) {
        // Ordinary running/paused updates do not renegotiate a no-reconnect
        // serial lifetime or invalidate a just-completed info observation.
        if matches!(state.as_str(), Some("stopped" | "error")) {
            self.invalidate();
        }
    }
}
struct Running {
    bridge: Arc<Mutex<bridge::State>>,
    child: Child,
    assigned_memory_mib: u64,
    expected_exit: bool,
    execution_quiescing: bool,
    // A new worker gets a new cell. Old info completions retain only the old
    // cell, so neither VM-id reuse nor a late reply can elevate its successor.
    guest_async_timeout: Arc<Mutex<GuestAsyncTimeout>>,
    exit_status: Option<std::process::ExitStatus>,
    disk_identity: Option<DiskIdentity>,
    stdin: ChildStdin,
    state: Arc<Mutex<Value>>,
    pending: Arc<Mutex<BTreeMap<u64, PendingReply>>>,
    next: u64,
}
// These values describe the original launch or its last Guest observation and remain
// historical facts after terminal/error events. This function only receives one Running's state;
// a replacement worker starts with a fresh state object and inherits nothing.
fn replace_runtime_state(current: &mut Value, mut next: Value) {
    for field in [
        "disk_provenance",
        "graphics_profile",
        "execution_guardian",
        "backend_pid",
        "vm_id",
        "serial_log",
        "stderr_log",
        "guest_metal_device_alert",
        "guest_metal_device_observation",
    ] {
        if let Some(value) = current.get(field) {
            next[field] = value.clone();
        }
    }
    *current = next;
}
// A missing or malformed graphics reply is not evidence that a previously lost
// device recovered. Keep the Guest envelope unchanged; this is only a worker-local
// health summary, and no query compiles a pipeline or proves application support.
fn update_guest_metal_health(state: &mut Value, info: &Value) {
    let graphics = &info["graphics"];
    let missing = graphics["status"] == "unavailable" && graphics["reason"] == "no_metal_device";
    let device = &graphics["device"];
    let device_name = device["name"]
        .as_str()
        .filter(|name| !name.trim().is_empty());
    let valid_registry = device["registry_id"].as_str().is_some_and(|value| {
        !value.is_empty()
            && value.bytes().all(|byte| byte.is_ascii_digit())
            && value.parse::<u64>().is_ok()
    });
    let reported = graphics["schema"] == 1
        && graphics["api"] == "metal"
        && graphics["status"] == "reported"
        && graphics["device_selection"] == "system-default"
        && graphics["evidence"] == "public-device-properties"
        && graphics["queried_at_unix_ms"].as_u64().is_some()
        && device_name.is_some()
        && valid_registry
        && info["metal_device"].as_str() == device_name;
    let mut observation = json!({
        "status":if missing { "unavailable" } else if reported { "reported" } else { "unknown" },
        "source":"guest_info",
        "scope":"last successful info reply for this worker; not polled; device presence does not validate rendering or performance",
    });
    if let Some(queried_at) = graphics["queried_at_unix_ms"].as_u64() {
        observation["queried_at_unix_ms"] = json!(queried_at);
    }
    if missing {
        observation["reason"] = json!("no_metal_device");
        let mut alert = json!({
            "reason":"no_metal_device",
            "source":"guest_info",
            "scope":"last explicit no-Metal-device observation for this worker; not polled; retained until a validated device report or a new worker",
            "operator_guidance":{
                "action":"save Guest work, request normal Guest shutdown, wait for the stopped state, start it again, then query vm_guest_info",
                "evidence":"normal Guest restarts recovered the device in the retained decision 0023 observations; the loss trigger remains unestablished",
                "decision":"docs/decisions/0023-macos-guest-metal-compute-limit.md",
                "automatic_recovery":false,
                "verification":"a reported device clears this loss alert; rendering, large compute pipelines and performance still require separate tests",
            },
        });
        if let Some(queried_at) = graphics["queried_at_unix_ms"].as_u64() {
            alert["queried_at_unix_ms"] = json!(queried_at);
        }
        state["guest_metal_device_alert"] = alert;
    } else if reported {
        state
            .as_object_mut()
            .unwrap()
            .remove("guest_metal_device_alert");
    } else {
        observation["reason"] = json!(if graphics.is_null() {
            "graphics_not_reported"
        } else {
            "invalid_or_unrecognized_graphics_report"
        });
    }
    state["guest_metal_device_observation"] = observation;
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
    /// Publishes this worker's exit once it is known; repeated calls are no-ops.
    fn publish_exit(&self, id: &str, events: &events::Log) {
        if let Some(status) = self.exit_status {
            events.exited(id, self.child.id(), status, self.expected_exit);
        }
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
/// One screenshot through the daemon, in microseconds, printed as one stderr line when its reply has been written.
/// Numbers only: no Guest content, path, identifier or hash. The `broker_*` and `worker_*` stages are the QEMU
/// worker's own report and stay zero for a worker that sends none.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ShotTiming {
    /// Command written to the worker until its reply reached the waiting connection.
    backend_us: u64,
    broker_wait_us: u64,
    broker_read_us: u64,
    worker_parse_us: u64,
    worker_wake_us: u64,
    /// The worker's timing record seen until the reply line that follows it was complete.
    pipe_us: u64,
    parse_us: u64,
    normalize_us: u64,
    bytes: u64,
}
/// The worker's stage names in its `screenshot_timing` record, and the names they get here.
const SHOT_WORKER_STAGES: [(&str, &str); 4] = [
    ("broker_wait_us", "broker_wait_us"),
    ("broker_read_us", "broker_read_us"),
    ("parse_us", "worker_parse_us"),
    ("wake_us", "worker_wake_us"),
];
impl ShotTiming {
    /// From the in-process note the worker's reader attached to a reply; anything but an unsigned integer is zero.
    fn from_reply(reply: &Value, backend_us: u64, normalize_us: u64) -> Self {
        let field = |name: &str| reply["screenshot_timing"][name].as_u64().unwrap_or(0);
        Self {
            backend_us,
            broker_wait_us: field("broker_wait_us"),
            broker_read_us: field("broker_read_us"),
            worker_parse_us: field("worker_parse_us"),
            worker_wake_us: field("worker_wake_us"),
            pipe_us: field("pipe_us"),
            parse_us: field("parse_us"),
            normalize_us,
            bytes: field("bytes"),
        }
    }
    fn line(&self, total_us: u64, write_us: u64) -> String {
        format!(
            "screenshot timing total_us={total_us} backend_us={} broker_wait_us={} broker_read_us={} \
             worker_parse_us={} worker_wake_us={} pipe_us={} parse_us={} normalize_us={} write_us={write_us} bytes={}",
            self.backend_us,
            self.broker_wait_us,
            self.broker_read_us,
            self.worker_parse_us,
            self.worker_wake_us,
            self.pipe_us,
            self.parse_us,
            self.normalize_us,
            self.bytes,
        )
    }
}
thread_local! {
    /// Set by `Pending::wait` on the connection's own thread and taken by `handle_connection` after the reply write.
    static SHOT_TIMING: std::cell::Cell<Option<ShotTiming>> = const { std::cell::Cell::new(None) };
}
const LOG_QUEUE_CAPACITY: usize = 64;
const LOG_LINE_LIMIT: usize = 4096;
enum LogMessage {
    Line(String),
    Flush(mpsc::Sender<()>),
}
static LOGGER: std::sync::OnceLock<LineLogger> = std::sync::OnceLock::new();
struct LineLogger {
    sender: mpsc::SyncSender<LogMessage>,
    dropped: std::sync::atomic::AtomicU64,
}
impl LineLogger {
    fn new(mut sink: impl Write + Send + 'static, capacity: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<LogMessage>(capacity);
        thread::spawn(move || {
            for message in receiver {
                match message {
                    LogMessage::Line(line) => {
                        let _ = writeln!(sink, "{line}");
                    }
                    LogMessage::Flush(done) => {
                        let _ = sink.flush();
                        let _ = done.send(());
                    }
                }
            }
        });
        Self {
            sender,
            dropped: std::sync::atomic::AtomicU64::new(0),
        }
    }
    fn flush(&self, deadline: Duration) -> bool {
        let started = std::time::Instant::now();
        let (done, received) = mpsc::channel();
        let mut barrier = LogMessage::Flush(done);
        loop {
            let remaining = deadline.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return false;
            }
            match self.sender.try_send(barrier) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(message)) => {
                    barrier = message;
                    thread::sleep(remaining.min(Duration::from_millis(1)));
                }
                Err(mpsc::TrySendError::Disconnected(_)) => return false,
            }
        }
        received
            .recv_timeout(deadline.saturating_sub(started.elapsed()))
            .is_ok()
    }
    fn send(&self, line: &str) {
        if line.len() > LOG_LINE_LIMIT
            || self
                .sender
                .try_send(LogMessage::Line(line.to_owned()))
                .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
/// A single writer owns stderr writes. Requests never wait for that sink or queue space; memory is bounded.
/// On SIGTERM, queued lines not yet written may be lost; at most the 64-line queue.
/// Silent in test builds, whose output stderr would bypass capture.
fn log_line(line: &str) {
    if !cfg!(test) {
        LOGGER
            .get_or_init(|| LineLogger::new(std::io::stderr(), LOG_QUEUE_CAPACITY))
            .send(line);
    }
}
/// Drain previously accepted lines, with one deadline covering queue space and sink acknowledgement.
/// A blocked sink can outlive the deadline; the caller never joins the writer thread.
pub fn flush_log(deadline: Duration) {
    if let Some(logger) = LOGGER.get() {
        logger.flush(deadline);
    }
}
struct LogDrain;
impl Drop for LogDrain {
    fn drop(&mut self) {
        flush_log(Duration::from_millis(250));
    }
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
type DriverTools = Arc<Mutex<BTreeMap<String, (u32, Value)>>>;
/// A Windows or macOS Guest's persistent cua-driver link (phase 2) and its cached
/// `tools/list`. Kept per VM with the worker pid it belongs to; a new worker gets a new one.
struct DriverLink {
    link: driver_link::Link,
    tools: Mutex<Option<Value>>,
}
type DriverLinks = Arc<Mutex<BTreeMap<String, (u32, Arc<DriverLink>)>>>;
/// The error agent input gets while a person controls the VM. The MCP adapter reads
/// the `person_in_control: <ms>` prefix back into an `ActionResult` refusal.
fn lease_refusal(wait: Duration) -> String {
    format!(
        "person_in_control: {} ms until agent input resumes; a person is using this VM and their input always wins",
        wait.as_millis()
    )
}
enum JobPost {
    Windows,
    GuestStreamStatus,
    GuestInfo {
        backend: Backend,
        guest: String,
        state: Arc<Mutex<Value>>,
        qemu_exec: bool,
        async_timeout: Arc<Mutex<GuestAsyncTimeout>>,
        async_generation: Option<u64>,
    },
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
    /// One text chunk: the Guest reply, error replies included, goes to `input::deliver_text`.
    GuestText,
    Screenshot {
        guest_wrapped: bool,
        baselines: Arc<Mutex<screen_wait::Baselines>>,
        vm: String,
        /// The scaled-JPEG form was asked for (QEMU console broker only); `None` is the PNG form.
        scaled: Option<screenshot_scaled::Scaled>,
    },
    /// A guest agent reply `{"id","result"}` returned as its result.
    GuestResult,
    /// A cua-driver reply frame: inside the guest agent's envelope on Apple Virtualization
    /// (`wrapped`), as the QEMU worker's `{"frame"}` result otherwise.
    Driver {
        wrapped: bool,
        finish: cua_bridge::Finish,
        cache: DriverTools,
        vm: String,
        pid: u32,
    },
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
    /// A cua-driver request over a Windows or macOS Guest's driver link.
    Driver {
        link: Arc<DriverLink>,
        request: cua_bridge::Request,
        scopes: cua_bridge::Scopes,
        desktop: cua_bridge::Desktop,
    },
    /// A Windows guest's command, file or desktop call over its SSH server.
    Windows {
        ssh: windows_guest::Ssh,
        call: WindowsCall,
    },
    CancelJobs {
        dir: PathBuf,
        response: Value,
        jobs: Vec<jobs::Job>,
        file_sessions: Vec<Value>,
    },
}
enum WindowsCall {
    Exec(Value),
    Desktop(String, Value),
    Put {
        name: String,
        bytes: Vec<u8>,
        sha256: String,
    },
    Get {
        name: String,
        owner: String,
        vm: String,
        dir: PathBuf,
        files: Arc<Mutex<windows_guest::Files>>,
    },
}
impl WindowsCall {
    fn run(self, ssh: &windows_guest::Ssh) -> Result<Value> {
        match self {
            Self::Exec(arguments) => windows_guest::exec(ssh, &arguments),
            Self::Desktop(method, arguments) => windows_guest::desktop(ssh, &method, &arguments),
            Self::Put {
                name,
                bytes,
                sha256,
            } => windows_guest::put(ssh, &name, &bytes, &sha256),
            Self::Get {
                name,
                owner,
                vm,
                dir,
                files,
            } => {
                let bytes = windows_guest::get(ssh, &name)?;
                files
                    .lock()
                    .unwrap()
                    .download_insert(&dir, &vm, &owner, &bytes)
            }
        }
    }
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
            Self::Driver {
                link,
                request,
                scopes,
                desktop,
            } => {
                let mut cache = link.tools.lock().unwrap().clone();
                let reply = cua_bridge::run(&link.link, request, scopes, desktop, &mut cache);
                *link.tools.lock().unwrap() = cache;
                reply
            }
            Self::Windows { ssh, call } => call.run(&ssh),
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
/// Original observation step of the MCP transport monitor.
const PEER_MONITOR_STEP: Duration = Duration::from_millis(50);
/// Upper bound on one blocking observation, so a kernel that does not wake a blocked poll for
/// some descriptor type still re-evaluates it, and the monitor still notices `stopped`.
const PEER_MONITOR_WAIT: Duration = Duration::from_millis(250);
/// `peer_disconnected`, but sleeping in the kernel for up to `limit` until a hang-up arrives.
/// Same descriptors, same read filter, same hang-up test. Descriptors that are merely readable
/// (a queued request, a regular file, a terminal) cannot block, so that case keeps the
/// original 50 ms step instead of spinning.
fn await_peer_disconnect(fds: &[std::os::fd::RawFd], limit: Duration) -> bool {
    let mut descriptors: Vec<libc::pollfd> = fds
        .iter()
        .map(|fd| libc::pollfd {
            fd: *fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let ready = unsafe {
        libc::poll(
            descriptors.as_mut_ptr(),
            descriptors.len() as libc::nfds_t,
            limit.as_millis().min(i32::MAX as u128) as i32,
        )
    };
    if ready == 0 {
        return false;
    }
    if ready > 0
        && descriptors
            .iter()
            .any(|fd| fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0)
    {
        return true;
    }
    thread::sleep(PEER_MONITOR_STEP.min(limit));
    false
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
/// Unicode text: validated and chunked on the host, delivered by the Guest service. The first
/// failure ends the call with exact delivery accounting; nothing is retried or dropped silently.
/// The text is never put into an error message or written anywhere by this function.
fn desktop_text(dir: &Path, mut arguments: Value, peer: &UnixStream) -> Result<Value> {
    let plan = input::plan_text(&arguments)?;
    // Mandatory: a Guest service without text_input would fence the execution transport.
    let info = call(dir, "guest_info", json!({"id":arguments["id"]}))?;
    let mut random = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    arguments["operation_id"] = json!(random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>());
    // True once the worker may hold this operation: a chunk was admitted, or its fate is unknown.
    let mut began = false;
    let result = input::deliver_text(&plan, &info, |index, chunk| {
        if peer_disconnected(&[peer.as_raw_fd()]) {
            return Ok(input::host_refusal("input client disconnected"));
        }
        arguments["begin"] = json!(index == 0);
        arguments["index"] = json!(index);
        arguments["text"] = json!(chunk);
        let reply = call_reply(dir, input::TEXT_STEP_METHOD, arguments.clone());
        arguments["text"] = Value::Null;
        thread::sleep(Duration::from_millis(20));
        match reply {
            Ok(Ok(reply)) => {
                began = true;
                Ok(reply)
            }
            // The daemon or the worker answered with a refusal: this chunk was not sent. A lost
            // worker answer is the one refusal that leaves the chunk's fate unknown.
            Ok(Err(message)) if !message.starts_with("backend response unavailable") => {
                Ok(input::host_refusal(&message))
            }
            Ok(Err(message)) => {
                began = true;
                bail!("{message}")
            }
            Err(error) => {
                began = true;
                Err(error)
            }
        }
    });
    if began {
        if let Err(error) = call(dir, "input_cleanup", arguments) {
            bail!(
                "input cleanup unconfirmed: {error}; text result: {}",
                match &result {
                    Ok(_) => "delivered".to_string(),
                    Err(error) => error.to_string(),
                }
            )
        }
    }
    Ok(result?)
}
fn desktop_input(
    dir: &Path,
    method: &str,
    mut arguments: Value,
    evdev: bool,
    peer: &UnixStream,
) -> Result<Value> {
    if method == "input_text" {
        return desktop_text(dir, arguments, peer);
    }
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

/// The optional `user`, `env` and `workdir` of an exec (`veynodevm exec -u -e -w`),
/// checked here and applied by the Linux Guest inside the container root.
fn exec_identity(args: &Value) -> Result<Vec<(&'static str, Value)>> {
    let mut fields = Vec::new();
    if let Some(user) = args.get("user").filter(|v| !v.is_null()) {
        let user = user.as_str().context("user must be a string")?;
        if user.is_empty() || user.len() > 256 || user.contains(['\0', '\n']) {
            bail!("user must be 1..256 bytes")
        }
        fields.push(("user", json!(user)));
    }
    if let Some(env) = args.get("env").filter(|v| !v.is_null()) {
        let env = env
            .as_array()
            .context("env must be an array of KEY=VALUE")?;
        if env.len() > 128
            || env.iter().any(|entry| {
                entry.as_str().is_none_or(|e| {
                    e.len() > 8192
                        || e.contains('\0')
                        || e.split_once('=').is_none_or(|(k, _)| k.is_empty())
                })
            })
        {
            bail!("env must be up to 128 KEY=VALUE strings")
        }
        if !env.is_empty() {
            fields.push(("env", json!(env)));
        }
    }
    if let Some(workdir) = args.get("workdir").filter(|v| !v.is_null()) {
        let workdir = workdir.as_str().context("workdir must be a string")?;
        if !workdir.starts_with('/') || workdir.len() > 4096 || workdir.contains('\0') {
            bail!("workdir must be an absolute path")
        }
        fields.push(("workdir", json!(workdir)));
    }
    Ok(fields)
}

fn exec_timeout_ms(arguments: &Value, maximum: u64) -> Result<u64> {
    let timeout = arguments
        .get("timeout_ms")
        .map(|value| value.as_u64().context("invalid timeout_ms"))
        .transpose()?
        .unwrap_or(5000);
    if !(1..=maximum).contains(&timeout) {
        bail!("timeout_ms must be 1..{maximum}")
    }
    Ok(timeout)
}
// Only this already nonce-bound, non-reconnecting transport has a Guest service
// lifetime matching Running. Vsock Guests can restart in the same VM process;
// their last info reply cannot safely authorize a later longer command.
fn advertised_async_timeout(info: &Value, qemu_exec: bool) -> u64 {
    if !qemu_exec
        || info["os"] != "linux"
        || info["transport"] != "virtio-serial-private-exec-v2"
        || info["transport_reconnect"] != false
        || info["execution_containment"] != "cgroup-v2-clone3-kill"
        || info["execution_identity"]["rootless"] != true
        || info["capabilities"]["exec"] != true
        || info["capabilities"]["async_exec"] != true
        || info["capabilities"]["descendant_containment"] != true
        || ["files", "shutdown", "desktop"]
            .iter()
            .any(|key| info["capabilities"][key] != false)
    {
        return SYNC_EXEC_MAX_TIMEOUT_MS;
    }
    info["max_async_timeout_ms"]
        .as_u64()
        .filter(|maximum| *maximum > 0)
        .map_or(SYNC_EXEC_MAX_TIMEOUT_MS, |maximum| {
            maximum.min(ASYNC_EXEC_MAX_TIMEOUT_MS)
        })
}
fn synchronous_exec(dir: &Path, arguments: Value, peer: &UnixStream) -> Result<Value> {
    // The shared asynchronous start route has a longer host ceiling. Do not
    // inherit that ceiling into the synchronous API or its 35-second waiter.
    exec_timeout_ms(&arguments, SYNC_EXEC_MAX_TIMEOUT_MS)?;
    // Every command uses the same registered Guest job path. Never fall back
    // to legacy uncancellable exec when an older Guest lacks asynchronous integration.
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
        let waiting = std::time::Instant::now();
        match self.receiver.recv_timeout(self.timeout) {
            Ok(mut reply) => {
                let backend_us = waiting.elapsed().as_micros() as u64;
                if matches!(self.post, Some(JobPost::GuestStreamStatus)) {
                    if let Some(error) = reply.get("error") {
                        if matches!(
                            error["code"].as_str(),
                            Some("unsupported" | "unknown_command")
                        ) || error.as_str() == Some("unknown command")
                        {
                            return Ok(
                                json!({"reachable":null,"reason":"worker_does_not_support_probe"}),
                            );
                        }
                        bail!("backend: {error}")
                    }
                    // The probe's nullable typed `error` is status data, not a Guest envelope.
                    return reply
                        .get_mut("result")
                        .map(Value::take)
                        .context("missing probe result");
                }
                if let Some(error) = reply.get("error") {
                    // A refusal of the scaled screenshot form keeps its fixed word beside the same text.
                    if let Some(JobPost::Screenshot {
                        scaled: Some(_), ..
                    }) = &self.post
                    {
                        if let Some(refusal) = screenshot_scaled::Refusal::from_name(
                            &reply[screenshot_scaled::REFUSAL_FIELD],
                        ) {
                            return Err(screenshot_scaled::Refused::new(
                                refusal,
                                format!("backend: {error}"),
                            )
                            .into());
                        }
                    }
                    bail!("backend: {error}")
                }
                // A Guest refusal of a text chunk carries delivery accounting that only
                // `input::deliver_text` interprets; every other Guest error ends the call here.
                if !matches!(self.post, Some(JobPost::GuestText)) {
                    if let Some(error) = reply["result"].get("error") {
                        bail!("guest: {error}")
                    }
                }
                let mut result = reply.get_mut("result").map_or(Value::Null, Value::take);
                match self.post {
                    Some(JobPost::GuestInfo {
                        backend,
                        guest,
                        state,
                        qemu_exec,
                        async_timeout,
                        async_generation,
                    }) => {
                        let mut policy = async_timeout.lock().unwrap();
                        if async_generation.is_some() && policy.generation == async_generation {
                            policy.maximum = advertised_async_timeout(&result["result"], qemu_exec);
                        }
                        drop(policy);
                        if backend == Backend::AppleVirtualization && guest == "macos" {
                            update_guest_metal_health(
                                &mut state.lock().unwrap(),
                                &result["result"],
                            );
                        }
                    }
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
                        scaled,
                    }) => {
                        let started = std::time::Instant::now();
                        result = normalize_screenshot(result, guest_wrapped)?;
                        let normalize_us = started.elapsed().as_micros() as u64;
                        SHOT_TIMING.set(Some(ShotTiming::from_reply(
                            &reply,
                            backend_us,
                            normalize_us,
                        )));
                        if let Some(scaled) = scaled {
                            verify_scaled_screenshot(&result, &scaled)?;
                        } else {
                            // A scaled control picture is not the full-size frame screen_wait compares against.
                            baselines.lock().unwrap().record(&vm, &result);
                        }
                    }
                    Some(JobPost::GuestResult) => {
                        result = result
                            .get("result")
                            .cloned()
                            .context("guest reply missing result")?;
                    }
                    Some(JobPost::Driver {
                        wrapped,
                        finish,
                        cache,
                        vm,
                        pid,
                    }) => {
                        let frame = if wrapped {
                            result["result"].get("frame")
                        } else {
                            result.get("frame")
                        }
                        .context("driver reply missing frame")?;
                        if finish.list {
                            if let Some(list) = frame.get("result") {
                                cache.lock().unwrap().insert(vm, (pid, list.clone()));
                            }
                        }
                        result = finish.result(frame)?;
                    }
                    Some(JobPost::GuestText) => {
                        // Counts and codes only: the Guest's per-key diagnostics never leave the daemon.
                        result = input::guest_text_summary(&result);
                    }
                    Some(JobPost::GuestStreamStatus) | None => {}
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
            .get_mut("result")
            .map(Value::take)
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
/// The reply to a request of the scaled form, after `normalize_screenshot`: the helper's JPEG, checked against
/// what was asked for, or a PNG from a helper without the form (already checked as any PNG reply; the caller
/// reads `mime_type`).
fn verify_scaled_screenshot(value: &Value, scaled: &screenshot_scaled::Scaled) -> Result<()> {
    if value["mime_type"] == "image/png" {
        return Ok(());
    }
    scaled.verify(value).map(drop)
}
pub(crate) fn screenshot_content(mut value: Value) -> Result<Value> {
    let data = value
        .as_object_mut()
        .and_then(|v| v.remove("data_base64"))
        .context("screenshot missing image data")?;
    Ok(
        json!({"content":[{"type":"image","mimeType":value["mime_type"],"data":data},{"type":"text","text":serde_json::to_string(&value)?}],"isError":false}),
    )
}
type FileSessionRoutes = BTreeMap<String, (std::time::Instant, BTreeSet<String>)>;

/// Test-input diagnostics only: never create or repair the operator's logging directory.
///
/// Returns the canonical path, which is the only path ever handed to the worker. The worker re-resolves the path at
/// open time, so every ancestor must be unable to be renamed or replaced by another local user: owned by root or the
/// current user and not group/other writable (a sticky bit is accepted). The directory itself must be 0700, owned by
/// the current user and carry no extended ACL: an inherited ACL entry would otherwise let another account read or
/// alter the key log although the mode says 0700.
pub fn validate_worker_key_event_log_dir(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("worker key-event log directory must be absolute")
    }
    // Strip trailing separators / `.` so lstat cannot follow a final symlink that way.
    let literal: PathBuf = path.components().collect();
    let metadata = fs::symlink_metadata(&literal).with_context(|| {
        format!(
            "worker key-event log directory must exist: {}",
            literal.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "worker key-event log directory must be a directory, not a symlink: {}",
            literal.display()
        )
    }
    let canonical = fs::canonicalize(&literal).with_context(|| {
        format!(
            "worker key-event log directory cannot be resolved: {}",
            literal.display()
        )
    })?;
    let metadata = fs::symlink_metadata(&canonical)?;
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid {
        bail!(
            "worker key-event log directory must be owned by the current user: {}",
            canonical.display()
        )
    }
    if metadata.mode() & 0o7777 != 0o700 {
        bail!(
            "worker key-event log directory must have mode 0700: {}",
            canonical.display()
        )
    }
    if has_extended_acl(&canonical)? {
        bail!(
            "worker key-event log directory must not carry an extended ACL: {}",
            canonical.display()
        )
    }
    for ancestor in canonical.ancestors().skip(1) {
        let meta = fs::symlink_metadata(ancestor)
            .with_context(|| format!("cannot inspect {}", ancestor.display()))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            bail!(
                "worker key-event log path has a non-directory ancestor: {}",
                ancestor.display()
            )
        }
        if meta.uid() != 0 && meta.uid() != euid {
            bail!(
                "worker key-event log path has an ancestor owned by another user: {}",
                ancestor.display()
            )
        }
        let mode = meta.mode();
        if mode & 0o022 != 0 && mode & 0o1000 == 0 {
            bail!(
                "worker key-event log path has a group/other-writable ancestor without the sticky bit: {}",
                ancestor.display()
            )
        }
    }
    Ok(canonical)
}

/// True when the path carries an extended ACL (macOS `acl_get_file(ACL_TYPE_EXTENDED)`: NULL with ENOENT when there is
/// none). ACLs are not visible through `listxattr`. Other targets have no such ACL model here and report false (this
/// product runs the macOS worker only).
#[cfg(target_os = "macos")]
fn has_extended_acl(path: &Path) -> Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    extern "C" {
        fn acl_get_file(path: *const libc::c_char, acl_type: libc::c_uint) -> *mut libc::c_void;
        fn acl_free(object: *mut libc::c_void) -> libc::c_int;
    }
    const ACL_TYPE_EXTENDED: libc::c_uint = 0x0000_0100;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let acl = unsafe { acl_get_file(c.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(false),
            _ => Err(error).with_context(|| format!("cannot read the ACL of {}", path.display())),
        };
    }
    unsafe { acl_free(acl) };
    Ok(true)
}
#[cfg(not(target_os = "macos"))]
fn has_extended_acl(_path: &Path) -> Result<bool> {
    Ok(false)
}

fn worker_start_args(
    backend: Backend,
    config_path: &Path,
    log_dir: Option<&Path>,
    id: &str,
    unix_seconds: u64,
    worker_instance: &str,
) -> (Vec<OsString>, Option<PathBuf>) {
    let mut args = Vec::new();
    let log_path = if backend == Backend::AppleVirtualization {
        args.push("run".into());
        log_dir.map(|dir| {
            let prefix: String = worker_instance.chars().take(8).collect();
            dir.join(format!("key-events-{id}-{unix_seconds}-{prefix}.jsonl"))
        })
    } else {
        None
    };
    args.push("--config".into());
    args.push(config_path.as_os_str().into());
    if let Some(path) = &log_path {
        args.push("--log-key-events".into());
        args.push(path.as_os_str().into());
    }
    (args, log_path)
}

#[cfg(test)]
#[path = "../tests/worker_key_event_log/unit.rs"]
mod worker_key_event_log_tests;

struct Manager {
    snapshot_ops: snapshot_control::Operations,
    restore_starts: Arc<Mutex<snapshot_restore::Starts>>,
    memory: MemoryBudget,
    dir: PathBuf,
    backend: PathBuf,
    qemu: Option<QemuRegistration>,
    worker_key_event_log_dir: Option<PathBuf>,
    worker_key_event_logs: BTreeMap<String, (String, PathBuf)>,
    qemu_window: Value,
    native_connections: BTreeMap<String, (String, String)>,
    configs: BTreeMap<String, Config>,
    /// Registered VMs that failed the creation checks at registry load, with the reason. Such an entry stays in the
    /// registry as it is, is listed as `invalid` and is refused by `start`; nothing here ever removes it.
    invalid: BTreeMap<String, String>,
    jobs: Arc<Mutex<jobs::Jobs>>,
    file_sessions: FileSessionRoutes,
    /// Staged transfers of Windows guests, which go over SSH, not a worker.
    windows_files: Arc<Mutex<windows_guest::Files>>,
    /// Each VM's cua-driver `tools/list` result, with the worker pid it came from.
    driver_tools: DriverTools,
    driver_links: DriverLinks,
    /// Who controls each VM's input: a person at a viewer, or the agent (`control_lease`).
    leases: Arc<control_lease::Leases>,
    baselines: Arc<Mutex<screen_wait::Baselines>>,
    running: BTreeMap<String, Running>,
    /// Lifecycle events for MCP adapters (`events`).
    events: Arc<events::Log>,
    /// The daemon's background exit monitor, parked while no worker is alive.
    exit_monitor: Option<thread::Thread>,
}
/// Background reaping cadence while at least one worker is alive (unchanged).
const EXIT_MONITOR_LIVE: Duration = Duration::from_millis(100);
/// With no live worker there is nothing to observe: stay parked until `Manager::started`
/// wakes the monitor. The bound only lets the thread notice a dropped manager.
const EXIT_MONITOR_IDLE: Duration = Duration::from_secs(5);
#[cfg(test)]
static EXIT_MONITOR_PASSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// One background observation of every worker; returns how long the monitor may park.
fn exit_monitor_pass(manager: &Mutex<Manager>) -> Duration {
    #[cfg(test)]
    EXIT_MONITOR_PASSES.fetch_add(1, Ordering::Relaxed);
    let mut live = false;
    let manager = &mut *manager.lock().unwrap();
    for (id, runtime) in manager.running.iter_mut() {
        // An observation error is not proof of exit; keep the live cadence.
        if !matches!(runtime.poll_exit(), Ok(Some(_))) {
            live = true;
        }
        runtime.publish_exit(id, &manager.events);
    }
    if live {
        EXIT_MONITOR_LIVE
    } else {
        EXIT_MONITOR_IDLE
    }
}
fn spawn_exit_monitor(manager: &Arc<Mutex<Manager>>) {
    let monitor = Arc::downgrade(manager);
    let handle = thread::spawn(move || {
        while let Some(manager) = monitor.upgrade() {
            let park = exit_monitor_pass(&manager);
            drop(manager);
            // An unpark delivered at any point after the pass above is retained as a token,
            // so a worker registered while this thread was not yet parked still wakes it.
            thread::park_timeout(park);
        }
    });
    manager.lock().unwrap().exit_monitor = Some(handle.thread().clone());
}
impl Manager {
    /// One cua-driver request (`cua_bridge`). Refusals and the cached tool list are
    /// answered here; anything else goes to the worker, and its reply is awaited off the
    /// manager lock: Apple Virtualization through the guest agent's `driver_rpc` method
    /// (vsock), QEMU through the worker's `veynode.driver` port. Windows and macOS Guests
    /// use a persistent link instead (`driver_link`): the driver's own socket proxy over
    /// SSH on Windows, the embedded driver over SSH in a macOS container.
    fn driver_rpc(
        &mut self,
        id: &str,
        args: &Value,
        deferred: &mut Option<Completion>,
    ) -> Result<Value> {
        let config = &self.configs[id];
        let linked = match (config.backend, config.guest.as_str()) {
            (Backend::QemuHvf, "windows") | (Backend::AppleVirtualization, "macos") => true,
            (_, "linux") => false,
            _ => bail!("unsupported: the cua-driver bridge serves Linux Guests, Windows machines and macOS containers"),
        };
        let wrapped = config.backend == Backend::AppleVirtualization;
        let request = cua_bridge::Request::from_args(args)?;
        if let cua_bridge::Request::Call { tool, .. } = &request {
            let acts = matches!(
                cua_bridge::scope(tool),
                Some(cua_bridge::Scope::Input | cua_bridge::Scope::Browser)
            );
            if acts {
                if let Err(wait) = self.leases.acquire(id, std::time::Instant::now()) {
                    return Ok(cua_bridge::refusal(
                        "person_in_control",
                        &lease_refusal(wait),
                    ));
                }
            }
        }
        let scopes = cua_bridge::Scopes::from_value(&args["scopes"]);
        let desktop = cua_bridge::Desktop {
            person_attached: self.native_connections.contains_key(id) || self.leases.watched(id),
        };
        let run = self.running.get_mut(id).context("VM process not active")?;
        if run.poll_exit()?.is_some() {
            bail!("VM process has exited")
        }
        let pid = run.child.id();
        if linked {
            let link = self.driver_link(id, pid)?;
            *deferred = Some(Completion::Driver {
                link,
                request,
                scopes,
                desktop,
            });
            return Ok(Value::Null);
        }
        let cached = self
            .driver_tools
            .lock()
            .unwrap()
            .get(id)
            .filter(|(from, _)| *from == pid)
            .map(|(_, list)| list.clone());
        let outgoing = match cua_bridge::prepare(request, scopes, desktop, cached.as_ref()) {
            cua_bridge::Prepared::Reply(reply) => return Ok(reply),
            cua_bridge::Prepared::Send(outgoing) => outgoing,
        };
        let number = run.next;
        run.next += 1;
        let mut frame = outgoing.frame;
        frame["id"] = json!(number);
        let deadline_ms = outgoing.deadline.as_millis() as u64;
        let command = if wrapped {
            json!({"id":number,"method":"guest_request","request":{"id":number.to_string(),"method":"driver_rpc","arguments":{"frame":frame,"deadline_ms":deadline_ms}}})
        } else {
            json!({"id":number,"method":"driver_rpc","frame":frame,"deadline_ms":deadline_ms})
        };
        let (tx, rx) = mpsc::channel();
        run.pending.lock().unwrap().insert(
            number,
            PendingReply {
                sender: tx,
                limit: response_limit("driver_rpc"),
            },
        );
        if let Err(e) = write_json(&mut run.stdin, &command) {
            run.pending.lock().unwrap().remove(&number);
            return Err(e);
        }
        *deferred = Some(Completion::Backend(Pending {
            receiver: rx,
            number,
            pending: run.pending.clone(),
            // Past the worker's own deadline, so its timeout reply is the one seen.
            timeout: outgoing.deadline + Duration::from_secs(5),
            post: Some(JobPost::Driver {
                wrapped,
                finish: outgoing.finish,
                cache: self.driver_tools.clone(),
                vm: id.into(),
                pid,
            }),
        }));
        Ok(Value::Null)
    }
    /// `control_lease {action}`: `status` (the default) reports who controls input;
    /// `release` gives the agent's lease back until its next input.
    fn control_lease(&mut self, id: &str, args: &Value) -> Result<Value> {
        let now = std::time::Instant::now();
        match args["action"].as_str().unwrap_or("status") {
            "status" => {}
            "release" => self.leases.release(id, now),
            other => bail!("control_lease action must be status or release, not {other}"),
        }
        Ok(self.leases.status(id, now))
    }
    /// The VM's driver link for this worker, made on first use; the driver is installed
    /// and started when the link first connects, off the manager lock.
    fn driver_link(&mut self, id: &str, pid: u32) -> Result<Arc<DriverLink>> {
        let mut links = self.driver_links.lock().unwrap();
        if let Some((from, link)) = links.get(id) {
            if *from == pid {
                return Ok(link.clone());
            }
        }
        let link = if self.configs[id].guest == "windows" {
            windows_driver::link(windows_guest::Ssh::locate(&self.dir, &self.configs[id])?)
        } else {
            container_tools::macos_driver(&self.dir, id)
        };
        let link = Arc::new(DriverLink {
            link,
            tools: Mutex::new(None),
        });
        // Replacing an old worker's link drops it, which closes its connection.
        links.insert(id.into(), (pid, link.clone()));
        Ok(link)
    }
    /// A deferred call to a running Windows guest over its SSH server.
    fn windows_call(&mut self, id: &str, call: WindowsCall) -> Result<Completion> {
        let run = self.running.get_mut(id).context("VM process not active")?;
        if run.poll_exit()?.is_some() {
            bail!("VM process has exited")
        }
        let ssh = windows_guest::Ssh::locate(&self.dir, &self.configs[id])?;
        Ok(Completion::Windows { ssh, call })
    }
    /// File tools of a Windows guest: chunks are staged here, and the file
    /// crosses SSH once (commit, download begin).
    fn windows_file(
        &mut self,
        id: &str,
        method: &str,
        args: &Value,
        deferred: &mut Option<Completion>,
    ) -> Result<Value> {
        let owner = args["owner"].as_str().unwrap_or_default().to_owned();
        let files = self.windows_files.clone();
        let mut store = files.lock().unwrap();
        match method {
            "file_session_end" => Ok(store.session_end(id, &owner)),
            "file_upload_begin" => store.upload_begin(&self.dir, id, args),
            "file_upload_write" => store.upload_write(id, args),
            "file_download_read" => store.download_read(id, args),
            "file_cancel" => store.cancel(id, args),
            "file_upload_commit" => {
                let (name, bytes, sha256) = store.upload_take(id, args)?;
                drop(store);
                *deferred = Some(self.windows_call(
                    id,
                    WindowsCall::Put {
                        name,
                        bytes,
                        sha256,
                    },
                )?);
                Ok(Value::Null)
            }
            "file_download_begin" => {
                drop(store);
                let name =
                    windows_guest::file_name(args["path"].as_str().context("missing path")?)?
                        .to_owned();
                *deferred = Some(self.windows_call(
                    id,
                    WindowsCall::Get {
                        name,
                        owner,
                        vm: id.into(),
                        dir: self.dir.clone(),
                        files,
                    },
                )?);
                Ok(Value::Null)
            }
            _ => bail!("unknown file method"),
        }
    }
    /// Registers a freshly started worker and resumes background exit observation at once.
    fn started(&mut self, id: String, running: Running) {
        if let Some(previous) = self.running.get(&id) {
            previous.publish_exit(&id, &self.events);
        }
        self.events.started(&id);
        self.running.insert(id, running);
        if let Some(monitor) = &self.exit_monitor {
            monitor.unpark();
        }
    }
    fn load(dir: PathBuf, backend: PathBuf, memory: MemoryBudget) -> Result<Self> {
        check_registry_schema(&dir)?;
        let registry = dir.join("registry.json");
        let configs = read_registry(&registry)?;
        for (id, cfg) in &configs {
            if *id != cfg.id {
                bail!("registry identity mismatch")
            }
        }
        let snapshot_ops = snapshot_control::Operations::open(&dir)?;
        Ok(Self {
            snapshot_ops,
            restore_starts: Arc::new(Mutex::new(snapshot_restore::Starts::default())),
            memory,
            dir,
            backend,
            qemu: None,
            worker_key_event_log_dir: None,
            worker_key_event_logs: BTreeMap::new(),
            qemu_window: json!({"native_window":false,"display_only":true,"input_enabled":false}),
            native_connections: BTreeMap::new(),
            configs,
            invalid: BTreeMap::new(),
            jobs: Arc::new(Mutex::new(jobs::Jobs::default())),
            file_sessions: BTreeMap::new(),
            windows_files: Arc::default(),
            driver_tools: Arc::default(),
            driver_links: Arc::default(),
            leases: Arc::default(),
            baselines: Arc::new(Mutex::new(screen_wait::Baselines::default())),
            running: BTreeMap::new(),
            events: Arc::new(events::Log::default()),
            exit_monitor: None,
        })
    }
    /// Everything `create` requires of a configuration beyond parsing it. `create`, `start` and the registry load use
    /// this one function, so a definition is judged the same way wherever it enters.
    fn admissible(&self, config: &Config) -> Result<()> {
        config.validate()?;
        if config.backend == Backend::QemuHvf {
            backend::require_registration(&self.qemu)?
                .require_profile(config.qemu_graphics_profile)?;
            if config.window && !backend::window_supported(&self.qemu_window) {
                bail!("native display-only window unavailable for registered QEMU helper")
            }
        }
        Ok(())
    }
    /// Registry load, once the daemon's QEMU registration is known: every entry goes through the creation checks. A
    /// failure is recorded per VM and stops neither the daemon nor the other VMs; the registry file is not written.
    /// The verdict holds until the next daemon start.
    fn validate_registry(&mut self) {
        self.invalid = self
            .configs
            .iter()
            .filter_map(|(id, config)| {
                let reason = format!("{:#}", validate_new_vm_id(id).and_then(|()| self.admissible(config)).err()?);
                log_line(&format!("vm-control: registered VM {id} failed validation at registry load and cannot be started: {reason}"));
                Some((id.clone(), reason))
            })
            .collect();
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
    /// `log_read {id, stream, max_bytes}`: the redacted tail of the VM's serial
    /// console log or its worker's diagnostic log (see `events::read_tail`).
    fn log_read(&self, id: &str, args: &Value) -> Result<Value> {
        let max_bytes = args
            .get("max_bytes")
            .map_or(Some(events::MAX_LOG_BYTES), Value::as_u64)
            .context("max_bytes must be an integer")?;
        let stream = args["stream"].as_str().unwrap_or("");
        let runtime = self
            .running
            .get(id)
            .map(|r| r.state.lock().unwrap().clone());
        let path = match stream {
            "serial" => self.configs[id]
                .serial_log
                .clone()
                .or_else(|| runtime.as_ref()?["serial_log"].as_str().map(PathBuf::from)),
            "worker" => runtime
                .as_ref()
                .and_then(|r| r["stderr_log"].as_str().map(PathBuf::from)),
            _ => bail!("stream must be serial or worker"),
        };
        let Some(path) = path else {
            bail!("unavailable: this VM has no {stream} log (serial needs serial_log; worker logs are QEMU only and appear once its worker has started)")
        };
        let mut result = events::read_tail(&path, max_bytes, stream == "worker")?;
        result["stream"] = json!(stream);
        Ok(result)
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
        result["key_event_log"] = json!({"enabled":false});
        if config.backend == Backend::AppleVirtualization && exit.is_none() {
            if let Some((instance, path)) = self.worker_key_event_logs.get(id) {
                if self.running.get(id).is_some_and(|runtime| {
                    runtime.bridge.lock().unwrap().worker_instance == *instance
                }) {
                    result["key_event_log"] = json!({"enabled":true,"path":path});
                }
            }
        }
        result["capabilities"]["product_capabilities"] =
            product_capabilities(config.backend, &config.guest);
        if config.backend == Backend::QemuHvf && !config.qemu_guest_exec {
            // This VM cannot reach the Guest service's execution transport.
            result["capabilities"]["product_capabilities"]["unicode_text_input"] = json!({"status":"unsupported","reason":"this VM is registered without qemu_guest_exec; vm_input_text needs the Guest execution transport and is refused"});
        }
        // The same narrowing by this VM's registration and this daemon's registered QEMU helpers.
        let mut features = control_features(config.backend, &config.guest);
        if config.backend == Backend::QemuHvf {
            if config.guest == "linux" {
                features["gpu_3d"] = qemu_gpu_3d(config.qemu_graphics_profile);
            }
            let mut narrow = |names: &[&str], reason: &str| {
                for name in names {
                    features[*name] = json!({"status":"unsupported","reason":reason});
                }
            };
            if !config.qemu_guest_exec {
                narrow(&["command_execution", "async_jobs", "unicode_text_input"], "this VM is registered without qemu_guest_exec; the observer-only transport carries no execution or text input and the calls are refused");
            }
            if self.qemu.as_ref().is_none_or(|q| !q.has_screenshot()) {
                narrow(
                    &["screenshot"],
                    "this daemon has no registered QEMU capture; vm_screenshot is refused",
                );
            }
            if self
                .qemu
                .as_ref()
                .is_none_or(|q| q.console_broker.is_none())
            {
                narrow(&["screenshot_scaled_jpeg"], "this daemon has no registered QEMU console broker; the scaled JPEG screenshot form is refused");
                narrow(&["pointer_key_input", "unicode_text_input"], "this daemon has no registered QEMU persistent console; input calls are refused");
            }
            if self.qemu.is_none() {
                narrow(&["gpu_3d"], "this daemon has no registered QEMU runtime; no Guest 3D acceleration is available");
            } else if config.qemu_graphics_profile.venus()
                && self.qemu.as_ref().is_none_or(|q| q.venus_runtime.is_none())
            {
                narrow(&["gpu_3d"], "this VM selects experimental Venus but the daemon has no registered Venus runtime; no Guest 3D acceleration is available");
            }
        }
        if config.backend == Backend::AppleVirtualization && config.guest == "linux" {
            if config.machine_identity_dir.is_none() {
                features["snapshot"] = json!({"status":"unsupported","reason":"This VM has no explicit persistent Linux machine_identity_dir. Snapshot capture refuses it; existing VMs are never silently assigned a new snapshot identity."});
            }
            result["capabilities"]["snapshot_kinds"] = json!({
                "full_state":features["snapshot"],
                "disk_rollback":{"named_create":false,"restore":false,"automatic_restore_protection":true,"reason":"Full-state restore retains an independently verified protective disk copy; disk-only rollback is not exposed as full-state restore."},
                "suspend":{"save":false,"resume":false,"reason":"A resumable named full-state snapshot is implemented separately; a standalone suspend lifecycle is not exposed."},
                "operation_fenced":self.snapshot_ops.active_vm(id)
            });
        }
        result["capabilities"]["control_features"] = features;
        // Added key, macOS Guests only: Linux and Windows status objects keep their exact previous key set.
        let limits = known_limits(config.backend, &config.guest);
        if limits.as_array().is_some_and(|limits| !limits.is_empty()) {
            result["capabilities"]["known_limits"] = limits;
        }
        if config.backend == Backend::AppleVirtualization && config.guest == "macos" {
            result["capabilities"]["screenshot"] = json!(true);
            result["capabilities"]["screenshot_scaled_jpeg"] = json!(false);
            result["capabilities"]["screenshot_availability"]=json!("requires running macOS Guest service --allow-observe and Guest Screen Recording permission; query guest_info for current availability");
        }
        if config.backend == Backend::QemuHvf {
            result["capabilities"]["graphics_profile"] = json!({"configured":config.qemu_graphics_profile,"local_runtime_registered":self.qemu.as_ref().is_some_and(|q|q.venus_runtime.is_some()),"host_supported":self.running.get(id).is_some_and(|r|r.state.lock().unwrap()["graphics_profile"]["host_supported"]==true),"guest_verified":false,"scope":"explicit experimental host profile; actual Guest application Vulkan driver/pixels require independent validation"});
            if !config.qemu_graphics_profile.venus() {
                result["capabilities"]["graphics_profile"]["local_runtime_registered"] =
                    json!(self.qemu.is_some());
                result["capabilities"]["graphics_profile"]["scope"] = json!(if config
                    .qemu_graphics_profile
                    .requires_gl()
                {
                    "configured Virgl profile; Guest driver, rendered pixels and performance require independent validation"
                } else {
                    "explicit unaccelerated virtio-2d console; no Guest 3D or snapshot acceptance implied"
                });
            }
            if config.qemu_graphics_profile.venus() {
                result["capabilities"]["graphics_profile"]["host_api_constraints"] =
                    qemu_graphics::host_api_constraints(config.qemu_graphics_profile);
            }
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
            result["capabilities"]["input_scope"] = json!(if config.qemu_guest_exec {
                "absolute click/drag/discrete wheel/explicit qcode; Unicode text only through the Guest service (vm_input_text, see product_capabilities.unicode_text_input); snapshot <=5s and matching generation required; no layout translation for keys"
            } else {
                "absolute click/drag/discrete wheel/explicit qcode only; snapshot <=5s and matching generation required; no text/layout translation (vm_input_text requires qemu_guest_exec)"
            });
            result["capabilities"]["screenshot"] =
                json!(self.qemu.as_ref().is_some_and(|q| q.has_screenshot()));
            // The scaled-JPEG request form exists only behind the console broker; whether the running helper
            // has it is known only from its reply (an older helper answers with a PNG).
            result["capabilities"]["screenshot_scaled_jpeg"] = json!(self
                .qemu
                .as_ref()
                .is_some_and(|q| q.console_broker.is_some()));
        }
        if config.backend == Backend::QemuHvf {
            result["capabilities"]["native_window"] =
                json!(backend::window_supported(&self.qemu_window));
            result["capabilities"]["native_window_details"] = self.qemu_window.clone();
        }
        if let Some(runtime) = self.running.get_mut(id) {
            runtime.publish_exit(id, &self.events);
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
        if let Some(reason) = self.invalid.get(id) {
            // Never running: `start` refuses it and nothing was running when the registry was loaded.
            result["state"] = json!("invalid");
            result["invalid_reason"] = json!(reason);
        }
        Ok(result)
    }
    fn dispatch(&mut self, request: Value, deferred: &mut Option<Completion>) -> Result<Value> {
        let method = request["method"].as_str().context("missing method")?;
        let args = &request["arguments"];
        if let Some(value) = self.snapshot_restore_dispatch(method, args, deferred)? {
            return Ok(value);
        }
        if let Some(value) = self.snapshot_dispatch(method, args, deferred)? {
            return Ok(value);
        }
        if let Some(id) = args["id"].as_str() {
            if self.snapshot_ops.active_vm(id)
                && !matches!(
                    method,
                    "status" | "capabilities" | "snapshot_restore_start" | "log_read"
                )
            {
                bail!("VM is fenced by an active or unresolved snapshot operation")
            }
        }
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
        if method == "events" {
            return self.events.since(args);
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
            let mut report = json!({"resource_admission":{"physical_memory_mib":self.memory.physical_memory_mib,"vm_memory_budget_mib":self.memory.vm_memory_budget_mib,"active_assigned_memory_mib":assigned,"remaining_budget_mib":self.memory.vm_memory_budget_mib.saturating_sub(assigned),"explicitly_configured":self.memory.explicitly_configured,"scope":"assigned guest RAM in this daemon; excludes VM overhead, other daemons and unrelated host workloads; not a host pressure guarantee"},"backends":{"apple-virtualization":backend::capabilities(Backend::AppleVirtualization),"qemu-hvf":{"registered":self.qemu.is_some(),"capabilities":qemu_capabilities}},"backend":"apple-virtualization","guests":["linux","macos"],"windows":false,"window_enumeration":"macOS Guest --allow-observe with existing Screen Recording permission and eligible console; no titles/AX tree/input authority; other backends unsupported","lifecycle":true,"native_window":true,"screenshot":true,"screenshot_requirements":"macOS Guest --allow-observe and existing Guest Screen Recording permission, registered QEMU capture, or an Apple Virtualization Linux desktop container","input":true,"input_requirements":"registered QEMU persistent console, macOS Guest --allow-observe --allow-input with existing Guest Screen Recording/Accessibility permissions, or an Apple Virtualization Linux desktop container; independent MCP input permission and a recent matching screenshot","screen_wait":"every screenshot backend; changed regions on Apple Virtualization Linux desktops","screen_text":"every screenshot backend; host Apple Vision through the registered vm-vision helper","desktop_windows":"Apple Virtualization Linux desktop containers through the guest agent (X11/EWMH), veynodevm macOS containers through the Accessibility API over SSH, and Windows machines through Win32 in the desktop user's session over SSH: state and apps observe; window actions and app launch need input permission","accessibility":"Apple Virtualization Linux desktop containers whose image runs at-spi2 (AT-SPI over the desktop's accessibility bus), veynodevm macOS containers (the macOS Accessibility API over SSH, granted at first boot) and Windows machines (UI Automation over SSH): tree and find observe; element actions need input permission","command_execution":"Apple Linux backend requires enabled async Guest jobs; macOS legacy exec is not exposed; QEMU requires explicit qemu_guest_exec v2 and rootless cgroup-v2 Guest service; Windows machines run vm_exec over their OpenSSH Server (machine ssh-key)","async_commands":"Apple Linux backend requires exec_start/exec_poll/exec_cancel support; jobs are bound to their VM, run under a guest timeout and are killed with their process group when they time out, are cancelled or their MCP session ends; QEMU default observer transport has no command capability","file_transfer":"Apple backend requires enabled Guest files integration: Linux dedicated non-root files account or macOS opt-in same-UID private root; QEMU private serial transports have no file capability, Windows machines transfer over their OpenSSH Server; query vm_guest_info for current capabilities and file_policy; single filenames, 8MiB max","remote_desktop":false,"guest_integration":"Apple backend uses enabled Linux/macOS host-only vsock service; QEMU private virtio-serial defaults to observer-only v1; explicit qemu_guest_exec selects v2 requiring rootless cgroup-v2 Guest execution; query vm_guest_info for actual availability and capabilities","guest_shutdown":"Apple Linux integration requires explicit shutdown enablement; QEMU guest-service shutdown is unsupported and QMP power-button requests remain separate; request acceptance is not shutdown completion","gpu":"requires guest measurements; no performance claim"});
            // Added fields only: every pre-existing key keeps its name, type and meaning.
            report["backends"]["apple-virtualization"]["product_capabilities"] = json!({
                "macos":product_capabilities(Backend::AppleVirtualization, "macos"),
                "linux":product_capabilities(Backend::AppleVirtualization, "linux"),
            });
            report["backends"]["qemu-hvf"]["product_capabilities"] = json!({
                "linux":product_capabilities(Backend::QemuHvf, "linux"),
                "windows":product_capabilities(Backend::QemuHvf, "windows"),
            });
            report["backends"]["apple-virtualization"]["control_features"] = json!({
                "macos":control_features(Backend::AppleVirtualization, "macos"),
                "linux":control_features(Backend::AppleVirtualization, "linux"),
            });
            // Added key under the Apple backend only, and only for the macOS Guest.
            report["backends"]["apple-virtualization"]["known_limits"] = json!({
                "macos":known_limits(Backend::AppleVirtualization, "macos"),
            });
            report["backends"]["qemu-hvf"]["control_features"] = json!({
                "linux":control_features(Backend::QemuHvf, "linux"),
                "macos":control_features(Backend::QemuHvf, "macos"),
                "windows":control_features(Backend::QemuHvf, "windows"),
            });
            return Ok(report);
        }
        if method == "create" {
            let config: Config = serde_json::from_value(args.clone())?;
            validate_new_vm_id(&config.id)?;
            self.admissible(&config)?;
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
        if method == "log_read" {
            return self.log_read(id, args);
        }
        if method == "control_lease" {
            return self.control_lease(id, args);
        }
        if method.starts_with("input_") || desktop_tools::acts(method) {
            if let Err(wait) = self.leases.acquire(id, std::time::Instant::now()) {
                bail!("{}", lease_refusal(wait))
            }
        }
        if method == "driver_rpc" {
            return self.driver_rpc(id, args, deferred);
        }
        if method == "delete" {
            if args["confirm"].as_str() != Some(id) {
                bail!("delete requires confirm equal to the VM id")
            }
            if let Some(run) = self.running.get_mut(id) {
                if run.poll_exit()?.is_none() {
                    bail!("VM is running; stop it before delete")
                }
                run.publish_exit(id, &self.events);
            }
            let mut next = self.configs.clone();
            let removed = next.remove(id).context("VM not found")?;
            atomic_json(&self.dir.join("registry.json"), &next)?;
            self.configs = next;
            self.running.remove(id);
            self.driver_links.lock().unwrap().remove(id);
            self.leases.forget(id);
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
        if method == "guest_stream_status" {
            if self.configs[id].backend != Backend::AppleVirtualization
                || self.configs[id].guest != "macos"
            {
                bail!("unsupported: stream probe requires a macOS Guest on Apple Virtualization")
            }
            if args
                .as_object()
                .is_none_or(|a| a.len() != 1 || !a.contains_key("id"))
            {
                bail!("guest_stream_status accepts only VM id")
            }
        }
        // Windows guests run commands and files over their SSH server below.
        let windows_ssh = self.configs[id].guest == "windows"
            && (method.starts_with("exec") || method.starts_with("file_"));
        if self.configs[id].backend == Backend::QemuHvf
            && !windows_ssh
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
            && (method.starts_with("exec")
                || method == "input_step"
                || method == input::TEXT_STEP_METHOD
                || desktop_tools::acts(method))
        {
            bail!("VM execution is quiescing for shutdown")
        }
        let windows_vm =
            self.configs[id].backend == Backend::QemuHvf && self.configs[id].guest == "windows";
        if windows_vm && ["exec_start", "exec_poll", "exec_cancel"].contains(&method) {
            bail!("unsupported: Windows guests run commands with vm_exec only (over SSH, up to 30 s); there are no asynchronous jobs")
        }
        if method == "exec" && windows_vm {
            exec_timeout_ms(args, SYNC_EXEC_MAX_TIMEOUT_MS)?;
            if !exec_identity(args)?.is_empty() {
                bail!("unsupported: user, env and workdir apply to Linux container commands")
            }
            let argv = args["argv"].as_array().map_or(0, Vec::len);
            if !(1..=128).contains(&argv) {
                bail!("argv must have 1..128 items")
            }
            *deferred = Some(self.windows_call(id, WindowsCall::Exec(args.clone()))?);
            return Ok(Value::Null);
        }
        if method == "exec" {
            exec_timeout_ms(args, SYNC_EXEC_MAX_TIMEOUT_MS)?;
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
            // Before any console check: the refusal depends only on the registered VM.
            if (method == "input_text" || method == input::TEXT_STEP_METHOD)
                && (self.configs[id].backend != Backend::QemuHvf
                    || self.configs[id].guest != "linux"
                    || !self.configs[id].qemu_guest_exec)
            {
                bail!("{}", input::TEXT_UNSUPPORTED_VM)
            }
            if !guest_input
                && (self.configs[id].backend != Backend::QemuHvf
                    || self
                        .qemu
                        .as_ref()
                        .is_none_or(|q| q.console_broker.is_none()))
            {
                bail!("unsupported: desktop input requires QEMU persistent console, a macOS Guest input service or an Apple Virtualization Linux desktop")
            }
            if method == "input_text" {
                input::plan_text(args)?;
                *deferred = Some(Completion::Input {
                    dir: self.dir.clone(),
                    arguments: args.clone(),
                    method: method.into(),
                    evdev: false,
                });
                return Ok(Value::Null);
            }
            if ["input_click", "input_drag", "input_scroll", "input_key"].contains(&method) {
                let arguments = input::source_pixel_arguments(method, args)?.into_owned();
                input::plan(method, &arguments)?;
                *deferred = Some(Completion::Input {
                    dir: self.dir.clone(),
                    arguments,
                    method: method.into(),
                    evdev: apple_guest && self.configs[id].guest == "linux",
                });
                return Ok(Value::Null);
            }
        }
        let mut desktop_arguments = None;
        let windows_vm =
            self.configs[id].backend == Backend::QemuHvf && self.configs[id].guest == "windows";
        if desktop_tools::METHODS.contains(&method) {
            let macos = self.configs[id].guest == "macos";
            if windows_vm {
                let arguments = desktop_tools::arguments(method, args)?;
                *deferred = Some(self.windows_call(
                    id,
                    WindowsCall::Desktop(desktop_tools::guest_method(method).into(), arguments),
                )?);
                return Ok(Value::Null);
            }
            if self.configs[id].backend != Backend::AppleVirtualization
                || !(macos || self.configs[id].guest == "linux")
            {
                bail!("unsupported: desktop window and application tools require an Apple Virtualization Linux desktop, a macOS container or a Windows machine with SSH; on a QEMU Linux desktop whose image ships cua-driver use the cua tools (cua_list_windows, cua_get_window_state)")
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
        // The scaled-JPEG form (decision 0025 item 2). No such field: `None`, and nothing below differs from before.
        let scaled_screenshot = if method == "screenshot" {
            let scaled = screenshot_scaled::Scaled::parse(args)?;
            if scaled.is_some()
                && (guest_screenshot
                    || self
                        .qemu
                        .as_ref()
                        .is_none_or(|q| q.console_broker.is_none()))
            {
                return Err(screenshot_scaled::Refused::new(
                    screenshot_scaled::Refusal::FormatUnavailable,
                    "unsupported: the scaled JPEG screenshot form requires the QEMU console broker; request the screenshot without format, max_width, max_height and quality",
                )
                .into());
            }
            scaled
        } else {
            None
        };
        if method == "status" {
            return self.view(id);
        }
        if method == "start" || method == snapshot_restore::START {
            if let Some(reason) = self.invalid.get(id) {
                bail!("VM configuration failed validation at registry load and cannot be started: {reason}")
            }
            let restore_plan = if method == snapshot_restore::START {
                Some(snapshot_restore::start_plan(self, args)?)
            } else {
                None
            };
            let config = match &restore_plan {
                Some(plan) => plan.config.clone(),
                None => self.configs.get(id).context("VM not found")?.clone(),
            };
            self.admissible(&config)?;
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
            // Key-event logging directory: checked before any state of this start is touched, and re-checked on
            // every start because the directory may have changed since the daemon started. The canonical path is
            // what the worker receives.
            if config.backend == Backend::AppleVirtualization
                && self.worker_key_event_log_dir.is_some()
            {
                let canonical = validate_worker_key_event_log_dir(
                    self.worker_key_event_log_dir.as_ref().unwrap(),
                )?;
                self.worker_key_event_log_dir = Some(canonical);
            }
            self.jobs.lock().unwrap().clear_vm(id);
            let config_path = if let Some(plan) = &restore_plan {
                // Prepared privately and durably by the operation thread. The
                // selected registry and ordinary start config stay untouched.
                plan.config_path.clone()
            } else {
                let path = self.dir.join(format!("{id}.json"));
                write_vm_config(&self.dir, &path, &config)?;
                path
            };
            let mut command = match config.backend {
                Backend::AppleVirtualization => Command::new(&self.backend),
                Backend::QemuHvf => backend::require_registration(&self.qemu)?.command(),
            };
            let bridge = Arc::new(Mutex::new(bridge::State::new()?));
            let worker_instance = bridge.lock().unwrap().worker_instance.clone();
            let unix_seconds = if config.backend == Backend::AppleVirtualization
                && self.worker_key_event_log_dir.is_some()
            {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs()
            } else {
                0
            };
            let (start_args, key_event_log_path) = worker_start_args(
                config.backend,
                &config_path,
                self.worker_key_event_log_dir.as_deref(),
                id,
                unix_seconds,
                &worker_instance,
            );
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
            let machine_state = match args.get("restore_machine_state") {
                Some(path) if restore_plan.is_none() => {
                    Some(container_state::restore_path(&config, path)?)
                }
                Some(_) => {
                    bail!("restore_machine_state cannot be combined with a snapshot restore")
                }
                None => None,
            };
            snapshot_restore::append_start_arguments(
                &mut command,
                &start_args,
                restore_plan
                    .as_ref()
                    .map(|plan| plan.worker_arguments())
                    .or(machine_state
                        .map(|path| vec!["--restore-machine-state".into(), path.into()])),
            );
            let mut child = command
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("starting VM backend")?;
            if let Some(path) = key_event_log_path {
                log_line(&format!(
                    "vm-control: worker key-event logging enabled for {id}: {path:?}"
                ));
                self.worker_key_event_logs
                    .insert(id.into(), (worker_instance, path));
            } else {
                self.worker_key_event_logs.remove(id);
            }
            drop(command);
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            let state = Arc::new(Mutex::new(json!({"state":"starting"})));
            let guest_async_timeout = Arc::new(Mutex::new(GuestAsyncTimeout::default()));
            let pending: Arc<Mutex<BTreeMap<u64, PendingReply>>> =
                Arc::new(Mutex::new(BTreeMap::new()));
            let output_state = state.clone();
            let output_async_timeout = guest_async_timeout.clone();
            let output_pending = pending.clone();
            let output_events = (self.events.clone(), id.to_string());
            let output_leases = self.leases.clone();
            let restore_expected = restore_plan.as_ref().map(|plan| {
                (
                    plan.config.id.clone(),
                    plan.capture_id.clone(),
                    plan.config_sha256.clone(),
                )
            });
            thread::spawn(move || {
                let mut reader = BufReader::with_capacity(LARGE_REPLY_READ_BUFFER, stdout);
                let mut restored_receipt: Option<Value> = None;
                // The worker's timing record for the screenshot reply that follows it: request number, stages,
                // and when it was read. Only unsigned integers are kept from the worker.
                let mut shot: Option<(Option<u64>, [u64; 4], std::time::Instant)> = None;
                loop {
                    let frame = read_frame_bounded(&mut reader, MAX_SCREENSHOT_RESPONSE);
                    let read = std::time::Instant::now();
                    match frame {
                        Ok(Some(bytes)) => match serde_json::from_slice::<Value>(&bytes) {
                            Ok(mut value) => {
                                let parsed = std::time::Instant::now();
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
                                if value["event"] == "snapshot_restored" {
                                    let valid = restore_expected.as_ref().is_some_and(
                                        |(vm, capture, configuration)| {
                                            value["vm_id"].as_str() == Some(vm.as_str())
                                                && value["capture_id"].as_str()
                                                    == Some(capture.as_str())
                                                && value["configuration_sha256"].as_str()
                                                    == Some(configuration.as_str())
                                                && value["state"] == "paused"
                                                && value["restored"] == true
                                        },
                                    );
                                    if !valid {
                                        replace_runtime_state(
                                            &mut output_state.lock().unwrap(),
                                            json!({"state":"error","error":"unbound saved-state restore receipt"}),
                                        );
                                        break;
                                    }
                                    restored_receipt = Some(value.clone());
                                }
                                if value["event"] == "state" {
                                    output_async_timeout
                                        .lock()
                                        .unwrap()
                                        .observe_worker_state(&value["state"]);
                                    output_events.0.worker_state(&output_events.1, &value);
                                    let mut observed_state = output_state.lock().unwrap();
                                    replace_runtime_state(&mut observed_state, value.clone());
                                    if let Some(receipt) = &restored_receipt {
                                        observed_state["snapshot_restore"] = receipt.clone();
                                    }
                                }
                                if let Some(change) = match value["event"].as_str() {
                                    Some("window_opened" | "window_closed") => output_leases
                                        .viewer(
                                            &output_events.1,
                                            "window",
                                            value["event"] == "window_opened",
                                            std::time::Instant::now(),
                                        ),
                                    Some("person_input") => output_leases
                                        .human_input(&output_events.1, std::time::Instant::now()),
                                    _ => None,
                                } {
                                    let holder = match change {
                                        control_lease::Change::PersonTookControl => "person",
                                        control_lease::Change::AgentMayAct => "agent",
                                    };
                                    output_events.0.publish(
                                        &output_events.1,
                                        "control",
                                        json!({"state":holder}),
                                    );
                                }
                                if value["event"] == "screenshot_timing" {
                                    let stages = SHOT_WORKER_STAGES
                                        .map(|(name, _)| value[name].as_u64().unwrap_or(0));
                                    shot = Some((value["request"].as_u64(), stages, parsed));
                                    continue;
                                }
                                if let Some(id) = value["id"].as_u64() {
                                    // In-process note beside `result`, read by `Pending::wait` and never forwarded.
                                    if let Some((_, stages, seen)) =
                                        shot.take().filter(|(request, ..)| *request == Some(id))
                                    {
                                        let mut note = json!({"pipe_us":read.duration_since(seen).as_micros() as u64,"parse_us":parsed.duration_since(read).as_micros() as u64,"bytes":bytes.len()});
                                        for ((_, name), stage) in
                                            SHOT_WORKER_STAGES.iter().zip(stages)
                                        {
                                            note[*name] = json!(stage);
                                        }
                                        value["screenshot_timing"] = note;
                                    }
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
                output_async_timeout.lock().unwrap().invalidate();
                output_pending.lock().unwrap().clear();
            });
            // A new VM process creates a new Guest service lifetime. Do not carry
            // cleanup-required state from the previous, already exited Guest.
            self.started(
                id.into(),
                Running {
                    bridge,
                    child,
                    assigned_memory_mib: config.memory_mib,
                    expected_exit: false,
                    execution_quiescing: false,
                    guest_async_timeout,
                    exit_status: None,
                    disk_identity,
                    stdin,
                    state,
                    pending,
                    next: 1,
                },
            );
            if let Some(plan) = &restore_plan {
                plan.attach_worker(self.running.get_mut(id).unwrap())?;
            }
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
            let config = self.configs.get(id).unwrap();
            let windows = config.backend == Backend::QemuHvf && config.guest == "windows";
            if !windows && !["linux", "macos"].contains(&config.guest.as_str()) {
                bail!("unsupported: file integration requires enabled Linux or macOS Guest service, or a Windows machine with SSH")
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
            let config = &self.configs[id];
            if config.backend == Backend::QemuHvf && config.guest == "windows" {
                return self.windows_file(id, method, args, deferred);
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
                input::TEXT_STEP_METHOD,
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
                "guest_stream_status",
                "windows",
                "exec_start",
                "exec_poll",
                "exec_cancel",
                container_state::SAVE,
                container_state::CLOCK_SYNC,
            ]
            .contains(&method)
        {
            let run = self.running.get_mut(id).context("VM process not active")?;
            if run.poll_exit()?.is_some() {
                bail!("VM process has exited")
            }
            if method == "exec_start" {
                let timeout = exec_timeout_ms(args, ASYNC_EXEC_MAX_TIMEOUT_MS)?;
                let maximum = if self.configs[id].backend == Backend::QemuHvf
                    && self.configs[id].guest == "linux"
                    && self.configs[id].qemu_guest_exec
                {
                    run.guest_async_timeout.lock().unwrap().maximum
                } else {
                    SYNC_EXEC_MAX_TIMEOUT_MS
                };
                if timeout > maximum {
                    bail!(
                        "timeout_ms exceeds this Guest's observed asynchronous limit {maximum}; query vm_guest_info before a longer start (unknown or reconnecting Guest limit is 30000)"
                    )
                }
            }
            if method == "guest_info" {
                // Failure or a missing/malformed policy leaves the safe legacy
                // ceiling, rather than reusing a prior successful observation.
                run.guest_async_timeout.lock().unwrap().invalidate();
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
            if let Some(scaled) = &scaled_screenshot {
                scaled.apply(&mut command);
            }
            if method == "windows" {
                command["method"] = json!("guest_request");
                command["request"] = json!({"id":number.to_string(),"method":"windows"});
            }
            if method == "guest_info" {
                command["method"] = json!("guest_request");
                command["request"] = json!({"id":number.to_string(),"method":"info"});
            }
            if method == "guest_stream_status" {
                command["method"] = json!("guest_stream_probe");
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
                let timeout = exec_timeout_ms(args, ASYNC_EXEC_MAX_TIMEOUT_MS)?;
                command["method"] = json!("guest_request");
                command["request"] = json!({"id":number.to_string(),"method":method,"argv":argv,"timeout_ms":timeout});
                for (key, value) in exec_identity(args)? {
                    command["request"][key] = value;
                }
            }
            if ["exec_poll", "exec_cancel"].contains(&method) {
                command["method"] = json!("guest_request");
                command["request"] =
                    json!({"id":number.to_string(),"method":method,"job_id":args["job_id"]});
            }
            let post = if bridge_post.is_some() {
                bridge_post
            } else if method == "guest_stream_status" {
                Some(JobPost::GuestStreamStatus)
            } else if method == "guest_info" {
                Some(JobPost::GuestInfo {
                    backend: self.configs[id].backend,
                    guest: self.configs[id].guest.clone(),
                    state: run.state.clone(),
                    qemu_exec: self.configs[id].backend == Backend::QemuHvf
                        && self.configs[id].guest == "linux"
                        && self.configs[id].qemu_guest_exec,
                    async_timeout: run.guest_async_timeout.clone(),
                    async_generation: run.guest_async_timeout.lock().unwrap().generation,
                })
            } else if method == input::TEXT_STEP_METHOD {
                Some(JobPost::GuestText)
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
                    scaled: scaled_screenshot,
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
            if method == container_state::SAVE {
                container_state::save_command(&self.configs[id], args, &mut command)?;
            }
            if method == container_state::CLOCK_SYNC {
                command["method"] = json!("guest_request");
                command["request"] = container_state::clock_request(number)?;
            }
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
                } else if method == "guest_stream_status" {
                    3
                } else if method == "screenshot" {
                    17
                } else if method == "screen_wait" {
                    30
                } else if method == "windows" {
                    12
                } else if method == container_state::SAVE {
                    container_state::SAVE_TIMEOUT_SECONDS
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
/// Product-spec capabilities that a backend/Guest pair either lacks or offers only under stated conditions. Each one is
/// listed by name with an explicit `status`, so a client never has to infer absence from a missing key. An entry is
/// `supported` only where code implements it; `unsupported` entries carry the reason. `guest` may be `windows`, which no
/// backend can register, so that its gaps are stated too.
pub const PRODUCT_CAPABILITIES: [&str; 7] = [
    "clipboard",
    "accessibility_metadata",
    "application_tools",
    "unicode_text_input",
    "retina_input_mapping",
    "multi_display_input_mapping",
    "window_metadata",
];
/// Known limits of a Guest that are not unimplemented features of this product: measured boundaries of the backend
/// the Guest runs on. Stated only for macOS Guests on Apple Virtualization; every other pair gets an empty list and no
/// `known_limits` key in its status. Each entry has exactly the keys in `KNOWN_LIMIT_KEYS`; `code` is the stable
/// identifier, and the wording claims no more than the cited decision established (one Guest version, one host).
pub const KNOWN_LIMIT_KEYS: [&str; 7] = [
    "code",
    "description",
    "effect",
    "classification",
    "measured_on",
    "decision",
    "evidence",
];
pub const KNOWN_LIMIT_CLASSIFICATION: &str =
    "backend limit of Apple's paravirtual graphics, not of this product's implementation";
pub const MACOS_GUEST_METAL_LARGE_COMPUTE: &str = "macos_guest_metal_large_compute_pipelines";
pub const MACOS_GUEST_OPENGL_NOT_ACCELERATED: &str = "macos_guest_opengl_not_accelerated";
pub fn known_limits(backend: Backend, guest: &str) -> Value {
    if backend != Backend::AppleVirtualization || guest != "macos" {
        return json!([]);
    }
    json!([
        {
            "code":MACOS_GUEST_METAL_LARGE_COMPUTE,
            "description":"Large Metal compute pipelines, including the Blender Cycles shading kernels, are not created on the Guest's Metal device, so Cycles rendering on the Guest's Metal device is not available.",
            "effect":"Metal graphics and small compute pipelines work, and Cycles renders on the Guest's CPUs. The largest kernels stall without an error instead of failing. This product has no render request path, so it raises no error of its own and performs no CPU fallback: the application in the Guest sees the failure or the stall.",
            "classification":KNOWN_LIMIT_CLASSIFICATION,
            "measured_on":"macOS 26.6.2 Guest on one host (Apple M1 Pro, macOS 26.4.1); other macOS versions and hosts were not measured, and the limit may move with an Apple update",
            "decision":"docs/decisions/0023-macos-guest-metal-compute-limit.md",
            "evidence":"archive/docs-evidence/blender-cycles-error-propagation/guest-baseline-app-v1/README.md",
        },
        {
            "code":MACOS_GUEST_OPENGL_NOT_ACCELERATED,
            "description":"The Guest has no accelerated OpenGL (CGL) renderer: an application that asks for OpenGL gets Apple Software Renderer.",
            "effect":"Metal applications are not affected. Requests that require an accelerated OpenGL renderer fail in the Guest (legacy, 3.2 core and 4.1 core were tried), and FreeCAD 1.1.4 ran on Apple Software Renderer 2.1; software-rendered results are not accepted as GPU acceleration.",
            "classification":KNOWN_LIMIT_CLASSIFICATION,
            "measured_on":"macOS 26.6.2 Guest with 4 CPUs and 4 GiB on one host (Apple M1 Pro); other macOS versions and hosts were not measured, and this is not a claim about future Apple releases",
            "decision":"docs/decisions/0017-macos-opengl-capability-gap.md",
            "evidence":"archive/docs-evidence/freecad-macos-live/cgl1-no-accelerated-renderer/README.md",
        },
    ])
}
pub fn product_capabilities(backend: Backend, guest: &str) -> Value {
    fn no(reason: &str) -> Value {
        json!({"status":"unsupported","reason":reason})
    }
    if guest == "windows" {
        let qemu = backend == Backend::QemuHvf;
        let mut out: serde_json::Map<String, Value> = PRODUCT_CAPABILITIES
            .iter()
            .map(|name| {
                let reason = if qemu {
                    "not implemented for Windows guests"
                } else {
                    "Apple Virtualization cannot run Windows; Windows guests run on QEMU/HVF (machine create --os windows)"
                };
                (name.to_string(), no(reason))
            })
            .collect();
        if qemu {
            let login = "Windows machines with SSH set up (machine ssh-key) and a logged-on desktop user; runs in that user's session over the Guest's OpenSSH Server";
            for (name, tools) in [
                ("window_metadata", "vm_desktop_state and vm_window_action through Win32"),
                ("accessibility_metadata", "vm_accessibility_tree, vm_accessibility_find and vm_accessibility_action through UI Automation"),
                ("application_tools", "vm_desktop_apps (Start menu entries) and vm_app_launch"),
            ] {
                out.insert(
                    name.into(),
                    json!({"status":"supported","requirements":format!("{tools}: {login}")}),
                );
            }
        }
        return Value::Object(out);
    }
    let mac = backend == Backend::AppleVirtualization && guest == "macos";
    let apple_linux = backend == Backend::AppleVirtualization && !mac;
    fn yes(requirements: &str) -> Value {
        json!({"status":"supported","requirements":requirements})
    }
    json!({
        "clipboard": no("not implemented: no clipboard channel exists in the daemon, the MCP tools or any Guest service"),
        "accessibility_metadata": if mac {
            yes("macOS containers: vm_accessibility_tree, vm_accessibility_find and vm_accessibility_action through the vm-macos-desktop helper over SSH (Accessibility API); needs the Guest Accessibility grant")
        } else if apple_linux {
            yes("Linux desktop containers (--desktop): vm_accessibility_tree, vm_accessibility_find and vm_accessibility_action; the guest agent reads AT-SPI over the desktop's accessibility bus")
        } else {
            no("not implemented: no Guest accessibility query exists for this Guest")
        },
        "application_tools": if mac {
            yes("macOS containers: vm_desktop_apps and vm_app_launch through the vm-macos-desktop helper over SSH; launching needs the input permission")
        } else if apple_linux {
            yes("Linux desktop containers (--desktop): vm_desktop_apps lists visible .desktop entries and vm_app_launch starts one as the desktop session's user; launching needs the input permission")
        } else {
            no("not implemented: no application launch or list tool exists; vm_exec_start runs an explicit argv only and is reported separately as command_execution")
        },
        "unicode_text_input": if mac {
            no("not implemented for macOS Guests: vm_input_text is refused; vm_input_key sends physical key positions that the Guest keyboard layout interprets")
        } else if apple_linux {
            no("not implemented for Apple Virtualization Linux Guests: vm_input_text requires a Linux Guest on QEMU; desktop containers take physical key positions through vm_input_key")
        } else {
            json!({"status":"supported","requirements":"vm_input_text: VM registered with qemu_guest_exec, persistent console, Guest service on the execution transport started with --allow-text-input, and a running X11 session with XTEST and at least 8 spare keycodes (no Wayland); a screenshot younger than 5 seconds; at most 256 Unicode scalar values per call, LF and TAB the only control characters; without these the call fails with an explicit unsupported error","evidence":"Guest half verified on real Xorg n=1, record not retained in this tree; host path offline-tested only; the X server acknowledges key events and application acceptance is not observed"})
        },
        "retina_input_mapping": if mac {
            json!({"status":"supported","requirements":"Guest service --allow-observe --allow-input with existing Screen Recording and Accessibility permissions; coordinates are pixels of the bound screenshot and the Guest maps them to Quartz points on its main display","evidence":"Guest unit test of the 2x mapping only; no live Guest input acceptance is recorded"})
        } else if apple_linux {
            no("not implemented: a Linux desktop container has one virtual display; input coordinates are its framebuffer pixels and no display scale factor is carried")
        } else {
            no("not implemented: the input binding carries no display scale factor; coordinates are physical framebuffer pixels of the bound screenshot and are validated only on an unscaled single-display Xorg desktop")
        },
        "multi_display_input_mapping": no(if mac {
            "not implemented: screenshots and input bind to the Guest main display only"
        } else if apple_linux {
            "not implemented: a Linux desktop container has one virtual display"
        } else {
            "not implemented: the QEMU console is one display and one absolute pointer device spans it"
        }),
        "window_metadata": if mac {
            json!({"status":"supported","requirements":"Guest service --allow-observe with existing Screen Recording permission and an eligible console; vm_windows returns no titles, focus, stacking order or Accessibility tree"})
        } else if apple_linux {
            yes("Linux desktop containers (--desktop): vm_desktop_state lists windows top-down with title, application, geometry and state through EWMH; vm_windows stays macOS-only")
        } else {
            no("not implemented on QEMU: vm_windows is refused for Linux Guests and the desktop window tools serve Apple Virtualization desktop containers only")
        },
    })
}
/// Every product-spec control feature, stated for each backend/Guest pair under `control_features`. Each entry has a
/// `status` from `FEATURE_STATUSES` and a `reason`. `supported` is used only with an `evidence` path to a real-Guest
/// record for that pair; code without such a record is `implemented_unverified` (offline tests at most).
/// `requires_guest_permission` means the code exists and only its refusal without the Guest grant is recorded.
/// `product_capabilities` is the older, narrower report and keeps its shape.
pub const CONTROL_FEATURES: [&str; 23] = [
    "lifecycle",
    "command_execution",
    "async_jobs",
    "file_transfer",
    "screenshot",
    "screenshot_scaled_jpeg",
    "pointer_key_input",
    "unicode_text_input",
    "window_metadata",
    "accessibility_metadata",
    "application_tools",
    "clipboard",
    "snapshot",
    "memory_reclaim",
    "multi_display_input_mapping",
    "retina_input_mapping",
    "shared_folders",
    "guest_shutdown",
    "balloon",
    "gpu_3d",
    "application_launch_list",
    "accessibility_tree",
    "window_list",
];
pub const FEATURE_STATUSES: [&str; 5] = [
    "supported",
    "implemented_unverified",
    "requires_guest_permission",
    "external_blocked",
    "unsupported",
];
pub fn control_features(backend: Backend, guest: &str) -> Value {
    let mut out = if backend == Backend::QemuHvf && guest == "macos" {
        Value::Object(CONTROL_FEATURES.iter().map(|name| {
            (name.to_string(), json!({"status":"unsupported","reason":"macOS Guests cannot be registered on QEMU; this backend supports Linux Guests only"}))
        }).collect())
    } else {
        base_control_features(backend, guest)
    };
    out["balloon"] = out["memory_reclaim"].clone();
    out["window_list"] = out["window_metadata"].clone();
    let desktop_tools = (backend == Backend::AppleVirtualization && guest != "windows")
        || (backend == Backend::QemuHvf && guest == "windows");
    out["application_launch_list"] = if desktop_tools {
        out["application_tools"].clone()
    } else {
        json!({"status":"unsupported","reason":if guest == "linux" {
            "not implemented: no vm_app_* launch or list tool exists on this backend; on Linux only, vm_exec_start runs explicit argv through enabled Guest command execution (see command_execution)"
        } else {
            "not implemented: no vm_app_* launch or list tool exists on this backend; command execution is exposed on Linux only"
        }})
    };
    out["accessibility_tree"] = if desktop_tools {
        out["accessibility_metadata"].clone()
    } else {
        json!({"status":"unsupported","reason":"not implemented: no Guest accessibility tree query exists for this backend and Guest"})
    };
    out["gpu_3d"] = if backend == Backend::AppleVirtualization && guest == "macos" {
        json!({"status":"supported","reason":"Narrow: Godot Metal Forward+ sample on one macOS 26.6.2 Guest; no accelerated OpenGL (decision 0017), large Metal compute/Cycles unavailable (decision 0023); see known_limits; no general compatibility or performance claim","evidence":"archive/docs-evidence/godot-macos-live/README.md"})
    } else if backend == Backend::QemuHvf && guest == "linux" {
        qemu_gpu_3d(qemu_graphics::Profile::Virgl)
    } else if backend == Backend::QemuHvf && guest == "windows" {
        qemu_gpu_3d(qemu_graphics::Profile::NeptuneDxmtExperimental)
    } else {
        json!({"status":"unsupported","reason":"no supported 3D acceleration path for this backend/Guest pair; host GPU availability does not establish Guest acceleration"})
    };
    if backend == Backend::QemuHvf && guest == "macos" {
        out["clipboard"] = json!({"status":"unsupported","reason":"not implemented: no clipboard channel exists; macOS Guests also cannot be registered on QEMU"});
        out["snapshot"] = json!({"status":"unsupported","reason":"not implemented: no live snapshot or VM save/restore; macOS Guests also cannot be registered on QEMU"});
    }
    out
}
fn qemu_gpu_3d(profile: qemu_graphics::Profile) -> Value {
    match profile {
        qemu_graphics::Profile::Virtio2d => {
            json!({"status":"unsupported","reason":"The explicit virtio-2d profile supplies an unaccelerated 2D console. It does not provide Virgl, Venus or Guest 3D acceleration; snapshot support requires separate backend validation."})
        }
        qemu_graphics::Profile::NeptuneDxmtExperimental => {
            json!({"status":"implemented_unverified","reason":"Experimental: D3D11 through UTM's Triton driver, Neptune and DXMT on Metal, pixel-checked and benchmarked on one Windows machine with guest tools 0.1.273; no D3D12 or game claim (docs/known-issues.md)"})
        }
        qemu_graphics::Profile::Virgl => {
            json!({"status":"supported","reason":"Narrow: registered QEMU virgl/ANGLE Metal, SolveSpace OpenGL 2.1 sample only; requires a matching Guest driver; no general compatibility, per-VM Guest verification or performance claim","evidence":"archive/docs-evidence/linux-cad-interaction-live-v11/README.md"})
        }
        qemu_graphics::Profile::VenusMoltenvkExperimental => {
            json!({"status":"supported","reason":"Narrow: opt-in registered Venus runtime, one Linux Guest raster/vkmark sample; requires a matching Guest driver; registered host Geometry contract is not accepted (see host_api_constraints); no general compatibility, per-VM Guest verification or performance claim","evidence":"archive/docs-evidence/linux-vulkan-control-live/v2-accepted/README.md","host_api_constraints":qemu_graphics::host_api_constraints(profile)})
        }
    }
}
fn unverified_older(older: &Value, name: &str) -> Value {
    json!({"status":"implemented_unverified","reason":older[name]["requirements"]})
}
fn base_control_features(backend: Backend, guest: &str) -> Value {
    fn yes(reason: &str, evidence: &str) -> Value {
        json!({"status":"supported","reason":reason,"evidence":evidence})
    }
    fn st(status: &str, reason: &str) -> Value {
        json!({"status":status,"reason":reason})
    }
    fn no(reason: &str) -> Value {
        st("unsupported", reason)
    }
    // The seven older entries keep their unsupported reasons; those that have code are restated below.
    let mut out = product_capabilities(backend, guest);
    // Supported entries of the older report have code but no real-Guest record in this tree.
    let older = out.clone();
    let unverified = |name: &str| {
        st(
            "implemented_unverified",
            older[name]["requirements"].as_str().unwrap(),
        )
    };
    for entry in out.as_object_mut().unwrap().values_mut() {
        if entry["status"] != "unsupported" {
            *entry = Value::Null;
        }
    }
    let mac = backend == Backend::AppleVirtualization && guest == "macos";
    let qemu = backend == Backend::QemuHvf;
    if guest == "windows" {
        for name in CONTROL_FEATURES {
            out[name] = no(if qemu {
                "not implemented for Windows guests"
            } else {
                "Apple Virtualization cannot run Windows; Windows guests run on QEMU/HVF (machine create --os windows)"
            });
        }
        if !qemu {
            return out;
        }
        // One live Windows 11 26300 machine; the record is docs/known-issues.md, not an archived evidence file.
        out["lifecycle"] = st("implemented_unverified", "machine create --os windows: unattended install from the user's ISO, start, power-button shutdown and cold reboots; live on one Windows 11 ARM64 machine (docs/known-issues.md)");
        out["command_execution"] = st("implemented_unverified", "vm_exec over the Guest's OpenSSH Server (machine ssh-key): argv without a shell as the SSH user, up to 30 s, 64 KiB per stream; live on one machine");
        out["async_jobs"] = no("unsupported: Windows guests run commands with vm_exec only; there is no exec_start, exec_poll or exec_cancel");
        out["file_transfer"] = st("implemented_unverified", "vm_file_* over SSH: one name in %USERPROFILE%\\veynodevm-files, up to 8 MiB, staged on the host, no overwrite; live on one machine");
        for name in [
            "window_metadata",
            "accessibility_metadata",
            "application_tools",
        ] {
            out[name] = unverified_older(&older, name);
        }
        return out;
    }
    out["snapshot"] = if backend == Backend::AppleVirtualization && guest == "linux" {
        st("implemented_unverified", "Narrow: named full-state Apple Linux snapshots; explicit persistent identity, idle Guest work and stopped-target restore; no real-Guest record in this tree; GPU migration and cross-host use unverified")
    } else if qemu {
        no("unsupported: Virgl/Venus GPU state blocks full-state migration; the explicit unaccelerated virtio-2d QMP helper has CPU tests only and is not connected to production snapshot operations")
    } else {
        no("unsupported: macOS Guest snapshot operations are not implemented; offline APFS clones contain no RAM, CPU or device state")
    };
    out["screenshot_scaled_jpeg"] = if qemu && guest == "linux" {
        st("implemented_unverified", "scaled JPEG form (format, max_width, max_height, quality); needs a console broker and a helper that knows it; an older helper returns its full-size PNG unchanged (check mime_type)")
    } else {
        no("unsupported: the scaled JPEG screenshot request form is refused on the Apple backend; requires a Linux Guest on QEMU with a registered console broker")
    };
    if mac {
        out["lifecycle"] = yes("register an installed disk, start, power-button shutdown request, confirmed force stop; a locked Guest does not act on the power-button request", "archive/docs-evidence/apple-macos-installation.json");
        let exec = "not implemented: the macOS Guest service has no exec_start, exec_poll or exec_cancel and its legacy exec is not exposed (decision 0014); the Guest answers unsupported";
        out["command_execution"] = no(exec);
        out["async_jobs"] = no(exec);
        out["file_transfer"] = yes("Guest service --allow-files --files-root with a private same-UID directory; single filenames, 8 MiB", "archive/docs-evidence/macos-files-live/README.md");
        out["screenshot"] = yes("Guest service --allow-observe with existing Guest Screen Recording permission; main display", "archive/docs-evidence/control-macos-screenshot.md");
        out["pointer_key_input"] = st("requires_guest_permission", "implemented; needs Guest service --allow-observe --allow-input and a Guest Accessibility grant; only the refusal accessibility_permission_required is recorded on a real Guest");
        out["window_metadata"] = yes("Guest service --allow-observe with existing Guest Screen Recording permission; no titles, focus, stacking order or Accessibility tree", "archive/docs-evidence/macos-windows-live/README.md");
        out["retina_input_mapping"] = st("implemented_unverified", "Guest maps bound-screenshot pixels to Quartz points on its main display; Guest unit test of the 2x mapping only, and input itself needs the Guest Accessibility grant");
        out["accessibility_metadata"] = unverified("accessibility_metadata");
        out["application_tools"] = unverified("application_tools");
        out["memory_reclaim"] = no("not implemented: no balloon device is configured for macOS Guests and nothing returns Guest memory to the host");
        out["shared_folders"] = yes("shared_dir at registration only: one read-only virtiofs share tagged veynode, mounted from inside the Guest", "archive/docs-evidence/apple-macos-desktop-probe.md");
        out["guest_shutdown"] = no("not implemented: the macOS Guest service has no shutdown method, so vm_shutdown mode guest is answered unsupported; mode backend is the power-button request");
    } else if !qemu {
        out["lifecycle"] = yes("register a direct-boot kernel, start, power-button shutdown request, confirmed force stop; the Guest kernel may ignore the power button", "archive/docs-evidence/apple-linux-lifecycle.txt");
        let exec = "enabled Guest service with asynchronous jobs; argv only; synchronous and unknown/reconnecting Guest host limit 30 s; only opted-in QEMU Linux exec-v2 can use up to 120 s after vm_guest_info reports its bound no-reconnect policy";
        out["command_execution"] = yes(exec, "archive/docs-evidence/control-sync-cancel.md");
        out["async_jobs"] = yes(exec, "archive/docs-evidence/control-sync-cancel.md");
        out["file_transfer"] = yes("Guest service files integration with a dedicated non-root files account; single filenames, 8 MiB", "archive/docs-evidence/control-files.md");
        out["screenshot"] = st("implemented_unverified", "Linux desktop containers (--desktop) only: guest agent DRM scanout, PNG (JPEG when large); vm_guest_info reports capabilities.screenshot");
        out["pointer_key_input"] = st("implemented_unverified", "Linux desktop containers (--desktop) only: guest agent uinput keyboard and absolute tablet, against a fresh screenshot anchor");
        out["window_metadata"] = unverified("window_metadata");
        out["accessibility_metadata"] = unverified("accessibility_metadata");
        out["application_tools"] = unverified("application_tools");
        out["memory_reclaim"] = no("not supported: a virtio balloon device is attached but nothing drives it; measured 2026-10-12, a 512 MiB balloon target raised the host footprint by 933 MiB");
        out["shared_folders"] = st("implemented_unverified", "shared_dir at registration attaches one read-only virtiofs share tagged veynode; no real-Guest record of a Linux Guest mounting it through this daemon");
        out["guest_shutdown"] = yes("vm_shutdown mode guest; Guest service running as root with shutdown enabled; request acceptance is not completion", "archive/docs-evidence/apple-linux-normal-shutdown.json");
    } else {
        out["lifecycle"] = yes("QEMU registered by the daemon operator; direct-boot kernel with optional raw disk; start, QMP power-button request, confirmed force stop", "archive/docs-evidence/installed-qemu-retention-normal-live/README.md");
        let exec = "VM registered with qemu_guest_exec and a rootless cgroup-v2 Guest service on the execution transport; refused with an explicit unsupported error otherwise";
        out["command_execution"] = yes(
            exec,
            "archive/docs-evidence/installed-qemu-exec-live/README.md",
        );
        out["async_jobs"] = yes(
            exec,
            "archive/docs-evidence/installed-qemu-exec-live/README.md",
        );
        out["file_transfer"] = no("not implemented: the QEMU serial transports carry no file capability; the daemon refuses file calls with an explicit unsupported error");
        out["screenshot"] = yes("QEMU capture registered by the daemon operator; refused with an explicit unsupported error otherwise", "archive/docs-evidence/spice-screenshot.md");
        out["pointer_key_input"] = yes("registered QEMU persistent console; absolute click, drag, wheel and physical key codes on an unscaled single-display Xorg desktop", "archive/docs-evidence/control-input.md");
        out["unicode_text_input"] = st("implemented_unverified", "vm_input_text: qemu_guest_exec, Guest service --allow-text-input, X11 with XTEST (no Wayland); full requirements and limits under product_capabilities.unicode_text_input");
        out["memory_reclaim"] = no("not supported: no balloon or free page reporting device is configured; measured 2026-10-12, free page reporting discarded 594 MiB without lowering the host footprint");
        out["shared_folders"] =
            no("not implemented: configuration validation refuses shared_dir for QEMU");
        out["guest_shutdown"] = no("not implemented: the daemon refuses vm_shutdown mode guest for QEMU with an explicit unsupported error; mode backend is the QMP power-button request");
    }
    out
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
    serve_with_worker_key_event_log(dir, backend, memory_budget_mib, qemu, None)
}
pub fn serve_with_worker_key_event_log(
    dir: PathBuf,
    backend: PathBuf,
    memory_budget_mib: Option<u64>,
    qemu: Option<QemuRegistration>,
    worker_key_event_log_dir: Option<PathBuf>,
) -> Result<()> {
    let _log_drain = LogDrain;
    let worker_key_event_log_dir = worker_key_event_log_dir
        .map(|path| validate_worker_key_event_log_dir(&path))
        .transpose()?;
    if let Some(registration) = &qemu {
        registration.validate()?;
    }
    let memory = MemoryBudget::detect(memory_budget_mib)?;
    private_dir(&dir)?;
    // Snapshot state is opened component by component without following symlinks.
    let dir = fs::canonicalize(&dir)?;
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
    manager.worker_key_event_log_dir = worker_key_event_log_dir;
    manager.qemu_window = match qemu
        .as_ref()
        .and_then(|q| q.console_broker.as_ref().zip(q.console_helper.as_ref()))
    {
        Some((broker, helper)) => backend::probe_broker_window(broker, helper)?,
        None => json!({"native_window":false,"display_only":true,"input_enabled":false}),
    };
    manager.qemu = qemu;
    manager.validate_registry();
    let manager = Arc::new(Mutex::new(manager));
    spawn_exit_monitor(&manager);
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
            let _ = serve_connection(stream, manager, admitted_at, peer_euid);
            active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
    }
    Ok(())
}
/// Effective uid of the process at the other end of a connected `control.sock` stream, as recorded by the kernel.
fn peer_euid(stream: &UnixStream) -> Result<libc::uid_t> {
    #[cfg(target_os = "macos")]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(uid)
    }
    #[cfg(target_os = "linux")]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut length,
            )
        } != 0
            || length as usize != std::mem::size_of::<libc::ucred>()
        {
            bail!("control peer credential unavailable")
        }
        Ok(cred.uid)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = stream;
        bail!("control peer identity unsupported on this platform")
    }
}
/// Admission for one accepted `control.sock` connection. The socket file mode (0600 inside a 0700 directory) is the first
/// barrier; this is the second, and it does not depend on the file system: the peer's effective uid must equal the
/// daemon's. An unreadable credential is a refusal. `peer_uid` is a parameter only so a test can present a foreign uid
/// without root; the daemon always passes `peer_euid`.
fn admit_peer(
    stream: &UnixStream,
    peer_uid: impl FnOnce(&UnixStream) -> Result<libc::uid_t>,
) -> Result<()> {
    let own = unsafe { libc::geteuid() };
    match peer_uid(stream) {
        Ok(uid) if uid == own => Ok(()),
        Ok(_) => bail!("control connection refused: peer uid differs from daemon uid"),
        Err(_) => bail!("control connection refused: peer credential unavailable"),
    }
}
/// Refuses a foreign or unidentifiable peer before a single request byte is read; the stream is dropped without a reply.
fn serve_connection(
    stream: UnixStream,
    manager: Arc<Mutex<Manager>>,
    admitted_at: std::time::Instant,
    peer_uid: impl FnOnce(&UnixStream) -> Result<libc::uid_t>,
) -> Result<()> {
    admit_peer(&stream, peer_uid)?;
    handle_connection(stream, manager, admitted_at)
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
    if request["method"] == "screenshot" {
        widen_send_buffer(&stream);
    }
    let writing = std::time::Instant::now();
    let written = write_json(
        &mut stream,
        &match result {
            Ok(value) => json!({"result":value}),
            Err(e) => {
                let mut reply = json!({"error":e.to_string()});
                // Only a refusal of the scaled screenshot form carries a second field.
                screenshot_scaled::Refused::mark(&mut reply, &e);
                reply
            }
        },
    );
    if let Some(timing) = SHOT_TIMING.take() {
        log_line(&timing.line(
            admitted_at.elapsed().as_micros() as u64,
            writing.elapsed().as_micros() as u64,
        ));
    }
    written
}
pub fn call(dir: &Path, method: &str, args: Value) -> Result<Value> {
    call_reply(dir, method, args)?.map_err(|message| anyhow::anyhow!("{message}"))
}
/// `call` with the two failure classes kept apart: the outer error is a failed exchange with the
/// daemon (the request may or may not have been acted on), the inner one is the daemon's answer.
fn call_reply(dir: &Path, method: &str, args: Value) -> Result<std::result::Result<Value, String>> {
    let mut socket = UnixStream::connect(dir.join("control.sock"))
        .context("connect daemon; run vm-control daemon first")?;
    // Saving a large Guest's memory can outlast the usual reply deadline.
    let read_timeout = if method == container_state::SAVE {
        container_state::SAVE_TIMEOUT_SECONDS + 10
    } else if method == "driver_rpc" {
        // The daemon ends the call at its own derived deadline.
        cua_bridge::MAX_DEADLINE.as_secs() + 20
    } else {
        40
    };
    socket.set_read_timeout(Some(Duration::from_secs(read_timeout)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    write_json(&mut socket, &json!({"method":method,"arguments":args}))?;
    let capacity = if method == "screenshot" || method == "driver_rpc" {
        LARGE_REPLY_READ_BUFFER
    } else {
        8192
    };
    let bytes = read_frame_bounded(
        &mut BufReader::with_capacity(capacity, socket),
        response_limit(method),
    )?
    .context("daemon disconnected")?;
    let mut reply: Value = serde_json::from_slice(&bytes)?;
    drop(bytes);
    if let Some(error) = reply.get("error") {
        return Ok(Err(match error.as_str() {
            Some(message) => message.to_string(),
            None => error.to_string(),
        }));
    }
    Ok(Ok(reply.get_mut("result").map_or(Value::Null, Value::take)))
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
        json!({"name":"vm_screenshot","description":"Capture the same console: by default at full source resolution (QEMU PNG, Apple Virtualization Linux desktop PNG (JPEG when large) or macOS Guest JPEG). max_width/max_height scale it down, region crops it and format/quality re-encode it on the host; the reply then gives image_scale and image_region (input x = image_region.x + image x / image_scale) and, for a whole-screen image, image_point_pixel_scale to pass with coordinate_space points. Returns actual MIME, frame id, dimensions (always the full frame's, for input), scale and capture timing. veynodevm macOS containers are captured through the Guest's Screen Sharing server (no Guest permission grants); other macOS Guests need the opted-in Guest service with existing Screen Recording permission, never requested automatically.","inputSchema":{"type":"object","properties":agent_view::screenshot_properties(),"required":["id"],"additionalProperties":false}}),
        json!({"name":"vm_screen_wait","description":"Wait until the screen changes, then optionally until it is still for settle_ms (e.g. after a click, until redrawing ends). Pass the frame_id you acted on so an earlier change counts. Returns changed, settled, timing and, on Apple Virtualization Linux desktops, changed regions in screenshot pixels (region limits the wait there); QEMU and macOS Guests detect whole-screen changes. Take a new screenshot before input.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"frame_id":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1,"maximum":screen_wait::MAX_TIMEOUT_MS,"default":10000},"settle_ms":{"type":"integer","minimum":0,"maximum":screen_wait::MAX_SETTLE_MS,"default":0},"region":{"type":"object","properties":{"x":{"type":"integer","minimum":0},"y":{"type":"integer","minimum":0},"width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1}},"required":["x","y","width","height"],"additionalProperties":false}},"required":["id"],"additionalProperties":false}}),
        json!({"name":"vm_screen_text","description":"Read screen text without an image: a screenshot plus host text recognition (Apple Vision), on every screenshot backend with nothing installed in the Guest. Returns lines in reading order with boxes and centers in screenshot pixels, plus frame_id, source_generation, width and height for the input tools. find returns each case-insensitive match with its box. fast trades accuracy for speed; languages are BCP 47 codes (default automatic).","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"find":{"type":"string","minLength":1,"maxLength":256},"fast":{"type":"boolean","default":false},"languages":{"type":"array","items":{"type":"string"},"maxItems":8}},"required":["id"],"additionalProperties":false}}),
        json!({"name":"vm_windows","description":"Read macOS Guest on-screen window metadata (--allow-observe, existing Screen Recording permission). No titles, focus, stacking order, Accessibility tree or input authority. Desktop-global points; not atomic with a screenshot. Linux and Windows Guests return unsupported, as do other Guest/backend combinations.","inputSchema":id}),
        json!({"name":"vm_guest_info","description":"Query the real Guest service: Apple host-only vsock or QEMU private virtio-serial (default observer-only; explicit v2 permits rootless cgroup execution). Reports actual capabilities; fails if unavailable.","inputSchema":id}),
        json!({"name":"vm_status","description":"Get VM configuration and process state; running does not assert guest health.","inputSchema":id}),
        json!({"name":"vm_capabilities","description":"Report implemented operations, explicitly unsupported integrations, and known backend limits of macOS Guests (known_limits).","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
        json!({"name":"vm_snapshot_list","description":"List named snapshot metadata for one VM. Capture integrity is separate from a successful restore. Hidden incomplete captures are never reported as complete.","inputSchema":id}),
        json!({"name":"vm_snapshot_status","description":"Query one snapshot operation by its returned operation_id. A timeout or client disconnect does not cancel an operation; inspect its terminal state before retrying.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"operation_id":{"type":"string"}},"required":["id","operation_id"],"additionalProperties":false}}),
    ];
    // After the screen tools, so observe-only listings keep their order.
    tools.splice(4..4, desktop_tools::tools(input));
    if lifecycle {
        tools.push(json!({"name":"vm_snapshot_create","description":"Create a named full-state Linux snapshot using the explicitly supported backend/device configuration. Returns an operation_id to poll. Never falls back to a disk-only clone. Unsupported configurations or unresolved Guest work are refused.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"},"kind":{"type":"string","enum":["full_state"]}},"required":["id","name","kind"],"additionalProperties":false}}));
        tools.push(json!({"name":"vm_snapshot_restore","description":"Restore one named full-state Linux snapshot into a stopped VM. Requires explicit confirmation equal to the VM id, verifies compatibility/integrity, preserves the current disk, and loads a fresh private disk generation without cold booting. Returns an operation_id; poll its state before retrying.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"},"confirm":{"type":"string"}},"required":["id","name","confirm"],"additionalProperties":false}}));
        tools.push(json!({"name":"vm_snapshot_delete","description":"Delete exactly one named snapshot. Requires explicit confirmation equal to its name. Does not delete the VM, original disk or other snapshots.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"},"confirm":{"type":"string"}},"required":["id","name","confirm"],"additionalProperties":false}}));
        tools.push(json!({"name":"vm_snapshot_reconcile","description":"Reconcile an interrupted create or restore only with its durable retirement proof and no live disk or persistent-identity owner. Create requires an admission-time witness; restore locks both disk generations. Confirmation equals VM id. Preserves history and artifacts; never claims capture or restore success.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"operation_id":{"type":"string"},"confirm":{"type":"string"}},"required":["id","operation_id","confirm"],"additionalProperties":false}}));
        tools.push(json!({"name":"vm_snapshot_resume_source","description":"Recover the same live Apple Linux source after an unsuccessful capture, without restoring a snapshot or changing disks. Requires confirmation equal to VM id, the original owned worker and a returned capture channel. Busy or unknown outcomes stay fenced; poll snapshot status before retrying. Successful recovery preserves the failed capture history.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"operation_id":{"type":"string"},"confirm":{"type":"string"}},"required":["id","operation_id","confirm"],"additionalProperties":false}}));
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
        properties["qemu_graphics_profile"] = json!({"type":"string","enum":["virgl","venus-moltenvk-experimental","virtio-2d"],"default":"virgl","description":"Linux QEMU graphics profile. virtio-2d is an explicit unaccelerated console, not a 3D fallback. Experimental Venus requires a trusted runtime pre-registered by the local daemon operator; this option grants no binary paths, environment overrides or Guest execution capability."});
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
        tools.push(json!({"name":"vm_exec_start","description":"Start a bounded async guest command; returns a job id to poll. Unknown or reconnecting Guests keep a 30000 ms host limit; only opted-in QEMU Linux exec-v2 allows up to 120000 ms once vm_guest_info reports its bound no-reconnect async policy. A longer unsupported request is rejected before Guest dispatch. The guest kills the command's process group at timeout_ms (default 5000), on cancel, or when this MCP session ends.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"argv":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":128},"timeout_ms":{"type":"integer","minimum":1,"maximum":ASYNC_EXEC_MAX_TIMEOUT_MS}},"required":["id","argv"],"additionalProperties":false}}));
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
            ("input_text", input::text_tool_properties(), vec!["text"]),
        ] {
            let mut properties = extra;
            for k in ["id", "frame_id"] {
                properties[k] = json!({"type":"string"});
            }
            for k in ["width", "height", "source_generation"] {
                properties[k] = json!({"type":"integer"});
            }
            properties["observe"] = agent_view::observe_schema();
            let mut required = required;
            required.extend(["id", "frame_id", "width", "height", "source_generation"]);
            tools.push(json!({"name":format!("vm_{name}"),"description":if name == "input_text" { "Type Unicode text into the focused application of a Linux Guest on QEMU through the Guest service (XTEST with temporary keycodes), not physical key codes. Needs a screenshot younger than 5 s (or of an unchanged screen), qemu_guest_exec and a Guest service started with --allow-text-input, else fails as unsupported. A failure reports how many characters were delivered. The Guest X server acknowledges key events; application acceptance is not observed." } else { "Bounded physical input to a QEMU console, Apple Virtualization Linux desktop or opted-in macOS Guest. Needs a screenshot of this VM with matching generation and dimensions, younger than 5 s or of a screen unchanged since (digest or console draw sequence). Pass observe to get the result and a fresh frame for the next input in one call. Dispatch does not prove app acceptance; no Unicode/layout translation; vm_guest_info lists supported keys." },"inputSchema":input::coordinate_tool_schema(name, json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}))}));
        }
        tools.push(agent_view::batch_tool());
        tools.push(json!({"name":"vm_control_lease","description":"Who controls this VM's input. A person typing or clicking in an open console window holds it, and agent input (vm_input_*, window and accessibility actions, cua_* input) is refused with person_in_control until they have been quiet for quiet_ms or close the window; their input always wins. action status (default) reports holder (person, agent or none), viewers and agent_may_act_in_ms; release gives the agent's lease back until its next input.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"action":{"type":"string","enum":["status","release"]}},"required":["id"],"additionalProperties":false}}));
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
    mcp_events::extend_tools(&mut tools);
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
    mcp_with_tools(
        dir,
        lifecycle,
        execute,
        files,
        input,
        tool_groups::default(),
    )
}
pub fn mcp_with_tools(
    dir: &Path,
    lifecycle: bool,
    execute: bool,
    files: bool,
    input: bool,
    groups: BTreeSet<&'static str>,
) -> Result<()> {
    let options = McpOptions {
        lifecycle,
        execute,
        files,
        input,
        ..McpOptions::default()
    };
    mcp_serve(dir, &options, groups)
}
/// What one stdio MCP session may call (`--allow-*`), and the VM a per-VM endpoint serves.
#[derive(Clone, Debug, Default)]
pub struct McpOptions {
    pub lifecycle: bool,
    pub execute: bool,
    pub files: bool,
    pub input: bool,
    /// cua-driver `browser_*` tools that act on pages.
    pub browser: bool,
    /// cua-driver clipboard tools.
    pub clipboard: bool,
    /// `--vm ID`: serve that VM's cua-driver tools under their own names, plus its
    /// checkpoint and snapshot tools, instead of the VM tools.
    pub vm: Option<String>,
}
impl McpOptions {
    pub fn scopes(&self) -> cua_bridge::Scopes {
        cua_bridge::Scopes {
            input: self.input,
            browser: self.browser,
            clipboard: self.clipboard,
            execute: self.execute,
        }
    }
}
/// Every tool these permissions grant on the main MCP: the VM tools and the `cua` group.
pub fn tools_for(options: &McpOptions) -> Vec<Value> {
    let mut tools = tools_with_input(
        options.lifecycle,
        options.execute,
        options.files,
        options.input,
    )["tools"]
        .as_array()
        .unwrap()
        .clone();
    tools.extend(cua_bridge::curated_tools(options.scopes()));
    tools
}
/// The per-VM endpoint's tools: the driver's own (scope-filtered by the bridge) and the
/// VM-level ones. A driver that cannot answer (VM stopped, image without cua-driver)
/// leaves the VM-level tools.
fn endpoint_tools(dir: &Path, vm: &str, options: &McpOptions) -> Vec<Value> {
    let mut tools = call(
        dir,
        "driver_rpc",
        json!({"id":vm,"list":true,"scopes":options.scopes().to_value()}),
    )
    .ok()
    .and_then(|list| list["tools"].as_array().cloned())
    .unwrap_or_default();
    tools.extend(cua_bridge::vm_level_tools(&tools_for(options), vm));
    tools
}
/// A VM tool's result as MCP text content.
fn text_content(result: Result<Value>) -> Result<Value> {
    Ok(match result {
        Ok(v) => {
            json!({"content":[{"type":"text","text":serde_json::to_string(&v)?}],"isError":false})
        }
        Err(e) => json!({"content":[{"type":"text","text":e.to_string()}],"isError":true}),
    })
}
/// A driver result is already MCP content; a failed exchange becomes an error result.
fn driver_content(result: Result<Value>) -> Value {
    result.unwrap_or_else(
        |e| json!({"content":[{"type":"text","text":e.to_string()}],"isError":true}),
    )
}
/// The stdio MCP server. `groups` narrows the listed tools within the permissions;
/// `vm_tools_enable` widens it again up to them. With `options.vm` it is that VM's
/// endpoint instead: no groups, resources or VM tools beyond the VM-level ones.
pub fn mcp_serve(
    dir: &Path,
    options: &McpOptions,
    mut groups: BTreeSet<&'static str>,
) -> Result<()> {
    let permitted = || tools_for(options);
    let endpoint = options.vm.as_deref();
    if let Some(vm) = endpoint {
        validate_id(vm)?;
    }
    let scopes = options.scopes();
    let execute = options.execute;
    let list_changed = endpoint.is_none() && !tool_groups::hidden(&permitted(), &groups).is_empty();
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let out = mcp_events::Out::new(std::io::stdout());
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
            if await_peer_disconnect(
                &[libc::STDIN_FILENO, libc::STDOUT_FILENO],
                PEER_MONITOR_WAIT,
            ) {
                monitor_disconnected.store(true, Ordering::Release);
                let _ = monitor_cleanup.finish();
                break;
            }
        }
    });
    let events = mcp_events::Session::new(dir, out.clone(), execute);
    let protocol_result = (|| -> Result<()> {
        let mut agent = agent_view::Session::default();
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
                    out.send(&rpc_error(Value::Null, -32700, "Parse error"))?;
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
                out.send(&rpc_error(Value::Null, -32600, "Invalid request"))?;
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
            let progress = if ready && method == "tools/call" {
                mcp_events::Progress::start(
                    &out,
                    &request,
                    request["params"]["name"].as_str().unwrap_or("tool"),
                )
            } else {
                None
            };
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
                    if let Some(vm) = endpoint {
                        let reply = json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":VERSION,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"veynode-vm-cua","version":env!("CARGO_PKG_VERSION")},"instructions":format!("cua-driver in the Linux Guest desktop of VM {vm}, with its own tool names and schemas, limited to this session's scopes (observe by default; input, browser, clipboard and execute are granted separately), plus this VM's checkpoint and snapshot tools. install_extension, check_for_update, install_ffmpeg and set_config are never available. `effect:\"unverifiable\"` and `unknown` are not success. A Guest image without cua-driver lists only the VM-level tools.")}});
                        out.send(&reply)?;
                        continue;
                    }
                    let mut reply = json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":VERSION,"capabilities":{"tools":{"listChanged":list_changed}},"serverInfo":{"name":"veynode-vm","version":env!("CARGO_PKG_VERSION")},"instructions":"Local trusted-user daemon. Observe is default; server --allow-lifecycle, --allow-execute, --allow-files and --allow-input each add their own tools, and --tools lists only some tool groups (vm_tools_enable, when listed, adds the rest). Commands need enabled Linux Guest async jobs; macOS legacy execution is not exposed by vm_exec, but container_exec runs commands in veynodevm macOS containers over SSH. Commands run under a guest timeout; the guest kills a command's process group when it times out, is cancelled or this MCP session ends. File transfer needs an enabled Guest files root: a dedicated Linux files account, or macOS --allow-files --files-root with a private directory owned by the service UID, not isolated from other same-UID programs. Query vm_guest_info for actual capabilities, file_policy, macOS permission availability and supported physical keys; Guest denial is an error, not permission to use another path. Screenshots need QEMU capture, an Apple Virtualization Linux desktop container (--desktop; captured and driven by the guest agent with no Guest setup), a veynodevm macOS container (Guest Screen Sharing; no Guest permission grants) or macOS Guest --allow-observe with existing Screen Recording permission. On Linux desktop and macOS containers the desktop and accessibility tools read windows, applications and the accessibility tree without a screenshot; acting on them needs input permission. Physical input needs one of those screenshot backends (macOS Guests also --allow-input with existing Accessibility permission) and a matching screenshot younger than 5 seconds or of an unchanged screen; pass observe on input and accessibility actions to get what changed and a fresh frame in the same call. Accessibility trees default to compact lines with short refs (eN) valid for this session. Physical keys carry no Unicode text or keyboard-layout translation. Backend input acknowledgement does not prove application acceptance. VM start returns before guest readiness."}});
                    mcp_events::initialize(&mut reply);
                    reply
                }
            } else if method == "ping" {
                json!({"jsonrpc":"2.0","id":id,"result":{}})
            } else if !ready {
                rpc_error(
                    id,
                    -32000,
                    "Initialize and send notifications/initialized first",
                )
            } else if let Some(vm) = endpoint {
                match method {
                    "tools/list" => {
                        json!({"jsonrpc":"2.0","id":id,"result":{"tools":endpoint_tools(dir, vm, options)}})
                    }
                    "tools/call" => {
                        let name = request["params"]["name"].as_str().unwrap_or("");
                        let args = request["params"]
                            .get("arguments")
                            .cloned()
                            .unwrap_or(json!({}));
                        if !args.is_object() {
                            rpc_error(id, -32602, "Invalid tool arguments")
                        } else {
                            let vm_tools = cua_bridge::vm_level_tools(&permitted(), vm);
                            let content = match cua_bridge::endpoint_call(
                                name, args, vm, scopes, &vm_tools,
                            ) {
                                cua_bridge::EndpointCall::Vm(name, args) => {
                                    text_content(if name.starts_with("container_") {
                                        container_tools::call(dir, &name, &args)
                                    } else {
                                        call(dir, name.strip_prefix("vm_").unwrap(), args)
                                    })?
                                }
                                cua_bridge::EndpointCall::Driver(rpc) => {
                                    driver_content(call(dir, "driver_rpc", rpc))
                                }
                            };
                            json!({"jsonrpc":"2.0","id":id,"result":content})
                        }
                    }
                    _ => rpc_error(id, -32601, "Method not found"),
                }
            } else if let Some(result) = events.handle(method, &request["params"]) {
                match result {
                    Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                    Err(error) => rpc_error(id, -32002, error),
                }
            } else if method == "tools/list" {
                json!({"jsonrpc":"2.0","id":id,"result":{"tools":tool_groups::select(permitted(), &groups)}})
            } else if method == "tools/call" {
                let name = request["params"]["name"].as_str().unwrap_or("");
                let listed = tool_groups::select(permitted(), &groups);
                let known = listed.iter().find(|t| t["name"] == name);
                if known.is_none() {
                    let message = if permitted().iter().any(|t| t["name"] == name) {
                        "Tool is in a group this session has not enabled; call vm_tools_enable"
                    } else {
                        "Tool unknown or permission not granted"
                    };
                    rpc_error(id, -32602, message)
                } else {
                    let mut args = request["params"]
                        .get("arguments")
                        .cloned()
                        .unwrap_or(json!({}));
                    let schema = known.map_or(Value::Null, |t| t["inputSchema"].clone());
                    // cua tools list their main arguments only; the driver validates the rest.
                    let open = schema["additionalProperties"] == true;
                    let valid = args.as_object().is_some_and(|a| {
                        open || a.keys().all(|k| schema["properties"].get(k).is_some())
                    }) && schema["required"]
                        .as_array()
                        .is_none_or(|r| r.iter().all(|k| args.get(k.as_str().unwrap()).is_some()));
                    if !valid {
                        rpc_error(id, -32602, "Invalid tool arguments")
                    } else if name == tool_groups::ENABLE_TOOL {
                        let offered = tool_groups::hidden(&permitted(), &groups);
                        let asked: Option<BTreeSet<&'static str>> = args["groups"]
                            .as_array()
                            .and_then(|a| {
                                a.iter()
                                    .map(|g| g.as_str().and_then(|g| offered.get(g).copied()))
                                    .collect()
                            })
                            .filter(|asked: &BTreeSet<_>| !asked.is_empty());
                        match asked {
                            None => rpc_error(id, -32602, "Invalid tool arguments"),
                            Some(asked) => {
                                let before = tool_groups::select(permitted(), &groups);
                                groups.extend(asked.iter());
                                let added: Vec<_> = tool_groups::select(permitted(), &groups)
                                    .into_iter()
                                    .filter(|t| !before.iter().any(|b| b["name"] == t["name"]))
                                    .map(|t| t["name"].clone())
                                    .collect();
                                let result = json!({"enabled":asked,"added_tools":added});
                                // The reply goes first; the notification follows as this
                                // request's last frame, so the client refetches the list.
                                out.send(
                                    &json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":result.to_string()}],"isError":false}}),
                                )?;
                                json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"})
                            }
                        }
                    } else {
                        // The session owner lets session_end cancel this session's
                        // jobs and file transfers when the client goes away.
                        if name.starts_with("vm_file_")
                            || ["vm_exec", "vm_exec_start"].contains(&name)
                        {
                            args["owner"] = json!(owner);
                        }
                        let result = if agent_view::handles(name) {
                            agent.call(&mut agent_view::Live(dir), name, args)
                        } else if name.starts_with(cua_bridge::PREFIX) {
                            let tool = name.strip_prefix(cua_bridge::PREFIX).unwrap();
                            let vm = args["vm"].as_str().unwrap_or_default().to_owned();
                            match agent.ocr_token_call(&vm, tool, &args) {
                                Some(answer) => Ok(answer),
                                None => {
                                    let outcome = cua_bridge::curated_call(name, args, scopes)
                                        .and_then(|rpc| call(dir, "driver_rpc", rpc));
                                    // No driver in this Guest: the host reads the screen.
                                    if tool == "get_window_state"
                                        && cua_bridge::helperless(&outcome)
                                    {
                                        match agent
                                            .ocr_window_state(&mut agent_view::Live(dir), &vm)
                                        {
                                            Ok(answer) => Ok(answer),
                                            Err(ocr) => Ok(driver_content(outcome.map(|mut r| {
                                                r["_meta"]["veynode/host_ocr_error"] =
                                                    json!(ocr.to_string());
                                                r
                                            }))),
                                        }
                                    } else if tool == "verify_state"
                                        && cua_bridge::helperless(&outcome)
                                    {
                                        // Nothing in the Guest can check a predicate: unknown, never an error.
                                        let structured = json!({"status":"unknown","reason":"driver_unavailable","hint":"This Guest has no cua-driver; read the screen with cua_get_window_state (host OCR) or vm_screen_text and compare yourself."});
                                        Ok(
                                            json!({"content":[{"type":"text","text":structured.to_string()}],"structuredContent":structured,"isError":false}),
                                        )
                                    } else {
                                        Ok(driver_content(outcome))
                                    }
                                }
                            }
                        } else if name.starts_with("container_") || name.starts_with("image_") {
                            container_tools::call(dir, name, &args)
                        } else if name == "vm_exec_poll" && args.get("wait_ms").is_some() {
                            mcp_events::exec_poll_wait(dir, args)
                        } else {
                            call(dir, name.strip_prefix("vm_").unwrap(), args)
                        };
                        let content = match result {
                            // Already MCP content.
                            Ok(v)
                                if agent_view::handles(name)
                                    || name.starts_with(cua_bridge::PREFIX) =>
                            {
                                v
                            }
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
            if let Some(progress) = progress {
                progress.finish();
            }
            out.send(&response)?;
        }
        Ok(())
    })();
    drop(events);
    let cleanup_result = cleanup.state.finish();
    protocol_result.and(cleanup_result)
}
#[cfg(test)]
mod cua_bridge_tests;
#[cfg(test)]
mod mcp_cleanup_tests;
#[cfg(test)]
mod registry_load_tests;

/// Unit fixtures entering the private Manager directly must satisfy the same
/// canonical0700 state-directory contract as serve_with_worker_key_event_log.
#[cfg(test)]
pub(crate) fn test_private_state(path: &Path) -> PathBuf {
    let canonical = path.canonicalize().unwrap();
    let metadata = fs::symlink_metadata(&canonical).unwrap();
    assert!(metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() });
    assert_ne!(canonical, Path::new("/"));
    fs::set_permissions(&canonical, fs::Permissions::from_mode(0o700)).unwrap();
    canonical
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exec_identity_passes_only_valid_user_env_and_workdir() {
        assert!(exec_identity(&json!({"argv":["id"]})).unwrap().is_empty());
        let fields = exec_identity(
            &json!({"user":"veynode","env":["A=1","B="],"workdir":"/srv","argv":["id"]}),
        )
        .unwrap();
        assert_eq!(
            fields,
            vec![
                ("user", json!("veynode")),
                ("env", json!(["A=1", "B="])),
                ("workdir", json!("/srv")),
            ]
        );
        for bad in [
            json!({"user":""}),
            json!({"user":7}),
            json!({"env":["NOEQUALS"]}),
            json!({"env":["=value"]}),
            json!({"env":"A=1"}),
            json!({"workdir":"relative"}),
        ] {
            assert!(exec_identity(&bad).is_err(), "{bad}");
        }
    }
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
            guest_async_timeout: Arc::new(Mutex::new(GuestAsyncTimeout::default())),
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
    fn peer_fixture() -> (tempfile::TempDir, Arc<Mutex<Manager>>) {
        let temp = tempfile::tempdir().unwrap();
        let mut manager = Manager::load(
            test_private_state(temp.path()),
            "/unused/apple".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        manager.configs.insert(
            "test".into(),
            serde_json::from_value(json!({"id":"test","guest":"linux","cpus":1,"memory_mib":512}))
                .unwrap(),
        );
        (temp, Arc::new(Mutex::new(manager)))
    }
    fn vm_present(manager: &Arc<Mutex<Manager>>) -> bool {
        manager.lock().unwrap().configs.contains_key("test")
    }
    /// A real listening socket, so the kernel records the connecting process's credential exactly as for `control.sock`.
    fn connected(temp: &tempfile::TempDir) -> (UnixStream, UnixStream) {
        let path = temp.path().join("peer.sock");
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }
    fn unread_bytes(stream: &UnixStream) -> usize {
        let mut pending: libc::c_int = 0;
        assert_eq!(
            unsafe { libc::ioctl(stream.as_raw_fd(), libc::FIONREAD, &mut pending) },
            0
        );
        pending as usize
    }
    fn remaining(mut client: UnixStream) -> Vec<u8> {
        // The server end is already dropped, so this cannot block; Darwin refuses a timeout on such a socket.
        // Closing a socket that still holds the unread request may surface as a reset instead of a clean EOF. Either
        // way the peer has hung up; what matters is every byte delivered before that.
        let mut bytes = Vec::new();
        if let Err(error) = client.read_to_end(&mut bytes) {
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        }
        bytes
    }
    #[test]
    fn control_socket_serves_a_same_uid_peer_through_the_kernel_credential() {
        let (temp, manager) = peer_fixture();
        let (mut client, server) = connected(&temp);
        assert_eq!(peer_euid(&server).unwrap(), unsafe { libc::geteuid() });
        write_json(
            &mut client,
            &json!({"method":"delete","arguments":{"id":"test","confirm":"test"}}),
        )
        .unwrap();
        serve_connection(
            server,
            manager.clone(),
            std::time::Instant::now(),
            peer_euid,
        )
        .unwrap();
        let reply: Value =
            serde_json::from_slice(&read_frame(&mut BufReader::new(client)).unwrap().unwrap())
                .unwrap();
        assert_eq!(reply["result"]["deleted"], true, "{reply}");
        assert!(!vm_present(&manager));
    }
    #[test]
    fn control_socket_refuses_a_foreign_uid_before_reading_any_request_byte() {
        let (temp, manager) = peer_fixture();
        let foreign = unsafe { libc::geteuid() }.wrapping_add(1);
        let valid = json!({"method":"delete","arguments":{"id":"test","confirm":"test"}});
        for request in [
            serde_json::to_vec(&valid).unwrap(),
            b"not json at all".to_vec(),
        ] {
            let (mut client, server) = connected(&temp);
            client.write_all(&request).unwrap();
            client.write_all(b"\n").unwrap();
            let sent = request.len() + 1;
            let error = serve_connection(
                server,
                manager.clone(),
                std::time::Instant::now(),
                |stream: &UnixStream| {
                    // The uid is consulted while the whole request is still unread in the socket buffer.
                    assert_eq!(unread_bytes(stream), sent);
                    Ok(foreign)
                },
            )
            .unwrap_err()
            .to_string();
            // The same refusal for a valid and an unparseable request: the bytes were never interpreted.
            assert_eq!(
                error,
                "control connection refused: peer uid differs from daemon uid"
            );
            assert_eq!(remaining(client), b"", "a refused peer receives no reply");
            assert!(vm_present(&manager));
        }
        // Control: the identical valid request is served, with its side effect, once the peer is the daemon's own uid.
        let (mut client, server) = connected(&temp);
        write_json(&mut client, &valid).unwrap();
        serve_connection(
            server,
            manager.clone(),
            std::time::Instant::now(),
            peer_euid,
        )
        .unwrap();
        assert!(!vm_present(&manager));
    }
    #[test]
    fn control_socket_fails_closed_when_the_peer_credential_is_unreadable() {
        let (temp, manager) = peer_fixture();
        let (mut client, server) = connected(&temp);
        write_json(
            &mut client,
            &json!({"method":"delete","arguments":{"id":"test","confirm":"test"}}),
        )
        .unwrap();
        let error = serve_connection(
            server,
            manager.clone(),
            std::time::Instant::now(),
            |_: &UnixStream| bail!("credential read failed"),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "control connection refused: peer credential unavailable"
        );
        assert_eq!(remaining(client), b"");
        assert!(vm_present(&manager));
        // The production reader itself fails, rather than reporting some uid, on a socket that has no peer.
        let unconnected = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        assert!(unconnected >= 0);
        let unconnected =
            unsafe { <UnixStream as std::os::fd::FromRawFd>::from_raw_fd(unconnected) };
        assert!(peer_euid(&unconnected).is_err());
        assert!(admit_peer(&unconnected, peer_euid).is_err());
    }
    #[test]
    fn control_socket_still_commits_cleanup_from_a_same_uid_peer_that_already_closed() {
        // The credential is recorded at connect time, so the existing disconnect-tolerant cleanup path survives admission.
        let (temp, manager) = peer_fixture();
        let (mut client, server) = connected(&temp);
        write_json(
            &mut client,
            &json!({"method":"delete","arguments":{"id":"test","confirm":"test"}}),
        )
        .unwrap();
        drop(client);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !peer_disconnected(&[server.as_raw_fd()]) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(peer_disconnected(&[server.as_raw_fd()]));
        assert_eq!(peer_euid(&server).unwrap(), unsafe { libc::geteuid() });
        let cleanup = serve_connection(
            server,
            manager.clone(),
            std::time::Instant::now(),
            peer_euid,
        );
        assert!(cleanup.is_err()); // Only the reply write fails.
        assert!(!vm_present(&manager));
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
            test_private_state(temp.path()),
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
                guest_async_timeout: Arc::new(Mutex::new(GuestAsyncTimeout::default())),
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
    include!("exec_timeout_tests.rs");
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
    fn blocking_peer_observation_wakes_on_hangup_and_never_spins_on_readable_data() {
        use std::os::fd::AsRawFd;
        fn pipe() -> (File, File) {
            use std::os::fd::FromRawFd;
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
        }
        fn observe_late_close<T: Send + 'static>(fd: std::os::fd::RawFd, peer: T) {
            let closer = thread::spawn(move || {
                thread::sleep(Duration::from_millis(150));
                drop(peer);
            });
            let started = std::time::Instant::now();
            assert!(await_peer_disconnect(&[fd], Duration::from_secs(5)));
            let elapsed = started.elapsed();
            assert!(elapsed >= Duration::from_millis(100), "elapsed={elapsed:?}");
            assert!(elapsed < Duration::from_secs(2), "elapsed={elapsed:?}");
            closer.join().unwrap();
            assert!(peer_disconnected(&[fd]));
        }
        let limit = Duration::from_secs(5);
        // Each hang-up must end one blocking observation long before its 5 s limit, for the
        // descriptor kinds an MCP stdio transport uses: both pipe ends and a Unix socket.
        let (read, write) = pipe();
        observe_late_close(read.as_raw_fd(), write);
        let (read, write) = pipe();
        observe_late_close(write.as_raw_fd(), read);
        let (local, remote) = UnixStream::pair().unwrap();
        observe_late_close(local.as_raw_fd(), remote);
        // A connected, silent peer: blocks for the whole limit and reports nothing.
        let (local, mut remote) = UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        assert!(!await_peer_disconnect(
            &[local.as_raw_fd()],
            Duration::from_millis(200)
        ));
        assert!(started.elapsed() >= Duration::from_millis(190));
        // Queued, unread bytes are not a disconnect and must cost the original 50 ms step.
        remote.write_all(b"pending bytes").unwrap();
        let started = std::time::Instant::now();
        assert!(!await_peer_disconnect(&[local.as_raw_fd()], limit));
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(45), "elapsed={elapsed:?}");
        assert!(elapsed < Duration::from_secs(2), "elapsed={elapsed:?}");
        let mut bytes = [0; 13];
        (&local).read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"pending bytes");
    }
    #[test]
    fn exit_monitor_parks_without_live_workers_and_resumes_when_one_starts() {
        let temp = tempfile::tempdir().unwrap();
        let manager = Arc::new(Mutex::new(
            Manager::load(
                test_private_state(temp.path()),
                "/unused".into(),
                MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
            )
            .unwrap(),
        ));
        assert_eq!(exit_monitor_pass(&manager), EXIT_MONITOR_IDLE);
        let before = EXIT_MONITOR_PASSES.load(Ordering::Relaxed);
        spawn_exit_monitor(&manager);
        thread::sleep(Duration::from_millis(600));
        // The fixed 100 ms loop made about six passes here; a parked monitor makes one.
        let idle = EXIT_MONITOR_PASSES.load(Ordering::Relaxed) - before;
        assert!((1..=2).contains(&idle), "idle passes={idle}");
        let mut child = Command::new("/bin/cat")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let stdin = child.stdin.take().unwrap();
        manager.lock().unwrap().started(
            "mac".into(),
            Running {
                bridge: Arc::new(Mutex::new(bridge::State::new().unwrap())),
                child,
                stdin,
                pending: Arc::new(Mutex::new(BTreeMap::new())),
                assigned_memory_mib: 512,
                expected_exit: false,
                execution_quiescing: false,
                guest_async_timeout: Arc::new(Mutex::new(GuestAsyncTimeout::default())),
                exit_status: None,
                disk_identity: None,
                state: Arc::new(Mutex::new(json!({"state":"running"}))),
                next: 1,
            },
        );
        // Woken by registration, the monitor is back on its 100 ms cadence.
        thread::sleep(Duration::from_millis(450));
        let live = EXIT_MONITOR_PASSES.load(Ordering::Relaxed) - before - idle;
        assert!(live >= 3, "live passes={live}");
        assert!(manager.lock().unwrap().running["mac"].exit_status.is_none());
        // Only the monitor may reap here: this thread never calls poll_exit.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while manager.lock().unwrap().running["mac"].exit_status.is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "background monitor did not reap the exited worker"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(exit_monitor_pass(&manager), EXIT_MONITOR_IDLE);
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
    fn bounded_logger_returns_and_counts_drops_when_sink_blocks_forever() {
        struct Blocked(mpsc::Sender<()>);
        impl Write for Blocked {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                self.0.send(()).unwrap();
                loop {
                    thread::park();
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (entered, waiting) = mpsc::channel();
        let logger = Arc::new(LineLogger::new(Blocked(entered), 2));
        logger.send("first");
        waiting.recv_timeout(Duration::from_secs(1)).unwrap();
        let producer = logger.clone();
        let (done, returned) = mpsc::channel();
        let caller = thread::spawn(move || {
            for _ in 0..10 {
                producer.send("queued");
            }
            producer.send(&"x".repeat(LOG_LINE_LIMIT + 1));
            done.send(()).unwrap();
        });
        returned.recv_timeout(Duration::from_secs(1)).unwrap();
        caller.join().unwrap();
        assert_eq!(logger.dropped.load(Ordering::Relaxed), 9);
    }
    #[test]
    fn logger_flush_writes_queued_lines_and_flushes_sink_before_acknowledging() {
        #[derive(Clone)]
        struct Recording(Arc<Mutex<(Vec<u8>, bool)>>);
        impl Write for Recording {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.0.lock().unwrap().1 = true;
                Ok(())
            }
        }
        let sink = Recording(Arc::new(Mutex::new((Vec::new(), false))));
        let logger = LineLogger::new(sink.clone(), 4);
        logger.send("first");
        logger.send("second");
        assert!(logger.flush(Duration::from_secs(1)));
        let recorded = sink.0.lock().unwrap();
        assert_eq!(recorded.0, b"first\nsecond\n");
        assert!(recorded.1);
        assert_eq!(logger.dropped.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn logger_flush_deadline_bounds_full_queue_and_blocked_sink_acknowledgement() {
        struct Blocked(mpsc::Sender<()>);
        impl Write for Blocked {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                self.0.send(()).unwrap();
                loop {
                    thread::park();
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for capacity in [1, 2] {
            let (entered, waiting) = mpsc::channel();
            let logger = LineLogger::new(Blocked(entered), capacity);
            logger.send("blocked");
            waiting.recv_timeout(Duration::from_secs(1)).unwrap();
            logger.send("queued");
            let (done, returned) = mpsc::channel();
            let caller = thread::spawn(move || {
                let started = std::time::Instant::now();
                assert!(!logger.flush(Duration::from_millis(50)));
                done.send(started.elapsed()).unwrap();
            });
            let elapsed = returned.recv_timeout(Duration::from_millis(250)).unwrap();
            // Allow scheduling overhead, but never wait for the permanently blocked writer.
            assert!(elapsed < Duration::from_millis(250), "{elapsed:?}");
            caller.join().unwrap();
        }
    }
    #[test]
    fn scaled_screenshot_capability_describes_unchanged_png_fallback() {
        let features = control_features(Backend::QemuHvf, "linux");
        let reason = features["screenshot_scaled_jpeg"]["reason"]
            .as_str()
            .unwrap();
        assert!(reason.contains("needs a console broker and a helper that knows it"));
        assert!(reason
            .contains("an older helper returns its full-size PNG unchanged (check mime_type)"));
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
            guest_async_timeout: Arc::new(Mutex::new(GuestAsyncTimeout::default())),
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
            test_private_state(temp.path()),
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
            test_private_state(temp.path()),
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
            test_private_state(temp.path()),
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
    fn known_limits_state_the_two_macos_guest_graphics_limits_and_nothing_for_other_guests() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let limits = known_limits(Backend::AppleVirtualization, "macos");
        let limits = limits.as_array().unwrap();
        assert_eq!(
            limits
                .iter()
                .map(|limit| limit["code"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "macos_guest_metal_large_compute_pipelines",
                "macos_guest_opengl_not_accelerated"
            ]
        );
        for limit in limits {
            let object = limit.as_object().unwrap();
            assert_eq!(object.len(), KNOWN_LIMIT_KEYS.len(), "{limit}");
            for key in KNOWN_LIMIT_KEYS {
                assert!(
                    limit[key].as_str().is_some_and(|text| !text.is_empty()),
                    "{key}: {limit}"
                );
            }
            assert_eq!(
                limit["classification"],
                "backend limit of Apple's paravirtual graphics, not of this product's implementation"
            );
            // Scope of the measurement: one Guest version, one host, and no claim beyond it.
            let measured = limit["measured_on"].as_str().unwrap();
            for scope in [
                "macOS 26.6.2 Guest",
                "one host",
                "other macOS versions and hosts were not measured",
            ] {
                assert!(measured.contains(scope), "{scope}: {measured}");
            }
            let text = limit.to_string().to_lowercase();
            for overclaim in [
                "all macos",
                "any macos",
                "every macos",
                "permanent",
                "never",
                "always",
            ] {
                assert!(!text.contains(overclaim), "{overclaim}: {limit}");
            }
            assert!(
                limit["decision"]
                    .as_str()
                    .unwrap()
                    .starts_with("docs/decisions/"),
                "{limit}"
            );
            assert!(
                repo.join(limit["decision"].as_str().unwrap()).exists(),
                "{limit}"
            );
            // A cited record is one retained in this tree.
            if let Some(evidence) = limit.get("evidence") {
                assert!(
                    evidence
                        .as_str()
                        .unwrap()
                        .starts_with("archive/docs-evidence/"),
                    "{limit}"
                );
                assert!(repo.join(evidence.as_str().unwrap()).exists(), "{limit}");
            }
        }
        let metal = &limits[0];
        assert_eq!(metal["description"], "Large Metal compute pipelines, including the Blender Cycles shading kernels, are not created on the Guest's Metal device, so Cycles rendering on the Guest's Metal device is not available.");
        assert_eq!(
            metal["decision"],
            "docs/decisions/0023-macos-guest-metal-compute-limit.md"
        );
        for stated in [
            "Cycles renders on the Guest's CPUs",
            "no render request path",
            "no CPU fallback",
            "stall without an error",
        ] {
            assert!(
                metal["effect"].as_str().unwrap().contains(stated),
                "{stated}"
            );
        }
        let opengl = &limits[1];
        assert_eq!(opengl["description"], "The Guest has no accelerated OpenGL (CGL) renderer: an application that asks for OpenGL gets Apple Software Renderer.");
        assert_eq!(
            opengl["decision"],
            "docs/decisions/0017-macos-opengl-capability-gap.md"
        );
        for stated in [
            "Metal applications are not affected",
            "Apple Software Renderer 2.1",
            "not accepted as GPU acceleration",
        ] {
            assert!(
                opengl["effect"].as_str().unwrap().contains(stated),
                "{stated}"
            );
        }
        // No such entries for any other pair.
        for (backend, guest) in [
            (Backend::AppleVirtualization, "linux"),
            (Backend::AppleVirtualization, "windows"),
            (Backend::QemuHvf, "linux"),
            (Backend::QemuHvf, "windows"),
            (Backend::QemuHvf, "macos"),
        ] {
            assert_eq!(
                known_limits(backend, guest),
                json!([]),
                "{backend:?} {guest}"
            );
        }
    }
    fn assert_control_features_pair(backend: Backend, guest: &str) {
        let cells = control_features(backend, guest);
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert_eq!(cells.as_object().unwrap().len(), CONTROL_FEATURES.len());
        for name in CONTROL_FEATURES {
            let cell = &cells[name];
            assert!(
                FEATURE_STATUSES.contains(&cell["status"].as_str().unwrap()),
                "{name}: {cell}"
            );
            let reason = cell["reason"].as_str().unwrap();
            assert!(reason.len() > 20 && reason.len() < 260, "{name}: {cell}");
            if cell["status"] == "supported" {
                let evidence = cell["evidence"].as_str().unwrap();
                assert!(
                    evidence.starts_with("archive/docs-evidence/") && repo.join(evidence).is_file()
                );
            }
        }
        for name in ["balloon", "clipboard"] {
            assert_eq!(cells[name]["status"], "unsupported", "{name}");
        }
        assert_eq!(
            cells["snapshot"]["status"],
            if backend == Backend::AppleVirtualization && guest == "linux" {
                "implemented_unverified"
            } else {
                "unsupported"
            }
        );
        assert_eq!(cells["balloon"], cells["memory_reclaim"]);
        assert_eq!(cells["window_list"], cells["window_metadata"]);
        // The desktop tools serve Apple Virtualization desktop containers, macOS containers and Windows machines.
        if (backend == Backend::AppleVirtualization) != (guest == "windows") {
            assert_eq!(cells["application_launch_list"], cells["application_tools"]);
            assert_eq!(cells["accessibility_tree"], cells["accessibility_metadata"]);
            assert_eq!(
                cells["accessibility_tree"]["status"],
                "implemented_unverified"
            );
        } else {
            for name in ["application_launch_list", "accessibility_tree"] {
                assert_eq!(cells[name]["status"], "unsupported", "{name}");
            }
            let apps = cells["application_launch_list"]["reason"].as_str().unwrap();
            assert!(apps.contains("no vm_app_*") && apps.contains("Linux only"));
            if guest == "linux" {
                assert!(apps.contains("vm_exec_start"));
            }
        }
        if backend == Backend::QemuHvf && guest == "macos" {
            for name in CONTROL_FEATURES {
                assert_eq!(cells[name]["status"], "unsupported", "{name}");
            }
        } else {
            // All pre-existing cells retain their exact values, including Windows refusal reasons.
            let original = base_control_features(backend, guest);
            for name in &CONTROL_FEATURES[..18] {
                assert_eq!(cells[*name], original[*name], "{name}");
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let mut manager = Manager::load(
            test_private_state(temp.path()),
            "/unused".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        let report = manager
            .dispatch(json!({"method":"capabilities","arguments":{}}), &mut None)
            .unwrap();
        let backend_name = serde_json::to_value(backend).unwrap();
        assert_eq!(
            report["backends"][backend_name.as_str().unwrap()]["control_features"][guest],
            cells
        );
    }
    #[test]
    fn control_features_apple_macos_completeness() {
        assert_control_features_pair(Backend::AppleVirtualization, "macos");
    }
    #[test]
    fn control_features_apple_linux_completeness() {
        assert_control_features_pair(Backend::AppleVirtualization, "linux");
    }
    #[test]
    fn control_features_qemu_linux_completeness() {
        assert_control_features_pair(Backend::QemuHvf, "linux");
    }
    #[test]
    fn control_features_qemu_macos_completeness() {
        assert_control_features_pair(Backend::QemuHvf, "macos");
    }
    #[test]
    fn control_features_qemu_windows_completeness() {
        assert_control_features_pair(Backend::QemuHvf, "windows");
    }
    #[test]
    fn control_features_macos_gpu_requires_evidence_and_states_limits() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for backend in [Backend::AppleVirtualization, Backend::QemuHvf] {
            let cells = control_features(backend, "macos");
            let gpu = &cells["gpu_3d"];
            if gpu["status"] == "supported" {
                assert!(repo.join(gpu["evidence"].as_str().unwrap()).is_file());
                let reason = gpu["reason"].as_str().unwrap();
                for part in [
                    "Narrow",
                    "no accelerated OpenGL",
                    "0017",
                    "0023",
                    "large Metal compute/Cycles unavailable",
                    "known_limits",
                ] {
                    assert!(reason.contains(part), "{reason}");
                }
            }
        }
    }
    #[test]
    fn control_features_gpu_narrows_by_qemu_runtime_and_profile() {
        let temp = tempfile::tempdir().unwrap();
        let mut manager = Manager::load(
            test_private_state(temp.path()),
            "/unused".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        manager.configs.insert("linux".into(), serde_json::from_value(
            json!({"id":"linux","backend":"qemu-hvf","guest":"linux","cpus":1,"memory_mib":512})
        ).unwrap());
        let gpu = |manager: &mut Manager| {
            manager.view("linux").unwrap()["capabilities"]["control_features"]["gpu_3d"].clone()
        };
        assert_eq!(gpu(&mut manager)["status"], "unsupported");
        // Registration metadata only; no runtime is launched or validated by this fixture.
        manager.qemu = Some(QemuRegistration {
            worker: "/unused".into(),
            launcher: "/unused".into(),
            library: "/unused".into(),
            resources: temp.path().into(),
            spice_library: None,
            screenshot_helper: None,
            console_broker: None,
            console_helper: None,
            venus_runtime: None,
        });
        assert_eq!(
            gpu(&mut manager),
            control_features(Backend::QemuHvf, "linux")["gpu_3d"]
        );
        manager
            .configs
            .get_mut("linux")
            .unwrap()
            .qemu_graphics_profile = qemu_graphics::Profile::VenusMoltenvkExperimental;
        assert_eq!(gpu(&mut manager)["status"], "unsupported");
        manager.qemu.as_mut().unwrap().venus_runtime = Some("/unused".into());
        let venus = gpu(&mut manager);
        assert_eq!(venus["status"], "supported");
        assert!(venus["reason"]
            .as_str()
            .unwrap()
            .contains("no general compatibility, per-VM Guest verification or performance claim"));
        let evidence = venus["evidence"].as_str().unwrap();
        assert!(Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(evidence)
            .is_file());
        assert_eq!(
            manager.view("linux").unwrap()["capabilities"]["graphics_profile"]["guest_verified"],
            false
        );
        assert!(manager.running.is_empty());
    }
    #[test]
    fn windows_linux_and_windows_return_typed_unsupported_before_worker_lookup() {
        let tools = tools_list(false, false);
        let windows = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "vm_windows")
            .unwrap();
        assert!(windows["description"]
            .as_str()
            .unwrap()
            .contains("Linux and Windows Guests return unsupported"));
        let temp = tempfile::tempdir().unwrap();
        let mut manager = Manager::load(
            test_private_state(temp.path()),
            "/unused".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        // Inject refused Guest kinds to test dispatch without registering or starting a VM.
        for (backend, guest) in [
            (Backend::AppleVirtualization, "linux"),
            (Backend::QemuHvf, "linux"),
            (Backend::QemuHvf, "windows"),
            (Backend::QemuHvf, "macos"),
        ] {
            manager.configs.insert("observe".into(), serde_json::from_value(
                json!({"id":"observe","backend":backend,"guest":guest,"cpus":1,"memory_mib":512})
            ).unwrap());
            let error = manager
                .dispatch(
                    json!({"method":"windows","arguments":{"id":"observe"}}),
                    &mut None,
                )
                .unwrap_err()
                .to_string();
            assert_eq!(
                error,
                "unsupported: window enumeration requires enabled macOS Guest observation service"
            );
        }
        assert!(manager.running.is_empty());
    }
    #[test]
    fn control_features_state_every_cell_with_status_reason_and_real_guest_evidence() {
        // One row per feature; columns are Apple macOS, Apple Linux, QEMU Linux, QEMU Windows.
        const S: &str = "supported";
        const I: &str = "implemented_unverified";
        const P: &str = "requires_guest_permission";
        const N: &str = "unsupported";
        let table: [(&str, [&str; 4]); 23] = [
            ("lifecycle", [S, S, S, I]),
            ("command_execution", [N, S, S, I]),
            ("async_jobs", [N, S, S, N]),
            ("file_transfer", [S, S, N, I]),
            ("screenshot", [S, I, S, N]),
            ("screenshot_scaled_jpeg", [N, N, I, N]),
            ("pointer_key_input", [P, I, S, N]),
            ("unicode_text_input", [N, N, I, N]),
            ("window_metadata", [S, I, N, I]),
            ("accessibility_metadata", [I, I, N, I]),
            ("application_tools", [I, I, N, I]),
            ("clipboard", [N, N, N, N]),
            ("snapshot", [N, I, N, N]),
            ("memory_reclaim", [N, N, N, N]),
            ("multi_display_input_mapping", [N, N, N, N]),
            ("retina_input_mapping", [I, N, N, N]),
            ("shared_folders", [S, I, N, N]),
            ("guest_shutdown", [N, S, N, N]),
            ("balloon", [N, N, N, N]),
            ("gpu_3d", [S, N, S, I]),
            ("application_launch_list", [I, I, N, I]),
            ("accessibility_tree", [I, I, N, I]),
            ("window_list", [S, I, N, I]),
        ];
        assert_eq!(table.map(|(name, _)| name), CONTROL_FEATURES);
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let pairs = [
            (Backend::AppleVirtualization, "macos"),
            (Backend::AppleVirtualization, "linux"),
            (Backend::QemuHvf, "linux"),
            (Backend::QemuHvf, "windows"),
        ];
        for (column, (backend, guest)) in pairs.into_iter().enumerate() {
            let cells = control_features(backend, guest);
            assert_eq!(cells.as_object().unwrap().len(), table.len(), "{guest}");
            for (name, expected) in table {
                let cell = &cells[name];
                let at = format!("{backend:?} {guest} {name}: {cell}");
                assert_eq!(cell["status"], expected[column], "{at}");
                assert!(FEATURE_STATUSES.contains(&cell["status"].as_str().unwrap()));
                let reason = cell["reason"].as_str().expect(&at);
                assert!(reason.len() > 20 && reason.len() < 260, "{at}");
                // Supported means a real-Guest record that exists in this tree, and nothing else carries one.
                assert_eq!(
                    cell.get("evidence").is_some(),
                    expected[column] == S,
                    "{at}"
                );
                assert_eq!(
                    cell.as_object().unwrap().len(),
                    2 + usize::from(expected[column] == S)
                );
                if let Some(evidence) = cell.get("evidence") {
                    let evidence = evidence.as_str().unwrap();
                    assert!(evidence.starts_with("archive/docs-evidence/"), "{at}");
                    assert!(repo.join(evidence).exists(), "{at}");
                }
                // The older report never claims more than this one.
                let old = &product_capabilities(backend, guest)[name];
                if PRODUCT_CAPABILITIES.contains(&name) && old["status"] == N {
                    assert_eq!(cell, old, "{at}");
                }
            }
        }
        // Memory reclaim: neither backend returns Guest memory, and both Linux cells state the measurement.
        for (backend, guest, measured) in [
            (
                Backend::AppleVirtualization,
                "linux",
                "raised the host footprint by 933 MiB",
            ),
            (
                Backend::QemuHvf,
                "linux",
                "without lowering the host footprint",
            ),
        ] {
            let reason = control_features(backend, guest)["memory_reclaim"]["reason"].to_string();
            for part in ["measured 2026-10-12", measured] {
                assert!(reason.contains(part), "{reason}");
            }
        }
        let mac = control_features(Backend::AppleVirtualization, "macos");
        assert!(mac["pointer_key_input"]["reason"]
            .to_string()
            .contains("accessibility_permission_required"));
        assert!(mac["retina_input_mapping"]["reason"]
            .to_string()
            .contains("unit test"));
        assert!(
            control_features(Backend::QemuHvf, "windows")["clipboard"]["reason"]
                .to_string()
                .contains("not implemented for Windows guests")
        );

        // A registered VM reports the same cells, narrowed by its registration and by what this daemon has registered.
        let temp = tempfile::tempdir().unwrap();
        let kernel = temp.path().join("kernel");
        fs::write(&kernel, b"registry only, never started").unwrap();
        let mut manager = Manager::load(
            test_private_state(temp.path()),
            "/unused".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        for (id, config) in [
            (
                "exec",
                json!({"backend":"qemu-hvf","guest":"linux","qemu_guest_exec":true}),
            ),
            ("observer", json!({"backend":"qemu-hvf","guest":"linux"})),
            ("vzlinux", json!({"guest":"linux"})),
            ("mac", json!({"guest":"macos"})),
        ] {
            let mut config = config;
            config["id"] = json!(id);
            config["cpus"] = json!(1);
            config["memory_mib"] = json!(512);
            manager
                .configs
                .insert(id.into(), serde_json::from_value(config).unwrap());
        }
        let cells = |manager: &mut Manager, id: &str| {
            manager.view(id).unwrap()["capabilities"]["control_features"].clone()
        };
        assert_eq!(cells(&mut manager, "mac"), mac);
        // Known limits appear in a macOS Guest's status and in no other VM's.
        assert_eq!(
            manager.view("mac").unwrap()["capabilities"]["known_limits"],
            known_limits(Backend::AppleVirtualization, "macos")
        );
        for id in ["vzlinux", "exec"] {
            assert!(
                manager.view(id).unwrap()["capabilities"]
                    .get("known_limits")
                    .is_none(),
                "{id}"
            );
        }
        let mut without_identity = control_features(Backend::AppleVirtualization, "linux");
        without_identity["snapshot"] = json!({"status":"unsupported","reason":"This VM has no explicit persistent Linux machine_identity_dir. Snapshot capture refuses it; existing VMs are never silently assigned a new snapshot identity."});
        assert_eq!(cells(&mut manager, "vzlinux"), without_identity);
        assert_eq!(
            manager.view("vzlinux").unwrap()["capabilities"]["snapshot_kinds"]["full_state"],
            without_identity["snapshot"]
        );
        // Explicit registration opts in; capability reporting must not create or assign an identity.
        let identity = manager.dir.join("test-linux-identity");
        assert!(!identity.exists());
        manager
            .configs
            .get_mut("vzlinux")
            .unwrap()
            .machine_identity_dir = Some(identity.clone());
        assert_eq!(
            cells(&mut manager, "vzlinux"),
            control_features(Backend::AppleVirtualization, "linux")
        );
        let kinds = manager.view("vzlinux").unwrap()["capabilities"]["snapshot_kinds"].clone();
        assert_eq!(kinds["full_state"]["status"], I);
        assert_eq!(kinds["disk_rollback"]["named_create"], false);
        assert_eq!(kinds["disk_rollback"]["restore"], false);
        assert_eq!(kinds["disk_rollback"]["automatic_restore_protection"], true);
        assert_eq!(kinds["suspend"]["save"], false);
        assert_eq!(kinds["suspend"]["resume"], false);
        assert_eq!(kinds["operation_fenced"], false);
        assert!(!identity.exists());
        // No QEMU registration: screenshot and input are refused by this daemon, and the report says so.
        let bare = cells(&mut manager, "exec");
        assert_eq!(bare["gpu_3d"]["status"], N);
        assert!(bare["gpu_3d"]["reason"]
            .as_str()
            .unwrap()
            .contains("no registered QEMU runtime"));
        for name in [
            "screenshot",
            "screenshot_scaled_jpeg",
            "pointer_key_input",
            "unicode_text_input",
        ] {
            assert_eq!(bare[name]["status"], N, "{name}");
            assert!(bare[name]["reason"]
                .to_string()
                .contains("this daemon has no registered QEMU"));
        }
        assert_eq!(bare["command_execution"]["status"], S);
        manager.qemu = Some(QemuRegistration {
            worker: kernel.clone(),
            launcher: kernel.clone(),
            library: kernel.clone(),
            resources: temp.path().into(),
            spice_library: None,
            screenshot_helper: Some(kernel.clone()),
            console_broker: Some(kernel.clone()),
            console_helper: Some(kernel.clone()),
            venus_runtime: None,
        });
        assert_eq!(
            cells(&mut manager, "exec"),
            control_features(Backend::QemuHvf, "linux")
        );
        assert_eq!(
            manager.view("exec").unwrap()["capabilities"]["screenshot_scaled_jpeg"],
            true
        );
        assert_eq!(
            cells(&mut manager, "exec")["screenshot_scaled_jpeg"]["status"],
            I
        );
        manager.qemu.as_mut().unwrap().console_broker = None;
        let without_broker = manager.view("exec").unwrap();
        assert_eq!(without_broker["capabilities"]["screenshot"], true);
        assert_eq!(
            without_broker["capabilities"]["screenshot_scaled_jpeg"],
            false
        );
        assert_eq!(
            without_broker["capabilities"]["control_features"]["screenshot_scaled_jpeg"]["status"],
            N
        );
        manager.qemu.as_mut().unwrap().console_broker = Some(kernel.clone());
        let observer = cells(&mut manager, "observer");
        for (name, _) in table {
            let narrowed =
                ["command_execution", "async_jobs", "unicode_text_input"].contains(&name);
            if narrowed {
                assert_eq!(observer[name]["status"], N, "{name}");
                assert!(observer[name]["reason"]
                    .to_string()
                    .contains("without qemu_guest_exec"));
            } else {
                assert_eq!(
                    observer[name],
                    control_features(Backend::QemuHvf, "linux")[name],
                    "{name}"
                );
            }
        }
        // The daemon refuses what the cells call unsupported, by name, before any worker is consulted.
        for (id, method, arguments) in [
            ("vzlinux", "windows", json!({})),
            ("exec", "windows", json!({})),
            ("exec", "file_upload_begin", json!({})),
            ("exec", "shutdown", json!({"mode":"guest"})),
            ("observer", "exec_start", json!({})),
        ] {
            let mut arguments = arguments;
            arguments["id"] = json!(id);
            let error = manager
                .dispatch(json!({"method":method,"arguments":arguments}), &mut None)
                .unwrap_err()
                .to_string();
            assert!(error.starts_with("unsupported: "), "{id} {method}: {error}");
        }
    }
    #[test]
    fn text_input_is_gated_by_vm_kind_and_never_echoes_text() {
        // Pipe/protocol fixture, not VM acceptance: the "worker" is cat.
        let temp = tempfile::tempdir().unwrap();
        let kernel = temp.path().join("kernel");
        fs::write(&kernel, b"protocol fixture").unwrap();
        let mut manager = Manager::load(
            test_private_state(temp.path()),
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
        for (id, config) in [
            (
                "exec",
                json!({"backend":"qemu-hvf","guest":"linux","qemu_guest_exec":true}),
            ),
            ("observer", json!({"backend":"qemu-hvf","guest":"linux"})),
            ("vzlinux", json!({"guest":"linux"})),
            ("mac", json!({"guest":"macos"})),
        ] {
            let mut config = config;
            config["id"] = json!(id);
            config["cpus"] = json!(1);
            config["memory_mib"] = json!(512);
            manager
                .configs
                .insert(id.into(), serde_json::from_value(config).unwrap());
        }
        let secret = "correct horse \u{4f60}\u{597d}";
        let frame =
            json!({"frame_id":"f".repeat(36),"width":1280,"height":800,"source_generation":1});
        let arguments = |id: &str| {
            let mut value = frame.clone();
            value["id"] = json!(id);
            value["text"] = json!(secret);
            value
        };
        // Every other backend/Guest pair: the same explicit refusal, before any console check.
        for id in ["observer", "vzlinux", "mac"] {
            for method in ["input_text", input::TEXT_STEP_METHOD] {
                let mut deferred = None;
                let error = manager
                    .dispatch(
                        json!({"method":method,"arguments":arguments(id)}),
                        &mut deferred,
                    )
                    .unwrap_err()
                    .to_string();
                assert_eq!(error, input::TEXT_UNSUPPORTED_VM, "{id} {method}");
                assert!(deferred.is_none() && !error.contains("horse"));
            }
            assert_eq!(
                manager.view(id).unwrap()["capabilities"]["product_capabilities"]
                    ["unicode_text_input"]["status"],
                "unsupported",
                "{id}"
            );
        }
        assert_eq!(
            manager.view("exec").unwrap()["capabilities"]["product_capabilities"]
                ["unicode_text_input"]["status"],
            "supported"
        );
        // Invalid text is refused before anything is deferred, and the refusal does not quote it.
        let mut bad = arguments("exec");
        bad["text"] = json!(format!("{secret}\u{1b}"));
        let mut deferred = None;
        let error = manager
            .dispatch(
                json!({"method":"input_text","arguments":bad}),
                &mut deferred,
            )
            .unwrap_err()
            .to_string();
        assert!(deferred.is_none() && !error.contains("horse"), "{error}");
        let mut deferred = None;
        manager
            .dispatch(
                json!({"method":"input_text","arguments":arguments("exec")}),
                &mut deferred,
            )
            .unwrap();
        assert!(matches!(deferred, Some(Completion::Input { .. })));
        // A chunk for a VM whose process is not active goes nowhere.
        assert!(manager
            .dispatch(
                json!({"method":input::TEXT_STEP_METHOD,"arguments":arguments("exec")}),
                &mut None
            )
            .unwrap_err()
            .to_string()
            .contains("not active"));

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
            "exec".into(),
            Running {
                bridge: Arc::new(Mutex::new(bridge::State::new().unwrap())),
                child,
                stdin,
                pending: pending.clone(),
                assigned_memory_mib: 512,
                expected_exit: false,
                execution_quiescing: false,
                guest_async_timeout: Arc::new(Mutex::new(GuestAsyncTimeout::default())),
                exit_status: None,
                disk_identity: None,
                state: Arc::new(Mutex::new(json!({"state":"running"}))),
                next: 1,
            },
        );
        let mut step = arguments("exec");
        step["operation_id"] = json!("b".repeat(32));
        step["begin"] = json!(true);
        step["index"] = json!(0);
        let mut first = None;
        let mut second = None;
        let sent = [&mut first, &mut second].map(|completion| {
            manager.dispatch(
                json!({"method":input::TEXT_STEP_METHOD,"arguments":step}),
                completion,
            )
        });
        let mut reader = BufReader::new(stdout);
        let mut descriptor = libc::pollfd {
            fd: reader.get_ref().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, 1000) };
        let wire = if ready > 0 {
            read_frame(&mut reader)
        } else {
            Err(anyhow::anyhow!("fixture output deadline"))
        };
        manager
            .running
            .get_mut("exec")
            .unwrap()
            .force_stop()
            .unwrap();
        for result in sent {
            result.unwrap();
        }
        let wire: Value = serde_json::from_slice(&wire.unwrap().unwrap()).unwrap();
        assert_eq!(wire["method"], input::TEXT_STEP_METHOD);
        assert_eq!(wire["arguments"], step);
        // A Guest refusal reaches the caller as a coded reply for delivery accounting, and a
        // success is reduced to counts: the Guest's per-key diagnostics never leave the daemon.
        let keysym = 0x0100_4f60u32;
        pending.lock().unwrap().remove(&1).unwrap().sender.send(json!({"id":1,"result":{"id":"text","error":{"code":"not_ready","message":"no window has keyboard focus; nothing was injected"}}})).unwrap();
        pending.lock().unwrap().remove(&2).unwrap().sender.send(json!({"id":2,"result":{"id":"text","result":{"injected_scalars":16,"injected_bytes":20,"backend":"x11-xtest-keymap","keymap_restore":"deferred","guest_delivery_confirmed":false,"batches":1,"waited_ms":0,"pool_keycodes":40,"bindings":[{"keycode":255,"keysym":keysym,"readback":[keysym,keysym],"reused":false}],"echo":secret}}})).unwrap();
        let (peer, _other) = UnixStream::pair().unwrap();
        let refused = first.unwrap().wait(&peer).unwrap();
        assert_eq!(
            refused,
            json!({"error":{"code":"not_ready","message":"no window has keyboard focus; nothing was injected"}})
        );
        let accepted = second.unwrap().wait(&peer).unwrap();
        assert_eq!(
            accepted,
            json!({"result":{"injected_scalars":16,"injected_bytes":20,"backend":"x11-xtest-keymap","keymap_restore":"deferred","guest_delivery_confirmed":false,"batches":1,"waited_ms":0,"pool_keycodes":40}})
        );
        // Nothing the daemon wrote to its state directory holds the text.
        fn scan(path: &Path, needle: &[u8]) {
            for entry in fs::read_dir(path).unwrap() {
                let entry = entry.unwrap().path();
                if entry.is_dir() {
                    scan(&entry, needle)
                } else if entry.is_file() {
                    let bytes = fs::read(&entry).unwrap();
                    assert!(
                        !bytes.windows(needle.len()).any(|w| w == needle),
                        "{} holds the text",
                        entry.display()
                    );
                }
            }
        }
        scan(temp.path(), b"correct horse");
        scan(temp.path(), b"636f727265637420686f727365");
    }
    #[test]
    fn desktop_text_chunks_accounts_and_cleans_up_against_a_scripted_daemon() {
        // The daemon side of the socket is scripted: this checks the per-call loop only
        // (capability query, chunk requests, refusal classes, cleanup), not a VM.
        use std::os::unix::net::UnixListener;
        type Script = Box<dyn Fn(&Value, usize) -> Value + Send>;
        let info = json!({"id":"1","result":{"capabilities":{"text_input":true},"text_input":{"enabled":true,"available":true,"backend":"x11-xtest-keymap","max_scalars_per_request":16,"max_bytes_per_request":64}}});
        let accepted = |chunk: &str| json!({"result":{"result":{"injected_scalars":chunk.chars().count(),"injected_bytes":chunk.len(),"backend":"x11-xtest-keymap","keymap_restore":"deferred","guest_delivery_confirmed":false}}});
        let text = format!("{}\u{4f60}\u{597d}", "a".repeat(38));
        let run = |script: Script| -> (Result<Value>, Vec<Value>) {
            let temp = tempfile::Builder::new()
                .prefix("vdt-")
                .tempdir_in("/tmp")
                .unwrap();
            let listener = UnixListener::bind(temp.path().join("control.sock")).unwrap();
            let (done, finished) = mpsc::channel::<()>();
            let server = thread::spawn(move || {
                let mut seen = Vec::new();
                listener.set_nonblocking(true).unwrap();
                loop {
                    match listener.accept() {
                        Ok((socket, _)) => {
                            socket.set_nonblocking(false).unwrap();
                            let mut reader = BufReader::new(socket.try_clone().unwrap());
                            let request: Value =
                                serde_json::from_slice(&read_frame(&mut reader).unwrap().unwrap())
                                    .unwrap();
                            let steps = seen
                                .iter()
                                .filter(|r: &&Value| r["method"] == input::TEXT_STEP_METHOD)
                                .count();
                            let reply = script(&request, steps);
                            seen.push(request);
                            let mut socket = socket;
                            write_json(&mut socket, &reply).unwrap();
                        }
                        Err(_) if finished.try_recv().is_ok() => return seen,
                        Err(_) => thread::sleep(Duration::from_millis(2)),
                    }
                }
            });
            let (peer, _held) = UnixStream::pair().unwrap();
            let arguments = json!({"id":"vm","frame_id":"f".repeat(36),"width":1280,"height":800,"source_generation":1,"text":text});
            let result = desktop_text(temp.path(), arguments, &peer);
            done.send(()).unwrap();
            (result, server.join().unwrap())
        };
        // Success: one capability query, three chunks of 16/16/8 under one operation, one cleanup without text.
        let reply_info = info.clone();
        let (result, seen) = run(Box::new(move |request, _| {
            match request["method"].as_str().unwrap() {
                "guest_info" => json!({"result":reply_info}),
                "input_cleanup" => json!({"result":{"released":true}}),
                _ => accepted(request["arguments"]["text"].as_str().unwrap()),
            }
        }));
        let result = result.unwrap();
        assert_eq!(
            (
                result["chunks"].as_u64(),
                result["scalars"].as_u64(),
                result["bytes"].as_u64()
            ),
            (Some(3), Some(40), Some(44))
        );
        let methods: Vec<&str> = seen.iter().map(|r| r["method"].as_str().unwrap()).collect();
        assert_eq!(
            methods,
            [
                "guest_info",
                "input_text_step",
                "input_text_step",
                "input_text_step",
                "input_cleanup"
            ]
        );
        assert_eq!(seen[0]["arguments"], json!({"id":"vm"}));
        let operation = seen[1]["arguments"]["operation_id"].as_str().unwrap();
        assert!(operation.len() == 32 && operation.bytes().all(|b| b.is_ascii_hexdigit()));
        let mut joined = String::new();
        for (index, request) in seen[1..4].iter().enumerate() {
            let arguments = &request["arguments"];
            assert_eq!(arguments["begin"], index == 0);
            assert_eq!(arguments["index"], index);
            assert_eq!(arguments["operation_id"], operation);
            assert!(
                arguments["text"].as_str().unwrap().chars().count() <= input::TEXT_CHUNK_SCALARS
            );
            joined += arguments["text"].as_str().unwrap();
        }
        assert_eq!(joined, text);
        assert!(
            seen[4]["arguments"]["text"].is_null()
                && seen[4]["arguments"]["operation_id"] == operation
        );
        // Guest without the service: explicit unsupported, no chunk and no cleanup request.
        let (result, seen) = run(Box::new(
            |_, _| json!({"result":{"id":"1","result":{"capabilities":{"text_input":false},"text_input":{"enabled":false,"available":false,"reason":"text input disabled at guest service launch (--allow-text-input)"}}}}),
        ));
        let error = result.unwrap_err().to_string();
        assert!(
            error.starts_with("unsupported: Guest text input service unavailable")
                && error.contains("0 of 40 characters delivered"),
            "{error}"
        );
        assert_eq!(seen.len(), 1);
        // The daemon refuses the first chunk (busy worker): nothing was sent, nothing to clean up.
        let reply_info = info.clone();
        let (result, seen) = run(Box::new(move |request, _| {
            match request["method"].as_str().unwrap() {
                "guest_info" => json!({"result":reply_info}),
                _ => json!({"error":"input busy: another operation is active"}),
            }
        }));
        let error = result.unwrap_err().to_string();
        assert!(
            error.starts_with(
                "busy: not sent: input busy: another operation is active (0 of 40 characters delivered;"
            ),
            "{error}"
        );
        assert_eq!(seen.len(), 2);
        // The worker refuses the second chunk: 16 delivered, none uncertain, cleanup still runs.
        let reply_info = info.clone();
        let (result, seen) = run(Box::new(move |request, steps| {
            match request["method"].as_str().unwrap() {
                "guest_info" => json!({"result":reply_info}),
                "input_cleanup" => json!({"result":{"released":true}}),
                _ if steps == 0 => accepted(request["arguments"]["text"].as_str().unwrap()),
                _ => json!({"error":"backend: \"screenshot observation expired\""}),
            }
        }));
        let error = result.unwrap_err().to_string();
        assert!(
            error.starts_with("not_delivered: not sent: backend:")
                && error.contains("(16 of 40 characters delivered; the remainder was not sent)"),
            "{error}"
        );
        assert_eq!(seen.last().unwrap()["method"], "input_cleanup");
        // The Guest refuses the second chunk, and a lost worker answer on it stays uncertain.
        for (reply, expected) in [
            (json!({"result":{"error":{"code":"not_ready","message":"no window has keyboard focus"}}}), "not_ready: Guest: no window has keyboard focus (16 of 40 characters delivered; the remainder was not sent)"),
            (json!({"result":{"error":{"code":"partial_delivery","message":"injected 3 of 16"}}}), "partial_delivery: Guest: injected 3 of 16 (16 of 40 characters delivered, up to 16 more unconfirmed; the remainder was not sent)"),
            (json!({"error":"backend response unavailable (timed out); query status before retrying"}), "guest_protocol: text chunk transport failed: backend response unavailable (timed out); query status before retrying (16 of 40 characters delivered, up to 16 more unconfirmed; the remainder was not sent)"),
        ] {
            let reply_info = info.clone();
            let (result, seen) = run(Box::new(move |request, steps| match request["method"].as_str().unwrap() {
                "guest_info" => json!({"result":reply_info}),
                "input_cleanup" => json!({"result":{"released":true}}),
                _ if steps == 0 => accepted(request["arguments"]["text"].as_str().unwrap()),
                _ => reply.clone(),
            }));
            let error = result.unwrap_err().to_string();
            assert_eq!(error, expected);
            assert_eq!(seen.len(), 4);
            assert!(!error.contains("aaaa"));
        }
        // Unconfirmed cleanup is reported with the delivery result, still without the text.
        let reply_info = info.clone();
        let (result, _) = run(Box::new(move |request, _| {
            match request["method"].as_str().unwrap() {
                "guest_info" => json!({"result":reply_info}),
                "input_cleanup" => json!({"error":"backend: unknown input operation"}),
                _ => accepted(request["arguments"]["text"].as_str().unwrap()),
            }
        }));
        let error = result.unwrap_err().to_string();
        assert_eq!(
            error,
            "input cleanup unconfirmed: backend: unknown input operation; text result: delivered"
        );
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
        // Text input is an input tool: granted by the input permission and by nothing else.
        assert!(only.contains("vm_input_text"));
        let tools = tools_with_input(false, false, false, true);
        let text = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "vm_input_text")
            .unwrap();
        let key = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "vm_input_key")
            .unwrap();
        let fences = |tool: &Value| {
            let mut required: Vec<String> = tool["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap().to_string())
                .filter(|name| name != "text" && name != "keys")
                .collect();
            required.sort();
            required
        };
        assert_eq!(fences(text), fences(key));
        assert_eq!(text["inputSchema"]["additionalProperties"], false);
        assert!(text["inputSchema"]["properties"]["text"]["maxLength"].is_u64());
    }
    #[test]
    fn tool_permissions() {
        let list = tools_list(false, false);
        // 14 observe vm_* tools plus 5 read-only container/image tools.
        assert_eq!(list["tools"].as_array().unwrap().len(), 19);
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
        let names: BTreeSet<_> = list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert!(names.contains("vm_snapshot_list") && names.contains("vm_snapshot_status"));
        let lifecycle = tools_list(true, false);
        let lifecycle_names: BTreeSet<_> = lifecycle["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        for method in [
            "vm_snapshot_create",
            "vm_snapshot_restore",
            "vm_snapshot_delete",
            "vm_snapshot_reconcile",
            "vm_snapshot_resume_source",
        ] {
            assert!(
                !names.contains(method),
                "observation alone cannot mutate snapshots"
            );
            assert!(lifecycle_names.contains(method));
        }
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
                test_private_state(directory.path()),
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
                    guest_async_timeout: Arc::new(Mutex::new(GuestAsyncTimeout::default())),
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
            self.forward_envelope(method, args, |number| json!({"id":number,"result":reply}))
        }
        /// As `forward`, but the caller writes the whole worker reply, so a worker-level error (the Guest was never
        /// reached) can be told apart from an error the Guest itself returned inside `result`.
        fn forward_envelope(
            &mut self,
            method: &str,
            args: Value,
            envelope: impl FnOnce(u64) -> Value,
        ) -> (Value, Result<Value>) {
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
                .send(envelope(number))
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
    // Exercise the same child-pipe fixture without a Unix socket: this route needs only Pending::wait.
    fn stream_status_reply(
        fixture: &mut FilePipeFixture,
        method: &str,
        reply: Value,
    ) -> (Value, Result<Value>) {
        let mut completion = None;
        fixture
            .manager
            .dispatch(
                json!({"method":method,"arguments":{"id":"mac"}}),
                &mut completion,
            )
            .unwrap();
        let Some(Completion::Backend(pending)) = completion else {
            panic!("expected backend completion")
        };
        if method == "guest_stream_status" {
            assert_eq!(pending.timeout, Duration::from_secs(3));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut bytes = Vec::new();
        while bytes.last() != Some(&b'\n') {
            assert!(std::time::Instant::now() < deadline && bytes.len() <= MAX_MESSAGE);
            let mut byte = [0];
            match fixture.output.read(&mut byte) {
                Ok(1) => bytes.push(byte[0]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                other => panic!("fixture pipe ended unexpectedly: {other:?}"),
            }
        }
        let wire = serde_json::from_slice(&bytes).unwrap();
        fixture.manager.running["mac"]
            .pending
            .lock()
            .unwrap()
            .remove(&pending.number)
            .unwrap()
            .send(reply)
            .unwrap();
        (wire, pending.wait())
    }
    #[test]
    fn guest_stream_status_forwards_one_probe_and_preserves_typed_status() {
        let mut fixture = FilePipeFixture::macos();
        for (index, error) in [
            Value::Null,
            json!("guest_unavailable"),
            json!("timeout"),
            json!("invalid_state"),
            json!("busy"),
        ]
        .into_iter()
        .enumerate()
        {
            let result = json!({"reachable":error.is_null(),"guest_port":5001,"error":error});
            let (wire, got) = stream_status_reply(
                &mut fixture,
                "guest_stream_status",
                json!({"result":result}),
            );
            assert_eq!(wire, json!({"id":index+1,"method":"guest_stream_probe"}));
            assert_eq!(got.unwrap(), result);
        }
        let info = json!({"id":"6","result":{"screenshot_available":true}});
        let (wire, got) = stream_status_reply(&mut fixture, "guest_info", json!({"result":info}));
        assert_eq!(
            wire,
            json!({"id":6,"method":"guest_request","request":{"id":"6","method":"info"}})
        );
        assert_eq!(
            got.unwrap(),
            info,
            "integration info stays a pure pass-through"
        );
    }
    #[test]
    fn guest_stream_status_old_worker_is_unknown_not_unreachable() {
        let mut fixture = FilePipeFixture::macos();
        for error in [
            json!({"code":"unsupported","message":"Unsupported native backend method"}),
            json!({"code":"unknown_command","message":"unknown command"}),
            json!("unknown command"),
        ] {
            let started = std::time::Instant::now();
            let (_, result) =
                stream_status_reply(&mut fixture, "guest_stream_status", json!({"error":error}));
            assert_eq!(
                result.unwrap(),
                json!({"reachable":null,"reason":"worker_does_not_support_probe"})
            );
            assert!(started.elapsed() < Duration::from_secs(2));
        }
        let (_, result) = stream_status_reply(
            &mut fixture,
            "guest_stream_status",
            json!({"error":{"code":"backend_error","message":"failure"}}),
        );
        assert!(result.unwrap_err().to_string().starts_with("backend: "));
    }
    #[test]
    fn guest_stream_status_rejects_other_backends_guests_and_extra_arguments() {
        let mut fixture = FilePipeFixture::macos();
        for (backend, guest, args) in [
            (Backend::QemuHvf, "linux", json!({"id":"mac"})),
            (Backend::AppleVirtualization, "linux", json!({"id":"mac"})),
            (
                Backend::AppleVirtualization,
                "macos",
                json!({"id":"mac","guest_port":5000}),
            ),
        ] {
            let config = fixture.manager.configs.get_mut("mac").unwrap();
            config.backend = backend;
            config.guest = guest.into();
            let mut completion = None;
            assert!(fixture
                .manager
                .dispatch(
                    json!({"method":"guest_stream_status","arguments":args}),
                    &mut completion
                )
                .is_err());
            assert!(completion.is_none());
            assert_eq!(fixture.manager.running["mac"].next, 1);
        }
    }
    /// The macOS Guest answers `info`, `screenshot` and `windows` from one service (vsock port 5000). The second Guest
    /// service (the video stream, port 5001) is not asked: each call is exactly one worker `guest_request`. When the
    /// worker cannot connect it answers `guest_unavailable` at once; the controller must pass that code on as a
    /// `backend:` error as soon as it arrives, not wait out its own bound (10 s info, 17 s screenshot, 12 s windows)
    /// and not turn it into a result.
    #[test]
    fn mac_guest_service_unreachable_is_a_prompt_typed_backend_error_for_info_screenshot_windows() {
        let mut fixture = FilePipeFixture::macos();
        for (number, (method, guest_method)) in [
            ("guest_info", "info"),
            ("screenshot", "screenshot"),
            ("windows", "windows"),
        ]
        .into_iter()
        .enumerate()
        {
            let number = number as u64 + 1;
            let started = std::time::Instant::now();
            let (wire, result) = fixture.forward_envelope(method, json!({"id":"mac"}), |id| {
                json!({"id":id,"error":{"code":"guest_unavailable","message":"Connection reset by peer"}})
            });
            assert_eq!(
                wire,
                json!({"id":number,"method":"guest_request","request":{"id":number.to_string(),"method":guest_method}}),
                "{method}: one request to the integration service and nothing else"
            );
            let text = result.unwrap_err().to_string();
            assert!(
                text.starts_with("backend: ") && text.contains("\"code\":\"guest_unavailable\""),
                "{method}: {text}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{method}: a worker refusal must not wait for the controller timeout"
            );
        }
        // A Guest that was reached and refused is a different, also typed, error.
        let (_, refused) = fixture.forward(
            "screenshot",
            json!({"id":"mac"}),
            json!({"id":"4","error":{"code":"screen_recording_permission_required"}}),
        );
        let text = refused.unwrap_err().to_string();
        assert!(
            text.starts_with("guest: ") && text.contains("screen_recording_permission_required"),
            "{text}"
        );
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
    // Exercise dispatch, reply completion and status without a listening socket or a Guest.
    fn complete_health_info(fixture: &mut FilePipeFixture, reply: Value) -> Result<Value> {
        let mut completion = None;
        fixture.manager.dispatch(
            json!({"method":"guest_info","arguments":{"id":"mac"}}),
            &mut completion,
        )?;
        let Some(Completion::Backend(pending)) = completion else {
            panic!("guest_info must use backend completion");
        };
        fixture.manager.running["mac"]
            .pending
            .lock()
            .unwrap()
            .remove(&pending.number)
            .unwrap()
            .send(reply)
            .unwrap();
        pending.wait()
    }
    fn reported_metal_info() -> Value {
        // The current Swift service's public-device schema, not workload evidence.
        json!({"id":"info","result":{
            "metal_device":"Apple Paravirtual device",
            "graphics":{"schema":1,"api":"metal","status":"reported",
                "device_selection":"system-default","evidence":"public-device-properties",
                "queried_at_unix_ms":5678,
                "device":{"name":"Apple Paravirtual device","registry_id":"18446744073709551615"}}
        }})
    }
    #[test]
    fn guest_metal_device_present_has_no_alert() {
        let mut fixture = FilePipeFixture::macos();
        let info = reported_metal_info();
        assert_eq!(
            complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
            info
        );
        let runtime = fixture.manager.view("mac").unwrap()["runtime"].clone();
        assert!(runtime.get("guest_metal_device_alert").is_none());
        assert_eq!(
            runtime["guest_metal_device_observation"]["status"],
            "reported"
        );
        assert_eq!(
            runtime["guest_metal_device_observation"]["queried_at_unix_ms"],
            5678
        );
    }
    #[test]
    fn guest_metal_device_lost_alert_is_in_status_and_list_without_changing_reply() {
        let mut fixture = FilePipeFixture::macos();
        let info = json!({"id":"info","result":{"metal_device":"none","graphics":{"status":"unavailable","reason":"no_metal_device","queried_at_unix_ms":1234}}});
        assert_eq!(
            complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
            info
        );
        let status = fixture.manager.view("mac").unwrap();
        let alert = &status["runtime"]["guest_metal_device_alert"];
        assert_eq!(alert["reason"], "no_metal_device");
        assert_eq!(alert["source"], "guest_info");
        assert_eq!(alert["queried_at_unix_ms"], 1234);
        assert!(alert["scope"].as_str().unwrap().contains("not polled"));
        assert_eq!(alert["operator_guidance"]["automatic_recovery"], false);
        assert!(alert["operator_guidance"]["action"]
            .as_str()
            .unwrap()
            .contains("normal Guest shutdown"));
        assert_eq!(
            status["runtime"]["guest_metal_device_observation"]["status"],
            "unavailable"
        );
        assert_eq!(status["state"], "running");
        let list = fixture
            .manager
            .dispatch(json!({"method":"list"}), &mut None)
            .unwrap();
        assert_eq!(list[0]["runtime"]["guest_metal_device_alert"], *alert);
    }
    #[test]
    fn guest_metal_device_old_agent_has_no_alert_or_error() {
        let mut fixture = FilePipeFixture::macos();
        let info = json!({"id":"info","result":{"metal_device":"Apple Paravirtual device","capabilities":{"files":false}}});
        assert_eq!(
            complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
            info
        );
        let runtime = fixture.manager.view("mac").unwrap()["runtime"].clone();
        assert!(runtime.get("guest_metal_device_alert").is_none());
        assert_eq!(
            runtime["guest_metal_device_observation"]["status"],
            "unknown"
        );
        assert_eq!(
            runtime["guest_metal_device_observation"]["reason"],
            "graphics_not_reported"
        );
    }
    #[test]
    fn guest_metal_device_alert_is_absent_for_other_backend_guest_pairs() {
        let mut fixture = FilePipeFixture::macos();
        let info = json!({"id":"info","result":{"graphics":{"status":"unavailable","reason":"no_metal_device"}}});
        for (backend, guest) in [
            (Backend::AppleVirtualization, "linux"),
            (Backend::QemuHvf, "linux"),
            (Backend::QemuHvf, "windows"),
            (Backend::QemuHvf, "macos"),
        ] {
            let config = fixture.manager.configs.get_mut("mac").unwrap();
            config.backend = backend;
            config.guest = guest.into();
            assert_eq!(
                complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
                info
            );
            let runtime = fixture.manager.view("mac").unwrap()["runtime"].clone();
            assert!(
                runtime.get("guest_metal_device_alert").is_none(),
                "{backend:?} {guest}"
            );
            assert!(
                runtime.get("guest_metal_device_observation").is_none(),
                "{backend:?} {guest}"
            );
        }
    }
    #[test]
    fn guest_metal_device_alert_survives_state_events_and_failed_queries() {
        let mut fixture = FilePipeFixture::macos();
        let info = json!({"id":"info","result":{"graphics":{"status":"unavailable","reason":"no_metal_device"}}});
        complete_health_info(&mut fixture, json!({"result":info})).unwrap();
        let alert =
            fixture.manager.view("mac").unwrap()["runtime"]["guest_metal_device_alert"].clone();
        let observation = fixture.manager.view("mac").unwrap()["runtime"]
            ["guest_metal_device_observation"]
            .clone();
        replace_runtime_state(
            &mut fixture.manager.running["mac"].state.lock().unwrap(),
            json!({"state":"running","event":"state"}),
        );
        for reply in [
            json!({"error":{"code":"guest_unavailable"}}),
            json!({"result":{"id":"info","error":{"code":"unsupported"}}}),
        ] {
            assert!(complete_health_info(&mut fixture, reply).is_err());
            assert_eq!(
                fixture.manager.view("mac").unwrap()["runtime"]["guest_metal_device_alert"],
                alert
            );
            assert_eq!(
                fixture.manager.view("mac").unwrap()["runtime"]["guest_metal_device_observation"],
                observation
            );
        }
        let mut fresh = FilePipeFixture::macos();
        assert!(fresh.manager.view("mac").unwrap()["runtime"]
            .get("guest_metal_device_alert")
            .is_none());
    }
    #[test]
    fn guest_metal_device_alert_clears_only_on_a_valid_device_report() {
        let mut fixture = FilePipeFixture::macos();
        complete_health_info(&mut fixture, json!({"result":{"result":{"graphics":{"status":"unavailable","reason":"no_metal_device","queried_at_unix_ms":1234}}}})).unwrap();
        let alert =
            fixture.manager.view("mac").unwrap()["runtime"]["guest_metal_device_alert"].clone();
        for info in [
            json!({"result":{}}),
            json!({"result":{"metal_device":"Apple Paravirtual device"}}),
            json!({"result":{"graphics":{"status":"reported"}}}),
            json!({"result":{"graphics":{"status":"unavailable","reason":"unsupported_query"}}}),
            json!({"result":{"graphics":false}}),
        ] {
            assert_eq!(
                complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
                info
            );
            let runtime = fixture.manager.view("mac").unwrap()["runtime"].clone();
            assert_eq!(
                runtime["guest_metal_device_alert"], alert,
                "unknown is not recovery: {info}"
            );
            assert_eq!(
                runtime["guest_metal_device_observation"]["status"],
                "unknown"
            );
        }
        let info = reported_metal_info();
        assert_eq!(
            complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
            info
        );
        let runtime = fixture.manager.view("mac").unwrap()["runtime"].clone();
        assert!(runtime.get("guest_metal_device_alert").is_none());
        assert_eq!(
            runtime["guest_metal_device_observation"]["status"],
            "reported"
        );
    }
    #[test]
    fn guest_metal_device_invalid_identity_or_schema_cannot_clear_loss() {
        let mut fixture = FilePipeFixture::macos();
        complete_health_info(&mut fixture, json!({"result":{"result":{"graphics":{"status":"unavailable","reason":"no_metal_device"}}}})).unwrap();
        let alert =
            fixture.manager.view("mac").unwrap()["runtime"]["guest_metal_device_alert"].clone();
        for (pointer, value) in [
            ("/result/graphics/schema", json!(2)),
            ("/result/graphics/api", json!("opengl")),
            ("/result/graphics/device_selection", json!("other")),
            ("/result/graphics/evidence", json!("test-fixture")),
            ("/result/graphics/queried_at_unix_ms", json!(-1)),
            ("/result/graphics/device", Value::Null),
            ("/result/graphics/device/name", json!(" ")),
            (
                "/result/graphics/device/registry_id",
                json!("18446744073709551616"),
            ),
            ("/result/graphics/device/registry_id", json!("+1")),
            ("/result/graphics/device/registry_id", json!(1)),
            ("/result/metal_device", json!("different device")),
        ] {
            let mut info = reported_metal_info();
            *info.pointer_mut(pointer).unwrap() = value;
            assert_eq!(
                complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
                info
            );
            let runtime = fixture.manager.view("mac").unwrap()["runtime"].clone();
            assert_eq!(
                runtime["guest_metal_device_alert"], alert,
                "{pointer}: {info}"
            );
            assert_eq!(
                runtime["guest_metal_device_observation"]["status"],
                "unknown"
            );
        }
    }
    #[test]
    fn guest_metal_device_retained_real_swift_wire_is_a_valid_presence_report() {
        // Replay the independently recorded actual Guest/MCP public properties.
        // This is wire compatibility evidence, not a fresh Guest recovery test.
        let evidence: Value = serde_json::from_str(include_str!(
            "../../../archive/docs-evidence/macos-graphics-stage8/evidence.json"
        ))
        .unwrap();
        assert_eq!(evidence["validation"]["actual_mcp_query"], true);
        let observation = &evidence["validation"]["observation"];
        assert_eq!(
            observation["graphics"]["assessment"]["performance"],
            "not-measured"
        );
        let info = json!({"id":"retained-wire","result":observation});
        let mut fixture = FilePipeFixture::macos();
        complete_health_info(&mut fixture, json!({"result":{"result":{"graphics":{"status":"unavailable","reason":"no_metal_device"}}}})).unwrap();
        assert_eq!(
            complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
            info
        );
        let runtime = fixture.manager.view("mac").unwrap()["runtime"].clone();
        assert!(runtime.get("guest_metal_device_alert").is_none());
        assert_eq!(
            runtime["guest_metal_device_observation"]["status"],
            "reported"
        );
        assert_eq!(
            runtime["guest_metal_device_observation"]["queried_at_unix_ms"],
            observation["graphics"]["queried_at_unix_ms"]
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
mod screenshot_scaled_tests;
#[cfg(test)]
mod screenshot_test_support;
