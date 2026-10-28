//! `files.copy`: one file between the host and a machine's guest. Windows
//! machines copy over SSH with scp; Linux machines through the guest
//! service's exec, in base64 pieces (bounded by its 30 s commands); macOS
//! guests through `services::macos`.
use super::{
    machines::{self, base64},
    util::string,
};
use crate::api_server::{Ctx, StreamIo};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{fs, path::Path, time::Duration};
use veynodevm_api::{refuse, ErrorCode};

/// Raw bytes per Linux exec: base64 makes 512 KiB of the request.
const LINUX_PIECE: usize = 384 * 1024;

pub fn copy(ctx: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
    let name = machines::resolve(string(&args, "name")?);
    let host = Path::new(string(&args, "hostPath")?);
    let guest = string(&args, "guestPath")?;
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
    if !host.is_absolute() {
        return Err(refuse(ErrorCode::Invalid, "hostPath must be absolute"));
    }
    if guest.is_empty() || guest.contains('\0') {
        return Err(refuse(ErrorCode::Invalid, "guestPath is empty"));
    }
    let machine = machines::load(&name)?;
    let status = ctx.vm("status", json!({"id":name}))?;
    if !machines::running(status["state"].as_str().unwrap_or("unknown")) {
        return Err(refuse(ErrorCode::State, format!("{name} is not running")));
    }
    match machine.os.as_str() {
        "windows" => {
            let config: crate::Config = serde_json::from_value(status["config"].clone())?;
            let ssh = crate::windows_guest::Ssh::locate(&ctx.state_dir, &config)?;
            if !inward {
                return Err(refuse(
                    ErrorCode::Unsupported,
                    "copying out of a Windows machine is not supported",
                ));
            }
            io.progress("Copying over SSH")?;
            ssh.copy(host, &guest.replace('\\', "/"), Duration::from_secs(600))?;
        }
        "macos" => super::macos::copy_machine(ctx, &machine, inward, host, guest, io)?,
        _ if inward => linux_in(ctx, &name, host, guest, io)?,
        _ => linux_out(ctx, &name, host, guest)?,
    }
    Ok(json!({ "exitCode": 0 }))
}

fn linux_run(ctx: &Ctx, name: &str, argv: Vec<String>) -> Result<Value> {
    let reply = ctx.vm("exec", json!({"id":name,"argv":argv,"timeout_ms":30000}))?;
    let result = if reply["result"].is_object() {
        reply["result"].clone()
    } else {
        reply
    };
    if result["exit_code"] != 0 {
        bail!(
            "the guest refused the copy: {}",
            result["stderr"].as_str().unwrap_or("no output").trim()
        )
    }
    Ok(result)
}

fn linux_in(ctx: &Ctx, name: &str, host: &Path, guest: &str, io: &StreamIo) -> Result<()> {
    let bytes = fs::read(host).with_context(|| format!("cannot read {}", host.display()))?;
    let part = format!("{guest}.veynode-part");
    linux_run(
        ctx,
        name,
        vec![
            "/bin/sh".into(),
            "-c".into(),
            ": > \"$1\"".into(),
            "sh".into(),
            part.clone(),
        ],
    )?;
    let pieces = bytes.len().div_ceil(LINUX_PIECE).max(1);
    for (index, piece) in bytes.chunks(LINUX_PIECE).enumerate() {
        if io.closed() {
            bail!("the client left")
        }
        io.progress(&format!("Copying piece {} of {pieces}", index + 1))?;
        linux_run(
            ctx,
            name,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf %s \"$1\" | base64 -d >> \"$2\"".into(),
                "sh".into(),
                base64(piece),
                part.clone(),
            ],
        )?;
    }
    linux_run(
        ctx,
        name,
        vec!["/bin/mv".into(), "-f".into(), part, guest.into()],
    )?;
    Ok(())
}

fn linux_out(ctx: &Ctx, name: &str, host: &Path, guest: &str) -> Result<()> {
    use base64::Engine;
    let result = linux_run(
        ctx,
        name,
        vec!["/usr/bin/base64".into(), "-w0".into(), guest.into()],
    )?;
    if result["stdout_truncated"] == true {
        return Err(refuse(
            ErrorCode::Unsupported,
            "the file is larger than a Linux guest command's output",
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(result["stdout"].as_str().unwrap_or_default().trim())
        .context("the guest's copy was not base64")?;
    fs::write(host, bytes)?;
    Ok(())
}
