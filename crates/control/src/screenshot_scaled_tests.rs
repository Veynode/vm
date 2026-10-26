//! The daemon's part of the scaled-JPEG screenshot form: what it writes to the worker, what it refuses itself and
//! what it does with the worker's answer. Pipe fixture, not VM acceptance: the "worker" is `cat`, so each command
//! the daemon writes can be read back byte for byte.
use super::*;
use screenshot_scaled::{tests::reply as helper_reply, Refusal, Refused, Scaled};

type PendingReplies = Arc<Mutex<BTreeMap<u64, PendingReply>>>;
struct Fixture {
    _temp: tempfile::TempDir,
    manager: Manager,
    /// Per VM: the worker's stdout (what the daemon wrote to it) and its pending replies.
    workers: BTreeMap<&'static str, (BufReader<std::process::ChildStdout>, PendingReplies)>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for run in self.manager.running.values_mut() {
            let _ = run.force_stop();
        }
    }
}
fn fixture(console_broker: bool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let kernel = temp.path().join("kernel");
    fs::write(&kernel, b"protocol fixture").unwrap();
    let mut manager = Manager::load(
        crate::test_private_state(temp.path()),
        "/unused".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    manager.qemu = Some(QemuRegistration {
        worker: kernel.clone(),
        launcher: kernel.clone(),
        library: kernel.clone(),
        resources: temp.path().into(),
        spice_library: None,
        screenshot_helper: (!console_broker).then(|| kernel.clone()),
        console_broker: console_broker.then(|| kernel.clone()),
        console_helper: console_broker.then(|| kernel.clone()),
        venus_runtime: None,
    });
    let mut workers = BTreeMap::new();
    for (id, config) in [
        ("qemu", json!({"backend":"qemu-hvf","guest":"linux"})),
        ("mac", json!({"guest":"macos"})),
        ("vzlinux", json!({"guest":"linux"})),
    ] {
        let mut config = config;
        config["id"] = json!(id);
        config["cpus"] = json!(1);
        config["memory_mib"] = json!(512);
        manager
            .configs
            .insert(id.into(), serde_json::from_value(config).unwrap());
        let mut child = Command::new("/bin/cat")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        manager.running.insert(
            id.into(),
            Running {
                bridge: Arc::new(Mutex::new(bridge::State::new().unwrap())),
                child,
                stdin,
                pending: pending.clone(),
                assigned_memory_mib: 512,
                expected_exit: false,
                execution_quiescing: false,
                guest_async_timeout: Arc::new(Mutex::new(GuestAsyncTimeout::default())),
                exit_status: None,
                disk_identity: None,
                state: Arc::new(Mutex::new(json!({"state":"running"}))),
                next: 1,
            },
        );
        workers.insert(id, (BufReader::new(stdout), pending));
    }
    Fixture {
        _temp: temp,
        manager,
        workers,
    }
}
impl Fixture {
    /// Dispatch one `screenshot`; on success, the exact line the daemon wrote to the worker and the completion.
    fn send(&mut self, id: &'static str, arguments: Value) -> Result<(Vec<u8>, Completion)> {
        let mut arguments = arguments;
        arguments["id"] = json!(id);
        let mut deferred = None;
        self.manager.dispatch(
            json!({"method":"screenshot","arguments":arguments}),
            &mut deferred,
        )?;
        let reader = &mut self.workers.get_mut(id).unwrap().0;
        let mut descriptor = libc::pollfd {
            fd: reader.get_ref().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(
            unsafe { libc::poll(&mut descriptor, 1, 2000) } > 0,
            "fixture output deadline"
        );
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).unwrap();
        Ok((line, deferred.expect("a deferred backend completion")))
    }
    /// The worker's answer to request `number`, and what the daemon makes of it.
    fn answer(
        &mut self,
        id: &'static str,
        number: u64,
        completion: Completion,
        reply: Value,
    ) -> Result<Value> {
        let mut reply = reply;
        reply["id"] = json!(number);
        self.workers[id]
            .1
            .lock()
            .unwrap()
            .remove(&number)
            .expect("pending request")
            .sender
            .send(reply)
            .unwrap();
        let (peer, _other) = UnixStream::pair().unwrap();
        completion.wait(&peer)
    }
}
fn ask() -> Value {
    json!({"format":"jpeg","max_width":1280,"max_height":720,"quality":50})
}
const ASKED: Scaled = Scaled {
    max_width: 1280,
    max_height: 720,
    quality: 50,
};
/// The helper's scaled reply with what the QEMU worker adds to every screenshot.
fn worker_result(divisor: u64) -> Value {
    let mut result = helper_reply(2560, 1440, divisor, 50);
    result["frame_id"] = json!("12345678-1234-4234-8234-123456789abc");
    result["display_id"] = json!(0);
    result["guest_ui_scale"] = Value::Null;
    result
}

#[test]
fn worker_command_without_the_scaled_form_is_byte_for_byte_what_it_was() {
    let mut f = fixture(true);
    let (line, _) = f.send("qemu", json!({})).unwrap();
    assert_eq!(line, b"{\"id\":1,\"method\":\"screenshot\"}\n");
    // Arguments the daemon never read are still not forwarded.
    let (line, _) = f.send("qemu", json!({"owner":"x","unknown":1})).unwrap();
    assert_eq!(line, b"{\"id\":2,\"method\":\"screenshot\"}\n");
    let (line, _) = f.send("mac", json!({})).unwrap();
    assert_eq!(
        line,
        b"{\"id\":1,\"method\":\"guest_request\",\"request\":{\"id\":\"1\",\"method\":\"screenshot\"}}\n"
    );
    // The scaled form adds its four fields and nothing else.
    let (line, _) = f.send("qemu", ask()).unwrap();
    assert_eq!(
        line,
        b"{\"format\":\"jpeg\",\"id\":3,\"max_height\":720,\"max_width\":1280,\"method\":\"screenshot\",\"quality\":50}\n"
    );
}

#[test]
fn scaled_form_is_refused_by_the_daemon_when_invalid_or_when_the_backend_has_none() {
    let mut f = fixture(true);
    let refused = |f: &mut Fixture, id: &'static str, arguments: Value| {
        let error = f
            .send(id, arguments)
            .err()
            .expect("refused before the worker");
        (Refused::of(&error), error.to_string())
    };
    for field in screenshot_scaled::FIELDS {
        let mut partial = ask();
        partial.as_object_mut().unwrap().remove(field);
        assert_eq!(
            refused(&mut f, "qemu", partial).0,
            Some(Refusal::InvalidRequest),
            "without {field}"
        );
    }
    for (field, value) in [
        ("max_width", json!(3841)),
        ("max_height", json!(1)),
        ("quality", json!(0)),
        ("quality", json!(50.5)),
        ("format", json!("png")),
    ] {
        let mut bad = ask();
        bad[field] = value;
        assert_eq!(
            refused(&mut f, "qemu", bad).0,
            Some(Refusal::InvalidRequest),
            "{field}"
        );
    }
    // The macOS Guest service makes its own JPEG and has no such form: refused by name, never sent, never dropped.
    let (refusal, text) = refused(&mut f, "mac", ask());
    assert_eq!(refusal, Some(Refusal::FormatUnavailable));
    assert!(
        text.starts_with(
            "unsupported: the scaled JPEG screenshot form requires the QEMU console broker"
        ),
        "{text}"
    );
    // An invalid form is named as invalid there too, before the backend is considered.
    let mut bad = ask();
    bad["quality"] = json!(101);
    assert_eq!(refused(&mut f, "mac", bad).0, Some(Refusal::InvalidRequest));
    // An Apple Linux desktop container captures its own JPEG through the Guest service: also refused by name.
    let (refusal, text) = refused(&mut f, "vzlinux", ask());
    assert_eq!(refusal, Some(Refusal::FormatUnavailable));
    assert!(
        text.starts_with(
            "unsupported: the scaled JPEG screenshot form requires the QEMU console broker"
        ),
        "{text}"
    );
    // Nothing above reached a worker: the next command is still number 1.
    assert_eq!(
        f.send("qemu", json!({})).unwrap().0,
        b"{\"id\":1,\"method\":\"screenshot\"}\n"
    );
    assert_eq!(
        f.send("mac", json!({})).unwrap().0,
        b"{\"id\":1,\"method\":\"guest_request\",\"request\":{\"id\":\"1\",\"method\":\"screenshot\"}}\n"
    );
    // QEMU with the one-shot capture helper only (no console broker): the same refusal; the PNG form still goes out.
    let mut legacy = fixture(false);
    let (refusal, _) = refused(&mut legacy, "qemu", ask());
    assert_eq!(refusal, Some(Refusal::FormatUnavailable));
    assert_eq!(
        legacy.send("qemu", json!({})).unwrap().0,
        b"{\"id\":1,\"method\":\"screenshot\"}\n"
    );
    // The per-VM report says where the form exists.
    assert_eq!(
        f.manager.view("qemu").unwrap()["capabilities"]["screenshot_scaled_jpeg"],
        true
    );
    assert_eq!(
        f.manager.view("mac").unwrap()["capabilities"]["screenshot_scaled_jpeg"],
        false
    );
    assert_eq!(
        legacy.manager.view("qemu").unwrap()["capabilities"]["screenshot_scaled_jpeg"],
        false
    );
    assert!(f.manager.view("vzlinux").unwrap()["capabilities"]
        .get("screenshot_scaled_jpeg")
        .is_none());
}

#[test]
fn worker_answer_to_the_scaled_form_is_checked_and_a_refusal_keeps_its_word() {
    let mut f = fixture(true);
    let mut number = 0;
    let mut exchange = |f: &mut Fixture, arguments: Value, reply: Value| {
        number += 1;
        let (_, completion) = f.send("qemu", arguments).unwrap();
        f.answer("qemu", number, completion, reply)
    };
    // The helper's JPEG, unchanged.
    let good = worker_result(2);
    assert_eq!(
        exchange(&mut f, ask(), json!({"result":good})).unwrap(),
        good
    );
    ASKED.verify(&good).unwrap();
    // A PNG from a helper without the form: passed on as any PNG, for the caller to recognise.
    let png = screenshot_test_support::value();
    assert_eq!(exchange(&mut f, ask(), json!({"result":png})).unwrap(), png);
    // Disagreements are refused here even if the worker let them through.
    type Change = Box<dyn Fn(&mut Value)>;
    let changes: Vec<(&str, Change)> = vec![
        (
            "scale disagrees",
            Box::new(|v| v["scale_divisor"] = json!(4)),
        ),
        (
            "scale disagrees",
            Box::new(|v| v["frame_height"] = json!(721)),
        ),
        (
            "requested one",
            Box::new(|v| v["frame_quality"] = json!(80)),
        ),
        (
            "screenshot size mismatch",
            Box::new(|v| v["byte_length"] = json!(3)),
        ),
        ("frame id", Box::new(|v| v["frame_id"] = json!("short"))),
    ];
    for (expected, change) in changes {
        let mut bad = good.clone();
        change(&mut bad);
        let error = exchange(&mut f, ask(), json!({"result":bad})).unwrap_err();
        assert!(error.to_string().contains(expected), "{expected}: {error}");
        assert_eq!(Refused::of(&error), None);
    }
    // A full-size frame when half was the bound.
    let error = exchange(&mut f, ask(), json!({"result":worker_result(1)})).unwrap_err();
    assert!(
        error.to_string().contains("exceeds the requested size"),
        "{error}"
    );
    // The worker's refusal word stays beside the text the daemon always wrote.
    for refusal in Refusal::ALL {
        let reply =
            json!({"error":"console operation failed: x","screenshot_refusal":refusal.name()});
        let error = exchange(&mut f, ask(), reply).unwrap_err();
        assert_eq!(Refused::of(&error), Some(refusal));
        assert_eq!(
            error.to_string(),
            "backend: \"console operation failed: x\""
        );
    }
    let unknown = json!({"error":"console operation failed: x","screenshot_refusal":"GUEST_TEXT"});
    let error = exchange(&mut f, ask(), unknown).unwrap_err();
    assert_eq!(
        (Refused::of(&error), error.to_string().as_str()),
        (None, "backend: \"console operation failed: x\"")
    );
    // A request that did not ask for the form: the error is the plain one whatever the worker attached, and a
    // JPEG that would have satisfied the scaled form is judged as before (only its own JPEG checks).
    let marked =
        json!({"error":"console operation failed: x","screenshot_refusal":"encoded_size_limit"});
    let error = exchange(&mut f, json!({}), marked).unwrap_err();
    assert_eq!(
        (Refused::of(&error), error.to_string().as_str()),
        (None, "backend: \"console operation failed: x\"")
    );
    assert_eq!(
        exchange(&mut f, json!({}), json!({"result":png})).unwrap(),
        png
    );
}
