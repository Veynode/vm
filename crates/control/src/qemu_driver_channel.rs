//! The QEMU worker's end of the `veynode.driver` virtio-serial port: newline-delimited
//! JSON-RPC 2.0 frames to and from the Guest's cua-driver, with no envelope.
//!
//! One persistent connection to QEMU's chardev socket carries every call. Ids are the
//! host's: each request gets a fresh one, so replies are matched by id, and the caller's
//! own id is restored on the reply. Frames are at most 1 MiB each way. Frames without an id
//! (driver notifications) and replies nobody waits for are dropped. A broken connection
//! fails the calls in flight; the next call connects again. Unlike the serial-v2 exec
//! channel this port has no fence or heartbeat: a stalled QEMU main thread only delays
//! driver replies, and each call ends at its own deadline.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// Request and reply bound of one frame, newline excluded.
pub const MAX_FRAME: usize = 1024 * 1024;
/// The longest deadline a call may ask for.
pub const MAX_DEADLINE: Duration = Duration::from_secs(600);

/// Calls waiting for a reply, by host id, with the connection they were sent on.
type Waiters = Arc<Mutex<HashMap<u64, (u64, mpsc::SyncSender<Result<Value>>)>>>;

struct Connection {
    stream: UnixStream,
    /// Bumped per connection, so a reader that outlives its stream fails only its own calls.
    generation: u64,
}

pub struct Channel {
    path: PathBuf,
    connection: Mutex<Option<Connection>>,
    waiters: Waiters,
    /// The last host id and connection generation handed out.
    next: Mutex<(u64, u64)>,
    /// The newest generation whose reader has seen the connection end.
    closed: Arc<AtomicU64>,
}

/// One call in flight; poll it from the worker loop.
pub struct Pending {
    rx: mpsc::Receiver<Result<Value>>,
    id: u64,
    caller_id: Value,
    deadline: Instant,
    waiters: Waiters,
    done: bool,
}
impl Pending {
    /// The driver's reply frame with the caller's id, an error, or None while waiting.
    pub fn poll(&mut self) -> Option<Result<Value>> {
        if self.done {
            return None;
        }
        let result = match self.rx.try_recv() {
            Ok(Ok(mut frame)) => {
                frame["id"] = self.caller_id.clone();
                Ok(frame)
            }
            Ok(Err(error)) => Err(error),
            Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!(
                "driver channel closed before the reply arrived"
            )),
            Err(mpsc::TryRecvError::Empty) if Instant::now() >= self.deadline => {
                Err(anyhow::anyhow!("deadline exceeded waiting for cua-driver"))
            }
            Err(mpsc::TryRecvError::Empty) => return None,
        };
        self.done = true;
        self.waiters.lock().unwrap().remove(&self.id);
        Some(result)
    }
    /// Blocks until the reply or the deadline.
    pub fn wait(mut self) -> Result<Value> {
        loop {
            if let Some(result) = self.poll() {
                return result;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.waiters.lock().unwrap().remove(&self.id);
    }
}

impl Channel {
    /// Connects lazily: QEMU listens on `path` (`server=on,wait=off`) from its start.
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            connection: Mutex::new(None),
            waiters: Arc::new(Mutex::new(HashMap::new())),
            next: Mutex::new((0, 0)),
            closed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Sends one JSON-RPC request frame. `deadline` is capped at `MAX_DEADLINE`.
    pub fn request(&self, mut frame: Value, deadline: Duration) -> Result<Pending> {
        if frame["jsonrpc"] != "2.0" || !frame["method"].is_string() {
            bail!("driver_rpc frame must be a JSON-RPC 2.0 request")
        }
        let caller_id = frame
            .get("id")
            .cloned()
            .context("driver_rpc frame needs an id")?;
        let deadline = Instant::now() + deadline.min(MAX_DEADLINE);
        let id = {
            let mut next = self.next.lock().unwrap();
            next.0 += 1;
            next.0
        };
        frame["id"] = json!(id);
        let mut line = serde_json::to_vec(&frame)?;
        if line.len() > MAX_FRAME {
            bail!("driver_rpc request exceeds 1 MiB")
        }
        line.push(b'\n');
        let (tx, rx) = mpsc::sync_channel(1);
        let mut connection = self.connection.lock().unwrap();
        let mut retry = true;
        loop {
            let reused = connection
                .as_ref()
                .is_some_and(|c| c.generation > self.closed.load(Ordering::Acquire));
            if !reused {
                *connection = Some(self.connect()?);
            }
            let current = connection.as_mut().unwrap();
            self.waiters
                .lock()
                .unwrap()
                .insert(id, (current.generation, tx.clone()));
            let sent = current
                .stream
                .set_write_timeout(Some(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_millis(1)),
                ))
                .and_then(|()| current.stream.write_all(&line));
            let Err(error) = sent else { break };
            self.waiters.lock().unwrap().remove(&id);
            // A partial frame leaves the stream out of step: drop it, and the calls on it.
            let broken = connection.take().unwrap();
            let _ = broken.stream.shutdown(std::net::Shutdown::Both);
            // A kept connection the peer closed before its reader saw the end: the
            // frame reached no one, so it goes once more on a new connection.
            if !(reused && retry && peer_closed(&error)) {
                return Err(error).context("writing to the driver port");
            }
            retry = false;
        }
        let pending = Pending {
            rx,
            id,
            caller_id,
            deadline,
            waiters: self.waiters.clone(),
            done: false,
        };
        Ok(pending)
    }

    fn connect(&self) -> Result<Connection> {
        let stream = UnixStream::connect(&self.path).context("connecting the driver port")?;
        let generation = {
            let mut next = self.next.lock().unwrap();
            next.1 += 1;
            next.1
        };
        let reader = stream.try_clone()?;
        let waiters = self.waiters.clone();
        let closed = self.closed.clone();
        thread::spawn(move || read_replies(reader, generation, waiters, closed));
        Ok(Connection { stream, generation })
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.lock().unwrap().take() {
            let _ = connection.stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// A write or socket option error that means the peer has gone (macOS says EINVAL
/// for a socket option on a socket its peer reset).
fn peer_closed(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionReset, NotConnected};
    matches!(error.kind(), BrokenPipe | ConnectionReset | NotConnected)
        || error.raw_os_error() == Some(libc::EINVAL)
}

fn read_replies(stream: UnixStream, generation: u64, waiters: Waiters, closed: Arc<AtomicU64>) {
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = Vec::new();
        match (&mut reader)
            .take(MAX_FRAME as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.last() != Some(&b'\n') {
            if line.len() > MAX_FRAME {
                // Skip the rest of an oversized frame; its caller ends at its deadline.
                let mut rest = Vec::new();
                if reader.read_until(b'\n', &mut rest).is_err() || rest.last() != Some(&b'\n') {
                    break;
                }
                continue;
            }
            break;
        }
        let Ok(frame) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        // Notifications and replies to calls that have ended are dropped.
        let Some(id) = frame["id"].as_u64() else {
            continue;
        };
        let mut waiting = waiters.lock().unwrap();
        if waiting.get(&id).is_some_and(|(g, _)| *g == generation) {
            let (_, waiter) = waiting.remove(&id).unwrap();
            let _ = waiter.send(Ok(frame));
        }
    }
    closed.fetch_max(generation, Ordering::AcqRel);
    waiters.lock().unwrap().retain(|_, (g, waiter)| {
        if *g == generation {
            let _ = waiter.send(Err(anyhow::anyhow!("driver port disconnected")));
            false
        } else {
            true
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn listener() -> (tempfile::TempDir, PathBuf, UnixListener) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("driver.sock");
        let listener = UnixListener::bind(&path).unwrap();
        (dir, path, listener)
    }

    #[test]
    fn a_kept_connection_the_peer_closed_is_replaced_and_the_call_sent_again() {
        let (_dir, path, listener) = listener();
        let channel = Channel::new(&path);
        // A connection whose peer is gone and whose reader has not said so yet.
        let (stale, peer) = UnixStream::pair().unwrap();
        drop(peer);
        *channel.connection.lock().unwrap() = Some(Connection {
            stream: stale,
            generation: 1,
        });
        channel.next.lock().unwrap().1 = 1;
        let guest = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            let frame: Value = serde_json::from_str(&line).unwrap();
            writeln!(
                writer,
                "{}",
                json!({"jsonrpc":"2.0","id":frame["id"],"result":{}})
            )
            .unwrap();
        });
        let reply = channel
            .request(
                json!({"jsonrpc":"2.0","id":"a","method":"tools/list"}),
                Duration::from_secs(2),
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(reply["id"], "a");
        guest.join().unwrap();
    }

    #[test]
    fn replies_are_matched_by_id_out_of_order() {
        let (_dir, path, listener) = listener();
        let guest = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut frames = vec![];
            for _ in 0..2 {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                frames.push(serde_json::from_str::<Value>(&line).unwrap());
            }
            writeln!(
                writer,
                "{}",
                json!({"jsonrpc":"2.0","method":"notifications/message"})
            )
            .unwrap();
            for frame in frames.iter().rev() {
                let reply = json!({"jsonrpc":"2.0","id":frame["id"],"result":{"echo":frame["params"]["name"]}});
                writeln!(writer, "{reply}").unwrap();
            }
            thread::sleep(Duration::from_millis(100));
        });
        let channel = Channel::new(&path);
        let call = |name: &str, id: Value| {
            channel
                .request(
                    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name}}),
                    Duration::from_secs(5),
                )
                .unwrap()
        };
        let first = call("a", json!("x"));
        let second = call("b", json!(7));
        let first = first.wait().unwrap();
        let second = second.wait().unwrap();
        assert_eq!(first["id"], "x");
        assert_eq!(first["result"]["echo"], "a");
        assert_eq!(second["id"], 7);
        assert_eq!(second["result"]["echo"], "b");
        guest.join().unwrap();
    }

    #[test]
    fn a_silent_guest_ends_at_the_call_deadline() {
        let (_dir, path, listener) = listener();
        let _guest = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_secs(2));
        });
        let channel = Channel::new(&path);
        let started = Instant::now();
        let error = channel
            .request(
                json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
                Duration::from_millis(200),
            )
            .unwrap()
            .wait()
            .unwrap_err();
        assert!(error.to_string().contains("deadline"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn oversized_requests_are_refused_and_a_closed_port_reconnects() {
        let (_dir, path, listener) = listener();
        let channel = Channel::new(&path);
        let big = "x".repeat(MAX_FRAME);
        assert!(channel
            .request(
                json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"text":big}}),
                Duration::from_secs(1)
            )
            .is_err());
        let guest = thread::spawn(move || {
            // The first connection closes at once; the second answers.
            drop(listener.accept().unwrap());
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            let frame: Value = serde_json::from_str(&line).unwrap();
            writeln!(
                writer,
                "{}",
                json!({"jsonrpc":"2.0","id":frame["id"],"result":{}})
            )
            .unwrap();
        });
        let frame = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        // The write may fail at once or the reply never comes: either way the call fails.
        if let Ok(first) = channel.request(frame.clone(), Duration::from_secs(2)) {
            assert!(first.wait().is_err());
        }
        // The reader saw EOF and marked the connection closed; the next call reconnects.
        let second = channel.request(frame, Duration::from_secs(2)).unwrap();
        assert_eq!(second.wait().unwrap()["id"], 1);
        guest.join().unwrap();
    }
}
