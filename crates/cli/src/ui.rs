//! `veynodevm ui`: the TanStack Start management UI on host loopback.
//!
//! Runs `node web/server.mjs` (registered as runtime.json "web") and opens the
//! browser on it. There is no sign-in: the server only binds 127.0.0.1 and
//! checks the Host header (the WebSockets also the Origin); server functions
//! call back into this executable (`VEYNODEVM_CLI`).
use anyhow::{bail, Context, Result};
use std::process::Command;
use vm_control::home;

pub fn command(rest: &[String]) -> Result<i32> {
    let p = crate::args::parse(
        rest,
        &[
            ("port", None, true),
            ("open", None, false),
            ("no-open", None, false),
        ],
        false,
    )?;
    let port: u16 = p
        .value("port")
        .map(str::parse)
        .transpose()
        .context("invalid --port")?
        .unwrap_or(3417);
    let runtime = home::Runtime::load()?;
    let server = runtime
        .web
        .context("web UI not registered: cd web && npm ci && npm run build, then scripts/install-dev-runtime.py")?;
    if !server
        .with_file_name("dist")
        .join("server/server.js")
        .is_file()
    {
        bail!(
            "web UI is not built: cd {} && npm run build",
            server.parent().unwrap().display()
        )
    }
    let node = veynodevm_oci::store::which("node").context("node not found on PATH")?;
    std::net::TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("127.0.0.1:{port} is in use (use --port)"))?;
    let url = format!("http://127.0.0.1:{port}/");
    let mut child = Command::new(node)
        .arg(&server)
        .env("VEYNODEVM_UI_PORT", port.to_string())
        .env("VEYNODEVM_CLI", std::env::current_exe()?)
        .env("VEYNODEVM_HOME", home::root()?)
        .spawn()
        .context("start web UI server")?;
    println!("VeynodeVM UI: {url}");
    // `--open` is the default now; it stays accepted for old scripts.
    if !p.has("no-open") {
        std::thread::sleep(std::time::Duration::from_millis(600));
        let _ = Command::new("open").arg(&url).status();
    }
    let status = child.wait()?;
    Ok(status.code().unwrap_or(0))
}
