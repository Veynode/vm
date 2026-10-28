//! OCI containers as micro-VMs.
//!
//! Each container is one daemon-managed VM booted from the shared micro-VM
//! kernel/initramfs with four block devices:
//!
//! ```text
//! vda  <container>/rw.raw       writable ext4 (formatted in-guest on first boot)
//! vdb  images/rootfs/<id>.sqfs  read-only image root (overlay lower)
//! vdc  <container>/config.img   "VEYNODEVM-CONFIG-V1\n" + JSON, read-only
//! vdd  boot.modloop             kernel modules, read-only
//! ```
//!
//! The guest agent runs as PID 1 (`container-init`) and reports progress on the
//! console as `@@veynodevm <kind> <value>` lines; everything between `started`
//! and `exit` is the container's output.
use super::{
    daemon_call,
    records::{self, Kind},
    util::random_hex,
};
use crate::home;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use veynodevm_oci::{ImageRecord, Store};

pub const CONFIG_MAGIC: &[u8] = b"VEYNODEVM-CONFIG-V1\n";
/// Apple Virtualization generic machine identifier, created by the worker.
pub const MACHINE_IDENTIFIER_FILE: &str = "vz-machine-id.bin";
const CONFIG_DISK_BYTES: usize = 1 << 20;
/// The configuration disk's last 4 KiB: a fresh random seed for every boot,
/// which the guest agent credits to the kernel before anything else runs.
/// Without it a micro-VM has no entropy source (no virtio-rng driver is loaded
/// and the kernel does not trust a boot loader seed), so `getrandom` blocks
/// for seconds: mke2fs alone waited ~4 s at the first boot. Older agents
/// decode only the JSON at the start of the disk and ignore it.
pub const SEED_MAGIC: &[u8] = b"VEYNODEVM-SEED-V1\n";
const SEED_OFFSET: usize = CONFIG_DISK_BYTES - 4096;
const SEED_BYTES: usize = 64;
pub const MARKER: &str = "@@veynodevm ";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PortSpec {
    pub host_address: std::net::Ipv4Addr,
    pub host_port: u16,
    pub guest_port: u16,
    pub protocol: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VolumeSpec {
    pub host: PathBuf,
    pub target: String,
    pub read_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub config_digest: String,
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub workdir: String,
    pub user: String,
    pub ports: Vec<PortSpec>,
    pub volumes: Vec<VolumeSpec>,
    pub backend: String,
    pub gpus: Option<String>,
    pub desktop: bool,
    pub engine: bool,
    /// `Some("systemd")`: the image's init is PID 1 ("full system" mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init: Option<String>,
    pub auto_remove: bool,
    pub cpus: u32,
    pub memory_mib: u64,
    pub created_at: u64,
    pub vm_id: String,
    /// "darwin" for macOS containers (see `macos`); empty for Linux.
    #[serde(default)]
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac_address: Option<String>,
    /// QEMU's loopback forward to the guest session server (`session`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_port: Option<u16>,
    /// Started with `-t` (entrypoint on a pty) or `-i` (stdin kept open).
    #[serde(default)]
    pub tty: bool,
    #[serde(default)]
    pub stdin: bool,
    /// Set on containers made by `fork` (see `checkpoint`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_of: Option<super::checkpoint::ForkOrigin>,
    /// The icon the web UI draws for it, `KIND[:COLOR]` (see `parse_icon`);
    /// `None` draws the kind of container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The guest display size of a desktop container, when not the 1280x800
    /// default (`run --desktop-size`, `update --desktop-size`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_size: Option<(u32, u32)>,
}

/// `container.run`'s arguments, the `veynodevm run` flags.
#[derive(Default, Clone, Debug, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct RunOptions {
    pub image: String,
    pub command: Vec<String>,
    pub entrypoint: Option<Vec<String>>,
    pub name: Option<String>,
    pub detach: bool,
    pub auto_remove: bool,
    pub ports: Vec<String>,
    pub volumes: Vec<String>,
    pub env: Vec<String>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    pub cpus: Option<u32>,
    #[serde(rename = "memoryMiB")]
    pub memory_mib: Option<u64>,
    #[serde(rename = "diskGiB")]
    pub disk_gib: Option<u64>,
    pub gpus: Option<String>,
    pub desktop: bool,
    /// `--desktop-size WxH`: the guest display (default 1280x800).
    pub desktop_size: Option<(u32, u32)>,
    pub engine: bool,
    pub dns: Vec<String>,
    pub mtu: Option<u16>,
    /// `--init systemd`; `None` runs the entrypoint under the VeynodeVM init.
    pub init: Option<String>,
    pub quiet: bool,
    pub tty: bool,
    pub stdin: bool,
}

/// `--init systemd` boots the image's `/sbin/init` as PID 1 instead of its
/// Entrypoint/Cmd. A COMMAND replaces the init (absolute path plus arguments,
/// e.g. `/lib/systemd/systemd systemd.unit=multi-user.target`).
pub fn systemd_argv(options: &RunOptions) -> Result<Vec<String>> {
    if options.entrypoint.is_some() {
        bail!(
            "--init systemd boots /sbin/init; give another init as COMMAND instead of --entrypoint"
        )
    }
    if options.user.is_some() {
        bail!("--init systemd runs init as root; -u/--user is not supported")
    }
    if options.tty || options.stdin {
        bail!("--init systemd has no interactive session; drop -i/-t and use `veynodevm exec`")
    }
    match options.command.first() {
        None => Ok(vec!["/sbin/init".to_owned()]),
        Some(path) if path.starts_with('/') => Ok(options.command.clone()),
        Some(path) => bail!("--init systemd: the init path must be absolute, got {path}"),
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `[ip:]host:guest[/proto]`, `guest` alone publishes the same host port.
pub fn parse_port(spec: &str) -> Result<PortSpec> {
    let (body, protocol) = match spec.rsplit_once('/') {
        Some((b, p)) => (b, p.to_owned()),
        None => (spec, "tcp".to_owned()),
    };
    if !["tcp", "udp"].contains(&protocol.as_str()) {
        bail!("port protocol must be tcp or udp: {spec}")
    }
    let parts: Vec<&str> = body.split(':').collect();
    let port = |s: &str| -> Result<u16> {
        match s.parse::<u16>() {
            Ok(p) if p > 0 => Ok(p),
            _ => bail!("invalid port in {spec}"),
        }
    };
    let (host_address, host_port, guest_port) = match parts.as_slice() {
        [guest] => (std::net::Ipv4Addr::LOCALHOST, port(guest)?, port(guest)?),
        [host, guest] => (std::net::Ipv4Addr::LOCALHOST, port(host)?, port(guest)?),
        [ip, host, guest] => (
            ip.parse()
                .with_context(|| format!("invalid IPv4 address in {spec}"))?,
            port(host)?,
            port(guest)?,
        ),
        _ => bail!("invalid port mapping: {spec}"),
    };
    Ok(PortSpec {
        host_address,
        host_port,
        guest_port,
        protocol,
    })
}

/// `/host/path:/container/path[:ro|rw]`
pub fn parse_volume(spec: &str) -> Result<VolumeSpec> {
    let parts: Vec<&str> = spec.split(':').collect();
    let (host, target, read_only) = match parts.as_slice() {
        [h, t] => (*h, *t, false),
        [h, t, "ro"] => (*h, *t, true),
        [h, t, "rw"] => (*h, *t, false),
        _ => bail!(
            "volume must be /host:/container[:ro|rw] (named volumes are not supported yet): {spec}"
        ),
    };
    let host = PathBuf::from(host);
    if !host.is_absolute() || !target.starts_with('/') {
        bail!("volume paths must be absolute: {spec}")
    }
    Ok(VolumeSpec {
        host,
        target: target.to_owned(),
        read_only,
    })
}

pub fn container_dir(id: &str) -> Result<PathBuf> {
    Ok(home::containers_dir()?.join(id))
}

pub fn list() -> Result<Vec<Container>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(home::containers_dir()?)? {
        let path = entry?.path().join("container.json");
        if let Ok(record) = records::read(Kind::Container, &path) {
            if let Ok(c) = serde_json::from_value::<Container>(record) {
                out.push(c);
            }
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.created_at));
    Ok(out)
}

pub fn find(name: &str) -> Result<Container> {
    let all = list()?;
    if let Some(c) = all.iter().find(|c| c.id == name || c.name == name) {
        return Ok(c.clone());
    }
    let matches: Vec<_> = all.iter().filter(|c| c.id.starts_with(name)).collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no such container: {name}"),
        _ => bail!("ambiguous container id prefix: {name}"),
    }
}

pub fn save(container: &Container) -> Result<()> {
    let path = container_dir(&container.id)?.join("container.json");
    records::write(&path, &records::stamped(Kind::Container, container)?)
}

/// The devices a container's icon can show and the colours it can take: the
/// computer icons of Veynode's design system (`@veynode/shared/avatar`).
pub const ICON_KINDS: [&str; 10] = [
    "monitor", "laptop", "vm", "box", "server", "terminal", "cpu", "globe", "folder", "code",
];
pub const ICON_COLORS: [&str; 8] = [
    "blue", "teal", "green", "yellow", "orange", "pink", "purple", "black",
];

/// Checks an icon, `KIND` (the kind's own colour) or `KIND:COLOR`.
pub fn parse_icon(icon: &str) -> Result<String> {
    let (kind, color) = match icon.split_once(':') {
        Some((kind, color)) => (kind, Some(color)),
        None => (icon, None),
    };
    if !ICON_KINDS.contains(&kind) {
        bail!(
            "unknown icon {kind:?}; use one of {}",
            ICON_KINDS.join(", ")
        )
    }
    if let Some(color) = color.filter(|c| !ICON_COLORS.contains(c)) {
        bail!(
            "unknown icon colour {color:?}; use one of {}",
            ICON_COLORS.join(", ")
        )
    }
    Ok(icon.to_owned())
}

/// Write the read-only configuration disk (vdc) the guest agent reads at boot.
pub fn write_config_disk(path: &Path, guest_config: &Value) -> Result<()> {
    let mut body = CONFIG_MAGIC.to_vec();
    body.extend(serde_json::to_vec(guest_config)?);
    body.push(b'\n');
    if body.len() > SEED_OFFSET {
        bail!("container configuration exceeds 1 MiB")
    }
    body.resize(CONFIG_DISK_BYTES, 0);
    let seed = &mut body[SEED_OFFSET..SEED_OFFSET + SEED_MAGIC.len() + SEED_BYTES];
    seed[..SEED_MAGIC.len()].copy_from_slice(SEED_MAGIC);
    let random = &mut seed[SEED_MAGIC.len()..];
    unsafe { libc::arc4random_buf(random.as_mut_ptr().cast(), random.len()) };
    private_file(path)?.write_all(&body)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o400))?;
    Ok(())
}

/// The guest configuration stored on a configuration disk.
pub fn read_config_disk(path: &Path) -> Result<Value> {
    let body = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let json = body
        .strip_prefix(CONFIG_MAGIC)
        .context("not a container configuration disk")?;
    let end = json.iter().position(|&b| b == b'\n').unwrap_or(json.len());
    Ok(serde_json::from_slice(&json[..end])?)
}

pub(crate) fn private_file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

/// Daemon VM state: "running", "stopped", "missing", ...
pub fn vm_state(container: &Container) -> String {
    vm_state_of(&container.vm_id)
}

/// The state of the daemon VM `id`: "missing" when there is none.
pub fn vm_state_of(id: &str) -> String {
    match daemon_call("status", json!({"id":id})) {
        // QEMU exec bookkeeping can outlive the VM process; without a pid it is stopped.
        Ok(v) if v["state"] == "recovery_required" && v["pid"].is_null() => "stopped".to_owned(),
        Ok(v) => v["state"].as_str().unwrap_or("unknown").to_owned(),
        Err(e) if e.to_string().contains("VM not found") => "missing".to_owned(),
        Err(_) => "unknown".to_owned(),
    }
}

pub fn is_active(container: &Container) -> bool {
    active(&vm_state(container))
}

pub fn active(state: &str) -> bool {
    !matches!(
        state,
        "stopped" | "crashed" | "missing" | "unknown" | "error"
    )
}

/// Facts parsed from the console log.
#[derive(Default, Debug, Clone)]
pub struct Console {
    pub ip: Option<String>,
    pub started: bool,
    pub exit: Option<i32>,
    pub gpu: Option<String>,
    pub errors: Vec<String>,
}

pub fn read_console(container: &Container) -> Console {
    let mut console = Console::default();
    let Ok(file) = File::open(
        container_dir(&container.id)
            .map(|d| d.join("console.log"))
            .unwrap_or_default(),
    ) else {
        return console;
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Some((kind, value)) = parse_marker(&line) {
            match kind {
                "ip" => console.ip = Some(value.to_owned()),
                "started" => console.started = true,
                "exit" => console.exit = value.parse().ok(),
                "gpu" => console.gpu = Some(value.to_owned()),
                "error" => console.errors.push(value.to_owned()),
                _ => {}
            }
        }
    }
    console
}

pub fn parse_marker(line: &str) -> Option<(&str, &str)> {
    let line = line.trim_end_matches('\r');
    let rest = &line[line.find(MARKER)? + MARKER.len()..];
    Some(rest.split_once(' ').unwrap_or((rest, "")))
}

pub fn status_text(container: &Container) -> (String, String) {
    let state = vm_state(container);
    if container.os == "darwin" {
        let text = if active(&state) { "Up" } else { "Exited" };
        return (state, text.to_owned());
    }
    let console = read_console(container);
    let text = if active(&state) {
        if console.started {
            "Up".to_owned()
        } else {
            "Starting".to_owned()
        }
    } else if let Some(code) = console.exit {
        format!("Exited ({code})")
    } else if !console.errors.is_empty() {
        format!("Failed: {}", console.errors.last().unwrap())
    } else if console.started {
        // Stopped from outside before the entrypoint reported its exit.
        "Exited".to_owned()
    } else {
        "Created".to_owned()
    };
    (state, text)
}

pub(crate) fn port_free(port: &PortSpec) -> Result<()> {
    if port.protocol == "tcp" {
        std::net::TcpListener::bind((port.host_address, port.host_port)).with_context(|| {
            format!(
                "host port {}:{} is not available",
                port.host_address, port.host_port
            )
        })?;
    } else {
        std::net::UdpSocket::bind((port.host_address, port.host_port)).with_context(|| {
            format!(
                "host port {}:{}/udp is not available",
                port.host_address, port.host_port
            )
        })?;
    }
    Ok(())
}

pub(crate) fn ensure_image(store: &Store, name: &str, quiet: bool) -> Result<ImageRecord> {
    if let Some(image) = store.find(name)? {
        return Ok(image);
    }
    if !quiet {
        super::progress(&format!("Unable to find image '{name}' locally"));
    }
    store.pull(name, &mut |m| {
        if !quiet {
            super::progress(m)
        }
    })
}

/// A name a container can take: `[A-Za-z0-9_.-]{1,64}`, used by no other.
pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        bail!("container name must be [A-Za-z0-9_.-]{{1,64}}")
    }
    if list()?.iter().any(|c| c.name == name) {
        bail!("container name already in use: {name}")
    }
    Ok(())
}

/// Renames a container's record, as `docker rename` does: the guest keeps the
/// hostname it booted with, and forks keep the source name they recorded.
pub fn rename(container: &mut Container, name: &str) -> Result<()> {
    if container.name == name {
        return Ok(());
    }
    check_name(name)?;
    container.name = name.to_owned();
    save(container)
}

/// Validated `--name`, or one derived from the image reference.
pub fn container_name(options: &RunOptions, image: &ImageRecord, id: &str) -> Result<String> {
    Ok(match &options.name {
        Some(n) => {
            check_name(n)?;
            n.clone()
        }
        None => {
            let base = image.reference.rsplit('/').next().unwrap_or("c");
            let base: String = base
                .split([':', '@'])
                .next()
                .unwrap_or("c")
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect();
            format!("{base}-{}", &id[..4])
        }
    })
}

/// Published ports, checked to be free. UDP needs QEMU's user network.
pub fn parse_ports(options: &RunOptions, qemu: bool) -> Result<Vec<PortSpec>> {
    let ports = options
        .ports
        .iter()
        .map(|p| parse_port(p))
        .collect::<Result<Vec<_>>>()?;
    if !qemu && ports.iter().any(|p| p.protocol != "tcp") {
        bail!("UDP publishing needs the QEMU backend; Apple Virtualization containers publish TCP only")
    }
    let mut seen = BTreeSet::new();
    for p in &ports {
        if !seen.insert((p.host_address, p.host_port, p.protocol.clone())) {
            bail!("duplicate published port {}", p.host_port)
        }
        port_free(p)?;
    }
    Ok(ports)
}

/// Bind mounts with created, canonical host directories.
pub fn parse_volumes(options: &RunOptions) -> Result<Vec<VolumeSpec>> {
    let mut volumes = options
        .volumes
        .iter()
        .map(|v| parse_volume(v))
        .collect::<Result<Vec<_>>>()?;
    for v in &mut volumes {
        fs::create_dir_all(&v.host)
            .with_context(|| format!("create volume directory {}", v.host.display()))?;
        v.host = v.host.canonicalize()?;
    }
    Ok(volumes)
}

/// The QEMU graphics profile for a `--gpus` value.
pub fn gpu_profile(gpus: &str) -> Result<&'static str> {
    Ok(match gpus {
        "all" | "virgl" | "gl" => "virgl",
        "venus" | "vulkan" => "venus-moltenvk-experimental",
        other => bail!("--gpus accepts all|virgl|venus, got {other}"),
    })
}

/// Memory when `-m` is not given. A full desktop distribution (GNOME, KDE)
/// needs more than a single app.
pub fn default_memory_mib(qemu: bool, engine: bool, desktop: bool, systemd: bool) -> u64 {
    if systemd && desktop {
        4096
    } else if qemu || engine || desktop || systemd {
        2048
    } else {
        512
    }
}

/// Create and start a container. Returns the record; the caller attaches.
pub fn create_and_start(options: &RunOptions) -> Result<Container> {
    let store = Store::open(&home::images_dir()?)?;
    let image = ensure_image(&store, &options.image, options.quiet)?;
    if image.config.os == "darwin" {
        if options.init.is_some() {
            bail!("--init systemd is for Linux images")
        }
        return super::macos::create_and_start(options, &store, &image);
    }
    let systemd_init = match options.init.as_deref() {
        None => None,
        Some("systemd") => Some(systemd_argv(options)?),
        Some(other) => bail!("--init accepts systemd, got {other}"),
    };
    let runtime = home::Runtime::load()?;
    let boot = runtime
        .boot
        .clone()
        .context("no micro-VM boot assets registered: run scripts/install-dev-runtime.py (runtime.json \"boot\")")?;
    let rootfs = store.rootfs(&image, &mut |m| {
        if !options.quiet {
            super::progress(m)
        }
    })?;
    // GPU acceleration is the QEMU/HVF path; a desktop without --gpus uses the
    // Apple Virtualization 2D virtio-gpu display and AppKit window.
    let qemu = options.gpus.is_some();
    if qemu && runtime.qemu.is_none() {
        bail!("--gpus needs the QEMU/HVF GPU runtime: run runtime/fetch-utm.sh and scripts/install-dev-runtime.py --qemu")
    }
    let id = random_hex(6);
    let name = container_name(options, &image, &id)?;
    let ports = parse_ports(options, qemu)?;
    let volumes = parse_volumes(options)?;
    let config = &image.config;
    let entrypoint = options
        .entrypoint
        .clone()
        .unwrap_or_else(|| config.entrypoint.clone());
    let cmd = if options.command.is_empty() && options.entrypoint.is_none() {
        config.cmd.clone()
    } else {
        options.command.clone()
    };
    let argv: Vec<String> = match &systemd_init {
        Some(init) => init.clone(),
        None => entrypoint.into_iter().chain(cmd).collect(),
    };
    if argv.is_empty() {
        bail!("no command specified and the image has no Entrypoint or Cmd")
    }
    let mut env = config.env.clone();
    for e in &options.env {
        let entry = if e.contains('=') {
            e.clone()
        } else {
            match std::env::var(e) {
                Ok(v) => format!("{e}={v}"),
                Err(_) => continue,
            }
        };
        let key = entry.split('=').next().unwrap().to_owned();
        env.retain(|x| x.split('=').next() != Some(key.as_str()));
        env.push(entry);
    }
    let targets: HashSet<&str> = volumes.iter().map(|v| v.target.as_str()).collect();
    let anonymous: Vec<String> = config
        .volumes
        .iter()
        .filter(|v| !targets.contains(v.as_str()))
        .cloned()
        .collect();
    let gpu_profile = options.gpus.as_deref().map(gpu_profile).transpose()?;
    let dns: Vec<String> = if options.dns.is_empty() {
        runtime.dns.iter().map(ToString::to_string).collect()
    } else {
        options
            .dns
            .iter()
            .map(|d| {
                d.parse::<std::net::IpAddr>()
                    .map(|ip| ip.to_string())
                    .with_context(|| format!("invalid --dns {d}"))
            })
            .collect::<Result<_>>()?
    };
    if dns.len() > 3 {
        bail!("at most 3 DNS servers")
    }
    let mtu = options.mtu.or(runtime.mtu);
    if mtu.is_some_and(|m| !(576..=9000).contains(&m)) {
        bail!("--mtu must be 576..9000")
    }
    let dir = container_dir(&id)?;
    fs::create_dir(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let result = (|| -> Result<Container> {
        let rw = dir.join("rw.raw");
        private_file(&rw)?.set_len(options.disk_gib.unwrap_or(16) << 30)?;
        let transport = if qemu { "virtio-serial-exec" } else { "vsock" };
        let session_token = random_hex(32);
        private_file(&dir.join(super::session::TOKEN_FILE))?.write_all(session_token.as_bytes())?;
        let session_port = if qemu {
            Some(super::session::free_port()?)
        } else {
            None
        };
        let mut guest_config = json!({
            "version": 1,
            "id": id,
            "hostname": name.chars().take(63).collect::<String>(),
            "argv": argv,
            "env": env,
            "workdir": options.workdir.clone().unwrap_or_else(|| config.working_dir.clone()),
            // systemd runs as root; the image USER does not apply to init.
            "user": if systemd_init.is_some() { String::new() } else { options.user.clone().unwrap_or_else(|| config.user.clone()) },
            "volumes": volumes.iter().enumerate().map(|(i, v)| json!({"tag":format!("v{i}"),"target":v.target,"read_only":v.read_only,"fs":if qemu {"9p"} else {"virtiofs"}})).collect::<Vec<_>>(),
            "anonymous_volumes": anonymous,
            "transport": transport,
            "gpu": gpu_profile.is_some(),
            "engine": options.engine,
            "dns": dns,
            "mtu": mtu,
            "desktop": options.desktop,
            "session_token": session_token,
            "tty": options.tty,
            "stdin": options.stdin,
        });
        if systemd_init.is_some() {
            // Only sent when used: older boot assets reject unknown fields.
            guest_config["init"] = json!("systemd");
        }
        let config_disk = dir.join("config.img");
        write_config_disk(&config_disk, &guest_config)?;
        let memory_mib = options.memory_mib.unwrap_or(default_memory_mib(
            qemu,
            options.engine,
            options.desktop,
            systemd_init.is_some(),
        ));
        let cpus = options.cpus.unwrap_or(2);
        let vm_id = format!("c-{id}");
        let mut vm = json!({
            "backend": if qemu {"qemu-hvf"} else {"apple-virtualization"},
            "id": vm_id,
            "guest": "linux",
            "cpus": cpus,
            "memory_mib": memory_mib,
            "kernel": boot.kernel,
            "initrd": boot.initrd,
            "command_line": format!("console={} rdinit=/veynode/init quiet loglevel=1 panic=-1", if qemu {"ttyAMA0"} else {"hvc0"}),
            "disk": rw,
            "extra_disks": [
                {"path": rootfs, "read_only": true},
                {"path": config_disk, "read_only": true},
                {"path": boot.modloop, "read_only": true},
            ],
            "serial_log": dir.join("console.log"),
            "shares": volumes.iter().enumerate().map(|(i, v)| json!({"tag":format!("v{i}"),"path":v.host,"read_only":v.read_only})).collect::<Vec<_>>(),
        });
        if options.desktop {
            let (width, height) = options.desktop_size.unwrap_or((1280, 800));
            vm["width"] = json!(width);
            vm["height"] = json!(height);
            // A desktop has sound: the viewer plays it and owns the microphone.
            vm["audio"] = json!(true);
        }
        let mac_address = if qemu {
            None
        } else {
            // A fixed MAC and machine identifier make saved machine state
            // restorable (warm checkpoints, see `checkpoint`).
            let mac = super::macos::random_mac();
            vm["mac_address"] = json!(mac);
            vm["machine_identifier_file"] = json!(dir.join(MACHINE_IDENTIFIER_FILE));
            Some(mac)
        };
        if qemu {
            vm["ports"] = json!(ports);
            if let Some(port) = session_port {
                super::session::add_forward(&mut vm, port);
            }
            vm["qemu_guest_exec"] = json!(true);
            if let Some(profile) = gpu_profile {
                vm["qemu_graphics_profile"] = json!(profile);
            }
        }
        let container = Container {
            id: id.clone(),
            name,
            image: image.reference.clone(),
            config_digest: image.config_digest.clone(),
            argv,
            env,
            workdir: guest_config["workdir"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            user: guest_config["user"].as_str().unwrap_or_default().to_owned(),
            ports,
            volumes,
            backend: vm["backend"].as_str().unwrap().to_owned(),
            gpus: gpu_profile.map(str::to_owned),
            desktop: options.desktop,
            engine: options.engine,
            init: options.init.clone(),
            auto_remove: options.auto_remove,
            cpus,
            memory_mib,
            created_at: now(),
            vm_id: vm_id.clone(),
            os: String::new(),
            mac_address,
            session_port,
            tty: options.tty,
            stdin: options.stdin,
            fork_of: None,
            icon: None,
            desktop_size: options.desktop.then_some(options.desktop_size).flatten(),
        };
        save(&container)?;
        daemon_call("create", vm)?;
        if let Err(e) = daemon_call("start", json!({"id":vm_id})) {
            let _ = daemon_call("delete", json!({"id":vm_id,"confirm":vm_id}));
            return Err(e);
        }
        if !qemu && !container.ports.is_empty() {
            super::proxy::spawn(&container)?;
        }
        Ok(container)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    result
}

/// A container's configuration on the backend a new `--gpus` selects.
pub struct BackendSwitch {
    pub record: Container,
    pub guest_config: Value,
    pub vm: Value,
}

/// Rebuild a stopped Linux container's records for the GPU `profile` (`None`:
/// Apple Virtualization, else QEMU/HVF). `session_port` is the QEMU session
/// forward and `mac_address` the Apple MAC; the writable layer, image disks,
/// shares, published ports and session token stay as they are.
pub fn switch_backend(
    c: &Container,
    vm: &Value,
    guest_config: &Value,
    dir: &Path,
    profile: Option<&str>,
    session_port: Option<u16>,
    mac_address: Option<String>,
) -> Result<BackendSwitch> {
    let was_qemu = c.backend == "qemu-hvf";
    let qemu = profile.is_some();
    if !vm.is_object() {
        bail!("daemon returned no configuration for {}", c.vm_id)
    }
    if !qemu && c.ports.iter().any(|p| p.protocol != "tcp") {
        bail!("{} publishes UDP, which needs the QEMU backend; remove it and run it again without --gpus", c.name)
    }
    let mut record = c.clone();
    record.backend = if qemu {
        "qemu-hvf"
    } else {
        "apple-virtualization"
    }
    .to_owned();
    record.gpus = profile.map(str::to_owned);
    record.session_port = if qemu { session_port } else { None };
    record.mac_address = if qemu { None } else { mac_address };
    // Only a default follows the backend; a size given with -m stays.
    let systemd = c.init.is_some();
    if c.memory_mib == default_memory_mib(was_qemu, c.engine, c.desktop, systemd) {
        record.memory_mib = default_memory_mib(qemu, c.engine, c.desktop, systemd);
    }

    let mut guest = guest_config.clone();
    guest["transport"] = json!(if qemu { "virtio-serial-exec" } else { "vsock" });
    guest["gpu"] = json!(qemu);
    for volume in guest["volumes"].as_array_mut().into_iter().flatten() {
        volume["fs"] = json!(if qemu { "9p" } else { "virtiofs" });
    }

    let mut vm = vm.clone();
    vm["backend"] = json!(record.backend);
    vm["memory_mib"] = json!(record.memory_mib);
    let (from, to) = if qemu {
        ("console=hvc0", "console=ttyAMA0")
    } else {
        ("console=ttyAMA0", "console=hvc0")
    };
    let command_line = vm["command_line"]
        .as_str()
        .unwrap_or_default()
        .replace(from, to);
    vm["command_line"] = json!(command_line);
    let fields = vm.as_object_mut().unwrap();
    for key in [
        "ports",
        "qemu_guest_exec",
        "qemu_graphics_profile",
        "mac_address",
        "machine_identifier_file",
    ] {
        fields.remove(key);
    }
    if qemu {
        vm["ports"] = json!(record.ports);
        if let Some(port) = record.session_port {
            super::session::add_forward(&mut vm, port);
        }
        vm["qemu_guest_exec"] = json!(true);
        vm["qemu_graphics_profile"] = json!(profile);
    } else {
        vm["mac_address"] = json!(record.mac_address);
        vm["machine_identifier_file"] = json!(dir.join(MACHINE_IDENTIFIER_FILE));
    }
    Ok(BackendSwitch {
        record,
        guest_config: guest,
        vm,
    })
}

/// What `update` changes; `None` (or `false`) keeps it. `gpus` is a `--gpus`
/// value or `none`; `ports` replaces the published ports (empty: none);
/// `dns` empty and `mtu` 0 go back to the defaults `run` uses.
#[derive(Default, Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Changes {
    pub cpus: Option<u32>,
    #[serde(rename = "memoryMiB")]
    pub memory_mib: Option<u64>,
    #[serde(rename = "diskGiB")]
    pub disk_gib: Option<u64>,
    pub gpus: Option<String>,
    pub desktop: Option<bool>,
    pub desktop_size: Option<(u32, u32)>,
    pub ports: Option<Vec<String>>,
    pub dns: Option<Vec<String>>,
    pub mtu: Option<u16>,
    pub new_mac: bool,
}

/// The disk holding a container's writable layer (a macOS container's whole
/// system), and its size and allocated bytes.
pub fn disk(container: &Container) -> Result<(PathBuf, u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let name = if container.os == "darwin" {
        "disk.img"
    } else {
        "rw.raw"
    };
    let path = container_dir(&container.id)?.join(name);
    let meta = fs::metadata(&path).with_context(|| format!("read {}", path.display()))?;
    Ok((path, meta.len(), meta.blocks() * 512))
}

/// Check the parts of `changes` that need no daemon: limits, grow-only disk,
/// what macOS and engine containers allow. Returns the GPU profile change
/// (`Some(None)`: to Apple Virtualization) and the parsed ports.
#[allow(clippy::type_complexity)]
fn check_changes(
    c: &Container,
    changes: &Changes,
    disk_bytes: u64,
) -> Result<(Option<Option<&'static str>>, Option<Vec<PortSpec>>)> {
    if c.engine {
        bail!(
            "{} runs the shared engine; recreate it with veynodevm engine start",
            c.name
        )
    }
    if let Some(cpus) = changes.cpus {
        if !(1..=128).contains(&cpus) {
            bail!("--cpus must be 1..128")
        }
    }
    if let Some(memory) = changes.memory_mib {
        if !(256..=1 << 20).contains(&memory) {
            bail!("--memory must be 256m..1024g")
        }
    }
    if c.os == "darwin" {
        if changes.disk_gib.is_some()
            || changes.gpus.is_some()
            || changes.desktop.is_some()
            || changes.desktop_size.is_some()
            || changes.ports.is_some()
            || changes.dns.is_some()
            || changes.mtu.is_some()
            || changes.new_mac
        {
            bail!("a macOS container changes only --cpus and --memory; its GPU, desktop, disk and network stay as created")
        }
        if changes.cpus.is_some_and(|n| n < 2) || changes.memory_mib.is_some_and(|m| m < 4096) {
            bail!("a macOS container needs at least 2 CPUs and 4096 MiB of memory")
        }
    }
    if let Some(gib) = changes.disk_gib {
        if gib << 30 < disk_bytes {
            bail!(
                "the disk can only grow: {} is {} GiB",
                c.name,
                disk_bytes.div_ceil(1 << 30)
            )
        }
        if gib > 4096 {
            bail!("--disk-size must be at most 4096 GiB")
        }
    }
    let profile = match changes.gpus.as_deref() {
        None => None,
        Some("none") => Some(None),
        Some(other) => Some(Some(gpu_profile(other).map_err(|_| {
            anyhow::anyhow!("--gpus accepts none|all|virgl|venus, got {other}")
        })?)),
    }
    .filter(|p| *p != c.gpus.as_deref());
    let qemu = match profile {
        Some(p) => p.is_some(),
        None => c.backend == "qemu-hvf",
    };
    let ports = changes
        .ports
        .as_ref()
        .map(|specs| {
            let ports = specs
                .iter()
                .map(|p| parse_port(p))
                .collect::<Result<Vec<_>>>()?;
            if !qemu && ports.iter().any(|p| p.protocol != "tcp") {
                bail!("UDP publishing needs the QEMU backend; Apple Virtualization containers publish TCP only")
            }
            let mut seen = BTreeSet::new();
            for p in &ports {
                if !seen.insert((p.host_address, p.host_port, p.protocol.clone())) {
                    bail!("duplicate published port {}", p.host_port)
                }
            }
            Ok(ports)
        })
        .transpose()?;
    let want = changes.desktop.unwrap_or(c.desktop);
    if changes.desktop_size.is_some() && (!want || !qemu) {
        bail!("--desktop-size needs a desktop on the QEMU backend (--gpus virgl|venus)")
    }
    if changes
        .mtu
        .is_some_and(|m| m != 0 && !(576..=9000).contains(&m))
    {
        bail!("--mtu must be 576..9000")
    }
    if changes.dns.as_ref().is_some_and(|d| d.len() > 3) {
        bail!("at most 3 DNS servers")
    }
    Ok((profile, ports))
}

/// `update`: change a container's hardware (CPUs, memory, disk, GPU, desktop,
/// network) in one re-registration of its VM. A running container is refused
/// unless `restart`, which stops it first and starts it again afterwards; the
/// writable layer, image, volumes and session token stay. A disk only grows,
/// and its file system grows at the next start.
pub fn update(c: &Container, changes: &Changes, restart: bool) -> Result<Container> {
    let (disk_path, disk_bytes, _) = disk(c)?;
    let (profile, ports) = check_changes(c, changes, disk_bytes)?;
    if profile.is_some_and(|p| p.is_some()) && home::Runtime::load()?.qemu.is_none() {
        bail!("--gpus needs the QEMU/HVF GPU runtime: run runtime/fetch-utm.sh and scripts/install-dev-runtime.py --qemu")
    }
    let runtime = home::Runtime::load()?;
    let dns = changes
        .dns
        .as_ref()
        .map(|dns| -> Result<Vec<String>> {
            if dns.is_empty() {
                return Ok(runtime.dns.iter().map(ToString::to_string).collect());
            }
            dns.iter()
                .map(|d| {
                    d.parse::<std::net::IpAddr>()
                        .map(|ip| ip.to_string())
                        .with_context(|| format!("invalid --dns {d}"))
                })
                .collect()
        })
        .transpose()?;
    let mtu = changes
        .mtu
        .map(|m| if m == 0 { runtime.mtu } else { Some(m) });
    let disk_gib = changes.disk_gib.filter(|gib| gib << 30 != disk_bytes);
    let want_desktop = changes.desktop.unwrap_or(c.desktop);
    let want_size = if want_desktop {
        changes.desktop_size.or(c.desktop_size)
    } else {
        None
    };
    let changed = profile.is_some()
        || changes.cpus.is_some_and(|n| n != c.cpus)
        || changes.memory_mib.is_some_and(|m| m != c.memory_mib)
        || disk_gib.is_some()
        || want_desktop != c.desktop
        || want_size != c.desktop_size
        || ports.as_ref().is_some_and(|p| *p != c.ports)
        || dns.is_some()
        || mtu.is_some()
        || changes.new_mac;
    if !changed {
        return Ok(c.clone());
    }
    // Ports the container holds itself are freed by stopping it.
    for p in ports.iter().flatten().filter(|p| !c.ports.contains(p)) {
        port_free(p)?;
    }
    let running = active(&vm_state(c));
    if running && !restart {
        bail!(
            "container {} is running; stop it first or use --restart",
            c.name
        )
    }
    if running {
        stop(c, default_stop_timeout(c))?;
    }
    super::proxy::stop_existing(c)?;
    let dir = container_dir(&c.id)?;
    let old_vm = daemon_call("status", json!({"id":c.vm_id}))?["config"].clone();
    if !old_vm.is_object() {
        bail!("daemon returned no configuration for {}", c.vm_id)
    }
    let config_disk = dir.join("config.img");
    let old_guest = if c.os == "darwin" {
        Value::Null
    } else {
        read_config_disk(&config_disk)?
    };

    let mut base = c.clone();
    if let Some(ports) = &ports {
        base.ports = ports.clone();
    }
    let (mut record, mut guest, mut vm) = match profile {
        Some(profile) => {
            let session_port = match (profile, c.session_port) {
                (Some(_), Some(port)) => Some(port),
                (Some(_), None) => Some(super::session::free_port()?),
                (None, _) => None,
            };
            let mac = c
                .mac_address
                .clone()
                .unwrap_or_else(super::macos::random_mac);
            let s = switch_backend(
                &base,
                &old_vm,
                &old_guest,
                &dir,
                profile,
                session_port,
                Some(mac),
            )?;
            (s.record, s.guest_config, s.vm)
        }
        None => {
            let mut vm = old_vm.clone();
            if ports.is_some() && base.backend == "qemu-hvf" {
                vm["ports"] = json!(base.ports);
                if let Some(port) = base.session_port {
                    super::session::add_forward(&mut vm, port);
                }
            }
            (base, old_guest.clone(), vm)
        }
    };
    if want_desktop != record.desktop || want_size != record.desktop_size {
        let qemu = record.backend == "qemu-hvf";
        let systemd = record.init.is_some();
        // Only a default follows the desktop; a size given with -m stays.
        if record.memory_mib == default_memory_mib(qemu, record.engine, record.desktop, systemd) {
            record.memory_mib = default_memory_mib(qemu, record.engine, want_desktop, systemd);
        }
        record.desktop = want_desktop;
        record.desktop_size = want_size;
        guest["desktop"] = json!(want_desktop);
        let fields = vm.as_object_mut().unwrap();
        fields.remove("width");
        fields.remove("height");
        fields.remove("audio");
        if want_desktop {
            let (width, height) = want_size.unwrap_or((1280, 800));
            vm["width"] = json!(width);
            vm["height"] = json!(height);
            vm["audio"] = json!(true);
        }
    }
    if let Some(cpus) = changes.cpus {
        record.cpus = cpus;
    }
    if let Some(memory) = changes.memory_mib {
        record.memory_mib = memory;
    }
    vm["cpus"] = json!(record.cpus);
    vm["memory_mib"] = json!(record.memory_mib);
    if let Some(dns) = dns {
        guest["dns"] = json!(dns);
    }
    if let Some(mtu) = mtu {
        guest["mtu"] = json!(mtu);
    }
    if changes.new_mac {
        let mac = super::macos::random_mac();
        vm["mac_address"] = json!(mac);
        record.mac_address = Some(mac);
    }

    let guest_changed = guest != old_guest;
    if guest_changed {
        replace_config_disk(&config_disk, &guest)?;
    }
    daemon_call("delete", json!({"id":c.vm_id,"confirm":c.vm_id}))?;
    if let Err(e) = daemon_call("create", vm) {
        let restored = (|| {
            if guest_changed {
                replace_config_disk(&config_disk, &old_guest)?;
            }
            daemon_call("create", old_vm)
        })();
        return Err(match restored {
            Ok(_) => e.context("registering the new VM configuration failed; kept the old one"),
            Err(r) => e.context(format!(
                "registering the new VM configuration failed, and restoring the old one too: {r:#}"
            )),
        });
    }
    // Sparse: the new space costs nothing until the guest writes it.
    if let Some(gib) = disk_gib {
        OpenOptions::new()
            .write(true)
            .open(&disk_path)?
            .set_len(gib << 30)?;
    }
    save(&record)?;
    if running {
        start(&record)?;
    }
    Ok(record)
}

/// Swap the (read-only) configuration disk for one holding `guest_config`.
fn replace_config_disk(path: &Path, guest_config: &Value) -> Result<()> {
    let staged = path.with_extension("img.new");
    let _ = fs::remove_file(&staged);
    write_config_disk(&staged, guest_config)?;
    fs::rename(&staged, path)?;
    Ok(())
}

pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);
extern "C" fn on_signal(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}
pub fn install_interrupt_handler() {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

/// Print the container output from the console log. With `follow`, keep
/// reading until the container exits; returns the exit code when known.
pub fn stream_logs(
    container: &Container,
    follow: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<Option<i32>> {
    if container.os == "darwin" {
        bail!("macOS containers have no entrypoint output; use veynodevm exec")
    }
    let path = container_dir(&container.id)?.join("console.log");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        if !follow || Instant::now() > deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut file = File::open(&path)?;
    let mut buffer = Vec::new();
    let mut started = false;
    let mut last_check = Instant::now();
    loop {
        let mut chunk = [0u8; 65536];
        let n = std::io::Read::read(&mut file, &mut chunk)?;
        if n > 0 {
            buffer.extend_from_slice(&chunk[..n]);
            while let Some(newline) = buffer.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = buffer.drain(..=newline).collect();
                let line = String::from_utf8_lossy(&raw);
                let line = line.trim_end_matches(['\n', '\r']);
                match parse_marker(line) {
                    Some(("started", _)) => started = true,
                    Some(("exit", code)) => {
                        out.flush()?;
                        return Ok(code.parse().ok());
                    }
                    Some(("error", message)) => writeln!(err, "veynodevm: {message}")?,
                    Some(("warning", message)) if !started => {
                        writeln!(err, "veynodevm: warning: {message}")?
                    }
                    Some(_) => {}
                    None if started => writeln!(out, "{line}")?,
                    None => {}
                }
            }
            out.flush()?;
            continue;
        }
        if !follow {
            return Ok(None);
        }
        if INTERRUPTED.load(Ordering::SeqCst) {
            return Ok(None);
        }
        if last_check.elapsed() > Duration::from_secs(1) {
            last_check = Instant::now();
            if !active(&vm_state(container)) {
                // Drain anything written just before the stop.
                let position = file.stream_position()?;
                if fs::metadata(&path)?.len() <= position {
                    return Ok(None);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Wait for the VM process to stop (the guest powers off after `exit`).
pub fn wait_stopped(container: &Container, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !active(&vm_state(container)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Start a stopped container again. The previous run's console is kept as
/// `console.prev.log` so markers (ip, exit) always describe the current run.
pub fn start(container: &Container) -> Result<()> {
    start_from(container, None)
}

/// `start`, or with `machine_state` resume a saved machine state instead of
/// booting (`checkpoint`): the run continues, so the console is kept.
pub fn start_from(container: &Container, machine_state: Option<&Path>) -> Result<()> {
    if active(&vm_state(container)) {
        bail!("container {} is already running", container.name)
    }
    let dir = container_dir(&container.id)?;
    if machine_state.is_none() && dir.join("console.log").exists() {
        fs::rename(dir.join("console.log"), dir.join("console.prev.log"))?;
    }
    super::session::refresh_forward(container)?;
    if machine_state.is_none() && container.os != "darwin" {
        // A booting guest gets a seed it has not seen before.
        let config_disk = dir.join("config.img");
        replace_config_disk(&config_disk, &read_config_disk(&config_disk)?)?;
    }
    let mut request = json!({"id":container.vm_id});
    if let Some(path) = machine_state {
        request["restore_machine_state"] = json!(path);
    }
    daemon_call("start", request)?;
    if container.os == "darwin" {
        super::macos::wait_started(container)?;
    }
    if !container.ports.is_empty() && container.backend == "apple-virtualization" {
        super::proxy::spawn(container)?;
    }
    if container.os == "darwin" && !container.argv.is_empty() {
        // Like an entrypoint, the COMMAND runs again on every start.
        super::macos::start_command(container)?;
    }
    Ok(())
}

fn systemd_poweroff() -> Vec<String> {
    ["systemctl", "--no-block", "poweroff"]
        .map(str::to_owned)
        .to_vec()
}

/// `stop` without `-t`: a whole distribution needs longer than one process.
pub fn default_stop_timeout(container: &Container) -> Duration {
    Duration::from_secs(if container.init.is_some() { 30 } else { 10 })
}

pub fn stop(container: &Container, timeout: Duration) -> Result<()> {
    super::proxy::stop_existing(container)?;
    if !active(&vm_state(container)) {
        return Ok(());
    }
    if container.os == "darwin" {
        return super::macos::stop(container, timeout);
    }
    // The guest service powers the micro-VM off; fall back to a hard stop.
    // Under systemd that is `systemctl poweroff`: units stop in order. QEMU's
    // shutdown is a power-button press the micro-VM kernel has no driver for,
    // so ask systemd through the guest service there.
    let asked = container.init.is_some()
        && container.backend == "qemu-hvf"
        && exec_once(container, &systemd_poweroff(), 10_000, &json!({}))
            .is_ok_and(|r| r["exit_code"].as_i64() == Some(0));
    if !asked {
        let _ = daemon_call("shutdown", json!({"id":container.vm_id,"mode":"guest"}));
    }
    if wait_stopped(container, timeout) {
        return Ok(());
    }
    daemon_call(
        "force_stop",
        json!({"id":container.vm_id,"confirm":container.vm_id}),
    )?;
    if !wait_stopped(container, Duration::from_secs(15)) {
        bail!("container {} did not stop", container.name)
    }
    Ok(())
}

pub fn remove(container: &Container, force: bool) -> Result<()> {
    if active(&vm_state(container)) {
        if !force {
            bail!(
                "container {} is running; stop it first or use -f",
                container.name
            )
        }
        stop(container, Duration::from_secs(5))?;
    }
    match daemon_call(
        "delete",
        json!({"id":container.vm_id,"confirm":container.vm_id}),
    ) {
        Ok(_) => {}
        Err(e) if e.to_string().contains("VM not found") => {}
        Err(e) => return Err(e),
    }
    super::proxy::stop_existing(container)?;
    let dir = container_dir(&container.id)?;
    // config.img is read-only; directory removal handles it.
    fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
    Ok(())
}

/// Run a bounded command in a running container through the guest service.
pub fn exec(container: &Container, argv: &[String], timeout_ms: u64) -> Result<Value> {
    exec_as(container, argv, timeout_ms, &json!({}))
}

/// `exec` with `{user, env, workdir}` (`exec -u -e -w`), applied by the Guest.
pub fn exec_as(
    container: &Container,
    argv: &[String],
    timeout_ms: u64,
    identity: &Value,
) -> Result<Value> {
    // The guest service comes up a few seconds after boot. QEMU's daemon refuses
    // with "not ready yet"; Apple Virtualization refuses before its socket exists
    // (invalid_state) or fails the vsock connect (guest_unavailable). None of
    // these dispatched anything, so wait and retry while the VM is still up.
    let deadline = Instant::now() + Duration::from_secs(60);
    let retryable = |e: &anyhow::Error| {
        let text = e.to_string();
        let refused = text.contains("not ready yet")
            || (container.backend == "apple-virtualization"
                && (text.contains("guest_unavailable") || text.contains("invalid_state")));
        refused && active(&vm_state(container))
    };
    loop {
        match exec_once(container, argv, timeout_ms, identity) {
            Err(e) if retryable(&e) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(500));
            }
            other => return other,
        }
    }
}

fn exec_once(
    container: &Container,
    argv: &[String],
    timeout_ms: u64,
    identity: &Value,
) -> Result<Value> {
    // The guest kills the command's process group at the timeout.
    let mut arguments = json!({"id":container.vm_id,"argv":argv,"timeout_ms":timeout_ms});
    for key in ["user", "env", "workdir"] {
        if !identity[key].is_null() {
            arguments[key] = identity[key].clone();
        }
    }
    let reply = daemon_call("exec", arguments)?;
    Ok(if reply["result"].is_object() {
        reply["result"].clone()
    } else {
        reply
    })
}

pub fn inspect(container: &Container) -> Result<Value> {
    let mut console = read_console(container);
    if container.os == "darwin" {
        console.ip = super::macos::guest_ip(container, Duration::ZERO).ok();
        // VZMacGraphicsDevice always presents Apple's paravirtualized GPU.
        console.gpu = Some("Apple Paravirtual device (Metal)".into());
    }
    let (state, status) = status_text(container);
    let vm = daemon_call("status", json!({"id":container.vm_id})).unwrap_or(Value::Null);
    let disk = disk(container).ok();
    // What the guest configures its network with (`update --dns --mtu`).
    let guest = read_config_disk(&container_dir(&container.id)?.join("config.img")).ok();
    Ok(json!({
        "dns": guest.as_ref().map(|g| g["dns"].clone()),
        "mtu": guest.as_ref().map(|g| g["mtu"].clone()),
        "disk_bytes": disk.as_ref().map(|d| d.1),
        "disk_used_bytes": disk.as_ref().map(|d| d.2),
        "container": container,
        "state": state,
        "status": status,
        "ip": console.ip,
        "exit_code": console.exit,
        "gpu": console.gpu,
        "errors": console.errors,
        "vm": vm,
        "directory": container_dir(&container.id)?,
    }))
}

/// Config digests still needed by containers (kept by image gc).
pub fn images_in_use() -> Result<HashSet<String>> {
    Ok(list()?.into_iter().map(|c| c.config_digest).collect())
}

/// Follow the console log until the IP marker appears (macOS: DHCP lease).
pub fn wait_for_ip(container: &Container, timeout: Duration) -> Result<String> {
    if container.os == "darwin" {
        return super::macos::guest_ip(container, timeout);
    }
    let deadline = Instant::now() + timeout;
    let path = container_dir(&container.id)?.join("console.log");
    let mut position = 0;
    loop {
        if let Ok(mut file) = File::open(&path) {
            file.seek(SeekFrom::Start(position))?;
            let mut reader = BufReader::new(file);
            let mut line = String::new();
            while reader.read_line(&mut line)? > 0 {
                if !line.ends_with('\n') {
                    break;
                }
                position += line.len() as u64;
                if let Some(("ip", ip)) = parse_marker(line.trim_end()) {
                    return Ok(ip.to_owned());
                }
                if let Some(("exit", _)) = parse_marker(line.trim_end()) {
                    bail!("container exited before networking was ready")
                }
                line.clear();
            }
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for container IP address")
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_disk_carries_a_fresh_seed() {
        let dir = std::env::temp_dir().join(format!("vvm-seed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.img");
        let config = json!({"id":"c-1"});
        write_config_disk(&path, &config).unwrap();
        let first = fs::read(&path).unwrap();
        assert_eq!(first.len(), CONFIG_DISK_BYTES);
        assert_eq!(read_config_disk(&path).unwrap(), config);
        let seed = SEED_OFFSET + SEED_MAGIC.len();
        assert_eq!(&first[SEED_OFFSET..seed], SEED_MAGIC);
        replace_config_disk(&path, &config).unwrap();
        let second = fs::read(&path).unwrap();
        assert_eq!(read_config_disk(&path).unwrap(), config);
        assert_ne!(
            first[seed..seed + SEED_BYTES],
            second[seed..seed + SEED_BYTES]
        );
        assert!(
            write_config_disk(&dir.join("big.img"), &json!({"x":"y".repeat(SEED_OFFSET)})).is_err()
        );
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn icons() {
        assert_eq!(parse_icon("box").unwrap(), "box");
        assert_eq!(parse_icon("monitor:blue").unwrap(), "monitor:blue");
        for bad in ["", "boat", "box:", "box:red", "box:blue:x", ":blue"] {
            assert!(parse_icon(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn ports() {
        assert_eq!(parse_port("8080:80").unwrap().host_port, 8080);
        assert_eq!(parse_port("80").unwrap().guest_port, 80);
        let p = parse_port("0.0.0.0:53:53/udp").unwrap();
        assert_eq!(
            (p.host_address.to_string().as_str(), p.protocol.as_str()),
            ("0.0.0.0", "udp")
        );
        assert!(parse_port("x:80").is_err());
        assert!(parse_port("80/sctp").is_err());
    }
    #[test]
    fn volumes() {
        let v = parse_volume("/tmp/a:/data:ro").unwrap();
        assert!(v.read_only);
        assert_eq!(v.target, "/data");
        assert!(parse_volume("named:/data").is_err());
        assert!(parse_volume("/a:rel").is_err());
    }
    #[test]
    fn markers() {
        assert_eq!(parse_marker("@@veynodevm exit 3\r"), Some(("exit", "3")));
        assert_eq!(
            parse_marker("[ 1.0] x @@veynodevm ip 192.168.64.5"),
            Some(("ip", "192.168.64.5"))
        );
        assert_eq!(parse_marker("hello"), None);
    }
    #[test]
    fn systemd_init_argv() {
        let mut options = RunOptions::default();
        assert_eq!(systemd_argv(&options).unwrap(), ["/sbin/init"]);
        options.command = vec![
            "/lib/systemd/systemd".into(),
            "systemd.unit=multi-user.target".into(),
        ];
        assert_eq!(systemd_argv(&options).unwrap(), options.command);
        options.command = vec!["bash".into()];
        assert!(systemd_argv(&options).is_err());
        options.command.clear();
        options.user = Some("ubuntu".into());
        assert!(systemd_argv(&options).is_err());
        options.user = None;
        options.entrypoint = Some(vec!["/init".into()]);
        assert!(systemd_argv(&options).is_err());
    }
    fn record(backend: &str, gpus: Option<&str>, memory_mib: u64) -> Container {
        let qemu = backend == "qemu-hvf";
        serde_json::from_value(json!({"id":"a","name":"n","image":"i","config_digest":"d","argv":["sh"],"env":[],"workdir":"","user":"","ports":[{"host_address":"127.0.0.1","host_port":8080,"guest_port":80,"protocol":"tcp"}],"volumes":[{"host":"/h/data","target":"/data","read_only":false}],"backend":backend,"gpus":gpus,"desktop":false,"engine":false,"auto_remove":false,"cpus":2,"memory_mib":memory_mib,"created_at":0,"vm_id":"c-a","mac_address":if qemu {None} else {Some("02:00:00:00:00:01")},"session_port":if qemu {Some(41000)} else {None},"icon":"box:blue"})).unwrap()
    }
    fn guest(qemu: bool) -> Value {
        json!({"version":1,"id":"a","argv":["sh"],"session_token":"t","transport":if qemu {"virtio-serial-exec"} else {"vsock"},"gpu":qemu,
            "volumes":[{"tag":"v0","target":"/data","read_only":false,"fs":if qemu {"9p"} else {"virtiofs"}}]})
    }
    fn vm(qemu: bool) -> Value {
        let mut vm = json!({"backend":if qemu {"qemu-hvf"} else {"apple-virtualization"},"id":"c-a","guest":"linux","cpus":2,
            "memory_mib":if qemu {2048} else {512},"kernel":"/k","initrd":"/i",
            "command_line":format!("console={} rdinit=/veynode/init quiet loglevel=1 panic=-1", if qemu {"ttyAMA0"} else {"hvc0"}),
            "disk":"/h/c/a/rw.raw","extra_disks":[{"path":"/r","read_only":true},{"path":"/h/c/a/config.img","read_only":true},{"path":"/m","read_only":true}],
            "serial_log":"/h/c/a/console.log","shares":[{"tag":"v0","path":"/h/data","read_only":false}]});
        if qemu {
            vm["ports"] = json!([{"host_address":"127.0.0.1","host_port":8080,"guest_port":80,"protocol":"tcp"}]);
            super::super::session::add_forward(&mut vm, 41000);
            vm["qemu_guest_exec"] = json!(true);
            vm["qemu_graphics_profile"] = json!("virgl");
        } else {
            vm["mac_address"] = json!("02:00:00:00:00:01");
            vm["machine_identifier_file"] = json!("/h/c/a/machine-identifier");
        }
        vm
    }
    #[test]
    fn gpu_switch_to_qemu() {
        let c = record("apple-virtualization", None, 512);
        let dir = Path::new("/h/c/a");
        let s = switch_backend(
            &c,
            &vm(false),
            &guest(false),
            dir,
            Some("virgl"),
            Some(41000),
            None,
        )
        .unwrap();
        assert_eq!(s.record.backend, "qemu-hvf");
        assert_eq!(s.record.gpus.as_deref(), Some("virgl"));
        assert_eq!(s.record.session_port, Some(41000));
        assert_eq!(s.record.mac_address, None);
        assert_eq!(s.record.memory_mib, 2048);
        assert_eq!(
            (s.record.name.as_str(), s.record.icon.as_deref()),
            ("n", Some("box:blue"))
        );
        // Built the way `run --gpus virgl` builds it.
        assert_eq!(s.guest_config, guest(true));
        assert_eq!(s.vm, vm(true));
    }
    #[test]
    fn gpu_switch_to_apple() {
        let c = record("qemu-hvf", Some("virgl"), 2048);
        let dir = Path::new("/h/c/a");
        let mac = Some("02:00:00:00:00:01".to_owned());
        let s = switch_backend(&c, &vm(true), &guest(true), dir, None, None, mac).unwrap();
        assert_eq!(s.record.backend, "apple-virtualization");
        assert_eq!((s.record.gpus, s.record.session_port), (None, None));
        assert_eq!(s.record.memory_mib, 512);
        assert_eq!(s.guest_config, guest(false));
        let mut expected = vm(false);
        expected["machine_identifier_file"] = json!(dir.join(MACHINE_IDENTIFIER_FILE));
        assert_eq!(s.vm, expected);
    }
    #[test]
    fn gpu_switch_keeps_an_explicit_memory_size_and_profile_changes() {
        let c = record("qemu-hvf", Some("virgl"), 8192);
        let s = switch_backend(
            &c,
            &vm(true),
            &guest(true),
            Path::new("/h/c/a"),
            Some("venus-moltenvk-experimental"),
            Some(41000),
            None,
        )
        .unwrap();
        assert_eq!(s.record.memory_mib, 8192);
        assert_eq!(s.vm["memory_mib"], 8192);
        assert_eq!(s.vm["qemu_graphics_profile"], "venus-moltenvk-experimental");
        assert_eq!(s.vm["ports"], vm(true)["ports"]);
        let mut udp = record("qemu-hvf", Some("virgl"), 2048);
        udp.ports[0].protocol = "udp".into();
        assert!(switch_backend(
            &udp,
            &vm(true),
            &guest(true),
            Path::new("/h/c/a"),
            None,
            None,
            None
        )
        .is_err());
    }
    #[test]
    fn records_without_init_still_load() {
        let record = json!({"id":"a","name":"n","image":"i","config_digest":"d","argv":["sh"],"env":[],"workdir":"","user":"","ports":[],"volumes":[],"backend":"apple-virtualization","gpus":null,"desktop":false,"engine":false,"auto_remove":false,"cpus":2,"memory_mib":512,"created_at":0,"vm_id":"c-a"});
        let container: Container = serde_json::from_value(record).unwrap();
        assert_eq!(container.init, None);
        assert_eq!(default_stop_timeout(&container), Duration::from_secs(10));
        assert!(serde_json::to_value(&container)
            .unwrap()
            .get("init")
            .is_none());
    }
    #[test]
    fn hardware_changes_are_checked_before_the_daemon() {
        let apple = record("apple-virtualization", None, 512);
        let qemu = record("qemu-hvf", Some("virgl"), 2048);
        let gib = 1u64 << 30;
        let check = |c: &Container, changes: Changes| check_changes(c, &changes, 16 * gib);
        // The disk only grows.
        assert!(check(
            &apple,
            Changes {
                disk_gib: Some(8),
                ..Default::default()
            }
        )
        .is_err());
        assert!(check(
            &apple,
            Changes {
                disk_gib: Some(32),
                ..Default::default()
            }
        )
        .is_ok());
        assert!(check(
            &apple,
            Changes {
                cpus: Some(0),
                ..Default::default()
            }
        )
        .is_err());
        assert!(check(
            &apple,
            Changes {
                memory_mib: Some(128),
                ..Default::default()
            }
        )
        .is_err());
        assert!(check(
            &apple,
            Changes {
                mtu: Some(100),
                ..Default::default()
            }
        )
        .is_err());
        assert!(check(
            &apple,
            Changes {
                mtu: Some(0),
                ..Default::default()
            }
        )
        .is_ok());
        let dns = |n: usize| Some(vec!["1.1.1.1".to_owned(); n]);
        assert!(check(
            &apple,
            Changes {
                dns: dns(4),
                ..Default::default()
            }
        )
        .is_err());
        // UDP needs QEMU, whichever backend the change ends on.
        let udp = Some(vec!["5353:53/udp".to_owned()]);
        assert!(check(
            &apple,
            Changes {
                ports: udp.clone(),
                ..Default::default()
            }
        )
        .is_err());
        assert!(check(
            &qemu,
            Changes {
                ports: udp.clone(),
                ..Default::default()
            }
        )
        .is_ok());
        let to_apple = Changes {
            gpus: Some("none".into()),
            ports: udp,
            ..Default::default()
        };
        assert!(check(&qemu, to_apple).is_err());
        let dup = Some(vec!["8080:80".to_owned(), "8080:81".to_owned()]);
        assert!(check(
            &apple,
            Changes {
                ports: dup,
                ..Default::default()
            }
        )
        .is_err());
        // The GPU it already has is no change.
        let (profile, _) = check(
            &qemu,
            Changes {
                gpus: Some("virgl".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(profile, None);
        let (profile, _) = check(
            &qemu,
            Changes {
                gpus: Some("none".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(profile, Some(None));
        assert!(check(
            &apple,
            Changes {
                desktop_size: Some((1920, 1080)),
                desktop: Some(true),
                ..Default::default()
            }
        )
        .is_err());
        let mut mac = apple.clone();
        mac.os = "darwin".into();
        assert!(check(
            &mac,
            Changes {
                cpus: Some(4),
                memory_mib: Some(8192),
                ..Default::default()
            }
        )
        .is_ok());
        assert!(check(
            &mac,
            Changes {
                new_mac: true,
                ..Default::default()
            }
        )
        .is_err());
        let mut engine = qemu.clone();
        engine.engine = true;
        assert!(check(
            &engine,
            Changes {
                cpus: Some(4),
                ..Default::default()
            }
        )
        .is_err());
    }
}
