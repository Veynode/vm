//! `veynodevm`: Docker-like CLI for VeynodeVM.
//!
//! Containers run as micro-VMs (one VM per container, GPU through QEMU/HVF
//! when requested) or, with `--shared`, inside the shared engine VM that also
//! serves the docker CLI. All state lives under `veynodevm_home` (see
//! `vm_control::home`); the per-user daemon owns VM processes.
mod args;
mod container;
mod daemon;
mod desktop;
mod engine;
mod proxy;
mod ui;

use anyhow::{bail, Context, Result};
use args::{memory_mib, parse};
use serde_json::{json, Value};
use std::{io::Write, path::PathBuf, time::Duration};
use veynodevm_oci::Store;
use vm_control::home;

const USAGE: &str = "veynodevm — OCI containers as GPU-capable micro-VMs on Apple Silicon

Usage: veynodevm [--json] COMMAND

Containers:
  run [OPTIONS] IMAGE [COMMAND...]   Create and start a container
      -d, --detach  --rm  --name NAME  -p, --publish [IP:]HOST:GUEST[/udp]
      -v, --volume /HOST:/PATH[:ro]  -e, --env KEY[=VALUE]  -w, --workdir DIR
      -u, --user USER  --entrypoint CMD  --cpus N  -m, --memory SIZE
      --disk-size GiB  --dns IP  --mtu N  --gpus all|virgl|venus  --desktop  --shared
      (--gpus selects QEMU/HVF; otherwise Apple Virtualization)
  ps [-a] [-q]                       List containers
  logs [-f] CONTAINER                Show container output
  exec CONTAINER COMMAND...          Run a command (bounded, 30 s max)
  start [-a] CONTAINER               Start a stopped container
  stop [-t SECONDS] CONTAINER...     Stop containers
  rm [-f] CONTAINER...               Remove containers
  inspect CONTAINER|IMAGE            Show low-level details
  desktop [--close] CONTAINER|VM     Open (or close) the native desktop window

Images:
  pull IMAGE        images [-q]       rmi IMAGE...
  load -i ARCHIVE   (docker save archive)
  build -t TAG [-f FILE] CONTEXT    Build in the engine, import for micro-VMs
  image gc          Delete unreferenced blobs and image roots

Shared engine (docker CLI compatible):
  engine start|stop|status|env      Shared Linux VM running dockerd

Virtual machines:
  vm ls | vm create CONFIG.json | vm start|stop|rm|inspect ID

Service:
  daemon run|start|stop|status      Per-user control daemon (launchd agent)
  mcp [--lifecycle] [--execute] [--files] [--input]   MCP over stdio
  ui [--port N] [--open]            Web management UI on localhost
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
    ("shared", None, false),
    ("dns", None, true),
    ("mtu", None, true),
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
        return engine::run_shared(rest, RUN_SPEC);
    }
    if p.has("interactive") || p.has("tty") {
        eprintln!("veynodevm: -i/-t are accepted but interactive stdin is not wired to micro-VMs yet; output is streamed");
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
        engine: false,
        dns: p.values("dns").to_vec(),
        mtu: p
            .value("mtu")
            .map(str::parse)
            .transpose()
            .context("invalid --mtu")?,
        quiet: p.has("quiet") || json_output,
    };
    container::install_interrupt_handler();
    let c = container::create_and_start(&options)?;
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

fn attach(c: &container::Container) -> Result<i32> {
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let code = container::stream_logs(c, true, &mut stdout, &mut stderr)?;
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
        return print_json(&Value::Array(rows.iter().map(|(c, state, status)| json!({"id":c.id,"name":c.name,"image":c.image,"command":c.argv,"created_at":c.created_at,"state":state,"status":status,"ports":c.ports,"backend":c.backend,"gpus":c.gpus,"desktop":c.desktop,"vm_id":c.vm_id})).collect())).map(|_| 0);
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
        "{:<40}{:<16}{:<18}SIZE",
        "REPOSITORY:TAG", "IMAGE ID", "CREATED"
    );
    for i in list {
        println!(
            "{:<40}{:<16}{:<18}{}",
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
            container::stream_logs(
                &c,
                p.has("follow"),
                &mut std::io::stdout(),
                &mut std::io::stderr(),
            )?;
            Ok(0)
        }
        "exec" => {
            let p = parse(
                rest,
                &[
                    ("interactive", Some('i'), false),
                    ("tty", Some('t'), false),
                    ("timeout", None, true),
                ],
                true,
            )?;
            let c = container::find(
                p.positional
                    .first()
                    .context("exec requires CONTAINER COMMAND")?,
            )?;
            let argv = &p.positional[1..];
            if argv.is_empty() {
                bail!("exec requires a COMMAND")
            }
            let timeout = p
                .value("timeout")
                .map(str::parse)
                .transpose()
                .context("invalid --timeout")?
                .unwrap_or(30000);
            let result = container::exec(&c, argv, timeout)?;
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
        "start" => {
            let (p, names) = names(rest, &[("attach", Some('a'), false)])?;
            for name in names {
                let c = container::find(&name)?;
                container::start(&c)?;
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
            let timeout = p
                .value("time")
                .map(str::parse)
                .transpose()
                .context("invalid -t")?
                .unwrap_or(10);
            for name in names {
                let c = container::find(&name)?;
                container::stop(&c, Duration::from_secs(timeout))?;
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
            store.rootfs(&image, &mut |m| {
                if !json_output {
                    eprintln!("{m}")
                }
            })?;
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
            _ => bail!("usage: veynodevm desktop [--close] CONTAINER|VM"),
        },
        "engine" => engine::command(rest, json_output),
        "build" => engine::build(rest),
        "vm" => vm(rest),
        "daemon" => daemon::command(rest),
        "mcp" => {
            let mut flags = Vec::new();
            for flag in rest {
                flags.push(match flag.as_str() {
                    "--lifecycle" | "--allow-lifecycle" => "--allow-lifecycle",
                    "--execute" | "--allow-execute" => "--allow-execute",
                    "--files" | "--allow-files" => "--allow-files",
                    "--input" | "--allow-input" => "--allow-input",
                    other => bail!("unknown mcp flag: {other}"),
                });
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
        other => bail!("unknown command: {other} (see veynodevm --help)"),
    }
}
