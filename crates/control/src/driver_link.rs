//! A persistent stdio link to cua-driver for Guests whose driver the host
//! reaches over SSH (Windows, macOS containers; docs/computer-use.md). One
//! child process per VM carries newline JSON-RPC both ways: on Windows an SSH
//! session running `cua-driver mcp --socket` against the driver daemon in the
//! desktop session, on macOS `veynodevm __driver`, which runs the driver over
//! SSH. It keeps the Linux agent's rules (`guest/veynode-agent/driver.go`):
//! the link owns the MCP handshake, the caller sends only `tools/*`,
//! `resources/*` and `ping`, request ids are rewritten and restored, and
//! failures are `code: message` with the agent's codes.
//!
//! The link reconnects on the next call after the child exits. A named
//! cua-driver session ends with its connection, so the `start_session` calls
//! that succeeded are sent again on a new connection, in order.
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    io::{BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// The release VeynodeVM embeds; the Linux image pins the same.
const PIN: &str = include_str!("../../../runtime/cua-driver.json");

pub const MAX_FRAME: usize = 1 << 20;
pub const MAX_DEADLINE: Duration = Duration::from_secs(600);
const HANDSHAKE: Duration = Duration::from_secs(30);
const STDERR_TAIL: usize = 4096;
const MAX_STARTS: usize = 5;
const START_WINDOW: Duration = Duration::from_secs(60);
const METHODS: &[&str] = &[
    "tools/list",
    "tools/call",
    "resources/list",
    "resources/read",
    "ping",
];

/// The pinned cua-driver binary for a Guest platform (`windows-arm64`,
/// `darwin-arm64`) on this host, checked against the pin, and its sha256.
pub fn host_binary(platform: &str) -> Result<(std::path::PathBuf, String)> {
    use sha2::{Digest, Sha256};
    let pin: Value = serde_json::from_str(PIN)?;
    let asset = &pin["assets"][platform];
    let (Some(binary), Some(sha256)) = (asset["binary"].as_str(), asset["binary_sha256"].as_str())
    else {
        bail!("no pinned cua-driver for {platform}")
    };
    let root = crate::home::Runtime::load()?.cua_driver.ok_or_else(|| {
        fail("unsupported", format!("cua-driver is not registered on this host (run runtime/fetch-cua-driver.sh {platform}, then scripts/install-dev-runtime.py)"))
    })?;
    let path = root.join(platform).join(binary);
    let bytes = std::fs::read(&path).map_err(|e| {
        fail(
            "unsupported",
            format!(
                "{}: {e} (run runtime/fetch-cua-driver.sh {platform})",
                path.display()
            ),
        )
    })?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != sha256 {
        bail!(fail(
            "unsupported",
            format!("{} has sha256 {actual}, pinned {sha256}", path.display())
        ))
    }
    Ok((path, sha256.to_owned()))
}

/// Starts the child whose stdio is the driver's MCP; it may first install the
/// driver in the Guest.
pub type Spawn = Box<dyn Fn() -> Result<Command> + Send + Sync>;

pub struct Link {
    spawn: Spawn,
    conn: Mutex<Option<Arc<Conn>>>,
    starts: Mutex<Vec<Instant>>,
    /// Successful `start_session` arguments by session name, replayed on reconnect.
    sessions: Mutex<BTreeMap<String, Value>>,
}

struct Conn {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    pending: Mutex<HashMap<String, mpsc::SyncSender<Value>>>,
    next: AtomicU64,
    alive: AtomicBool,
    stderr: Arc<Mutex<Vec<u8>>>,
}

fn fail(code: &str, message: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("{code}: {message}")
}

impl Link {
    pub fn new(spawn: Spawn) -> Self {
        Self {
            spawn,
            conn: Mutex::new(None),
            starts: Mutex::new(Vec::new()),
            sessions: Mutex::new(BTreeMap::new()),
        }
    }

    /// Sends one request frame and returns the driver's reply frame, with the
    /// caller's id.
    pub fn call(&self, frame: &Value, deadline: Duration) -> Result<Value> {
        let id = frame
            .get("id")
            .filter(|id| !id.is_null())
            .cloned()
            .ok_or_else(|| {
                fail(
                    "invalid_request",
                    "frame needs an id; notifications are not forwarded",
                )
            })?;
        let method = frame["method"].as_str().unwrap_or("");
        if !frame.is_object() || !METHODS.contains(&method) {
            bail!(fail(
                "invalid_request",
                format!("method {method:?} is not forwarded")
            ))
        }
        if serde_json::to_vec(frame)?.len() > MAX_FRAME {
            bail!(fail("invalid_request", "frame exceeds 1 MiB"))
        }
        let deadline = deadline.min(MAX_DEADLINE);
        let conn = self.connected()?;
        let mut reply = conn.request(frame, deadline)?;
        if method == "tools/call" {
            self.remember_session(&frame["params"], &reply);
        }
        reply["id"] = id;
        Ok(reply)
    }

    /// Ends the child; the next call starts a new one.
    pub fn close(&self) {
        if let Some(conn) = self.conn.lock().unwrap().take() {
            conn.stop();
        }
    }

    fn connected(&self) -> Result<Arc<Conn>> {
        let mut slot = self.conn.lock().unwrap();
        if let Some(conn) = slot.as_ref() {
            if conn.alive.load(Ordering::Acquire) {
                return Ok(conn.clone());
            }
            conn.stop();
            *slot = None;
        }
        {
            let mut starts = self.starts.lock().unwrap();
            let now = Instant::now();
            starts.retain(|t| now.duration_since(*t) < START_WINDOW);
            if starts.len() >= MAX_STARTS {
                bail!(fail(
                    "driver_unavailable",
                    format!(
                        "the driver link failed {} times within a minute; not reconnecting yet",
                        starts.len()
                    )
                ))
            }
            starts.push(now);
        }
        let command = (self.spawn)()?;
        let conn = Conn::start(command)?;
        let handshake = conn.request(&json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"veynodevm","version":env!("CARGO_PKG_VERSION")}}}), HANDSHAKE)
            .and_then(|reply| match reply.get("error") {
                Some(error) => Err(fail("driver_unavailable", format!("cua-driver refused initialize: {error}"))),
                None => Ok(()),
            })
            .and_then(|()| conn.write(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})));
        if let Err(error) = handshake {
            conn.stop();
            return Err(error);
        }
        for (name, arguments) in self.sessions.lock().unwrap().iter() {
            let frame = json!({"jsonrpc":"2.0","id":0,"method":"tools/call","params":{"name":"start_session","arguments":arguments}});
            if let Err(error) = conn.request(&frame, HANDSHAKE) {
                conn.stop();
                return Err(fail(
                    "driver_unavailable",
                    format!("restoring session {name:?}: {error}"),
                ));
            }
        }
        *slot = Some(conn.clone());
        Ok(conn)
    }

    fn remember_session(&self, params: &Value, reply: &Value) {
        let succeeded = reply.get("error").is_none() && reply["result"]["isError"] != true;
        let name = params["arguments"]["session"].as_str().map(str::to_owned);
        match (params["name"].as_str(), name) {
            (Some("start_session"), Some(name)) if succeeded => {
                self.sessions
                    .lock()
                    .unwrap()
                    .insert(name, params["arguments"].clone());
            }
            (Some("end_session"), Some(name)) => {
                self.sessions.lock().unwrap().remove(&name);
            }
            _ => {}
        }
    }
}

impl crate::cua_bridge::Transport for Link {
    fn call(&self, frame: Value, deadline: Duration) -> Result<Value> {
        Link::call(self, &frame, deadline)
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.close();
    }
}

impl Conn {
    fn start(mut command: Command) -> Result<Arc<Self>> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                fail(
                    "driver_unavailable",
                    format!("cannot start the driver link: {e}"),
                )
            })?;
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let conn = Arc::new(Self {
            stdin: Mutex::new(child.stdin.take().unwrap()),
            child: Mutex::new(child),
            pending: Mutex::new(HashMap::new()),
            next: AtomicU64::new(0),
            alive: AtomicBool::new(true),
            stderr: Arc::new(Mutex::new(Vec::new())),
        });
        let tail = conn.stderr.clone();
        thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(n @ 1..) = stderr.read(&mut buffer) {
                let mut tail = tail.lock().unwrap();
                tail.extend_from_slice(&buffer[..n]);
                let excess = tail.len().saturating_sub(STDERR_TAIL);
                tail.drain(..excess);
            }
        });
        let reader = conn.clone();
        thread::spawn(move || reader.read(BufReader::with_capacity(64 << 10, stdout)));
        Ok(conn)
    }

    fn read(&self, mut stdout: impl BufRead) {
        let mut line = Vec::new();
        loop {
            line.clear();
            // A line over 4 MiB is cut short and fails to parse; its caller times out.
            match stdout
                .by_ref()
                .take(4 * MAX_FRAME as u64)
                .read_until(b'\n', &mut line)
            {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(message) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if message.get("method").is_some() {
                if let Some(id) = message.get("id") {
                    // A request from the driver (roots, sampling): this client has none.
                    let _ = self.write(&json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"not supported by this client"}}));
                }
                continue;
            }
            let key = message["id"].to_string();
            if let Some(waiter) = self.pending.lock().unwrap().remove(&key) {
                let _ = waiter.try_send(if line.len() > MAX_FRAME {
                    json!({"veynode_too_large": line.len()})
                } else {
                    message
                });
            }
        }
        self.alive.store(false, Ordering::Release);
        // Dropping the senders wakes every waiter with an exit.
        self.pending.lock().unwrap().clear();
    }

    fn write(&self, message: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        let mut stdin = self.stdin.lock().unwrap();
        stdin
            .write_all(&bytes)
            .and_then(|()| stdin.flush())
            .map_err(|_| self.exited())
    }

    fn request(&self, frame: &Value, deadline: Duration) -> Result<Value> {
        let id = format!(
            "veynodevm-{}",
            self.next.fetch_add(1, Ordering::Relaxed) + 1
        );
        let key = json!(id).to_string();
        let (tx, rx) = mpsc::sync_channel(1);
        if !self.alive.load(Ordering::Acquire) {
            return Err(self.exited());
        }
        self.pending.lock().unwrap().insert(key.clone(), tx);
        let mut framed = frame.clone();
        framed["id"] = json!(id);
        if let Err(error) = self.write(&framed) {
            self.pending.lock().unwrap().remove(&key);
            return Err(error);
        }
        match rx.recv_timeout(deadline) {
            Ok(reply) if reply.get("veynode_too_large").is_some() => Err(fail(
                "reply_too_large",
                format!(
                    "cua-driver reply of {} bytes exceeds 1 MiB",
                    reply["veynode_too_large"]
                ),
            )),
            Ok(reply) => Ok(reply),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // A late reply finds no waiter and is dropped; the link stays up.
                self.pending.lock().unwrap().remove(&key);
                Err(fail(
                    "deadline_exceeded",
                    format!(
                        "cua-driver did not answer within {} ms",
                        deadline.as_millis()
                    ),
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(self.exited()),
        }
    }

    fn exited(&self) -> anyhow::Error {
        let tail = String::from_utf8_lossy(&self.stderr.lock().unwrap())
            .trim()
            .to_owned();
        if tail.is_empty() {
            fail("driver_exited", "the driver link closed")
        } else {
            fail("driver_exited", format!("the driver link closed: {tail}"))
        }
    }

    fn stop(&self) {
        self.alive.store(false, Ordering::Release);
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake driver: a Python MCP server on stdio whose tools act as named.
    const FAKE: &str = r#"
import json, os, sys, threading, time
lock = threading.Lock()
def send(m):
    with lock:
        sys.stdout.write(json.dumps(m) + "\n"); sys.stdout.flush()
inits = 0
for line in sys.stdin:
    m = json.loads(line)
    method, mid = m.get("method"), m.get("id")
    name = m.get("params", {}).get("name")
    args = m.get("params", {}).get("arguments", {})
    if method == "initialize":
        inits += 1
        send({"jsonrpc": "2.0", "id": mid, "result": {"serverInfo": {"name": "fake"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [], "inits": inits, "pid": os.getpid()}})
    elif name == "echo":
        send({"jsonrpc": "2.0", "method": "notifications/message", "params": {}})
        send({"jsonrpc": "2.0", "id": 99, "method": "roots/list"})
        send({"jsonrpc": "2.0", "id": mid, "result": {"echo": args}})
    elif name == "start_session":
        with open(os.environ["FAKE_LOG"], "a") as log: log.write(json.dumps(args) + "\n")
        send({"jsonrpc": "2.0", "id": mid, "result": {"content": []}})
    elif name == "slow":
        threading.Thread(target=lambda i=mid: (time.sleep(0.3), send({"jsonrpc": "2.0", "id": i, "result": {}}))).start()
    elif name == "big":
        send({"jsonrpc": "2.0", "id": mid, "result": {"blob": "x" * (1 << 20)}})
    elif name == "crash":
        sys.stderr.write("fake driver crashed\n"); sys.stderr.flush(); os._exit(3)
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": {"content": []}})
"#;

    fn link(log: &std::path::Path) -> Link {
        let log = log.to_path_buf();
        Link::new(Box::new(move || {
            let mut command = Command::new("python3");
            command.args(["-u", "-c", FAKE]).env("FAKE_LOG", &log);
            Ok(command)
        }))
    }

    fn tool(id: &str, name: &str, arguments: Value) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}})
    }

    fn code(error: anyhow::Error) -> String {
        error.to_string().split(':').next().unwrap().to_owned()
    }

    #[test]
    fn concurrent_callers_get_their_own_ids_and_one_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let link = Arc::new(link(&dir.path().join("log")));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let link = link.clone();
                thread::spawn(move || {
                    let reply = link
                        .call(
                            &tool(&format!("c{i}"), "echo", json!({"n": i})),
                            Duration::from_secs(5),
                        )
                        .unwrap();
                    assert_eq!(reply["id"], format!("c{i}"));
                    assert_eq!(reply["result"]["echo"]["n"], i);
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let listed = link
            .call(
                &json!({"jsonrpc":"2.0","id":7,"method":"tools/list"}),
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(listed["id"], 7);
        assert_eq!(listed["result"]["inits"], 1);
    }

    #[test]
    fn only_host_requests_are_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let link = link(&dir.path().join("log"));
        for frame in [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
            json!({"jsonrpc":"2.0","method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":null,"method":"tools/list"}),
            tool("big", "echo", json!({"text": "y".repeat(MAX_FRAME)})),
        ] {
            assert_eq!(
                code(link.call(&frame, Duration::from_secs(1)).unwrap_err()),
                "invalid_request",
                "{frame}"
            );
        }
        assert!(
            link.conn.lock().unwrap().is_none(),
            "a refused frame started the link"
        );
    }

    #[test]
    fn deadline_oversize_and_exit_are_reported_and_the_link_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let link = link(&dir.path().join("log"));
        let short = Duration::from_millis(50);
        assert_eq!(
            code(link.call(&tool("s", "slow", json!({})), short).unwrap_err()),
            "deadline_exceeded"
        );
        thread::sleep(Duration::from_millis(400));
        assert_eq!(
            code(
                link.call(&tool("b", "big", json!({})), Duration::from_secs(5))
                    .unwrap_err()
            ),
            "reply_too_large"
        );
        let error = link
            .call(&tool("c", "crash", json!({})), Duration::from_secs(5))
            .unwrap_err();
        assert_eq!(code(anyhow!(error.to_string())), "driver_exited");
        assert!(error.to_string().contains("fake driver crashed"), "{error}");
        let reply = link
            .call(&tool("e", "echo", json!({})), Duration::from_secs(5))
            .unwrap();
        assert_eq!(reply["id"], "e");
    }

    #[test]
    fn started_sessions_are_restored_after_a_reconnect() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let link = link(&log);
        let wait = Duration::from_secs(5);
        link.call(
            &tool("1", "start_session", json!({"session":"a","label":"x"})),
            wait,
        )
        .unwrap();
        link.call(&tool("2", "start_session", json!({"session":"b"})), wait)
            .unwrap();
        link.call(&tool("3", "end_session", json!({"session":"b"})), wait)
            .unwrap();
        let _ = link.call(&tool("4", "crash", json!({})), wait);
        link.call(&tool("5", "echo", json!({})), wait).unwrap();
        let replayed: Vec<Value> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            replayed,
            vec![
                json!({"session":"a","label":"x"}),
                json!({"session":"b"}),
                json!({"session":"a","label":"x"})
            ]
        );
    }

    #[test]
    fn a_crash_loop_is_not_reconnected() {
        let dir = tempfile::tempdir().unwrap();
        let link = link(&dir.path().join("log"));
        let mut last = String::new();
        for i in 0..=MAX_STARTS {
            last = code(
                link.call(
                    &tool(&i.to_string(), "crash", json!({})),
                    Duration::from_secs(5),
                )
                .unwrap_err(),
            );
        }
        assert_eq!(last, "driver_unavailable");
    }

    #[test]
    fn the_bridge_runs_over_a_link_and_caches_its_tool_list() {
        use crate::cua_bridge::{run, Desktop, Request, Scopes};
        let dir = tempfile::tempdir().unwrap();
        let link = link(&dir.path().join("log"));
        let scopes = Scopes {
            input: true,
            browser: false,
            clipboard: false,
            execute: false,
        };
        let desktop = Desktop {
            person_attached: false,
        };
        let mut cache = None;
        run(&link, Request::List, scopes, desktop, &mut cache).unwrap();
        assert_eq!(cache.as_ref().unwrap()["inits"], 1);
        let refused = run(
            &link,
            Request::Call {
                tool: "install_extension".into(),
                arguments: json!({}),
            },
            scopes,
            desktop,
            &mut cache,
        )
        .unwrap();
        assert_eq!(refused["isError"], true);
        let clicked = run(
            &link,
            Request::Call {
                tool: "click".into(),
                arguments: json!({"pid": 1, "x": 2, "y": 3}),
            },
            scopes,
            desktop,
            &mut cache,
        );
        assert!(clicked.is_ok(), "{clicked:?}");
    }
}
