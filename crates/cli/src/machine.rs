//! `veynodevm machine`: full Linux distributions installed from their own
//! installer ISO and booted through UEFI with their own kernel and bootloader,
//! and Windows 11 ARM64 installed from the user's own ISO (`--os windows`,
//! QEMU/HVF only: Apple Virtualization has no Windows support).
//!
//! A machine is a daemon VM whose id is the machine name, plus a private
//! directory `<home>/machines/NAME` holding a sparse raw `disk.raw`, the
//! per-VM UEFI variable store `efi-vars.fd` (created by the worker on first
//! start) and `serial.log`. The installer ISO is attached read-only until
//! `machine eject`.
use crate::{args::memory_mib, args::parse, daemon_call, print_json};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use vm_control::home;

pub const USAGE: &str =
    "Full Linux distributions and Windows 11 ARM64 (UEFI boot from their own ISO/disk):
  machine create NAME [--iso PATH] [--disk-size 40G] [-m, --memory 4g] [--cpus 4]
                      [--gpus virgl|venus | --backend apple|qemu] [--monitor]
                      [--attach IMAGE]...   (read-only seed image, e.g. cloud-init/kickstart)
                      [--os linux|windows]  (windows: QEMU/HVF, 64G disk default, --attach an
                                             Autounattend and the virtio-win image; --gpus neptune:
                                             UTM Neptune/DXMT, needs UTM's viogpu3d guest driver)
                      [--disk-bus virtio|nvme]  (QEMU; nvme for disks installed without viostor)
                      [-p, --publish HOST:GUEST]...  (QEMU; TCP 127.0.0.1:HOST → guest, e.g. 2222:22)
  machine start NAME            Boot (installer ISO first while attached, then the disk)
  machine stop [-f] NAME        ACPI power button (-f kills the VM)
  machine eject NAME            Detach the installer ISO and seed images (machine stopped)
  machine update NAME [--cpus N] [-m, --memory SIZE] [--disk-size SIZE] (grow only)
                      [--gpus none|virgl|venus|neptune] [-p, --publish HOST:GUEST|none]...
                      [--new-mac]   Change a stopped machine's hardware (QEMU: GPU, ports)
  machine ssh-key NAME --user USER --key PRIVATE_KEY
                                Windows: the guest login MCP uses for vm_exec, files and the
                                desktop tools (the key the answer file authorized)
  machine display NAME WIDTHxHEIGHT
                                Windows (running, with the ssh login above): set the guest's
                                display mode, e.g. 1920x1080 with --gpus neptune (the driver
                                lists modes up to 2048x1152; the choice persists across boots)
  machine rename NAME NEW_NAME  Rename a stopped machine (its directory and VM id)
  machine icon NAME [KIND[:COLOR]|--reset]
                                The sticker the UI shows (as `veynodevm icon`)
  machine ls | inspect NAME | rm NAME
                                NAME may also be the machine's id (`machine ls --json`)
  desktop NAME                  Native window (Apple Virtualization machines)";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Machine {
    pub name: String,
    /// Stays the same when the machine is renamed (the web UI's addresses);
    /// commands take it in place of NAME. Older records get one when read.
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
}

fn linux() -> String {
    "linux".into()
}

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
}

/// `HOST:GUEST` TCP ports, e.g. `2222:22`.
fn publish_port(value: &str) -> Result<(u16, u16)> {
    let parsed = value
        .split_once(':')
        .and_then(|(host, guest)| Some((host.parse::<u16>().ok()?, guest.parse::<u16>().ok()?)));
    match parsed {
        Some((host, guest)) if host != 0 && guest != 0 => Ok((host, guest)),
        _ => bail!("--publish takes HOST:GUEST TCP ports, got {value}"),
    }
}

/// The `qemu_graphics_profile` for a `--gpus` value: virgl and Venus need a
/// Linux guest, Neptune a Windows one (`vm_control::qemu_graphics`).
fn gpu_profile(value: &str, windows: bool) -> Result<&'static str> {
    Ok(match (windows, value) {
        (false, "all" | "virgl" | "gl") => "virgl",
        (false, "venus" | "vulkan") => "venus-moltenvk-experimental",
        // UTM 5.0.5 Neptune + DXMT; the guest needs UTM's viogpu3d driver.
        (true, "all" | "neptune" | "dxmt" | "d3d") => "neptune-dxmt-experimental",
        (false, "neptune" | "dxmt" | "d3d") => bail!("--gpus {value} needs a Windows guest"),
        (true, "virgl" | "gl" | "venus" | "vulkan") => {
            bail!("--gpus {value} needs a Linux guest; Windows accepts all|neptune")
        }
        (false, other) => bail!("--gpus accepts all|virgl|venus, got {other}"),
        (true, other) => bail!("--gpus for Windows accepts all|neptune, got {other}"),
    })
}

/// `40G`, `40g`, `40GiB`, `40` (GiB), `1T` → GiB.
pub fn disk_gib(value: &str) -> Result<u64> {
    let lower = value.to_ascii_lowercase();
    let trimmed = lower.trim_end_matches("ib").trim_end_matches('b');
    let (digits, factor) = match trimmed.chars().last() {
        Some('t') => (&trimmed[..trimmed.len() - 1], 1024),
        Some('g') => (&trimmed[..trimmed.len() - 1], 1),
        _ => (trimmed, 1),
    };
    let gib = digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(factor))
        .with_context(|| format!("invalid disk size: {value}"))?;
    if !(4..=4096).contains(&gib) {
        bail!("disk size must be 4 GiB through 4 TiB")
    }
    Ok(gib)
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
    if spec.qemu {
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
            vm["ports"] = json!(spec
                .publish
                .iter()
                .map(|(host, guest)| json!({"host_address":"127.0.0.1","host_port":host,"guest_port":guest,"protocol":"tcp"}))
                .collect::<Vec<_>>());
        }
    } else {
        vm["width"] = json!(1920);
        vm["height"] = json!(1080);
    }
    vm
}

fn dir(name: &str) -> Result<PathBuf> {
    vm_control::validate_id(name)?;
    Ok(home::machines_dir()?.join(name))
}

fn load(name: &str) -> Result<Machine> {
    let path = dir(name)?.join("machine.json");
    let bytes = fs::read(&path).with_context(|| format!("no such machine: {name}"))?;
    Ok(with_id(serde_json::from_slice(&bytes)?))
}

/// A record from before machines had ids gets one, saved with it.
fn with_id(mut machine: Machine) -> Machine {
    if machine.id.is_empty() {
        machine.id = crate::random_hex(6);
        let _ = save(&machine);
    }
    machine
}

/// The machine's NAME for ARG, which is its name or its id; ARG itself
/// where neither matches (the command then says there is no such machine).
pub fn resolve(arg: &str) -> String {
    if dir(arg).is_ok_and(|d| d.join("machine.json").is_file()) {
        return arg.to_owned();
    }
    let Ok(entries) = home::machines_dir().and_then(|d| Ok(fs::read_dir(d)?)) else {
        return arg.to_owned();
    };
    entries
        .flatten()
        .filter_map(|e| fs::read(e.path().join("machine.json")).ok())
        .filter_map(|b| serde_json::from_slice::<Machine>(&b).ok())
        .find(|m| !m.id.is_empty() && m.id == arg)
        .map_or_else(|| arg.to_owned(), |m| m.name)
}

/// The web UI terminal of a running machine: an SSH login with the key
/// `machine ssh-key` stored, through its forward to guest port 22, as
/// `vm_exec` reaches Windows. `None` when NAME is no machine.
pub fn terminal(name: &str, argv: &[String]) -> Result<Option<std::process::Command>> {
    let name = &resolve(name);
    if vm_control::validate_id(name).is_err() || load(name).is_err() {
        return Ok(None);
    }
    if !argv.is_empty() {
        bail!("a machine's terminal opens its login shell; it takes no command")
    }
    let vm = daemon_call("status", json!({"id":name}))?;
    if !running(vm["state"].as_str().unwrap_or("unknown")) {
        bail!("{name} is not running")
    }
    let config: vm_control::Config = serde_json::from_value(vm["config"].clone())?;
    let ssh = vm_control::windows_guest::Ssh::locate(&home::state_dir()?, &config)?;
    Ok(Some(ssh.terminal()))
}

fn save(machine: &Machine) -> Result<()> {
    let dir = dir(&machine.name)?;
    let tmp = dir.join("machine.json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(machine)?)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(tmp, dir.join("machine.json"))?;
    Ok(())
}

fn state(name: &str) -> String {
    match daemon_call("status", json!({"id":name})) {
        Ok(v) => v["state"].as_str().unwrap_or("unknown").to_owned(),
        Err(e) if e.to_string().contains("VM not found") => "missing".to_owned(),
        Err(_) => "unknown".to_owned(),
    }
}

fn running(state: &str) -> bool {
    !matches!(state, "stopped" | "crashed" | "missing" | "unknown")
}

fn create(rest: &[String]) -> Result<Value> {
    let p = parse(
        rest,
        &[
            ("iso", None, true),
            ("disk-size", None, true),
            ("memory", Some('m'), true),
            ("cpus", None, true),
            ("gpus", None, true),
            ("backend", None, true),
            ("monitor", None, false),
            ("attach", None, true),
            ("os", None, true),
            ("disk-bus", None, true),
            ("publish", Some('p'), true),
        ],
        false,
    )?;
    let windows = match p.value("os") {
        None | Some("linux") => false,
        Some("windows") => true,
        Some(other) => bail!("--os accepts linux|windows, got {other}"),
    };
    let [name] = p.positional.as_slice() else {
        bail!("usage: veynodevm machine create NAME [--iso PATH] [--disk-size 40G] ...")
    };
    let gpu_profile = p
        .value("gpus")
        .map(|value| gpu_profile(value, windows))
        .transpose()?;
    let qemu = match p.value("backend") {
        None => windows || gpu_profile.is_some(),
        Some("qemu" | "qemu-hvf") => true,
        Some("apple" | "apple-virtualization") if windows => {
            bail!("Apple Virtualization cannot run Windows; use the QEMU/HVF backend")
        }
        Some("apple" | "apple-virtualization") if gpu_profile.is_none() => false,
        Some("apple" | "apple-virtualization") => {
            bail!("--gpus needs the QEMU/HVF backend")
        }
        Some(other) => bail!("--backend accepts apple|qemu, got {other}"),
    };
    if qemu && home::Runtime::load()?.qemu.is_none() {
        bail!("the QEMU backend needs the QEMU/HVF runtime: run runtime/fetch-utm.sh and scripts/install-dev-runtime.py --qemu")
    }
    if p.has("monitor") && !qemu {
        bail!("--monitor is a QEMU option")
    }
    let nvme = match p.value("disk-bus") {
        None | Some("virtio") => false,
        Some("nvme") if qemu => true,
        Some("nvme") => bail!("--disk-bus nvme is a QEMU option"),
        Some(other) => bail!("--disk-bus accepts virtio|nvme, got {other}"),
    };
    let publish = p
        .values("publish")
        .iter()
        .map(|value| publish_port(value))
        .collect::<Result<Vec<_>>>()?;
    if !publish.is_empty() && !qemu {
        bail!("--publish is a QEMU option (Apple Virtualization guests are reachable directly)")
    }
    let iso = p
        .value("iso")
        .map(|iso| fs::canonicalize(iso).with_context(|| format!("installer ISO not found: {iso}")))
        .transpose()?;
    let default_disk = if windows { "64G" } else { "32G" };
    let disk_gib = disk_gib(p.value("disk-size").unwrap_or(default_disk))?;
    let spec = Spec {
        name: name.clone(),
        qemu,
        windows,
        gpu_profile,
        iso: iso.clone(),
        cpus: p
            .value("cpus")
            .map(str::parse)
            .transpose()
            .context("invalid --cpus")?
            .unwrap_or(4),
        memory_mib: p
            .value("memory")
            .map(memory_mib)
            .transpose()?
            .unwrap_or(4096),
        monitor: p.has("monitor"),
        nvme,
        publish,
        attach: p
            .values("attach")
            .iter()
            .map(|path| {
                fs::canonicalize(path).with_context(|| format!("--attach image not found: {path}"))
            })
            .collect::<Result<_>>()?,
    };
    let dir = dir(name)?;
    if dir.exists() {
        bail!("machine {name} already exists")
    }
    home::private_dir(&dir)?;
    let result = (|| -> Result<Value> {
        // Sparse: only blocks the guest writes take host space.
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("disk.raw"))?
            .set_len(disk_gib << 30)?;
        let created = daemon_call("create", vm_config(&spec, &dir))?;
        save(&Machine {
            name: name.clone(),
            backend: if qemu {
                "qemu-hvf"
            } else {
                "apple-virtualization"
            }
            .into(),
            os: if windows { "windows" } else { "linux" }.into(),
            iso,
            disk_gib,
            id: crate::random_hex(6),
            created_at: crate::container::now(),
            icon: None,
        })?;
        Ok(created)
    })();
    if result.is_err() {
        cleanup_files(&dir);
    }
    result
}

const SSH_FILES: [&str; 3] = ["id_ed25519", "known_hosts", "user"];

/// The guest-side script: ChangeDisplaySettings with CDS_UPDATEREGISTRY, which only
/// takes effect on the interactive desktop, so `display` runs it as a one-shot task.
const DISPLAY_SCRIPT: &str = include_str!("windows_display.ps1");

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

/// `machine display`: parses `WIDTHxHEIGHT` for a Windows machine's display mode.
fn display_size(spec: &str) -> Result<(u32, u32)> {
    let (w, h) = spec
        .split_once('x')
        .context("size is WIDTHxHEIGHT, e.g. 1920x1080")?;
    let (w, h): (u32, u32) = (w.parse()?, h.parse()?);
    if !(640..=3840).contains(&w) || !(480..=2160).contains(&h) {
        bail!("size must be between 640x480 and 3840x2160")
    }
    Ok((w, h))
}

/// Runs the display script in the logged-in user's session through the daemon's
/// Windows guest login and returns the guest's own `result=0 now=WxH` line.
fn set_display(name: &str, spec: &str) -> Result<String> {
    let machine = load(name)?;
    if machine.os != "windows" {
        bail!(
            "display is for Windows machines; {name} runs {}",
            machine.os
        )
    }
    let (w, h) = display_size(spec)?;
    let script = format!(
        "$ErrorActionPreference='Stop'; $f=Join-Path $env:TEMP 'veynode-display.ps1'; $o=Join-Path $env:TEMP 'veynode-display.out'; \
         [IO.File]::WriteAllBytes($f,[Convert]::FromBase64String('{}')); Remove-Item $o -ErrorAction SilentlyContinue; \
         schtasks /create /tn VeynodeDisplay /tr ('cmd /c powershell -NoProfile -ExecutionPolicy Bypass -File ' + $f + ' -W {w} -H {h} > ' + $o + ' 2>&1') /sc once /st 00:00 /it /ru $env:USERNAME /f | Out-Null; \
         schtasks /run /tn VeynodeDisplay | Out-Null; \
         for($i=0;$i -lt 80 -and -not ((Test-Path $o) -and (Get-Item $o).Length -gt 0);$i++){{Start-Sleep -Milliseconds 250}}; Get-Content $o; \
         schtasks /delete /tn VeynodeDisplay /f | Out-Null",
        base64(DISPLAY_SCRIPT.as_bytes())
    );
    let reply = daemon_call(
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
    Ok(out)
}

/// `machine ssh-key`: copies the private key and records the user in
/// `<machine>/ssh/`, where the daemon's Windows guest tools find them
/// (`vm_control::windows_guest`).
fn ssh_key(name: &str, args: &[String]) -> Result<PathBuf> {
    let machine = load(name)?;
    if machine.os != "windows" {
        bail!(
            "ssh-key is for Windows machines; {name} runs {}",
            machine.os
        )
    }
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .with_context(|| format!("{flag} required"))
    };
    let user = value("--user")?;
    if user.is_empty()
        || !user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        bail!("--user takes a Windows account name (letters, digits, . _ -)")
    }
    let key = fs::read(value("--key")?).context("cannot read --key")?;
    if !key.starts_with(b"-----BEGIN OPENSSH PRIVATE KEY-----") {
        bail!("--key must be an OpenSSH private key")
    }
    let ssh = home::private_dir(&dir(name)?.join("ssh"))?;
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
    Ok(ssh)
}

/// Remove only the files a machine owns, then its directory (never recursive).
fn cleanup_files(dir: &Path) {
    for file in [
        "disk.raw",
        "efi-vars.fd",
        "serial.log",
        "machine.json",
        "machine.json.tmp",
    ] {
        let _ = fs::remove_file(dir.join(file));
    }
    for file in SSH_FILES {
        let _ = fs::remove_file(dir.join("ssh").join(file));
    }
    let _ = fs::remove_dir(dir.join("ssh"));
    if dir.exists() && fs::remove_dir(dir).is_err() {
        eprintln!(
            "veynodevm: left {} in place: it contains files VeynodeVM did not create",
            dir.display()
        );
    }
}

/// `machine rename`: the name is the daemon VM id and the directory, so a
/// stopped machine moves to the new directory and is registered again under
/// the new id with its paths moved (VM configurations are immutable in the
/// daemon); the old registration goes last, so a failure leaves the old
/// machine as it was.
fn rename(old: &str, new: &str) -> Result<()> {
    let mut machine = load(old)?;
    if old == new {
        return Ok(());
    }
    let (from, to) = (dir(old)?, dir(new)?);
    let state = state(old);
    if state == "unknown" {
        bail!("cannot read {old}'s state from the daemon")
    }
    if running(&state) {
        bail!("stop {old} before renaming it")
    }
    if to.exists() || self::state(new) != "missing" {
        bail!("machine name already in use: {new}")
    }
    let config = (state != "missing")
        .then(|| daemon_call("status", json!({"id":old})).map(|v| v["config"].clone()))
        .transpose()?;
    fs::rename(&from, &to)?;
    if let Some(mut config) = config {
        config["id"] = json!(new);
        moved_paths(&mut config, &from, &to);
        if let Err(e) = daemon_call("create", config) {
            let _ = fs::rename(&to, &from);
            bail!("registering {new} failed: {e:#}")
        }
        daemon_call("delete", json!({"id":old,"confirm":old}))?;
    }
    machine.name = new.to_owned();
    save(&machine)
}

/// Every string in a VM configuration that names a file under `from`, moved under `to`.
fn moved_paths(value: &mut Value, from: &Path, to: &Path) {
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

/// VM configurations are immutable in the daemon: a changed one is deleted
/// and created again. If the new one is refused, the old one is created
/// again, so the machine stays registered.
fn reregister(name: &str, old: Value, new: Value) -> Result<()> {
    daemon_call("delete", json!({"id":name,"confirm":name}))?;
    if let Err(e) = daemon_call("create", new) {
        if let Err(restore) = daemon_call("create", old) {
            bail!("re-registering {name} failed: {e:#}; restoring its old configuration also failed: {restore:#}")
        }
        bail!("re-registering {name} failed (its old configuration is kept): {e:#}")
    }
    Ok(())
}

/// What `machine update` was asked to change; `None` leaves a field alone.
#[derive(Default)]
struct Changes {
    cpus: Option<u32>,
    memory_mib: Option<u64>,
    disk_gib: Option<u64>,
    /// `Some(None)` removes the GPU.
    gpus: Option<Option<String>>,
    /// `Some(empty)` clears the forwards.
    publish: Option<Vec<(u16, u16)>>,
    mac_address: Option<String>,
}

impl Changes {
    fn parse(args: &[String]) -> Result<(String, Changes)> {
        let p = parse(
            args,
            &[
                ("cpus", None, true),
                ("memory", Some('m'), true),
                ("disk-size", None, true),
                ("gpus", None, true),
                ("publish", Some('p'), true),
                ("new-mac", None, false),
            ],
            false,
        )?;
        let [name] = p.positional.as_slice() else {
            bail!("usage: veynodevm machine update NAME [--cpus N] [-m SIZE] [--disk-size SIZE] [--gpus ...] [-p HOST:GUEST|none]... [--new-mac]")
        };
        let publish = match p.values("publish") {
            [] => None,
            [none] if none == "none" => Some(Vec::new()),
            values => Some(
                values
                    .iter()
                    .map(|value| publish_port(value))
                    .collect::<Result<Vec<_>>>()?,
            ),
        };
        let changes = Changes {
            cpus: p
                .value("cpus")
                .map(str::parse)
                .transpose()
                .context("invalid --cpus")?,
            memory_mib: p.value("memory").map(memory_mib).transpose()?,
            disk_gib: p.value("disk-size").map(disk_gib).transpose()?,
            gpus: p
                .value("gpus")
                .map(|value| (value != "none").then(|| value.to_owned())),
            publish,
            mac_address: p.has("new-mac").then(crate::macos::random_mac),
        };
        if changes.cpus.is_none()
            && changes.memory_mib.is_none()
            && changes.disk_gib.is_none()
            && changes.gpus.is_none()
            && changes.publish.is_none()
            && changes.mac_address.is_none()
        {
            bail!("machine update needs at least one of --cpus, --memory, --disk-size, --gpus, --publish, --new-mac")
        }
        Ok((name.clone(), changes))
    }
}

/// Applies `changes` to a machine's daemon VM configuration (not the disk
/// size, which is the disk file's: see `grown_disk`).
fn edit_config(config: &mut Value, machine: &Machine, changes: &Changes) -> Result<()> {
    let qemu = machine.backend == "qemu-hvf";
    let fields = config.as_object_mut().context("invalid VM configuration")?;
    if let Some(cpus) = changes.cpus {
        if !(1..=128).contains(&cpus) {
            bail!("--cpus must be 1 through 128")
        }
        fields.insert("cpus".into(), json!(cpus));
    }
    if let Some(memory) = changes.memory_mib {
        if !(256..=1 << 20).contains(&memory) {
            bail!("--memory must be 256 MiB through 1 TiB")
        }
        fields.insert("memory_mib".into(), json!(memory));
    }
    match &changes.gpus {
        None => {}
        Some(None) => {
            fields.remove("qemu_graphics_profile");
        }
        Some(Some(_)) if !qemu => bail!("--gpus needs the QEMU/HVF backend"),
        Some(Some(value)) => {
            let profile = gpu_profile(value, machine.os == "windows")?;
            fields.insert("qemu_graphics_profile".into(), json!(profile));
        }
    }
    match &changes.publish {
        None => {}
        Some(ports) if ports.is_empty() => {
            fields.remove("ports");
        }
        Some(_) if !qemu => {
            bail!("--publish is a QEMU option (Apple Virtualization guests are reachable directly)")
        }
        Some(ports) => {
            fields.insert("ports".into(), json!(ports
                .iter()
                .map(|(host, guest)| json!({"host_address":"127.0.0.1","host_port":host,"guest_port":guest,"protocol":"tcp"}))
                .collect::<Vec<_>>()));
        }
    }
    if let Some(mac) = &changes.mac_address {
        fields.insert("mac_address".into(), json!(mac));
    }
    Ok(())
}

/// The new disk size when `requested` grows it, `None` when it is the same;
/// disks never shrink (the guest's filesystem would lose its end).
fn grown_disk(current: u64, requested: Option<u64>) -> Result<Option<u64>> {
    match requested {
        Some(gib) if gib < current => {
            bail!("disks can only grow: {gib} GiB is less than the current {current} GiB")
        }
        Some(gib) if gib > current => Ok(Some(gib)),
        _ => Ok(None),
    }
}

/// `machine update`: edits a stopped machine's hardware.
fn update(name: &str, changes: &Changes) -> Result<()> {
    let mut machine = load(name)?;
    let state = state(name);
    match state.as_str() {
        "unknown" => bail!("cannot read {name}'s state from the daemon"),
        "missing" => bail!("{name} is not registered with the daemon"),
        state if running(state) => bail!("stop {name} before changing its hardware"),
        _ => {}
    }
    let old = daemon_call("status", json!({"id":name}))?["config"].clone();
    let mut config = old.clone();
    edit_config(&mut config, &machine, changes)?;
    let grow = grown_disk(machine.disk_gib, changes.disk_gib)?;
    if config != old {
        reregister(name, old, config)?;
    }
    if let Some(gib) = grow {
        // Sparse: the new end takes no host space until the guest writes it.
        OpenOptions::new()
            .write(true)
            .open(dir(name)?.join("disk.raw"))?
            .set_len(gib << 30)?;
        machine.disk_gib = gib;
        save(&machine)?;
    }
    Ok(())
}

fn done(name: &str, json_output: bool) -> Result<()> {
    if json_output {
        print_json(&json!({"name":name}))
    } else {
        println!("{name}");
        Ok(())
    }
}

fn wait_stopped(name: &str, timeout: Duration) -> bool {
    let until = Instant::now() + timeout;
    while Instant::now() < until {
        if !running(&state(name)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

pub fn command(rest: &[String], json_output: bool) -> Result<i32> {
    let action = rest.first().map(String::as_str).unwrap_or("ls");
    let given = rest.get(1..).unwrap_or(&[]);
    // NAME may be given as the machine's id (not to create, where it is new).
    let mut resolved = given.to_vec();
    if action != "create" {
        if let Some(first) = resolved.iter_mut().find(|a| !a.starts_with('-')) {
            *first = resolve(first);
        }
    }
    let args = resolved.as_slice();
    let name = || -> Result<&str> {
        args.iter()
            .map(String::as_str)
            .find(|a| !a.starts_with('-'))
            .context("machine NAME required")
    };
    match action {
        "create" => {
            let value = create(args)?;
            if json_output {
                print_json(&value)?;
            } else {
                println!("{}", value["config"]["id"].as_str().unwrap_or_default());
            }
        }
        "start" => {
            let name = name()?;
            load(name)?;
            let value = daemon_call("start", json!({"id":name}))?;
            if json_output {
                print_json(&value)?;
            } else {
                done(name, json_output)?;
            }
        }
        "stop" => {
            let name = name()?;
            if args.iter().any(|a| a == "-f" || a == "--force") {
                daemon_call("force_stop", json!({"id":name,"confirm":name}))?;
            } else {
                daemon_call("shutdown", json!({"id":name}))?;
            }
            done(name, json_output)?;
        }
        "eject" => {
            let name = name()?;
            let mut machine = load(name)?;
            if running(&state(name)) {
                bail!("stop {name} before ejecting the installer ISO")
            }
            let old = daemon_call("status", json!({"id":name}))?["config"].clone();
            let mut config = old.clone();
            if config["cdrom"].is_null() && config["extra_disks"].is_null() {
                bail!("{name} has no installer ISO or seed image attached")
            }
            let fields = config.as_object_mut().context("invalid VM configuration")?;
            fields.remove("cdrom");
            fields.remove("extra_disks");
            reregister(name, old, config)?;
            machine.iso = None;
            save(&machine)?;
            done(name, json_output)?;
        }
        "rm" => {
            let name = name()?;
            let dir = dir(name)?;
            load(name)?;
            let state = state(name);
            if running(&state) {
                if !args.iter().any(|a| a == "-f" || a == "--force") {
                    bail!("{name} is running; stop it or use rm -f")
                }
                daemon_call("force_stop", json!({"id":name,"confirm":name}))?;
                wait_stopped(name, Duration::from_secs(20));
            }
            if state != "missing" {
                daemon_call("delete", json!({"id":name,"confirm":name}))?;
            }
            cleanup_files(&dir);
            done(name, json_output)?;
        }
        "update" => {
            let (name, changes) = Changes::parse(args)?;
            update(&name, &changes)?;
            done(&name, json_output)?;
        }
        "rename" => {
            let [old, new] = args else {
                bail!("machine rename requires NAME NEW_NAME")
            };
            rename(old, new)?;
            done(new, json_output)?;
        }
        "icon" => {
            let p = parse(args, &[("reset", None, false)], false)?;
            let name = p
                .positional
                .first()
                .context("machine icon requires a NAME")?;
            let mut machine = load(name)?;
            match (p.positional.get(1), p.has("reset")) {
                (Some(_), true) => bail!("give an icon or --reset, not both"),
                (Some(icon), false) => machine.icon = Some(crate::container::parse_icon(icon)?),
                (None, true) => machine.icon = None,
                (None, false) => {
                    println!("{}", machine.icon.as_deref().unwrap_or(""));
                    return Ok(0);
                }
            }
            save(&machine)?;
            if json_output {
                print_json(&json!({"name":machine.name,"icon":machine.icon}))?;
            }
        }
        "display" => {
            let name = name()?;
            let spec = args.get(1).context("size required: WIDTHxHEIGHT")?;
            let line = set_display(name, spec)?;
            if json_output {
                print_json(&json!({"name":name,"display":line}))?;
            } else {
                println!("{line}");
            }
        }
        "ssh-key" => {
            let name = name()?;
            let ssh = ssh_key(name, args)?;
            if json_output {
                print_json(&json!({"name":name,"ssh":ssh}))?;
            } else {
                println!("{}", ssh.display());
            }
        }
        "inspect" => {
            let name = name()?;
            let machine = load(name)?;
            let vm = daemon_call("status", json!({"id":name})).unwrap_or(Value::Null);
            print_json(&json!({"machine":machine,"vm":vm}))?;
        }
        "ls" | "list" => {
            let mut rows = Vec::new();
            for entry in fs::read_dir(home::machines_dir()?)? {
                let path = entry?.path().join("machine.json");
                if let Ok(m) = fs::read(&path)
                    .map_err(anyhow::Error::from)
                    .and_then(|b| Ok(serde_json::from_slice::<Machine>(&b)?))
                {
                    let m = with_id(m);
                    let state = state(&m.name);
                    let used = fs::metadata(path.with_file_name("disk.raw"))
                        .map(|m| std::os::unix::fs::MetadataExt::blocks(&m) * 512)
                        .unwrap_or(0);
                    rows.push((m, state, used));
                }
            }
            rows.sort_by(|a, b| a.0.name.cmp(&b.0.name));
            if json_output {
                print_json(&Value::Array(
                    rows.iter()
                        .map(|(m, s, used)| json!({"machine":m,"state":s,"disk_used_bytes":used}))
                        .collect(),
                ))?;
            } else {
                println!(
                    "{:<20}{:<22}{:<12}{:<18}ISO",
                    "NAME", "BACKEND", "STATE", "DISK USED/SIZE"
                );
                for (m, s, used) in rows {
                    println!(
                        "{:<20}{:<22}{:<12}{:<18}{}",
                        m.name,
                        m.backend,
                        s,
                        format!("{:.1}/{}G", used as f64 / (1u64 << 30) as f64, m.disk_gib),
                        m.iso
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "-".into())
                    );
                }
            }
        }
        "-h" | "--help" | "help" => println!("{USAGE}"),
        other => bail!("unknown machine command: {other}\n{USAGE}"),
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_script_encoding_and_sizes() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(display_size("1920x1080").unwrap(), (1920, 1080));
        for bad in ["1920", "axb", "100x100", "1920x4000", "1920x"] {
            assert!(display_size(bad).is_err(), "{bad}");
        }
    }
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
                cpus: Some(129),
                ..Default::default()
            },
            Changes {
                memory_mib: Some(255),
                ..Default::default()
            },
            Changes {
                memory_mib: Some((1 << 20) + 1),
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
        assert_eq!(c["ports"].as_array().unwrap().len(), 2);
        assert_eq!(
            c["ports"][1],
            json!({"host_address":"127.0.0.1","host_port":8080,"guest_port":80,"protocol":"tcp"})
        );

        let mac = crate::macos::random_mac();
        let c = edited(
            &linux,
            Changes {
                mac_address: Some(mac.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(c["mac_address"], json!(mac));
        let first = u8::from_str_radix(&mac[..2], 16).unwrap();
        assert_eq!(first & 0x03, 0x02, "locally administered unicast: {mac}");
    }

    #[test]
    fn update_gpus_follow_the_guest_os_and_backend() {
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
        assert!(gpus(&linux, "neptune").is_err());
        assert!(gpus(&windows, "virgl").is_err() && gpus(&windows, "venus").is_err());
        assert!(gpus(&linux, "metal").is_err());

        let apple = machine("apple-virtualization", "linux");
        assert!(gpus(&apple, "virgl").is_err());
        assert!(edited(
            &apple,
            Changes {
                publish: Some(vec![(2222, 22)]),
                ..Default::default()
            }
        )
        .is_err());
        assert!(edited(
            &apple,
            Changes {
                gpus: Some(None),
                publish: Some(vec![]),
                ..Default::default()
            }
        )
        .is_ok());
    }

    #[test]
    fn update_parses_flags_and_disks_only_grow() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (name, c) = Changes::parse(&args(&[
            "u",
            "--cpus",
            "8",
            "-m",
            "8g",
            "--disk-size",
            "64G",
            "--gpus",
            "none",
            "-p",
            "none",
            "--new-mac",
        ]))
        .unwrap();
        assert_eq!(
            (name.as_str(), c.cpus, c.memory_mib, c.disk_gib),
            ("u", Some(8), Some(8192), Some(64))
        );
        assert!(
            matches!(c.gpus, Some(None)) && c.publish == Some(vec![]) && c.mac_address.is_some()
        );
        let (_, c) = Changes::parse(&args(&["u", "-p", "2222:22", "-p", "80:80"])).unwrap();
        assert_eq!(c.publish, Some(vec![(2222, 22), (80, 80)]));
        assert!(Changes::parse(&args(&["u"])).is_err());
        assert!(Changes::parse(&args(&["u", "-p", "none", "-p", "80:80"])).is_err());
        assert!(Changes::parse(&args(&["u", "--disk-size", "2G"])).is_err());

        assert_eq!(grown_disk(32, Some(64)).unwrap(), Some(64));
        assert_eq!(grown_disk(32, Some(32)).unwrap(), None);
        assert_eq!(grown_disk(32, None).unwrap(), None);
        assert!(grown_disk(32, Some(16))
            .unwrap_err()
            .to_string()
            .contains("disks can only grow"));
    }

    #[test]
    fn disk_sizes() {
        assert_eq!(disk_gib("40G").unwrap(), 40);
        assert_eq!(disk_gib("40gib").unwrap(), 40);
        assert_eq!(disk_gib("32").unwrap(), 32);
        assert_eq!(disk_gib("1T").unwrap(), 1024);
        assert!(disk_gib("2G").is_err());
        assert!(disk_gib("x").is_err());
        assert!(disk_gib("99999999999999999999T").is_err());
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
        assert!(q.get("kernel").is_none() && q.get("width").is_none());
        let config: vm_control::Config = serde_json::from_value(q).unwrap();
        assert_eq!(config.boot, vm_control::BootMode::Uefi);
        let a = vm_config(&spec(false), dir);
        assert_eq!(a["backend"], "apple-virtualization");
        assert!(a.get("qemu_monitor").is_none() && a.get("qemu_graphics_profile").is_none());
        assert_eq!(a["width"], 1920);
        let mut installed = spec(false);
        installed.iso = None;
        installed.attach.clear();
        let installed = vm_config(&installed, dir);
        assert!(installed.get("cdrom").is_none() && installed.get("extra_disks").is_none());
    }
    #[test]
    fn windows_machines_are_qemu_uefi_windows_guests() {
        let mut windows = spec(true);
        windows.windows = true;
        windows.gpu_profile = None;
        let w = vm_config(&windows, Path::new("/home/machines/win"));
        assert_eq!(w["guest"], "windows");
        assert_eq!(w["backend"], "qemu-hvf");
        assert!(w.get("qemu_graphics_profile").is_none() && w.get("qemu_disk_bus").is_none());
        windows.nvme = true;
        let nvme = vm_config(&windows, Path::new("/home/machines/win"));
        assert_eq!(nvme["qemu_disk_bus"], "nvme");
        let w = vm_config(
            &{
                windows.nvme = false;
                windows.publish = vec![(2222, 22)];
                windows
            },
            Path::new("/home/machines/win"),
        );
        let config: vm_control::Config = serde_json::from_value(w).unwrap();
        assert_eq!(config.guest, "windows");
        assert_eq!(
            (
                config.ports[0].host_address,
                config.ports[0].host_port,
                config.ports[0].guest_port
            ),
            (std::net::Ipv4Addr::LOCALHOST, 2222, 22)
        );
        assert_eq!(publish_port("2222:22").unwrap(), (2222, 22));
        for bad in ["22", "0:22", "x:22", "2222:70000"] {
            assert!(publish_port(bad).is_err(), "{bad}");
        }
        // Older machine.json files have no os field.
        let old: Machine = serde_json::from_value(json!({
            "name":"u","backend":"qemu-hvf","disk_gib":32,"created_at":0
        }))
        .unwrap();
        assert_eq!(old.os, "linux");
    }
}
