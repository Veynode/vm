//! A VM's registered configuration (`Config`) and its checks.
use super::*;

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
    /// Guest sound output and microphone input, carried to whichever desktop
    /// viewer is attached: a QEMU sound card on SPICE audio, or the Guest's
    /// own PipeWire devices relayed by the Guest agent on Apple. A macOS Guest
    /// gets a sound card the host discards, recorded by its Guest service; it
    /// has no microphone.
    #[serde(default, skip_serializing_if = "is_false")]
    pub audio: bool,
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
    /// macOS Guest with the port relay (`services::macos_relay`): the Unix
    /// socket on which the worker splices connections to the relay's vsock
    /// port, instead of the Guest's vmnet address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_ports_socket: Option<PathBuf>,
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
pub(crate) fn loopback() -> std::net::Ipv4Addr {
    std::net::Ipv4Addr::LOCALHOST
}
pub(crate) fn tcp() -> String {
    "tcp".into()
}
pub(crate) fn valid_local_mac(mac: &str) -> bool {
    let octets: Vec<_> = mac.split(':').collect();
    octets.len() == 6
        && octets
            .iter()
            .all(|o| o.len() == 2 && o.bytes().all(|b| b.is_ascii_hexdigit()))
        && u8::from_str_radix(octets[0], 16).is_ok_and(|first| first & 0b11 == 0b10)
}
pub(crate) fn is_false(value: &bool) -> bool {
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
        if let Some(socket) = &self.guest_ports_socket {
            if self.backend != Backend::AppleVirtualization
                || self.guest != "macos"
                || !socket.is_absolute()
                || socket.as_os_str().len() >= 104
            {
                bail!("guest_ports_socket needs an Apple macOS Guest and an absolute path under 104 bytes")
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
    pub(crate) fn validate_boot(&self) -> Result<()> {
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
