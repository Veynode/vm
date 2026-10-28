//! Guest command execution: identity, timeouts and the synchronous path.
use super::*;

pub(crate) const SYNC_EXEC_MAX_TIMEOUT_MS: u64 = 30_000;
pub(crate) const ASYNC_EXEC_MAX_TIMEOUT_MS: u64 = 120_000;
pub(crate) fn completed_exec_result(reply: &Value) -> Result<Option<Value>> {
    match reply["result"]["state"].as_str() {
        Some("running") => Ok(None),
        Some("finished") => {
            if let Some(error) = reply["result"].get("error") {
                bail!("guest command failed: {error}")
            }
            let result = &reply["result"]["result"];
            if !result.is_object()
                || !result["exit_code"].is_i64()
                || !result["cancelled"].is_boolean()
            {
                bail!("guest returned invalid terminal command result")
            }
            Ok(Some(json!({"id":reply["id"],"result":result})))
        }
        _ => bail!("guest returned invalid job state"),
    }
}
/// Unicode text: validated and chunked on the host, delivered by the Guest service. The first
/// failure ends the call with exact delivery accounting; nothing is retried or dropped silently.
/// The text is never put into an error message or written anywhere by this function.
pub(crate) fn desktop_text(dir: &Path, mut arguments: Value, peer: &UnixStream) -> Result<Value> {
    let plan = input::plan_text(&arguments)?;
    // Mandatory: a Guest service without text_input would fence the execution transport.
    let info = call(dir, "guest_info", json!({"id":arguments["id"]}))?;
    let mut random = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    arguments["operation_id"] = json!(random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>());
    // True once the worker may hold this operation: a chunk was admitted, or its fate is unknown.
    let mut began = false;
    let result = input::deliver_text(&plan, &info, |index, chunk| {
        if peer_disconnected(&[peer.as_raw_fd()]) {
            return Ok(input::host_refusal("input client disconnected"));
        }
        arguments["begin"] = json!(index == 0);
        arguments["index"] = json!(index);
        arguments["text"] = json!(chunk);
        let reply = call_reply(dir, input::TEXT_STEP_METHOD, arguments.clone());
        arguments["text"] = Value::Null;
        thread::sleep(Duration::from_millis(20));
        match reply {
            Ok(Ok(reply)) => {
                began = true;
                Ok(reply)
            }
            // The daemon or the worker answered with a refusal: this chunk was not sent. A lost
            // worker answer is the one refusal that leaves the chunk's fate unknown.
            Ok(Err(message)) if !message.starts_with("backend response unavailable") => {
                Ok(input::host_refusal(&message))
            }
            Ok(Err(message)) => {
                began = true;
                bail!("{message}")
            }
            Err(error) => {
                began = true;
                Err(error)
            }
        }
    });
    if began {
        if let Err(error) = call(dir, "input_cleanup", arguments) {
            bail!(
                "input cleanup unconfirmed: {error}; text result: {}",
                match &result {
                    Ok(_) => "delivered".to_string(),
                    Err(error) => error.to_string(),
                }
            )
        }
    }
    Ok(result?)
}
pub(crate) fn desktop_input(
    dir: &Path,
    method: &str,
    mut arguments: Value,
    evdev: bool,
    peer: &UnixStream,
) -> Result<Value> {
    if method == "input_text" {
        return desktop_text(dir, arguments, peer);
    }
    let mut steps = input::plan(method, &arguments)?;
    let first_gap = input::first_gap_ms(method, &arguments)?;
    if evdev {
        // Apple Virtualization Linux: the guest agent drives uinput devices.
        steps = steps
            .iter()
            .map(|events| desktop_input::planned_evdev(events))
            .collect::<Result<_>>()?;
    }
    let mut random = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    arguments["operation_id"] = json!(random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>());
    let mut sent = 0;
    let result = (|| {
        for (index, events) in steps.iter().enumerate() {
            if peer_disconnected(&[peer.as_raw_fd()]) {
                bail!("input client disconnected")
            }
            if events.is_empty() {
                continue; // a wheel release has no evdev event
            }
            arguments["begin"] = json!(index == 0);
            arguments["events"] = json!(events);
            call(dir, "input_step", arguments.clone())?;
            sent += 1;
            if index + 1 < steps.len() {
                let gap = if index == 0 { first_gap } else { 20 };
                thread::sleep(Duration::from_millis(gap));
            }
        }
        Ok(())
    })();
    // Releases are safety cleanup, not new user input. An opaque operation token prevents
    // delayed cleanup from releasing a later client's buttons or keys.
    let cleanup = call(dir, "input_cleanup", arguments);
    if let Err(error) = cleanup {
        bail!("input cleanup unconfirmed after {sent} steps: {error}; operation result: {result:?}")
    }
    result?;
    Ok(
        json!({"dispatched":true,"steps":sent,"guest_delivery_confirmed":false,"note":"Backend dispatch acknowledgement is not application acceptance; physical keys use Guest layout."}),
    )
}

/// The optional `user`, `env` and `workdir` of an exec (`veynodevm exec -u -e -w`),
/// checked here and applied by the Linux Guest inside the container root.
pub(crate) fn exec_identity(args: &Value) -> Result<Vec<(&'static str, Value)>> {
    let mut fields = Vec::new();
    if let Some(user) = args.get("user").filter(|v| !v.is_null()) {
        let user = user.as_str().context("user must be a string")?;
        if user.is_empty() || user.len() > 256 || user.contains(['\0', '\n']) {
            bail!("user must be 1..256 bytes")
        }
        fields.push(("user", json!(user)));
    }
    if let Some(env) = args.get("env").filter(|v| !v.is_null()) {
        let env = env
            .as_array()
            .context("env must be an array of KEY=VALUE")?;
        if env.len() > 128
            || env.iter().any(|entry| {
                entry.as_str().is_none_or(|e| {
                    e.len() > 8192
                        || e.contains('\0')
                        || e.split_once('=').is_none_or(|(k, _)| k.is_empty())
                })
            })
        {
            bail!("env must be up to 128 KEY=VALUE strings")
        }
        if !env.is_empty() {
            fields.push(("env", json!(env)));
        }
    }
    if let Some(workdir) = args.get("workdir").filter(|v| !v.is_null()) {
        let workdir = workdir.as_str().context("workdir must be a string")?;
        if !workdir.starts_with('/') || workdir.len() > 4096 || workdir.contains('\0') {
            bail!("workdir must be an absolute path")
        }
        fields.push(("workdir", json!(workdir)));
    }
    Ok(fields)
}

pub(crate) fn exec_timeout_ms(arguments: &Value, maximum: u64) -> Result<u64> {
    let timeout = arguments
        .get("timeout_ms")
        .map(|value| value.as_u64().context("invalid timeout_ms"))
        .transpose()?
        .unwrap_or(5000);
    if !(1..=maximum).contains(&timeout) {
        bail!("timeout_ms must be 1..{maximum}")
    }
    Ok(timeout)
}
// Only this already nonce-bound, non-reconnecting transport has a Guest service
// lifetime matching Running. Vsock Guests can restart in the same VM process;
// their last info reply cannot safely authorize a later longer command.
pub(crate) fn advertised_async_timeout(info: &Value, qemu_exec: bool) -> u64 {
    if !qemu_exec
        || info["os"] != "linux"
        || info["transport"] != "virtio-serial-private-exec-v2"
        || info["transport_reconnect"] != false
        || info["execution_containment"] != "cgroup-v2-clone3-kill"
        || info["execution_identity"]["rootless"] != true
        || info["capabilities"]["exec"] != true
        || info["capabilities"]["async_exec"] != true
        || info["capabilities"]["descendant_containment"] != true
        || ["files", "shutdown", "desktop"]
            .iter()
            .any(|key| info["capabilities"][key] != false)
    {
        return SYNC_EXEC_MAX_TIMEOUT_MS;
    }
    info["max_async_timeout_ms"]
        .as_u64()
        .filter(|maximum| *maximum > 0)
        .map_or(SYNC_EXEC_MAX_TIMEOUT_MS, |maximum| {
            maximum.min(ASYNC_EXEC_MAX_TIMEOUT_MS)
        })
}
/// The pause after the `polls`-th `exec_poll` of a synchronous command that is
/// still running: 2, 4, 8, 16, 32 ms, then every 50 ms. A short command is
/// answered a few milliseconds after it ends; a long one costs no more polls
/// than a steady 50 ms. There is no server-side wait to block on: each poll
/// is answered by the Guest.
pub(crate) fn exec_poll_delay(polls: u32) -> Duration {
    Duration::from_millis((2u64 << polls.min(5)).min(50))
}
pub(crate) fn synchronous_exec(dir: &Path, arguments: Value, peer: &UnixStream) -> Result<Value> {
    // The shared asynchronous start route has a longer host ceiling. Do not
    // inherit that ceiling into the synchronous API or its 35-second waiter.
    exec_timeout_ms(&arguments, SYNC_EXEC_MAX_TIMEOUT_MS)?;
    // Every command uses the same registered Guest job path. Never fall back
    // to legacy uncancellable exec when an older Guest lacks asynchronous integration.
    let started = call(dir, "exec_start", arguments.clone()).map_err(|error| anyhow::anyhow!(
        "synchronous command start failed (Guest exec_start/exec_poll/exec_cancel support required): {error}"
    ))?;
    let job_id = started["result"]["job_id"]
        .as_str()
        .context("guest returned no job identity")?;
    let job = json!({"id":arguments["id"],"job_id":job_id});
    let deadline = std::time::Instant::now() + Duration::from_secs(35);
    let mut polls = 0;
    let result = (|| loop {
        if peer_disconnected(&[peer.as_raw_fd()]) {
            bail!("command client disconnected")
        }
        if std::time::Instant::now() >= deadline {
            bail!("synchronous command result deadline exceeded")
        }
        let reply = call(dir, "exec_poll", job.clone())?;
        if let Some(result) = completed_exec_result(&reply)? {
            return Ok(result);
        }
        thread::sleep(exec_poll_delay(polls));
        polls += 1;
    })();
    if let Err(error) = result {
        return match call(dir, "exec_cancel", job) {
            Ok(_) => Err(anyhow::anyhow!(
                "command wait failed; cancellation requested, not confirmed: {error}"
            )),
            Err(cancel) => Err(anyhow::anyhow!(
                "command wait failed: {error}; cancellation also failed: {cancel}"
            )),
        };
    }
    result
}

#[cfg(test)]
mod poll_tests {
    use super::*;

    #[test]
    fn synchronous_polls_start_fast_and_settle_at_50_ms() {
        let delays: Vec<u64> = (0..8)
            .map(|n| exec_poll_delay(n).as_millis() as u64)
            .collect();
        assert_eq!(delays, [2, 4, 8, 16, 32, 50, 50, 50]);
        assert_eq!(exec_poll_delay(u32::MAX), Duration::from_millis(50));
    }
}
