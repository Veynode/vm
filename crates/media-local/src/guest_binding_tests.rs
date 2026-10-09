use super::*;
use std::{
    os::unix::fs::PermissionsExt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
struct Fixture {
    _dir: tempfile::TempDir,
    options: Options,
    socket: PathBuf,
    _bridge: std::os::unix::net::UnixListener,
    reply: Arc<Mutex<Value>>,
    delay_ms: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}
fn identity() -> Value {
    json!({"version":1,"security_session_id":42,"uid":501,"console_set":null,"display_id":1,"physical_width":2560,"physical_height":1440,"display_geometry_sha256":"a".repeat(64)})
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // macOS temp paths can contain /var symlink; media startup canonicalizes its socket.
        let path = dir.path().canonicalize().unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = path.join("bridge.sock");
        let bridge = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        let control = path.join("control.sock");
        let listener = tokio::net::UnixListener::bind(&control).unwrap();
        fs::set_permissions(&control, fs::Permissions::from_mode(0o600)).unwrap();
        let options = Options {
            state: path,
            vm: "mac-test".into(),
            worker: "b".repeat(64),
            bridge: "c".repeat(64),
            recovery_bridge: None,
        };
        let m = fs::metadata(&socket).unwrap();
        let reply = Arc::new(Mutex::new(
            json!({"binding":{"version":1,"vm_id":options.vm,"worker_instance":options.worker,"bridge_instance":options.bridge,"worker_pid":123,"socket_path":socket,"socket_device":m.dev(),"socket_inode":m.ino(),"guest_port":5001,"scope":"host-worker-listener-only","input_authorized":false},"info":{"console_identity":identity(),"console_identity_status":{"version":1,"identity_eligible":true}}}),
        ));
        let state = reply.clone();
        let delay_ms = Arc::new(AtomicU64::new(0));
        let delay = delay_ms.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let state = state.clone();
                let delay = delay.clone();
                tokio::spawn(async move {
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap();
                    let value = state.lock().unwrap().clone();
                    tokio::time::sleep(Duration::from_millis(delay.load(Ordering::Relaxed))).await;
                    let response = if request["method"] == "guest_info" {
                        json!({"result":{"id":"fixture","result":value["info"]}})
                    } else {
                        json!({"result":{"binding":value["binding"]}})
                    };
                    let _ = stream
                        .get_mut()
                        .write_all(format!("{response}\n").as_bytes())
                        .await;
                });
            }
        });
        Self {
            _dir: dir,
            options,
            socket,
            _bridge: bridge,
            reply,
            delay_ms,
            task,
        }
    }
    async fn open(&self) -> Result<Source> {
        Source::open(
            self.options.clone(),
            &self.socket,
            "mac-test",
            false,
            true,
            &json!({"console_identity_binding_version":1}),
        )
        .await
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
#[test]
fn options_are_explicit_complete_unique_and_do_not_consume_auth() {
    let good = [
        "--guest-binding-state",
        "/private/tmp/x",
        "--guest-binding-vm",
        "mac-test",
        "--guest-worker-instance",
        &"b".repeat(64),
        "--guest-bridge-instance",
        &"c".repeat(64),
        "--auth-state",
        "/private/tmp/auth",
    ]
    .map(String::from);
    let (options, rest) = parse_options(&good).unwrap();
    assert_eq!(options.unwrap().vm, "mac-test");
    assert_eq!(rest, ["--auth-state", "/private/tmp/auth"]);
    for args in [
        vec!["--guest-binding-vm", "x"],
        vec!["--guest-binding-vm"],
        vec!["--guest-binding-vm", "x", "--guest-binding-vm", "x"],
    ] {
        assert!(parse_options(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err());
    }
    assert!(parse_options(&[]).unwrap().0.is_none());
}
#[test]
fn console_schema_rejects_missing_null_invalid_and_unknown_without_unlock_claim() {
    assert!(ConsoleIdentity::parse(identity()).is_ok());
    for (key, value) in [
        ("uid", json!(0)),
        ("uid", json!(true)),
        ("security_session_id", json!(u32::MAX)),
        ("console_set", json!(-1)),
        ("console_set", json!(true)),
        ("physical_width", json!(16385)),
        ("display_geometry_sha256", json!("A".repeat(64))),
        ("unlocked", json!(true)),
    ] {
        let mut object = identity();
        object[key] = value;
        assert!(ConsoleIdentity::parse(object).is_err(), "{key}");
    }
    let mut object = identity();
    object.as_object_mut().unwrap().remove("console_set");
    assert!(ConsoleIdentity::parse(object).is_err());
}
#[tokio::test]
async fn actual_unix_queries_publish_private_exact_identity_then_remove_it() {
    let fixture = Fixture::new().await;
    let source = fixture.open().await.unwrap();
    let file = source.prepare().await.unwrap();
    let path = file.path().to_owned();
    let dir = path.parent().unwrap().to_owned();
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
        identity()
    );
    // Real child consumes only the private contract file; this does not capture the host.
    let output = tokio::process::Command::new("/bin/cat")
        .arg(&path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        identity()
    );
    drop(file);
    assert!(!path.exists() && !dir.exists());
}
#[tokio::test]
async fn stale_worker_bridge_cross_vm_and_input_claim_are_refused() {
    let fixture = Fixture::new().await;
    for (key, value) in [
        ("worker_instance", json!("d".repeat(64))),
        ("bridge_instance", json!("d".repeat(64))),
        ("vm_id", json!("another-vm")),
        ("input_authorized", json!(true)),
    ] {
        let old = fixture.reply.lock().unwrap()["binding"][key].clone();
        fixture.reply.lock().unwrap()["binding"][key] = value;
        assert!(fixture.open().await.is_err(), "{key}");
        fixture.reply.lock().unwrap()["binding"][key] = old;
    }
    let source = fixture.open().await.unwrap();
    fixture.reply.lock().unwrap()["binding"]["worker_pid"] = json!(456);
    assert!(source.validate().await.is_err());
    assert!(source.prepare().await.is_err());
}
#[tokio::test]
async fn current_ineligible_identity_fails_without_publishing_expected_file() {
    let fixture = Fixture::new().await;
    let source = fixture.open().await.unwrap();
    fixture.reply.lock().unwrap()["info"]["console_identity_status"]["identity_eligible"] =
        json!(false);
    assert!(source.prepare().await.is_err());
}
#[tokio::test]
async fn replacing_socket_same_path_and_controller_restart_invalidates_proof() {
    let fixture = Fixture::new().await;
    let source = fixture.open().await.unwrap();
    let saved = fixture.socket.with_extension("old");
    fs::rename(&fixture.socket, &saved).unwrap();
    let _new = std::os::unix::net::UnixListener::bind(&fixture.socket).unwrap();
    fs::set_permissions(&fixture.socket, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(source.validate().await.is_err());
    fs::remove_file(&fixture.socket).unwrap();
    fs::rename(&saved, &fixture.socket).unwrap();
    source.validate().await.unwrap();
    let control = fixture.options.state.join("control.sock");
    fs::rename(&control, control.with_extension("old")).unwrap();
    let _new = std::os::unix::net::UnixListener::bind(&control).unwrap();
    fs::set_permissions(&control, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(source.validate().await.is_err());
}
#[tokio::test]
async fn watch_provenance_failure_is_error() {
    let fixture = Fixture::new().await;
    let source = fixture.open().await.unwrap();
    fixture.reply.lock().unwrap()["binding"]["bridge_instance"] = json!("f".repeat(64));
    assert!(tokio::time::timeout(Duration::from_secs(2), source.watch())
        .await
        .unwrap()
        .is_err());
}
#[tokio::test]
async fn source_requires_signed_same_vm_observe_capable_encoder() {
    let fixture = Fixture::new().await;
    for (vm, input, guest, caps) in [
        ("other", false, true, 1),
        ("mac-test", true, true, 1),
        ("mac-test", false, false, 1),
        ("mac-test", false, true, 0),
    ] {
        assert!(Source::open(
            fixture.options.clone(),
            &fixture.socket,
            vm,
            input,
            guest,
            &json!({"console_identity_binding_version":caps})
        )
        .await
        .is_err());
    }
}

#[tokio::test]
async fn stalled_rpc_deadline_and_oversize_reply_are_bounded() {
    for oversized in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let socket = SocketIdentity::read(&path).unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            if oversized {
                let _ = stream
                    .write_all(&vec![b'x'; RESPONSE_LIMIT as usize + 2])
                    .await;
            } else {
                let _ = stream.write_all(b"{\"result\":").await;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        let started = std::time::Instant::now();
        let error = tokio::time::timeout(
            Duration::from_millis(200),
            rpc(&socket, "guest_info", json!({}), Duration::from_millis(40)),
        )
        .await
        .unwrap()
        .unwrap_err();
        if !oversized {
            assert!(started.elapsed() >= Duration::from_millis(35));
            assert!(error.to_string().contains("deadline"));
        }
        task.abort();
        let _ = task.await;
    }
}

#[tokio::test]
async fn cancelled_session_scope_removes_only_its_private_identity_file() {
    let (send, receive) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let file = ExpectedFile::create(&ConsoleIdentity::parse(identity()).unwrap()).unwrap();
        send.send(file.path().to_owned()).unwrap();
        std::future::pending::<()>().await;
        drop(file);
    });
    let path = receive.await.unwrap();
    assert!(path.exists());
    task.abort();
    let _ = task.await;
    assert!(!path.exists());
    assert!(!path.parent().unwrap().exists());
}

#[tokio::test]
async fn serial_successful_rpcs_share_total_identity_budget_before_file_or_encoder() {
    let fixture = Fixture::new().await;
    let source = fixture.open().await.unwrap();
    // 210ms/300ms is the 2.1s/3s reported failure scaled down. Each RPC is
    // independently valid and below its 3s deadline, but their sum is not.
    fixture.delay_ms.store(210, Ordering::Relaxed);
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("encoder-started");
    let begin = std::time::Instant::now();
    let attempt = async {
        let _identity = source
            .prepare_at(Duration::from_millis(300), directory.path())
            .await?;
        let status = tokio::process::Command::new("/usr/bin/touch")
            .arg(&marker)
            .status()
            .await?;
        ensure!(status.success(), "synthetic encoder marker failed");
        Ok::<(), anyhow::Error>(())
    }
    .await;
    assert!(attempt.unwrap_err().to_string().contains("total deadline"));
    assert!(begin.elapsed() >= Duration::from_millis(290));
    assert!(begin.elapsed() < Duration::from_millis(550));
    assert!(!marker.exists());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    // Startup calls this same identity-budget function, before it publishes HTTP.
    assert!(source
        .identity(Duration::from_millis(300))
        .await
        .unwrap_err()
        .to_string()
        .contains("total deadline"));
}

#[tokio::test]
async fn pre_encoder_identity_failures_have_only_allowlisted_public_classifications() {
    let fixture = Fixture::new().await;
    let source = fixture.open().await.unwrap();
    for (rejection, expected) in [
        ("display_inactive", "guest_display_inactive"),
        ("display_asleep", "guest_display_asleep"),
        ("security_session_unavailable", "guest_capture_unavailable"),
        (
            "/private/SECRET display_asleep",
            "guest_capture_unavailable",
        ),
    ] {
        fixture.reply.lock().unwrap()["info"]["console_identity_status"] =
            json!({"version":1,"identity_eligible":false,"identity_rejection":rejection});
        let error = match source.prepare().await {
            Ok(_) => panic!("ineligible identity prepared"),
            Err(error) => error,
        };
        let public = source_failure(&error);
        assert_eq!(public.code, expected);
        assert!(!serde_json::to_string(&public).unwrap().contains("SECRET"));
    }
    // A malformed status must not be interpreted as an actual sleep observation.
    fixture.reply.lock().unwrap()["info"]["console_identity_status"] =
        json!({"version":2,"identity_eligible":false,"identity_rejection":"display_asleep"});
    let error = source.identity(IDENTITY_DEADLINE).await.unwrap_err();
    assert_eq!(source_failure(&error).code, "guest_capture_unavailable");
    fixture.reply.lock().unwrap()["binding"]["worker_instance"] = json!("f".repeat(64));
    assert_eq!(
        source_failure(&source.validate().await.unwrap_err()).code,
        "guest_capture_unavailable"
    );
    assert_eq!(
        source_failure(&anyhow::anyhow!("SECRET display_asleep timeout")).code,
        "guest_capture_unavailable"
    );
}

#[tokio::test]
async fn production_session_early_binding_rejection_retains_failure_without_encoder() {
    use super::super::{auth, run_session, App, Offer, SessionBook};
    use std::sync::atomic::AtomicBool;
    use tokio::sync::watch;
    let fixture = Fixture::new().await;
    let source = fixture.open().await.unwrap();
    fixture.reply.lock().unwrap()["binding"]["bridge_instance"] = json!("d".repeat(64));
    let app = App {
        host: "127.0.0.1:1".into(),
        origin: "http://127.0.0.1:1".into(),
        // No such executable: a misplaced spawn would fail differently.
        encoder: fixture.options.state.join("must-not-spawn"),
        socket: fixture.socket.clone(),
        web: fixture.options.state.clone(),
        capabilities: json!({}),
        fault_au_egress_hold: false,
        fault_drop_first_keyframe: false,
        fault_suppress_hevc_frames: false,
        active: Arc::new(tokio::sync::Mutex::new(SessionBook::default())),
        auth: None,
        cleanup_failure_exit: Some(78),
        guest_binding: Some(Arc::new(source)),
        encoder_rate: None,
    };
    let (answer, _receiver) = tokio::sync::oneshot::channel();
    let (_cancel, cancelled) = watch::channel(false);
    let (_confirm, confirmed) = watch::channel(false);
    let clean = Arc::new(AtomicBool::new(true));
    let mut evidence = super::super::SessionEvidence::default();
    let request = Offer {
        request_id: "fixture-request".into(),
        sdp: "must-not-parse".into(),
        r#type: "offer".into(),
        codec: "h264".into(),
    };
    let result = run_session(
        &app,
        &request,
        cancelled,
        answer,
        clean.clone(),
        &mut evidence,
        auth::SessionSetup {
            id: "fixture-session".into(),
            accepted: None,
            confirmed,
        },
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("bridge proof"));
    assert_eq!(
        evidence.source_failure.unwrap().code,
        "guest_capture_unavailable"
    );
    assert!(clean.load(Ordering::Acquire)); // Nothing was created, not a fabricated stop ACK.
    let mut book = SessionBook::default();
    book.completed.insert(
        request.request_id.clone(),
        (std::time::Instant::now(), true, "fixture-session".into()),
    );
    book.source_failures.insert(
        request.request_id.clone(),
        ("fixture-session".into(), evidence.source_failure.unwrap()),
    );
    assert_eq!(
        book.close_receipt(&request.request_id)["media_failure"]["code"],
        "guest_capture_unavailable"
    );
}

#[tokio::test]
async fn cleanup_recovery_uses_pinned_provenance_without_requiring_awake_desktop() {
    let fixture = Fixture::new().await;
    let source = fixture.open().await.unwrap();
    fixture.reply.lock().unwrap()["info"] = json!({"console_identity":null,"console_identity_status":{"version":1,"identity_eligible":false,"identity_rejection":"display_inactive"}});
    let ticket_file = fixture.options.state.join("receipt.json");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&ticket_file)
        .unwrap();
    file.write_all(serde_json::to_string(&json!({"version":1,"service_instance":"a".repeat(32),"capture_instance":"b".repeat(32),"cleanup_retention_ms":120000})).unwrap().as_bytes()).unwrap();
    let ticket = super::super::guest_cleanup::Ticket::read(&ticket_file).unwrap();
    fixture._bridge.set_nonblocking(true).unwrap();
    let listener =
        tokio::net::UnixListener::from_std(fixture._bridge.try_clone().unwrap()).unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["method"],
            "cleanup_status"
        );
        let body=serde_json::to_vec(&json!({"event":"cleanup_status","id":"media-cleanup-query","protocol_version":1,"service_instance":"a".repeat(32),"capture_instance":"b".repeat(32),"status":"clean","cleanup_confirmed":true,"retention_ms":120000,"cleanup_basis":"stop_capture_completed"})).unwrap();
        stream
            .get_mut()
            .write_u32((body.len() + 1) as u32)
            .await
            .unwrap();
        stream.get_mut().write_u8(1).await.unwrap();
        stream.get_mut().write_all(&body).await.unwrap();
    });
    source.recover_cleanup(&ticket).await.unwrap();
    task.await.unwrap();
    assert!(
        source.prepare().await.is_err(),
        "cleanup never enables capture of an ineligible desktop"
    );
    // A replacement file cannot redirect a prior receipt to another worker.
    let replacement = fixture.options.state.join("replacement.json");
    let mut proof = fixture.reply.lock().unwrap()["binding"].clone();
    proof["worker_instance"] = json!("f".repeat(64));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&replacement)
        .unwrap();
    file.write_all(serde_json::to_string(&proof).unwrap().as_bytes())
        .unwrap();
    let mut source = source;
    source.options.recovery_bridge = Some(replacement);
    assert!(source
        .recover_cleanup(&ticket)
        .await
        .unwrap_err()
        .to_string()
        .contains("different VM or worker"));
}

fn private_cleanup_json(path: &Path, value: &Value) {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(&serde_json::to_vec(value).unwrap()).unwrap();
}
fn recovery_ticket(fixture: &Fixture) -> super::super::guest_cleanup::Ticket {
    let path = fixture.options.state.join("recovery-receipt.json");
    private_cleanup_json(
        &path,
        &json!({"version":1,"service_instance":"a".repeat(32),"capture_instance":"b".repeat(32),"cleanup_retention_ms":120000}),
    );
    super::super::guest_cleanup::Ticket::read(&path).unwrap()
}
fn clean_recovery_status() -> Value {
    json!({"event":"cleanup_status","id":"media-cleanup-query","protocol_version":1,"service_instance":"a".repeat(32),"capture_instance":"b".repeat(32),"status":"clean","cleanup_confirmed":true,"retention_ms":120000,"cleanup_basis":"stop_capture_completed"})
}
async fn read_cleanup_request(stream: &mut BufReader<tokio::net::UnixStream>) {
    let mut line = String::new();
    stream.read_line(&mut line).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap(),
        json!({"id":"media-cleanup-query","method":"cleanup_status","service_instance":"a".repeat(32),"capture_instance":"b".repeat(32)})
    );
}
async fn write_cleanup_status(stream: &mut BufReader<tokio::net::UnixStream>, value: Value) {
    let body = serde_json::to_vec(&value).unwrap();
    stream
        .get_mut()
        .write_u32((body.len() + 1) as u32)
        .await
        .unwrap();
    stream.get_mut().write_u8(1).await.unwrap();
    stream.get_mut().write_all(&body).await.unwrap();
    stream.get_mut().shutdown().await.unwrap();
}
fn replacement_listener(fixture: &Fixture, source: &mut Source) -> tokio::net::UnixListener {
    let path = fixture.options.state.join("replacement.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let metadata = fs::metadata(&path).unwrap();
    let mut binding = fixture.reply.lock().unwrap()["binding"].clone();
    binding["bridge_instance"] = json!("d".repeat(64));
    binding["socket_path"] = json!(path);
    binding["socket_device"] = json!(metadata.dev());
    binding["socket_inode"] = json!(metadata.ino());
    let file = fixture.options.state.join("replacement-binding.json");
    private_cleanup_json(&file, &binding);
    source.options.recovery_bridge = Some(file);
    fixture.reply.lock().unwrap()["binding"] = binding;
    listener
}

#[tokio::test]
async fn replacement_bridge_recovers_only_cleanup_and_preserves_invalid_original_source() {
    let fixture = Fixture::new().await;
    let mut source = fixture.open().await.unwrap();
    let original = source.proof.clone();
    let ticket = recovery_ticket(&fixture);
    let listener = replacement_listener(&fixture, &mut source);
    fixture.reply.lock().unwrap()["info"] =
        json!({"console_identity_status":{"identity_eligible":false}});
    assert!(source.validate().await.is_err());
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(socket);
        read_cleanup_request(&mut stream).await;
        write_cleanup_status(&mut stream, clean_recovery_status()).await;
    });
    tokio::time::timeout(Duration::from_secs(1), source.recover_cleanup(&ticket))
        .await
        .unwrap()
        .unwrap();
    server.await.unwrap();
    assert_eq!(source.proof, original);
    assert_eq!(source.options.bridge, original.bridge_instance);
    assert!(source.validate().await.is_err());
    assert!(source.prepare().await.is_err());
}

#[tokio::test]
async fn recovery_rejects_wrong_capture_tuple_and_controller_change_after_guest_reply() {
    for mode in ["service", "capture", "post_query_controller"] {
        let fixture = Fixture::new().await;
        let mut source = fixture.open().await.unwrap();
        let ticket = recovery_ticket(&fixture);
        let listener = replacement_listener(&fixture, &mut source);
        let controller = fixture.reply.clone();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(socket);
            read_cleanup_request(&mut stream).await;
            let mut response = clean_recovery_status();
            match mode {
                "service" => response["service_instance"] = json!("e".repeat(32)),
                "capture" => response["capture_instance"] = json!("e".repeat(32)),
                "post_query_controller" => {
                    controller.lock().unwrap()["binding"]["worker_pid"] = json!(456)
                }
                _ => unreachable!(),
            }
            write_cleanup_status(&mut stream, response).await;
        });
        let result = tokio::time::timeout(Duration::from_secs(1), source.recover_cleanup(&ticket))
            .await
            .unwrap();
        assert!(result.is_err(), "{mode}");
        server.await.unwrap();
    }
}

#[tokio::test]
async fn concurrent_recovery_is_rejected_and_cancellation_releases_gate_and_socket() {
    let fixture = Fixture::new().await;
    let mut source = fixture.open().await.unwrap();
    let ticket = recovery_ticket(&fixture);
    let listener = replacement_listener(&fixture, &mut source);
    let source = Arc::new(source);
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (eof_tx, eof) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(socket);
        read_cleanup_request(&mut stream).await;
        entered_tx.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(
            stream.read(&mut byte).await.unwrap(),
            0,
            "cancelled query left socket open"
        );
        eof_tx.send(()).unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(socket);
        read_cleanup_request(&mut stream).await;
        write_cleanup_status(&mut stream, clean_recovery_status()).await;
    });
    let worker = {
        let source = source.clone();
        let ticket = ticket.clone();
        tokio::spawn(async move { source.recover_cleanup(&ticket).await })
    };
    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .unwrap()
        .unwrap();
    let rejection =
        tokio::time::timeout(Duration::from_millis(100), source.recover_cleanup(&ticket))
            .await
            .unwrap()
            .unwrap_err();
    assert!(rejection.to_string().contains("already in progress"));
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), eof)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), source.recover_cleanup(&ticket))
        .await
        .unwrap()
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn close_retry_requires_exact_failed_session_and_current_unix_proof_before_unfencing() {
    use super::super::{close_inner, App, Close, SessionBook};
    use axum::{body::to_bytes, http::StatusCode};
    let fixture = Fixture::new().await;
    let mut source = fixture.open().await.unwrap();
    let ticket = recovery_ticket(&fixture);
    let listener = replacement_listener(&fixture, &mut source);
    let request = uuid::Uuid::new_v4().to_string();
    let session = uuid::Uuid::new_v4().to_string();
    let mut book = SessionBook::default();
    book.completed.insert(
        request.clone(),
        (std::time::Instant::now(), false, session.clone()),
    );
    book.recoveries
        .insert(request.clone(), (session.clone(), ticket));
    let app = App {
        host: "127.0.0.1:1".into(),
        origin: "http://127.0.0.1:1".into(),
        encoder: fixture.options.state.join("must-not-spawn"),
        socket: fixture.socket.clone(),
        web: fixture.options.state.clone(),
        capabilities: json!({}),
        fault_au_egress_hold: false,
        fault_drop_first_keyframe: false,
        fault_suppress_hevc_frames: false,
        active: Arc::new(tokio::sync::Mutex::new(book)),
        auth: None,
        cleanup_failure_exit: Some(78),
        guest_binding: Some(Arc::new(source)),
        encoder_rate: None,
    };
    let close = || Close {
        request_id: request.clone(),
        session_id: Some(session.clone()),
    };
    assert_eq!(
        close_inner(
            &app,
            Close {
                request_id: request.clone(),
                session_id: Some(uuid::Uuid::new_v4().to_string())
            }
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    fixture.reply.lock().unwrap()["binding"]["worker_instance"] = json!("f".repeat(64));
    assert_eq!(
        close_inner(&app, close()).await.status(),
        StatusCode::GATEWAY_TIMEOUT
    );
    {
        let book = app.active.lock().await;
        assert!(!book.completed[&request].1);
        assert!(book.recoveries.contains_key(&request));
    }
    // Both denied attempts stop before contacting the Guest query endpoint.
    assert!(
        tokio::time::timeout(Duration::from_millis(40), listener.accept())
            .await
            .is_err()
    );
    fixture.reply.lock().unwrap()["binding"]["worker_instance"] = json!("b".repeat(64));
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(socket);
        read_cleanup_request(&mut stream).await;
        write_cleanup_status(&mut stream, clean_recovery_status()).await;
    });
    let response = tokio::time::timeout(Duration::from_secs(1), close_inner(&app, close()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&to_bytes(response.into_body(), 4096).await.unwrap())
            .unwrap(),
        json!({"closed":true})
    );
    server.await.unwrap(); // Listener now closed; repeat receipt cannot depend on another query.
    let response = close_inner(&app, close()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&to_bytes(response.into_body(), 4096).await.unwrap())
            .unwrap(),
        json!({"closed":true})
    );
    let book = app.active.lock().await;
    assert!(book.completed[&request].1);
    assert_eq!(book.completed[&request].2, session);
    assert!(!book.recoveries.contains_key(&request));
    assert!(book.active.is_none());
}
