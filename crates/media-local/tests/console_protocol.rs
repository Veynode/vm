//! Actual broker process and Unix sockets, using an adversarial native-protocol
//! fixture. These tests establish no VM, SPICE, codec or GPU acceptance.
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Barrier},
    thread,
    time::{Duration, Instant},
};
use vm_media_local::console_wire::{read_record, Record};

struct Broker {
    directory: PathBuf,
    child: Child,
}

struct Shim(Child);
impl Shim {
    fn start(broker: &Broker) -> Self {
        Self(
            Command::new(env!("CARGO_BIN_EXE_vm-console-stream"))
                .arg(broker.directory.join("console.sock"))
                .args(["--codec", "h264"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        )
    }
    fn stop_line_then_eof(&mut self) -> (std::process::ExitStatus, String) {
        let mut input = self.0.stdin.take().unwrap();
        input.write_all(b"stop\n").unwrap();
        drop(input);
        let deadline = Instant::now() + Duration::from_secs(4);
        let status = loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "stream shim did not stop");
            thread::sleep(Duration::from_millis(10));
        };
        let mut stderr = String::new();
        self.0
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        (status, stderr)
    }
}
impl Drop for Shim {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

#[test]
fn stream_stop_line_then_eof_waits_for_delayed_ack_and_releases_owner() {
    let mut broker = Broker::start("delayed-stop");
    let mut shim = Shim::start(&broker);
    let (status, stderr) = shim.stop_line_then_eof();
    assert!(status.success(), "shim cleanup failed: {stderr}");
    assert!(!stderr.contains("stop unconfirmed"));
    let log = broker.log();
    let epoch = log.iter().find(|v| v["method"] == "start").unwrap()["epoch"]
        .as_u64()
        .unwrap();
    assert_eq!(
        log.iter().filter(|v| v["method"] == "stop").count(),
        1,
        "stdin stop plus EOF must not send duplicate native stops"
    );
    assert!(broker.child.try_wait().unwrap().is_none());
    assert!(broker.connect().start("fresh-after-shim") > epoch);
}

#[test]
fn stream_failed_native_stop_exits_nonzero_even_after_stdin_eof() {
    let broker = Broker::start("stop-error");
    let mut shim = Shim::start(&broker);
    let (status, stderr) = shim.stop_line_then_eof();
    assert!(
        !status.success(),
        "failed native cleanup was accepted: {stderr}"
    );
    assert!(
        stderr.contains("stop unconfirmed"),
        "missing cleanup error: {stderr}"
    );
    assert_eq!(
        broker
            .log()
            .iter()
            .filter(|v| v["method"] == "stop")
            .count(),
        1
    );
}
impl Broker {
    fn start(mode: &str) -> Self {
        let directory = std::env::temp_dir().join(format!("vc-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let encoder = directory.join("native-fixture.py");
        fs::write(&encoder, include_bytes!("fixtures/console_native.py")).unwrap();
        fs::set_permissions(&encoder, fs::Permissions::from_mode(0o700)).unwrap();
        // The native fixture deliberately ignores this file. No VM is started.
        fs::write(directory.join("spice-placeholder"), b"protocol fixture").unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_vm-console"))
            .args([
                encoder,
                directory.join("spice-placeholder"),
                directory.join("console.sock"),
            ])
            .env("CONSOLE_TEST_MODE", mode)
            .env("CONSOLE_TEST_LOG", directory.join("native.jsonl"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(
                fs::File::create(directory.join("broker.stderr")).unwrap(),
            ))
            .spawn()
            .unwrap();
        let mut broker = Self { directory, child };
        // Ready is bounded by the production process's own startup deadline.
        let mut ready = String::new();
        BufReader::new(broker.child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert!(
            !ready.is_empty(),
            "broker startup failed: {}",
            fs::read_to_string(broker.directory.join("broker.stderr")).unwrap_or_default()
        );
        assert_eq!(
            serde_json::from_str::<Value>(&ready).unwrap()["event"],
            "ready"
        );
        broker
    }
    fn connect(&self) -> Client {
        let stream = UnixStream::connect(self.directory.join("console.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
        }
    }
    fn log(&self) -> Vec<Value> {
        fs::read_to_string(self.directory.join("native.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
    fn wait_for(&self, method: &str, epoch: u64) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if self
                .log()
                .iter()
                .any(|v| v["method"] == method && v["epoch"] == epoch)
            {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("native did not receive {method} for epoch {epoch}");
    }
    fn assert_failed_exit_and_native_reaped(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(
                    !status.success(),
                    "failed cleanup reported successful broker exit"
                );
                break;
            }
            assert!(Instant::now() < deadline, "fenced broker did not exit");
            thread::sleep(Duration::from_millis(10));
        }
        let pid = self.log().iter().find(|v| v["event"] == "spawn").unwrap()["pid"]
            .as_i64()
            .unwrap();
        assert_eq!(
            unsafe { libc::kill(pid as i32, 0) },
            -1,
            "native fixture still alive after broker exited"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
impl Drop for Broker {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}
struct Client {
    reader: BufReader<UnixStream>,
    stream: UnixStream,
}
impl Client {
    fn send(&mut self, id: &str, method: &str) {
        let mut value = json!({"request_id":id,"method":method});
        if method == "start" {
            value["codec"] = json!("h264");
        }
        writeln!(self.stream, "{value}").unwrap();
    }
    fn record(&mut self) -> Record {
        read_record(&mut self.reader).unwrap()
    }
    fn response(&mut self, id: &str) -> Value {
        let record = self.record();
        assert_eq!(record.kind, 1, "AU overtook the expected response {id}");
        let value = record.value().unwrap();
        assert_eq!(value["request_id"], id);
        value
    }
    fn start(&mut self, id: &str) -> u64 {
        self.send(id, "start");
        let value = self.response(id);
        let epoch = value["result"]["epoch"]
            .as_u64()
            .expect("start result epoch");
        for _ in 0..2 {
            let frame = self.record();
            assert_eq!(frame.kind, 2);
            assert_eq!(frame.epoch, epoch, "stale epoch frame escaped filter");
        }
        epoch
    }
}

#[test]
fn start_ack_precedes_early_aus_and_other_clients_cannot_control_owner() {
    let broker = Broker::start("normal");
    let mut first = broker.connect();
    let mut second = broker.connect();
    let old = first.start("first");
    for method in ["start", "stop", "keyframe"] {
        second.send(method, method);
        assert!(second.response(method)["error"].is_string());
    }
    assert_eq!(
        broker
            .log()
            .iter()
            .filter(|v| v["method"] == "start")
            .count(),
        1
    );
    assert!(!broker
        .log()
        .iter()
        .any(|v| matches!(v["method"].as_str(), Some("stop" | "keyframe"))));
    first.send("stop-first", "stop");
    assert_eq!(first.response("stop-first")["result"]["state"], "stopped");
    let next = second.start("second");
    assert!(next > old);
    first.send("old-keyframe", "keyframe");
    assert!(first.response("old-keyframe")["error"].is_string());
    second.send("new-keyframe", "keyframe");
    assert_eq!(second.response("new-keyframe")["result"]["requested"], true);
    assert!(broker
        .log()
        .iter()
        .filter(|v| v["method"] == "keyframe")
        .all(|v| v["epoch"] == next));
    second.send("stop-second", "stop");
    assert_eq!(second.response("stop-second")["result"]["state"], "stopped");
    assert!(
        broker
            .log()
            .iter()
            .any(|v| v["method"] == "stop" && v["epoch"] == next),
        "old-epoch session end must not bypass native stop for the new owner"
    );
}

#[test]
fn simultaneous_clients_reserve_exactly_one_encoder_owner() {
    let broker = Broker::start("normal");
    let clients = [broker.connect(), broker.connect()];
    let start = Arc::new(Barrier::new(2));
    let (ready_tx, ready_rx) = mpsc::channel();
    let mut releases = Vec::new();
    let workers: Vec<_> = clients
        .into_iter()
        .map(|mut client| {
            let start = start.clone();
            let ready = ready_tx.clone();
            let (release_tx, release_rx) = mpsc::channel();
            releases.push(release_tx);
            thread::spawn(move || {
                start.wait();
                client.send("race", "start");
                let value = client.response("race");
                let won = value["result"]["epoch"].is_u64();
                if won {
                    for _ in 0..2 {
                        assert_eq!(
                            client.record().epoch,
                            value["result"]["epoch"].as_u64().unwrap()
                        );
                    }
                } else {
                    assert!(value["error"].as_str().unwrap().contains("busy"));
                }
                // Keep the winner's socket alive until both start replies arrived.
                ready.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                won
            })
        })
        .collect();
    for _ in 0..2 {
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    }
    for release in releases {
        release.send(()).unwrap();
    }
    assert_eq!(
        workers
            .into_iter()
            .map(|w| usize::from(w.join().unwrap()))
            .sum::<usize>(),
        1
    );
    assert_eq!(
        broker
            .log()
            .iter()
            .filter(|v| v["method"] == "start")
            .count(),
        1
    );
}

#[test]
fn eof_stops_owned_epoch_and_allows_a_fresh_client() {
    let broker = Broker::start("normal");
    let mut first = broker.connect();
    let old = first.start("first");
    drop(first);
    broker.wait_for("stop", old);
    // stop receipt precedes its acknowledgement; allow the broker to process
    // that reply without converting a legitimate transient busy into a failure.
    let mut second = broker.connect();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        second.send("next", "start");
        let result = second.response("next");
        if let Some(epoch) = result["result"]["epoch"].as_u64() {
            assert!(epoch > old);
            for _ in 0..2 {
                assert_eq!(second.record().epoch, epoch);
            }
            break;
        }
        assert!(
            Instant::now() < deadline,
            "EOF owner never cleared: {result}"
        );
        assert!(result["error"].as_str().unwrap().contains("busy"));
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn early_session_failure_follows_start_ack_and_confirmed_end_releases_owner() {
    let broker = Broker::start("session-error");
    let mut first = broker.connect();
    first.send("start", "start");
    let epoch = first.response("start")["result"]["epoch"].as_u64().unwrap();
    let error = first.record();
    assert_eq!(error.epoch, epoch);
    assert_eq!(error.value().unwrap()["event"], "session-error");
    first.send("stop", "stop");
    assert_eq!(
        first.response("stop")["result"]["reason"],
        "native_session_ended"
    );
    assert!(!broker.log().iter().any(|v| v["method"] == "stop"));
    assert!(broker.connect().start("fresh") > epoch);
}

#[test]
fn failed_stop_never_becomes_a_successful_cleanup_without_confirmation() {
    let mut broker = Broker::start("stop-error");
    let mut client = broker.connect();
    client.start("start");
    client.send("stop-fails", "stop");
    // The supervisor may close the socket before the writer sends the error.
    // Either is safe; a successful cleanup reply while native remains active is not.
    if let Ok(record) = read_record(&mut client.reader) {
        let value = record.value().unwrap();
        assert!(
            value.get("result").is_none(),
            "false cleanup success: {value}"
        );
        if writeln!(
            client.stream,
            "{}",
            json!({"request_id":"stop-again","method":"stop"})
        )
        .is_ok()
        {
            if let Ok(record) = read_record(&mut client.reader) {
                let value = record.value().unwrap();
                assert!(
                    value.get("result").is_none(),
                    "false cleanup success after failed native stop: {value}"
                );
            }
        }
    }
    broker.assert_failed_exit_and_native_reaped();
}

#[test]
fn truncated_native_au_never_reaches_client_and_reaps_native() {
    let mut broker = Broker::start("truncated-au");
    let mut client = broker.connect();
    client.send("start", "start");
    // The valid start reply can race the supervisor's close. A partial AU must
    // never become a delivered record, irrespective of that scheduling.
    while let Ok(record) = read_record(&mut client.reader) {
        assert_eq!(record.kind, 1, "truncated native AU was delivered");
        assert_eq!(record.value().unwrap()["request_id"], "start");
    }
    broker.assert_failed_exit_and_native_reaped();
}

#[test]
fn unanswered_native_stop_hits_real_deadline_and_fences_owner() {
    let mut broker = Broker::start("stop-timeout");
    let mut client = broker.connect();
    client
        .stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    client.start("start");
    let before = Instant::now();
    client.send("stop", "stop");
    if let Ok(record) = read_record(&mut client.reader) {
        assert!(
            record.value().unwrap().get("result").is_none(),
            "unacknowledged stop reported cleanup success"
        );
    }
    assert!(
        before.elapsed() >= Duration::from_secs(11),
        "deadline bypassed"
    );
    assert!(
        before.elapsed() < Duration::from_secs(15),
        "deadline not bounded"
    );
    broker.assert_failed_exit_and_native_reaped();
}

#[test]
fn malformed_native_stop_acknowledgements_cannot_clear_owner() {
    for mode in ["malformed-stop", "wrong-stop-state"] {
        let mut broker = Broker::start(mode);
        let mut client = broker.connect();
        client.start("start");
        client.send("stop", "stop");
        if let Ok(record) = read_record(&mut client.reader) {
            assert!(
                record.value().unwrap().get("result").is_none(),
                "{mode} falsely confirmed cleanup"
            );
        }
        broker.assert_failed_exit_and_native_reaped();
    }
}

#[test]
fn session_end_with_mismatched_body_epoch_cannot_authorize_cleanup() {
    let mut broker = Broker::start("mismatched-session-error");
    let mut client = broker.connect();
    client.send("start", "start");
    while let Ok(record) = read_record(&mut client.reader) {
        let value = record.value().unwrap();
        assert_ne!(
            value["event"], "session-error",
            "malformed session end escaped validation"
        );
        assert_eq!(value["request_id"], "start");
    }
    broker.assert_failed_exit_and_native_reaped();
}

#[test]
fn display_only_window_close_preserves_capture_and_encoder_owner() {
    let broker = Broker::start("normal");
    let mut media = broker.connect();
    let epoch = media.start("owner");
    let mut window = broker.connect();
    for (method, open) in [
        ("show_window", true),
        ("window_status", true),
        ("close_window", false),
        ("window_status", false),
    ] {
        window.send(method, method);
        let record = read_record(&mut window.reader).unwrap();
        assert_eq!(record.epoch, 0);
        let reply = record.value().unwrap();
        assert_eq!(reply["request_id"], method);
        assert_eq!(reply["result"]["window_open"], open);
        assert_eq!(reply["result"]["input_enabled"], false);
        assert_eq!(reply["result"]["display_only"], true);
    }
    assert!(!broker.log().iter().any(|v| v["method"] == "stop"));
    media.send("key-after-close", "keyframe");
    assert_eq!(
        media.response("key-after-close")["result"]["requested"],
        true
    );
    assert!(broker
        .log()
        .iter()
        .any(|v| v["method"] == "keyframe" && v["epoch"] == epoch));
    window.send("status", "status");
    assert_eq!(
        window.response("status")["result"]["capture_connected"],
        true
    );
    media.send("finish", "stop");
    assert_eq!(media.response("finish")["result"]["state"], "stopped");
}

#[test]
fn display_only_broker_does_not_require_hardware_h264() {
    let broker = Broker::start("window-only");
    let mut client = broker.connect();
    client.send("caps", "capabilities");
    let caps = client.response("caps")["result"].clone();
    assert_eq!(caps["h264"], "unavailable");
    assert_eq!(caps["native_window"], true);
    assert_eq!(caps["broker_window_protocol"], 1);
    client.send("show", "show_window");
    assert_eq!(client.response("show")["result"]["window_open"], true);
    client.send("close", "close_window");
    assert_eq!(client.response("close")["result"]["window_open"], false);
    assert!(!broker.log().iter().any(|v| v["method"] == "start"));
}

#[test]
fn broker_rejects_unimplemented_requested_fps_before_native_start() {
    let broker = Broker::start("normal");
    let mut client = broker.connect();
    client.send("caps", "capabilities");
    let caps = client.response("caps")["result"].clone();
    assert_eq!(caps["encoder_fps_request_supported"], false);
    writeln!(
        client.stream,
        "{}",
        json!({"request_id":"unsupported-fps","method":"start","codec":"h264","fps":60})
    )
    .unwrap();
    assert!(read_record(&mut client.reader).is_err());
    assert!(!broker.log().iter().any(|v| v["method"] == "start"));
    // Invalid requests cannot reserve the encoder or break another client.
    let mut valid = broker.connect();
    assert!(valid.start("valid-default") > 0);
    valid.send("finish", "stop");
    assert_eq!(valid.response("finish")["result"]["state"], "stopped");
}

#[test]
fn rate_updates_require_encoder_owner_and_reject_old_receipts() {
    for mode in ["normal", "stale-rate-receipt"] {
        let broker = Broker::start(mode);
        let mut owner = broker.connect();
        let epoch = owner.start("rate-owner");
        let mut other = broker.connect();
        let status = json!({"version":1,"request_id":"rate-status","method":"rate_status"});
        writeln!(other.stream, "{status}").unwrap();
        assert!(other.response("rate-status")["error"].is_string());
        writeln!(owner.stream, "{status}").unwrap();
        let result = owner.response("rate-status")["result"].clone();
        assert_eq!(result["sequence"], 0);
        let mut set = json!({"version":1,"request_id":"rate-update","method":"set_bitrate","encoder_instance":result["encoder_instance"],"epoch":epoch,"sequence":1,"target_bps":2_000_000});
        writeln!(owner.stream, "{set}").unwrap();
        let answer = owner.response("rate-update");
        if mode == "normal" {
            assert_eq!(answer["result"]["target_bps"], 2_000_000);
            writeln!(owner.stream, "{set}").unwrap();
            assert!(owner.response("rate-update")["error"].is_string());
        } else {
            assert!(
                answer["error"].is_string(),
                "stale native receipt must not succeed"
            )
        }
        let mut foreign = set.clone();
        foreign["encoder_instance"] = json!("f".repeat(32));
        foreign["sequence"] = json!(2);
        writeln!(owner.stream, "{foreign}").unwrap();
        assert!(owner.response("rate-update")["error"].is_string());
        set["epoch"] = json!(epoch + 1);
        writeln!(owner.stream, "{set}").unwrap();
        assert!(owner.response("rate-update")["error"].is_string());
        owner.send("rate-stop", "stop");
        assert_eq!(owner.response("rate-stop")["result"]["state"], "stopped");
        writeln!(owner.stream, "{status}").unwrap();
        assert!(owner.response("rate-status")["error"].is_string());
        assert_eq!(
            broker
                .log()
                .iter()
                .filter(|v| v["method"] == "start")
                .count(),
            1,
            "rate changes never restart encoder"
        );
    }
}

#[test]
fn actual_stream_shim_private_fd_correlates_rate_and_preserves_stop_ack() {
    use std::os::{fd::AsRawFd, unix::process::CommandExt};
    let broker = Broker::start("normal");
    let (mut parent, child) = UnixStream::pair().unwrap();
    parent
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let fd = child.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_vm-console-stream"));
    command
        .arg(broker.directory.join("console.sock"))
        .args(["--codec", "h264", "--rate-control-fd", &fd.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut shim = Shim(command.spawn().unwrap());
    drop(child);
    writeln!(
        parent,
        "{}",
        json!({"version":1,"request_id":"query","method":"rate_status"})
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(&mut parent).read_line(&mut line).unwrap();
    let reply: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["request_id"], "query");
    let result = &reply["result"];
    assert_eq!(result["sequence"], 0);
    writeln!(parent,"{}",json!({"version":1,"request_id":"lower","method":"set_bitrate","encoder_instance":result["encoder_instance"],"epoch":result["epoch"],"sequence":1,"target_bps":1_000_000})).unwrap();
    line.clear();
    BufReader::new(&mut parent).read_line(&mut line).unwrap();
    let reply: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["result"]["readback_bps"], 1_000_000);
    let (status, stderr) = shim.stop_line_then_eof();
    assert!(status.success(), "{stderr}");
    let mut media = Vec::new();
    shim.0
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut media)
        .unwrap();
    assert!(!media.is_empty());
    assert!(!media.windows(10).any(|w| w == b"request_id"));
}

#[test]
fn stream_shim_starts_a_fresh_epoch_when_the_source_is_replaced() {
    let broker = Broker::start("source-replaced");
    let mut shim = Shim::start(&broker);
    let mut stdout = shim.0.stdout.take().unwrap();
    // One access unit from the replaced epoch, then the fresh epoch's.
    let mut epochs = Vec::new();
    for _ in 0..2 {
        let mut header = [0u8; 13];
        stdout.read_exact(&mut header).unwrap();
        let size = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let mut body = vec![0; size];
        stdout.read_exact(&mut body).unwrap();
        epochs.push(
            broker
                .log()
                .iter()
                .filter(|v| v["method"] == "start")
                .count(),
        );
    }
    assert_eq!(epochs, [1, 2]);
    assert!(
        shim.0.try_wait().unwrap().is_none(),
        "the stream kept running"
    );
    let (status, stderr) = shim.stop_line_then_eof();
    assert!(status.success(), "{stderr}");
}

#[test]
fn audio_goes_to_the_newest_listener_and_only_its_microphone_reaches_native() {
    let broker = Broker::start("normal");
    let mut first = broker.connect();
    let mut second = broker.connect();
    let listen = |client: &mut Client, id: &str| {
        client.send(id, "audio_start");
        let mut kinds = Vec::new();
        for _ in 0..2 {
            let record = client.record();
            if record.kind == 1 {
                let value = record.value().unwrap();
                assert_eq!(value["request_id"], id);
                assert_eq!(value["result"]["listening"], true);
            } else {
                assert_eq!((record.kind, record.epoch), (3, 0));
                assert_eq!(&record.body[..5], &[0, 0, 0xbb, 0x80, 2]);
            }
            kinds.push(record.kind);
        }
        kinds.sort();
        assert_eq!(kinds, [1, 3]);
    };
    listen(&mut first, "a1");
    listen(&mut second, "a2");
    // The first client lost the audio: its microphone frame is dropped, the second's is forwarded.
    for (client, pcm) in [(&mut first, "AAAA"), (&mut second, "AQAB")] {
        writeln!(
            client.stream,
            "{}",
            json!({"request_id":"m","method":"audio_in","pcm":pcm})
        )
        .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while !broker.log().iter().any(|v| v["method"] == "audio_in") {
        assert!(
            Instant::now() < deadline,
            "microphone frame never reached native"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let frames: Vec<_> = broker
        .log()
        .into_iter()
        .filter(|v| v["method"] == "audio_in")
        .collect();
    assert_eq!(frames, [json!({"method":"audio_in","pcm":"AQAB"})]);
    // No reply to a microphone frame: the next record is the stop acknowledgement.
    second.send("s", "audio_stop");
    assert_eq!(second.response("s")["result"]["listening"], false);
    // A malformed frame ends that connection without touching native.
    writeln!(
        first.stream,
        "{}",
        json!({"request_id":"x","method":"audio_in","pcm":"not base64!"})
    )
    .unwrap();
    assert!(read_record(&mut first.reader).is_err());
    assert_eq!(
        broker
            .log()
            .iter()
            .filter(|v| v["method"] == "audio_in")
            .count(),
        1
    );
}

#[test]
fn clipboard_text_goes_up_in_parts_and_its_events_reach_every_watcher() {
    let broker = Broker::start("normal");
    let mut first = broker.connect();
    let mut second = broker.connect();
    let mut bystander = broker.connect();
    for (client, id) in [(&mut first, "w1"), (&mut second, "w2")] {
        client.send(id, "clipboard_watch");
        assert_eq!(client.response(id)["result"]["watching"], true);
    }
    // "你好, world" in two base64 parts; neither gets a reply.
    for (data, last) in [("5L2g5aW9", false), ("LCB3b3JsZA==", true)] {
        writeln!(
            first.stream,
            "{}",
            json!({"request_id":"c","method":"clipboard_chunk","data":data,"last":last})
        )
        .unwrap();
    }
    writeln!(
        first.stream,
        "{}",
        json!({"request_id":"g","method":"clipboard_get"})
    )
    .unwrap();
    for client in [&mut first, &mut second] {
        let record = client.record();
        assert_eq!((record.kind, record.epoch), (1, 0));
        assert_eq!(
            record.value().unwrap(),
            json!({"event":"clipboard","text":"你好, world"})
        );
    }
    // A client that never watched hears nothing: its next record is its own reply.
    bystander.send("s", "status");
    assert!(bystander.response("s")["result"].is_object());
    // A part that is not base64 ends that connection.
    writeln!(
        second.stream,
        "{}",
        json!({"request_id":"x","method":"clipboard_chunk","data":"a!b?","last":true})
    )
    .unwrap();
    assert!(read_record(&mut second.reader).is_err());
}
