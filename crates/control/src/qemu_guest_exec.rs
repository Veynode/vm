//! Opt-in v2: bounded multiplexed control only. Commands run asynchronously in Guest cgroups.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    #[serde(rename = "type")]
    kind: String,
    guest_nonce: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_nonce: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response: Option<Value>,
}
fn hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn wire(e: &Envelope) -> Result<Vec<u8>> {
    let b = serde_json::to_vec(e)?;
    if b.len() > 65536 {
        bail!("serial execution request exceeds bound")
    };
    let mut out = (b.len() as u32).to_be_bytes().to_vec();
    out.extend(b);
    Ok(out)
}
fn send_until(s: &mut UnixStream, e: &Envelope, deadline: Instant) -> Result<()> {
    let b = wire(e)?;
    let mut n = 0;
    let until = deadline.min(Instant::now() + Duration::from_millis(100));
    while n < b.len() {
        if Instant::now() >= until {
            bail!("serial execution write deadline")
        };
        match s.write(&b[n..]) {
            Ok(0) => bail!("serial execution write EOF"),
            Ok(x) => n += x,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2))
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
#[cfg(test)]
fn send(s: &mut UnixStream, e: &Envelope) -> Result<()> {
    send_until(s, e, Instant::now() + Duration::from_millis(100))
}
fn initial(s: &mut UnixStream, until: Instant) -> Result<Envelope> {
    let mut h = [0; 4];
    super::guest_transport::read_exact(s, &mut h, until)?;
    let n = u32::from_be_bytes(h) as usize;
    if n == 0 || n > 1048576 {
        bail!("serial execution response bound")
    };
    let mut b = vec![0; n];
    super::guest_transport::read_exact(s, &mut b, until)?;
    Ok(serde_json::from_slice(&b)?)
}
struct Task {
    request: Value,
    deadline: Instant,
    reply: mpsc::SyncSender<Result<Value>>,
    cancelled: Arc<AtomicBool>,
    lane: Arc<AtomicBool>,
}
struct Awaited {
    task: Option<Task>,
    id: Value,
    deadline: Instant,
    method: String,
}
pub struct Pending {
    rx: mpsc::Receiver<Result<Value>>,
    cancelled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<&'static str>>,
    stream: Arc<Mutex<super::guest_transport::Lifetime>>,
    done: bool,
}
impl Pending {
    pub fn poll(&mut self) -> Option<Result<Value>> {
        let r = match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                // Unwinding can drop the task sender before the worker's outer
                // epilogue publishes its fence. Never expose that failure to a
                // caller while the same incarnation still reports ready or
                // accepts another request.
                self.stop.store(true, Ordering::Release);
                *self.state.lock().unwrap() = "fenced";
                self.stream.lock().unwrap().stop();
                Some(Err(anyhow::anyhow!("serial execution fenced")))
            }
        };
        if r.is_some() {
            self.done = true
        }
        r
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        if !self.done {
            self.cancelled.store(true, Ordering::Release);
            self.stop.store(true, Ordering::Release);
        }
    }
}
pub struct Transport {
    tx: mpsc::SyncSender<Task>,
    state: Arc<Mutex<&'static str>>,
    stop: Arc<AtomicBool>,
    lane: Arc<AtomicBool>,
    /// The VM's vCPUs are stopped (`pause`): no heartbeats, no new requests,
    /// and no deadline runs out until it resumes.
    paused: Arc<AtomicBool>,
    stream: Arc<Mutex<super::guest_transport::Lifetime>>,
    #[cfg(test)]
    iterations: Arc<std::sync::atomic::AtomicU64>,
}
impl Transport {
    pub fn state(&self) -> &'static str {
        *self.state.lock().unwrap()
    }
    /// Set before the vCPUs stop and cleared once they run again. A stopped
    /// Guest cannot answer, so its silence is not a fault: deadlines in flight
    /// move by the time spent paused and heartbeats start again on resume.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }
    pub fn start(path: &Path, host: String) -> Self {
        Self::start_configured(path, host, |_| Ok(()))
    }
    fn start_configured(
        path: &Path,
        host: String,
        setup: impl FnOnce(&UnixStream) -> Result<()> + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Task>(2);
        let state = Arc::new(Mutex::new("starting"));
        let status = state.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let stream = Arc::new(Mutex::new(super::guest_transport::Lifetime::default()));
        let saved = stream.clone();
        let paused = Arc::new(AtomicBool::new(false));
        let frozen = paused.clone();
        let path = path.to_owned();
        #[cfg(test)]
        let iterations = Arc::new(std::sync::atomic::AtomicU64::new(0));
        #[cfg(test)]
        let counted = iterations.clone();
        thread::spawn(move || {
            let run = || -> Result<()> {
                if !hex(&host) || stopped.load(Ordering::Acquire) {
                    bail!("invalid serial execution incarnation")
                }
                // QEMU already listens before this call. No retry/reconnect after the first connect.
                let mut s = super::guest_transport::connect_private(&path, &saved)?;
                s.set_nonblocking(true)?;
                setup(&s)?;
                if stopped.load(Ordering::Acquire) {
                    bail!("serial execution stopped during connect")
                }
                let until = Instant::now() + Duration::from_secs(120);
                let challenge = initial(&mut s, until)?;
                if challenge.version != 2
                    || challenge.kind != "challenge"
                    || !hex(&challenge.guest_nonce)
                    || challenge.host_nonce.is_some()
                    || challenge.sequence.is_some()
                    || challenge.request.is_some()
                    || challenge.response.is_some()
                {
                    bail!("serial execution challenge rejected")
                }
                let guest = challenge.guest_nonce;
                send_until(
                    &mut s,
                    &Envelope {
                        version: 2,
                        kind: "hello".into(),
                        guest_nonce: guest.clone(),
                        host_nonce: Some(host.clone()),
                        sequence: None,
                        request: None,
                        response: None,
                    },
                    until,
                )?;
                let ready = initial(&mut s, until)?;
                if ready.version != 2
                    || ready.kind != "ready"
                    || ready.guest_nonce != guest
                    || ready.host_nonce.as_deref() != Some(&host)
                    || ready.sequence.is_some()
                    || ready.request.is_some()
                    || ready.response.is_some()
                {
                    bail!("serial execution handshake rejected")
                }
                *status.lock().unwrap() = "ready";
                let mut sequence = 0u64;
                let mut pending = BTreeMap::<u64, Awaited>::new();
                let mut bytes = Vec::new();
                let mut next_heartbeat = Instant::now();
                // A task taken off the channel by the idle wait below, dispatched next pass.
                let mut carried: Option<Task> = None;
                let mut paused_since: Option<Instant> = None;
                loop {
                    #[cfg(test)]
                    counted.fetch_add(1, Ordering::Relaxed);
                    if stopped.load(Ordering::Acquire) {
                        bail!("serial execution cancelled")
                    }
                    let now = Instant::now();
                    match (frozen.load(Ordering::Acquire), paused_since) {
                        (true, None) => paused_since = Some(now),
                        (false, Some(since)) => {
                            let paused_for = now.duration_since(since);
                            for p in pending.values_mut() {
                                p.deadline += paused_for;
                            }
                            next_heartbeat = now;
                            paused_since = None;
                        }
                        _ => {}
                    }
                    if paused_since.is_some() {
                        // A reply sent just before the stop waits in the socket; nothing is
                        // sent and nothing times out until the vCPUs run again.
                        thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    if pending.values().any(|p| {
                        now >= p.deadline
                            || p.task
                                .as_ref()
                                .is_some_and(|t| t.cancelled.load(Ordering::Acquire))
                    }) {
                        bail!("serial execution response deadline; incarnation fenced")
                    }
                    let task = match carried.take() {
                        Some(t) => Some(t),
                        None => match rx.try_recv() {
                            Ok(t) => Some(t),
                            Err(mpsc::TryRecvError::Empty) => None,
                            Err(_) => bail!("serial execution owner closed"),
                        },
                    };
                    let mut queue = Vec::new();
                    if let Some(task) = task {
                        if now >= task.deadline || task.cancelled.load(Ordering::Acquire) {
                            bail!("expired serial execution dispatch")
                        };
                        queue.push((task.request.clone(), Some(task)));
                    }
                    if now >= next_heartbeat && !pending.values().any(|p| p.method == "heartbeat") {
                        queue.push((json!({"id":"heartbeat","method":"heartbeat"}), None));
                        next_heartbeat = now + Duration::from_millis(500);
                    }
                    for (request, task) in queue {
                        sequence = sequence
                            .checked_add(1)
                            .context("serial sequence exhausted")?;
                        let method = request["method"]
                            .as_str()
                            .context("serial method required")?
                            .to_owned();
                        let deadline = task
                            .as_ref()
                            .map(|t| t.deadline)
                            .unwrap_or(now + Duration::from_secs(1));
                        let id = request["id"].clone();
                        send_until(
                            &mut s,
                            &Envelope {
                                version: 2,
                                kind: "request".into(),
                                guest_nonce: guest.clone(),
                                host_nonce: Some(host.clone()),
                                sequence: Some(sequence),
                                request: Some(request),
                                response: None,
                            },
                            deadline,
                        )?;
                        pending.insert(
                            sequence,
                            Awaited {
                                task,
                                id,
                                deadline,
                                method,
                            },
                        );
                        if pending.len() > 3 {
                            bail!("serial execution lane overflow")
                        }
                    }
                    let mut chunk = [0; 16384];
                    let mut received = false;
                    loop {
                        match s.read(&mut chunk) {
                            Ok(0) => bail!("serial execution EOF"),
                            Ok(n) => {
                                received = true;
                                bytes.extend_from_slice(&chunk[..n]);
                                if bytes.len() > 1048580 + 16384 {
                                    bail!("serial response buffer bound")
                                };
                                break;
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(e) => return Err(e.into()),
                        }
                    }
                    while bytes.len() >= 4 {
                        let n = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
                        if n == 0 || n > 1048576 {
                            bail!("serial response size")
                        };
                        if bytes.len() < n + 4 {
                            break;
                        };
                        let reply: Envelope = serde_json::from_slice(&bytes[4..n + 4])?;
                        bytes.drain(..n + 4);
                        if reply.version != 2
                            || reply.kind != "response"
                            || reply.guest_nonce != guest
                            || reply.host_nonce.as_deref() != Some(&host)
                            || reply.request.is_some()
                        {
                            bail!("serial response incarnation mismatch")
                        }
                        let record = pending
                            .remove(&reply.sequence.context("response sequence missing")?)
                            .context("unsolicited serial response")?;
                        if Instant::now() >= record.deadline {
                            bail!("late serial execution response")
                        };
                        let response = reply.response.context("missing serial response")?;
                        if response["id"] != record.id
                            || response.get("result").is_some() == response.get("error").is_some()
                        {
                            bail!("serial response identity/result invalid")
                        }
                        validate_reply(&record.method, &response)?;
                        if let Some(task) = record.task {
                            task.lane.store(false, Ordering::Release);
                            let _ = task.reply.send(Ok(response));
                        }
                    }
                    // Idle: nothing awaited, nothing buffered, nothing just read. The Guest only
                    // speaks in reply to a request, so the socket carries nothing we must act on
                    // except EOF or a protocol violation; both still fence, at most 50 ms later.
                    // Block on the channel so a new task wakes the loop at once, bounded by the
                    // next heartbeat so its schedule is unchanged, and by 50 ms so stop, EOF and
                    // unsolicited bytes are still observed promptly.
                    if !received && pending.is_empty() && bytes.is_empty() {
                        let wait = next_heartbeat
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(50));
                        // Timeout and disconnect both fall through: the next pass re-checks stop
                        // first and then reports a closed owner exactly as before.
                        if let Ok(t) = rx.recv_timeout(wait) {
                            carried = Some(t);
                        }
                    } else {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            };
            // The only record of why an incarnation was fenced (and its QEMU terminated).
            if let Err(error) = run() {
                eprintln!("vm-qemu: serial execution fenced: {error:#}");
            }
            stopped.store(true, Ordering::Release);
            *status.lock().unwrap() = "fenced";
            saved.lock().unwrap().stop();
        });
        Self {
            tx,
            state,
            stop,
            paused,
            lane: Arc::new(AtomicBool::new(false)),
            stream,
            #[cfg(test)]
            iterations,
        }
    }
    #[cfg(test)]
    pub fn request(&self, request: Value) -> Result<Pending> {
        self.request_before(request, Instant::now() + Duration::from_secs(5))
    }
    pub fn request_before(&self, request: Value, admitted_deadline: Instant) -> Result<Pending> {
        if Instant::now() >= admitted_deadline {
            bail!("serial execution dispatch deadline expired")
        }

        let method = request["method"].as_str().context("method required")?;
        if ![
            "info",
            "exec_start",
            "exec_poll",
            "exec_cancel",
            vm_control::input::TEXT_GUEST_METHOD,
        ]
        .contains(&method)
        {
            bail!("unsupported serial execution method")
        }
        if self.state() != "ready" || self.stop.load(Ordering::Acquire) {
            bail!("serial execution unavailable or fenced")
        }
        if self.paused.load(Ordering::Acquire) {
            bail!("serial execution unavailable: the VM is paused")
        }
        let lane = self.lane.clone();
        if lane.swap(true, Ordering::AcqRel) {
            bail!("serial execution busy")
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let deadline = admitted_deadline.min(
            Instant::now()
                + Duration::from_secs(match method {
                    // One text chunk; the Guest bounds its own work to 3 s (quiet-period waits).
                    // Exceeding this deadline fences the incarnation like any other late reply.
                    vm_control::input::TEXT_GUEST_METHOD => 4,
                    _ => 1,
                }),
        );
        if self
            .tx
            .try_send(Task {
                request,
                deadline,
                reply: tx,
                cancelled: cancelled.clone(),
                lane: lane.clone(),
            })
            .is_err()
        {
            lane.store(false, Ordering::Release);
            bail!("serial execution queue unavailable")
        }
        Ok(Pending {
            rx,
            cancelled,
            stop: self.stop.clone(),
            state: self.state.clone(),
            stream: self.stream.clone(),
            done: false,
        })
    }
}
impl Drop for Transport {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.stream.lock().unwrap().stop();
    }
}
fn validate_reply(method: &str, response: &Value) -> Result<()> {
    if response.get("error").is_some() {
        if method == "heartbeat" {
            bail!("serial heartbeat rejected")
        }
        return Ok(());
    }
    let r = &response["result"];
    match method {
        "heartbeat" if r != &json!({"alive":true}) => bail!("invalid serial heartbeat proof"),
        "info"
            if r["transport"] != "virtio-serial-private-exec-v2"
                || r["execution_containment"] != "cgroup-v2-clone3-kill"
                // A rootless service, or an OCI micro-VM's container root, as the Guest admits.
                || !(r["execution_identity"]["rootless"] == true
                    || (r["execution_identity"]["mode"] == "container-root"
                        && r["execution_identity"]["chroot"]
                            .as_str()
                            .is_some_and(|c| !c.is_empty())))
                || r["transport_reconnect"] != false
                || r["capabilities"]["exec"] != true
                || r["capabilities"]["async_exec"] != true
                || r["capabilities"]["descendant_containment"] != true
                || ["files", "shutdown", "desktop"]
                    .iter()
                    .any(|k| r["capabilities"][k] != false) =>
        {
            bail!("serial execution capability contract rejected")
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disconnected_task_publishes_fence_and_closes_stream_before_error_is_observed() {
        let directory = tempfile::Builder::new()
            .prefix("v2fence-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = directory.path().join("s");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let lifetime = Arc::new(Mutex::new(
            super::super::guest_transport::Lifetime::default(),
        ));
        let _socket = super::super::guest_transport::connect_private(&path, &lifetime).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let state = Arc::new(Mutex::new("ready"));
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut pending = Pending {
            rx: receiver,
            cancelled: Arc::new(AtomicBool::new(false)),
            stop: stop.clone(),
            state: state.clone(),
            stream: lifetime.clone(),
            done: false,
        };
        // Deliberately reproduce the interval before any worker epilogue runs.
        drop(sender);
        assert!(pending.poll().unwrap().is_err());
        assert!(stop.load(Ordering::Acquire));
        assert_eq!(*state.lock().unwrap(), "fenced");
        assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
        assert!(super::super::guest_transport::connect_private(&path, &lifetime).is_err());
    }
    fn hello(s: &mut UnixStream) -> (String, String) {
        let g = "a".repeat(64);
        s.set_nonblocking(true).unwrap();
        send(
            s,
            &Envelope {
                version: 2,
                kind: "challenge".into(),
                guest_nonce: g.clone(),
                host_nonce: None,
                sequence: None,
                request: None,
                response: None,
            },
        )
        .unwrap();
        let h = initial(s, Instant::now() + Duration::from_secs(2)).unwrap();
        assert_eq!(h.kind, "hello");
        let host = h.host_nonce.unwrap();
        send(
            s,
            &Envelope {
                version: 2,
                kind: "ready".into(),
                guest_nonce: g.clone(),
                host_nonce: Some(host.clone()),
                sequence: None,
                request: None,
                response: None,
            },
        )
        .unwrap();
        (g, host)
    }
    fn reply(s: &mut UnixStream, g: &str, h: &str, r: Envelope, value: Value) {
        send(
            s,
            &Envelope {
                version: 2,
                kind: "response".into(),
                guest_nonce: g.into(),
                host_nonce: Some(h.into()),
                sequence: r.sequence,
                request: None,
                response: Some(json!({"id":r.request.unwrap()["id"],"result":value})),
            },
        )
        .unwrap();
    }
    fn poll(p: &mut Pending) -> Result<Value> {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(r) = p.poll() {
                return r;
            }
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn heartbeat_progresses_while_start_is_pending() {
        let d = tempfile::Builder::new()
            .prefix("v2-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = d.path().join("a.sock");
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let (g, h) = hello(&mut s);
            let mut held = None;
            let mut hb = 0;
            let mut last = 0;
            while held.is_none() || hb < 2 {
                let r = initial(&mut s, Instant::now() + Duration::from_secs(2)).unwrap();
                assert_eq!(r.sequence, Some(last + 1));
                last += 1;
                match r.request.as_ref().unwrap()["method"].as_str().unwrap() {
                    "heartbeat" => {
                        hb += 1;
                        reply(&mut s, &g, &h, r, json!({"alive":true}));
                    }
                    "exec_start" => held = Some(r),
                    _ => panic!("unexpected"),
                };
            }
            reply(
                &mut s,
                &g,
                &h,
                held.unwrap(),
                json!({"job_id":"b".repeat(32),"state":"running"}),
            );
            thread::sleep(Duration::from_millis(30));
        });
        let t = Transport::start(&path, "c".repeat(64));
        let until = Instant::now() + Duration::from_secs(2);
        while t.state() == "starting" {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        }
        let mut start = t
            .request_before(
                json!({"id":"start","method":"exec_start","argv":["true"],"timeout_ms":1000}),
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap();
        assert!(t.request(json!({"id":"other","method":"info"})).is_err());
        assert_eq!(poll(&mut start).unwrap()["id"], "start");
        server.join().unwrap();
    }
    #[test]
    fn idle_loop_blocks_between_heartbeats_and_wakes_for_a_task() {
        let d = tempfile::Builder::new()
            .prefix("v2idle-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = d.path().join("a.sock");
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let (g, h) = hello(&mut s);
            let mut heartbeats = 0;
            loop {
                let r = initial(&mut s, Instant::now() + Duration::from_secs(2)).unwrap();
                match r.request.as_ref().unwrap()["method"].as_str().unwrap() {
                    "heartbeat" => {
                        heartbeats += 1;
                        reply(&mut s, &g, &h, r, json!({"alive":true}));
                    }
                    "exec_poll" => {
                        reply(&mut s, &g, &h, r, json!({"state":"running"}));
                        break;
                    }
                    _ => panic!("unexpected"),
                }
            }
            thread::sleep(Duration::from_millis(30));
            heartbeats
        });
        let t = Transport::start(&path, "c".repeat(64));
        let until = Instant::now() + Duration::from_secs(2);
        while t.state() == "starting" {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        }
        // Let the first heartbeat settle, then observe a window spanning the next one.
        thread::sleep(Duration::from_millis(100));
        let before = t.iterations.load(Ordering::Relaxed);
        thread::sleep(Duration::from_millis(600));
        let passes = t.iterations.load(Ordering::Relaxed) - before;
        // A 2 ms poll makes about 300 passes in 600 ms; a 50 ms idle wait makes about 12
        // plus the few passes that carry one heartbeat exchange.
        assert!(passes > 0, "loop stopped running");
        assert!(passes < 60, "idle loop spun: {passes} passes in 600 ms");
        assert_eq!(t.state(), "ready");
        let mut job = t
            .request(json!({"id":"poll","method":"exec_poll","job_id":"b".repeat(32)}))
            .unwrap();
        assert_eq!(poll(&mut job).unwrap()["id"], "poll");
        assert!(server.join().unwrap() >= 2, "heartbeat schedule slipped");
    }
    #[test]
    fn a_paused_guest_is_not_fenced_and_heartbeats_resume() {
        let d = tempfile::Builder::new()
            .prefix("v2pause-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = d.path().join("a.sock");
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let frozen = Arc::new(AtomicBool::new(false));
        let thawed = Arc::new(AtomicBool::new(false));
        let (guest_frozen, guest_thawed) = (frozen.clone(), thawed.clone());
        let server = thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let (g, h) = hello(&mut s);
            let mut after = 0;
            loop {
                let r = initial(&mut s, Instant::now() + Duration::from_secs(5)).unwrap();
                match r.request.as_ref().unwrap()["method"].as_str().unwrap() {
                    "heartbeat" => {
                        // A stopped Guest answers only once its vCPUs run again.
                        while guest_frozen.load(Ordering::Acquire)
                            && !guest_thawed.load(Ordering::Acquire)
                        {
                            thread::sleep(Duration::from_millis(5));
                        }
                        if guest_thawed.load(Ordering::Acquire) {
                            after += 1;
                        }
                        reply(&mut s, &g, &h, r, json!({"alive":true}));
                    }
                    "exec_poll" => {
                        reply(&mut s, &g, &h, r, json!({"state":"running"}));
                        break;
                    }
                    _ => panic!("unexpected"),
                }
            }
            thread::sleep(Duration::from_millis(30));
            after
        });
        let t = Transport::start(&path, "c".repeat(64));
        let until = Instant::now() + Duration::from_secs(2);
        while t.state() == "starting" {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        }
        thread::sleep(Duration::from_millis(100));
        frozen.store(true, Ordering::Release);
        t.set_paused(true);
        assert!(t.request(json!({"id":"info","method":"info"})).is_err());
        // Longer than a heartbeat's 1 s deadline.
        thread::sleep(Duration::from_millis(1500));
        assert_eq!(
            t.state(),
            "ready",
            "a paused Guest's silence fenced the transport"
        );
        t.set_paused(false);
        thawed.store(true, Ordering::Release);
        thread::sleep(Duration::from_millis(700));
        let mut job = t
            .request(json!({"id":"poll","method":"exec_poll","job_id":"b".repeat(32)}))
            .unwrap();
        assert_eq!(poll(&mut job).unwrap()["id"], "poll");
        assert_eq!(t.state(), "ready");
        assert!(server.join().unwrap() >= 1, "heartbeats did not resume");
    }
    #[test]
    fn heartbeat_requires_exact_proof() {
        assert!(validate_reply("heartbeat", &json!({"result":{}})).is_err());
        assert!(validate_reply("heartbeat", &json!({"error":{"code":"timeout"}})).is_err());
    }
    #[test]
    fn info_accepts_a_rootless_service_or_a_container_root() {
        let info = |identity: Value| json!({"result":{"transport":"virtio-serial-private-exec-v2","execution_containment":"cgroup-v2-clone3-kill","execution_identity":identity,"transport_reconnect":false,"capabilities":{"exec":true,"async_exec":true,"descendant_containment":true,"files":false,"shutdown":false,"desktop":false}}});
        assert!(validate_reply("info", &info(json!({"rootless":true}))).is_ok());
        assert!(validate_reply(
            "info",
            &info(json!({"rootless":false,"mode":"container-root","chroot":"/run/veynode/root"}))
        )
        .is_ok());
        for identity in [
            json!({"rootless":false}),
            json!({"rootless":false,"mode":"container-root"}),
            json!({"rootless":false,"mode":"container-root","chroot":""}),
        ] {
            assert!(validate_reply("info", &info(identity)).is_err());
        }
    }
    #[test]
    #[ignore = "requires explicitly built Go test-only interop binary; not real Guest cgroup evidence"]
    fn actual_go_serial_v2_interop() {
        let binary = std::env::var_os("VEYNODE_SERIAL_EXEC_GO_TEST_BIN")
            .expect("Go test executable required");
        let directory = tempfile::Builder::new()
            .prefix("v2go-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = directory.path().join("agent.sock");
        struct Owned(std::process::Child);
        impl Drop for Owned {
            fn drop(&mut self) {
                if self.0.try_wait().ok().flatten().is_none() {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
        }
        let mut child = Owned(
            std::process::Command::new(binary)
                .arg("-test.run=^TestSerialExecInteropProcess$")
                .env("VEYNODE_SERIAL_EXEC_SOCKET", &path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let until = Instant::now() + Duration::from_secs(3);
        while !path.exists() {
            assert!(Instant::now() < until);
            assert!(child.0.try_wait().unwrap().is_none());
            thread::sleep(Duration::from_millis(5));
        }
        let transport = Transport::start(&path, "c".repeat(64));
        while transport.state() == "starting" {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(transport.state(), "ready");
        let mut start=transport.request(json!({"id":"start","method":"exec_start","argv":["/bin/sh","-c","sleep 20"],"timeout_ms":30000})).unwrap();
        let started = poll(&mut start).unwrap();
        let id = started["result"]["job_id"]
            .as_str()
            .expect("real Go start returned no job");
        let mut cancel = transport
            .request(json!({"id":"cancel","method":"exec_cancel","job_id":id}))
            .unwrap();
        poll(&mut cancel).unwrap();
        let mut job = transport
            .request(json!({"id":"poll","method":"exec_poll","job_id":id}))
            .unwrap();
        let result = poll(&mut job).unwrap();
        assert_eq!(result["result"]["state"], "finished");
        assert_eq!(result["result"]["result"]["cancelled"], true);
        drop(transport);
        let end = Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn foreign_nonce_replayed_sequence_and_failed_heartbeat_fence_before_eof() {
        for kind in ["nonce", "sequence", "version", "heartbeat_error"] {
            let directory = tempfile::Builder::new()
                .prefix("v2bad-")
                .tempdir_in("/tmp")
                .unwrap();
            let path = directory.path().join("s");
            let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let (tx, rx) = mpsc::channel();
            let server = thread::spawn(move || {
                let (mut s, _) = l.accept().unwrap();
                let (g, h) = hello(&mut s);
                let r = initial(&mut s, Instant::now() + Duration::from_secs(2)).unwrap();
                assert_eq!(r.request.as_ref().unwrap()["method"], "heartbeat");
                let mut response = Envelope {
                    version: 2,
                    kind: "response".into(),
                    guest_nonce: g,
                    host_nonce: Some(h),
                    sequence: r.sequence,
                    request: None,
                    response: Some(json!({"id":"heartbeat","result":{"alive":true}})),
                };
                match kind {
                    "nonce" => response.guest_nonce = "e".repeat(64),
                    "sequence" => response.sequence = Some(0),
                    "version" => response.version = 1,
                    "heartbeat_error" => {
                        response.response =
                            Some(json!({"id":"heartbeat","error":{"code":"unavailable"}}))
                    }
                    _ => unreachable!(),
                };
                send(&mut s, &response).unwrap();
                rx.recv_timeout(Duration::from_secs(2)).unwrap();
            });
            let t = Transport::start(&path, "c".repeat(64));
            let until = Instant::now() + Duration::from_secs(1);
            while t.state() != "fenced" {
                assert!(
                    Instant::now() < until,
                    "invalid response did not fence before server EOF"
                );
                thread::sleep(Duration::from_millis(2));
            }
            assert!(t.request(json!({"id":"info","method":"info"})).is_err());
            tx.send(()).unwrap();
            server.join().unwrap();
        }
    }
    #[test]
    fn original_dispatch_deadline_fences_partial_frame_before_delayed_reader_resumes() {
        use std::os::fd::AsRawFd;
        let directory = tempfile::Builder::new()
            .prefix("v2pressure-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = directory.path().join("s");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let (peer_ready, ready) = mpsc::channel();
        let (resume, reader_resume) = mpsc::channel();
        let peer = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let (guest, host) = hello(&mut s);
            let heartbeat = initial(&mut s, Instant::now() + Duration::from_secs(2)).unwrap();
            assert_eq!(heartbeat.request.as_ref().unwrap()["method"], "heartbeat");
            reply(&mut s, &guest, &host, heartbeat, json!({"alive":true}));
            peer_ready.send(()).unwrap();
            // No reads while the actual host socket fills and dispatch deadline expires.
            reader_resume.recv_timeout(Duration::from_secs(2)).unwrap();
            thread::sleep(Duration::from_millis(50));
            let mut bytes = Vec::new();
            let until = Instant::now() + Duration::from_secs(1);
            loop {
                let mut b = [0; 4096];
                match s.read(&mut b) {
                    Ok(0) => break,
                    Ok(n) => bytes.extend_from_slice(&b[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < until, "fenced stream never reached EOF");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => panic!("unexpected stream error: {e}"),
                }
            }
            assert!(bytes.len() >= 4, "no partial frame was written");
            let promised = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize + 4;
            assert!(
                bytes.len() < promised,
                "expired request was completed after reader resumed"
            );
            bytes.len()
        });
        let transport = Transport::start_configured(&path, "c".repeat(64), |socket| {
            let size: libc::c_int = 1024;
            if unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut pending=transport.request_before(json!({"id":"start","method":"exec_start","argv":["/bin/echo","x".repeat(7000),"y".repeat(7000),"z".repeat(7000),"w".repeat(7000)],"timeout_ms":1000}),Instant::now()+Duration::from_millis(20)).unwrap();
        // Reader resumes at 50ms, between the 20ms dispatch deadline and the old
        // independently reset 100ms write deadline. Old behavior completes this frame.
        resume.send(()).unwrap();
        assert!(poll(&mut pending).is_err());
        assert_eq!(transport.state(), "fenced");
        assert!(transport
            .request(json!({"id":"retry","method":"info"}))
            .is_err());
        assert!(peer.join().unwrap() > 4);
    }
    #[test]
    fn already_expired_write_sends_no_frame_prefix() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        b.set_nonblocking(true).unwrap();
        let e = Envelope {
            version: 2,
            kind: "request".into(),
            guest_nonce: "a".repeat(64),
            host_nonce: Some("b".repeat(64)),
            sequence: Some(1),
            request: Some(json!({"id":"h","method":"heartbeat"})),
            response: None,
        };
        assert!(send_until(&mut a, &e, Instant::now() - Duration::from_millis(1)).is_err());
        let mut bytes = [0; 4];
        assert_eq!(
            b.read(&mut bytes).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
