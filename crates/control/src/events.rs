//! Daemon side of pushed MCP events: a bounded, sequence-numbered log of VM
//! lifecycle changes that MCP adapters read with `events {after}`, and
//! bounded, redacted tails of a VM's serial and worker logs (`log_read`).
//! Neither call blocks: an adapter must not hold one of the daemon's 32
//! connections while it waits for something to happen.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// Events retained for adapters that read late. Older ones are dropped and a
/// reader that asks for them is told so with `lost`.
pub const CAPACITY: usize = 512;
/// Largest tail `log_read` returns, before redaction.
pub const MAX_LOG_BYTES: u64 = 64 * 1024;

#[derive(Default)]
pub struct Log(Mutex<Inner>);
#[derive(Default)]
struct Inner {
    /// Sequence of the next event; the first one is 1.
    next: u64,
    events: VecDeque<Value>,
    /// Worker pid whose exit was already published, per VM.
    exits: BTreeMap<String, u32>,
    /// Last worker-reported state, per VM, so a repeated state is not an event.
    states: BTreeMap<String, String>,
}
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
impl Log {
    /// `fields` may add `state`, `reason` and `exit_code`; anything else is
    /// dropped, so no host path or process id reaches a remote principal.
    pub fn publish(&self, vm: &str, event: &str, fields: Value) {
        let mut inner = self.0.lock().unwrap();
        inner.next = inner.next.max(1);
        let mut value = json!({"seq":inner.next,"vm":vm,"event":event,"unix_ms":unix_ms()});
        for key in ["state", "reason", "exit_code"] {
            if let Some(v) = fields.get(key).filter(|v| v.is_string() || v.is_i64()) {
                value[key] = v.clone();
            }
        }
        inner.next += 1;
        inner.events.push_back(value);
        while inner.events.len() > CAPACITY {
            inner.events.pop_front();
        }
    }
    pub fn started(&self, vm: &str) {
        self.0.lock().unwrap().states.remove(vm);
        self.publish(vm, "started", json!({}));
    }
    /// One worker `state` event. A guest reset is an event of its own; other
    /// states are published only when they differ from the previous one.
    pub fn worker_state(&self, vm: &str, state: &Value) {
        let Some(name) = state["state"].as_str() else {
            return;
        };
        if state["reason"] == "guest-reset" {
            self.0.lock().unwrap().states.insert(vm.into(), name.into());
            self.publish(
                vm,
                "guest_reset",
                json!({"state":name,"reason":"guest-reset"}),
            );
            return;
        }
        let changed = self
            .0
            .lock()
            .unwrap()
            .states
            .insert(vm.into(), name.into())
            .is_none_or(|previous| previous != name);
        if changed {
            self.publish(vm, "state", json!({"state":name}));
        }
    }
    /// Publishes a worker's exit once per worker process, however many
    /// observers (exit monitor, status, restart) see it.
    pub fn exited(&self, vm: &str, pid: u32, status: std::process::ExitStatus, expected: bool) {
        if self.0.lock().unwrap().exits.insert(vm.into(), pid) == Some(pid) {
            return;
        }
        let crashed = !(status.success() || expected);
        let mut fields = json!({"state":if crashed {"crashed"} else {"stopped"}});
        if let Some(code) = status.code() {
            fields["exit_code"] = json!(code);
        }
        self.publish(vm, if crashed { "crashed" } else { "stopped" }, fields);
    }
    /// Events after `after`, oldest first. Without `after` only the cursor is
    /// returned, so a new reader starts at the present.
    pub fn since(&self, args: &Value) -> Result<Value> {
        let inner = self.0.lock().unwrap();
        let next = inner.next.max(1);
        let Some(after) = args.get("after") else {
            return Ok(json!({"events":[],"next":next,"lost":false}));
        };
        let after = after.as_u64().context("after must be an event sequence")?;
        let oldest = inner
            .events
            .front()
            .map_or(next, |e| e["seq"].as_u64().unwrap());
        let events: Vec<Value> = inner
            .events
            .iter()
            .filter(|e| e["seq"].as_u64().unwrap() > after)
            .cloned()
            .collect();
        Ok(json!({"events":events,"next":next,"lost":after + 1 < oldest}))
    }
}

/// The tail of an owner-private regular log file, at most `max_bytes`, as
/// text: invalid UTF-8 replaced, terminal escapes and control characters
/// removed, long token-like runs (hex, base64) replaced, and for host logs
/// absolute paths replaced. `max_bytes` 0 returns only the size.
pub fn read_tail(path: &Path, max_bytes: u64, host: bool) -> Result<Value> {
    if max_bytes > MAX_LOG_BYTES {
        bail!("max_bytes must be 0..{MAX_LOG_BYTES}")
    }
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"size":0,"text":"","truncated":false,"exists":false}))
        }
        Err(_) => bail!("log is not readable"),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!("log must be an owner-private regular file")
    }
    let size = metadata.len();
    let start = size.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file as &mut File)
        .take(max_bytes)
        .read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0 {
        // The first line is cut; start at the next whole one.
        text = text
            .split_once('\n')
            .map_or(String::new(), |(_, rest)| rest.into());
    }
    Ok(json!({"size":size,"text":redact(&text, host),"truncated":start > 0,"exists":true}))
}

/// Removes what a log must not carry across the MCP boundary. Guest serial
/// output keeps its (guest) paths; host logs lose every absolute path.
pub fn redact(text: &str, host: bool) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ params final-byte; other escapes: ESC + one char.
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
        } else if c == '\n' || c == '\t' || !c.is_control() {
            plain.push(c);
        }
    }
    let mut out = String::with_capacity(plain.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        let tokenish = word.len() >= 32
            && word
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+/=_".contains(&b))
            && word.bytes().any(|b| b.is_ascii_digit());
        if tokenish {
            out.push_str("<redacted>");
        } else if host && word.starts_with('/') && word.len() > 1 {
            out.push_str("<path>");
        } else if host && word.contains(":/") && !word.contains("://") {
            // `option=value:/path` style arguments in worker diagnostics.
            let (head, _) = word.split_once(":/").unwrap();
            out.push_str(head);
            out.push_str(":<path>");
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in plain.chars() {
        if c.is_whitespace() || "\"'(),;<>[]{}=`".contains(c) {
            flush(&mut word, &mut out);
            out.push(c);
        } else {
            word.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{fs::PermissionsExt, process::ExitStatusExt};

    #[test]
    fn events_are_sequenced_bounded_and_carry_only_public_fields() {
        let log = Log::default();
        assert_eq!(log.since(&json!({})).unwrap()["next"], 1);
        log.publish(
            "a",
            "started",
            json!({"pid":7,"serial_log":"/Users/x/s.log"}),
        );
        let first = log.since(&json!({"after":0})).unwrap();
        assert_eq!(first["events"][0]["seq"], 1);
        assert!(first["events"][0].get("pid").is_none());
        assert!(first["events"][0].get("serial_log").is_none());
        assert_eq!(first["lost"], false);
        for _ in 0..CAPACITY + 5 {
            log.publish("a", "state", json!({"state":"running"}));
        }
        let late = log.since(&json!({"after":1})).unwrap();
        assert_eq!(late["lost"], true);
        assert_eq!(late["events"].as_array().unwrap().len(), CAPACITY);
        let current = log
            .since(&json!({"after":late["next"].as_u64().unwrap() - 1}))
            .unwrap();
        assert_eq!(current["events"].as_array().unwrap().len(), 0);
        assert_eq!(current["lost"], false);
        assert!(log.since(&json!({"after":"x"})).is_err());
    }

    #[test]
    fn worker_states_publish_changes_resets_and_one_exit_per_worker() {
        let log = Log::default();
        log.started("a");
        log.worker_state("a", &json!({"event":"state","state":"running"}));
        log.worker_state("a", &json!({"event":"state","state":"running"}));
        log.worker_state(
            "a",
            &json!({"event":"state","state":"starting","reason":"guest-reset"}),
        );
        let ok = std::process::ExitStatus::from_raw(0);
        let failed = std::process::ExitStatus::from_raw(1 << 8);
        log.exited("a", 10, ok, false);
        log.exited("a", 10, ok, false);
        log.exited("a", 11, failed, false);
        log.exited("a", 12, failed, true);
        let events = log.since(&json!({"after":0})).unwrap()["events"].clone();
        let kinds: Vec<&str> = events
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["event"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "started",
                "state",
                "guest_reset",
                "stopped",
                "crashed",
                "stopped"
            ]
        );
        assert_eq!(events[4]["exit_code"], 1);
    }

    #[test]
    fn log_tails_are_bounded_private_and_redacted() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("w.log");
        assert_eq!(read_tail(&path, 10, true).unwrap()["exists"], false);
        let token = "VNMARK-TOKEN-0123456789abcdef0123456789abcdef";
        let body = format!(
            "first line\n\u{1b}[31mred\u{1b}[0m qemu: -drive file=/Users/me/disk.img opened /private/var/x\ntoken {} {}\nguest /dev/vda ok\n",
            token.replace('-', ""),
            "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU2Nzg5"
        );
        std::fs::write(&path, &body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_tail(&path, 100, true).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let host = read_tail(&path, MAX_LOG_BYTES, true).unwrap();
        let text = host["text"].as_str().unwrap();
        assert_eq!(host["size"], body.len() as u64);
        assert!(!text.contains('\u{1b}') && text.contains("red qemu"));
        assert!(!text.contains("/Users/") && !text.contains("/private/"));
        assert!(!text.contains("VNMARKTOKEN") && !text.contains("QUJDREVG"));
        assert!(text.contains("<redacted>") && text.contains("file=<path>"));
        let guest = read_tail(&path, MAX_LOG_BYTES, false).unwrap();
        assert!(guest["text"].as_str().unwrap().contains("/dev/vda"));
        let tail = read_tail(&path, 20, false).unwrap();
        assert_eq!(tail["truncated"], true);
        assert_eq!(tail["text"], "guest /dev/vda ok\n");
        assert_eq!(read_tail(&path, 0, true).unwrap()["text"], "");
        assert!(read_tail(&path, MAX_LOG_BYTES + 1, true).is_err());
        let link = temp.path().join("link.log");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_tail(&link, 10, true).is_err());
    }
}
