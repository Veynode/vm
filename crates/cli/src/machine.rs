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
  machine ssh-key NAME --user USER --key PRIVATE_KEY
                                Windows: the guest login MCP uses for vm_exec, files and the
                                desktop tools (the key the answer file authorized)
  machine ls | inspect NAME | rm NAME
  desktop NAME                  Native window (Apple Virtualization machines)";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Machine {
    pub name: String,
    pub backend: String,
    #[serde(default = "linux")]
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso: Option<PathBuf>,
    pub disk_gib: u64,
    pub created_at: u64,
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
    Ok(serde_json::from_slice(&bytes)?)
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
    let gpu_profile = match p.value("gpus") {
        None => None,
        Some(_) if windows => None, // below
        Some("all" | "virgl" | "gl") => Some("virgl"),
        Some("venus" | "vulkan") => Some("venus-moltenvk-experimental"),
        Some(other) => bail!("--gpus accepts all|virgl|venus, got {other}"),
    };
    let gpu_profile = match (windows, p.value("gpus")) {
        // UTM 5.0.5 Neptune + DXMT; the guest needs UTM's viogpu3d driver.
        (true, Some("all" | "neptune" | "dxmt" | "d3d")) => Some("neptune-dxmt-experimental"),
        (true, Some(other)) => bail!("--gpus for Windows accepts all|neptune, got {other}"),
        _ => gpu_profile,
    };
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
            created_at: crate::container::now(),
        })?;
        Ok(created)
    })();
    if result.is_err() {
        cleanup_files(&dir);
    }
    result
}

const SSH_FILES: [&str; 3] = ["id_ed25519", "known_hosts", "user"];

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
    let args = rest.get(1..).unwrap_or(&[]);
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
            let mut config = daemon_call("status", json!({"id":name}))?["config"].clone();
            if config["cdrom"].is_null() && config["extra_disks"].is_null() {
                bail!("{name} has no installer ISO or seed image attached")
            }
            let fields = config.as_object_mut().context("invalid VM configuration")?;
            fields.remove("cdrom");
            fields.remove("extra_disks");
            // VM configurations are immutable in the daemon: re-register.
            daemon_call("delete", json!({"id":name,"confirm":name}))?;
            if let Err(e) = daemon_call("create", config) {
                bail!("re-registering {name} without its ISO failed: {e:#}")
            }
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
