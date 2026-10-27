//! `veynodevm`: Docker-like CLI for VeynodeVM.
//!
//! Containers run as micro-VMs (one VM per container, GPU through QEMU/HVF
//! when requested) or, with `--shared`, inside the shared engine VM that also
//! serves the docker CLI. Tart-format macOS images run as macOS VMs. All state lives under `veynodevm_home` (see
//! `vm_control::home`); the per-user daemon owns VM processes.
mod args;
mod checkpoint;
mod container;
mod daemon;
mod desktop;
mod desktop_stream;
mod engine;
mod gpt;
mod machine;
mod macos;
mod macos_desktop;
mod macos_driver;
mod macos_grant;
mod proxy;
mod session;
mod ui;

use anyhow::{bail, Context, Result};
use args::{desktop_size, memory_mib, parse};
use serde_json::{json, Value};
use std::{io::Write, path::PathBuf, time::Duration};
use veynodevm_oci::Store;
use vm_control::home;

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
  rm [-f] CONTAINER...               Remove containers
  inspect CONTAINER|IMAGE            Show low-level details
  rename CONTAINER NEW_NAME           Rename a container
  update [--cpus N] [-m SIZE] [--disk-size GiB] [--gpus none|virgl|venus]
         [--desktop on|off] [--desktop-size WxH] [-p HOST:GUEST... | -p none]
         [--dns IP... | --dns none] [--mtu N|default] [--new-mac] [--restart] CONTAINER
                                     Change a container's hardware in one go: a GPU
                                     switches QEMU/HVF <-> Apple Virtualization; the
                                     disk only grows; --restart if it is running
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
  machine start|stop [-f]|eject|inspect|rm NAME     machine ls
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

pub fn daemon_call(method: &str, args: Value) -> Result<Value> {
    vm_control::call(&home::state_dir()?, method, args)
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

fn age(seconds: u64) -> String {
    let d = container::now().saturating_sub(seconds);
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
        let store = Store::open(&home::images_dir()?)?;
        if store.find(&image)?.is_some_and(|i| i.config.os == "darwin") {
            bail!("macOS images run as their own VM; drop --shared")
        }
        return engine::run_shared(rest, RUN_SPEC);
    }
    let options = container::RunOptions {
        image,
        command,
        entrypoint: p.value("entrypoint").map(|e| {
            if e.is_empty() {
                vec![]
            } else {
                vec![e.to_owned()]
            }
        }),
        name: p.value("name").map(str::to_owned),
        detach: p.has("detach"),
        auto_remove: p.has("rm"),
        ports: p.values("publish").to_vec(),
        volumes: p.values("volume").to_vec(),
        env: p.values("env").to_vec(),
        workdir: p.value("workdir").map(str::to_owned),
        user: p.value("user").map(str::to_owned),
        cpus: p
            .value("cpus")
            .map(|c| c.parse::<f64>().map(|v| v.ceil().max(1.0) as u32))
            .transpose()
            .context("invalid --cpus")?,
        memory_mib: p.value("memory").map(memory_mib).transpose()?,
        disk_gib: p
            .value("disk-size")
            .map(|d| d.trim_end_matches(['g', 'G']).parse())
            .transpose()
            .context("invalid --disk-size")?,
        gpus: p.value("gpus").map(str::to_owned),
        desktop: p.has("desktop"),
        desktop_size: p.value("desktop-size").map(desktop_size).transpose()?,
        engine: false,
        dns: p.values("dns").to_vec(),
        mtu: p
            .value("mtu")
            .map(str::parse)
            .transpose()
            .context("invalid --mtu")?,
        init: p.value("init").map(str::to_owned),
        quiet: p.has("quiet") || json_output,
        tty: p.has("tty"),
        stdin: p.has("interactive"),
    };
    container::install_interrupt_handler();
    let c = container::create_and_start(&options)?;
    if c.os == "darwin" {
        return run_macos(
            &c,
            &options,
            p.has("tty"),
            p.has("interactive"),
            json_output,
        );
    }
    if options.detach {
        if json_output {
            print_json(&json!({"id":c.id,"name":c.name}))?;
        } else {
            println!("{}", c.id);
        }
        return Ok(0);
    }
    attach(&c)
}

/// Attach to the entrypoint: its terminal with `-t`, its stdin plus the
/// console output with `-i`, otherwise just the output.
fn attach(c: &container::Container) -> Result<i32> {
    if !(c.tty || c.stdin) {
        return follow(c);
    }
    let stream = session::connect(
        c,
        &session::Request {
            op: "attach",
            argv: vec![],
            tty: c.tty,
            stdin: true,
            env: vec![],
            workdir: None,
            user: None,
            size: session::terminal_size(),
        },
    )?;
    if !c.tty {
        // Output comes from the console log; the session only carries stdin.
        let writer = session::Writer::new(&stream)?;
        std::thread::spawn(move || {
            let mut input = std::io::stdin().lock();
            let mut buffer = [0u8; 8192];
            loop {
                match std::io::Read::read(&mut input, &mut buffer) {
                    Ok(0) | Err(_) => {
                        let _ = writer.frame(b'e', &[]);
                        return;
                    }
                    Ok(n) => {
                        if writer.frame(b'i', &buffer[..n]).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        let code = follow(c);
        drop(stream);
        return code;
    }
    match session::run(stream, true, true, true)? {
        session::Ended::Detached => {
            eprintln!("\r\nveynodevm: detached from {}", c.name);
            Ok(0)
        }
        _ => finish(c, None),
    }
}

/// macOS has no entrypoint: COMMAND (or a login shell) runs over SSH once the
/// Guest is up. In the foreground the container stops when it exits, as a
/// Linux container does; with `-d` the COMMAND runs in the background.
fn run_macos(
    c: &container::Container,
    options: &container::RunOptions,
    tty: bool,
    interactive: bool,
    json_output: bool,
) -> Result<i32> {
    if options.detach {
        if options.command.is_empty() {
            // First-boot setup (volumes, the Accessibility grant) happens now
            // rather than on the first exec.
            macos::ensure_ready(c, options.quiet)?;
        } else {
            macos::start_command(c)?;
        }
        if json_output {
            print_json(&json!({"id":c.id,"name":c.name}))?;
        } else {
            println!("{}", c.id);
        }
        return Ok(0);
    }
    let code = macos::exec(c, &options.command, tty, interactive);
    if c.auto_remove {
        container::remove(c, true)?;
    } else {
        container::stop(c, Duration::from_secs(10))?;
    }
    code
}

fn follow(c: &container::Container) -> Result<i32> {
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let code = container::stream_logs(c, true, &mut stdout, &mut stderr)?;
    finish(c, code)
}

/// After the entrypoint's output ended: wait for the VM, report its status
/// and honor `--rm`.
fn finish(c: &container::Container, code: Option<i32>) -> Result<i32> {
    if container::INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
        eprintln!("veynodevm: stopping {}", c.name);
        container::stop(c, Duration::from_secs(5))?;
    } else {
        container::wait_stopped(c, Duration::from_secs(20));
    }
    let code = code.or_else(|| container::read_console(c).exit);
    if c.auto_remove {
        container::remove(c, true)?;
    }
    match code {
        Some(code) => Ok(code),
        None => {
            let console = container::read_console(c);
            if let Some(e) = console.errors.last() {
                bail!("container failed: {e}")
            }
            bail!("container stopped without reporting an exit status")
        }
    }
}

fn ps(rest: &[String], json_output: bool) -> Result<i32> {
    let p = parse(
        rest,
        &[("all", Some('a'), false), ("quiet", Some('q'), false)],
        false,
    )?;
    let mut rows = Vec::new();
    for c in container::list()? {
        let (state, status) = container::status_text(&c);
        let running = !matches!(
            state.as_str(),
            "stopped" | "crashed" | "missing" | "unknown"
        );
        if !running && !p.has("all") {
            continue;
        }
        rows.push((c, state, status));
    }
    if json_output {
        return print_json(&Value::Array(rows.iter().map(|(c, state, status)| json!({"id":c.id,"name":c.name,"image":c.image,"command":c.argv,"created_at":c.created_at,"state":state,"status":status,"ports":c.ports,"backend":c.backend,"gpus":c.gpus,"desktop":c.desktop,"desktop_size":c.desktop_size,"vm_id":c.vm_id,"icon":c.icon,"os":c.os,"engine":c.engine,"cpus":c.cpus,"memory_mib":c.memory_mib})).collect())).map(|_| 0);
    }
    if p.has("quiet") {
        for (c, _, _) in rows {
            println!("{}", c.id);
        }
        return Ok(0);
    }
    println!(
        "{:<14}{:<24}{:<26}{:<16}{:<24}{:<26}NAMES",
        "CONTAINER ID", "IMAGE", "COMMAND", "CREATED", "STATUS", "PORTS"
    );
    for (c, _, status) in rows {
        let mut command = format!("\"{}\"", c.argv.join(" "));
        if command.chars().count() > 22 {
            command = format!("{}…\"", command.chars().take(20).collect::<String>());
        }
        let mut image = c.image.clone();
        if image.chars().count() > 22 {
            image = format!("{}…", image.chars().take(21).collect::<String>());
        }
        let ports: Vec<String> = c
            .ports
            .iter()
            .map(|p| {
                format!(
                    "{}:{}->{}/{}",
                    p.host_address, p.host_port, p.guest_port, p.protocol
                )
            })
            .collect();
        let gpu = c
            .gpus
            .as_ref()
            .map(|g| format!(" [gpu:{g}]"))
            .unwrap_or_default();
        println!(
            "{:<14}{:<24}{:<26}{:<16}{:<24}{:<26}{}",
            c.id,
            image,
            command,
            age(c.created_at),
            format!("{status}{gpu}"),
            ports.join(","),
            c.name
        );
    }
    Ok(0)
}

fn images(rest: &[String], json_output: bool) -> Result<i32> {
    let p = parse(rest, &[("quiet", Some('q'), false)], false)?;
    let store = Store::open(&home::images_dir()?)?;
    let list = store.list()?;
    if json_output {
        return print_json(&serde_json::to_value(&list)?).map(|_| 0);
    }
    if p.has("quiet") {
        for i in list {
            println!("{}", i.id());
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
            i.reference,
            i.id(),
            age(i.created_at),
            human_size(i.size)
        );
    }
    Ok(0)
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
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
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
            let c = container::find(p.positional.first().context("logs requires a CONTAINER")?)?;
            if c.os == "darwin" {
                return macos::logs(&c, p.has("follow"));
            }
            container::stream_logs(
                &c,
                p.has("follow"),
                &mut std::io::stdout(),
                &mut std::io::stderr(),
            )?;
            Ok(0)
        }
        "attach" => {
            let c = container::find(rest.first().context("attach requires a CONTAINER")?)?;
            if c.os == "darwin" {
                return macos::logs(&c, true);
            }
            container::install_interrupt_handler();
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
            let c = container::find(
                p.positional
                    .first()
                    .context("exec requires CONTAINER COMMAND")?,
            )?;
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
            if c.os != "darwin" && (tty || interactive) && !json_output {
                if tty && !env.iter().any(|e| e.starts_with("TERM=")) {
                    env.push(format!(
                        "TERM={}",
                        std::env::var("TERM").unwrap_or_else(|_| "xterm".into())
                    ));
                }
                let stream = session::connect(
                    &c,
                    &session::Request {
                        op: "exec",
                        argv: if argv.is_empty() {
                            session::default_shell()
                        } else {
                            argv.to_vec()
                        },
                        tty,
                        stdin: interactive,
                        env,
                        workdir: p.value("workdir").map(str::to_owned),
                        user: p.value("user").map(str::to_owned),
                        size: session::terminal_size(),
                    },
                )?;
                return match session::run(stream, tty, interactive, false)? {
                    session::Ended::Exit(code) => Ok(code),
                    _ => bail!("the session to {} closed without an exit status", c.name),
                };
            }
            if c.os == "darwin" {
                if json_output {
                    let result = macos::exec_capture(&c, argv)?;
                    print_json(&result)?;
                    return Ok(result["exit_code"].as_i64().unwrap_or(255) as i32);
                }
                return macos::exec(&c, argv, p.has("tty"), p.has("interactive"));
            }
            let timeout = p
                .value("timeout")
                .map(str::parse)
                .transpose()
                .context("invalid --timeout")?
                .unwrap_or(30000);
            let mut identity = json!({});
            if let Some(user) = p.value("user") {
                identity["user"] = json!(user);
            }
            if !env.is_empty() {
                identity["env"] = json!(env);
            }
            if let Some(workdir) = p.value("workdir") {
                identity["workdir"] = json!(workdir);
            }
            let result = container::exec_as(&c, argv, timeout, &identity)?;
            if json_output {
                print_json(&result)?;
            } else {
                print!("{}", result["stdout"].as_str().unwrap_or_default());
                eprint!("{}", result["stderr"].as_str().unwrap_or_default());
                if result["timed_out"] == true {
                    eprintln!("veynodevm: command timed out");
                }
            }
            Ok(result["exit_code"].as_i64().unwrap_or(126) as i32)
        }
        "checkpoint" => {
            let (action, rest) = rest
                .split_first()
                .context("usage: veynodevm checkpoint create|ls|rm CONTAINER [NAME]")?;
            let p = parse(rest, &[("disk-only", None, false)], false)?;
            let args = &p.positional;
            let c = container::find(args.first().context("checkpoint requires a CONTAINER")?)?;
            let name = || {
                args.get(1)
                    .map(String::as_str)
                    .context("checkpoint requires a NAME")
            };
            let value = match action.as_str() {
                "create" => checkpoint::create(&c, name()?, !p.has("disk-only"))?,
                "ls" | "list" => serde_json::to_value(checkpoint::list(&c)?)?,
                "rm" | "delete" => {
                    checkpoint::remove(&c, name()?)?;
                    json!({"container":c.name,"removed":name()?})
                }
                other => bail!("unknown checkpoint action: {other} (create, ls, rm)"),
            };
            if json_output {
                print_json(&value)?;
            } else if action == "ls" || action == "list" {
                println!("{:<24} {:<16} CONSISTENCY", "NAME", "CREATED");
                for m in checkpoint::list(&c)? {
                    let consistency = if m.machine_state_bytes.is_some() {
                        "running, memory saved"
                    } else if !m.source_running {
                        "stopped"
                    } else if m.guest_synced {
                        "running, synced"
                    } else {
                        "running, unsynced"
                    };
                    println!("{:<24} {:<16} {consistency}", m.name, age(m.created_at));
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
            let report =
                checkpoint::rollback(&container::find(container)?, name, start, !p.has("cold"))?;
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
                let report = checkpoint::remove_forks(selector)?;
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
            let source = container::find(p.positional.first().context(
                "usage: veynodevm fork [-n N] [--checkpoint NAME] [--no-start] CONTAINER",
            )?)?;
            let options = checkpoint::ForkOptions {
                count: p
                    .value("count")
                    .map(str::parse)
                    .transpose()
                    .context("invalid -n")?
                    .unwrap_or(1),
                checkpoint: p.value("checkpoint").map(str::to_owned),
                start: !p.has("no-start"),
            };
            let report = checkpoint::fork(&source, &options)?;
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
            [source, target] => macos::cp(source, target),
            _ => bail!("usage: veynodevm cp SRC DST (one side CONTAINER:PATH)"),
        },
        "start" => {
            let (p, names) = names(rest, &[("attach", Some('a'), false)])?;
            for name in names {
                let c = container::find(&name)?;
                container::start(&c)?;
                if p.has("attach") && c.os == "darwin" {
                    return macos::logs(&c, true);
                }
                if p.has("attach") {
                    container::install_interrupt_handler();
                    return attach(&c);
                }
                println!("{}", c.name);
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
                let c = container::find(&name)?;
                let timeout = timeout
                    .map(Duration::from_secs)
                    .unwrap_or_else(|| container::default_stop_timeout(&c));
                container::stop(&c, timeout)?;
                println!("{}", c.name);
            }
            Ok(0)
        }
        "rm" => {
            let (p, names) = names(rest, &[("force", Some('f'), false)])?;
            for name in names {
                let c = container::find(&name)?;
                container::remove(&c, p.has("force"))?;
                println!("{}", c.name);
            }
            Ok(0)
        }
        "rename" => {
            let [old, new] = rest else {
                bail!("rename requires CONTAINER NEW_NAME")
            };
            let mut c = container::find(old)?;
            container::rename(&mut c, new)?;
            if json_output {
                print_json(&json!({"id":c.id,"name":c.name}))?;
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
                ],
                false,
            )?;
            let [name] = p.positional.as_slice() else {
                bail!("usage: veynodevm update [--cpus N] [-m SIZE] [--disk-size GiB] [--gpus none|virgl|venus] [--desktop on|off] [--desktop-size WxH] [-p HOST:GUEST[/udp]...|--publish none] [--dns IP...|--dns none] [--mtu N|default] [--new-mac] [--restart] CONTAINER")
            };
            // `none` alone clears a list.
            let list = |name: &str| {
                let values = p.values(name);
                (!values.is_empty()).then(|| match values {
                    [only] if only == "none" => Vec::new(),
                    values => values.to_vec(),
                })
            };
            let changes = container::Changes {
                cpus: p
                    .value("cpus")
                    .map(|v| v.parse().with_context(|| format!("invalid --cpus {v}")))
                    .transpose()?,
                memory_mib: p.value("memory").map(memory_mib).transpose()?,
                disk_gib: p
                    .value("disk-size")
                    .map(|v| {
                        v.trim_end_matches(['g', 'G'])
                            .parse()
                            .with_context(|| format!("invalid --disk-size {v} (GiB)"))
                    })
                    .transpose()?,
                gpus: p.value("gpus").map(str::to_owned),
                desktop: p
                    .value("desktop")
                    .map(|v| match v {
                        "on" => Ok(true),
                        "off" => Ok(false),
                        other => bail!("--desktop accepts on|off, got {other}"),
                    })
                    .transpose()?,
                desktop_size: p.value("desktop-size").map(desktop_size).transpose()?,
                ports: list("publish"),
                dns: list("dns"),
                mtu: p
                    .value("mtu")
                    .map(|v| match v {
                        "default" => Ok(0),
                        v => v.parse().with_context(|| format!("invalid --mtu {v}")),
                    })
                    .transpose()?,
                new_mac: p.has("new-mac"),
            };
            if changes.cpus.is_none()
                && changes.memory_mib.is_none()
                && changes.disk_gib.is_none()
                && changes.gpus.is_none()
                && changes.desktop.is_none()
                && changes.desktop_size.is_none()
                && changes.ports.is_none()
                && changes.dns.is_none()
                && changes.mtu.is_none()
                && !changes.new_mac
            {
                bail!("update needs something to change: --cpus, --memory, --disk-size, --gpus, --desktop, --desktop-size, --publish, --dns, --mtu or --new-mac")
            }
            let c = container::update(&container::find(name)?, &changes, p.has("restart"))?;
            if json_output {
                let (_, disk_bytes, _) = container::disk(&c)?;
                print_json(
                    &json!({"id":c.id,"backend":c.backend,"gpus":c.gpus,"desktop":c.desktop,"desktop_size":c.desktop_size,"cpus":c.cpus,"memory_mib":c.memory_mib,"disk_bytes":disk_bytes,"ports":c.ports,"mac_address":c.mac_address}),
                )?;
            } else {
                println!("{}", c.name);
            }
            Ok(0)
        }
        "icon" => {
            let p = parse(rest, &[("reset", None, false)], false)?;
            let mut c =
                container::find(p.positional.first().context("icon requires a CONTAINER")?)?;
            match (p.positional.get(1), p.has("reset")) {
                (Some(_), true) => bail!("give an icon or --reset, not both"),
                (Some(icon), false) => c.icon = Some(container::parse_icon(icon)?),
                (None, true) => c.icon = None,
                (None, false) => {
                    println!("{}", c.icon.as_deref().unwrap_or(""));
                    return Ok(0);
                }
            }
            container::save(&c)?;
            if json_output {
                print_json(&json!({"id":c.id,"icon":c.icon}))?;
            }
            Ok(0)
        }
        "inspect" => {
            let name = rest.first().context("inspect requires a name")?;
            if let Ok(c) = container::find(name) {
                print_json(&container::inspect(&c)?)?;
            } else {
                let store = Store::open(&home::images_dir()?)?;
                let image = store
                    .find(name)?
                    .with_context(|| format!("no such container or image: {name}"))?;
                print_json(&serde_json::to_value(image)?)?;
            }
            Ok(0)
        }
        "pull" => {
            let name = rest.first().context("pull requires an IMAGE")?;
            let store = Store::open(&home::images_dir()?)?;
            let image = store.pull(name, &mut |m| {
                if !json_output {
                    eprintln!("{m}")
                }
            })?;
            if image.config.os != "darwin" {
                store.rootfs(&image, &mut |m| {
                    if !json_output {
                        eprintln!("{m}")
                    }
                })?;
            }
            if json_output {
                print_json(&serde_json::to_value(&image)?)?;
            } else {
                println!("{}", image.reference);
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
                let store = Store::open(&home::images_dir()?)?;
                let freed = store.gc(&container::images_in_use()?)?;
                println!("Total reclaimed space: {}", human_size(freed));
                Ok(0)
            }
            _ => bail!("image commands: ls, pull, rm, gc"),
        },
        "rmi" => {
            let (_, names) = names(rest, &[("force", Some('f'), false)])?;
            let store = Store::open(&home::images_dir()?)?;
            for name in names {
                let image = store.remove(&name)?;
                println!("Untagged: {}", image.reference);
            }
            Ok(0)
        }
        "load" => {
            let p = parse(rest, &[("input", Some('i'), true)], false)?;
            let path = p
                .value("input")
                .or(p.positional.first().map(String::as_str))
                .context("load requires -i ARCHIVE")?;
            let store = Store::open(&home::images_dir()?)?;
            for image in store.load(std::path::Path::new(path))? {
                println!("Loaded image: {}", image.reference);
            }
            Ok(0)
        }
        "desktop" => match rest {
            [close, name] if close == "--close" => desktop::close(name),
            [name] => desktop::open(name),
            [stream, name] if stream == "--stream" => desktop_stream::run(name, "hevc", false),
            [stream, codec, value, name] if stream == "--stream" && codec == "--codec" => {
                desktop_stream::run(name, value, false)
            }
            [stream, codec, value, tracks, name]
                if stream == "--stream" && codec == "--codec" && tracks == "--tracks" =>
            {
                desktop_stream::run(name, value, true)
            }
            _ => bail!("usage: veynodevm desktop [--close] CONTAINER|VM"),
        },
        "engine" => engine::command(rest, json_output),
        "build" => engine::build(rest),
        "vm" => vm(rest),
        "machine" => machine::command(rest, json_output),
        "daemon" => daemon::command(rest, json_output),
        "mcp" => {
            let mut flags = Vec::new();
            let mut rest = rest.iter();
            while let Some(flag) = rest.next() {
                let mapped = match flag.as_str() {
                    "--lifecycle" | "--allow-lifecycle" => "--allow-lifecycle",
                    "--execute" | "--allow-execute" => "--allow-execute",
                    "--files" | "--allow-files" => "--allow-files",
                    "--input" | "--allow-input" => "--allow-input",
                    "--browser" | "--allow-browser" => "--allow-browser",
                    "--clipboard" | "--allow-clipboard" => "--allow-clipboard",
                    // One VM's cua-driver endpoint instead of the VM tools.
                    "--vm" => {
                        flags.push("--vm");
                        rest.next().context("--vm needs a VM id")?.as_str()
                    }
                    // Tool groups narrow the listing; vm-control validates the names.
                    "--tools" => {
                        flags.push("--tools");
                        rest.next().context("--tools needs GROUP,...")?.as_str()
                    }
                    other => bail!("unknown mcp flag: {other}"),
                };
                flags.push(mapped);
            }
            let err = std::os::unix::process::CommandExt::exec(
                std::process::Command::new(sibling("vm-control")?)
                    .arg("mcp")
                    .args(flags),
            );
            Err(err.into())
        }
        "ui" => ui::command(rest),
        "__proxy" => proxy::run(rest.first().context("missing id")?).map(|_| 0),
        "__terminal" => match rest {
            [name, rows, cols, argv @ ..] => {
                session::relay(name, rows.parse()?, cols.parse()?, argv.to_vec())
            }
            _ => bail!("usage: veynodevm __terminal CONTAINER|MACHINE ROWS COLS [COMMAND...]"),
        },
        "__desktop" => match rest {
            [vm, method, arguments] => {
                print_json(&macos_desktop::run(vm, method, arguments)?)?;
                Ok(0)
            }
            _ => bail!("usage: veynodevm __desktop VM METHOD ARGUMENTS"),
        },
        "__driver" => match rest {
            [vm] => macos_driver::run(vm),
            _ => bail!("usage: veynodevm __driver VM"),
        },
        other => bail!("unknown command: {other} (see veynodevm --help)"),
    }
}
