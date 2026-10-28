//! Direct-to-host HTTPS MCP. No hosted TLS termination, OAuth discovery, or relay E2EE is claimed.
use anyhow::{bail, Context, Result};
use axum::{
    body::Bytes,
    extract::Request,
    extract::{DefaultBodyLimit, Path as UrlPath, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    net::SocketAddr,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{mpsc, oneshot, Mutex, Notify, OnceCell, OwnedMutexGuard, Semaphore},
    task::JoinHandle,
    time::timeout,
};
use veynodevm_mcp::resource::{resource_vm, Resource};

/// Server-to-client notifications queued per session for its GET stream. A
/// slow or absent reader loses the oldest ones, never grows memory.
const MAX_QUEUED_EVENTS: usize = 256;
const MAX_QUEUED_EVENT_BYTES: usize = 1024 * 1024;
const MAX_EVENT_BYTES: usize = 64 * 1024;
/// Screen subscriptions across all HTTPS sessions: each one keeps a daemon
/// connection busy most of the time, and the daemon serves 32 at once.
const MAX_SCREEN_SUBSCRIPTIONS: usize = 8;
const KEEPALIVE: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default = "default_bind")]
    bind: SocketAddr,
    tls_cert: PathBuf,
    tls_key: PathBuf,
    #[serde(default)]
    allowed_origins: Vec<String>,
    principals: Vec<Principal>,
    #[serde(default = "default_ttl")]
    session_idle_seconds: u64,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Principal {
    name: String,
    token_sha256: String,
    scopes: Vec<String>,
    #[serde(default)]
    vm_ids: Vec<String>,
    /// Tool groups to list (`vm-control mcp --tools`); empty lists every permitted group.
    #[serde(default)]
    tools: Vec<String>,
}
fn default_bind() -> SocketAddr {
    "127.0.0.1:8443".parse().unwrap()
}
fn default_ttl() -> u64 {
    900
}
struct Session {
    principal: String,
    /// The VM of a `/vm/{id}/mcp` session; None for `/mcp`.
    vm: Option<String>,
    created: Instant,
    touched: Instant,
    io: Arc<Mutex<Bridge>>,
    lifecycle: Arc<Lifecycle>,
}
struct Lifecycle {
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    retired: AtomicBool,
    outcome: OnceCell<bool>,
    inbox: Arc<Inbox>,
    reader: std::sync::Mutex<Option<JoinHandle<()>>>,
}
/// Everything the MCP child writes, routed by one reader task: the response
/// to the single in-flight request (and its progress), and notifications the
/// principal may observe, queued for the session's GET stream.
#[derive(Default)]
struct Inbox {
    pending: std::sync::Mutex<Option<Pending>>,
    events: std::sync::Mutex<Queue>,
    changed: Notify,
    closed: AtomicBool,
}
struct Pending {
    id: Value,
    token: Option<Value>,
    progress: Option<mpsc::Sender<Value>>,
    response: oneshot::Sender<(Value, usize)>,
}
#[derive(Default)]
struct Queue {
    events: std::collections::VecDeque<String>,
    bytes: usize,
    dropped: u64,
    next_id: u64,
}
impl Queue {
    fn push(&mut self, event: String) {
        if event.len() > MAX_EVENT_BYTES {
            self.dropped += 1;
            return;
        }
        self.bytes += event.len();
        self.events.push_back(event);
        while self.events.len() > MAX_QUEUED_EVENTS || self.bytes > MAX_QUEUED_EVENT_BYTES {
            let old = self.events.pop_front().unwrap();
            self.bytes -= old.len();
            self.dropped += 1;
        }
    }
    /// The next SSE frame: a drop notice first when events were lost.
    fn pop(&mut self) -> Option<String> {
        let data = if self.dropped > 0 {
            let notice = json!({"jsonrpc":"2.0","method":"notifications/veynode/events_dropped","params":{"dropped":self.dropped}}).to_string();
            self.dropped = 0;
            notice
        } else {
            let event = self.events.pop_front()?;
            self.bytes -= event.len();
            event
        };
        self.next_id += 1;
        Some(sse_frame(Some(self.next_id), &data))
    }
}
fn sse_frame(id: Option<u64>, data: &str) -> String {
    match id {
        Some(id) => format!("id: {id}\nevent: message\ndata: {data}\n\n"),
        None => format!("event: message\ndata: {data}\n\n"),
    }
}
/// Whether a child notification may reach this principal: its own tool list
/// changing (after vm_tools_enable), or resource updates for an allowlisted VM.
/// Anything else from the child is dropped.
fn observable(principal: &Principal, notification: &Value) -> bool {
    notification["method"] == "notifications/tools/list_changed"
        || notification["method"] == "notifications/resources/updated"
            && notification["params"]["uri"]
                .as_str()
                .and_then(resource_vm)
                .is_some_and(|vm| principal.vm_ids.contains(&vm))
}
async fn read_child(stdout: ChildStdout, inbox: Arc<Inbox>, principal: Principal) {
    let mut reader = BufReader::new(stdout);
    let limit = veynodevm_api::MAX_LARGE_LINE + 1;
    let mut broken = false;
    loop {
        let mut line = Vec::new();
        let read = match (&mut reader)
            .take(limit as u64)
            .read_until(b'\n', &mut line)
            .await
        {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        // After a malformed frame keep draining, so the child can reach its
        // EOF cleanup, but route nothing: the stream is out of step.
        if broken {
            continue;
        }
        let value = match serde_json::from_slice::<Value>(&line) {
            Ok(value) if line.last() == Some(&b'\n') => value,
            _ => {
                broken = true;
                inbox.pending.lock().unwrap().take();
                continue;
            }
        };
        if value.get("method").is_some() {
            if value["method"] == "notifications/progress" {
                let pending = inbox.pending.lock().unwrap();
                if let Some(Pending {
                    token: Some(token),
                    progress: Some(progress),
                    ..
                }) = pending.as_ref()
                {
                    if value["params"]["progressToken"] == *token {
                        let _ = progress.try_send(value);
                    }
                }
            } else if observable(&principal, &value) {
                inbox.events.lock().unwrap().push(value.to_string());
                inbox.changed.notify_waiters();
            }
            continue;
        }
        let mut pending = inbox.pending.lock().unwrap();
        if pending.as_ref().is_some_and(|p| p.id == value["id"]) {
            let _ = pending.take().unwrap().response.send((value, read));
        }
    }
    inbox.closed.store(true, Ordering::Release);
    inbox.pending.lock().unwrap().take();
    inbox.changed.notify_waiters();
}
struct Bridge {
    lifecycle: Arc<Lifecycle>,
}
impl Bridge {
    async fn exchange(
        &mut self,
        request: &Value,
        progress: Option<mpsc::Sender<Value>>,
    ) -> Result<Option<Value>> {
        let mut bytes = serde_json::to_vec(request)?;
        bytes.push(b'\n');
        let inbox = &self.lifecycle.inbox;
        let response = if let Some(id) = request.get("id") {
            let (sender, receiver) = oneshot::channel();
            *inbox.pending.lock().unwrap() = Some(Pending {
                id: id.clone(),
                token: Some(request["params"]["_meta"]["progressToken"].clone())
                    .filter(|t| !t.is_null()),
                progress,
                response: sender,
            });
            Some(receiver)
        } else {
            None
        };
        {
            let mut stdin = self.lifecycle.stdin.lock().await;
            if self.lifecycle.retired.load(Ordering::Acquire)
                || inbox.closed.load(Ordering::Acquire)
            {
                bail!("session is retired")
            }
            let stdin = stdin.as_mut().context("session is closed")?;
            stdin.write_all(&bytes).await?;
            stdin.flush().await?;
        }
        let Some(response) = response else {
            return Ok(None);
        };
        let (response, read) = response.await.context("invalid stdio bridge response")?;
        if read > veynodevm_mcp::response_limit(request) {
            bail!("invalid stdio bridge response")
        }
        Ok(Some(response))
    }
}
impl Lifecycle {
    fn new(mut child: Child, principal: Principal) -> Arc<Self> {
        let inbox = Arc::new(Inbox::default());
        let reader = tokio::spawn(read_child(
            child.stdout.take().unwrap(),
            inbox.clone(),
            principal,
        ));
        Arc::new(Lifecycle {
            stdin: Mutex::new(child.stdin.take()),
            retired: AtomicBool::new(false),
            outcome: OnceCell::new(),
            child: Mutex::new(child),
            inbox,
            reader: std::sync::Mutex::new(Some(reader)),
        })
    }
    async fn close(&self) -> bool {
        *self
            .outcome
            .get_or_init(|| async {
                // Close stdin without waiting behind stdout/tool processing. The MCP
                // disconnect monitor retires authority and retains its first outcome.
                let eof = timeout(Duration::from_secs(1), async {
                    self.stdin.lock().await.take();
                })
                .await
                .is_ok();
                let mut child = self.child.lock().await;
                // The reader task keeps draining output no HTTP caller consumes,
                // so a full pipe cannot keep the MCP child from its EOF cleanup.
                let result = timeout(Duration::from_secs(42), child.wait()).await;
                self.inbox.changed.notify_waiters();
                let clean = match result {
                    Ok(Ok(status)) => eof && status.success(),
                    _ => {
                        // SIGKILL/reaping is resource cleanup, never Guest cleanup proof.
                        let _ = child.start_kill();
                        let _ = timeout(Duration::from_secs(1), child.wait()).await;
                        false
                    }
                };
                let reader = self.reader.lock().unwrap().take();
                if let Some(reader) = reader {
                    // A grandchild holding stdout open must not pin cleanup.
                    let _ = timeout(Duration::from_secs(1), reader).await;
                }
                clean
            })
            .await
    }
}

struct App {
    config: Config,
    state_dir: PathBuf,
    control_bin: PathBuf,
    sessions: Mutex<BTreeMap<String, Session>>,
    requests: Semaphore,
    closing: AtomicBool,
    cleanup_failed: AtomicBool,
    changed: Notify,
    /// Subscribed screen URIs per session, for MAX_SCREEN_SUBSCRIPTIONS.
    screens: std::sync::Mutex<BTreeMap<String, BTreeSet<String>>>,
}
impl App {
    // Spawned once, tracked in sessions until completion. Request cancellation,
    // DELETE timeout and TTL expiry cannot drop the last cleanup owner.
    fn retire(self: &Arc<Self>, id: String, lifecycle: Arc<Lifecycle>) {
        if lifecycle.retired.swap(true, Ordering::AcqRel) {
            return;
        }
        lifecycle.inbox.changed.notify_waiters();
        self.screens.lock().unwrap().remove(&id);
        let app = self.clone();
        tokio::spawn(async move {
            if !lifecycle.close().await {
                app.cleanup_failed.store(true, Ordering::Release);
            }
            let mut sessions = app.sessions.lock().await;
            if sessions
                .get(&id)
                .is_some_and(|s| Arc::ptr_eq(&s.lifecycle, &lifecycle))
            {
                sessions.remove(&id);
            }
            app.changed.notify_waiters();
        });
    }
    async fn shutdown(self: &Arc<Self>) -> Result<()> {
        self.closing.store(true, Ordering::Release);
        {
            let sessions = self.sessions.lock().await;
            for (id, session) in sessions.iter() {
                self.retire(id.clone(), session.lifecycle.clone());
            }
        }
        let settled = timeout(Duration::from_secs(45), async {
            loop {
                let changed = self.changed.notified();
                if self.sessions.lock().await.is_empty() {
                    break;
                }
                changed.await;
            }
        })
        .await
        .is_ok();
        if !settled || self.cleanup_failed.load(Ordering::Acquire) {
            bail!("MCP session cleanup unconfirmed; authority/Guest cleanup requests or child reaping failed")
        }
        Ok(())
    }
}
// Cancelling an HTTP future after writing a command must retire its session.
// Cleanup ownership transfers to a tracked task, not to the cancelled request.
struct ExchangeGuard {
    app: Arc<App>,
    id: String,
    lifecycle: Arc<Lifecycle>,
    armed: bool,
}
impl Drop for ExchangeGuard {
    fn drop(&mut self) {
        if self.armed {
            self.app.retire(self.id.clone(), self.lifecycle.clone());
        }
    }
}
fn private_file(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file()
        || m.file_type().is_symlink()
        || m.mode() & 0o077 != 0
        || m.uid() != unsafe { libc::geteuid() }
    {
        bail!(
            "{} must be an owner-only regular file (0600)",
            path.display()
        )
    }
    Ok(())
}
fn random_token() -> Result<String> {
    let mut b = [0u8; 32];
    File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|b| format!("{b:02x}")).collect())
}
fn authenticate(app: &App, headers: &HeaderMap) -> Option<Principal> {
    let token = headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    if token.len() < 32 || token.len() > 512 {
        return None;
    }
    let hash = Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    app.config
        .principals
        .iter()
        .find(|p| bool::from(p.token_sha256.as_bytes().ct_eq(hash.as_bytes())))
        .cloned()
}
fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error":message}))).into_response()
}
fn mime_list(header: Option<&HeaderValue>, mime: &str) -> bool {
    header.and_then(|h| h.to_str().ok()).is_some_and(|h| {
        h.split(',')
            .any(|x| x.trim().split(';').next() == Some(mime))
    })
}
/// `vm` is the VM of a per-VM session, whose child serves that VM only.
fn remote_authorized(principal: &Principal, vm: Option<&str>, request: &Value) -> bool {
    if let Some(vm) = vm {
        // The child has no resources and binds every call to its VM.
        return principal.vm_ids.iter().any(|allowed| allowed == vm)
            && !request["method"]
                .as_str()
                .is_some_and(|m| m.starts_with("resources/"));
    }
    if [
        "resources/read",
        "resources/subscribe",
        "resources/unsubscribe",
    ]
    .iter()
    .any(|m| request["method"] == *m)
    {
        return request["params"]["uri"]
            .as_str()
            .and_then(resource_vm)
            .is_some_and(|vm| principal.vm_ids.contains(&vm));
    }
    if request["method"] != "tools/call" {
        return true;
    }
    match request["params"]["name"].as_str().unwrap_or("") {
        "vm_create" => false,
        "vm_list" | "vm_capabilities" | "vm_tools_enable" => true,
        name if name.starts_with(veynodevm_mcp::cua::PREFIX) => request["params"]["arguments"]
            ["vm"]
            .as_str()
            .is_some_and(|id| principal.vm_ids.iter().any(|allowed| allowed == id)),
        _ => request["params"]["arguments"]["id"]
            .as_str()
            .is_some_and(|id| principal.vm_ids.iter().any(|allowed| allowed == id)),
    }
}
fn public_vm(value: &mut Value) {
    if let Some(config) = value["config"].as_object_mut() {
        config.retain(|key, _| {
            [
                "id",
                "guest",
                "backend",
                "cpus",
                "memory_mib",
                "window",
                "width",
                "height",
            ]
            .contains(&key.as_str())
        });
    }
    // Process identifiers and backend error detail are host diagnostics, not remote observation data.
    if let Some(value) = value.as_object_mut() {
        value.remove("pid");
        if let Some(runtime) = value.get_mut("runtime").and_then(Value::as_object_mut) {
            runtime.retain(|key, _| {
                [
                    "event",
                    "state",
                    "vm_id",
                    "window_open",
                    "installing",
                    "guest_ready",
                    "qmp_status",
                ]
                .contains(&key.as_str())
            });
        }
    }
}
fn filter_reply(principal: &Principal, request: &Value, mut reply: Value) -> Value {
    if request["method"] == "tools/list" {
        if let Some(tools) = reply["result"]["tools"].as_array_mut() {
            tools.retain(|tool| tool["name"] != "vm_create");
        }
    }
    if request["method"] == "resources/list" {
        if let Some(resources) = reply["result"]["resources"].as_array_mut() {
            resources.retain(|r| {
                r["uri"]
                    .as_str()
                    .and_then(resource_vm)
                    .is_some_and(|vm| principal.vm_ids.contains(&vm))
            });
        }
    }
    if request["method"] == "resources/read"
        && matches!(
            request["params"]["uri"].as_str().and_then(Resource::parse),
            Some(Resource::Status(_))
        )
    {
        // The status resource is vm_status: the same public projection applies.
        if let Some(contents) = reply["result"]["contents"].as_array_mut() {
            for content in contents {
                let public = content["text"]
                    .as_str()
                    .and_then(|t| serde_json::from_str::<Value>(t).ok())
                    .map(|mut value| {
                        public_vm(&mut value);
                        value.to_string()
                    });
                content["text"] = json!(public.unwrap_or_default());
            }
        }
    }
    let name = request["params"]["name"].as_str().unwrap_or("");
    if request["method"] == "tools/call" && name == "vm_tools_enable" {
        if let Some(text) = reply["result"]["content"][0]["text"].as_str() {
            if let Ok(mut value) = serde_json::from_str::<Value>(text) {
                if let Some(added) = value["added_tools"].as_array_mut() {
                    added.retain(|tool| tool != "vm_create");
                }
                reply["result"]["content"][0]["text"] = json!(value.to_string());
            }
        }
    }
    if request["method"] == "tools/call" && name.starts_with("vm_snapshot_") {
        // Snapshot receipts contain host paths, backend details and artifact
        // identities. Only the fixed public projection crosses this boundary.
        if reply["result"]["isError"] != false {
            reply = json!({"jsonrpc":"2.0","id":reply["id"],"result":{"isError":true,"content":[{"type":"text","text":"Snapshot operation refused or failed. Local diagnostics contain the detailed reason."}]}});
        } else if let Some(raw) = reply["result"]["content"][0]["text"].as_str() {
            let public = serde_json::from_str::<Value>(raw)
                .ok()
                .filter(|v| {
                    v.is_object()
                        || v.as_array()
                            .is_some_and(|rows| rows.iter().all(Value::is_object))
                })
                .map(|v| public_snapshot(&v));
            if let Some(public) = public {
                reply = json!({"jsonrpc":"2.0","id":reply["id"],"result":{"isError":false,"content":[{"type":"text","text":public.to_string()}]}});
            } else {
                reply = json!({"jsonrpc":"2.0","id":reply["id"],"result":{"isError":true,"content":[{"type":"text","text":"Invalid snapshot response."}]}});
            }
        } else {
            reply = json!({"jsonrpc":"2.0","id":reply["id"],"result":{"isError":true,"content":[{"type":"text","text":"Invalid snapshot response."}]}});
        }
    }
    if request["method"] == "tools/call"
        && ["vm_list", "vm_status", "vm_start"].contains(&name)
        && reply["result"]["isError"] == false
    {
        if let Some(text) = reply["result"]["content"][0]["text"].as_str() {
            if let Ok(mut value) = serde_json::from_str::<Value>(text) {
                if name == "vm_list" {
                    if let Some(list) = value.as_array_mut() {
                        list.retain(|vm| {
                            vm["config"]["id"].as_str().is_some_and(|id| {
                                principal.vm_ids.iter().any(|allowed| allowed == id)
                            })
                        });
                        for vm in list {
                            public_vm(vm);
                        }
                    }
                } else {
                    public_vm(&mut value);
                }
                reply["result"]["content"][0]["text"] = json!(value.to_string());
            }
        }
    }
    reply
}
fn public_snapshot(value: &Value) -> Value {
    match value {
        Value::Array(rows) => Value::Array(rows.iter().map(public_snapshot).collect()),
        Value::Object(fields) => {
            let mut result = serde_json::Map::new();
            for key in [
                "id",
                "vm",
                "name",
                "kind",
                "operation_id",
                "state",
                "phase",
                "created_unix_ms",
                "capture_verified",
                "restore_verified",
                "deleted",
                "completed",
                "requires_recovery",
            ] {
                if let Some(v) = fields.get(key) {
                    if v.is_string() || v.is_boolean() || v.is_u64() {
                        result.insert(key.into(), v.clone());
                    }
                }
            }
            for key in ["snapshots", "result"] {
                if let Some(v) = fields.get(key) {
                    result.insert(key.into(), public_snapshot(v));
                }
            }
            for key in [
                "reconciled",
                "restored",
                "source_resumed",
                "source_already_running",
                "source_running_confirmed",
                "source_resume_pending",
                "source_channel_released",
                "native_io_retirement_verified",
                "snapshot_result_verified",
            ] {
                if let Some(v) = fields.get(key).and_then(Value::as_bool) {
                    result.insert(key.into(), json!(v));
                }
            }
            if let Some(v) = fields
                .get("action")
                .and_then(Value::as_str)
                .filter(|v| matches!(*v, "create" | "restore" | "delete"))
            {
                result.insert("action".into(), json!(v));
            }
            if let Some(v) = fields
                .get("error_code")
                .and_then(Value::as_str)
                .filter(|v| {
                    !v.is_empty()
                        && v.len() <= 128
                        && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                })
            {
                result.insert("error_code".into(), json!(v));
            }
            if fields.contains_key("error") {
                result.insert(
                    "error".into(),
                    json!("Snapshot operation failed; inspect local diagnostics."),
                );
            }
            Value::Object(result)
        }
        _ => Value::Null,
    }
}
async fn admission(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    if app.closing.load(Ordering::Acquire) {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Server is shutting down");
    }
    let _permit = match app.requests.try_acquire() {
        Ok(p) => p,
        Err(_) => {
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many concurrent requests",
            )
        }
    };
    next.run(request).await
}
async fn endpoint(
    State(app): State<Arc<App>>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve(app, None, method, headers, body).await
}
/// `/vm/{id}/mcp`: one VM's cua-driver tools under their own names (`vm-control mcp --vm`).
async fn vm_endpoint(
    State(app): State<Arc<App>>,
    UrlPath(vm): UrlPath<String>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve(app, Some(vm), method, headers, body).await
}
async fn serve(
    app: Arc<App>,
    vm: Option<String>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let origin = headers.get("origin").and_then(|h| h.to_str().ok());
    if headers.contains_key("origin")
        && origin.is_none_or(|o| {
            !app.config
                .allowed_origins
                .iter()
                .any(|allowed| allowed == o)
        })
    {
        return error(StatusCode::FORBIDDEN, "Origin is not permitted");
    }
    let mut response = if method == Method::OPTIONS {
        if origin.is_none() {
            error(
                StatusCode::FORBIDDEN,
                "Preflight requires an allowed Origin",
            )
        } else {
            let mut r = StatusCode::NO_CONTENT.into_response();
            r.headers_mut().insert(
                "access-control-allow-methods",
                HeaderValue::from_static("POST, GET, DELETE, OPTIONS"),
            );
            r.headers_mut().insert(
                "access-control-allow-headers",
                HeaderValue::from_static(
                    "Authorization, Content-Type, Accept, MCP-Protocol-Version, MCP-Session-Id",
                ),
            );
            r
        }
    } else if let Some(principal) = authenticate(&app, &headers) {
        if vm
            .as_ref()
            .is_some_and(|vm| !principal.vm_ids.iter().any(|allowed| allowed == vm))
        {
            error(
                StatusCode::NOT_FOUND,
                "VM is not authorized for this principal",
            )
        } else {
            dispatch(&app, &principal, vm.as_deref(), &method, &headers, &body).await
        }
    } else {
        let mut r = error(StatusCode::UNAUTHORIZED, "Bearer authentication required");
        r.headers_mut().insert(
            "www-authenticate",
            HeaderValue::from_static("Bearer realm=\"veynode-vm\""),
        );
        r
    };
    if let Some(origin) = origin {
        if let Ok(value) = HeaderValue::from_str(origin) {
            response
                .headers_mut()
                .insert("access-control-allow-origin", value);
            response
                .headers_mut()
                .insert("vary", HeaderValue::from_static("Origin"));
            response.headers_mut().insert(
                "access-control-expose-headers",
                HeaderValue::from_static("MCP-Session-Id"),
            );
        }
    }
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}
async fn dispatch(
    app: &Arc<App>,
    principal: &Principal,
    vm: Option<&str>,
    method: &Method,
    headers: &HeaderMap,
    body: &[u8],
) -> Response {
    if ![Method::GET, Method::POST, Method::DELETE].contains(method) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let request: Option<Value> = if *method == Method::POST {
        if !mime_list(headers.get("content-type"), "application/json") {
            return error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type must be application/json",
            );
        }
        if !mime_list(headers.get("accept"), "application/json")
            || !mime_list(headers.get("accept"), "text/event-stream")
        {
            return error(
                StatusCode::NOT_ACCEPTABLE,
                "Accept must include application/json and text/event-stream",
            );
        }
        match serde_json::from_slice::<Value>(body) {
            Ok(v) if v.is_object() && v["jsonrpc"] == "2.0" => Some(v),
            _ => return error(StatusCode::BAD_REQUEST, "Expected one JSON-RPC 2.0 message"),
        }
    } else {
        None
    };
    if let Some(message) = &request {
        let id = message.get("id");
        if id.is_some_and(|v| !v.is_string() && !v.is_i64() && !v.is_u64())
            || (!message.get("method").is_some_and(Value::is_string)
                && !(id.is_some()
                    && (message.get("result").is_some() ^ message.get("error").is_some())))
        {
            return error(StatusCode::BAD_REQUEST, "Invalid JSON-RPC message envelope");
        }
    }
    let initializing = request
        .as_ref()
        .is_some_and(|r| r["method"] == "initialize");
    if let Some(version) = headers.get("mcp-protocol-version") {
        if version != veynodevm_mcp::VERSION {
            return error(StatusCode::BAD_REQUEST, "Unsupported MCP protocol version");
        }
    }
    if !initializing && headers.get("mcp-protocol-version").is_none() {
        return error(
            StatusCode::BAD_REQUEST,
            "MCP-Protocol-Version is required for this 2025-06-18 endpoint",
        );
    }
    if initializing {
        if headers.contains_key("mcp-session-id") {
            return error(
                StatusCode::BAD_REQUEST,
                "Initialize must not reuse a session",
            );
        }
        let request = request.unwrap();
        if request.get("id").is_none() {
            return error(StatusCode::BAD_REQUEST, "Initialize requires request id");
        }
        let mut sessions = app.sessions.lock().await;
        if app.closing.load(Ordering::Acquire) {
            return error(StatusCode::SERVICE_UNAVAILABLE, "Server is shutting down");
        }
        if sessions.len() >= 32
            || sessions
                .values()
                .filter(|s| s.principal == principal.name)
                .count()
                >= 4
        {
            return error(StatusCode::TOO_MANY_REQUESTS, "Session quota exceeded");
        }
        let session_id = match random_token() {
            Ok(s) => s,
            Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Cannot create session"),
        };
        let mut command = Command::new(&app.control_bin);
        command
            .arg("mcp")
            .arg("--state-dir")
            .arg(&app.state_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if principal.scopes.iter().any(|s| s == "lifecycle") {
            command.arg("--allow-lifecycle");
        }
        if principal.scopes.iter().any(|s| s == "execute") {
            command.arg("--allow-execute");
        }
        if principal.scopes.iter().any(|s| s == "input") {
            command.arg("--allow-input");
        }
        if principal.scopes.iter().any(|s| s == "files") {
            command.arg("--allow-files");
        }
        if principal.scopes.iter().any(|s| s == "browser") {
            command.arg("--allow-browser");
        }
        if principal.scopes.iter().any(|s| s == "clipboard") {
            command.arg("--allow-clipboard");
        }
        if let Some(vm) = vm {
            command.arg("--vm").arg(vm);
        }
        if !principal.tools.is_empty() {
            command.arg("--tools").arg(principal.tools.join(","));
        }
        let child = match command.spawn() {
            Ok(c) => c,
            Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "Cannot start MCP session"),
        };
        let lifecycle = Lifecycle::new(child, principal.clone());
        let bridge = Arc::new(Mutex::new(Bridge {
            lifecycle: lifecycle.clone(),
        }));
        // Register before any asynchronous handshake, so signal shutdown owns even
        // initializing sessions. Quotas count retiring sessions until child reap.
        sessions.insert(
            session_id.clone(),
            Session {
                principal: principal.name.clone(),
                vm: vm.map(str::to_owned),
                created: Instant::now(),
                touched: Instant::now(),
                lifecycle: lifecycle.clone(),
                io: bridge.clone(),
            },
        );
        drop(sessions);
        let mut guard = ExchangeGuard {
            app: app.clone(),
            id: session_id.clone(),
            lifecycle: lifecycle.clone(),
            armed: true,
        };
        let reply = match timeout(Duration::from_secs(5), async {
            bridge.lock().await.exchange(&request, None).await
        })
        .await
        {
            Ok(Ok(Some(value)))
                if !app.closing.load(Ordering::Acquire)
                    && !lifecycle.retired.load(Ordering::Acquire) =>
            {
                value
            }
            _ => {
                app.retire(session_id, lifecycle.clone());
                let _ = lifecycle.close().await;
                return error(StatusCode::BAD_GATEWAY, "MCP initialization failed");
            }
        };
        if reply.get("error").is_some() {
            app.retire(session_id, lifecycle.clone());
            if !lifecycle.close().await {
                return error(StatusCode::BAD_GATEWAY, "MCP cleanup unconfirmed");
            }
            return Json(reply).into_response();
        }
        guard.armed = false;
        let mut reply = reply;
        // This transport drops server notifications, so it cannot honour listChanged.
        if reply["result"]["capabilities"]["tools"]["listChanged"] == true {
            reply["result"]["capabilities"]["tools"]["listChanged"] = json!(false);
        }
        let mut r = Json(reply).into_response();
        r.headers_mut().insert(
            "mcp-session-id",
            HeaderValue::from_str(&session_id).unwrap(),
        );
        return r;
    }
    let session_id = match headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) {
        Some(s) => s,
        None => return error(StatusCode::BAD_REQUEST, "MCP-Session-Id is required"),
    };
    let mut sessions = app.sessions.lock().await;
    let session = match sessions.get_mut(session_id) {
        // A session answers only on the path it was created on.
        Some(s)
            if s.principal == principal.name
                && s.vm.as_deref() == vm
                && !s.lifecycle.retired.load(Ordering::Acquire)
                && !app.closing.load(Ordering::Acquire) =>
        {
            s
        }
        _ => return error(StatusCode::NOT_FOUND, "Session not found"),
    };
    if session.touched.elapsed() > Duration::from_secs(app.config.session_idle_seconds)
        || session.created.elapsed() > Duration::from_secs(28800)
    {
        let lifecycle = session.lifecycle.clone();
        app.retire(session_id.into(), lifecycle.clone());
        drop(sessions);
        if !lifecycle.close().await {
            return error(
                StatusCode::BAD_GATEWAY,
                "Expired session cleanup unconfirmed",
            );
        }
        return error(StatusCode::NOT_FOUND, "Session expired");
    }
    session.touched = Instant::now();
    let bridge = session.io.clone();
    let lifecycle = session.lifecycle.clone();
    if *method == Method::DELETE {
        app.retire(session_id.into(), lifecycle.clone());
        drop(sessions);
        if !lifecycle.close().await {
            return error(StatusCode::BAD_GATEWAY, "MCP cleanup unconfirmed");
        }
        return StatusCode::NO_CONTENT.into_response();
    }
    drop(sessions);
    if *method == Method::GET {
        if !mime_list(headers.get("accept"), "text/event-stream") {
            return error(
                StatusCode::NOT_ACCEPTABLE,
                "Accept must include text/event-stream",
            );
        }
        return event_stream(app.clone(), session_id.into(), lifecycle);
    }
    let request = request.unwrap();
    if request.get("method").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    if !remote_authorized(principal, vm, &request) {
        return Json(json!({"jsonrpc":"2.0","id":request.get("id").cloned().unwrap_or(Value::Null),"error":{"code":-32602,"message":"Tool or VM is not authorized for this principal"}})).into_response();
    }
    let screen = screen_subscription(&request);
    if let Some((true, uri)) = &screen {
        let screens = app.screens.lock().unwrap();
        let subscribed = screens.get(session_id).is_some_and(|s| s.contains(uri));
        if !subscribed
            && screens.values().map(BTreeSet::len).sum::<usize>() >= MAX_SCREEN_SUBSCRIPTIONS
        {
            return Json(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32002,"message":"screen subscription limit reached on this server"}})).into_response();
        }
    }
    // Reject concurrent messages in the same session rather than queueing stale control operations.
    let io = match bridge.try_lock_owned() {
        Ok(io) => io,
        Err(_) => {
            return error(
                StatusCode::CONFLICT,
                "Another request is active in this session",
            )
        }
    };
    // A request that goes away after its command was written still retires
    // the session (the guard), as before; the exchange task owns the bridge
    // until the child answers or the session ends.
    let streamed = request["params"]["_meta"]["progressToken"].is_string()
        || request["params"]["_meta"]["progressToken"].is_i64()
        || request["params"]["_meta"]["progressToken"].is_u64();
    let (progress, updates) = if streamed {
        let (sender, receiver) = mpsc::channel(16);
        (Some(sender), Some(receiver))
    } else {
        (None, None)
    };
    let mut guard = ExchangeGuard {
        app: app.clone(),
        id: session_id.into(),
        lifecycle: lifecycle.clone(),
        armed: true,
    };
    let exchange = tokio::spawn(forward(
        app.clone(),
        session_id.to_string(),
        lifecycle,
        io,
        request.clone(),
        progress,
        screen,
    ));
    if let Some(updates) = updates {
        if request.get("id").is_some() {
            return progress_stream(principal.clone(), request, updates, exchange, guard);
        }
    }
    let result = exchange.await;
    guard.armed = false;
    match result {
        Ok(Ok(Some(reply))) => Json(filter_reply(principal, &request, reply)).into_response(),
        Ok(Ok(None)) => StatusCode::ACCEPTED.into_response(),
        _ => error(
            StatusCode::BAD_GATEWAY,
            "MCP session failed; initialize a new session",
        ),
    }
}
/// `Some((subscribe, uri))` for a screen subscribe or unsubscribe request.
fn screen_subscription(request: &Value) -> Option<(bool, String)> {
    let subscribe = match request["method"].as_str()? {
        "resources/subscribe" => true,
        "resources/unsubscribe" => false,
        _ => return None,
    };
    let uri = request["params"]["uri"].as_str()?;
    matches!(Resource::parse(uri)?, Resource::Screen(_)).then(|| (subscribe, uri.to_string()))
}
async fn forward(
    app: Arc<App>,
    session_id: String,
    lifecycle: Arc<Lifecycle>,
    mut io: OwnedMutexGuard<Bridge>,
    request: Value,
    progress: Option<mpsc::Sender<Value>>,
    screen: Option<(bool, String)>,
) -> Result<Option<Value>> {
    match timeout(Duration::from_secs(40), io.exchange(&request, progress)).await {
        Ok(Ok(reply)) => {
            if let (Some((subscribe, uri)), Some(reply)) = (screen, &reply) {
                if reply.get("result").is_some() && !lifecycle.retired.load(Ordering::Acquire) {
                    let mut screens = app.screens.lock().unwrap();
                    let uris = screens.entry(session_id).or_default();
                    if subscribe {
                        uris.insert(uri);
                    } else {
                        uris.remove(&uri);
                    }
                }
            }
            Ok(reply)
        }
        _ => {
            drop(io);
            app.retire(session_id, lifecycle.clone());
            let _ = lifecycle.close().await;
            bail!("MCP session failed")
        }
    }
}
fn sse_response(body: axum::body::Body) -> Response {
    let mut response = Response::new(body);
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    response
}
/// A POST that carries a progress token: its progress notifications, then
/// its response, as one SSE stream.
fn progress_stream(
    principal: Principal,
    request: Value,
    updates: mpsc::Receiver<Value>,
    exchange: JoinHandle<Result<Option<Value>>>,
    guard: ExchangeGuard,
) -> Response {
    // Dropping the stream before the response (the client went away) drops
    // the armed guard, which retires the session as a cancelled POST does.
    let state = Some((updates, exchange, guard));
    let stream = futures_util::stream::unfold(state, move |state| {
        let (principal, request) = (principal.clone(), request.clone());
        async move {
            let (mut updates, mut exchange, mut guard) = state?;
            tokio::select! {
                biased;
                Some(update) = updates.recv() => {
                    let frame = sse_frame(None, &update.to_string());
                    Some((Ok::<_, std::convert::Infallible>(frame), Some((updates, exchange, guard))))
                }
                result = &mut exchange => {
                    guard.armed = false;
                    let reply = match result {
                        Ok(Ok(Some(reply))) => filter_reply(&principal, &request, reply),
                        _ => json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"MCP session failed; initialize a new session"}}),
                    };
                    Some((Ok(sse_frame(None, &reply.to_string())), None))
                }
            }
        }
    });
    sse_response(axum::body::Body::from_stream(stream))
}
/// The session's GET stream: queued notifications, then new ones as they
/// arrive, with a keepalive comment every 15 s that also counts as session
/// activity. It ends when the session ends; idle and absolute limits still
/// apply.
fn event_stream(app: Arc<App>, session_id: String, lifecycle: Arc<Lifecycle>) -> Response {
    let stream = futures_util::stream::unfold(
        (app, session_id, lifecycle),
        |(app, session_id, lifecycle)| async move {
            let inbox = lifecycle.inbox.clone();
            loop {
                let changed = inbox.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if lifecycle.retired.load(Ordering::Acquire) || inbox.closed.load(Ordering::Acquire)
                {
                    return None;
                }
                let next = inbox.events.lock().unwrap().pop();
                if let Some(frame) = next {
                    return Some((
                        Ok::<_, std::convert::Infallible>(frame),
                        (app, session_id, lifecycle),
                    ));
                }
                tokio::select! {
                    _ = &mut changed => continue,
                    _ = tokio::time::sleep(KEEPALIVE) => {
                        if let Some(session) = app.sessions.lock().await.get_mut(&session_id) {
                            session.touched = Instant::now();
                        }
                        return Some((Ok(": keepalive\n\n".to_string()), (app, session_id, lifecycle)));
                    }
                }
            }
        },
    );
    sse_response(axum::body::Body::from_stream(stream))
}
#[tokio::main]
async fn main() {
    // Workspace feature unification can enable both rustls providers; pick one explicitly.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    if let Err(e) = run().await {
        eprintln!("vm-mcp-http: {e:#}");
        std::process::exit(1)
    }
}
async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut config_path = None;
    let mut state_dir = None;
    let mut control_bin = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config_path = args.next().map(PathBuf::from),
            "--state-dir" => state_dir = args.next().map(PathBuf::from),
            "--control-bin" => control_bin = args.next().map(PathBuf::from),
            _ => bail!(
                "usage: vm-mcp-http --config PRIVATE_JSON --state-dir PATH [--control-bin PATH]"
            ),
        }
    }
    let path = config_path.context("--config is required")?;
    private_file(&path)?;
    if fs::metadata(&path)?.len() > 65536 {
        bail!("config too large")
    }
    // serde's message quotes the offending value; a misplaced raw token must not reach stderr.
    let config: Config = serde_json::from_slice(&fs::read(path)?).map_err(|e| {
        anyhow::anyhow!(
            "config does not match the expected schema at line {} column {}",
            e.line(),
            e.column()
        )
    })?;
    private_file(&config.tls_key)?;
    if config.principals.is_empty()
        || config.principals.len() > 128
        || !(1..=3600).contains(&config.session_idle_seconds)
    {
        bail!("invalid principal count or session TTL")
    }
    let mut names = BTreeSet::new();
    let mut hashes = BTreeSet::new();
    for p in &config.principals {
        if p.name.is_empty()
            || !names.insert(&p.name)
            || p.token_sha256.len() != 64
            || !p
                .token_sha256
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            || !hashes.insert(&p.token_sha256)
            || p.vm_ids.len() > 1024
            || p.vm_ids
                .iter()
                .any(|id| veynodevm_api::validate_id(id).is_err())
            || !p.scopes.iter().any(|s| s == "observe")
            || p.scopes.iter().any(|s| {
                ![
                    "observe",
                    "lifecycle",
                    "execute",
                    "files",
                    "input",
                    "browser",
                    "clipboard",
                ]
                .contains(&s.as_str())
            })
            || (!p.tools.is_empty()
                && veynodevm_mcp::tool_groups::parse(&p.tools.join(",")).is_err())
        {
            bail!("invalid or duplicate principal definition")
        }
    }
    for origin in &config.allowed_origins {
        let uri: axum::http::Uri = origin.parse().context("invalid allowed Origin")?;
        if uri.scheme_str() != Some("https")
            || uri.authority().is_none()
            || uri.path_and_query().is_some_and(|pq| pq.as_str() != "/")
            || origin.ends_with('/')
        {
            bail!("allowed origins must be exact HTTPS origins without paths")
        }
    }
    let bind = config.bind;
    let tls =
        axum_server::tls_rustls::RustlsConfig::from_pem_file(&config.tls_cert, &config.tls_key)
            .await?;
    let state_dir = state_dir.context("--state-dir required")?.canonicalize()?;
    let control_bin = control_bin
        .unwrap_or(std::env::current_exe()?.with_file_name("vm-control"))
        .canonicalize()?;
    let app = Arc::new(App {
        config,
        state_dir,
        control_bin,
        sessions: Mutex::new(BTreeMap::new()),
        requests: Semaphore::new(64),
        closing: AtomicBool::new(false),
        cleanup_failed: AtomicBool::new(false),
        changed: Notify::new(),
        screens: std::sync::Mutex::new(BTreeMap::new()),
    });
    let shutdown_signal = install_shutdown_signal()?;
    let cleaner = app.clone();
    let cleaner_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if cleaner.closing.load(Ordering::Acquire) {
                break;
            }
            let sessions = cleaner.sessions.lock().await;
            for (id, session) in sessions.iter() {
                if session.touched.elapsed()
                    > Duration::from_secs(cleaner.config.session_idle_seconds)
                    || session.created.elapsed() > Duration::from_secs(28800)
                {
                    cleaner.retire(id.clone(), session.lifecycle.clone());
                }
            }
        }
    });
    let router = Router::new()
        .route("/mcp", any(endpoint))
        .route("/vm/{id}/mcp", any(vm_endpoint))
        .layer(DefaultBodyLimit::max(veynodevm_api::MAX_LINE))
        .layer(middleware::from_fn_with_state(app.clone(), admission))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(45),
        ))
        .with_state(app.clone());
    eprintln!("MCP HTTPS listening on {bind}; TLS terminates on this host; provisioned bearer authentication");
    let handle = axum_server::Handle::new();
    let server = axum_server::bind_rustls(bind, tls)
        .handle(handle.clone())
        .serve(router.into_make_service());
    tokio::pin!(server);
    let (server_result, signal_result) = tokio::select! {
        signal = shutdown_signal => (None, signal),
        result = &mut server => (Some(result), Ok(())),
    };
    app.closing.store(true, Ordering::Release);
    handle.graceful_shutdown(Some(Duration::from_secs(45)));
    cleaner_task.abort();
    let cleanup = app.shutdown();
    let transport = async {
        if let Some(result) = server_result {
            result.map_err(anyhow::Error::from)
        } else {
            timeout(Duration::from_secs(46), &mut server)
                .await
                .context("HTTP shutdown timed out")?
                .map_err(anyhow::Error::from)
        }
    };
    let (cleanup, transport) = tokio::join!(cleanup, transport);
    cleanup?;
    transport?;
    signal_result?;
    Ok(())
}
fn install_shutdown_signal(
) -> std::io::Result<impl std::future::Future<Output = std::io::Result<()>>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! { value = interrupt.recv() => value, value = terminate.recv() => value }
            .ok_or_else(|| std::io::Error::other("shutdown signal stream closed"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn principal() -> Principal {
        Principal {
            name: "agent".into(),
            token_sha256: "a".repeat(64),
            scopes: vec!["observe".into(), "lifecycle".into()],
            vm_ids: vec!["allowed".into()],
            tools: vec![],
        }
    }
    #[test]
    fn snapshot_remote_projection_keeps_scope_and_removes_all_host_receipts() {
        let p = principal();
        for tool in [
            "vm_snapshot_create",
            "vm_snapshot_list",
            "vm_snapshot_status",
            "vm_snapshot_delete",
            "vm_snapshot_restore",
            "vm_snapshot_reconcile",
            "vm_snapshot_resume_source",
        ] {
            let request =
                json!({"method":"tools/call","params":{"name":tool,"arguments":{"id":"allowed"}}});
            assert!(remote_authorized(&p, None, &request));
            let denied =
                json!({"method":"tools/call","params":{"name":tool,"arguments":{"id":"other"}}});
            assert!(!remote_authorized(&p, None, &denied));
            let private = json!({"vm":"allowed","name":"checkpoint","operation_id":"abc","state":"failed","error":"secret /private/disk.raw","compatibility":{"backend_sha256":"secret"},"result":{"capture_verified":true,"disk":"/private/disk.raw"}});
            let reply = filter_reply(
                &p,
                &request,
                json!({"jsonrpc":"2.0","id":7,"result":{"isError":false,"content":[{"type":"text","text":private.to_string()},{"type":"text","text":"secret"}],"structuredContent":{"secret":"secret"}}}),
            );
            assert!(!reply.to_string().contains("secret"));
            assert!(!reply.to_string().contains("/private"));
            assert_eq!(reply["result"]["content"].as_array().unwrap().len(), 1);
            let value: Value =
                serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            assert_eq!(value["name"], "checkpoint");
            assert_eq!(value["result"]["capture_verified"], true);
            for bad in [
                json!({"error":{"message":"secret"}}),
                json!({"result":{"isError":true,"content":[{"text":"secret"}]}}),
                json!({"result":{"isError":false,"content":[{"text":"not-json secret"}]}}),
                json!({"result":{"isError":false,"structuredContent":{"secret":"secret"}}}),
                json!({"result":{"isError":false,"content":[{"text":"null"}]}}),
                json!({"result":{"isError":false,"content":[{"text":"[1]"}]}}),
            ] {
                let cleaned = filter_reply(&p, &request, bad);
                assert!(!cleaned.to_string().contains("secret"));
                assert_eq!(cleaned["result"]["isError"], true);
            }
        }
    }
    #[test]
    fn snapshot_recovery_projection_preserves_typed_outcome_without_private_details() {
        let value = public_snapshot(
            &json!({"action":"create","state":"failed","error_code":"capture_source_resumed",
            "source_resume_pending":true,"result":{"source_resumed":true,"restored":false,"snapshot_result_verified":false,
                "native_io_retirement_verified":true,"private":"/private/secret"}}),
        );
        assert_eq!(value["error_code"], "capture_source_resumed");
        assert_eq!(value["action"], "create");
        assert_eq!(value["source_resume_pending"], true);
        assert_eq!(value["result"]["source_resumed"], true);
        assert_eq!(value["result"]["restored"], false);
        assert!(!value.to_string().contains("secret"));
        let bad = public_snapshot(
            &json!({"action":"/private/secret","error_code":"/private/secret",
            "source_resumed":"secret","native_io_retirement_verified":1}),
        );
        assert_eq!(bad, json!({}));
    }
    #[test]
    fn remote_lifecycle_cannot_configure_host_paths_or_touch_other_vms() {
        let p = principal();
        assert!(!remote_authorized(
            &p,
            None,
            &json!({"method":"tools/call","params":{"name":"vm_create","arguments":{"id":"allowed","disk":"/host/private-file"}}})
        ));
        assert!(!remote_authorized(
            &p,
            None,
            &json!({"method":"tools/call","params":{"name":"vm_start","arguments":{"id":"not-allowed"}}})
        ));
        assert!(remote_authorized(
            &p,
            None,
            &json!({"method":"tools/call","params":{"name":"vm_start","arguments":{"id":"allowed"}}})
        ));
    }
    #[test]
    fn graphics_profile_does_not_enable_remote_create() {
        let p = principal();
        for profile in ["virgl", "venus-moltenvk-experimental", "unknown"] {
            let request = json!({"method":"tools/call","params":{"name":"vm_create","arguments":{"id":"allowed","backend":"qemu-hvf","qemu_graphics_profile":profile}}});
            assert!(!remote_authorized(&p, None, &request));
        }
        let reply = filter_reply(
            &p,
            &json!({"method":"tools/list"}),
            json!({"result":{"tools":[{"name":"vm_create","inputSchema":{"properties":{"qemu_graphics_profile":{"enum":["virgl","venus-moltenvk-experimental"]}}}},{"name":"vm_status"}]}}),
        );
        assert_eq!(reply["result"]["tools"], json!([{"name":"vm_status"}]));
    }
    #[test]
    fn tool_groups_enable_needs_no_vm_and_never_names_remote_create() {
        let p = principal();
        let request = json!({"method":"tools/call","params":{"name":"vm_tools_enable","arguments":{"groups":["core"]}}});
        assert!(remote_authorized(&p, None, &request));
        let result = json!({"enabled":["core"],"added_tools":["vm_start","vm_create"]});
        let reply = filter_reply(
            &p,
            &request,
            json!({"result":{"isError":false,"content":[{"type":"text","text":result.to_string()}]}}),
        );
        let value: Value =
            serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(value["added_tools"], json!(["vm_start"]));
        let mut bad = principal();
        bad.tools = vec!["core".into(), "shell".into()];
        assert!(veynodevm_mcp::tool_groups::parse(&bad.tools.join(",")).is_err());
    }
    #[test]
    fn tool_list_changes_reach_the_event_stream() {
        let notice = json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"});
        assert!(observable(&principal(), &notice));
        let other = json!({"jsonrpc":"2.0","method":"notifications/message","params":{}});
        assert!(!observable(&principal(), &other));
    }
    #[test]
    fn window_observation_is_still_vm_allowlisted() {
        let mut p = principal();
        p.scopes = vec!["observe".into()];
        assert!(remote_authorized(
            &p,
            None,
            &json!({"method":"tools/call","params":{"name":"vm_windows","arguments":{"id":"allowed"}}})
        ));
        assert!(!remote_authorized(
            &p,
            None,
            &json!({"method":"tools/call","params":{"name":"vm_windows","arguments":{"id":"other"}}})
        ));
    }
    #[test]
    fn cua_tools_and_per_vm_sessions_are_vm_allowlisted() {
        let p = principal();
        let cua = |vm: &str| json!({"method":"tools/call","params":{"name":"cua_click","arguments":{"vm":vm,"id":"allowed"}}});
        assert!(remote_authorized(&p, None, &cua("allowed")));
        // The VM is the `vm` argument; an allowed `id` beside it does not count.
        assert!(!remote_authorized(&p, None, &cua("other")));
        let click = json!({"method":"tools/call","params":{"name":"click","arguments":{"pid":1}}});
        assert!(remote_authorized(&p, Some("allowed"), &click));
        assert!(!remote_authorized(&p, Some("other"), &click));
        // A per-VM session has no resources, not even its own VM's.
        let screen =
            json!({"method":"resources/read","params":{"uri":"veynode://vm/allowed/screen"}});
        assert!(!remote_authorized(&p, Some("allowed"), &screen));
    }
    #[test]
    fn observations_filter_vm_ids_and_host_diagnostics() {
        let vm = json!({"config":{"id":"allowed","kernel":"/host/private-kernel","command_line":"token=secret","shared_dir":"/host/private","memory_mib":1024},"pid":42,"runtime":{"state":"error","error":"cannot open /host/private"}});
        let request = json!({"method":"tools/call","params":{"name":"vm_list"}});
        let reply = json!({"result":{"isError":false,"content":[{"type":"text","text":json!([vm,{"config":{"id":"hidden"}}]).to_string()}]}});
        let filtered = filter_reply(&principal(), &request, reply);
        let text = filtered["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            !text.contains("private")
                && !text.contains("secret")
                && !text.contains("hidden")
                && !text.contains("pid")
        );
        let values: Value = serde_json::from_str(text).unwrap();
        assert_eq!(values[0]["config"]["memory_mib"], 1024);
        assert_eq!(values[0]["runtime"]["state"], "error");
    }
}

#[cfg(test)]
mod event_tests {
    use super::*;
    fn principal() -> Principal {
        Principal {
            name: "agent".into(),
            token_sha256: "a".repeat(64),
            scopes: vec!["observe".into()],
            vm_ids: vec!["allowed".into()],
            tools: vec![],
        }
    }
    #[test]
    fn resources_and_notifications_follow_the_vm_allowlist() {
        let p = principal();
        for method in [
            "resources/read",
            "resources/subscribe",
            "resources/unsubscribe",
        ] {
            let request = |uri: &str| json!({"method":method,"params":{"uri":uri}});
            assert!(remote_authorized(
                &p,
                None,
                &request("veynode://vm/allowed/status")
            ));
            assert!(!remote_authorized(
                &p,
                None,
                &request("veynode://vm/other/screen")
            ));
            assert!(!remote_authorized(
                &p,
                None,
                &request("veynode://vm/../status")
            ));
            assert!(!remote_authorized(
                &p,
                None,
                &json!({"method":method,"params":{}})
            ));
        }
        assert!(remote_authorized(
            &p,
            None,
            &json!({"method":"resources/list"})
        ));
        let updated = |uri: &str| json!({"jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":uri}});
        assert!(observable(&p, &updated("veynode://vm/allowed/log/worker")));
        assert!(!observable(&p, &updated("veynode://vm/other/status")));
        assert!(!observable(
            &p,
            &json!({"method":"notifications/message","params":{"uri":"veynode://vm/allowed/status"}})
        ));
        let list = filter_reply(
            &p,
            &json!({"method":"resources/list"}),
            json!({"jsonrpc":"2.0","id":1,"result":{"resources":[{"uri":"veynode://vm/allowed/status"},{"uri":"veynode://vm/other/status"}]}}),
        );
        assert_eq!(list["result"]["resources"].as_array().unwrap().len(), 1);
        let status = json!({"config":{"id":"allowed","disk":"/Users/me/disk.img","kernel_cmdline":"secret"},"pid":42,"runtime":{"state":"running","serial_log":"/Users/me/serial.log"}});
        let read = filter_reply(
            &p,
            &json!({"method":"resources/read","params":{"uri":"veynode://vm/allowed/status"}}),
            json!({"jsonrpc":"2.0","id":1,"result":{"contents":[{"uri":"veynode://vm/allowed/status","text":status.to_string()}]}}),
        );
        let text = read["result"]["contents"][0]["text"].as_str().unwrap();
        assert!(!text.contains("/Users/") && !text.contains("secret") && !text.contains("42"));
        assert!(text.contains("running"));
    }
    #[test]
    fn event_queue_is_bounded_and_reports_drops() {
        let mut queue = Queue::default();
        for index in 0..MAX_QUEUED_EVENTS + 10 {
            queue.push(json!({"n":index}).to_string());
        }
        queue.push("x".repeat(MAX_EVENT_BYTES + 1));
        assert_eq!(queue.events.len(), MAX_QUEUED_EVENTS);
        let first = queue.pop().unwrap();
        assert!(first.starts_with("id: 1\nevent: message\ndata: "));
        assert!(first.contains("events_dropped") && first.contains("\"dropped\":11"));
        assert!(queue.pop().unwrap().contains("{\"n\":10}"));
        for _ in 0..MAX_QUEUED_EVENT_BYTES / MAX_EVENT_BYTES + 4 {
            queue.push("y".repeat(MAX_EVENT_BYTES));
        }
        assert!(queue.bytes <= MAX_QUEUED_EVENT_BYTES);
        assert!(queue.dropped > 0);
    }
    #[test]
    fn screen_subscriptions_are_recognized_for_the_global_bound() {
        assert_eq!(
            screen_subscription(
                &json!({"method":"resources/subscribe","params":{"uri":"veynode://vm/a/screen"}})
            ),
            Some((true, "veynode://vm/a/screen".into()))
        );
        assert_eq!(
            screen_subscription(
                &json!({"method":"resources/unsubscribe","params":{"uri":"veynode://vm/a/screen"}})
            ),
            Some((false, "veynode://vm/a/screen".into()))
        );
        assert!(screen_subscription(
            &json!({"method":"resources/subscribe","params":{"uri":"veynode://vm/a/status"}})
        )
        .is_none());
    }
}

#[cfg(test)]
#[path = "vm_mcp_http_tests/mod.rs"]
mod shutdown_tests;
