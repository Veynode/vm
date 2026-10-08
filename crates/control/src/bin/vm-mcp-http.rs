//! Direct-to-host HTTPS MCP. No hosted TLS termination, OAuth discovery, or relay E2EE is claimed.
use anyhow::{bail, Context, Result};
use axum::{
    body::Bytes,
    extract::Request,
    extract::{DefaultBodyLimit, State},
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
    sync::Arc,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, Semaphore},
    time::timeout,
};

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
}
fn default_bind() -> SocketAddr {
    "127.0.0.1:8443".parse().unwrap()
}
fn default_ttl() -> u64 {
    900
}
struct Session {
    principal: String,
    created: Instant,
    touched: Instant,
    io: Arc<Mutex<Bridge>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
}
struct Bridge {
    child: Child,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    stdout: BufReader<ChildStdout>,
}
impl Bridge {
    async fn exchange(&mut self, request: &Value) -> Result<Option<Value>> {
        let mut bytes = serde_json::to_vec(request)?;
        bytes.push(b'\n');
        {
            let mut stdin = self.stdin.lock().await;
            let stdin = stdin.as_mut().context("session is closed")?;
            stdin.write_all(&bytes).await?;
            stdin.flush().await?;
        }
        if request.get("id").is_none() {
            return Ok(None);
        }
        let mut line = Vec::new();
        let read = (&mut self.stdout)
            .take((vm_control::MAX_MESSAGE + 1) as u64)
            .read_until(b'\n', &mut line)
            .await?;
        if read == 0 || read > vm_control::MAX_MESSAGE || line.last() != Some(&b'\n') {
            bail!("invalid stdio bridge response")
        }
        let response: Value = serde_json::from_slice(&line)?;
        if response["id"] != request["id"] {
            bail!("stdio response identity mismatch")
        }
        Ok(Some(response))
    }
    async fn close(&mut self) {
        self.stdin.lock().await.take();
        if timeout(Duration::from_secs(3), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}
struct App {
    config: Config,
    state_dir: PathBuf,
    control_bin: PathBuf,
    sessions: Mutex<BTreeMap<String, Session>>,
    requests: Semaphore,
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
fn remote_authorized(principal: &Principal, request: &Value) -> bool {
    if request["method"] != "tools/call" {
        return true;
    }
    match request["params"]["name"].as_str().unwrap_or("") {
        "vm_create" => false,
        "vm_list" | "vm_capabilities" => true,
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
    let name = request["params"]["name"].as_str().unwrap_or("");
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
async fn admission(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
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
        dispatch(&app, &principal, &method, &headers, &body).await
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
        if version != vm_control::VERSION {
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
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "Cannot start MCP session"),
        };
        let mut bridge = Bridge {
            stdin: Arc::new(Mutex::new(child.stdin.take())),
            stdout: BufReader::new(child.stdout.take().unwrap()),
            child,
        };
        let reply = match timeout(Duration::from_secs(5), bridge.exchange(&request)).await {
            Ok(Ok(Some(value))) => value,
            _ => {
                bridge.close().await;
                return error(StatusCode::BAD_GATEWAY, "MCP initialization failed");
            }
        };
        if reply.get("error").is_some() {
            bridge.close().await;
            return Json(reply).into_response();
        }
        sessions.insert(
            session_id.clone(),
            Session {
                principal: principal.name.clone(),
                created: Instant::now(),
                touched: Instant::now(),
                stdin: bridge.stdin.clone(),
                io: Arc::new(Mutex::new(bridge)),
            },
        );
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
        Some(s) if s.principal == principal.name => s,
        _ => return error(StatusCode::NOT_FOUND, "Session not found"),
    };
    if session.touched.elapsed() > Duration::from_secs(app.config.session_idle_seconds)
        || session.created.elapsed() > Duration::from_secs(28800)
    {
        let session = sessions.remove(session_id).unwrap();
        drop(sessions);
        session.stdin.lock().await.take();
        session.io.lock().await.close().await;
        return error(StatusCode::NOT_FOUND, "Session expired");
    }
    session.touched = Instant::now();
    let bridge = session.io.clone();
    let stdin = session.stdin.clone();
    if *method == Method::DELETE {
        sessions.remove(session_id);
        drop(sessions);
        // Do not wait behind a long tool response to revoke this session's transport.
        stdin.lock().await.take();
        let _ = timeout(Duration::from_secs(40), async {
            bridge.lock().await.close().await
        })
        .await;
        return StatusCode::NO_CONTENT.into_response();
    }
    drop(sessions);
    if *method == Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let request = request.unwrap();
    if request.get("method").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    if !remote_authorized(principal, &request) {
        return Json(json!({"jsonrpc":"2.0","id":request.get("id").cloned().unwrap_or(Value::Null),"error":{"code":-32602,"message":"Tool or VM is not authorized for this principal"}})).into_response();
    }
    // Reject concurrent messages in the same session rather than queueing stale control operations.
    let mut io = match bridge.try_lock() {
        Ok(io) => io,
        Err(_) => {
            return error(
                StatusCode::CONFLICT,
                "Another request is active in this session",
            )
        }
    };
    match timeout(Duration::from_secs(40), io.exchange(&request)).await {
        Ok(Ok(Some(reply))) => Json(filter_reply(principal, &request, reply)).into_response(),
        Ok(Ok(None)) => StatusCode::ACCEPTED.into_response(),
        _ => {
            io.close().await;
            app.sessions.lock().await.remove(session_id);
            error(
                StatusCode::BAD_GATEWAY,
                "MCP session failed; initialize a new session",
            )
        }
    }
}
#[tokio::main]
async fn main() {
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
    let config: Config = serde_json::from_slice(&fs::read(path)?)?;
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
                .any(|id| vm_control::validate_id(id).is_err())
            || !p.scopes.iter().any(|s| s == "observe")
            || p.scopes.iter().any(|s| {
                !["observe", "lifecycle", "execute", "files", "input"].contains(&s.as_str())
            })
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
    });
    let cleaner = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let expired = {
                let mut sessions = cleaner.sessions.lock().await;
                let ids = sessions
                    .iter()
                    .filter(|(_, s)| {
                        s.touched.elapsed()
                            > Duration::from_secs(cleaner.config.session_idle_seconds)
                            || s.created.elapsed() > Duration::from_secs(28800)
                    })
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                ids.into_iter()
                    .filter_map(|id| sessions.remove(&id))
                    .collect::<Vec<_>>()
            };
            for session in expired {
                session.stdin.lock().await.take();
                let _ = timeout(Duration::from_secs(40), async {
                    session.io.lock().await.close().await
                })
                .await;
            }
        }
    });
    let router = Router::new()
        .route("/mcp", any(endpoint))
        .layer(DefaultBodyLimit::max(vm_control::MAX_MESSAGE))
        .layer(middleware::from_fn_with_state(app.clone(), admission))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(45),
        ))
        .with_state(app);
    eprintln!("MCP HTTPS listening on {bind}; TLS terminates on this host; provisioned bearer authentication");
    axum_server::bind_rustls(bind, tls)
        .serve(router.into_make_service())
        .await?;
    Ok(())
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
        }
    }
    #[test]
    fn remote_lifecycle_cannot_configure_host_paths_or_touch_other_vms() {
        let p = principal();
        assert!(!remote_authorized(
            &p,
            &json!({"method":"tools/call","params":{"name":"vm_create","arguments":{"id":"allowed","disk":"/host/private-file"}}})
        ));
        assert!(!remote_authorized(
            &p,
            &json!({"method":"tools/call","params":{"name":"vm_start","arguments":{"id":"not-allowed"}}})
        ));
        assert!(remote_authorized(
            &p,
            &json!({"method":"tools/call","params":{"name":"vm_start","arguments":{"id":"allowed"}}})
        ));
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
