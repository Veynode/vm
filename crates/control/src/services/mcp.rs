//! `mcp.serve`: one MCP session in the daemon, its JSON-RPC lines on the
//! stream's `stdin` and `stdout`. The stdio and HTTPS adapters
//! (`veynodevm mcp`, `vm-mcp-http`) relay a client to it; the session's
//! permissions are the arguments, as the flags of `vm-control mcp` are.
use super::util::{flag, opt_string};
use crate::{
    api_server::{Ctx, StreamIo},
    tool_groups, McpOptions,
};
use anyhow::Result;
use serde_json::{json, Value};
use std::{
    io::BufReader,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use veynodevm_api::{refuse, ErrorCode};

pub fn serve(ctx: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
    let options = McpOptions {
        lifecycle: flag(&args, "lifecycle")?,
        execute: flag(&args, "execute")?,
        files: flag(&args, "files")?,
        input: flag(&args, "input")?,
        browser: flag(&args, "browser")?,
        clipboard: flag(&args, "clipboard")?,
        vm: opt_string(&args, "vm")?.map(str::to_owned),
    };
    let groups = match &args["tools"] {
        Value::Null => tool_groups::default(),
        Value::Array(names) => {
            let names = names
                .iter()
                .map(|name| name.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| refuse(ErrorCode::Invalid, "tools must be group names"))?;
            tool_groups::parse(&names.join(","))
                .map_err(|e| refuse(ErrorCode::Invalid, e.to_string()))?
        }
        _ => return Err(refuse(ErrorCode::Invalid, "tools must be group names")),
    };
    let gone = io.gone();
    crate::mcp_session(
        &ctx.state_dir,
        &options,
        groups,
        BufReader::new(io.reader()),
        io.output(),
        move |wait| {
            let until = Instant::now() + wait;
            loop {
                if gone.load(Ordering::SeqCst) {
                    return true;
                }
                if Instant::now() >= until {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(25).min(wait));
            }
        },
    )?;
    Ok(json!({}))
}
