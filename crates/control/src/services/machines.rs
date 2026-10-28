//! Machines (`machine.*`): persistent VMs booted through UEFI with their own
//! kernel and bootloader. Linux from a raw disk image or an installer ISO,
//! Windows 11 ARM64 from the user's ISO (QEMU/HVF only), macOS from an OCI
//! image (`services::macos`).
//!
//! A machine is a daemon VM whose id is the machine name, plus a private
//! directory `<home>/machines/NAME` holding a sparse raw `disk.raw`, the
//! per-VM UEFI variable store `efi-vars.fd` (created by the worker on first
//! start), `serial.log`, `machine.json` and, for SSH guests, `ssh/`. The
//! installer ISO is attached read-only until `machine.eject`.
use super::{
    pump,
    records::{self, Kind},
    util::{self, flag, opt_string, opt_u64, string},
};
use crate::{
    api_server::{Ctx, StreamIo},
    home,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use veynodevm_api::{refuse, ErrorCode};

/// `machine.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Machine {
    pub name: String,
    /// Stays the same when the machine is renamed (the web UI's addresses);
    /// methods take it in place of the name. Older records get one when read.
    #[serde(default)]
    pub id: String,
    pub backend: String,
    #[serde(default = "linux")]
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso: Option<PathBuf>,
    pub disk_gib: u64,
    pub created_at: u64,
    /// The sticker the UI shows for it, as a container's (`KIND[:COLOR]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The disk image the machine's disk was cloned from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_source: Option<PathBuf>,
    /// The OCI image a macOS machine was created from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The driver image attached for a Windows install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drivers_iso: Option<PathBuf>,
    /// `machine update --idle-pause`: the daemon pauses the VM after this
    /// many seconds without activity (`crate::idle`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_pause_s: Option<u64>,
}

fn linux() -> String {
    "linux".into()
}

/// What `machine.create` builds.
pub struct Spec {
    pub name: String,
    pub qemu: bool,
    pub windows: bool,
    pub gpu_profile: Option<&'static str>,
    pub iso: Option<PathBuf>,
    pub cpus: u32,
    pub memory_mib: u64,
    pub monitor: bool,
    pub attach: Vec<PathBuf>,
    pub nvme: bool,
    /// QEMU user-network TCP forwards, 127.0.0.1:HOST → guest GUEST.
    pub publish: Vec<(u16, u16)>,
    /// virtiofs shares: tag, host path, read-only.
    pub shares: Vec<(String, PathBuf, bool)>,
    pub mac_address: Option<String>,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    refuse(ErrorCode::Invalid, message)
}
fn state_error(message: impl Into<String>) -> anyhow::Error {
    refuse(ErrorCode::State, message)
}

/// The `qemu_graphics_profile` for a GPU name: virgl and Venus need a Linux
/// guest, Neptune a Windows one (`crate::qemu_graphics`).
pub fn gpu_profile(value: &str, windows: bool) -> Result<&'static str> {
    Ok(match (windows, value) {
        (false, "all" | "virgl" | "gl") => "virgl",
        (false, "venus" | "vulkan") => "venus-moltenvk-experimental",
        // UTM 5.0.5 Neptune + DXMT; the guest needs UTM's viogpu3d driver.
        (true, "all" | "neptune" | "dxmt" | "d3d") => "neptune-dxmt-experimental",
        (false, "neptune" | "dxmt" | "d3d") => {
            return Err(invalid(format!("gpu {value} needs a Windows guest")))
        }
        (true, "virgl" | "gl" | "venus" | "vulkan") => {
            return Err(invalid(format!(
                "gpu {value} needs a Linux guest; Windows accepts all|neptune"
            )))
        }
        (false, other) => return Err(invalid(format!("gpu accepts all|virgl|venus, got {other}"))),
        (true, other) => {
            return Err(invalid(format!(
                "gpu for Windows accepts all|neptune, got {other}"
            )))
        }
    })
}

/// The API's GPU name for a VM configuration's graphics profile.
fn gpu_name(profile: &Value) -> Value {
    match profile.as_str() {
        Some("virgl") => json!("virgl"),
        Some("venus-moltenvk-experimental") => json!("venus"),
        Some("neptune-dxmt-experimental") => json!("neptune"),
        _ => Value::Null,
    }
}

/// The daemon VM configuration for a machine living in `dir`.
pub fn vm_config(spec: &Spec, dir: &Path) -> Value {
    let mut vm = json!({
        "backend": if spec.qemu {"qemu-hvf"} else {"apple-virtualization"},
        "id": spec.name,
        "guest": if spec.windows {"windows"} else {"linux"},
        "cpus": spec.cpus,
        "memory_mib": spec.memory_mib,
        "boot": "uefi",
        "disk": dir.join("disk.raw"),
        "efi_vars": dir.join("efi-vars.fd"),
        "serial_log": dir.join("serial.log"),
    });
    if let Some(iso) = &spec.iso {
        vm["cdrom"] = json!(iso);
    }
    if !spec.attach.is_empty() {
        vm["extra_disks"] = json!(spec
            .attach
            .iter()
            .map(|path| json!({"path":path,"read_only":true}))
            .collect::<Vec<_>>());
    }
    if !spec.shares.is_empty() {
        vm["shares"] = json!(spec
            .shares
            .iter()
            .map(|(tag, path, read_only)| json!({"tag":tag,"path":path,"read_only":read_only}))
            .collect::<Vec<_>>());
    }
    if let Some(mac) = &spec.mac_address {
        vm["mac_address"] = json!(mac);
    }
    if spec.qemu {
        // The viewer plays the sound card and owns the microphone.
        vm["audio"] = json!(true);
        if let Some(profile) = spec.gpu_profile {
            vm["qemu_graphics_profile"] = json!(profile);
        }
        if spec.monitor {
            vm["qemu_monitor"] = json!(true);
        }
        if spec.nvme {
            vm["qemu_disk_bus"] = json!("nvme");
        }
        if !spec.publish.is_empty() {
            vm["ports"] = ports_config(&spec.publish);
        }
    } else {
        vm["width"] = json!(1920);
        vm["height"] = json!(1080);
    }
    vm
}

fn ports_config(ports: &[(u16, u16)]) -> Value {
    json!(ports
        .iter()
        .map(|(host, guest)| json!({"host_address":"127.0.0.1","host_port":host,"guest_port":guest,"protocol":"tcp"}))
        .collect::<Vec<_>>())
}

/// `[{host, guest}]` TCP ports.
fn parse_ports(value: &Value) -> Result<Vec<(u16, u16)>> {
    let Some(items) = value.as_array() else {
        return Err(invalid("ports must be an array of {host, guest}"));
    };
    items
        .iter()
        .map(|port| {
            let field = |name: &str| {
                port[name]
                    .as_u64()
                    .filter(|n| (1..=65535).contains(n))
                    .map(|n| n as u16)
                    .ok_or_else(|| invalid(format!("ports[].{name} must be a TCP port")))
            };
            Ok((field("host")?, field("guest")?))
        })
        .collect()
}

pub fn dir(name: &str) -> Result<PathBuf> {
    crate::validate_id(name).map_err(|e| invalid(e.to_string()))?;
    Ok(home::machines_dir()?.join(name))
}

pub fn load(name: &str) -> Result<Machine> {
    let path = dir(name)?.join("machine.json");
    if !path.is_file() {
        return Err(refuse(
            ErrorCode::NotFound,
            format!("no such machine: {name}"),
        ));
    }
    Ok(serde_json::from_value(records::read(
        Kind::Machine,
        &path,
    )?)?)
}

/// The machine's name for `arg`, which is its name or its id; `arg` itself
/// where neither matches (the method then says there is no such machine).
pub fn resolve(arg: &str) -> String {
    if dir(arg).is_ok_and(|d| d.join("machine.json").is_file()) {
        return arg.to_owned();
    }
    records()
        .into_iter()
        .find(|m| !m.id.is_empty() && m.id == arg)
        .map_or_else(|| arg.to_owned(), |m| m.name)
}

fn records() -> Vec<Machine> {
    let Ok(entries) = home::machines_dir().and_then(|d| Ok(fs::read_dir(d)?)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| records::read(Kind::Machine, &e.path().join("machine.json")).ok())
        .filter_map(|record| serde_json::from_value::<Machine>(record).ok())
        .collect()
}

pub fn save(machine: &Machine) -> Result<()> {
    let path = dir(&machine.name)?.join("machine.json");
    records::write(&path, &records::stamped(Kind::Machine, machine)?)
}

/// The daemon's state of a VM: its own word, `missing` when it is not
/// registered, `unknown` when the daemon did not say.
pub fn vm_state(ctx: &Ctx, name: &str) -> String {
    match ctx.vm("status", json!({"id":name})) {
        Ok(v) => v["state"].as_str().unwrap_or("unknown").to_owned(),
        Err(e) if e.to_string().contains("VM not found") => "missing".to_owned(),
        Err(_) => "unknown".to_owned(),
    }
}

pub fn running(state: &str) -> bool {
    !matches!(
        state,
        "stopped" | "crashed" | "error" | "missing" | "unknown"
    )
}

/// The API's state for a daemon state.
fn api_state(state: &str) -> &str {
    match state {
        "starting" | "running" | "stopping" | "stopped" => state,
        "crashed" | "error" | "missing" | "unknown" => "failed",
        other => other,
    }
}

/// The machine's disk in its directory: macOS machines keep the image's name.
fn disk_file(machine: &Machine) -> &'static str {
    if machine.os == "macos" {
        "disk.img"
    } else {
        "disk.raw"
    }
}

/// The machine as `machine.ls` and `machine.inspect` give it (`docs/api.md`),
/// with the record and the daemon's VM beside it for the CLI.
pub fn view(ctx: &Ctx, machine: &Machine) -> Value {
    let status = ctx.vm("status", json!({"id":machine.name})).ok();
    let raw = match &status {
        Some(v) => v["state"].as_str().unwrap_or("unknown").to_owned(),
        None => vm_state(ctx, &machine.name),
    };
    let config = status.as_ref().map_or(Value::Null, |s| s["config"].clone());
    let used = dir(&machine.name)
        .ok()
        .and_then(|d| fs::metadata(d.join(disk_file(machine))).ok())
        .map_or(0, |m| m.blocks() * 512);
    let guest_path = |tag: &str| match machine.os.as_str() {
        "linux" => json!(format!("/mnt/{tag}")),
        "macos" => json!(format!("{}/{tag}", super::macos::SHARED)),
        _ => Value::Null,
    };
    let shares = config["shares"]
        .as_array()
        .map(|shares| {
            shares
                .iter()
                .map(|s| {
                    let tag = s["tag"].as_str().unwrap_or_default();
                    json!({"hostPath":s["path"],"tag":tag,"readOnly":s["read_only"].as_bool().unwrap_or(false),"guestPath":guest_path(tag)})
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let ports = config["ports"]
        .as_array()
        .map(|ports| {
            ports
                .iter()
                .filter(|p| p["guest_port"] != crate::services::SESSION_PORT)
                .map(|p| json!({"host":p["host_port"],"guest":p["guest_port"]}))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({
        "name": machine.name,
        "id": machine.id,
        "os": machine.os,
        "backend": machine.backend,
        "state": api_state(&raw),
        "source": {
            "iso": machine.iso,
            "disk": machine.disk_source,
            "from": machine.image,
            "driversIso": machine.drivers_iso,
        },
        "cpus": config["cpus"],
        "memoryMiB": config["memory_mib"],
        "diskGiB": machine.disk_gib,
        "gpu": gpu_name(&config["qemu_graphics_profile"]),
        "macAddress": config["mac_address"],
        "shares": shares,
        "ports": ports,
        "createdAt": machine.created_at,
        "diskUsedBytes": used,
        "icon": machine.icon,
        "idlePauseS": machine.idle_pause_s.unwrap_or(0),
        "vmState": raw,
        "record": machine,
        "vm": status,
    })
}

fn name_arg(args: &Value) -> Result<String> {
    Ok(resolve(string(args, "name")?))
}

pub fn ls(ctx: &Ctx, _: Value) -> Result<Value> {
    let mut machines = records();
    machines.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Value::Array(
        machines.iter().map(|m| view(ctx, m)).collect(),
    ))
}

pub fn inspect(ctx: &Ctx, args: Value) -> Result<Value> {
    Ok(view(ctx, &load(&name_arg(&args)?)?))
}

/// The `Spec` `machine.create` asks for, with the source it names.
fn create_spec(ctx: &Ctx, args: &Value) -> Result<(Spec, u64, Option<PathBuf>)> {
    let name = string(args, "name")?.to_owned();
    crate::validate_id(&name).map_err(|e| invalid(e.to_string()))?;
    let windows = match opt_string(args, "os")? {
        None | Some("linux") => false,
        Some("windows") => true,
        Some(other) => {
            return Err(invalid(format!(
                "os accepts linux|windows|macos, got {other}"
            )))
        }
    };
    let gpu_profile = opt_string(args, "gpu")?
        .filter(|g| *g != "none")
        .map(|value| gpu_profile(value, windows))
        .transpose()?;
    let qemu = match opt_string(args, "backend")? {
        None => windows || gpu_profile.is_some(),
        Some("qemu" | "qemu-hvf") => true,
        Some("apple" | "apple-virtualization") if windows => {
            return Err(invalid(
                "Apple Virtualization cannot run Windows; use the QEMU/HVF backend",
            ))
        }
        Some("apple" | "apple-virtualization") if gpu_profile.is_none() => false,
        Some("apple" | "apple-virtualization") => {
            return Err(invalid("a GPU needs the QEMU/HVF backend"))
        }
        Some(other) => return Err(invalid(format!("backend accepts apple|qemu, got {other}"))),
    };
    if qemu && !ctx.runtime.qemu {
        return Err(refuse(
            ErrorCode::Unsupported,
            "the QEMU backend needs the QEMU/HVF runtime: run runtime/fetch-utm.sh and scripts/install-dev-runtime.py --qemu",
        ));
    }
    let monitor = flag(args, "monitor")?;
    if monitor && !qemu {
        return Err(invalid("monitor is a QEMU option"));
    }
    let nvme = match opt_string(args, "diskBus")? {
        None | Some("virtio") => false,
        Some("nvme") if qemu => true,
        Some("nvme") => return Err(invalid("diskBus nvme is a QEMU option")),
        Some(other) => return Err(invalid(format!("diskBus accepts virtio|nvme, got {other}"))),
    };
    let publish = match &args["ports"] {
        Value::Null => Vec::new(),
        ports => parse_ports(ports)?,
    };
    if !publish.is_empty() && !qemu {
        return Err(invalid(
            "ports are a QEMU option (Apple Virtualization guests are reachable at their address)",
        ));
    }
    let existing = |field: &str, path: &str| {
        fs::canonicalize(path).map_err(|_| invalid(format!("{field} not found: {path}")))
    };
    let disk = opt_string(args, "disk")?
        .map(|p| existing("disk", p))
        .transpose()?;
    let iso = opt_string(args, "iso")?
        .map(|p| existing("iso", p))
        .transpose()?;
    if disk.is_some() && iso.is_some() {
        return Err(invalid("give one source: disk or iso"));
    }
    if disk.is_some() && windows {
        return Err(invalid("Windows machines are installed from an iso"));
    }
    let mut attach = match &args["attach"] {
        Value::Null => Vec::new(),
        Value::Array(paths) => paths
            .iter()
            .map(|p| existing("attach image", p.as_str().unwrap_or_default()))
            .collect::<Result<_>>()?,
        _ => return Err(invalid("attach must be an array of paths")),
    };
    if let Some(drivers) = opt_string(args, "driversIso")? {
        attach.push(existing("driversIso", drivers)?);
    }
    if flag(args, "unattended")? && attach.is_empty() {
        return Err(refuse(
            ErrorCode::Unsupported,
            "an unattended Windows install needs its answer file attached (attach: an image holding Autounattend.xml)",
        ));
    }
    let shares = match &args["shares"] {
        Value::Null => Vec::new(),
        Value::Array(shares) if windows && !shares.is_empty() => {
            return Err(invalid("Windows machines take no shares"))
        }
        Value::Array(shares) => shares
            .iter()
            .map(|s| {
                let path = existing("share", s["hostPath"].as_str().unwrap_or_default())?;
                let tag = s["tag"]
                    .as_str()
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| invalid("shares[].tag is required"))?;
                Ok((
                    tag.to_owned(),
                    path,
                    s["readOnly"].as_bool().unwrap_or(false),
                ))
            })
            .collect::<Result<_>>()?,
        _ => return Err(invalid("shares must be an array")),
    };
    let disk_gib = match opt_u64(args, "diskGiB")? {
        Some(gib) if !(4..=4096).contains(&gib) => {
            return Err(invalid("diskGiB must be 4 through 4096"))
        }
        Some(gib) => gib,
        None if windows => 64,
        None => 32,
    };
    let cpus = opt_u64(args, "cpus")?.unwrap_or(4);
    if !(1..=128).contains(&cpus) {
        return Err(invalid("cpus must be 1 through 128"));
    }
    let memory_mib = opt_u64(args, "memoryMiB")?.unwrap_or(4096);
    if !(256..=1 << 20).contains(&memory_mib) {
        return Err(invalid("memoryMiB must be 256 through 1048576"));
    }
    Ok((
        Spec {
            name,
            qemu,
            windows,
            gpu_profile,
            iso,
            cpus: cpus as u32,
            memory_mib,
            monitor,
            attach,
            nvme,
            publish,
            shares,
            // A known address lets `machine.endpoints` find the guest's lease.
            mac_address: (!qemu).then(util::random_mac),
        },
        disk_gib,
        disk,
    ))
}

pub fn create(ctx: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
    if opt_string(&args, "os")? == Some("macos") || !args["from"].is_null() {
        return super::macos::create_machine(ctx, &args, io);
    }
    let (spec, disk_gib, disk) = create_spec(ctx, &args)?;
    let name = spec.name.clone();
    let dir = dir(&name)?;
    if dir.exists() || vm_state(ctx, &name) != "missing" {
        return Err(state_error(format!("machine {name} already exists")));
    }
    home::private_dir(&dir)?;
    let result = (|| -> Result<Value> {
        let target = dir.join("disk.raw");
        let disk_gib = match &disk {
            Some(source) => {
                io.progress("Cloning the disk")?;
                if util::clone_file(source, &target).is_err() {
                    io.progress("Copying the disk (it is on another volume)")?;
                    fs::copy(source, &target)?;
                    fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
                }
                let have = fs::metadata(&target)?.len();
                let want = disk_gib << 30;
                if want < have && !args["diskGiB"].is_null() {
                    return Err(invalid(format!(
                        "diskGiB {disk_gib} is smaller than the image's {} GiB",
                        have.div_ceil(1 << 30)
                    )));
                }
                if want > have {
                    OpenOptions::new()
                        .write(true)
                        .open(&target)?
                        .set_len(want)?;
                }
                want.max(have).div_ceil(1 << 30)
            }
            None => {
                // Sparse: only blocks the guest writes take host space.
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&target)?
                    .set_len(disk_gib << 30)?;
                disk_gib
            }
        };
        io.progress("Registering the machine")?;
        ctx.vm("create", vm_config(&spec, &dir))?;
        let machine = Machine {
            name: name.clone(),
            backend: if spec.qemu {
                "qemu-hvf"
            } else {
                "apple-virtualization"
            }
            .into(),
            os: if spec.windows { "windows" } else { "linux" }.into(),
            iso: spec.iso.clone(),
            disk_gib,
            id: util::random_hex(6),
            created_at: util::now(),
            icon: None,
            disk_source: disk.clone(),
            image: None,
            drivers_iso: opt_string(&args, "driversIso")?.map(PathBuf::from),
            idle_pause_s: None,
        };
        save(&machine)?;
        Ok(view(ctx, &machine))
    })();
    if result.is_err() {
        if vm_state(ctx, &name) != "missing" {
            let _ = ctx.vm("delete", json!({"id":name,"confirm":name}));
        }
        cleanup_files(&dir);
    }
    result
}

/// Remove only the files a machine owns, then its directory (never recursive).
pub fn cleanup_files(dir: &Path) {
    for file in [
        "disk.raw",
        "disk.img",
        "efi-vars.fd",
        "serial.log",
        "machine.json",
        "machine.json.tmp",
        // macOS machines (`services::macos`).
        "login",
        "askpass",
        "known_hosts",
        "setup",
        "grant",
        "grant.running",
    ] {
        let _ = fs::remove_file(dir.join(file));
    }
    for sub in ["ssh", "machine"] {
        if let Ok(entries) = fs::read_dir(dir.join(sub)) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
        let _ = fs::remove_dir(dir.join(sub));
    }
    if dir.exists() && fs::remove_dir(dir).is_err() {
        crate::log_line(&format!(
            "machine: left {} in place: it contains files VeynodeVM did not create",
            dir.display()
        ));
    }
}

pub fn start(ctx: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
    let name = name_arg(&args)?;
    let machine = load(&name)?;
    if running(&vm_state(ctx, &name)) {
        return Err(state_error(format!("{name} is running")));
    }
    io.progress("Starting")?;
    ctx.vm(
        "start",
        json!({"id":name,"idle_pause_s":machine.idle_pause_s.unwrap_or(0)}),
    )?;
    if flag(&args, "wait")? {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let state = vm_state(ctx, &name);
            if state == "running" {
                break;
            }
            if !running(&state) {
                bail!("{name} stopped while starting ({state})")
            }
            if Instant::now() > deadline || io.closed() {
                bail!("{name} did not reach running within 120 s")
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    Ok(view(ctx, &machine))
}

/// `machine.pause`: stops the VM's vCPUs; its memory stays in use.
pub fn pause(ctx: &Ctx, args: Value) -> Result<Value> {
    let name = name_arg(&args)?;
    load(&name)?;
    ctx.vm("pause", json!({"id":name}))?;
    Ok(json!({ "name": name }))
}

/// `machine.unpause`: resumes a paused machine.
pub fn unpause(ctx: &Ctx, args: Value) -> Result<Value> {
    let name = name_arg(&args)?;
    load(&name)?;
    ctx.vm("resume", json!({"id":name}))?;
    Ok(json!({ "name": name }))
}

pub fn stop(ctx: &Ctx, args: Value) -> Result<Value> {
    let name = name_arg(&args)?;
    load(&name)?;
    if !running(&vm_state(ctx, &name)) {
        return Err(state_error(format!("{name} is not running")));
    }
    if flag(&args, "force")? {
        ctx.vm("force_stop", json!({"id":name,"confirm":name}))?;
    } else {
        ctx.vm("shutdown", json!({"id":name}))?;
    }
    Ok(json!({ "name": name }))
}

fn wait_stopped(ctx: &Ctx, name: &str, timeout: Duration) -> bool {
    let until = Instant::now() + timeout;
    while Instant::now() < until {
        if !running(&vm_state(ctx, name)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

pub fn rm(ctx: &Ctx, args: Value) -> Result<Value> {
    let name = name_arg(&args)?;
    let dir = dir(&name)?;
    load(&name)?;
    let state = vm_state(ctx, &name);
    if running(&state) {
        if !flag(&args, "force")? {
            return Err(state_error(format!("{name} is running; stop it or force")));
        }
        ctx.vm("force_stop", json!({"id":name,"confirm":name}))?;
        wait_stopped(ctx, &name, Duration::from_secs(20));
    }
    if state != "missing" {
        ctx.vm("delete", json!({"id":name,"confirm":name}))?;
    }
    cleanup_files(&dir);
    Ok(json!({ "name": name }))
}

/// A stopped machine's daemon configuration, for an edit.
fn stopped_config(ctx: &Ctx, name: &str, doing: &str) -> Result<Value> {
    match vm_state(ctx, name).as_str() {
        "unknown" => bail!("cannot read {name}'s state from the daemon"),
        "missing" => bail!("{name} is not registered with the daemon"),
        state if running(state) => return Err(state_error(format!("stop {name} before {doing}"))),
        _ => {}
    }
    Ok(ctx.vm("status", json!({"id":name}))?["config"].clone())
}

/// VM configurations are immutable in the daemon: a changed one is deleted
/// and created again. If the new one is refused, the old one is created
/// again, so the machine stays registered.
fn reregister(ctx: &Ctx, name: &str, old: Value, new: Value) -> Result<()> {
    ctx.vm("delete", json!({"id":name,"confirm":name}))?;
    if let Err(e) = ctx.vm("create", new) {
        if let Err(restore) = ctx.vm("create", old) {
            bail!("re-registering {name} failed: {e:#}; restoring its old configuration also failed: {restore:#}")
        }
        bail!("re-registering {name} failed (its old configuration is kept): {e:#}")
    }
    Ok(())
}

/// What `machine.update` was asked to change; `None` leaves a field alone.
#[derive(Default)]
pub struct Changes {
    pub cpus: Option<u32>,
    pub memory_mib: Option<u64>,
    pub disk_gib: Option<u64>,
    /// `Some(None)` removes the GPU.
    pub gpus: Option<Option<String>>,
    /// `Some(empty)` clears the forwards.
    pub publish: Option<Vec<(u16, u16)>>,
    pub mac_address: Option<String>,
    /// The QEMU sound card.
    pub audio: Option<bool>,
}

impl Changes {
    fn from_args(args: &Value) -> Result<Self> {
        let changes = Changes {
            cpus: opt_u64(args, "cpus")?.map(|n| n as u32),
            memory_mib: opt_u64(args, "memoryMiB")?,
            disk_gib: opt_u64(args, "diskGiB")?,
            gpus: match args.get("gpu") {
                None => None,
                Some(Value::Null) => Some(None),
                Some(Value::String(g)) if g == "none" => Some(None),
                Some(Value::String(g)) => Some(Some(g.clone())),
                Some(_) => return Err(invalid("gpu must be a string or null")),
            },
            publish: match &args["ports"] {
                Value::Null => None,
                ports => Some(parse_ports(ports)?),
            },
            mac_address: flag(args, "newMac")?.then(util::random_mac),
            audio: match &args["audio"] {
                Value::Null => None,
                Value::Bool(b) => Some(*b),
                _ => return Err(invalid("audio must be true or false")),
            },
        };
        if changes.cpus.is_none()
            && changes.memory_mib.is_none()
            && changes.disk_gib.is_none()
            && changes.gpus.is_none()
            && changes.publish.is_none()
            && changes.mac_address.is_none()
            && changes.audio.is_none()
        {
            return Err(invalid(
                "update needs at least one of cpus, memoryMiB, diskGiB, gpu, ports, audio, newMac",
            ));
        }
        if let Some(gib) = changes.disk_gib {
            if !(4..=4096).contains(&gib) {
                return Err(invalid("diskGiB must be 4 through 4096"));
            }
        }
        Ok(changes)
    }
}

/// Applies `changes` to a machine's daemon VM configuration (not the disk
/// size, which is the disk file's: see `grown_disk`).
pub fn edit_config(config: &mut Value, machine: &Machine, changes: &Changes) -> Result<()> {
    let qemu = machine.backend == "qemu-hvf";
    let fields = config.as_object_mut().context("invalid VM configuration")?;
    if let Some(cpus) = changes.cpus {
        if !(1..=128).contains(&cpus) {
            return Err(invalid("cpus must be 1 through 128"));
        }
        fields.insert("cpus".into(), json!(cpus));
    }
    if let Some(memory) = changes.memory_mib {
        if !(256..=1 << 20).contains(&memory) {
            return Err(invalid("memory must be 256 MiB through 1 TiB"));
        }
        fields.insert("memory_mib".into(), json!(memory));
    }
    match &changes.gpus {
        None => {}
        Some(None) => {
            fields.remove("qemu_graphics_profile");
        }
        Some(Some(_)) if !qemu => return Err(invalid("a GPU needs the QEMU/HVF backend")),
        Some(Some(value)) => {
            let profile = gpu_profile(value, machine.os == "windows")?;
            fields.insert("qemu_graphics_profile".into(), json!(profile));
        }
    }
    match changes.audio {
        None => {}
        Some(false) => {
            fields.remove("audio");
        }
        Some(true) if !qemu => {
            return Err(invalid("audio needs the QEMU/HVF backend (an installed Apple Virtualization guest has no VeynodeVM audio relay)"))
        }
        Some(true) => {
            fields.insert("audio".into(), json!(true));
        }
    }
    match &changes.publish {
        None => {}
        Some(ports) if ports.is_empty() => {
            fields.remove("ports");
        }
        Some(_) if !qemu => return Err(invalid(
            "ports are a QEMU option (Apple Virtualization guests are reachable at their address)",
        )),
        Some(ports) => {
            fields.insert("ports".into(), ports_config(ports));
        }
    }
    if let Some(mac) = &changes.mac_address {
        fields.insert("mac_address".into(), json!(mac));
    }
    Ok(())
}

/// The new disk size when `requested` grows it, `None` when it is the same;
/// disks never shrink (the guest's filesystem would lose its end).
pub fn grown_disk(current: u64, requested: Option<u64>) -> Result<Option<u64>> {
    match requested {
        Some(gib) if gib < current => Err(invalid(format!(
            "disks can only grow: {gib} GiB is less than the current {current} GiB"
        ))),
        Some(gib) if gib > current => Ok(Some(gib)),
        _ => Ok(None),
    }
}

pub fn update(ctx: &Ctx, args: Value) -> Result<Value> {
    let name = name_arg(&args)?;
    let mut machine = load(&name)?;
    // The idle pause needs no restart: it is recorded, and a running VM gets it now.
    if let Some(seconds) = opt_u64(&args, "idlePauseS")? {
        machine.idle_pause_s =
            super::container::check_idle_pause(seconds).map_err(|e| invalid(e.to_string()))?;
        save(&machine)?;
        if running(&vm_state(ctx, &name)) {
            ctx.vm("idle_policy", json!({"id":name,"idle_pause_s":seconds}))?;
        }
        if args
            .as_object()
            .is_some_and(|a| a.keys().all(|k| k == "name" || k == "idlePauseS"))
        {
            return Ok(json!({ "name": name }));
        }
    }
    let changes = Changes::from_args(&args)?;
    let old = stopped_config(ctx, &name, "changing its hardware")?;
    let mut config = old.clone();
    edit_config(&mut config, &machine, &changes)?;
    let grow = grown_disk(machine.disk_gib, changes.disk_gib)?;
    if config != old {
        reregister(ctx, &name, old, config)?;
    }
    if let Some(gib) = grow {
        // Sparse: the new end takes no host space until the guest writes it.
        if machine.os == "macos" {
            super::macos::grow_machine_disk(&dir(&name)?, gib)?;
        } else {
            OpenOptions::new()
                .write(true)
                .open(dir(&name)?.join("disk.raw"))?
                .set_len(gib << 30)?;
        }
        machine.disk_gib = gib;
        save(&machine)?;
    }
    Ok(json!({ "name": name }))
}

/// Every string in a VM configuration that names a file under `from`, moved under `to`.
pub fn moved_paths(value: &mut Value, from: &Path, to: &Path) {
    match value {
        Value::String(s) => {
            if let Ok(rest) = Path::new(s.as_str()).strip_prefix(from) {
                *s = to.join(rest).display().to_string();
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| moved_paths(v, from, to)),
        Value::Object(fields) => fields.values_mut().for_each(|v| moved_paths(v, from, to)),
        _ => {}
    }
}

fn new_name(ctx: &Ctx, args: &Value) -> Result<(String, PathBuf)> {
    let new = string(args, "newName")?.to_owned();
    let to = dir(&new)?;
    if to.exists() || vm_state(ctx, &new) != "missing" {
        return Err(state_error(format!("machine name already in use: {new}")));
    }
    Ok((new, to))
}

/// `machine.rename`: the name is the daemon VM id and the directory, so a
/// stopped machine moves to the new directory and is registered again under
/// the new id with its paths moved (VM configurations are immutable in the
/// daemon); the old registration goes last, so a failure leaves the old
/// machine as it was.
pub fn rename(ctx: &Ctx, args: Value) -> Result<Value> {
    let old = name_arg(&args)?;
    let mut machine = load(&old)?;
    if string(&args, "newName")? == old {
        return Ok(view(ctx, &machine));
    }
    let state = vm_state(ctx, &old);
    if state == "unknown" {
        bail!("cannot read {old}'s state from the daemon")
    }
    if running(&state) {
        return Err(state_error(format!("stop {old} before renaming it")));
    }
    let (new, to) = new_name(ctx, &args)?;
    let from = dir(&old)?;
    let config = (state != "missing")
        .then(|| {
            ctx.vm("status", json!({"id":old}))
                .map(|v| v["config"].clone())
        })
        .transpose()?;
    fs::rename(&from, &to)?;
    if let Some(mut config) = config {
        config["id"] = json!(new);
        moved_paths(&mut config, &from, &to);
        if let Err(e) = ctx.vm("create", config) {
            let _ = fs::rename(&to, &from);
            bail!("registering {new} failed: {e:#}")
        }
        ctx.vm("delete", json!({"id":old,"confirm":old}))?;
    }
    machine.name = new;
    save(&machine)?;
    Ok(view(ctx, &machine))
}

/// `machine.clone`: a stopped machine's files cloned with APFS into a new
/// machine, registered with a new MAC address. The UEFI variable store is
/// cloned too, so the copy boots the same entry; macOS machines get a fresh
/// hardware identity from `services::macos`.
pub fn clone(ctx: &Ctx, args: Value) -> Result<Value> {
    let source = name_arg(&args)?;
    let machine = load(&source)?;
    if running(&vm_state(ctx, &source)) {
        return Err(state_error(format!("stop {source} before cloning it")));
    }
    let mut config = stopped_config(ctx, &source, "cloning it")?;
    let (new, to) = new_name(ctx, &args)?;
    let from = dir(&source)?;
    home::private_dir(&to)?;
    let result = (|| -> Result<Value> {
        for file in ["disk.raw", "disk.img", "efi-vars.fd"] {
            if from.join(file).is_file() {
                util::clone_file(&from.join(file), &to.join(file))?;
            }
        }
        if from.join("ssh").is_dir() {
            let ssh = home::private_dir(&to.join("ssh"))?;
            for entry in fs::read_dir(from.join("ssh"))?.flatten() {
                if entry.file_type()?.is_file() {
                    let target = ssh.join(entry.file_name());
                    fs::copy(entry.path(), &target)?;
                    fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
                }
            }
        }
        config["id"] = json!(new);
        moved_paths(&mut config, &from, &to);
        if let Some(fields) = config.as_object_mut() {
            fields.insert("mac_address".into(), json!(util::random_mac()));
            // Two machines cannot share host ports.
            fields.remove("ports");
        }
        if machine.os == "macos" {
            super::macos::fresh_identity(&from, &to, &mut config)?;
        }
        ctx.vm("create", config.clone())?;
        let copy = Machine {
            name: new.clone(),
            id: util::random_hex(6),
            created_at: util::now(),
            ..machine.clone()
        };
        save(&copy)?;
        Ok(view(ctx, &copy))
    })();
    if result.is_err() {
        if vm_state(ctx, &new) != "missing" {
            let _ = ctx.vm("delete", json!({"id":new,"confirm":new}));
        }
        cleanup_files(&to);
    }
    result
}

pub fn eject(ctx: &Ctx, args: Value) -> Result<Value> {
    let name = name_arg(&args)?;
    let mut machine = load(&name)?;
    let old = stopped_config(ctx, &name, "ejecting the installer ISO")?;
    let mut config = old.clone();
    if config["cdrom"].is_null() && config["extra_disks"].is_null() {
        return Err(state_error(format!(
            "{name} has no installer ISO or seed image attached"
        )));
    }
    let fields = config.as_object_mut().context("invalid VM configuration")?;
    fields.remove("cdrom");
    fields.remove("extra_disks");
    reregister(ctx, &name, old, config)?;
    machine.iso = None;
    save(&machine)?;
    Ok(json!({ "name": name }))
}

pub fn icon(_: &Ctx, args: Value) -> Result<Value> {
    let name = name_arg(&args)?;
    let mut machine = load(&name)?;
    machine.icon = opt_string(&args, "icon")?
        .map(util::parse_icon)
        .transpose()?;
    save(&machine)?;
    Ok(json!({"name":machine.name,"icon":machine.icon}))
}

const SSH_USER_CHARS: &[u8] = b"._-";

/// `machine.ssh_key`: copies the private key and records the user in
/// `<machine>/ssh/`, where the daemon's Windows guest tools find them
/// (`crate::windows_guest`).
pub fn ssh_key(_: &Ctx, args: Value) -> Result<Value> {
    let name = name_arg(&args)?;
    let machine = load(&name)?;
    if machine.os != "windows" {
        return Err(refuse(
            ErrorCode::Unsupported,
            format!(
                "ssh_key is for Windows machines; {name} runs {}",
                machine.os
            ),
        ));
    }
    let user = string(&args, "user")?;
    if user.is_empty()
        || !user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || SSH_USER_CHARS.contains(&b))
    {
        return Err(invalid(
            "user takes a Windows account name (letters, digits, . _ -)",
        ));
    }
    let key = fs::read(string(&args, "key")?).map_err(|_| invalid("cannot read key"))?;
    if !key.starts_with(b"-----BEGIN OPENSSH PRIVATE KEY-----") {
        return Err(invalid("key must be an OpenSSH private key"));
    }
    let ssh = home::private_dir(&dir(&name)?.join("ssh"))?;
    for (file, bytes) in [("id_ed25519", key.as_slice()), ("user", user.as_bytes())] {
        let tmp = ssh.join(format!("{file}.tmp"));
        let _ = fs::remove_file(&tmp);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| std::io::Write::write_all(&mut f, bytes))?;
        fs::rename(tmp, ssh.join(file))?;
    }
    Ok(json!({"name":name,"ssh":ssh}))
}

/// The guest-side script: ChangeDisplaySettings with CDS_UPDATEREGISTRY, which only
/// takes effect on the interactive desktop, so `display` runs it as a one-shot task.
const DISPLAY_SCRIPT: &str = include_str!("windows_display.ps1");

pub fn base64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// `machine.display`: a Windows machine's display mode, through the daemon's
/// Windows guest login; answers with the guest's own `result=0 now=WxH` line.
pub fn display(ctx: &Ctx, args: Value) -> Result<Value> {
    let name = name_arg(&args)?;
    let machine = load(&name)?;
    if machine.os != "windows" {
        return Err(refuse(
            ErrorCode::Unsupported,
            format!(
                "display is for Windows machines; {name} runs {}",
                machine.os
            ),
        ));
    }
    let (w, h) = (
        opt_u64(&args, "width")?.unwrap_or(0),
        opt_u64(&args, "height")?.unwrap_or(0),
    );
    if !(640..=3840).contains(&w) || !(480..=2160).contains(&h) {
        return Err(invalid("size must be between 640x480 and 3840x2160"));
    }
    let script = format!(
        "$ErrorActionPreference='Stop'; $f=Join-Path $env:TEMP 'veynode-display.ps1'; $o=Join-Path $env:TEMP 'veynode-display.out'; \
         [IO.File]::WriteAllBytes($f,[Convert]::FromBase64String('{}')); Remove-Item $o -ErrorAction SilentlyContinue; \
         schtasks /create /tn VeynodeDisplay /tr ('cmd /c powershell -NoProfile -ExecutionPolicy Bypass -File ' + $f + ' -W {w} -H {h} > ' + $o + ' 2>&1') /sc once /st 00:00 /it /ru $env:USERNAME /f | Out-Null; \
         schtasks /run /tn VeynodeDisplay | Out-Null; \
         for($i=0;$i -lt 80 -and -not ((Test-Path $o) -and (Get-Item $o).Length -gt 0);$i++){{Start-Sleep -Milliseconds 250}}; Get-Content $o; \
         schtasks /delete /tn VeynodeDisplay /f | Out-Null",
        base64(DISPLAY_SCRIPT.as_bytes())
    );
    let reply = ctx.vm(
        "exec",
        json!({"id":name,"argv":["powershell","-NoProfile","-Command",script],"timeout_ms":30000}),
    )?;
    let reply = if reply["result"].is_object() {
        reply["result"].clone()
    } else {
        reply
    };
    let out = reply["stdout"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if reply["exit_code"] != 0 || !out.contains("result=0") {
        bail!(
            "the guest did not change its display mode: {}",
            if out.is_empty() {
                reply["stderr"].as_str().unwrap_or("no output").trim()
            } else {
                &out
            }
        )
    }
    Ok(json!({"name":name,"width":w,"height":h,"display":out}))
}

/// `machine.endpoints`: where the guest and its published ports are. An Apple
/// Virtualization guest is on vmnet's NAT, found by its DHCP lease, and
/// reaches the host at that network's first address; a QEMU guest is behind
/// user networking, reached only through its forwards, and sees the host at
/// 10.0.2.2.
pub fn endpoints(ctx: &Ctx, args: Value) -> Result<Value> {
    let name = name_arg(&args)?;
    let machine = load(&name)?;
    let status = ctx.vm("status", json!({"id":name}))?;
    let config = &status["config"];
    let live = running(status["state"].as_str().unwrap_or("unknown"));
    let (address, host) = if machine.backend == "qemu-hvf" {
        (None, Some("10.0.2.2".to_string()))
    } else {
        let address = config["mac_address"]
            .as_str()
            .filter(|_| live)
            .and_then(util::leased_ip);
        let host = address.as_deref().and_then(|ip| {
            let (net, _) = ip.rsplit_once('.')?;
            Some(format!("{net}.1"))
        });
        (address, host)
    };
    let ports = config["ports"]
        .as_array()
        .map(|ports| {
            ports
                .iter()
                .filter(|p| p["guest_port"] != crate::services::SESSION_PORT)
                .map(|p| json!({"host":p["host_port"],"guest":p["guest_port"],"address":p["host_address"]}))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(json!({"address":address,"hostAddress":host,"ports":ports}))
}

/// `machine.exec`. Windows and macOS guests run the command over SSH, with
/// input, output and signals streamed. Linux guests run it through the guest
/// service's bounded synchronous exec (at most 30 s, output when it ends);
/// their input is read to its end first and handed to the command whole.
pub fn exec(ctx: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
    let name = name_arg(&args)?;
    let machine = load(&name)?;
    let _awake = util::KeepAwake::new(&name);
    let status = ctx.vm("status", json!({"id":name}))?;
    if !running(status["state"].as_str().unwrap_or("unknown")) {
        return Err(state_error(format!("{name} is not running")));
    }
    let argv: Vec<String> = serde_json::from_value(args["argv"].clone())
        .map_err(|_| invalid("argv must be an array of strings"))?;
    if argv.is_empty() {
        return Err(invalid("argv is empty"));
    }
    let env: Vec<String> = match &args["env"] {
        Value::Null => Vec::new(),
        env => serde_json::from_value(env.clone())
            .map_err(|_| invalid("env must be an array of KEY=VALUE"))?,
    };
    let stdin = flag(&args, "stdin")?;
    let code = match machine.os.as_str() {
        "windows" => {
            let config: crate::Config = serde_json::from_value(status["config"].clone())?;
            let ssh = crate::windows_guest::Ssh::locate(&ctx.state_dir, &config)?;
            if opt_string(&args, "user")?.is_some() {
                return Err(refuse(
                    ErrorCode::Unsupported,
                    "Windows commands run as the machine's SSH login",
                ));
            }
            pump::run(ssh.stdio(&windows_start(&argv, &env)), stdin, io)?
        }
        "macos" => super::macos::exec_machine(ctx, &machine, &argv, &env, &args, stdin, io)?,
        _ => linux_exec(ctx, &name, argv, env, &args, stdin, io)?,
    };
    Ok(json!({ "exitCode": code }))
}

/// A PowerShell line that runs `argv` with the SSH session's own stdio, so
/// its input and output stream, and exits with its code. The arguments are
/// quoted for CommandLineToArgvW, as `windows_guest::exec` does.
fn windows_start(argv: &[String], env: &[String]) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let mut line = format!(
        "$i=New-Object Diagnostics.ProcessStartInfo; $i.FileName={}; $i.Arguments={}; $i.UseShellExecute=$false; ",
        quote(&argv[0]),
        quote(&crate::windows_guest::command_line(&argv[1..]))
    );
    for entry in env {
        let (key, value) = entry.split_once('=').unwrap_or((entry, ""));
        line.push_str(&format!(
            "$i.EnvironmentVariables[{}]={}; ",
            quote(key),
            quote(value)
        ));
    }
    line.push_str("$p=[Diagnostics.Process]::Start($i); $p.WaitForExit(); exit $p.ExitCode");
    line
}

/// The most input a Linux machine's command takes (it travels in the request).
const LINUX_STDIN_MAX: usize = 512 * 1024;

fn linux_exec(
    ctx: &Ctx,
    name: &str,
    mut argv: Vec<String>,
    env: Vec<String>,
    args: &Value,
    stdin: bool,
    io: &StreamIo,
) -> Result<i32> {
    let mut request = json!({"id":name,"timeout_ms":30000});
    if stdin {
        let mut input = Vec::new();
        loop {
            match io.stdin(Duration::from_millis(100)) {
                crate::api_server::Input::Data(data) => {
                    input.extend(data);
                    if input.len() > LINUX_STDIN_MAX {
                        return Err(refuse(
                            ErrorCode::Unsupported,
                            format!("a Linux machine's command takes at most {LINUX_STDIN_MAX} bytes of input; copy larger input with files.copy"),
                        ));
                    }
                }
                crate::api_server::Input::Eof => break,
                crate::api_server::Input::Idle => {}
                crate::api_server::Input::Closed => bail!("the client left"),
            }
        }
        let mut wrapped = vec![
            "/bin/sh".to_string(),
            "-c".into(),
            "printf %s \"$1\" | base64 -d | { shift; \"$@\"; }".into(),
            "sh".into(),
            base64(&input),
        ];
        wrapped.append(&mut argv);
        argv = wrapped;
    }
    request["argv"] = json!(argv);
    if !env.is_empty() {
        request["env"] = json!(env);
    }
    if let Some(user) = opt_string(args, "user")? {
        request["user"] = json!(user);
    }
    if let Some(cwd) = opt_string(args, "cwd")? {
        request["workdir"] = json!(cwd);
    }
    let reply = ctx.vm("exec", request)?;
    let result = if reply["result"].is_object() {
        &reply["result"]
    } else {
        &reply
    };
    io.stdout(result["stdout"].as_str().unwrap_or_default().as_bytes())?;
    io.stderr(result["stderr"].as_str().unwrap_or_default().as_bytes())?;
    Ok(result["exit_code"].as_i64().unwrap_or(255) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renaming_moves_only_paths_in_the_machine_directory() {
        let mut config = json!({"id":"a","disk":"/h/machines/a/disk.raw",
            "cdrom":"/h/machines/ab.iso","extra_disks":[{"path":"/h/machines/a/seed.img"}]});
        moved_paths(
            &mut config,
            Path::new("/h/machines/a"),
            Path::new("/h/machines/b"),
        );
        assert_eq!(config["disk"], "/h/machines/b/disk.raw");
        assert_eq!(config["cdrom"], "/h/machines/ab.iso");
        assert_eq!(config["extra_disks"][0]["path"], "/h/machines/b/seed.img");
        assert_eq!(config["id"], "a");
    }

    fn machine(backend: &str, os: &str) -> Machine {
        Machine {
            name: "m".into(),
            backend: backend.into(),
            os: os.into(),
            iso: None,
            disk_gib: 32,
            id: String::new(),
            created_at: 0,
            icon: None,
            disk_source: None,
            image: None,
            drivers_iso: None,
            idle_pause_s: None,
        }
    }

    fn edited(machine: &Machine, changes: Changes) -> Result<Value> {
        let mut config = json!({"id":"m","cpus":4,"memory_mib":4096,
            "qemu_graphics_profile":"virgl","ports":[{"host_port":1,"guest_port":1}]});
        edit_config(&mut config, machine, &changes).map(|()| config)
    }

    #[test]
    fn update_edits_only_the_given_fields() {
        let linux = machine("qemu-hvf", "linux");
        let c = edited(
            &linux,
            Changes {
                cpus: Some(8),
                memory_mib: Some(8192),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            (c["cpus"].as_u64(), c["memory_mib"].as_u64()),
            (Some(8), Some(8192))
        );
        assert_eq!(c["qemu_graphics_profile"], "virgl");
        assert_eq!(c["ports"][0]["host_port"], 1);
        assert!(c.get("mac_address").is_none());
        for bad in [
            Changes {
                cpus: Some(0),
                ..Default::default()
            },
            Changes {
                memory_mib: Some(255),
                ..Default::default()
            },
        ] {
            assert!(edited(&linux, bad).is_err());
        }
        let c = edited(
            &linux,
            Changes {
                gpus: Some(None),
                publish: Some(vec![]),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(c.get("qemu_graphics_profile").is_none() && c.get("ports").is_none());
        let c = edited(
            &linux,
            Changes {
                gpus: Some(Some("venus".into())),
                publish: Some(vec![(2222, 22), (8080, 80)]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(c["qemu_graphics_profile"], "venus-moltenvk-experimental");
        assert_eq!(
            c["ports"][1],
            json!({"host_address":"127.0.0.1","host_port":8080,"guest_port":80,"protocol":"tcp"})
        );
    }

    #[test]
    fn gpus_follow_the_guest_os_and_backend() {
        let gpus = |m: &Machine, value: &str| {
            edited(
                m,
                Changes {
                    gpus: Some(Some(value.into())),
                    ..Default::default()
                },
            )
            .map(|c| c["qemu_graphics_profile"].as_str().unwrap().to_owned())
        };
        let (linux, windows) = (machine("qemu-hvf", "linux"), machine("qemu-hvf", "windows"));
        assert_eq!(gpus(&linux, "virgl").unwrap(), "virgl");
        assert_eq!(
            gpus(&windows, "neptune").unwrap(),
            "neptune-dxmt-experimental"
        );
        assert!(gpus(&linux, "neptune").is_err() && gpus(&windows, "virgl").is_err());
        let apple = machine("apple-virtualization", "linux");
        assert!(gpus(&apple, "virgl").is_err());
        assert_eq!(
            gpu_name(&json!("venus-moltenvk-experimental")),
            json!("venus")
        );
    }

    #[test]
    fn update_arguments_and_disks_only_grow() {
        let c = Changes::from_args(
            &json!({"name":"u","cpus":8,"memoryMiB":8192,"diskGiB":64,"gpu":null,"ports":[],"newMac":true}),
        )
        .unwrap();
        assert_eq!(
            (c.cpus, c.memory_mib, c.disk_gib),
            (Some(8), Some(8192), Some(64))
        );
        assert!(
            matches!(c.gpus, Some(None)) && c.publish == Some(vec![]) && c.mac_address.is_some()
        );
        assert!(Changes::from_args(&json!({"name":"u"})).is_err());
        assert!(Changes::from_args(&json!({"name":"u","diskGiB":2})).is_err());
        assert!(Changes::from_args(&json!({"name":"u","ports":[{"host":0,"guest":22}]})).is_err());
        assert_eq!(grown_disk(32, Some(64)).unwrap(), Some(64));
        assert_eq!(grown_disk(32, Some(32)).unwrap(), None);
        assert!(grown_disk(32, Some(16)).is_err());
    }

    fn spec(qemu: bool) -> Spec {
        Spec {
            name: "ubuntu".into(),
            qemu,
            windows: false,
            gpu_profile: qemu.then_some("virgl"),
            iso: Some("/isos/ubuntu.iso".into()),
            cpus: 4,
            memory_mib: 4096,
            monitor: qemu,
            attach: vec!["/seed/cidata.iso".into()],
            nvme: false,
            publish: vec![],
            shares: vec![],
            mac_address: None,
        }
    }

    #[test]
    fn machine_configs_are_uefi_with_private_files() {
        let dir = Path::new("/home/machines/ubuntu");
        let q = vm_config(&spec(true), dir);
        assert_eq!(q["backend"], "qemu-hvf");
        assert_eq!(q["boot"], "uefi");
        assert_eq!(q["disk"], "/home/machines/ubuntu/disk.raw");
        assert_eq!(q["efi_vars"], "/home/machines/ubuntu/efi-vars.fd");
        assert_eq!(q["cdrom"], "/isos/ubuntu.iso");
        assert_eq!(q["qemu_monitor"], true);
        assert_eq!(
            q["extra_disks"],
            json!([{"path":"/seed/cidata.iso","read_only":true}])
        );
        let config: crate::Config = serde_json::from_value(q).unwrap();
        assert_eq!(config.boot, crate::BootMode::Uefi);
        let mut apple = spec(false);
        apple.shares = vec![("m0".into(), "/Users/a/src".into(), true)];
        apple.mac_address = Some("02:00:00:00:00:01".into());
        let a = vm_config(&apple, dir);
        assert_eq!(a["backend"], "apple-virtualization");
        assert_eq!(a["width"], 1920);
        assert_eq!(
            a["shares"],
            json!([{"tag":"m0","path":"/Users/a/src","read_only":true}])
        );
        let config: crate::Config = serde_json::from_value(a).unwrap();
        assert_eq!(config.shares.len(), 1);
        assert_eq!(config.mac_address.as_deref(), Some("02:00:00:00:00:01"));
    }

    #[test]
    fn windows_commands_keep_their_arguments() {
        let line = windows_start(
            &["C:/tools/it's.exe".into(), "a b".into(), "c".into()],
            &["K=v'1".into()],
        );
        assert!(line.contains("$i.FileName='C:/tools/it''s.exe'"));
        assert!(line.contains("$i.Arguments='\"a b\" c'"));
        assert!(line.contains("$i.EnvironmentVariables['K']='v''1'"));
        assert!(line.ends_with("exit $p.ExitCode"));
    }

    #[test]
    fn ports_and_states() {
        assert_eq!(
            parse_ports(&json!([{"host":2222,"guest":22}])).unwrap(),
            vec![(2222, 22)]
        );
        assert!(parse_ports(&json!([{"host":2222}])).is_err());
        assert!(parse_ports(&json!("2222:22")).is_err());
        assert_eq!(api_state("crashed"), "failed");
        assert_eq!(api_state("running"), "running");
        assert!(!running("missing") && running("starting"));
        // Older machine.json files have no os field.
        let old: Machine = serde_json::from_value(
            json!({"name":"u","backend":"qemu-hvf","disk_gib":32,"created_at":0}),
        )
        .unwrap();
        assert_eq!(old.os, "linux");
    }
}
