//! Pushed events for one MCP session: `notifications/progress` while a tool
//! call runs, and resources (VM status, screen, serial and worker logs, async
//! jobs) that a client reads with `resources/read` and watches with
//! `resources/subscribe`, receiving `notifications/resources/updated`.
//!
//! The daemon has no push channel, and an adapter must not hold one of its
//! 32 connections indefinitely, so this session watches with short calls: a
//! 500 ms tick reads the daemon's lifecycle event log (`events`), log sizes
//! (`log_read`) and subscribed jobs (`exec_poll`); each subscribed screen has
//! one thread looping over a 5 s `screen_wait`. Polling tools keep working.
use crate::{call, validate_id, write_json};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// Subscriptions one session may hold, of which screens (each a watcher
/// holding a daemon connection most of the time) are a small part.
pub const MAX_SUBSCRIPTIONS: usize = 32;
pub const MAX_SCREEN_SUBSCRIPTIONS: usize = 2;
/// Longest `vm_exec_poll` `wait_ms`, inside the HTTPS 40 s exchange bound.
pub const MAX_POLL_WAIT_MS: u64 = 30_000;
const TICK: Duration = Duration::from_millis(500);
const LOG_TICKS: u32 = 2;
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
const SCREEN_WAIT_MS: u64 = 5_000;
const SCREEN_SETTLE_MS: u64 = 750;
const SCREEN_MIN_INTERVAL: Duration = Duration::from_secs(1);
const PREFIX: &str = "veynode://vm/";
pub const EVENT_META: &str = "veynode.dev/event";

pub const INSTRUCTIONS: &str = "Events: tools/call with _meta.progressToken receives notifications/progress about once a second until its result. vm_exec_poll takes wait_ms (up to 30000) to wait for the job instead of polling. Resources veynode://vm/{id}/status, /screen, /log/serial, /log/worker and /job/{job_id} can be read and subscribed; notifications/resources/updated carries _meta[\"veynode.dev/event\"]: status started, stopped, crashed, guest_reset or state; screen changed then settled (at most one change a second); log appended with its size; job finished with exit_code (Guest output arrives with the exit, read the job resource or poll for it). Clients without notifications keep using vm_status, vm_screen_wait and vm_exec_poll.";

/// The session's single writer: responses and notifications are whole frames.
#[derive(Clone)]
pub struct Out(Arc<Mutex<Box<dyn Write + Send>>>);
impl Out {
    pub fn new(sink: impl Write + Send + 'static) -> Self {
        Self(Arc::new(Mutex::new(Box::new(sink))))
    }
    pub fn send(&self, value: &Value) -> Result<()> {
        write_json(&mut *self.0.lock().unwrap(), value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Resource {
    Status(String),
    Screen(String),
    Log(String, &'static str),
    Job(String, String),
}
impl Resource {
    pub fn parse(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix(PREFIX)?;
        let parts: Vec<&str> = rest.split('/').collect();
        let id = parts.first()?.to_string();
        validate_id(&id).ok()?;
        match parts[1..] {
            ["status"] => Some(Self::Status(id)),
            ["screen"] => Some(Self::Screen(id)),
            ["log", "serial"] => Some(Self::Log(id, "serial")),
            ["log", "worker"] => Some(Self::Log(id, "worker")),
            ["job", job]
                if job.len() == 32 && job.bytes().all(|b| b"0123456789abcdef".contains(&b)) =>
            {
                Some(Self::Job(id, job.into()))
            }
            _ => None,
        }
    }
    pub fn vm(&self) -> &str {
        match self {
            Self::Status(id) | Self::Screen(id) | Self::Log(id, _) | Self::Job(id, _) => id,
        }
    }
    pub fn uri(&self) -> String {
        match self {
            Self::Status(id) => format!("{PREFIX}{id}/status"),
            Self::Screen(id) => format!("{PREFIX}{id}/screen"),
            Self::Log(id, stream) => format!("{PREFIX}{id}/log/{stream}"),
            Self::Job(id, job) => format!("{PREFIX}{id}/job/{job}"),
        }
    }
}
/// The VM a resource URI names, for transports that authorize by VM.
pub fn resource_vm(uri: &str) -> Option<String> {
    Resource::parse(uri).map(|r| r.vm().to_string())
}

/// Adds the event capabilities and instructions to an `initialize` reply.
pub fn initialize(reply: &mut Value) {
    reply["result"]["capabilities"]["resources"] = json!({"subscribe":true,"listChanged":false});
    let instructions = reply["result"]["instructions"].as_str().unwrap_or("");
    reply["result"]["instructions"] = json!(format!("{instructions} {INSTRUCTIONS}"));
}
/// `wait_ms` on `vm_exec_poll`: the only tool-list change events need.
pub fn extend_tools(tools: &mut [Value]) {
    for tool in tools.iter_mut().filter(|t| t["name"] == "vm_exec_poll") {
        tool["inputSchema"]["properties"]["wait_ms"] = json!({"type":"integer","minimum":0,"maximum":MAX_POLL_WAIT_MS,"description":"Wait up to this long for the job to finish before replying (default 0: reply at once)."});
        tool["description"] = json!(format!(
            "{} wait_ms waits for completion instead of polling.",
            tool["description"].as_str().unwrap_or("")
        ));
    }
}
/// `vm_exec_poll` with `wait_ms`: polls the daemon until the job is no longer
/// running or the wait ends, and returns the last reply unchanged.
pub fn exec_poll_wait(dir: &Path, mut args: Value) -> Result<Value> {
    let wait = args
        .as_object_mut()
        .and_then(|a| a.remove("wait_ms"))
        .map_or(Some(0), |w| w.as_u64())
        .filter(|w| *w <= MAX_POLL_WAIT_MS)
        .context("wait_ms must be 0..30000")?;
    let deadline = Instant::now() + Duration::from_millis(wait);
    loop {
        let reply = call(dir, "exec_poll", args.clone())?;
        let now = Instant::now();
        if reply["result"]["state"] != "running" || now >= deadline {
            return Ok(reply);
        }
        thread::sleep((deadline - now).min(Duration::from_millis(200)));
    }
}

/// Heartbeat `notifications/progress` for one request that carries a
/// progress token. `finish` stops it before the response is written, so no
/// progress follows the result.
pub struct Progress {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Progress {
    pub fn start(out: &Out, request: &Value, label: &str) -> Option<Self> {
        let token = request["params"]["_meta"]["progressToken"].clone();
        if !(token.is_string() || token.is_i64() || token.is_u64()) {
            return None;
        }
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let (out, flag, label) = (out.clone(), stop.clone(), label.to_string());
        let thread = thread::spawn(move || {
            let started = Instant::now();
            let (lock, wake) = &*flag;
            let mut stopped = lock.lock().unwrap();
            loop {
                stopped = wake.wait_timeout(stopped, PROGRESS_INTERVAL).unwrap().0;
                if *stopped {
                    return;
                }
                let elapsed = started.elapsed().as_millis() as u64;
                let message = format!("{label} running for {} s", elapsed / 1000);
                // Written under the stop lock: `finish` cannot return while a
                // progress frame for this request is still being written.
                if out
                    .send(&json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":token,"progress":elapsed,"message":message}}))
                    .is_err()
                {
                    return;
                }
            }
        });
        Some(Self {
            stop,
            thread: Some(thread),
        })
    }
    pub fn finish(mut self) {
        self.halt();
    }
    fn halt(&mut self) {
        *self.stop.0.lock().unwrap() = true;
        self.stop.1.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for Progress {
    fn drop(&mut self) {
        self.halt();
    }
}

#[derive(Default)]
struct Subscriptions {
    status: BTreeSet<String>,
    /// Last seen size per (VM, stream).
    logs: BTreeMap<(String, &'static str), Option<u64>>,
    jobs: BTreeSet<(String, String)>,
    screens: BTreeMap<String, Arc<AtomicBool>>,
    cursor: Option<u64>,
    pump: bool,
}
impl Subscriptions {
    fn len(&self) -> usize {
        self.status.len() + self.logs.len() + self.jobs.len() + self.screens.len()
    }
}
struct Shared {
    dir: PathBuf,
    out: Out,
    stop: AtomicBool,
    subs: Mutex<Subscriptions>,
}
pub struct Session {
    shared: Arc<Shared>,
    execute: bool,
}
fn updated(out: &Out, uri: &str, event: Value) -> Result<()> {
    out.send(&json!({"jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":uri,"_meta":{EVENT_META:event}}}))
}
fn wrapped(reply: &Value) -> &Value {
    if reply.get("result").is_some() {
        &reply["result"]
    } else {
        reply
    }
}
impl Session {
    pub fn new(dir: &Path, out: Out, execute: bool) -> Self {
        Self {
            shared: Arc::new(Shared {
                dir: dir.to_path_buf(),
                out,
                stop: AtomicBool::new(false),
                subs: Mutex::new(Subscriptions::default()),
            }),
            execute,
        }
    }
    /// Handles a `resources/*` request: `Some(result or error message)`, or
    /// `None` when the method is not a resources method.
    pub fn handle(&self, method: &str, params: &Value) -> Option<Result<Value>> {
        let result = match method {
            "resources/list" => self.list(),
            "resources/templates/list" => Ok(self.templates()),
            "resources/read" => self.parse(params).and_then(|r| self.read(&r)),
            "resources/subscribe" => self.parse(params).and_then(|r| self.subscribe(r)),
            "resources/unsubscribe" => self.parse(params).map(|r| self.unsubscribe(&r)),
            _ => return None,
        };
        Some(result)
    }
    fn parse(&self, params: &Value) -> Result<Resource> {
        let resource = params["uri"]
            .as_str()
            .and_then(Resource::parse)
            .context("unknown resource URI")?;
        if matches!(resource, Resource::Job(..)) && !self.execute {
            bail!("job resources require execute permission")
        }
        Ok(resource)
    }
    fn list(&self) -> Result<Value> {
        let vms = call(&self.shared.dir, "list", json!({}))?;
        let mut resources = Vec::new();
        for vm in vms.as_array().into_iter().flatten().take(256) {
            let Some(id) = vm["config"]["id"].as_str() else {
                continue;
            };
            for (resource, mime, title) in [
                (Resource::Status(id.into()), "application/json", "status"),
                (Resource::Screen(id.into()), "image/png", "screen"),
                (
                    Resource::Log(id.into(), "serial"),
                    "text/plain",
                    "serial console log",
                ),
                (
                    Resource::Log(id.into(), "worker"),
                    "text/plain",
                    "worker log",
                ),
            ] {
                resources.push(
                    json!({"uri":resource.uri(),"name":format!("{id} {title}"),"mimeType":mime}),
                );
            }
        }
        Ok(json!({"resources":resources}))
    }
    fn templates(&self) -> Value {
        let mut templates = vec![
            json!({"uriTemplate":"veynode://vm/{id}/status","name":"VM status","mimeType":"application/json","description":"vm_status as a resource; subscribe for started, stopped, crashed, guest_reset and state events."}),
            json!({"uriTemplate":"veynode://vm/{id}/screen","name":"VM screen","description":"The current screenshot; subscribe for changed and settled events."}),
            json!({"uriTemplate":"veynode://vm/{id}/log/serial","name":"Serial console log","mimeType":"text/plain","description":"Last 64 KiB, control characters and token-like runs removed; subscribe for appended events."}),
            json!({"uriTemplate":"veynode://vm/{id}/log/worker","name":"Worker log","mimeType":"text/plain","description":"QEMU worker diagnostics, last 64 KiB, host paths and token-like runs removed; subscribe for appended events."}),
        ];
        if self.execute {
            templates.push(json!({"uriTemplate":"veynode://vm/{id}/job/{job_id}","name":"Async job","mimeType":"application/json","description":"vm_exec_poll as a resource; subscribe for the finished event with exit_code."}));
        }
        json!({"resourceTemplates":templates})
    }
    fn read(&self, resource: &Resource) -> Result<Value> {
        let dir = &self.shared.dir;
        let uri = resource.uri();
        let id = resource.vm();
        Ok(match resource {
            Resource::Status(_) => {
                let status = call(dir, "status", json!({"id":id}))?;
                json!({"contents":[{"uri":uri,"mimeType":"application/json","text":status.to_string()}]})
            }
            Resource::Screen(_) => {
                let mut shot = call(dir, "screenshot", json!({"id":id}))?;
                let data = shot
                    .as_object_mut()
                    .and_then(|s| s.remove("data_base64"))
                    .context("screenshot missing image data")?;
                json!({"contents":[{"uri":uri,"mimeType":shot["mime_type"],"blob":data},{"uri":uri,"mimeType":"application/json","text":shot.to_string()}]})
            }
            Resource::Log(_, stream) => {
                let log = call(dir, "log_read", json!({"id":id,"stream":stream}))?;
                json!({"contents":[{"uri":uri,"mimeType":"text/plain","text":log["text"].as_str().unwrap_or(""),"_meta":{"veynode.dev/log":{"size":log["size"],"truncated":log["truncated"]}}}]})
            }
            Resource::Job(_, job) => {
                let reply = call(dir, "exec_poll", json!({"id":id,"job_id":job}))?;
                json!({"contents":[{"uri":uri,"mimeType":"application/json","text":reply.to_string()}]})
            }
        })
    }
    fn subscribe(&self, resource: Resource) -> Result<Value> {
        let dir = &self.shared.dir;
        // Fail on an unknown VM now rather than watching nothing.
        if !matches!(resource, Resource::Job(..)) {
            call(dir, "status", json!({"id":resource.vm()}))?;
        }
        let mut subs = self.shared.subs.lock().unwrap();
        let present = match &resource {
            Resource::Status(id) => subs.status.contains(id),
            Resource::Screen(id) => subs.screens.contains_key(id),
            Resource::Log(id, stream) => subs.logs.contains_key(&(id.clone(), *stream)),
            Resource::Job(id, job) => subs.jobs.contains(&(id.clone(), job.clone())),
        };
        if present {
            return Ok(json!({}));
        }
        if subs.len() >= MAX_SUBSCRIPTIONS {
            bail!("subscription limit reached ({MAX_SUBSCRIPTIONS} per session)")
        }
        match resource {
            Resource::Status(id) => {
                if subs.cursor.is_none() {
                    subs.cursor = call(dir, "events", json!({}))?["next"]
                        .as_u64()
                        .map(|n| n.saturating_sub(1));
                }
                subs.status.insert(id);
            }
            Resource::Log(id, stream) => {
                let size = call(
                    dir,
                    "log_read",
                    json!({"id":id,"stream":stream,"max_bytes":0}),
                )
                .ok()
                .and_then(|l| l["size"].as_u64());
                subs.logs.insert((id, stream), size);
            }
            Resource::Job(id, job) => {
                // Unknown and expired jobs are refused; a finished one is
                // reported on the next tick.
                call(dir, "exec_poll", json!({"id":id,"job_id":job}))?;
                subs.jobs.insert((id, job));
            }
            Resource::Screen(id) => {
                if subs.screens.len() >= MAX_SCREEN_SUBSCRIPTIONS {
                    bail!("screen subscription limit reached ({MAX_SCREEN_SUBSCRIPTIONS} per session)")
                }
                let active = Arc::new(AtomicBool::new(true));
                subs.screens.insert(id.clone(), active.clone());
                let shared = self.shared.clone();
                thread::spawn(move || watch_screen(&shared, &id, &active));
            }
        }
        if !subs.pump {
            subs.pump = true;
            let shared = self.shared.clone();
            thread::spawn(move || pump(&shared));
        }
        Ok(json!({}))
    }
    fn unsubscribe(&self, resource: &Resource) -> Value {
        let mut subs = self.shared.subs.lock().unwrap();
        match resource {
            Resource::Status(id) => {
                subs.status.remove(id);
            }
            Resource::Log(id, stream) => {
                subs.logs.remove(&(id.clone(), *stream));
            }
            Resource::Job(id, job) => {
                subs.jobs.remove(&(id.clone(), job.clone()));
            }
            Resource::Screen(id) => {
                if let Some(active) = subs.screens.remove(id) {
                    active.store(false, Ordering::Release);
                }
            }
        }
        json!({})
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        for active in self.shared.subs.lock().unwrap().screens.values() {
            active.store(false, Ordering::Release);
        }
    }
}

fn pump(shared: &Shared) {
    let mut ticks = 0u32;
    while !shared.stop.load(Ordering::Acquire) {
        thread::sleep(TICK);
        ticks = ticks.wrapping_add(1);
        if tick(shared, ticks.is_multiple_of(LOG_TICKS)).is_err() {
            // The client is gone; the session ends on its own.
            return;
        }
    }
}
/// One observation pass. Daemon failures are skipped (the next tick retries);
/// only a failed write to the client is an error.
fn tick(shared: &Shared, logs: bool) -> Result<()> {
    let dir = &shared.dir;
    let (status, cursor, jobs, log_subs) = {
        let subs = shared.subs.lock().unwrap();
        (
            subs.status.clone(),
            subs.cursor,
            subs.jobs.clone(),
            if logs {
                subs.logs.clone()
            } else {
                BTreeMap::new()
            },
        )
    };
    if let (false, Some(after)) = (status.is_empty(), cursor) {
        if let Ok(reply) = call(dir, "events", json!({"after":after})) {
            if reply["lost"] == true {
                for id in &status {
                    updated(
                        &shared.out,
                        &Resource::Status(id.clone()).uri(),
                        json!({"event":"lost","reason":"events were dropped before this session read them; read the resource"}),
                    )?;
                }
            }
            for event in reply["events"].as_array().into_iter().flatten() {
                if let Some(id) = event["vm"].as_str().filter(|id| status.contains(*id)) {
                    updated(
                        &shared.out,
                        &Resource::Status(id.into()).uri(),
                        event.clone(),
                    )?;
                }
            }
            if let Some(next) = reply["next"].as_u64() {
                shared.subs.lock().unwrap().cursor = Some(next.saturating_sub(1));
            }
        }
    }
    for (id, job) in jobs {
        let resource = Resource::Job(id.clone(), job.clone());
        let event = match call(dir, "exec_poll", json!({"id":id,"job_id":job})) {
            Ok(reply) if reply["result"]["state"] == "running" => continue,
            Ok(reply) => {
                let result = &reply["result"]["result"];
                let mut event = json!({"event":"finished"});
                for key in [
                    "exit_code",
                    "timed_out",
                    "cancelled",
                    "stdout_truncated",
                    "stderr_truncated",
                ] {
                    if let Some(value) = result.get(key) {
                        event[key] = value.clone();
                    }
                }
                if reply["result"].get("error").is_some() {
                    event["error"] = json!(true);
                }
                event
            }
            Err(error) if error.to_string().contains("unknown or expired") => {
                json!({"event":"expired"})
            }
            Err(_) => continue,
        };
        if shared.subs.lock().unwrap().jobs.remove(&(id, job)) {
            updated(&shared.out, &resource.uri(), event)?;
        }
    }
    for ((id, stream), last) in log_subs {
        let Ok(size) = call(
            dir,
            "log_read",
            json!({"id":id,"stream":stream,"max_bytes":0}),
        )
        .map(|l| l["size"].as_u64()) else {
            continue;
        };
        if size == last {
            continue;
        }
        let key = (id.clone(), stream);
        let mut subs = shared.subs.lock().unwrap();
        if let Some(entry) = subs.logs.get_mut(&key) {
            *entry = size;
            drop(subs);
            if last.is_some() || size.is_some_and(|s| s > 0) {
                updated(
                    &shared.out,
                    &Resource::Log(id, stream).uri(),
                    json!({"event":"appended","size":size}),
                )?;
            }
        }
    }
    Ok(())
}
/// Waits for screen changes of one VM while subscribed: a change is reported
/// at most once a second, then `settled` once the screen has been still for
/// 750 ms. Changes during the short gaps between waits can be missed.
fn watch_screen(shared: &Shared, id: &str, active: &AtomicBool) {
    let uri = Resource::Screen(id.into()).uri();
    let running = || active.load(Ordering::Acquire) && !shared.stop.load(Ordering::Acquire);
    let pause = |duration: Duration| {
        let until = Instant::now() + duration;
        while running() && Instant::now() < until {
            thread::sleep(Duration::from_millis(100));
        }
    };
    let mut last: Option<Instant> = None;
    while running() {
        let reply = match call(
            &shared.dir,
            "screen_wait",
            json!({"id":id,"timeout_ms":SCREEN_WAIT_MS}),
        ) {
            Ok(reply) => reply,
            Err(_) => {
                pause(Duration::from_secs(5));
                continue;
            }
        };
        let reply = wrapped(&reply);
        if reply["changed"] != true {
            continue;
        }
        if let Some(previous) = last {
            pause(SCREEN_MIN_INTERVAL.saturating_sub(previous.elapsed()));
        }
        if !running() {
            return;
        }
        last = Some(Instant::now());
        if updated(
            &shared.out,
            &uri,
            json!({"event":"changed","resized":reply["resized"] == true}),
        )
        .is_err()
        {
            return;
        }
        let started = Instant::now();
        while running() && started.elapsed() < Duration::from_secs(30) {
            match call(
                &shared.dir,
                "screen_wait",
                json!({"id":id,"timeout_ms":SCREEN_SETTLE_MS}),
            ) {
                Ok(still) if wrapped(&still)["changed"] != true => {
                    if updated(
                        &shared.out,
                        &uri,
                        json!({"event":"settled","after_ms":started.elapsed().as_millis() as u64}),
                    )
                    .is_err()
                    {
                        return;
                    }
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_uris_round_trip_and_reject_traversal() {
        for uri in [
            "veynode://vm/a/status",
            "veynode://vm/a/screen",
            "veynode://vm/a/log/serial",
            "veynode://vm/a/log/worker",
            &format!("veynode://vm/a/job/{}", "0".repeat(32)),
        ] {
            assert_eq!(Resource::parse(uri).unwrap().uri(), uri);
            assert_eq!(resource_vm(uri).as_deref(), Some("a"));
        }
        for uri in [
            "veynode://vm/../status",
            "veynode://vm/a/log/other",
            "veynode://vm/a/status/x",
            "veynode://vm/a/job/ABC",
            "file:///etc/passwd",
            "veynode://vm//status",
        ] {
            assert!(Resource::parse(uri).is_none(), "{uri}");
        }
    }

    #[test]
    fn exec_poll_gains_only_an_optional_wait() {
        let mut tools = vec![
            json!({"name":"vm_exec_poll","description":"Poll.","inputSchema":{"properties":{"id":{},"job_id":{}},"required":["id","job_id"]}}),
            json!({"name":"vm_status","inputSchema":{"properties":{"id":{}}}}),
        ];
        extend_tools(&mut tools);
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["wait_ms"]["maximum"],
            MAX_POLL_WAIT_MS
        );
        assert_eq!(tools[0]["inputSchema"]["required"], json!(["id", "job_id"]));
        assert!(tools[1]["inputSchema"]["properties"]
            .get("wait_ms")
            .is_none());
    }

    #[test]
    fn progress_stops_before_the_response() {
        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        let out = Out::new(sink.clone());
        assert!(Progress::start(&out, &json!({"params":{}}), "x").is_none());
        let progress = Progress::start(
            &out,
            &json!({"params":{"_meta":{"progressToken":7}}}),
            "vm_exec",
        )
        .unwrap();
        thread::sleep(Duration::from_millis(2300));
        progress.finish();
        let written = sink.0.lock().unwrap().len();
        thread::sleep(Duration::from_millis(1200));
        assert_eq!(sink.0.lock().unwrap().len(), written);
        let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        let frames: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["params"]["progressToken"], 7);
        assert!(
            frames[1]["params"]["progress"].as_u64() > frames[0]["params"]["progress"].as_u64()
        );
    }
}
