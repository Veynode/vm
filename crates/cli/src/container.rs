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
use crate::{daemon_call, random_hex};
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
use vm_control::home;

pub const CONFIG_MAGIC: &[u8] = b"VEYNODEVM-CONFIG-V1\n";
/// Apple Virtualization generic machine identifier, created by the worker.
pub const MACHINE_IDENTIFIER_FILE: &str = "vz-machine-id.bin";
const CONFIG_DISK_BYTES: usize = 1 << 20;
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
    pub fork_of: Option<crate::checkpoint::ForkOrigin>,
}

#[derive(Default, Clone, Debug)]
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
    pub memory_mib: Option<u64>,
    pub disk_gib: Option<u64>,
    pub gpus: Option<String>,
    pub desktop: bool,
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
        if let Ok(bytes) = fs::read(&path) {
            if let Ok(c) = serde_json::from_slice::<Container>(&bytes) {
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
    let dir = container_dir(&container.id)?;
    let tmp = dir.join("container.json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(container)?)?;
    fs::rename(tmp, dir.join("container.json"))?;
    Ok(())
}

/// Write the read-only configuration disk (vdc) the guest agent reads at boot.
pub fn write_config_disk(path: &Path, guest_config: &Value) -> Result<()> {
    let mut body = CONFIG_MAGIC.to_vec();
    body.extend(serde_json::to_vec(guest_config)?);
    body.push(b'\n');
    if body.len() > CONFIG_DISK_BYTES {
        bail!("container configuration exceeds 1 MiB")
    }
    body.resize(CONFIG_DISK_BYTES, 0);
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
    match daemon_call("status", json!({"id":container.vm_id})) {
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

fn ensure_image(store: &Store, name: &str, quiet: bool) -> Result<ImageRecord> {
    if let Some(image) = store.find(name)? {
        return Ok(image);
    }
    if !quiet {
        eprintln!("Unable to find image '{name}' locally");
    }
    store.pull(name, &mut |m| {
        if !quiet {
            eprintln!("{m}")
        }
    })
}

/// Validated `--name`, or one derived from the image reference.
pub fn container_name(options: &RunOptions, image: &ImageRecord, id: &str) -> Result<String> {
    Ok(match &options.name {
        Some(n) => {
            if n.is_empty()
                || n.len() > 64
                || !n
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            {
                bail!("container name must be [A-Za-z0-9_.-]{{1,64}}")
            }
            if list()?.iter().any(|c| &c.name == n) {
                bail!("container name already in use: {n}")
            }
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

/// Create and start a container. Returns the record; the caller attaches.
pub fn create_and_start(options: &RunOptions) -> Result<Container> {
    let store = Store::open(&home::images_dir()?)?;
    let image = ensure_image(&store, &options.image, options.quiet)?;
    if image.config.os == "darwin" {
        if options.init.is_some() {
            bail!("--init systemd is for Linux images")
        }
        return crate::macos::create_and_start(options, &store, &image);
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
            eprintln!("{m}")
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
    let gpu_profile = match options.gpus.as_deref() {
        None => None,
        Some("all" | "virgl" | "gl") => Some("virgl"),
        Some("venus" | "vulkan") => Some("venus-moltenvk-experimental"),
        Some(other) => bail!("--gpus accepts all|virgl|venus, got {other}"),
    };
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
        private_file(&dir.join(crate::session::TOKEN_FILE))?.write_all(session_token.as_bytes())?;
        let session_port = if qemu {
            Some(crate::session::free_port()?)
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
        // A full desktop distribution (GNOME, KDE) needs more than a single app.
        let memory_mib =
            options
                .memory_mib
                .unwrap_or(if systemd_init.is_some() && options.desktop {
                    4096
                } else if qemu || options.engine || options.desktop || systemd_init.is_some() {
                    2048
                } else {
                    512
                });
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
            vm["width"] = json!(1280);
            vm["height"] = json!(800);
        }
        let mac_address = if qemu {
            None
        } else {
            // A fixed MAC and machine identifier make saved machine state
            // restorable (warm checkpoints, see `checkpoint`).
            let mac = crate::macos::random_mac();
            vm["mac_address"] = json!(mac);
            vm["machine_identifier_file"] = json!(dir.join(MACHINE_IDENTIFIER_FILE));
            Some(mac)
        };
        if qemu {
            vm["ports"] = json!(ports);
            if let Some(port) = session_port {
                crate::session::add_forward(&mut vm, port);
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
        };
        save(&container)?;
        daemon_call("create", vm)?;
        if let Err(e) = daemon_call("start", json!({"id":vm_id})) {
            let _ = daemon_call("delete", json!({"id":vm_id,"confirm":vm_id}));
            return Err(e);
        }
        if !qemu && !container.ports.is_empty() {
            crate::proxy::spawn(&container)?;
        }
        Ok(container)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    result
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
    crate::session::refresh_forward(container)?;
    let mut request = json!({"id":container.vm_id});
    if let Some(path) = machine_state {
        request["restore_machine_state"] = json!(path);
    }
    daemon_call("start", request)?;
    if container.os == "darwin" {
        crate::macos::wait_started(container)?;
    }
    if !container.ports.is_empty() && container.backend == "apple-virtualization" {
        crate::proxy::spawn(container)?;
    }
    if container.os == "darwin" && !container.argv.is_empty() {
        // Like an entrypoint, the COMMAND runs again on every start.
        crate::macos::start_command(container)?;
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
    crate::proxy::stop_existing(container)?;
    if !active(&vm_state(container)) {
        return Ok(());
    }
    if container.os == "darwin" {
        return crate::macos::stop(container, timeout);
    }
    // The guest service powers the micro-VM off; fall back to a hard stop.
    // Under systemd that is `systemctl poweroff`: units stop in order. QEMU's
    // shutdown is a power-button press the micro-VM kernel has no driver for,
    // so ask systemd through the guest service there.
    let asked = container.init.is_some()
        && container.backend == "qemu-hvf"
        && exec_once(container, &systemd_poweroff(), 10_000)
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
    crate::proxy::stop_existing(container)?;
    let dir = container_dir(&container.id)?;
    // config.img is read-only; directory removal handles it.
    fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
    Ok(())
}

/// Run a bounded command in a running container through the guest service.
pub fn exec(container: &Container, argv: &[String], timeout_ms: u64) -> Result<Value> {
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
        match exec_once(container, argv, timeout_ms) {
            Err(e) if retryable(&e) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(500));
            }
            other => return other,
        }
    }
}

fn exec_once(container: &Container, argv: &[String], timeout_ms: u64) -> Result<Value> {
    // The guest kills the command's process group at the timeout.
    let reply = daemon_call(
        "exec",
        json!({"id":container.vm_id,"argv":argv,"timeout_ms":timeout_ms}),
    )?;
    Ok(if reply["result"].is_object() {
        reply["result"].clone()
    } else {
        reply
    })
}

pub fn inspect(container: &Container) -> Result<Value> {
    let mut console = read_console(container);
    if container.os == "darwin" {
        console.ip = crate::macos::guest_ip(container, Duration::ZERO).ok();
        // VZMacGraphicsDevice always presents Apple's paravirtualized GPU.
        console.gpu = Some("Apple Paravirtual device (Metal)".into());
    }
    let (state, status) = status_text(container);
    let vm = daemon_call("status", json!({"id":container.vm_id})).unwrap_or(Value::Null);
    Ok(json!({
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
        return crate::macos::guest_ip(container, timeout);
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
}
