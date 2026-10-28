//! The daemon's services: the namespaced methods of API 1 (`docs/api.md`).
//! Each runs on its connection's thread, outside the manager lock, and
//! reaches VMs through [`Ctx::vm`].
use crate::api_server::{Ctx, StreamIo, DAEMON_VERSION};
use anyhow::Result;
use serde_json::{json, Value};

pub mod checkpoint;
pub mod cloud_init;
pub mod container;
pub mod containers;
pub mod desktop_stream;
pub mod display;
pub mod engine;
pub mod files;
pub mod gpt;
pub mod images;
pub mod machine_guest;
pub mod machines;
pub mod macos;
pub mod macos_desktop;
pub mod macos_driver;
pub mod macos_grant;
pub mod mcp;
pub mod proxy;
pub mod publisher;
pub mod pump;
pub mod records;
pub mod session;
pub mod sound;
pub mod util;
pub mod windows_install;

/// This core's own executable, `vm-control`, which runs the helpers the
/// daemon starts as processes (`__proxy`, `__driver`).
pub fn core_exe() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    if exe.file_name().is_some_and(|n| n == "vm-control") {
        return Ok(exe);
    }
    let sibling = exe
        .parent()
        .map(|dir| dir.join("vm-control"))
        .filter(|path| path.is_file());
    sibling.ok_or_else(|| anyhow::anyhow!("vm-control not found next to {}", exe.display()))
}

/// A helper executable shipped next to `vm-control` (`vm-console-stream`,
/// `vz-desktop-stream`, ...).
pub fn sibling(name: &str) -> Result<std::path::PathBuf> {
    let core = core_exe()?;
    let path = core.parent().map(|dir| dir.join(name));
    path.filter(|p| p.is_file()).ok_or_else(|| {
        anyhow::anyhow!(
            "{name} not found next to {} (build the workspace with cargo build)",
            core.display()
        )
    })
}

/// A VM-layer call to the daemon of this home, over its socket: what the
/// container and macOS services use, from the daemon or from a process
/// beside it.
pub fn daemon_call(method: &str, args: Value) -> Result<Value> {
    crate::call(&crate::home::state_dir()?, method, args)
}

thread_local! {
    static PROGRESS: std::cell::RefCell<Option<*const StreamIo>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` with [`progress`] reporting to `io`.
pub fn with_progress<T>(io: &StreamIo, f: impl FnOnce() -> T) -> T {
    struct Reset(Option<*const StreamIo>);
    impl Drop for Reset {
        fn drop(&mut self) {
            PROGRESS.with(|p| *p.borrow_mut() = self.0);
        }
    }
    let _reset = Reset(PROGRESS.with(|p| p.borrow_mut().replace(io as *const StreamIo)));
    f()
}

/// A progress message: an `event` frame of the stream this thread serves,
/// else a line on stderr (the daemon's log, or a CLI's terminal).
pub fn progress(message: &str) {
    let sent = PROGRESS.with(|p| {
        // SAFETY: set only by `with_progress`, which outlives every call made
        // inside it on this thread and clears it on the way out.
        p.borrow()
            .map(|io| unsafe { &*io }.progress(message).is_ok())
    });
    if sent != Some(true) {
        eprintln!("{message}");
    }
}

/// The guest agent's session port, which a container's VM forwards and the
/// API never shows as a published port.
pub const SESSION_PORT: u16 = 52022;

pub type CallFn = fn(&Ctx, Value) -> Result<Value>;
pub type StreamFn = fn(&Ctx, Value, &StreamIo) -> Result<Value>;

/// How a method answers.
#[derive(Clone, Copy)]
pub enum Handler {
    /// Once.
    Call(CallFn),
    /// With a stream of frames, then `end`.
    Stream(StreamFn),
}

use Handler::{Call, Stream};

const TABLE: &[(&str, Handler)] = &[
    ("daemon.status", Call(daemon_status)),
    ("machine.ls", Call(machines::ls)),
    ("machine.inspect", Call(machines::inspect)),
    ("machine.create", Stream(machines::create)),
    ("machine.start", Stream(machines::start)),
    ("machine.stop", Call(machines::stop)),
    ("machine.rm", Call(machines::rm)),
    ("machine.update", Call(machines::update)),
    ("machine.rename", Call(machines::rename)),
    ("machine.clone", Call(machines::clone)),
    ("machine.eject", Call(machines::eject)),
    ("machine.icon", Call(machines::icon)),
    ("machine.ssh_key", Call(machines::ssh_key)),
    ("machine.resolution", Call(machines::resolution)),
    ("machine.display", Call(machines::display)),
    ("machine.sound", Call(machines::sound)),
    ("machine.microphone", Call(machines::microphone)),
    ("machine.endpoints", Call(machines::endpoints)),
    ("machine.exec", Stream(machines::exec)),
    ("files.copy", Stream(files::copy)),
    ("container.run", Stream(containers::run)),
    ("container.ls", Call(containers::ls)),
    ("container.inspect", Call(containers::inspect)),
    ("container.start", Call(containers::start)),
    ("container.stop", Call(containers::stop)),
    ("container.rm", Call(containers::rm)),
    ("container.rename", Call(containers::rename)),
    ("container.update", Call(containers::update)),
    ("container.icon", Call(containers::icon)),
    ("container.exec", Stream(containers::exec)),
    ("container.logs", Stream(containers::logs)),
    ("container.attach", Stream(containers::attach)),
    ("container.cp", Stream(containers::cp)),
    ("container.desktop", Call(containers::desktop)),
    ("checkpoint.create", Call(containers::checkpoint_create)),
    ("checkpoint.ls", Call(containers::checkpoint_ls)),
    ("checkpoint.rm", Call(containers::checkpoint_rm)),
    ("checkpoint.rollback", Call(containers::checkpoint_rollback)),
    ("checkpoint.fork", Call(containers::checkpoint_fork)),
    ("checkpoint.unfork", Call(containers::checkpoint_unfork)),
    ("image.ls", Call(images::ls)),
    ("image.inspect", Call(images::inspect)),
    ("image.pull", Stream(images::pull)),
    ("image.rm", Call(images::rm)),
    ("image.load", Stream(images::load)),
    ("image.gc", Call(images::gc)),
    ("engine.status", Call(engine::status)),
    ("engine.start", Stream(engine::start)),
    ("engine.stop", Call(engine::stop)),
    ("engine.rm", Call(engine::rm)),
    ("desktop.open", Call(containers::desktop_open)),
    ("desktop.close", Call(containers::desktop_close)),
    ("desktop.stream", Stream(desktop_stream::stream)),
    ("terminal.open", Stream(session::terminal)),
    ("mcp.serve", Stream(mcp::serve)),
];

pub fn lookup(method: &str) -> Option<Handler> {
    #[cfg(test)]
    let mut table = TABLE.iter().chain(tests::TABLE);
    #[cfg(not(test))]
    let mut table = TABLE.iter();
    table
        .find(|(name, _)| *name == method)
        .map(|(_, handler)| *handler)
}

pub fn names() -> impl Iterator<Item = &'static str> {
    TABLE.iter().map(|(name, _)| *name)
}

fn daemon_status(ctx: &Ctx, _: Value) -> Result<Value> {
    let vms = ctx.vm("list", json!({}))?;
    Ok(json!({
        "pid": std::process::id(),
        "daemon": DAEMON_VERSION,
        "api": veynodevm_api::API,
        "home": crate::home::root()?,
        "runtime": ctx.runtime,
        "vms": vms.as_array().map_or(0, Vec::len),
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::api_server::Input;
    use std::time::Duration;

    pub const TABLE: &[(&str, Handler)] = &[
        ("test.echo", Handler::Stream(echo)),
        ("test.refuse", Handler::Call(refuse)),
        ("test.wait", Handler::Stream(wait)),
        ("test.session", Handler::Stream(session)),
    ];

    /// Echoes stdin to stdout until its end, reports each control message as
    /// an event, and answers with the bytes it saw.
    fn echo(_: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
        io.progress("echoing")?;
        let mut seen = 0;
        loop {
            while let Some(message) = io.control(Duration::ZERO) {
                io.event(&json!({ "control": message }))?;
            }
            match io.stdin(Duration::from_millis(20)) {
                Input::Data(data) => {
                    seen += data.len();
                    io.stdout(&data)?;
                }
                Input::Eof => break,
                Input::Idle => {}
                Input::Closed => anyhow::bail!("client left"),
            }
        }
        io.stderr(b"done")?;
        Ok(json!({ "bytes": seen, "tag": args["tag"] }))
    }

    fn refuse(_: &Ctx, _: Value) -> Result<Value> {
        Err(veynodevm_api::refuse(
            veynodevm_api::ErrorCode::NotFound,
            "no machine ghost",
        ))
    }

    /// A guest session at `127.0.0.1:{port}`, relayed as `container.exec`
    /// relays one.
    fn session(_: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
        let port = args["port"].as_u64().unwrap() as u16;
        let stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
        Ok(match super::session::serve(stream, io, true)? {
            super::session::Ended::Exit(code) => json!({ "exitCode": code }),
            super::session::Ended::Closed => json!({ "closed": true }),
            super::session::Ended::Left => json!({ "left": true }),
        })
    }

    /// Runs until the client leaves, then records that it noticed.
    fn wait(ctx: &Ctx, _: Value, io: &StreamIo) -> Result<Value> {
        while !io.closed() {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::write(ctx.state_dir.join("noticed-close"), b"")?;
        Ok(Value::Null)
    }
}
