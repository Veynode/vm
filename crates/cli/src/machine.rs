//! `veynodevm machine`: full Linux distributions installed from their own
//! installer ISO or a disk image and booted through UEFI, and Windows 11 ARM64
//! installed from the user's own ISO (`--os windows`, QEMU/HVF only). The
//! daemon owns machines (`machine.*` in docs/api.md); this module turns flags
//! into those calls and prints their answers.
use crate::{
    api_call, api_stream,
    args::{idle_seconds, memory_mib, parse},
    print_json,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::fs;

pub const USAGE: &str =
    "Full Linux distributions and Windows 11 ARM64 (UEFI boot from their own ISO/disk):
  machine create NAME [--iso PATH | --disk IMAGE] [--disk-size 40G] [-m, --memory 4g] [--cpus 4]
                      [--gpus virgl|venus | --backend apple|qemu] [--monitor]
                      [--attach IMAGE]...   (read-only seed image, e.g. cloud-init/kickstart)
                      [--os linux|windows]  (windows: QEMU/HVF, 64G disk default, --attach an
                                             Autounattend and the virtio-win image; --gpus neptune:
                                             UTM Neptune/DXMT, needs UTM's viogpu3d guest driver)
                      [--disk-bus virtio|nvme]  (QEMU; nvme for disks installed without viostor)
                      [-p, --publish HOST:GUEST]...  (QEMU; TCP 127.0.0.1:HOST → guest, e.g. 2222:22)
                      [-v, --volume HOST_DIR:TAG[:ro]]... (virtiofs share)
  machine start NAME            Boot (installer ISO first while attached, then the disk)
  machine stop [-f] NAME        ACPI power button (-f kills the VM)
  machine pause|unpause NAME    Stop or resume the VM's vCPUs (its memory stays in use)
  machine eject NAME            Detach the installer ISO and seed images (machine stopped)
  machine update NAME [--cpus N] [-m, --memory SIZE] [--disk-size SIZE] (grow only)
                      [--gpus none|virgl|venus|neptune] [-p, --publish HOST:GUEST|none]...
                      [--new-mac]   Change a stopped machine's hardware (QEMU: GPU, ports)
                      [--idle-pause DURATION|off]
                                Pause the VM after 5m, 1h, ... without activity;
                                applies at once, running or not
  machine clone NAME NEW_NAME   APFS copy of a stopped machine, with a new MAC address
  machine exec NAME CMD...      Run a command in the guest (-i passes stdin)
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
  machine ls | inspect NAME | endpoints NAME | rm NAME
                                NAME may also be the machine's id (`machine ls --json`)
  desktop NAME                  Native window (Apple Virtualization machines)";

/// `HOST:GUEST` TCP ports, e.g. `2222:22`.
fn publish_port(value: &str) -> Result<Value> {
    let parsed = value
        .split_once(':')
        .and_then(|(host, guest)| Some((host.parse::<u16>().ok()?, guest.parse::<u16>().ok()?)));
    match parsed {
        Some((host, guest)) if host != 0 && guest != 0 => Ok(json!({"host":host,"guest":guest})),
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

/// An absolute path for the daemon, which does not share this directory.
fn absolute(path: &str) -> Result<String> {
    Ok(fs::canonicalize(path)
        .with_context(|| format!("not found: {path}"))?
        .display()
        .to_string())
}

fn create_args(rest: &[String]) -> Result<Value> {
    let p = parse(
        rest,
        &[
            ("iso", None, true),
            ("disk", None, true),
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
            ("volume", Some('v'), true),
        ],
        false,
    )?;
    let [name] = p.positional.as_slice() else {
        bail!("usage: veynodevm machine create NAME [--iso PATH] [--disk-size 40G] ...")
    };
    let mut args = Map::new();
    args.insert("name".into(), json!(name));
    args.insert("os".into(), json!(p.value("os").unwrap_or("linux")));
    for (flag, field) in [
        ("gpus", "gpu"),
        ("backend", "backend"),
        ("disk-bus", "diskBus"),
    ] {
        if let Some(value) = p.value(flag) {
            args.insert(field.into(), json!(value));
        }
    }
    for (flag, field) in [("iso", "iso"), ("disk", "disk")] {
        if let Some(path) = p.value(flag) {
            args.insert(field.into(), json!(absolute(path)?));
        }
    }
    if let Some(size) = p.value("disk-size") {
        args.insert("diskGiB".into(), json!(disk_gib(size)?));
    }
    if let Some(cpus) = p.value("cpus") {
        args.insert(
            "cpus".into(),
            json!(cpus.parse::<u32>().context("invalid --cpus")?),
        );
    }
    if let Some(memory) = p.value("memory") {
        args.insert("memoryMiB".into(), json!(memory_mib(memory)?));
    }
    if p.has("monitor") {
        args.insert("monitor".into(), json!(true));
    }
    if !p.values("attach").is_empty() {
        let attach = p
            .values("attach")
            .iter()
            .map(|path| absolute(path))
            .collect::<Result<Vec<_>>>()?;
        args.insert("attach".into(), json!(attach));
    }
    if !p.values("publish").is_empty() {
        let ports = p
            .values("publish")
            .iter()
            .map(|value| publish_port(value))
            .collect::<Result<Vec<_>>>()?;
        args.insert("ports".into(), json!(ports));
    }
    if !p.values("volume").is_empty() {
        let shares = p
            .values("volume")
            .iter()
            .map(|value| {
                let mut parts = value.split(':');
                let (Some(host), Some(tag)) = (parts.next(), parts.next()) else {
                    bail!("--volume takes HOST_DIR:TAG[:ro], got {value}")
                };
                let read_only = match parts.next() {
                    None => false,
                    Some("ro") => true,
                    Some(other) => bail!("--volume mode is ro, got {other}"),
                };
                Ok(json!({"hostPath":absolute(host)?,"tag":tag,"readOnly":read_only}))
            })
            .collect::<Result<Vec<_>>>()?;
        args.insert("shares".into(), json!(shares));
    }
    Ok(Value::Object(args))
}

fn update_args(rest: &[String]) -> Result<Value> {
    let p = parse(
        rest,
        &[
            ("cpus", None, true),
            ("memory", Some('m'), true),
            ("disk-size", None, true),
            ("gpus", None, true),
            ("publish", Some('p'), true),
            ("new-mac", None, false),
            ("audio", None, true),
            ("idle-pause", None, true),
        ],
        false,
    )?;
    let [name] = p.positional.as_slice() else {
        bail!("usage: veynodevm machine update NAME [--cpus N] [-m SIZE] [--disk-size SIZE] [--gpus ...] [-p HOST:GUEST|none]... [--audio on|off] [--new-mac] [--idle-pause DURATION|off]")
    };
    let mut args = Map::new();
    args.insert("name".into(), json!(name));
    if let Some(cpus) = p.value("cpus") {
        args.insert(
            "cpus".into(),
            json!(cpus.parse::<u32>().context("invalid --cpus")?),
        );
    }
    if let Some(memory) = p.value("memory") {
        args.insert("memoryMiB".into(), json!(memory_mib(memory)?));
    }
    if let Some(size) = p.value("disk-size") {
        args.insert("diskGiB".into(), json!(disk_gib(size)?));
    }
    if let Some(gpus) = p.value("gpus") {
        args.insert(
            "gpu".into(),
            if gpus == "none" {
                Value::Null
            } else {
                json!(gpus)
            },
        );
    }
    match p.values("publish") {
        [] => {}
        [none] if none == "none" => {
            args.insert("ports".into(), json!([]));
        }
        values => {
            let ports = values
                .iter()
                .map(|value| publish_port(value))
                .collect::<Result<Vec<_>>>()?;
            args.insert("ports".into(), json!(ports));
        }
    }
    if p.has("new-mac") {
        args.insert("newMac".into(), json!(true));
    }
    match p.value("audio") {
        None => {}
        Some("on") => {
            args.insert("audio".into(), json!(true));
        }
        Some("off") => {
            args.insert("audio".into(), json!(false));
        }
        Some(other) => bail!("--audio takes on or off, got {other}"),
    }
    if let Some(value) = p.value("idle-pause") {
        args.insert("idlePauseS".into(), json!(idle_seconds(value)?));
    }
    if args.len() == 1 {
        bail!("machine update needs at least one of --cpus, --memory, --disk-size, --gpus, --publish, --audio, --new-mac, --idle-pause")
    }
    Ok(Value::Object(args))
}

fn done(name: &str, json_output: bool) -> Result<()> {
    if json_output {
        print_json(&json!({"name":name}))
    } else {
        println!("{name}");
        Ok(())
    }
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
    let force = args.iter().any(|a| a == "-f" || a == "--force");
    match action {
        "create" => {
            let machine = api_stream("machine.create", create_args(args)?, json_output)?;
            if json_output {
                print_json(&machine)?;
            } else {
                println!("{}", machine["name"].as_str().unwrap_or_default());
            }
        }
        "start" => {
            let name = name()?;
            let machine = api_stream("machine.start", json!({"name":name}), json_output)?;
            if json_output {
                print_json(&machine)?;
            } else {
                done(machine["name"].as_str().unwrap_or(name), false)?;
            }
        }
        "stop" => {
            let reply = api_call("machine.stop", json!({"name":name()?,"force":force}))?;
            done(reply["name"].as_str().unwrap_or_default(), json_output)?;
        }
        "pause" | "unpause" => {
            let reply = api_call(&format!("machine.{action}"), json!({"name":name()?}))?;
            done(reply["name"].as_str().unwrap_or_default(), json_output)?;
        }
        "eject" => {
            let reply = api_call("machine.eject", json!({"name":name()?}))?;
            done(reply["name"].as_str().unwrap_or_default(), json_output)?;
        }
        "rm" => {
            let reply = api_call("machine.rm", json!({"name":name()?,"force":force}))?;
            done(reply["name"].as_str().unwrap_or_default(), json_output)?;
        }
        "update" => {
            let reply = api_call("machine.update", update_args(args)?)?;
            done(reply["name"].as_str().unwrap_or_default(), json_output)?;
        }
        "rename" | "clone" => {
            let [old, new] = args else {
                bail!("machine {action} requires NAME NEW_NAME")
            };
            let machine = api_call(
                &format!("machine.{action}"),
                json!({"name":old,"newName":new}),
            )?;
            done(machine["name"].as_str().unwrap_or(new), json_output)?;
        }
        "icon" => {
            let p = parse(args, &[("reset", None, false)], false)?;
            let name = p
                .positional
                .first()
                .context("machine icon requires a NAME")?;
            let icon = match (p.positional.get(1), p.has("reset")) {
                (Some(_), true) => bail!("give an icon or --reset, not both"),
                (Some(icon), false) => json!(icon),
                (None, true) => Value::Null,
                (None, false) => {
                    let machine = api_call("machine.inspect", json!({"name":name}))?;
                    println!("{}", machine["icon"].as_str().unwrap_or(""));
                    return Ok(0);
                }
            };
            let reply = api_call("machine.icon", json!({"name":name,"icon":icon}))?;
            if json_output {
                print_json(&reply)?;
            }
        }
        "display" => {
            let name = name()?;
            let spec = args.get(1).context("size required: WIDTHxHEIGHT")?;
            let (w, h) = spec
                .split_once('x')
                .context("size is WIDTHxHEIGHT, e.g. 1920x1080")?;
            let (w, h): (u32, u32) = (w.parse()?, h.parse()?);
            let reply = api_call(
                "machine.resolution",
                json!({"name":name,"width":w,"height":h}),
            )?;
            if json_output {
                print_json(&json!({"name":reply["name"],"display":reply["display"]}))?;
            } else {
                println!("{}", reply["display"].as_str().unwrap_or_default());
            }
        }
        "ssh-key" => {
            let name = name()?;
            let value = |flag: &str| {
                args.iter()
                    .position(|a| a == flag)
                    .and_then(|i| args.get(i + 1))
                    .with_context(|| format!("{flag} required"))
            };
            let key = absolute(value("--key")?).context("cannot read --key")?;
            let reply = api_call(
                "machine.ssh_key",
                json!({"name":name,"user":value("--user")?,"key":key}),
            )?;
            if json_output {
                print_json(&reply)?;
            } else {
                println!("{}", reply["ssh"].as_str().unwrap_or_default());
            }
        }
        "endpoints" => print_json(&api_call("machine.endpoints", json!({"name":name()?}))?)?,
        "exec" => {
            let p = parse(
                args,
                &[
                    ("interactive", Some('i'), false),
                    ("env", Some('e'), true),
                    ("user", Some('u'), true),
                    ("workdir", Some('w'), true),
                ],
                true,
            )?;
            let [name, argv @ ..] = p.positional.as_slice() else {
                bail!("usage: veynodevm machine exec [-i] NAME CMD...")
            };
            if argv.is_empty() {
                bail!("machine exec needs a command")
            }
            return exec(name, argv, &p);
        }
        "inspect" => {
            let machine = api_call("machine.inspect", json!({"name":name()?}))?;
            print_json(&json!({"machine":machine["record"],"vm":machine["vm"]}))?;
        }
        "ls" | "list" => {
            let machines = api_call("machine.ls", json!({}))?;
            let rows = machines.as_array().cloned().unwrap_or_default();
            if json_output {
                print_json(&Value::Array(
                    rows.iter()
                        .map(|m| json!({"machine":m["record"],"state":m["vmState"],"disk_used_bytes":m["diskUsedBytes"]}))
                        .collect(),
                ))?;
            } else {
                println!(
                    "{:<20}{:<22}{:<12}{:<18}ISO",
                    "NAME", "BACKEND", "STATE", "DISK USED/SIZE"
                );
                for m in rows {
                    println!(
                        "{:<20}{:<22}{:<12}{:<18}{}",
                        m["name"].as_str().unwrap_or_default(),
                        m["backend"].as_str().unwrap_or_default(),
                        m["vmState"].as_str().unwrap_or_default(),
                        format!(
                            "{:.1}/{}G",
                            m["diskUsedBytes"].as_u64().unwrap_or(0) as f64 / (1u64 << 30) as f64,
                            m["diskGiB"]
                        ),
                        m["source"]["iso"].as_str().unwrap_or("-")
                    );
                }
            }
        }
        "-h" | "--help" | "help" => println!("{USAGE}"),
        other => bail!("unknown machine command: {other}\n{USAGE}"),
    }
    Ok(0)
}

/// `machine exec`: the command's output to this process, its exit code ours.
fn exec(name: &str, argv: &[String], p: &crate::args::Parsed) -> Result<i32> {
    let stdin = p.has("interactive");
    let mut args = json!({"name":name,"argv":argv,"stdin":stdin,"env":p.values("env")});
    if let Some(user) = p.value("user") {
        args["user"] = json!(user);
    }
    if let Some(dir) = p.value("workdir") {
        args["cwd"] = json!(dir);
    }
    let stream = crate::api()?.stream("machine.exec", args)?;
    if stdin {
        let sender = stream.sender();
        std::thread::spawn(move || {
            let mut buffer = vec![0u8; 64 * 1024];
            let mut input = std::io::stdin().lock();
            loop {
                match std::io::Read::read(&mut input, &mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sender.stdin(&buffer[..n]).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = sender.close_stdin();
        });
    }
    let result = stream.finish(|event| {
        use std::io::Write;
        match event {
            veynodevm_api::Event::Stdout(data) => std::io::stdout().write_all(&data)?,
            veynodevm_api::Event::Stderr(data) => std::io::stderr().write_all(&data)?,
            _ => {}
        }
        Ok(())
    })?;
    Ok(result["exitCode"].as_i64().unwrap_or(1) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn create_flags_become_api_arguments() {
        let a = create_args(&args(&[
            "u",
            "--cpus",
            "2",
            "-m",
            "2g",
            "--disk-size",
            "40G",
            "--gpus",
            "virgl",
            "-p",
            "2222:22",
            "--disk-bus",
            "nvme",
        ]))
        .unwrap();
        assert_eq!(
            a,
            json!({"name":"u","os":"linux","cpus":2,"memoryMiB":2048,"diskGiB":40,
                   "gpu":"virgl","diskBus":"nvme","ports":[{"host":2222,"guest":22}]})
        );
        assert!(create_args(&args(&["u", "-p", "22"])).is_err());
        assert!(create_args(&args(&["u", "--iso", "/no/such.iso"])).is_err());
        let dir = std::env::temp_dir();
        let v = create_args(&args(&["u", "-v", &format!("{}:m0:ro", dir.display())])).unwrap();
        assert_eq!(v["shares"][0]["tag"], "m0");
        assert_eq!(v["shares"][0]["readOnly"], true);
    }

    #[test]
    fn update_flags_become_api_arguments() {
        let a = update_args(&args(&[
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
            "--audio",
            "on",
        ]))
        .unwrap();
        assert_eq!(
            a,
            json!({"name":"u","cpus":8,"memoryMiB":8192,"diskGiB":64,"gpu":null,
                   "ports":[],"newMac":true,"audio":true})
        );
        let a = update_args(&args(&["u", "-p", "2222:22", "-p", "80:80"])).unwrap();
        assert_eq!(
            a["ports"],
            json!([{"host":2222,"guest":22},{"host":80,"guest":80}])
        );
        assert!(update_args(&args(&["u"])).is_err());
        assert!(update_args(&args(&["u", "--disk-size", "2G"])).is_err());
    }

    #[test]
    fn disk_sizes() {
        assert_eq!(disk_gib("40G").unwrap(), 40);
        assert_eq!(disk_gib("40gib").unwrap(), 40);
        assert_eq!(disk_gib("32").unwrap(), 32);
        assert_eq!(disk_gib("1T").unwrap(), 1024);
        assert!(disk_gib("2G").is_err());
        assert!(disk_gib("x").is_err());
    }
}
