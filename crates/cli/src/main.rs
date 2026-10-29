//! `veynodevm`: Docker-like CLI for VeynodeVM.
//!
//! Containers run as micro-VMs (one VM per container, GPU through QEMU/HVF
//! when requested) or, with `--shared`, inside the shared engine VM that also
//! serves the docker CLI. Tart-format macOS images run as macOS VMs. All state lives under `veynodevm_home` (see
//! `vm_control::home`); the per-user daemon owns VM processes.
mod args;
mod daemon;
mod desktop;
mod engine;
mod home;
mod machine;
mod session;
mod ui;

use anyhow::{bail, Context, Result};
use args::{desktop_size, idle_seconds, memory_mib, parse};
use serde_json::{json, Map, Value};
use std::{io::Write, path::PathBuf};

const USAGE: &str = "veynodevm — OCI containers as GPU-capable micro-VMs on Apple Silicon

Usage: veynodevm [--json] COMMAND

Containers:
  run [OPTIONS] IMAGE [COMMAND...]   Create and start a container
      -i, --interactive  -t, --tty  (Ctrl-P Ctrl-Q detaches from -it)
      -d, --detach  --rm  --name NAME  -p, --publish [IP:]HOST:GUEST[/udp]
      -v, --volume /HOST:/PATH[:ro]  -e, --env KEY[=VALUE]  -w, --workdir DIR
      -u, --user USER  --entrypoint CMD  --cpus N  -m, --memory SIZE
      --disk-size GiB  --dns IP  --mtu N  --gpus all|virgl|venus  --desktop [--desktop-size WxH]  --shared
      --init systemd   (boot the image's /sbin/init as PID 1: a full distribution)
      --idle-pause DURATION  (pause the VM after 5m, 1h, ... without activity)
      --idle-hibernate DURATION  (save the VM's memory to disk and stop it after
                     that long without activity; the next use restores it.
                     Apple Virtualization Linux containers only)
      (--gpus selects QEMU/HVF; otherwise Apple Virtualization)
      macOS images (e.g. ghcr.io/cirruslabs/macos-tahoe-vanilla) run as macOS VMs
      with Metal; COMMAND runs over SSH (-d: in the background, see logs; no
      COMMAND: a login shell), -u sets the login (password from
      VEYNODEVM_MACOS_PASSWORD, default admin), --disk-size grows the disk
  ps [-a] [-q]                       List containers
  logs [-f] CONTAINER                Show container output
  exec [-i] [-t] [-e K=V] [-w DIR] [-u USER] CONTAINER [COMMAND...]
                                     Run a command; without -i/-t it is bounded
                                     (30 s, --timeout MS); -it with no COMMAND
                                     opens a shell
  attach CONTAINER                   Attach to a container started with -i/-t
                                     (otherwise follow its output)
  cp SRC DST                         Copy files; one side is CONTAINER:PATH (macOS)
  start [-a] CONTAINER               Start a stopped container
  stop [-t SECONDS] CONTAINER...     Stop containers
  pause|unpause CONTAINER...         Stop or resume a container's vCPUs (its
                                     memory stays in use)
  sleep CONTAINER...                 Hibernate a container now if it can be
                                     (else pause it); its next use wakes it
  keep-awake DURATION|off CONTAINER  No idle pause or hibernation for that long
  rm [-f] CONTAINER...               Remove containers
  inspect CONTAINER|IMAGE            Show low-level details
  rename CONTAINER NEW_NAME           Rename a container
  update [--cpus N] [-m SIZE] [--disk-size GiB] [--gpus none|virgl|venus]
         [--desktop on|off] [--desktop-size WxH] [-p HOST:GUEST... | -p none]
         [--dns IP... | --dns none] [--mtu N|default] [--new-mac] [--restart]
         [--idle-pause DURATION|off] [--idle-hibernate DURATION|off] CONTAINER
                                     Change a container's hardware in one go: a GPU
                                     switches QEMU/HVF <-> Apple Virtualization; the
                                     disk only grows; --restart if it is running.
                                     --idle-pause and --idle-hibernate apply at
                                     once, running or not
  icon CONTAINER [KIND[:COLOR]|--reset]
                                     Show or set the icon the web UI draws
  desktop [--close] CONTAINER|VM     Open (or close) the native desktop window

Checkpoints and forks (Linux containers; APFS clones of the writable layer):
  checkpoint create [--disk-only] CONTAINER NAME
                                     Apple Virtualization: memory + disk in one
                                     pause; otherwise a crash-consistent disk clone
  checkpoint ls|rm CONTAINER [NAME]
  rollback [--start|--no-start] [--cold] CONTAINER NAME
                                     Discard the current state; resume NAME's
                                     memory when saved, else boot its disk
  fork [-n N] [--checkpoint NAME] [--no-start] CONTAINER
                                     N new containers from one state, each with
                                     a fresh id, hostname, MAC and host ports
  fork rm GROUP|CONTAINER            Remove a fork group (or every fork of one)

Images:
  pull IMAGE        images [-q]       rmi IMAGE...
  load -i ARCHIVE   (docker save archive)
  build -t TAG [-f FILE] CONTEXT    Build in the engine, import for micro-VMs
  image gc          Delete unreferenced blobs and image roots

Shared engine (docker CLI compatible):
  engine start|stop|status|env      Shared Linux VM running dockerd
      start [--gpus venus|virgl] [--share DIR]...  (applied when the engine is created)

Full Linux distributions (UEFI, installed from the distribution's ISO):
  machine create NAME [--iso PATH] [--disk-size 40G] [-m 4g] [--cpus 4]
                      [--gpus virgl|venus | --backend apple|qemu] [--monitor]
  machine start|stop [-f]|pause|unpause|eject|inspect|rm NAME     machine ls
  (--gpus or --backend qemu selects QEMU/HVF; otherwise Apple Virtualization)

Virtual machines:
  vm ls | vm create CONFIG.json | vm start|stop|rm|inspect ID

Service:
  daemon run|start|stop|status      Per-user control daemon (launchd agent)
  mcp [--lifecycle] [--execute] [--files] [--input] [--browser] [--clipboard]
      [--tools GROUP,...] [--vm ID]
                                    MCP over stdio; --tools lists only those groups
                                    (core exec files screen input desktop snapshots cua);
                                    --vm serves one VM's cua-driver tools
  ui [--port N] [--no-open]         Web management UI on localhost; opens the browser
  version
";

/// The daemon, as this CLI.
pub fn api() -> Result<veynodevm_api::Client> {
    veynodevm_api::Client::from_env(concat!("veynodevm-cli/", env!("CARGO_PKG_VERSION")))
}

/// An API 1 method that answers once. Daemon services may take minutes
/// (a disk clone, a guest shutdown), so the wait is long.
pub fn api_call(method: &str, args: Value) -> Result<Value> {
    api()?.call_within(method, args, std::time::Duration::from_secs(900))
}

/// A streaming API 1 method to its end: output to this process's stdout
/// and stderr, progress to stderr unless `quiet`.
pub fn api_stream(method: &str, args: Value, quiet: bool) -> Result<Value> {
    use std::io::Write;
    api()?.stream(method, args)?.finish(|event| {
        match event {
            veynodevm_api::Event::Stdout(data) => {
                let mut out = std::io::stdout();
                out.write_all(&data)?;
                out.flush()?;
            }
            veynodevm_api::Event::Stderr(data) => std::io::stderr().write_all(&data)?,
            veynodevm_api::Event::Event(event) => {
                if let (false, Some(progress)) = (quiet, event["progress"].as_str()) {
                    eprintln!("{progress}");
                }
            }
            veynodevm_api::Event::End(_) => {}
        }
        Ok(())
    })
}

/// A VM-layer method (`vm`, `daemon status`).
pub fn daemon_call(method: &str, args: Value) -> Result<Value> {
    api()?.call(method, args)
}

pub fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    let mut file = std::fs::File::open("/dev/urandom").expect("/dev/urandom");
    std::io::Read::read_exact(&mut file, &mut buffer).expect("random bytes");
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

/// Sibling executable shipped next to `veynodevm` (vm-control, vm-console, ...).
pub fn sibling(name: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let path = exe.parent().context("executable has no parent")?.join(name);
    if !path.is_file() {
        bail!(
            "{name} not found next to {} (build the workspace with cargo build)",
            exe.display()
        )
    }
    Ok(path)
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn age(seconds: u64) -> String {
    let d = now().saturating_sub(seconds);
    match d {
        0..=59 => format!("{d} seconds ago"),
        60..=3599 => format!("{} minutes ago", d / 60),
        3600..=86399 => format!("{} hours ago", d / 3600),
        _ => format!("{} days ago", d / 86400),
    }
}

fn human_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.2}GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1}MB", b as f64 / (1u64 << 20) as f64),
        b => format!("{:.1}kB", b as f64 / 1024.0),
    }
}

const RUN_SPEC: &[(&str, Option<char>, bool)] = &[
    ("detach", Some('d'), false),
    ("interactive", Some('i'), false),
    ("tty", Some('t'), false),
    ("rm", None, false),
    ("name", None, true),
    ("publish", Some('p'), true),
    ("volume", Some('v'), true),
    ("env", Some('e'), true),
    ("workdir", Some('w'), true),
    ("user", Some('u'), true),
    ("entrypoint", None, true),
    ("cpus", None, true),
    ("memory", Some('m'), true),
    ("disk-size", None, true),
    ("gpus", None, true),
    ("desktop", None, false),
    ("desktop-size", None, true),
    ("shared", None, false),
    ("dns", None, true),
    ("mtu", None, true),
    ("init", None, true),
    ("quiet", Some('q'), false),
    ("idle-pause", None, true),
    ("idle-hibernate", None, true),
];

fn run(rest: &[String], json_output: bool) -> Result<i32> {
    let p = parse(rest, RUN_SPEC, true)?;
    let image = p
        .positional
        .first()
        .context("run requires an IMAGE")?
        .clone();
    let command: Vec<String> = p.positional[1..].to_vec();
    if p.has("shared") {
        if p.has("idle-pause") || p.has("idle-hibernate") {
            bail!("--idle-pause and --idle-hibernate apply to micro-VM containers; the shared engine is not paused")
        }
        let os = api_call("image.inspect", json!({"reference":image}))
            .map(|i| i["config"]["os"].clone())
            .unwrap_or_default();
        if os == "darwin" {
            bail!("macOS images run as their own VM; drop --shared")
        }
        return engine::run_shared(rest, RUN_SPEC);
    }
    let mut options = Map::new();
    let mut set = |key: &str, value: Value| {
        if !value.is_null() {
            options.insert(key.into(), value);
        }
    };
    set("image", json!(image));
    set("command", json!(command));
    set(
        "entrypoint",
        json!(p.value("entrypoint").map(|e| {
            if e.is_empty() {
                vec![]
            } else {
                vec![e.to_owned()]
            }
        })),
    );
    set("name", json!(p.value("name")));
    set("detach", json!(p.has("detach")));
    set("autoRemove", json!(p.has("rm")));
    set("ports", json!(p.values("publish")));
    set("volumes", json!(p.values("volume")));
    set("env", json!(p.values("env")));
    set("workdir", json!(p.value("workdir")));
    set("user", json!(p.value("user")));
    set(
        "cpus",
        json!(p
            .value("cpus")
            .map(|c| c.parse::<f64>().map(|v| v.ceil().max(1.0) as u32))
            .transpose()
            .context("invalid --cpus")?),
    );
    set(
        "memoryMiB",
        json!(p.value("memory").map(memory_mib).transpose()?),
    );
    set(
        "diskGiB",
        json!(p
            .value("disk-size")
            .map(|d| d.trim_end_matches(['g', 'G']).parse::<u64>())
            .transpose()
            .context("invalid --disk-size")?),
    );
    set("gpus", json!(p.value("gpus")));
    set("desktop", json!(p.has("desktop")));
    set(
        "desktopSize",
        json!(p.value("desktop-size").map(desktop_size).transpose()?),
    );
    set("dns", json!(p.values("dns")));
    set(
        "mtu",
        json!(p
            .value("mtu")
            .map(str::parse::<u16>)
            .transpose()
            .context("invalid --mtu")?),
    );
    set("init", json!(p.value("init")));
    set(
        "idlePauseS",
        json!(p.value("idle-pause").map(idle_seconds).transpose()?),
    );
    set(
        "idleHibernateS",
        json!(p.value("idle-hibernate").map(idle_seconds).transpose()?),
    );
    set("quiet", json!(p.has("quiet") || json_output));
    set("tty", json!(p.has("tty")));
    set("stdin", json!(p.has("interactive")));
    session::install_interrupt_handler();
    let row = api_stream("container.run", Value::Object(options), false)?;
    let id = row["id"].as_str().unwrap_or_default();
    if p.has("detach") {
        if json_output {
            print_json(&json!({"id":id,"name":row["name"]}))?;
        } else {
            println!("{id}");
        }
        return Ok(0);
    }
    if row["os"] == "darwin" {
        return run_macos(&row, &command, p.has("tty"), p.has("interactive"));
    }
    attach(&row)
}

/// Attach to the entrypoint (`c` is its record or row): its terminal with
/// `-t`, its stdin plus the console output with `-i`, otherwise just the
/// output; then report its status and honor `--rm`.
fn attach(c: &Value) -> Result<i32> {
    let id = c["id"].as_str().unwrap_or_default();
    let name = c["name"].as_str().unwrap_or(id);
    let (tty, stdin) = (c["tty"] == true, c["stdin"] == true && c["os"] != "darwin");
    match session::run("container.attach", json!({"name":id}), tty, stdin, tty)? {
        session::Ended::Detached => {
            eprintln!("\r\nveynodevm: detached from {name}");
            Ok(0)
        }
        session::Ended::Interrupted => {
            eprintln!("veynodevm: stopping {name}");
            api_call("container.stop", json!({"name":id,"timeoutS":5}))?;
            let inspect = api_call("container.inspect", json!({"name":id}))?;
            finish(c, &inspect["exit_code"], &inspect["errors"])
        }
        session::Ended::Done(result) if result["left"] == true => Ok(0),
        session::Ended::Done(result) if c["os"] == "darwin" => {
            Ok(result["exitCode"].as_i64().unwrap_or(255) as i32)
        }
        session::Ended::Done(result) => finish(c, &result["exitCode"], &result["errors"]),
    }
}

/// macOS has no entrypoint: COMMAND (or a login shell) runs over SSH once the
/// Guest is up. In the foreground the container stops when it exits, as a
/// Linux container does.
fn run_macos(c: &Value, command: &[String], tty: bool, interactive: bool) -> Result<i32> {
    let id = c["id"].as_str().unwrap_or_default();
    let ended = session::run(
        "container.exec",
        json!({"name":id,"argv":command,"tty":tty,"stdin":tty || interactive}),
        tty,
        tty || interactive,
        false,
    );
    if c["auto_remove"] == true {
        api_call("container.rm", json!({"name":id,"force":true}))?;
    } else {
        api_call("container.stop", json!({"name":id,"timeoutS":10}))?;
    }
    match ended? {
        session::Ended::Done(result) => Ok(result["exitCode"].as_i64().unwrap_or(255) as i32),
        _ => Ok(130),
    }
}

/// After the entrypoint ended: its status, and `--rm`.
fn finish(c: &Value, code: &Value, errors: &Value) -> Result<i32> {
    if c["auto_remove"] == true {
        api_call("container.rm", json!({"name":c["id"],"force":true}))?;
    }
    match code.as_i64() {
        Some(code) => Ok(code as i32),
        None => {
            if let Some(e) = errors.as_array().and_then(|e| e.last()) {
                bail!("container failed: {}", e.as_str().unwrap_or_default())
            }
            bail!("container stopped without reporting an exit status")
        }
    }
}

/// `cp SRC DST` where exactly one side is `CONTAINER:PATH` (macOS).
fn cp(source: &str, target: &str) -> Result<i32> {
    let split = |s: &str| -> Option<(String, String)> {
        let (name, path) = s.split_once(':')?;
        if name.contains('/') {
            return None;
        }
        find(name).ok().map(|_| (name.to_owned(), path.to_owned()))
    };
    let (name, guest, host, inward) = match (split(source), split(target)) {
        (Some((name, guest)), None) => (name, guest, target, false),
        (None, Some((name, guest))) => (name, guest, source, true),
        _ => bail!("cp needs exactly one CONTAINER:PATH argument"),
    };
    let result = api_stream(
        "container.cp",
        json!({"name":name,"guestPath":guest,"hostPath":std::path::absolute(host)?,"direction":if inward {"in"} else {"out"}}),
        false,
    )?;
    Ok(result["exitCode"].as_i64().unwrap_or(255) as i32)
}

/// `desktop --stream`: the web UI's desktop relay (`desktop.stream`) on
/// this process's pipes.
fn desktop_stream(name: &str, codec: &str, tracks: bool) -> Result<i32> {
    session::pipe(
        "desktop.stream",
        json!({"name":name,"codec":codec,"tracks":tracks}),
    )
}

fn ps(rest: &[String], json_output: bool) -> Result<i32> {
    let p = parse(
        rest,
        &[("all", Some('a'), false), ("quiet", Some('q'), false)],
        false,
    )?;
    let rows = api_call("container.ls", json!({"all":p.has("all")}))?;
    if json_output {
        return print_json(&rows).map(|_| 0);
    }
    let rows = rows.as_array().cloned().unwrap_or_default();
    let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    if p.has("quiet") {
        for c in rows {
            println!("{}", text(&c["id"]));
        }
        return Ok(0);
    }
    println!(
        "{:<14}{:<24}{:<26}{:<16}{:<24}{:<26}NAMES",
        "CONTAINER ID", "IMAGE", "COMMAND", "CREATED", "STATUS", "PORTS"
    );
    for c in rows {
        let argv: Vec<String> = c["command"]
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect();
        let mut command = format!("\"{}\"", argv.join(" "));
        if command.chars().count() > 22 {
            command = format!("{}…\"", command.chars().take(20).collect::<String>());
        }
        let mut image = text(&c["image"]);
        if image.chars().count() > 22 {
            image = format!("{}…", image.chars().take(21).collect::<String>());
        }
        let ports: Vec<String> = c["ports"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| {
                format!(
                    "{}:{}->{}/{}",
                    text(&p["host_address"]),
                    p["host_port"],
                    p["guest_port"],
                    text(&p["protocol"])
                )
            })
            .collect();
        let gpu = c["gpus"]
            .as_str()
            .map(|g| format!(" [gpu:{g}]"))
            .unwrap_or_default();
        println!(
            "{:<14}{:<24}{:<26}{:<16}{:<24}{:<26}{}",
            text(&c["id"]),
            image,
            command,
            age(c["created_at"].as_u64().unwrap_or_default()),
            format!("{}{gpu}", text(&c["status"])),
            ports.join(","),
            text(&c["name"])
        );
    }
    Ok(0)
}

fn images(rest: &[String], json_output: bool) -> Result<i32> {
    let p = parse(rest, &[("quiet", Some('q'), false)], false)?;
    let list = api_call("image.ls", json!({}))?;
    if json_output {
        return print_json(&list).map(|_| 0);
    }
    let list = list.as_array().cloned().unwrap_or_default();
    // Docker's short id: the first 12 hex digits of the config digest.
    let id = |i: &Value| {
        let digest = i["config_digest"].as_str().unwrap_or_default();
        digest
            .strip_prefix("sha256:")
            .unwrap_or(digest)
            .chars()
            .take(12)
            .collect::<String>()
    };
    if p.has("quiet") {
        for i in list {
            println!("{}", id(&i));
        }
        return Ok(0);
    }
    println!(
        "{:<40} {:<15} {:<17} SIZE",
        "REPOSITORY:TAG", "IMAGE ID", "CREATED"
    );
    for i in list {
        println!(
            "{:<40} {:<15} {:<17} {}",
            i["reference"].as_str().unwrap_or_default(),
            id(&i),
            age(i["created_at"].as_u64().unwrap_or_default()),
            human_size(i["size"].as_u64().unwrap_or_default())
        );
    }
    Ok(0)
}

/// A container's record, as `container.inspect` has it.
fn find(name: &str) -> Result<Value> {
    Ok(api_call("container.inspect", json!({"name":name}))?["container"].take())
}

fn names(
    rest: &[String],
    spec: &[(&str, Option<char>, bool)],
) -> Result<(args::Parsed, Vec<String>)> {
    let p = parse(rest, spec, false)?;
    if p.positional.is_empty() {
        bail!("at least one name or id is required")
    }
    let n = p.positional.clone();
    Ok((p, n))
}

fn vm(rest: &[String]) -> Result<i32> {
    let action = rest.first().map(String::as_str).unwrap_or("ls");
    let id = rest.get(1).map(String::as_str);
    let need = || id.context("VM id required");
    let value = match action {
        "ls" | "list" => daemon_call("list", json!({}))?,
        "create" => {
            let config: Value = serde_json::from_slice(&std::fs::read(need()?)?)?;
            daemon_call("create", config)?
        }
        "start" => daemon_call("start", json!({"id":need()?}))?,
        "stop" => daemon_call("shutdown", json!({"id":need()?}))?,
        "kill" => {
            let id = need()?;
            daemon_call("force_stop", json!({"id":id,"confirm":id}))?
        }
        "rm" => daemon_call("delete", json!({"id":need()?,"confirm":need()?}))?,
        "inspect" | "status" => daemon_call("status", json!({"id":need()?}))?,
        other => bail!("unknown vm command: {other}"),
    };
    print_json(&value)?;
    Ok(0)
}

fn main() {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    let json_output = if argv.first().map(String::as_str) == Some("--json") {
        argv.remove(0);
        true
    } else {
        false
    };
    let code = match dispatch(&argv, json_output) {
        Ok(code) => code,
        Err(e) => {
            if json_output {
                println!("{}", json!({"error":format!("{e:#}")}));
            } else {
                eprintln!("veynodevm: {e:#}");
            }
            125
        }
    };
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

fn dispatch(argv: &[String], json_output: bool) -> Result<i32> {
    let Some(command) = argv.first() else {
        print!("{USAGE}");
        return Ok(0);
    };
    let rest = &argv[1..];
    match command.as_str() {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(0)
        }
        "version" | "--version" => {
            println!("veynodevm {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        "run" => run(rest, json_output),
        "ps" => ps(rest, json_output),
        "logs" => {
            let p = parse(rest, &[("follow", Some('f'), false)], false)?;
            let name = p.positional.first().context("logs requires a CONTAINER")?;
            let result = api_stream(
                "container.logs",
                json!({"name":name,"follow":p.has("follow")}),
                false,
            )?;
            // A macOS container's output comes from SSH, whose status counts.
            Ok(match find(name)?["os"].as_str() {
                Some("darwin") => result["exitCode"].as_i64().unwrap_or(255) as i32,
                _ => 0,
            })
        }
        "attach" => {
            let c = find(rest.first().context("attach requires a CONTAINER")?)?;
            session::install_interrupt_handler();
            attach(&c)
        }
        "exec" => {
            let p = parse(
                rest,
                &[
                    ("interactive", Some('i'), false),
                    ("tty", Some('t'), false),
                    ("timeout", None, true),
                    ("env", Some('e'), true),
                    ("workdir", Some('w'), true),
                    ("user", Some('u'), true),
                ],
                true,
            )?;
            let name = p
                .positional
                .first()
                .context("exec requires CONTAINER COMMAND")?;
            let argv = &p.positional[1..];
            let (tty, interactive) = (p.has("tty"), p.has("interactive"));
            if argv.is_empty() && !(tty && interactive) {
                bail!("exec requires a COMMAND")
            }
            // `-e KEY` passes the host's value, as Docker does.
            let mut env: Vec<String> = p
                .values("env")
                .iter()
                .filter_map(|e| {
                    if e.contains('=') {
                        Some(e.clone())
                    } else {
                        std::env::var(e).ok().map(|v| format!("{e}={v}"))
                    }
                })
                .collect();
            if tty && !env.iter().any(|e| e.starts_with("TERM=")) {
                env.push(format!(
                    "TERM={}",
                    std::env::var("TERM").unwrap_or_else(|_| "xterm".into())
                ));
            }
            if (tty || interactive) && !json_output {
                let request = json!({"name":name,"argv":argv,"user":p.value("user"),"env":env,"workdir":p.value("workdir"),"stdin":interactive,"tty":tty});
                return match session::run("container.exec", request, tty, interactive, false)? {
                    session::Ended::Done(result) => match result["exitCode"].as_i64() {
                        Some(code) => Ok(code as i32),
                        None => bail!("the session to {name} closed without an exit status"),
                    },
                    _ => Ok(130),
                };
            }
            let timeout: Option<u64> = p
                .value("timeout")
                .map(str::parse)
                .transpose()
                .context("invalid --timeout")?;
            let request = json!({"name":name,"argv":argv,"timeoutMs":timeout,"user":p.value("user"),"env":env,"workdir":p.value("workdir"),"stdin":interactive,"tty":tty});
            let result = if json_output {
                let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
                let result = api()?.stream("container.exec", request)?.finish(|event| {
                    match event {
                        veynodevm_api::Event::Stdout(data) => stdout.extend(data),
                        veynodevm_api::Event::Stderr(data) => stderr.extend(data),
                        _ => {}
                    }
                    Ok(())
                })?;
                print_json(
                    &json!({"exit_code":result["exitCode"],"stdout":String::from_utf8_lossy(&stdout),"stderr":String::from_utf8_lossy(&stderr),"timed_out":result["timedOut"]}),
                )?;
                result
            } else {
                let result = api_stream("container.exec", request, false)?;
                if result["timedOut"] == true {
                    eprintln!("veynodevm: command timed out");
                }
                result
            };
            Ok(result["exitCode"].as_i64().unwrap_or(126) as i32)
        }
        "checkpoint" => {
            let (action, rest) = rest
                .split_first()
                .context("usage: veynodevm checkpoint create|ls|rm CONTAINER [NAME]")?;
            let p = parse(rest, &[("disk-only", None, false)], false)?;
            let args = &p.positional;
            let c = args.first().context("checkpoint requires a CONTAINER")?;
            let name = || {
                args.get(1)
                    .map(String::as_str)
                    .context("checkpoint requires a NAME")
            };
            let value = match action.as_str() {
                "create" => api_call(
                    "checkpoint.create",
                    json!({"name":c,"checkpoint":name()?,"memory":!p.has("disk-only")}),
                )?,
                "ls" | "list" => api_call("checkpoint.ls", json!({"name":c}))?,
                "rm" | "delete" => {
                    api_call("checkpoint.rm", json!({"name":c,"checkpoint":name()?}))?
                }
                other => bail!("unknown checkpoint action: {other} (create, ls, rm)"),
            };
            if json_output {
                print_json(&value)?;
            } else if action == "ls" || action == "list" {
                println!("{:<24} {:<16} CONSISTENCY", "NAME", "CREATED");
                for m in value.as_array().into_iter().flatten() {
                    let consistency = if !m["machine_state_bytes"].is_null() {
                        "running, memory saved"
                    } else if m["source_running"] != true {
                        "stopped"
                    } else if m["guest_synced"] == true {
                        "running, synced"
                    } else {
                        "running, unsynced"
                    };
                    println!(
                        "{:<24} {:<16} {consistency}",
                        m["name"].as_str().unwrap_or_default(),
                        age(m["created_at"].as_u64().unwrap_or_default())
                    );
                }
            } else {
                println!("{}", name()?);
            }
            Ok(0)
        }
        "rollback" => {
            let p = parse(
                rest,
                &[
                    ("start", None, false),
                    ("no-start", None, false),
                    ("cold", None, false),
                ],
                false,
            )?;
            let [container, name] = p.positional.as_slice() else {
                bail!("usage: veynodevm rollback [--start|--no-start] [--cold] CONTAINER NAME")
            };
            let start = match (p.has("start"), p.has("no-start")) {
                (true, true) => bail!("--start and --no-start are exclusive"),
                (true, false) => Some(true),
                (false, true) => Some(false),
                _ => None,
            };
            let report = api_call(
                "checkpoint.rollback",
                json!({"name":container,"checkpoint":name,"start":start,"memory":!p.has("cold")}),
            )?;
            if json_output {
                print_json(&report)?;
            } else {
                println!(
                    "{} rolled back to {name} in {} ms ({})",
                    report["container"].as_str().unwrap_or_default(),
                    report["total_ms"],
                    report["path"].as_str().unwrap_or("stopped")
                );
            }
            Ok(0)
        }
        "fork" => {
            if rest.first().is_some_and(|a| a == "rm") {
                let selector = rest
                    .get(1)
                    .context("usage: veynodevm fork rm GROUP|CONTAINER")?;
                let report = api_call("checkpoint.unfork", json!({"selector":selector}))?;
                if json_output {
                    print_json(&report)?;
                } else {
                    for name in report["removed"].as_array().into_iter().flatten() {
                        println!("{}", name.as_str().unwrap_or_default());
                    }
                }
                return Ok(0);
            }
            let p = parse(
                rest,
                &[
                    ("count", Some('n'), true),
                    ("checkpoint", None, true),
                    ("no-start", None, false),
                ],
                false,
            )?;
            let source = p.positional.first().context(
                "usage: veynodevm fork [-n N] [--checkpoint NAME] [--no-start] CONTAINER",
            )?;
            let count: u32 = p
                .value("count")
                .map(str::parse)
                .transpose()
                .context("invalid -n")?
                .unwrap_or(1);
            let report = api_call(
                "checkpoint.fork",
                json!({"name":source,"count":count,"checkpoint":p.value("checkpoint"),"start":!p.has("no-start")}),
            )?;
            if json_output {
                print_json(&report)?;
            } else {
                for fork in report["forks"].as_array().into_iter().flatten() {
                    println!("{}", fork["name"].as_str().unwrap_or_default());
                }
                eprintln!(
                    "group {} (remove with: veynodevm fork rm {})",
                    report["group"].as_str().unwrap_or_default(),
                    report["group"].as_str().unwrap_or_default()
                );
            }
            Ok(0)
        }
        "cp" => match rest {
            [source, target] => cp(source, target),
            _ => bail!("usage: veynodevm cp SRC DST (one side CONTAINER:PATH)"),
        },
        "start" => {
            let (p, names) = names(rest, &[("attach", Some('a'), false)])?;
            for name in names {
                let row = api_call("container.start", json!({"name":name}))?;
                if p.has("attach") {
                    session::install_interrupt_handler();
                    return attach(&row);
                }
                println!("{}", row["name"].as_str().unwrap_or_default());
            }
            Ok(0)
        }
        "stop" => {
            let (p, names) = names(rest, &[("time", Some('t'), true)])?;
            let timeout: Option<u64> = p
                .value("time")
                .map(str::parse)
                .transpose()
                .context("invalid -t")?;
            for name in names {
                let row = api_call("container.stop", json!({"name":name,"timeoutS":timeout}))?;
                println!("{}", row["name"].as_str().unwrap_or_default());
            }
            Ok(0)
        }
        "pause" | "unpause" => {
            let (_, names) = names(rest, &[])?;
            for name in names {
                let row = api_call(&format!("container.{command}"), json!({"name":name}))?;
                println!("{}", row["name"].as_str().unwrap_or_default());
            }
            Ok(0)
        }
        "sleep" => {
            let (_, names) = names(rest, &[])?;
            for name in names {
                let row = api_call("container.sleep", json!({"name":name}))?;
                println!("{}", row["name"].as_str().unwrap_or_default());
            }
            Ok(0)
        }
        "keep-awake" => {
            let [duration, name] = rest else {
                bail!("usage: veynodevm keep-awake DURATION|off CONTAINER")
            };
            let seconds = idle_seconds(duration)?;
            let row = api_call(
                "container.keepAwake",
                json!({"name":name,"seconds":seconds}),
            )?;
            println!("{}", row["name"].as_str().unwrap_or_default());
            Ok(0)
        }
        "rm" => {
            let (p, names) = names(rest, &[("force", Some('f'), false)])?;
            for name in names {
                let row = api_call("container.rm", json!({"name":name,"force":p.has("force")}))?;
                println!("{}", row["name"].as_str().unwrap_or_default());
            }
            Ok(0)
        }
        "rename" => {
            let [old, new] = rest else {
                bail!("rename requires CONTAINER NEW_NAME")
            };
            let c = api_call("container.rename", json!({"name":old,"newName":new}))?;
            if json_output {
                print_json(&json!({"id":c["id"],"name":c["name"]}))?;
            }
            Ok(0)
        }
        "update" => {
            let p = parse(
                rest,
                &[
                    ("cpus", None, true),
                    ("memory", Some('m'), true),
                    ("disk-size", None, true),
                    ("gpus", None, true),
                    ("desktop", None, true),
                    ("desktop-size", None, true),
                    ("publish", Some('p'), true),
                    ("dns", None, true),
                    ("mtu", None, true),
                    ("new-mac", None, false),
                    ("restart", None, false),
                    ("idle-pause", None, true),
                    ("idle-hibernate", None, true),
                ],
                false,
            )?;
            let [name] = p.positional.as_slice() else {
                bail!("usage: veynodevm update [--cpus N] [-m SIZE] [--disk-size GiB] [--gpus none|virgl|venus] [--desktop on|off] [--desktop-size WxH] [-p HOST:GUEST[/udp]...|--publish none] [--dns IP...|--dns none] [--mtu N|default] [--new-mac] [--restart] [--idle-pause DURATION|off] [--idle-hibernate DURATION|off] CONTAINER")
            };
            // `none` alone clears a list.
            let list = |name: &str| {
                let values = p.values(name);
                (!values.is_empty()).then(|| match values {
                    [only] if only == "none" => Vec::new(),
                    values => values.to_vec(),
                })
            };
            let mut changes = Map::new();
            let mut change = |key: &str, value: Value| {
                if !value.is_null() {
                    changes.insert(key.into(), value);
                }
            };
            change(
                "cpus",
                json!(p
                    .value("cpus")
                    .map(|v| v
                        .parse::<u32>()
                        .with_context(|| format!("invalid --cpus {v}")))
                    .transpose()?),
            );
            change(
                "memoryMiB",
                json!(p.value("memory").map(memory_mib).transpose()?),
            );
            change(
                "diskGiB",
                json!(p
                    .value("disk-size")
                    .map(|v| {
                        v.trim_end_matches(['g', 'G'])
                            .parse::<u64>()
                            .with_context(|| format!("invalid --disk-size {v} (GiB)"))
                    })
                    .transpose()?),
            );
            change("gpus", json!(p.value("gpus")));
            change(
                "desktop",
                json!(p
                    .value("desktop")
                    .map(|v| match v {
                        "on" => Ok(true),
                        "off" => Ok(false),
                        other => bail!("--desktop accepts on|off, got {other}"),
                    })
                    .transpose()?),
            );
            change(
                "desktopSize",
                json!(p.value("desktop-size").map(desktop_size).transpose()?),
            );
            change("ports", json!(list("publish")));
            change("dns", json!(list("dns")));
            change(
                "mtu",
                json!(p
                    .value("mtu")
                    .map(|v| match v {
                        "default" => Ok(0),
                        v => v
                            .parse::<u16>()
                            .with_context(|| format!("invalid --mtu {v}")),
                    })
                    .transpose()?),
            );
            if p.has("new-mac") {
                change("newMac", json!(true));
            }
            change(
                "idlePauseS",
                json!(p.value("idle-pause").map(idle_seconds).transpose()?),
            );
            change(
                "idleHibernateS",
                json!(p.value("idle-hibernate").map(idle_seconds).transpose()?),
            );
            if changes.is_empty() {
                bail!("update needs something to change: --cpus, --memory, --disk-size, --gpus, --desktop, --desktop-size, --publish, --dns, --mtu, --new-mac, --idle-pause or --idle-hibernate")
            }
            let c = api_call(
                "container.update",
                json!({"name":name,"changes":changes,"restart":p.has("restart")}),
            )?;
            if json_output {
                let mut c = c;
                c.as_object_mut().map(|o| o.remove("name"));
                print_json(&c)?;
            } else {
                println!("{}", c["name"].as_str().unwrap_or_default());
            }
            Ok(0)
        }
        "icon" => {
            let p = parse(rest, &[("reset", None, false)], false)?;
            let name = p.positional.first().context("icon requires a CONTAINER")?;
            let icon = match (p.positional.get(1), p.has("reset")) {
                (Some(_), true) => bail!("give an icon or --reset, not both"),
                (Some(icon), false) => json!({"name":name,"icon":icon}),
                (None, true) => json!({"name":name,"icon":null}),
                (None, false) => {
                    let c = api_call("container.icon", json!({"name":name}))?;
                    println!("{}", c["icon"].as_str().unwrap_or(""));
                    return Ok(0);
                }
            };
            let c = api_call("container.icon", icon)?;
            if json_output {
                print_json(&c)?;
            }
            Ok(0)
        }
        "inspect" => {
            let name = rest.first().context("inspect requires a name")?;
            match api_call("container.inspect", json!({"name":name})) {
                Ok(c) => print_json(&c)?,
                Err(e) if veynodevm_api::code_of(&e) == veynodevm_api::ErrorCode::NotFound => {
                    let image = api_call("image.inspect", json!({"reference":name}))
                        .map_err(|_| anyhow::anyhow!("no such container or image: {name}"))?;
                    print_json(&image)?;
                }
                Err(e) => return Err(e),
            }
            Ok(0)
        }
        "pull" => {
            let name = rest.first().context("pull requires an IMAGE")?;
            let image = api_stream("image.pull", json!({"reference":name}), json_output)?;
            if json_output {
                print_json(&image)?;
            } else {
                println!("{}", image["reference"].as_str().unwrap_or_default());
            }
            Ok(0)
        }
        "images" => images(rest, json_output),
        "image" => match rest.first().map(String::as_str) {
            Some("ls") => images(&rest[1..], json_output),
            Some("pull") => dispatch(
                &[vec!["pull".to_owned()], rest[1..].to_vec()].concat(),
                json_output,
            ),
            Some("rm") => dispatch(
                &[vec!["rmi".to_owned()], rest[1..].to_vec()].concat(),
                json_output,
            ),
            Some("gc" | "prune") => {
                let freed = api_call("image.gc", json!({}))?["removedBytes"]
                    .as_u64()
                    .unwrap_or_default();
                println!("Total reclaimed space: {}", human_size(freed));
                Ok(0)
            }
            _ => bail!("image commands: ls, pull, rm, gc"),
        },
        "rmi" => {
            let (_, names) = names(rest, &[("force", Some('f'), false)])?;
            for name in names {
                let image = api_call("image.rm", json!({"reference":name}))?;
                println!(
                    "Untagged: {}",
                    image["reference"].as_str().unwrap_or_default()
                );
            }
            Ok(0)
        }
        "load" => {
            let p = parse(rest, &[("input", Some('i'), true)], false)?;
            let path = p
                .value("input")
                .or(p.positional.first().map(String::as_str))
                .context("load requires -i ARCHIVE")?;
            let path = std::path::absolute(path)?;
            let images = api_stream("image.load", json!({"path":path}), json_output)?;
            for image in images.as_array().into_iter().flatten() {
                println!(
                    "Loaded image: {}",
                    image["reference"].as_str().unwrap_or_default()
                );
            }
            Ok(0)
        }
        "desktop" => match rest {
            [close, name] if close == "--close" => desktop::close(name),
            [name] => desktop::open(name),
            [stream, name] if stream == "--stream" => desktop_stream(name, "hevc", false),
            [stream, codec, value, name] if stream == "--stream" && codec == "--codec" => {
                desktop_stream(name, value, false)
            }
            [stream, codec, value, tracks, name]
                if stream == "--stream" && codec == "--codec" && tracks == "--tracks" =>
            {
                desktop_stream(name, value, true)
            }
            _ => bail!("usage: veynodevm desktop [--close] CONTAINER|VM"),
        },
        "engine" => engine::command(rest, json_output),
        "build" => engine::build(rest),
        "vm" => vm(rest),
        "machine" => machine::command(rest, json_output),
        "daemon" => daemon::command(rest, json_output),
        "mcp" => mcp(rest),
        "ui" => ui::command(rest),
        "__terminal" => match rest {
            [name, rows, cols, argv @ ..] => {
                session::relay(name, rows.parse()?, cols.parse()?, argv.to_vec())
            }
            _ => bail!("usage: veynodevm __terminal CONTAINER|MACHINE ROWS COLS [COMMAND...]"),
        },
        other => bail!("unknown command: {other} (see veynodevm --help)"),
    }
}

/// `veynodevm mcp`: a stdio MCP server, relayed to a session in the daemon
/// (`mcp.serve`). `--state-dir` names the daemon by its state directory, as
/// `vm-mcp-http` does for each HTTPS session.
fn mcp(rest: &[String]) -> Result<i32> {
    let mut args = json!({});
    let mut client = api()?;
    let mut rest = rest.iter();
    while let Some(flag) = rest.next() {
        match flag.as_str() {
            "--lifecycle" | "--allow-lifecycle" => args["lifecycle"] = json!(true),
            "--execute" | "--allow-execute" => args["execute"] = json!(true),
            "--files" | "--allow-files" => args["files"] = json!(true),
            "--input" | "--allow-input" => args["input"] = json!(true),
            "--browser" | "--allow-browser" => args["browser"] = json!(true),
            "--clipboard" | "--allow-clipboard" => args["clipboard"] = json!(true),
            // One VM's cua-driver endpoint instead of the VM tools.
            "--vm" => args["vm"] = json!(rest.next().context("--vm needs a VM id")?),
            // Tool groups narrow the listing; the daemon validates the names.
            "--tools" => {
                let list = rest.next().context("--tools needs GROUP,...")?;
                args["tools"] = json!(list.split(',').map(str::trim).collect::<Vec<_>>());
            }
            "--state-dir" => {
                let dir = rest.next().context("--state-dir needs a directory")?;
                client = veynodevm_api::Client::at_socket(
                    PathBuf::from(dir).join("control.sock"),
                    concat!("veynodevm-mcp/", env!("CARGO_PKG_VERSION")),
                );
            }
            other => bail!("unknown mcp flag: {other}"),
        }
    }
    match session::run_on(&client, "mcp.serve", args, false, true, false)? {
        session::Ended::Done(_) => Ok(0),
        _ => Ok(1),
    }
}
