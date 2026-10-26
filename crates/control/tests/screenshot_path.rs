//! One screenshot through the real daemon and the real QEMU worker, against scripted peers (`screenshot_path_peers.py`):
//! a QEMU that only answers QMP and a console broker that answers `screenshot` after a configured delay. No Guest,
//! no hypervisor, no native helper. `budget` (ignored by default) puts a real `vm-console` in front of a scripted
//! helper when one is built beside the worker, and prints where the time of one screenshot goes.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const PEERS: &str = include_str!("screenshot_path_peers.py");
const SMALL: usize = 21 * 1024;
const LARGE: usize = 2_900_000;
const STDERR_TAIL_LINES: usize = 32;
const STDERR_LINE_BYTES: usize = 2048;

#[derive(Default)]
struct StderrTail(VecDeque<String>);
impl StderrTail {
    fn push(&mut self, mut line: String) {
        if line.len() > STDERR_LINE_BYTES {
            let mut end = STDERR_LINE_BYTES - " [truncated]".len();
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
            line.push_str(" [truncated]");
        }
        if self.0.len() == STDERR_TAIL_LINES {
            self.0.pop_front();
        }
        self.0.push_back(line);
    }
    fn snapshot(&self) -> String {
        if self.0.is_empty() {
            "[no non-timing stderr captured yet]".into()
        } else {
            self.0
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

fn monotonic_ns() -> u64 {
    let mut t: libc::timespec = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) },
        0
    );
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}
struct Rig {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    peers: PathBuf,
    daemon: Child,
    worker: i32,
    /// The daemon's `screenshot timing` stderr lines, in order.
    lines: Arc<Mutex<Vec<String>>>,
    /// The last 32 other stderr lines, at most 2 KiB each, as observed by the reader thread.
    stderr_tail: Arc<Mutex<StderrTail>>,
}
impl Drop for Rig {
    fn drop(&mut self) {
        // Daemon loss closes the worker's stdin; the worker then quits its QEMU, ends its broker and removes its
        // runtime directory by itself.
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let until = Instant::now() + Duration::from_secs(5);
        while unsafe { libc::kill(self.worker, 0) } == 0 && Instant::now() < until {
            thread::sleep(Duration::from_millis(10));
        }
    }
}
struct Shot {
    result: Result<Value, String>,
    /// Request written until the first reply byte, and from there to the end of the reply: the media service's
    /// `rpc_wait_ms` and `rpc_read_ms`.
    wait: Duration,
    read: Duration,
    done_ns: u64,
}
impl Rig {
    fn new(broker: Option<&Path>) -> Self {
        Self::with(broker, json!({}))
    }
    /// `initial` is in `peers.json` when the scripted broker starts (it announces its capabilities then).
    fn with(broker: Option<&Path>, initial: Value) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let peers = tmp.path().join("peers.py");
        std::fs::write(&peers, PEERS).unwrap();
        std::fs::set_permissions(&peers, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(tmp.path().join("resources")).unwrap();
        std::fs::write(tmp.path().join("kernel"), b"not bootable").unwrap();
        let dir = tmp.path().join("state");
        let mut rig = Self {
            dir: dir.clone(),
            peers: tmp.path().to_owned(),
            daemon: Command::new(env!("CARGO_BIN_EXE_vm-control"))
                .args(["daemon", "--state-dir"])
                .arg(&dir)
                .args(["--backend", "/usr/bin/false", "--qemu-worker"])
                .arg(env!("CARGO_BIN_EXE_vm-qemu"))
                .arg("--qemu-launcher")
                .arg(&peers)
                .arg("--qemu-library")
                .arg(&peers)
                .arg("--qemu-resources")
                .arg(tmp.path().join("resources"))
                .arg("--qemu-console-broker")
                .arg(broker.unwrap_or(&peers))
                .arg("--qemu-console-helper")
                .arg(&peers)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
            worker: 0,
            lines: Arc::default(),
            stderr_tail: Arc::default(),
            _tmp: tmp,
        };
        rig.configure(SMALL, 0, initial);
        let (stderr, lines) = (rig.daemon.stderr.take().unwrap(), rig.lines.clone());
        let stderr_tail = rig.stderr_tail.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if line.starts_with("screenshot timing ") {
                    lines.lock().unwrap().push(line);
                } else {
                    stderr_tail.lock().unwrap().push(line);
                }
            }
        });
        // The socket file appears at bind(), before the daemon listens: a connect in that window is refused. Wait until
        // the daemon actually answers (this was the intermittent failure of this rig, seen only under host load).
        rig.until("daemon socket", || {
            dir.join("control.sock").exists() && vm_control::call(&dir, "list", json!({})).is_ok()
        });
        let kernel = rig.peers.join("kernel");
        let created = vm_control::call(
            &dir,
            "create",
            json!({"id":"shot","backend":"qemu-hvf","guest":"linux","cpus":1,"memory_mib":256,"kernel":kernel}),
        );
        assert!(
            created.is_ok(),
            "screenshot rig create failed: {created:?}\ndaemon stderr tail (non-timing lines captured so far):\n{}",
            rig.stderr_tail.lock().unwrap().snapshot()
        );
        let started = vm_control::call(&dir, "start", json!({"id":"shot"}));
        assert!(
            started.is_ok(),
            "screenshot rig start failed: {started:?}\ndaemon stderr tail (non-timing lines captured so far):\n{}",
            rig.stderr_tail.lock().unwrap().snapshot()
        );
        rig.until("running worker", || {
            let status = vm_control::call(&dir, "status", json!({"id":"shot"})).unwrap();
            assert_ne!(status["state"], "error", "{status}");
            status["state"] == "running" && status["runtime"]["capture_connected"] == true
        });
        let status = vm_control::call(&dir, "status", json!({"id":"shot"})).unwrap();
        rig.worker = status["pid"].as_i64().expect("worker pid") as i32;
        rig
    }
    fn until(&mut self, what: &str, mut done: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(
                self.daemon.try_wait().unwrap().is_none(),
                "daemon exited waiting for {what}"
            );
            assert!(Instant::now() < until, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }
    /// What the scripted helper or broker answers from now on.
    fn configure(&self, png_bytes: usize, delay_ms: u64, extra: Value) {
        let (width, height) = if png_bytes > 1024 * 1024 {
            (2560, 1440)
        } else {
            (1280, 800)
        };
        let mut config =
            json!({"png_bytes":png_bytes,"delay_ms":delay_ms,"width":width,"height":height});
        config
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let staged = self.peers.join("peers.json.new");
        std::fs::write(&staged, config.to_string()).unwrap();
        std::fs::rename(staged, self.peers.join("peers.json")).unwrap();
    }
    /// The same exchange as the media service's `rpc_timed`, with blocking I/O and its 8 KiB reader.
    fn shot(&self) -> Shot {
        let mut stream = UnixStream::connect(self.dir.join("control.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let started = Instant::now();
        stream
            .write_all(b"{\"method\":\"screenshot\",\"arguments\":{\"id\":\"shot\"}}\n")
            .unwrap();
        let mut input = BufReader::new(stream).take(vm_control::MAX_SCREENSHOT_RESPONSE as u64 + 1);
        input.fill_buf().unwrap();
        let wait = started.elapsed();
        let mut frame = Vec::new();
        input.read_until(b'\n', &mut frame).unwrap();
        let (read, done_ns) = (started.elapsed() - wait, monotonic_ns());
        assert_eq!(
            frame.last(),
            Some(&b'\n'),
            "reply is one terminated line within the response bound"
        );
        let mut reply: Value = serde_json::from_slice(&frame).unwrap();
        let result = match reply.get("error") {
            Some(error) => Err(error.to_string()),
            None => Ok(reply["result"].take()),
        };
        Shot {
            result,
            wait,
            read,
            done_ns,
        }
    }
    /// One `screenshot` exchange with `arguments` exactly as given; the daemon's whole reply object.
    fn reply(&self, arguments: Value) -> Value {
        let mut stream = UnixStream::connect(self.dir.join("control.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let request = json!({"method":"screenshot","arguments":arguments});
        stream.write_all(format!("{request}\n").as_bytes()).unwrap();
        let mut frame = Vec::new();
        BufReader::new(stream)
            .take(vm_control::MAX_SCREENSHOT_RESPONSE as u64 + 1)
            .read_until(b'\n', &mut frame)
            .unwrap();
        assert_eq!(
            frame.last(),
            Some(&b'\n'),
            "reply is one terminated line within the response bound"
        );
        serde_json::from_slice(&frame).unwrap()
    }
    /// Every request line the scripted broker has read, byte for byte, without the line end.
    fn requests(&self) -> Vec<String> {
        let log = std::fs::read_to_string(self.peers.join("requests.log")).unwrap_or_default();
        log.lines().map(str::to_owned).collect()
    }
    /// The same exchange in a task of a multi-thread tokio runtime with tokio's buffered reader, as the media
    /// service runs it (its runtime is busy with media as well; this one is not).
    fn shot_async(&self, runtime: &tokio::runtime::Runtime) -> (Duration, Duration) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
        let path = self.dir.join("control.sock");
        let task = runtime.spawn(async move {
            let started = Instant::now();
            let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
            stream
                .write_all(b"{\"method\":\"screenshot\",\"arguments\":{\"id\":\"shot\"}}\n")
                .await
                .unwrap();
            let mut input = tokio::io::BufReader::new(stream)
                .take(vm_control::MAX_SCREENSHOT_RESPONSE as u64 + 1);
            input.fill_buf().await.unwrap();
            let wait = started.elapsed();
            let mut frame = Vec::new();
            input.read_until(b'\n', &mut frame).await.unwrap();
            assert!(frame.len() > 1024 && frame.last() == Some(&b'\n'));
            (wait, started.elapsed() - wait)
        });
        runtime.block_on(task).unwrap()
    }
    /// The last line a scripted peer logged: request read, reply write begun, reply write complete (monotonic ns).
    fn peer_stamps(&self) -> [u64; 3] {
        let log = std::fs::read_to_string(self.peers.join("peers.log")).unwrap();
        let mut fields = log
            .lines()
            .last()
            .unwrap()
            .split(' ')
            .skip(1)
            .map(|n| n.parse().unwrap());
        [
            fields.next().unwrap(),
            fields.next().unwrap(),
            fields.next().unwrap(),
        ]
    }
    fn sent(&self, png_bytes: usize) -> Value {
        serde_json::from_slice(
            &std::fs::read(self.peers.join(format!("sent-{png_bytes}.json"))).unwrap(),
        )
        .unwrap()
    }
    /// Scheduler wake-ups the worker process has accumulated, as the kernel counts them.
    fn worker_wakeups(&self) -> u64 {
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        let status = unsafe {
            libc::proc_pid_rusage(
                self.worker,
                libc::RUSAGE_INFO_V2,
                (&mut info as *mut libc::rusage_info_v2).cast(),
            )
        };
        assert_eq!(status, 0, "worker resource usage unavailable");
        info.ri_pkg_idle_wkups + info.ri_interrupt_wkups
    }
    fn idle_wakeups_per_second(&self) -> u64 {
        let before = self.worker_wakeups();
        thread::sleep(Duration::from_secs(1));
        self.worker_wakeups() - before
    }
}
fn field(line: &str, name: &str) -> u64 {
    line.split(' ')
        .find_map(|part| part.strip_prefix(name)?.strip_prefix('='))
        .unwrap_or_else(|| panic!("{name} missing in {line}"))
        .parse()
        .unwrap()
}

#[test]
fn stderr_tail_keeps_recent_lines_with_bounded_utf8_text() {
    let mut tail = StderrTail::default();
    assert_eq!(tail.snapshot(), "[no non-timing stderr captured yet]");
    for index in 0..STDERR_TAIL_LINES + 3 {
        tail.push(format!("line {index}"));
    }
    let expected = (3..STDERR_TAIL_LINES + 3)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(tail.snapshot(), expected);
    let long = "界".repeat(STDERR_LINE_BYTES);
    tail.push(long.clone());
    assert_eq!(tail.0.len(), STDERR_TAIL_LINES);
    let clipped = tail.0.back().unwrap();
    assert!(clipped.len() <= STDERR_LINE_BYTES);
    assert!(clipped.ends_with(" [truncated]"));
    assert!(long.starts_with(clipped.strip_suffix(" [truncated]").unwrap()));
    assert_eq!(tail.snapshot().lines().count(), STDERR_TAIL_LINES);
}

#[test]
fn reply_follows_a_delayed_broker_answer_at_once() {
    let rig = Rig::new(None);
    rig.shot().result.unwrap();
    // Delays that end at different points of the worker's 50 ms step: without a wake on completion the reply
    // waits for the next step, 15 to 40 ms here.
    let mut lags = Vec::new();
    for delay in [10, 35, 60, 85, 110] {
        rig.configure(SMALL, delay, json!({}));
        let shot = rig.shot();
        shot.result.unwrap();
        let [received, _, answered] = rig.peer_stamps();
        assert!(
            answered - received >= delay * 1_000_000,
            "scripted delay was honoured"
        );
        lags.push((shot.done_ns.saturating_sub(answered)) / 1000);
    }
    println!("reply after the broker's answer, us, at delays 10/35/60/85/110 ms: {lags:?}");
    assert!(
        lags.iter().all(|lag| *lag < 12_000),
        "reply did not follow the broker's answer at once: {lags:?} us"
    );
}

#[test]
fn forwarded_picture_is_byte_identical_and_the_bounds_still_refuse() {
    let rig = Rig::new(None);
    let largest = vm_control::MAX_SCREENSHOT_BYTES;
    for size in [SMALL, LARGE, largest] {
        rig.configure(size, 0, json!({}));
        let result = rig.shot().result.unwrap();
        let sent = rig.sent(size);
        assert!(
            result["data_base64"] == sent["data_base64"],
            "picture bytes changed on the way at {size}"
        );
        assert_eq!(
            result["data_base64"].as_str().unwrap().len(),
            size.div_ceil(3) * 4
        );
        let mut rest = result.clone();
        let object = rest.as_object_mut().unwrap();
        object.remove("data_base64");
        assert_eq!(
            object.remove("frame_id").unwrap().as_str().unwrap().len(),
            36
        );
        assert_eq!(
            rest,
            json!({"byte_length":size,"captured_at_us":1,"captured_monotonic_us":2,"coordinate_space":"guest-physical-pixels","display_id":0,"guest_ui_scale":null,"height":sent["height"],"mime_type":"image/png","source_generation":3,"source_sequence":4,"width":sent["width"]}),
            "metadata changed on the way"
        );
    }
    let timing = rig
        .lines
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("daemon printed a timing line");
    let reply = field(&timing, "bytes") as usize;
    assert!(
        (largest.div_ceil(3) * 4..largest.div_ceil(3) * 4 + 1024).contains(&reply),
        "{timing}"
    );
    assert!(
        !timing.contains('/') && !timing.contains('"'),
        "timing line carries numbers only: {timing}"
    );
    assert!(
        field(&timing, "broker_read_us") > 0 && field(&timing, "pipe_us") > 0,
        "worker stages reached the daemon"
    );
    // One byte over the PNG bound, honestly declared: the reply still fits the 12 MiB frame and is refused on its
    // declared size.
    rig.configure(largest + 1, 0, json!({}));
    let refused = rig.shot().result.unwrap_err();
    assert!(
        refused.contains("invalid console screenshot metadata"),
        "{refused}"
    );
    // The same picture declared as within the bound: refused by the daemon's decoded-size check.
    rig.configure(largest + 1, 0, json!({"declared":largest}));
    let refused = rig.shot().result.unwrap_err();
    assert!(refused.contains("screenshot size mismatch"), "{refused}");
    // A record one byte over the 12 MiB response bound is refused on its header.
    rig.configure(SMALL, 0, json!({"mode":"oversize_frame"}));
    let refused = rig.shot().result.unwrap_err();
    assert!(
        refused.contains("invalid or oversized console screenshot frame"),
        "{refused}"
    );
    rig.configure(SMALL, 0, json!({}));
    assert!(rig.shot().result.unwrap()["data_base64"] == rig.sent(SMALL)["data_base64"]);
}

#[test]
fn idle_worker_wakes_no_more_after_captures_than_before_any() {
    let rig = Rig::new(None);
    thread::sleep(Duration::from_millis(300));
    let before = rig.idle_wakeups_per_second();
    for delay in [0, 20, 40] {
        rig.configure(LARGE, delay, json!({}));
        rig.shot().result.unwrap();
    }
    thread::sleep(Duration::from_millis(300));
    let after = rig.idle_wakeups_per_second();
    println!("idle worker wake-ups per second: {before} before any capture, {after} after three");
    // The kernel counts every thread of the process and unrelated timer interrupts too, hence the slack; a wake
    // source left behind by a capture would add at least the 20 a second of the in-flight step.
    assert!(
        after <= before + 5,
        "idle worker wakes more often: {before} -> {after} per second"
    );
}

fn median(mut values: Vec<u64>) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}
/// `cargo test --release -p vm-control --test screenshot_path budget -- --ignored --nocapture`, after
/// `cargo build --release -p vm-media-local --bin vm-console` for the real broker.
#[test]
#[ignore = "prints a time budget; run alone, in release"]
fn budget() {
    let real = Path::new(env!("CARGO_BIN_EXE_vm-qemu")).with_file_name("vm-console");
    let real = std::env::var_os("VEYNODE_TEST_VM_CONSOLE")
        .map(PathBuf::from)
        .or(real.is_file().then_some(real));
    println!(
        "broker: {}",
        if real.is_some() {
            "real vm-console, scripted helper"
        } else {
            "scripted (no vm-console built)"
        }
    );
    let rig = Rig::new(real.as_deref());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    println!(
        "idle worker wake-ups per second before any capture: {}",
        rig.idle_wakeups_per_second()
    );
    for (size, delays) in [(SMALL, [0u64, 8]), (LARGE, [0, 215])] {
        for delay in delays {
            rig.configure(size, delay, json!({}));
            rig.shot().result.unwrap();
            let names = [
                "total",
                "client_wait",
                "client_read",
                "peer_delay",
                "peer_write",
                "broker_wait",
                "broker_plumbing",
                "broker_read",
                "worker_parse",
                "worker_wake",
                "pipe",
                "daemon_parse",
                "normalize",
                "write",
                "backend",
                "daemon_total",
                "tokio_wait",
                "tokio_read",
            ];
            let mut samples = vec![Vec::new(); names.len()];
            for _ in 0..25 {
                let shot = rig.shot();
                shot.result.unwrap();
                let [received, started, written] = rig.peer_stamps();
                thread::sleep(Duration::from_millis(5));
                // A daemon older than the timing line prints none: its stages read zero.
                let line = rig.lines.lock().unwrap().last().cloned();
                let field = |name: &str| line.as_deref().map_or(0, |line| field(line, name));
                let peer_delay = (started - received) / 1000;
                let row = [
                    (shot.wait + shot.read).as_micros() as u64,
                    shot.wait.as_micros() as u64,
                    shot.read.as_micros() as u64,
                    peer_delay,
                    (written - started) / 1000,
                    field("broker_wait_us"),
                    field("broker_wait_us").saturating_sub(peer_delay),
                    field("broker_read_us"),
                    field("worker_parse_us"),
                    field("worker_wake_us"),
                    field("pipe_us"),
                    field("parse_us"),
                    field("normalize_us"),
                    field("write_us"),
                    field("backend_us"),
                    field("total_us"),
                ];
                for (index, value) in row.into_iter().enumerate() {
                    samples[index].push(value);
                }
                let (wait, read) = rig.shot_async(&runtime);
                samples[16].push(wait.as_micros() as u64);
                samples[17].push(read.as_micros() as u64);
            }
            let bytes = rig
                .lines
                .lock()
                .unwrap()
                .last()
                .map_or(0, |line| field(line, "bytes"));
            print!("png={size} reply={bytes} helper_delay_ms={delay} n=25 median_us:");
            for (name, values) in names.iter().zip(samples) {
                print!(" {name}={}", median(values));
            }
            println!();
        }
    }
    if real.is_some() {
        // The native helper refuses a request until its 10 ms owner tick has seen the previous reply written.
        rig.configure(SMALL, 3, json!({"busy_ms":10}));
        let mut totals = Vec::new();
        for _ in 0..25 {
            let shot = rig.shot();
            shot.result
                .expect("a request right after a reply waits out the helper's busy window");
            totals.push((shot.wait + shot.read).as_micros() as u64);
        }
        let log = std::fs::read_to_string(rig.peers.join("peers.log")).unwrap();
        let refusals = log.lines().filter(|line| line.starts_with("busy ")).count();
        println!(
            "back to back, helper busy for 10 ms after each reply, helper_delay_ms=3: n=25, no refusal reached the \
             client, {refusals} absorbed by the broker, median total_us={}",
            median(totals)
        );
    }
    thread::sleep(Duration::from_millis(300));
    println!(
        "idle worker wake-ups per second after the captures: {}",
        rig.idle_wakeups_per_second()
    );
}

fn ask() -> Value {
    json!({"id":"shot","format":"jpeg","max_width":1280,"max_height":720,"quality":50})
}
/// The broker request of the PNG form, byte for byte: `{"method":"screenshot","request_id":"screenshot-<number>"}`.
fn assert_plain_request_line(line: &str) {
    let number = line
        .strip_prefix("{\"method\":\"screenshot\",\"request_id\":\"screenshot-")
        .and_then(|rest| rest.strip_suffix("\"}"))
        .unwrap_or_else(|| panic!("request line: {line}"));
    assert!(
        !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()),
        "{line}"
    );
}

#[test]
fn scaled_form_reaches_the_broker_as_asked_and_its_jpeg_is_checked_on_the_way_back() {
    let rig = Rig::new(None);
    // A request without the four fields is written to the broker exactly as before, and after a scaled one too.
    rig.configure(LARGE, 0, json!({}));
    let before = rig.shot().result.unwrap();
    assert_eq!(rig.requests().len(), 1);
    assert_plain_request_line(&rig.requests()[0]);

    let reply = rig.reply(ask());
    let line = rig.requests().pop().unwrap();
    let number = line
        .strip_prefix("{\"format\":\"jpeg\",\"max_height\":720,\"max_width\":1280,\"method\":\"screenshot\",\"quality\":50,\"request_id\":\"screenshot-")
        .and_then(|rest| rest.strip_suffix("\"}"))
        .unwrap_or_else(|| panic!("scaled request line: {line}"));
    assert!(number.bytes().all(|b| b.is_ascii_digit()), "{line}");
    assert_eq!(
        reply.as_object().unwrap().len(),
        1,
        "a result and nothing else"
    );
    let result = &reply["result"];
    let sent: Value =
        serde_json::from_slice(&std::fs::read(rig.peers.join("sent-scaled.json")).unwrap())
            .unwrap();
    assert!(
        result["data_base64"] == sent["data_base64"],
        "JPEG bytes changed on the way"
    );
    let mut rest = result.clone();
    let object = rest.as_object_mut().unwrap();
    assert_eq!(
        object.remove("frame_id").unwrap().as_str().unwrap().len(),
        36
    );
    // The helper's fields untouched (source size in width/height), plus what the worker adds to every screenshot.
    let mut expected = sent.clone();
    expected["display_id"] = json!(0);
    expected["guest_ui_scale"] = Value::Null;
    assert_eq!(rest, expected, "metadata changed on the way");
    assert_eq!(
        (
            &result["mime_type"],
            &result["width"],
            &result["height"],
            &result["frame_width"],
            &result["frame_height"]
        ),
        (
            &json!("image/jpeg"),
            &json!(2560),
            &json!(1440),
            &json!(1280),
            &json!(720)
        )
    );
    assert_eq!(
        (&result["scale_divisor"], &result["frame_quality"]),
        (&json!(2), &json!(50))
    );

    // Exactly the 700 KiB bound passes; one byte more is refused, declared honestly or not.
    let bound = vm_control::screenshot_scaled::MAX_BYTES;
    rig.configure(LARGE, 0, json!({"jpeg_bytes":bound}));
    assert_eq!(rig.reply(ask())["result"]["byte_length"], bound);
    rig.configure(LARGE, 0, json!({"jpeg_bytes":bound + 1}));
    let refused = rig.reply(ask());
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("invalid console screenshot metadata"),
        "{refused}"
    );
    rig.configure(LARGE, 0, json!({"jpeg_bytes":bound + 1,"declared":bound}));
    let refused = rig.reply(ask());
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("invalid console scaled screenshot"),
        "{refused}"
    );
    // A reply that disagrees with the request or with itself never reaches the caller.
    for fault in [
        "wrong_dimensions",
        "wrong_divisor",
        "wrong_quality",
        "ignored_bound",
    ] {
        rig.configure(LARGE, 0, json!({"scaled":fault}));
        let refused = rig.reply(ask());
        assert!(
            refused["error"]
                .as_str()
                .is_some_and(|error| error.contains("invalid console scaled screenshot")),
            "{fault}: {refused}"
        );
        assert_eq!(
            refused.as_object().unwrap().len(),
            1,
            "{fault}: an ordinary error, no refusal word"
        );
    }
    // A helper without the form answers with its PNG: forwarded as any PNG, for the caller to recognise.
    rig.configure(LARGE, 0, json!({"scaled":"png"}));
    let png = rig.reply(ask());
    assert_eq!(png["result"]["mime_type"], "image/png");
    assert!(png["result"]["data_base64"] == rig.sent(LARGE)["data_base64"]);
    assert!(png["result"].get("frame_width").is_none());

    // The broker's refusals keep their fixed word beside the text, hop by hop; nothing else becomes a word.
    for word in [
        "encoded_size_limit",
        "invalid_request",
        "format_unavailable",
        "scale_unavailable",
    ] {
        rig.configure(LARGE, 0, json!({"scaled":format!("refuse:{word}")}));
        let refused = rig.reply(ask());
        assert_eq!(refused["screenshot_refusal"], word);
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("native operation failed: scripted"),
            "{refused}"
        );
        assert_eq!(refused.as_object().unwrap().len(), 2);
    }
    rig.configure(LARGE, 0, json!({"scaled":"refuse:GUEST_TEXT"}));
    let refused = rig.reply(ask());
    assert_eq!(refused.as_object().unwrap().len(), 1, "{refused}");
    assert!(refused["error"].is_string());
    let asked = rig.requests().len();

    // The daemon refuses a partial or out-of-range request itself: nothing reaches the worker's broker.
    rig.configure(LARGE, 0, json!({}));
    for (field, value) in [
        ("format", json!("png")),
        ("max_width", json!(1)),
        ("max_width", json!(3841)),
        ("max_height", json!(2161)),
        ("max_height", json!(720.0)),
        ("quality", json!(0)),
        ("quality", json!(101)),
        ("quality", json!("50")),
        ("quality", Value::Null),
    ] {
        let mut bad = ask();
        bad[field] = value;
        let refused = rig.reply(bad.clone());
        assert_eq!(refused["screenshot_refusal"], "invalid_request", "{bad}");
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .starts_with("invalid scaled screenshot request"),
            "{refused}"
        );
    }
    for field in ["format", "max_width", "max_height", "quality"] {
        let mut partial = ask();
        partial.as_object_mut().unwrap().remove(field);
        assert_eq!(
            rig.reply(partial)["screenshot_refusal"],
            "invalid_request",
            "without {field}"
        );
        assert_eq!(
            rig.reply(json!({"id":"shot", field: ask()[field]}))["screenshot_refusal"],
            "invalid_request",
            "{field} alone"
        );
    }
    assert_eq!(
        rig.requests().len(),
        asked,
        "a refused request was forwarded"
    );

    // After all of that, the request without the fields and its reply are what they were at the start.
    let after = rig.shot().result.unwrap();
    assert_plain_request_line(rig.requests().last().unwrap());
    let strip = |mut value: Value| {
        value.as_object_mut().unwrap().remove("frame_id");
        value
    };
    assert_eq!(strip(after), strip(before));
    // An error reply to it has one field, as before.
    rig.configure(vm_control::MAX_SCREENSHOT_BYTES + 1, 0, json!({}));
    let refused = rig.reply(json!({"id":"shot"}));
    assert_eq!(refused.as_object().unwrap().len(), 1);
    assert!(refused["error"]
        .as_str()
        .unwrap()
        .contains("invalid console screenshot metadata"));
}

#[test]
fn console_broker_from_before_the_scaled_form_is_never_sent_it() {
    let rig = Rig::with(None, json!({"old_broker":true}));
    let refused = rig.reply(ask());
    assert_eq!(refused["screenshot_refusal"], "format_unavailable");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("this console broker has no scaled JPEG screenshot form"),
        "{refused}"
    );
    assert!(
        rig.requests().is_empty(),
        "the old broker was sent the new fields"
    );
    // The PNG form is untouched.
    let result = rig.shot().result.unwrap();
    assert!(result["data_base64"] == rig.sent(SMALL)["data_base64"]);
    assert_plain_request_line(&rig.requests()[0]);
}

#[test]
fn mcp_screenshot_scales_on_the_host_and_asks_the_broker_for_png_only() {
    let rig = Rig::new(None);
    let mut child = Command::new(env!("CARGO_BIN_EXE_vm-control"))
        .args(["mcp", "--state-dir"])
        .arg(&rig.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut exchange = |request: Value| -> Value {
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let started = exchange(
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"integration-test","version":"1"}}}),
    );
    assert_eq!(started["result"]["protocolVersion"], vm_control::VERSION);
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    let mut exchange = |request: Value| -> Value {
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let call = |id: u64, arguments: Value| json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"vm_screenshot","arguments":arguments}});
    // MCP scaling is done on the host (`agent_view`, through vm-vision): the fields pass the MCP schema, and
    // the broker is still asked only for its PNG form. This rig registers no vm-vision, so the host refuses
    // after capture rather than passing the fields on.
    for (index, arguments) in [
        ask(),
        json!({"id":"shot","max_width":1280}),
        json!({"id":"shot","format":"jpeg","quality":50}),
    ]
    .into_iter()
    .enumerate()
    {
        let scaled = exchange(call(9 + index as u64, arguments));
        assert!(scaled.get("error").is_none(), "{scaled}");
        assert_eq!(scaled["result"]["isError"], true, "{scaled}");
        assert!(
            scaled["result"]["content"][0]["text"]
                .as_str()
                .is_some_and(|t| t.contains("text recognition helper not registered")),
            "{scaled}"
        );
    }
    let before = rig.requests().len();
    for line in rig.requests() {
        assert_plain_request_line(&line);
    }
    let shot = exchange(call(2, json!({"id":"shot"})));
    let content = &shot["result"]["content"];
    assert_eq!(
        shot["result"]["isError"], false,
        "{}",
        shot["result"]["content"][1]
    );
    assert_eq!(content[0]["mimeType"], "image/png");
    assert!(
        content[0]["data"] == rig.sent(SMALL)["data_base64"],
        "MCP picture differs from the helper's PNG"
    );
    let text: Value = serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["mime_type"], "image/png");
    assert!(text.get("frame_width").is_none() && text.get("scale_divisor").is_none());
    assert_eq!(rig.requests().len(), before + 1);
    assert_plain_request_line(&rig.requests()[before]);
    drop(input);
    let _ = child.wait();
}
