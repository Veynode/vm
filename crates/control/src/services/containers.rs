//! `container.*` and `checkpoint.*`: the API over [`container`] and
//! [`checkpoint`]. Rows and records keep the shapes `veynodevm --json`
//! printed, which the web UI and MCP tools read.
use super::{
    checkpoint, container, macos, pump, session,
    util::{flag, only, opt_string, opt_u64, string, KeepAwake},
    with_progress,
};
use crate::api_server::{Ctx, StreamIo};
use anyhow::Result;
use serde_json::{json, Value};
use std::{io::Write, path::Path, time::Duration};
use veynodevm_api::{refuse, ErrorCode};

fn find(args: &Value) -> Result<container::Container> {
    let name = string(args, "name")?;
    container::find(name)
        .map_err(|_| refuse(ErrorCode::NotFound, format!("no such container: {name}")))
}

fn strings(args: &Value, field: &str) -> Result<Vec<String>> {
    match &args[field] {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| refuse(ErrorCode::Invalid, format!("{field} must be strings")))
            })
            .collect(),
        _ => Err(refuse(
            ErrorCode::Invalid,
            format!("{field} must be an array"),
        )),
    }
}

/// A `container.ls` row.
pub fn row(c: &container::Container) -> Value {
    let (state, status) = container::status_text(c);
    json!({"id":c.id,"name":c.name,"image":c.image,"command":c.argv,"created_at":c.created_at,"state":state,"status":status,"ports":c.ports,"backend":c.backend,"gpus":c.gpus,"desktop":c.desktop,"desktop_size":c.desktop_size,"vm_id":c.vm_id,"icon":c.icon,"os":c.os,"engine":c.engine,"init":c.init,"cpus":c.cpus,"memory_mib":c.memory_mib,"tty":c.tty,"stdin":c.stdin,"auto_remove":c.auto_remove,"idle_pause_s":c.idle_pause_s.unwrap_or(0),"idle_hibernate_s":c.idle_hibernate_s.unwrap_or(0)})
}

/// `container.run`: creates and starts a container. A macOS container with
/// `detach` also finishes its first boot (or starts its COMMAND); without
/// `detach` the client attaches to it afterwards.
pub fn run(_: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
    let options: container::RunOptions = serde_json::from_value(args)
        .map_err(|e| refuse(ErrorCode::Invalid, format!("invalid run options: {e}")))?;
    let mut options = options;
    options.idle_pause_s = options
        .idle_pause_s
        .map(container::check_idle_pause)
        .transpose()
        .map_err(|e| refuse(ErrorCode::Invalid, e.to_string()))?
        .flatten();
    options.idle_hibernate_s = options
        .idle_hibernate_s
        .map(container::check_idle_hibernate)
        .transpose()
        .map_err(|e| refuse(ErrorCode::Invalid, e.to_string()))?
        .flatten();
    with_progress(io, || {
        let c = container::create_and_start(&options)?;
        if c.os == "darwin" && options.detach {
            if options.command.is_empty() {
                // First-boot setup (volumes, the Accessibility grant) happens
                // now rather than on the first exec.
                macos::ensure_ready(&c, options.quiet)?;
            } else {
                macos::start_command(&c)?;
            }
        }
        Ok(row(&c))
    })
}

pub fn ls(_: &Ctx, args: Value) -> Result<Value> {
    only(&args, &["all"])?;
    let all = flag(&args, "all")?;
    let mut rows = Vec::new();
    for c in container::list()? {
        let row = row(&c);
        if all
            || !matches!(
                row["state"].as_str(),
                Some("stopped" | "crashed" | "missing" | "unknown")
            )
        {
            rows.push(row);
        }
    }
    Ok(Value::Array(rows))
}

pub fn inspect(_: &Ctx, args: Value) -> Result<Value> {
    container::inspect(&find(&args)?)
}

pub fn start(_: &Ctx, args: Value) -> Result<Value> {
    let c = find(&args)?;
    container::start(&c)?;
    Ok(row(&c))
}

pub fn stop(_: &Ctx, args: Value) -> Result<Value> {
    let c = find(&args)?;
    let timeout = opt_u64(&args, "timeoutS")?
        .map(Duration::from_secs)
        .unwrap_or_else(|| container::default_stop_timeout(&c));
    container::stop(&c, timeout)?;
    Ok(row(&c))
}

/// `container.pause`: stops the VM's vCPUs; its memory stays in use.
pub fn pause(_: &Ctx, args: Value) -> Result<Value> {
    only(&args, &["name"])?;
    let c = find(&args)?;
    if c.engine {
        return Err(refuse(
            ErrorCode::Unsupported,
            format!("{} runs the shared engine; it cannot be paused", c.name),
        ));
    }
    super::daemon_call("pause", json!({"id":c.vm_id}))?;
    Ok(row(&c))
}

/// `container.unpause`: resumes a paused container.
pub fn unpause(_: &Ctx, args: Value) -> Result<Value> {
    only(&args, &["name"])?;
    let c = find(&args)?;
    super::daemon_call("resume", json!({"id":c.vm_id}))?;
    Ok(row(&c))
}

pub fn rm(_: &Ctx, args: Value) -> Result<Value> {
    let c = find(&args)?;
    container::remove(&c, flag(&args, "force")?)?;
    Ok(json!({"id":c.id,"name":c.name}))
}

pub fn rename(_: &Ctx, args: Value) -> Result<Value> {
    let mut c = find(&args)?;
    container::rename(&mut c, string(&args, "newName")?)?;
    Ok(row(&c))
}

pub fn update(_: &Ctx, args: Value) -> Result<Value> {
    let c = find(&args)?;
    let changes: container::Changes = serde_json::from_value(args["changes"].clone())
        .map_err(|e| refuse(ErrorCode::Invalid, format!("invalid changes: {e}")))?;
    if let Some(seconds) = changes.idle_pause_s {
        container::check_idle_pause(seconds)
            .map_err(|e| refuse(ErrorCode::Invalid, e.to_string()))?;
    }
    if let Some(seconds) = changes.idle_hibernate_s {
        container::check_idle_hibernate(seconds)
            .map_err(|e| refuse(ErrorCode::Invalid, e.to_string()))?;
    }
    let mut c = container::update(&c, &changes, flag(&args, "restart")?)?;
    if let Some(seconds) = changes.idle_pause_s {
        container::set_idle_pause(&mut c, seconds)?;
    }
    if let Some(seconds) = changes.idle_hibernate_s {
        container::set_idle_hibernate(&mut c, seconds)?;
    }
    let (_, disk_bytes, _) = container::disk(&c)?;
    Ok(
        json!({"id":c.id,"name":c.name,"backend":c.backend,"gpus":c.gpus,"desktop":c.desktop,"desktop_size":c.desktop_size,"cpus":c.cpus,"memory_mib":c.memory_mib,"disk_bytes":disk_bytes,"ports":c.ports,"mac_address":c.mac_address,"idle_pause_s":c.idle_pause_s.unwrap_or(0),"idle_hibernate_s":c.idle_hibernate_s.unwrap_or(0)}),
    )
}

/// `container.icon`: sets the icon with `icon` (null resets it); without
/// it, answers the current one.
pub fn icon(_: &Ctx, args: Value) -> Result<Value> {
    let mut c = find(&args)?;
    match args.get("icon") {
        None => {}
        Some(Value::Null) => {
            c.icon = None;
            container::save(&c)?;
        }
        Some(_) => {
            c.icon = Some(
                container::parse_icon(string(&args, "icon")?)
                    .map_err(|e| refuse(ErrorCode::Invalid, format!("{e:#}")))?,
            );
            container::save(&c)?;
        }
    }
    Ok(json!({"id":c.id,"icon":c.icon}))
}

/// `container.exec`: in a Linux container, with `stdin` or `tty` a guest
/// session (an empty `argv` is a shell), otherwise a bounded guest job
/// (`timeoutMs`, default 30 s) whose output comes at its end; in a macOS
/// container, the command over SSH, with the client's stdin when `stdin`.
pub fn exec(_: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
    let c = find(&args)?;
    let _awake = KeepAwake::new(&c.vm_id);
    let argv = strings(&args, "argv")?;
    let (tty, stdin) = (flag(&args, "tty")?, flag(&args, "stdin")?);
    if c.os != "darwin" && (tty || stdin) {
        let stream = session::connect(
            &c,
            &session::Request {
                op: "exec",
                argv: if argv.is_empty() {
                    session::default_shell()
                } else {
                    argv
                },
                tty,
                stdin,
                env: strings(&args, "env")?,
                workdir: opt_string(&args, "workdir")?.map(str::to_owned),
                user: opt_string(&args, "user")?.map(str::to_owned),
                size: size(&args),
            },
        )?;
        return match session::serve(stream, io, stdin)? {
            session::Ended::Exit(code) => Ok(json!({"exitCode":code,"timedOut":false})),
            session::Ended::Left => Ok(json!({"exitCode":null,"timedOut":false})),
            session::Ended::Closed => Err(anyhow::anyhow!(
                "the session to {} closed without an exit status",
                c.name
            )),
        };
    }
    if c.os == "darwin" {
        let command = with_progress(io, || macos::exec_command(&c, &argv, flag(&args, "tty")?))?;
        let code = pump::run(command, flag(&args, "stdin")?, io)?;
        return Ok(json!({"exitCode":code,"timedOut":false}));
    }
    if argv.is_empty() {
        return Err(refuse(ErrorCode::Invalid, "argv is required"));
    }
    let mut identity = json!({});
    if let Some(user) = opt_string(&args, "user")? {
        identity["user"] = json!(user);
    }
    let env = strings(&args, "env")?;
    if !env.is_empty() {
        identity["env"] = json!(env);
    }
    if let Some(workdir) = opt_string(&args, "workdir")? {
        identity["workdir"] = json!(workdir);
    }
    let timeout = opt_u64(&args, "timeoutMs")?.unwrap_or(30000);
    let result = container::exec_as(&c, &argv, timeout, &identity)?;
    io.stdout(result["stdout"].as_str().unwrap_or_default().as_bytes())?;
    io.stderr(result["stderr"].as_str().unwrap_or_default().as_bytes())?;
    Ok(
        json!({"exitCode":result["exit_code"].as_i64().unwrap_or(126),"timedOut":result["timed_out"] == true}),
    )
}

/// `{rows, cols}` of a client's terminal.
fn size(args: &Value) -> Option<(u16, u16)> {
    let (rows, cols) = (args["rows"].as_u64()?, args["cols"].as_u64()?);
    Some((rows.clamp(1, 1000) as u16, cols.clamp(1, 1000) as u16))
}

/// `container.attach {name, rows?, cols?}`: the entrypoint of a running
/// container. Started with `-t`, its terminal; with `-i`, the console output
/// and the client's stdin to the entrypoint; otherwise the output alone. A
/// macOS container's background COMMAND output. Once the entrypoint ends
/// the answer carries its exit status (`exitCode`, null when it reported
/// none) and the console's `errors`; `left` when the client went first.
pub fn attach(_: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
    let c = find(&args)?;
    let _awake = KeepAwake::new(&c.vm_id);
    if c.os == "darwin" {
        let command = with_progress(io, || macos::logs_command(&c, true))?;
        let code = pump::run(command, false, io)?;
        return Ok(json!({"exitCode":code,"left":io.closed()}));
    }
    let code = if c.tty || c.stdin {
        let stream = session::connect(
            &c,
            &session::Request {
                op: "attach",
                argv: vec![],
                tty: c.tty,
                stdin: true,
                env: vec![],
                workdir: None,
                user: None,
                size: size(&args),
            },
        )?;
        if c.tty {
            match session::serve(stream, io, true)? {
                session::Ended::Exit(code) => Some(code),
                session::Ended::Closed => None,
                session::Ended::Left => return Ok(json!({"left":true})),
            }
        } else {
            // Output comes from the console log; the session only carries stdin.
            let writer = session::Writer::new(&stream)?;
            let done = std::sync::atomic::AtomicBool::new(false);
            std::thread::scope(|scope| -> Result<Option<i32>> {
                scope.spawn(|| loop {
                    if done.load(std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    match io.stdin(Duration::from_millis(50)) {
                        crate::api_server::Input::Data(data) => {
                            if writer.frame(b'i', &data).is_err() {
                                return;
                            }
                        }
                        crate::api_server::Input::Eof => {
                            let _ = writer.frame(b'e', &[]);
                            return;
                        }
                        crate::api_server::Input::Idle => {}
                        crate::api_server::Input::Closed => return,
                    }
                });
                let code = follow(&c, io);
                done.store(true, std::sync::atomic::Ordering::SeqCst);
                drop(stream);
                code
            })?
        }
    } else {
        follow(&c, io)?
    };
    if io.closed() {
        return Ok(json!({"left":true}));
    }
    container::wait_stopped(&c, Duration::from_secs(20));
    let console = container::read_console(&c);
    Ok(json!({"exitCode":code.or(console.exit),"errors":console.errors,"left":false}))
}

/// The console output until the container exits (or the client leaves).
fn follow(c: &container::Container, io: &StreamIo) -> Result<Option<i32>> {
    match container::stream_logs(c, true, &mut Out(io, true), &mut Out(io, false)) {
        Err(_) if io.closed() => Ok(None),
        other => other,
    }
}

/// Writes to one of a stream's output channels.
struct Out<'a>(&'a StreamIo, bool);

impl Write for Out<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let sent = if self.1 {
            self.0.stdout(data)
        } else {
            self.0.stderr(data)
        };
        sent.map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `container.logs`: the container's output on `stdout`/`stderr`; with
/// `follow`, until it exits. The answer carries its exit code when known.
pub fn logs(_: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
    let c = find(&args)?;
    let follow = flag(&args, "follow")?;
    if c.os == "darwin" {
        let command = with_progress(io, || macos::logs_command(&c, follow))?;
        let code = pump::run(command, false, io)?;
        return Ok(json!({"exitCode":code}));
    }
    let code = container::stream_logs(&c, follow, &mut Out(io, true), &mut Out(io, false))?;
    Ok(json!({"exitCode":code}))
}

/// `container.cp`: one path between the host and a macOS container.
pub fn cp(_: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
    let c = find(&args)?;
    let _awake = KeepAwake::new(&c.vm_id);
    let host = Path::new(string(&args, "hostPath")?);
    if !host.is_absolute() {
        return Err(refuse(ErrorCode::Invalid, "hostPath must be absolute"));
    }
    let inward = match string(&args, "direction")? {
        "in" => true,
        "out" => false,
        other => {
            return Err(refuse(
                ErrorCode::Invalid,
                format!("direction is in or out, got {other}"),
            ))
        }
    };
    if c.os != "darwin" {
        return Err(refuse(
            ErrorCode::Unsupported,
            "cp is implemented for macOS containers",
        ));
    }
    let command = with_progress(io, || {
        macos::cp_command(&c, inward, host, string(&args, "guestPath")?)
    })?;
    Ok(json!({"exitCode":pump::run(command, false, io)?}))
}

/// `container.desktop`: a desktop or accessibility method of a macOS
/// container, run by its helper in the Guest.
pub fn desktop(_: &Ctx, args: Value) -> Result<Value> {
    super::macos_desktop::run(
        string(&args, "vm")?,
        string(&args, "method")?,
        &args["arguments"].to_string(),
    )
}

pub fn checkpoint_create(_: &Ctx, args: Value) -> Result<Value> {
    let c = find(&args)?;
    let memory = args["memory"].as_bool().unwrap_or(true);
    checkpoint::create(&c, string(&args, "checkpoint")?, memory)
}

pub fn checkpoint_ls(_: &Ctx, args: Value) -> Result<Value> {
    Ok(serde_json::to_value(checkpoint::list(&find(&args)?)?)?)
}

pub fn checkpoint_rm(_: &Ctx, args: Value) -> Result<Value> {
    let c = find(&args)?;
    let name = string(&args, "checkpoint")?;
    checkpoint::remove(&c, name)?;
    Ok(json!({"container":c.name,"removed":name}))
}

pub fn checkpoint_rollback(_: &Ctx, args: Value) -> Result<Value> {
    let c = find(&args)?;
    let memory = args["memory"].as_bool().unwrap_or(true);
    checkpoint::rollback(
        &c,
        string(&args, "checkpoint")?,
        args["start"].as_bool(),
        memory,
    )
}

pub fn checkpoint_fork(_: &Ctx, args: Value) -> Result<Value> {
    let c = find(&args)?;
    let options = checkpoint::ForkOptions {
        count: opt_u64(&args, "count")?.unwrap_or(1) as u32,
        checkpoint: opt_string(&args, "checkpoint")?.map(str::to_owned),
        start: args["start"].as_bool().unwrap_or(true),
    };
    checkpoint::fork(&c, &options)
}

/// `checkpoint.unfork`: removes a fork group, or every fork of a container.
pub fn checkpoint_unfork(_: &Ctx, args: Value) -> Result<Value> {
    checkpoint::remove_forks(string(&args, "selector")?)
}

/// The VM behind a container name, a machine name or a VM id.
fn vm_of(ctx: &Ctx, name: &str) -> Result<String> {
    if let Ok(c) = container::find(name) {
        return Ok(c.vm_id);
    }
    let machine = super::machines::resolve(name);
    if super::machines::load(&machine).is_ok() {
        return Ok(machine);
    }
    ctx.vm("status", json!({"id":name}))?;
    Ok(name.to_owned())
}

/// `desktop.open {name}`: the native desktop window of a running VM.
pub fn desktop_open(ctx: &Ctx, args: Value) -> Result<Value> {
    let name = string(&args, "name")?;
    let id = vm_of(ctx, name)?;
    let status = ctx.vm("status", json!({"id":id}))?;
    if matches!(status["state"].as_str(), Some("stopped" | "crashed") | None) {
        return Err(refuse(ErrorCode::State, format!("{name} is not running")));
    }
    ctx.vm("show_window", json!({"id":id}))?;
    Ok(json!({"name":name,"vm":id}))
}

/// `desktop.close {name}`: closes that window; the VM keeps running.
pub fn desktop_close(ctx: &Ctx, args: Value) -> Result<Value> {
    let name = string(&args, "name")?;
    let id = vm_of(ctx, name)?;
    ctx.vm("close_window", json!({"id":id}))?;
    Ok(json!({"name":name,"vm":id}))
}
