//! Regression for cleanup receipts outliving lease ownership. Real worker pipe,
//! controlled backend replies; no VM or Guest-delivery acceptance.
use super::*;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{ChildStdout, Stdio};

struct Fixture {
    manager: Manager,
    stdout: BufReader<ChildStdout>,
    owner: String,
    epoch: u64,
    _dir: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = Manager::load(
            dir.path().into(),
            "/unused".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        manager.qemu = Some(backend::QemuRegistration {
            worker: "/unused".into(),
            launcher: "/unused".into(),
            library: "/unused".into(),
            resources: "/unused".into(),
            spice_library: None,
            screenshot_helper: None,
            console_broker: Some("/unused".into()),
            console_helper: Some("/unused".into()),
            venus_runtime: None,
        });
        manager.configs.insert(
            "vm".into(),
            serde_json::from_value(
                json!({"id":"vm","backend":"qemu-hvf","guest":"linux","cpus":1,"memory_mib":512}),
            )
            .unwrap(),
        );
        let mut child = Command::new("/bin/cat")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        manager.running.insert(
            "vm".into(),
            Running {
                bridge: Arc::new(Mutex::new(bridge::State::new().unwrap())),
                child,
                stdin,
                assigned_memory_mib: 512,
                expected_exit: false,
                execution_quiescing: false,
                exit_status: None,
                disk_identity: None,
                state: Arc::new(Mutex::new(json!({"state":"running"}))),
                pending: Arc::new(Mutex::new(BTreeMap::new())),
                next: 1,
            },
        );
        let owner = "a".repeat(64);
        let epoch = manager
            .leases
            .lock()
            .unwrap()
            .acquire("vm", &owner, std::time::Instant::now())
            .unwrap();
        let mut fixture = Self {
            manager,
            stdout,
            owner,
            epoch,
            _dir: dir,
        };
        fixture.input_attempt(fixture.owner.clone(), epoch);
        fixture
    }
    fn input_attempt(&mut self, owner: String, epoch: u64) {
        let mut pending = None;
        self.manager
            .dispatch(
                json!({"method":"input_step","arguments":{
            "id":"vm","owner":owner,"epoch":epoch,"operation_id":"b".repeat(32),
            "begin":true,"events":[input::button("left",true)]}}),
                &mut pending,
            )
            .unwrap();
        let mut fd = libc::pollfd {
            fd: self.stdout.get_ref().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let flags = unsafe { libc::fcntl(fd.fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(fd.fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        // A ready pipe may contain only a prefix. Preserve bytes across
        // WouldBlock, with one deadline for the whole bounded JSONL record.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut bytes = Vec::new();
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "worker pipe request deadline"
            );
            let mut byte = [0];
            match std::io::Read::read(&mut self.stdout, &mut byte) {
                Ok(0) => panic!("worker pipe EOF before complete request"),
                Ok(_) => {
                    bytes.push(byte[0]);
                    assert!(bytes.len() <= 16 * 1024, "unexpected test request size");
                    if byte[0] == b'\n' {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    let result =
                        unsafe { libc::poll(&mut fd, 1, remaining.as_millis().max(1) as i32) };
                    if result < 0
                        && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                    {
                        continue;
                    }
                    assert_eq!(result, 1, "worker pipe request deadline");
                }
                Err(error) => panic!("worker pipe read failed: {error}"),
            }
        }
        let wire: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(wire["method"], "input_step");
        assert_eq!(wire["arguments"]["epoch"], epoch);
        let id = wire["id"].as_u64().unwrap();
        self.manager.running["vm"]
            .pending
            .lock()
            .unwrap()
            .remove(&id)
            .unwrap()
            .send(json!({"id":id,"result":{"dispatched":true,"guest_delivery_confirmed":false}}))
            .unwrap();
        let (peer, _other) = UnixStream::pair().unwrap();
        pending.unwrap().wait(&peer).unwrap();
    }
    fn retire(&mut self, method: &str) -> Completion {
        let mut completion = None;
        self.manager.dispatch(json!({"method":method,"arguments":{"id":"vm","owner":self.owner,"epoch":self.epoch}}),&mut completion).unwrap();
        completion.expect("retirement must defer backend release confirmation")
    }
    fn expect_original_epoch(&self, completion: &Completion) {
        match completion {
            Completion::CancelJobs {
                input_revocations, ..
            } => {
                assert_eq!(
                    input_revocations.len(),
                    1,
                    "unconfirmed input must survive owner removal"
                );
                let record = &input_revocations[0];
                assert_eq!(record.owner, self.owner);
                assert_eq!(record.vm, "vm");
                assert_eq!(
                    record.epoch, self.epoch,
                    "old cleanup cannot target current new-owner epoch"
                );
            }
            _ => panic!("wrong retirement completion"),
        }
    }
    fn fails_without_backend_ack(completion: Completion) {
        // There is no controller listener in this private fixture. A real internal
        // Unix RPC therefore fails; disappearance of lease ownership is no proof.
        let (peer, _other) = UnixStream::pair().unwrap();
        assert!(
            completion.wait(&peer).is_err(),
            "missing revoke ACK must not become closed:true"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for worker in self.manager.running.values_mut() {
            let _ = worker.force_stop();
        }
    }
}

#[test]
fn concurrent_session_end_cannot_skip_pending_input_revoke() {
    let mut fixture = Fixture::new();
    let first = fixture.retire("session_end");
    fixture.expect_original_epoch(&first);
    let second = fixture.retire("session_end");
    fixture.expect_original_epoch(&second);
    Fixture::fails_without_backend_ack(second);
    Fixture::fails_without_backend_ack(first);
}

#[test]
fn failed_session_end_retains_release_requirement_for_retry() {
    let mut fixture = Fixture::new();
    let first = fixture.retire("session_end");
    Fixture::fails_without_backend_ack(first);
    let retry = fixture.retire("session_end");
    fixture.expect_original_epoch(&retry);
    Fixture::fails_without_backend_ack(retry);
}

#[test]
fn failed_release_or_takeover_cannot_make_later_eof_cleanup_succeed() {
    for method in ["release_control", "human_takeover"] {
        let mut fixture = Fixture::new();
        let first = fixture.retire(method);
        Fixture::fails_without_backend_ack(first);
        let eof = fixture.retire("session_end");
        fixture.expect_original_epoch(&eof);
        Fixture::fails_without_backend_ack(eof);
    }
}

#[test]
fn expired_owner_cleanup_uses_original_epoch_after_new_owner_acquires() {
    let mut fixture = Fixture::new();
    let next_owner = "c".repeat(64);
    let next = fixture
        .manager
        .leases
        .lock()
        .unwrap()
        .acquire(
            "vm",
            &next_owner,
            std::time::Instant::now() + Duration::from_secs(61),
        )
        .unwrap();
    assert!(next > fixture.epoch);
    fixture.input_attempt(next_owner.clone(), next);
    let old = fixture.retire("session_end");
    fixture.expect_original_epoch(&old);
    assert!(fixture
        .manager
        .leases
        .lock()
        .unwrap()
        .check("vm", &next_owner, next, std::time::Instant::now())
        .is_ok());
    Fixture::fails_without_backend_ack(old);
}

#[test]
fn retirement_batch_bounds_parallel_requests_and_does_not_dispatch_after_deadline() {
    let mut fixture = Fixture::new();
    let original = fixture
        .manager
        .input_retirements
        .lock()
        .unwrap()
        .records(Some(&fixture.owner), None)[0]
        .clone();
    for epoch in 2..=9 {
        let mut extra = original.clone();
        extra.epoch = epoch;
        fixture
            .manager
            .input_retirements
            .lock()
            .unwrap()
            .record(extra)
            .unwrap();
    }
    let listener = UnixListener::bind(fixture._dir.path().join("control.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let start = std::time::Instant::now();
        let mut pending = Vec::new();
        while start.elapsed() < Duration::from_millis(5250) {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let request: Value = serde_json::from_slice(
                        &read_frame(&mut BufReader::new(stream.try_clone().unwrap()))
                            .unwrap()
                            .unwrap(),
                    )
                    .unwrap();
                    assert_eq!(request["method"], "input_revoke");
                    pending.push((stream, request["arguments"]["through_epoch"].clone()));
                    assert!(
                        pending.len() <= 8,
                        "cleanup must not create more than eight concurrent backend requests"
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("fixture accept: {error}"),
            }
        }
        assert_eq!(pending.len(), 8);
        for (mut stream, epoch) in pending {
            write_json(&mut stream,&json!({"result":{"released":true,"synthetic_release_dispatched":true,"through_epoch":epoch}})).unwrap();
        }
        let check = std::time::Instant::now();
        while check.elapsed() < Duration::from_millis(150) {
            assert!(
                matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock),
                "expired batch dispatched a queued ninth obligation"
            );
            thread::sleep(Duration::from_millis(5));
        }
    });
    let completion = fixture.retire("session_end");
    let (peer, _other) = UnixStream::pair().unwrap();
    assert!(completion
        .wait(&peer)
        .unwrap_err()
        .to_string()
        .contains("unconfirmed"));
    server.join().unwrap();
    let ledger = fixture.manager.input_retirements.lock().unwrap();
    let remaining = ledger.records(Some(&fixture.owner), None);
    assert_eq!(
        remaining.len(),
        1,
        "late exact ACKs remove only eight started obligations"
    );
    assert_eq!(
        remaining[0].epoch, 9,
        "undispatched cleanup must remain pending"
    );
}
