use super::*;
#[test]
fn exec_identity_passes_only_valid_user_env_and_workdir() {
    assert!(exec_identity(&json!({"argv":["id"]})).unwrap().is_empty());
    let fields =
        exec_identity(&json!({"user":"veynode","env":["A=1","B="],"workdir":"/srv","argv":["id"]}))
            .unwrap();
    assert_eq!(
        fields,
        vec![
            ("user", json!("veynode")),
            ("env", json!(["A=1", "B="])),
            ("workdir", json!("/srv")),
        ]
    );
    for bad in [
        json!({"user":""}),
        json!({"user":7}),
        json!({"env":["NOEQUALS"]}),
        json!({"env":["=value"]}),
        json!({"env":"A=1"}),
        json!({"workdir":"relative"}),
    ] {
        assert!(exec_identity(&bad).is_err(), "{bad}");
    }
}
#[test]
fn stopped_owned_worker_is_not_reaped_and_can_continue_to_real_exit() {
    struct Owned(Running);
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.0.force_stop();
        }
    }
    let mut child=Command::new("python3").args(["-c","import os,signal,sys; os.kill(os.getpid(),signal.SIGSTOP); sys.stdin.readline(); sys.exit(7)"])
        .process_group(0).stdin(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut owned = Owned(Running {
        bridge: Arc::new(Mutex::new(bridge::State::new().unwrap())),
        child,
        stdin,
        assigned_memory_mib: 0,
        expected_exit: false,
        execution_quiescing: false,
        guest_async_timeout: Arc::new(Mutex::new(GuestAsyncTimeout::default())),
        exit_status: None,
        disk_identity: None,
        state: Arc::new(Mutex::new(json!({}))),
        pending: Arc::new(Mutex::new(BTreeMap::new())),
        next: 1,
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe {
                libc::waitid(
                    libc::P_PID,
                    owned.0.child.id() as libc::id_t,
                    &mut info,
                    libc::WSTOPPED | libc::WNOHANG | libc::WNOWAIT,
                )
            },
            0
        );
        if info.si_code == libc::CLD_STOPPED {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "owned fixture did not stop"
        );
        thread::sleep(Duration::from_millis(5));
    }
    for _ in 0..3 {
        assert!(
            owned.0.poll_exit().unwrap().is_none(),
            "stopped worker must remain alive"
        );
    }
    assert_eq!(
        unsafe { libc::kill(owned.0.child.id() as i32, libc::SIGCONT) },
        0
    );
    owned.0.stdin.write_all(b"continue\n").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = owned.0.poll_exit().unwrap() {
            assert_eq!(status.code(), Some(7));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "owned fixture did not exit"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn peer_fixture() -> (tempfile::TempDir, Arc<Mutex<Manager>>) {
    let temp = tempfile::tempdir().unwrap();
    let mut manager = Manager::load(
        test_private_state(temp.path()),
        "/unused/apple".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    manager.configs.insert(
        "test".into(),
        serde_json::from_value(json!({"id":"test","guest":"linux","cpus":1,"memory_mib":512}))
            .unwrap(),
    );
    (temp, Arc::new(Mutex::new(manager)))
}
fn vm_present(manager: &Arc<Mutex<Manager>>) -> bool {
    manager.lock().unwrap().configs.contains_key("test")
}
/// A real listening socket, so the kernel records the connecting process's credential exactly as for `control.sock`.
fn connected(temp: &tempfile::TempDir) -> (UnixStream, UnixStream) {
    let path = temp.path().join("peer.sock");
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let client = UnixStream::connect(&path).unwrap();
    let (server, _) = listener.accept().unwrap();
    (client, server)
}
fn unread_bytes(stream: &UnixStream) -> usize {
    let mut pending: libc::c_int = 0;
    assert_eq!(
        unsafe { libc::ioctl(stream.as_raw_fd(), libc::FIONREAD, &mut pending) },
        0
    );
    pending as usize
}
fn remaining(mut client: UnixStream) -> Vec<u8> {
    // The server end is already dropped, so this cannot block; Darwin refuses a timeout on such a socket.
    // Closing a socket that still holds the unread request may surface as a reset instead of a clean EOF. Either
    // way the peer has hung up; what matters is every byte delivered before that.
    let mut bytes = Vec::new();
    if let Err(error) = client.read_to_end(&mut bytes) {
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
    }
    bytes
}
#[test]
fn control_socket_serves_a_same_uid_peer_through_the_kernel_credential() {
    let (temp, manager) = peer_fixture();
    let (mut client, server) = connected(&temp);
    assert_eq!(peer_euid(&server).unwrap(), unsafe { libc::geteuid() });
    write_json(
        &mut client,
        &json!({"method":"delete","arguments":{"id":"test","confirm":"test"}}),
    )
    .unwrap();
    serve_connection(
        server,
        manager.clone(),
        std::time::Instant::now(),
        peer_euid,
    )
    .unwrap();
    let reply: Value =
        serde_json::from_slice(&read_frame(&mut BufReader::new(client)).unwrap().unwrap()).unwrap();
    assert_eq!(reply["result"]["deleted"], true, "{reply}");
    assert!(!vm_present(&manager));
}
#[test]
fn control_socket_refuses_a_foreign_uid_before_reading_any_request_byte() {
    let (temp, manager) = peer_fixture();
    let foreign = unsafe { libc::geteuid() }.wrapping_add(1);
    let valid = json!({"method":"delete","arguments":{"id":"test","confirm":"test"}});
    for request in [
        serde_json::to_vec(&valid).unwrap(),
        b"not json at all".to_vec(),
    ] {
        let (mut client, server) = connected(&temp);
        client.write_all(&request).unwrap();
        client.write_all(b"\n").unwrap();
        let sent = request.len() + 1;
        let error = serve_connection(
            server,
            manager.clone(),
            std::time::Instant::now(),
            |stream: &UnixStream| {
                // The uid is consulted while the whole request is still unread in the socket buffer.
                assert_eq!(unread_bytes(stream), sent);
                Ok(foreign)
            },
        )
        .unwrap_err()
        .to_string();
        // The same refusal for a valid and an unparseable request: the bytes were never interpreted.
        assert_eq!(
            error,
            "control connection refused: peer uid differs from daemon uid"
        );
        assert_eq!(remaining(client), b"", "a refused peer receives no reply");
        assert!(vm_present(&manager));
    }
    // Control: the identical valid request is served, with its side effect, once the peer is the daemon's own uid.
    let (mut client, server) = connected(&temp);
    write_json(&mut client, &valid).unwrap();
    serve_connection(
        server,
        manager.clone(),
        std::time::Instant::now(),
        peer_euid,
    )
    .unwrap();
    assert!(!vm_present(&manager));
}
#[test]
fn control_socket_fails_closed_when_the_peer_credential_is_unreadable() {
    let (temp, manager) = peer_fixture();
    let (mut client, server) = connected(&temp);
    write_json(
        &mut client,
        &json!({"method":"delete","arguments":{"id":"test","confirm":"test"}}),
    )
    .unwrap();
    let error = serve_connection(
        server,
        manager.clone(),
        std::time::Instant::now(),
        |_: &UnixStream| bail!("credential read failed"),
    )
    .unwrap_err()
    .to_string();
    assert_eq!(
        error,
        "control connection refused: peer credential unavailable"
    );
    assert_eq!(remaining(client), b"");
    assert!(vm_present(&manager));
    // The production reader itself fails, rather than reporting some uid, on a socket that has no peer.
    let unconnected = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    assert!(unconnected >= 0);
    let unconnected = unsafe { <UnixStream as std::os::fd::FromRawFd>::from_raw_fd(unconnected) };
    assert!(peer_euid(&unconnected).is_err());
    assert!(admit_peer(&unconnected, peer_euid).is_err());
}
#[test]
fn control_socket_still_commits_cleanup_from_a_same_uid_peer_that_already_closed() {
    // The credential is recorded at connect time, so the existing disconnect-tolerant cleanup path survives admission.
    let (temp, manager) = peer_fixture();
    let (mut client, server) = connected(&temp);
    write_json(
        &mut client,
        &json!({"method":"delete","arguments":{"id":"test","confirm":"test"}}),
    )
    .unwrap();
    drop(client);
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while !peer_disconnected(&[server.as_raw_fd()]) && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(peer_disconnected(&[server.as_raw_fd()]));
    assert_eq!(peer_euid(&server).unwrap(), unsafe { libc::geteuid() });
    let cleanup = serve_connection(
        server,
        manager.clone(),
        std::time::Instant::now(),
        peer_euid,
    );
    assert!(cleanup.is_err()); // Only the reply write fails.
    assert!(!vm_present(&manager));
}
#[test]
fn qemu_input_cleanup_acknowledgements_cannot_be_empty_or_negative() {
    for result in [
        json!({}),
        json!({"released":false}),
        json!({"released":true}),
    ] {
        assert!(normalize_backend_input(result, "input_cleanup", false).is_err());
    }
    let valid = json!({"released":true,"synthetic_release_dispatched":true});
    assert_eq!(
        normalize_backend_input(valid.clone(), "input_cleanup", false).unwrap(),
        valid
    );
}
#[test]
fn mac_guest_input_acknowledgements_are_explicit() {
    for result in [
        json!({}),
        json!({"dispatched":true}),
        json!({"dispatched":false,"guest_delivery_confirmed":false}),
    ] {
        assert!(normalize_guest_input(json!({"result":result}), "input_step").is_err());
    }
    let step = json!({"dispatched":true,"guest_delivery_confirmed":false});
    assert_eq!(
        normalize_guest_input(json!({"result":step}), "input_step").unwrap(),
        step
    );
    assert!(normalize_guest_input(json!({"result":{"released":false}}), "input_cleanup").is_ok());
    assert!(normalize_guest_input(json!({"result":{"released":true}}), "input_cleanup").is_err());
    assert!(normalize_guest_input(json!({"result":{}}), "input_revoke").is_err());
}
#[test]
fn mac_input_routes_through_guest_transport_and_preserves_guest_denial() {
    // This is a pipe/protocol fixture, not VM acceptance. The real child is
    // only cat: inspect the exact request and supply a controlled Guest reply.
    let temp = tempfile::tempdir().unwrap();
    let mut manager = Manager::load(
        test_private_state(temp.path()),
        "/unused".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    manager.configs.insert(
        "mac".into(),
        serde_json::from_value(json!({"id":"mac","guest":"macos","cpus":1,"memory_mib":512}))
            .unwrap(),
    );
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
        "mac".into(),
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
    let args = json!({"id":"mac","operation_id":"b".repeat(32),"begin":true,"events":[]});
    let mut completion = None;
    let dispatched = manager.dispatch(
        json!({"method":"input_step","arguments":args}),
        &mut completion,
    );
    // Reap our fixture before assertions so even a protocol assertion fails cleanly.
    let mut descriptor = libc::pollfd {
        fd: stdout.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut descriptor, 1, 1000) };
    unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    let wire = if ready > 0 {
        read_frame(&mut BufReader::new(stdout))
    } else {
        Err(anyhow::anyhow!("fixture output deadline"))
    };
    manager
        .running
        .get_mut("mac")
        .unwrap()
        .force_stop()
        .unwrap();
    dispatched.unwrap();
    let wire: Value = serde_json::from_slice(&wire.unwrap().unwrap()).unwrap();
    assert_eq!(wire["method"], "guest_request");
    assert_eq!(wire["request"]["method"], "input_step");
    assert_eq!(wire["request"]["arguments"], args);
    pending.lock().unwrap().remove(&1).unwrap().send(json!({"id":1,"result":{"id":"1","error":{"code":"accessibility_permission_required","message":"denied"}}})).unwrap();
    let (peer, _) = UnixStream::pair().unwrap();
    assert!(completion
        .unwrap()
        .wait(&peer)
        .unwrap_err()
        .to_string()
        .contains("accessibility_permission_required"));
}
#[test]
fn apple_guest_capabilities_do_not_conflate_screenshot_and_async_execution() {
    let mac = backend::guest_capabilities(Backend::AppleVirtualization, "macos");
    assert_eq!(mac["screenshot"], true);
    assert_eq!(mac["input"], true);
    assert_eq!(mac["command_execution"], false);
    assert_eq!(mac["async_commands"], false);
    assert!(mac["guest_integration"].as_str().unwrap().contains("macos"));
    let linux = backend::guest_capabilities(Backend::AppleVirtualization, "linux");
    assert_eq!(linux["screenshot"], true);
    assert_eq!(linux["input"], true);
    assert!(linux["input_requirements"]
        .as_str()
        .unwrap()
        .contains("desktop container"));
    assert_eq!(linux["command_execution"], true);
    assert_eq!(linux["async_commands"], true);
}
#[test]
fn mac_address_must_be_local_unicast() {
    assert!(valid_local_mac("72:76:f4:5e:d8:14"));
    assert!(!valid_local_mac("73:76:f4:5e:d8:14"), "multicast");
    assert!(
        !valid_local_mac("00:1c:42:00:00:01"),
        "universally administered"
    );
    assert!(!valid_local_mac("72:76:f4:5e:d8"));
    assert!(!valid_local_mac("72:76:f4:5e:d8:1g"));
}
#[test]
fn guest_stream_bridge_is_strict_local_apple_only() {
    let start = json!({"id":"mac","socket_path":"/private/owner/stream.sock"});
    assert!(validate_local_stream_bridge(
        "start_guest_stream_bridge",
        &start,
        Backend::AppleVirtualization,
        "macos"
    )
    .is_ok());
    assert!(validate_local_stream_bridge(
        "start_guest_stream_bridge",
        &start,
        Backend::QemuHvf,
        "macos"
    )
    .is_err());
    assert!(validate_local_stream_bridge(
        "start_guest_stream_bridge",
        &start,
        Backend::AppleVirtualization,
        "linux"
    )
    .is_ok());
    assert!(validate_local_stream_bridge(
        "start_guest_stream_bridge",
        &start,
        Backend::AppleVirtualization,
        "windows"
    )
    .is_err());
    assert!(validate_local_stream_bridge(
        "stop_guest_stream_bridge",
        &start,
        Backend::AppleVirtualization,
        "macos"
    )
    .is_err());
    assert!(validate_local_stream_bridge(
        "start_guest_stream_bridge",
        &json!({"id":"mac","socket_path":"relative"}),
        Backend::AppleVirtualization,
        "macos"
    )
    .is_err());
    assert!(!tools_with_input(true, true, true, true)
        .to_string()
        .contains("guest_stream_bridge"));
}
#[test]
fn screenshots_preserve_real_mime_and_reject_malformed_images() {
    use base64::Engine;
    for (mime, bytes) in [
        ("image/png", b"\x89PNG\r\n\x1a\n".as_slice()),
        ("image/jpeg", [0xff, 0xd8, 0xff, 0xd9].as_slice()),
    ] {
        let value = json!({"mime_type":mime,"data_base64":base64::engine::general_purpose::STANDARD.encode(bytes),"byte_length":bytes.len(),"width":1280,"height":800,"frame_id":"00000000-0000-0000-0000-000000000000"});
        let normalized = normalize_screenshot(json!({"id":"1","result":value}), true).unwrap();
        let content = screenshot_content(normalized).unwrap();
        assert_eq!(content["content"][0]["mimeType"], mime);
        let mut bad = value.clone();
        bad["mime_type"] = json!("image/svg+xml");
        assert!(normalize_screenshot(bad, false).is_err());
        let mut bad = value.clone();
        bad["byte_length"] = json!(1);
        assert!(normalize_screenshot(bad, false).is_err());
        let mut bad = value;
        bad["data_base64"] = json!("not base64");
        assert!(normalize_screenshot(bad, false).is_err());
    }
}
include!("file_routes_tests.rs");
include!("job_retention_tests.rs");
include!("exec_timeout_tests.rs");
#[test]
fn synchronous_terminal_result_preserves_exit_and_cancellation() {
    assert!(
        completed_exec_result(&json!({"result":{"state":"running"}}))
            .unwrap()
            .is_none()
    );
    let result =
        json!({"exit_code":-1,"stdout":"你好","stderr":"","cancelled":true,"timed_out":false});
    let reply =
        completed_exec_result(&json!({"id":"42","result":{"state":"finished","result":result}}))
            .unwrap()
            .unwrap();
    assert_eq!(reply, json!({"id":"42","result":result}));
    assert!(completed_exec_result(
        &json!({"result":{"state":"finished","error":{"code":"internal"}}})
    )
    .is_err());
    assert!(completed_exec_result(&json!({"result":{"state":"finished"}})).is_err());
    assert!(completed_exec_result(&json!({"result":{"state":"unknown"}})).is_err());
}
#[test]
fn unix_peer_disconnect_is_detected_without_consuming_data() {
    let (local, mut remote) = UnixStream::pair().unwrap();
    remote.write_all(b"pending bytes").unwrap();
    assert!(!peer_disconnected(&[local.as_raw_fd()]));
    drop(remote);
    // Kernel readiness is asynchronous; concurrent process creation may also
    // briefly retain a CLOEXEC descriptor until exec. Do not consume bytes.
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while !peer_disconnected(&[local.as_raw_fd()]) {
        assert!(
            std::time::Instant::now() < deadline,
            "peer EOF readiness deadline"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let mut bytes = String::new();
    (&local).read_to_string(&mut bytes).unwrap();
    assert_eq!(bytes, "pending bytes");
}
#[test]
fn blocking_peer_observation_wakes_on_hangup_and_never_spins_on_readable_data() {
    use std::os::fd::AsRawFd;
    fn pipe() -> (File, File) {
        use std::os::fd::FromRawFd;
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }
    fn observe_late_close<T: Send + 'static>(fd: std::os::fd::RawFd, peer: T) {
        let closer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            drop(peer);
        });
        let started = std::time::Instant::now();
        assert!(await_peer_disconnect(&[fd], Duration::from_secs(5)));
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(100), "elapsed={elapsed:?}");
        assert!(elapsed < Duration::from_secs(2), "elapsed={elapsed:?}");
        closer.join().unwrap();
        assert!(peer_disconnected(&[fd]));
    }
    let limit = Duration::from_secs(5);
    // Each hang-up must end one blocking observation long before its 5 s limit, for the
    // descriptor kinds an MCP stdio transport uses: both pipe ends and a Unix socket.
    let (read, write) = pipe();
    observe_late_close(read.as_raw_fd(), write);
    let (read, write) = pipe();
    observe_late_close(write.as_raw_fd(), read);
    let (local, remote) = UnixStream::pair().unwrap();
    observe_late_close(local.as_raw_fd(), remote);
    // A connected, silent peer: blocks for the whole limit and reports nothing.
    let (local, mut remote) = UnixStream::pair().unwrap();
    let started = std::time::Instant::now();
    assert!(!await_peer_disconnect(
        &[local.as_raw_fd()],
        Duration::from_millis(200)
    ));
    assert!(started.elapsed() >= Duration::from_millis(190));
    // Queued, unread bytes are not a disconnect and must cost the original 50 ms step.
    remote.write_all(b"pending bytes").unwrap();
    let started = std::time::Instant::now();
    assert!(!await_peer_disconnect(&[local.as_raw_fd()], limit));
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(45), "elapsed={elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "elapsed={elapsed:?}");
    let mut bytes = [0; 13];
    (&local).read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"pending bytes");
}
#[test]
fn exit_monitor_parks_without_live_workers_and_resumes_when_one_starts() {
    let temp = tempfile::tempdir().unwrap();
    let manager = Arc::new(Mutex::new(
        Manager::load(
            test_private_state(temp.path()),
            "/unused".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap(),
    ));
    assert_eq!(exit_monitor_pass(&manager), EXIT_MONITOR_IDLE);
    let before = EXIT_MONITOR_PASSES.load(Ordering::Relaxed);
    spawn_exit_monitor(&manager);
    thread::sleep(Duration::from_millis(600));
    // The fixed 100 ms loop made about six passes here; a parked monitor makes one.
    let idle = EXIT_MONITOR_PASSES.load(Ordering::Relaxed) - before;
    assert!((1..=2).contains(&idle), "idle passes={idle}");
    let mut child = Command::new("/bin/cat")
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    let stdin = child.stdin.take().unwrap();
    manager.lock().unwrap().started(
        "mac".into(),
        Running {
            bridge: Arc::new(Mutex::new(bridge::State::new().unwrap())),
            child,
            stdin,
            pending: Arc::new(Mutex::new(BTreeMap::new())),
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
    // Woken by registration, the monitor is back on its 100 ms cadence.
    thread::sleep(Duration::from_millis(450));
    let live = EXIT_MONITOR_PASSES.load(Ordering::Relaxed) - before - idle;
    assert!(live >= 3, "live passes={live}");
    assert!(manager.lock().unwrap().running["mac"].exit_status.is_none());
    // Only the monitor may reap here: this thread never calls poll_exit.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while manager.lock().unwrap().running["mac"].exit_status.is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "background monitor did not reap the exited worker"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(exit_monitor_pass(&manager), EXIT_MONITOR_IDLE);
}
#[test]
fn shutdown_rejects_unsupported_mode() {
    assert!(shutdown_mode(&json!({"mode":"force"})).is_err());
    assert_eq!(shutdown_mode(&json!({})).unwrap(), "backend");
    assert_eq!(shutdown_mode(&json!({"mode":"guest"})).unwrap(), "guest");
}
#[test]
fn shutdown_rejects_nonstring_mode() {
    for value in [Value::Null, json!(false), json!(12), json!(["guest"])] {
        assert!(shutdown_mode(&json!({"mode":value})).is_err());
    }
}
#[test]
fn frame_bounds() {
    assert!(read_frame(&mut &b"{}\n"[..]).unwrap().is_some());
    assert!(read_frame(&mut &b"{}"[..]).is_err());
    assert!(read_frame(&mut vec![b'a'; MAX_MESSAGE + 1].as_slice()).is_err());
}
#[test]
fn scaled_screenshot_capability_describes_unchanged_png_fallback() {
    let features = control_features(Backend::QemuHvf, "linux");
    let reason = features["screenshot_scaled_jpeg"]["reason"]
        .as_str()
        .unwrap();
    assert!(reason.contains("needs a console broker and a helper that knows it"));
    assert!(
        reason.contains("an older helper returns its full-size PNG unchanged (check mime_type)")
    );
}
#[test]
fn ids_reject_traversal() {
    for id in ["../x", "", "a/b", "x.y", "é"] {
        assert!(validate_id(id).is_err())
    }
    assert!(validate_id("linux-01_test").is_ok());
}
#[cfg(target_os = "macos")]
#[test]
fn darwin_zombie_only_group_eperm_is_reaped_without_hiding_live_group_errors() {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe {
            libc::waitid(
                libc::P_PID,
                child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        },
        0
    );
    assert_eq!(
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM)
    );
    assert!(only_waitable_leader(Some(libc::EPERM), child.id()));
    assert!(!only_waitable_leader(Some(libc::EACCES), child.id()));
    assert!(!only_waitable_leader(
        Some(libc::EPERM),
        unsafe { libc::getpgrp() } as u32
    ));
    let stdin = child.stdin.take().unwrap();
    let mut run = Running {
        bridge: Arc::new(Mutex::new(bridge::State::new().unwrap())),
        child,
        assigned_memory_mib: 0,
        expected_exit: false,
        execution_quiescing: false,
        guest_async_timeout: Arc::new(Mutex::new(GuestAsyncTimeout::default())),
        exit_status: None,
        disk_identity: None,
        stdin,
        state: Arc::new(Mutex::new(json!({}))),
        pending: Arc::new(Mutex::new(BTreeMap::new())),
        next: 1,
    };
    assert!(run.poll_exit().unwrap().unwrap().success());
    assert!(run.poll_exit().unwrap().unwrap().success());
}
#[test]
fn qemu_window_creation_requires_verified_registered_display_capability() {
    let temp = tempfile::tempdir().unwrap();
    let kernel = temp.path().join("kernel");
    fs::write(&kernel, b"protocol fixture").unwrap();
    let mut manager = Manager::load(
        test_private_state(temp.path()),
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
        screenshot_helper: None,
        console_broker: Some(kernel.clone()),
        console_helper: Some(kernel.clone()),
        venus_runtime: None,
    });
    let request = json!({"method":"create","arguments":{"backend":"qemu-hvf","id":"window-test","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel,"window":true}});
    assert!(manager.dispatch(request.clone(), &mut None).is_err());
    assert!(!temp.path().join("registry.json").exists());
    manager.qemu_window = json!({"native_window":true,"native_window_protocol":1,"display_only":true,"input_enabled":false});
    let result = manager.dispatch(request, &mut None).unwrap();
    assert_eq!(result["capabilities"]["native_window"], true);
    assert_eq!(
        result["capabilities"]["native_window_details"]["input_enabled"],
        false
    );
    assert!(
        manager.running.is_empty(),
        "capability fixture does not start a VM"
    );
}
#[test]
fn uefi_machines_register_on_both_backends_without_a_kernel() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let iso = root.join("installer.iso");
    fs::write(&iso, vec![0u8; 2048]).unwrap();
    let disk = root.join("disk.raw");
    fs::write(&disk, vec![0u8; 4096]).unwrap();
    fs::set_permissions(&disk, fs::Permissions::from_mode(0o600)).unwrap();
    fs::create_dir(root.join("state")).unwrap();
    fs::set_permissions(root.join("state"), fs::Permissions::from_mode(0o700)).unwrap();
    let mut manager = Manager::load(
        root.join("state"),
        "/unused".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    manager.qemu = Some(QemuRegistration {
        worker: iso.clone(),
        launcher: iso.clone(),
        library: iso.clone(),
        resources: root.clone(),
        spice_library: None,
        screenshot_helper: None,
        console_broker: None,
        console_helper: None,
        venus_runtime: None,
    });
    for (id, backend) in [("q", "qemu-hvf"), ("a", "apple-virtualization")] {
        let request = json!({"method":"create","arguments":{"backend":backend,"id":id,"guest":"linux","cpus":2,"memory_mib":2048,"boot":"uefi","disk":disk,"cdrom":iso,"efi_vars":root.join(format!("{id}-vars.fd"))}});
        let view = manager.dispatch(request, &mut None).unwrap();
        assert_eq!(view["config"]["boot"], "uefi");
        assert_eq!(view["capabilities"]["boot"], json!(["kernel", "uefi"]));
        assert!(view["config"].get("kernel").is_none());
    }
    // The firmware store is created by the worker, never by registration.
    assert!(!root.join("q-vars.fd").exists());
    let bad = json!({"method":"create","arguments":{"id":"x","guest":"linux","cpus":1,"memory_mib":512,"boot":"uefi","cdrom":iso}});
    assert!(manager.dispatch(bad, &mut None).is_err());
}
#[test]
fn backend_selection_is_explicit_and_unsupported_qemu_features_reject() {
    let temp = tempfile::tempdir().unwrap();
    let kernel = temp.path().join("Image");
    fs::write(&kernel, b"fixture").unwrap();
    let value = json!({"id":"test","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel});
    let apple: Config = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(apple.backend, Backend::AppleVirtualization);
    let mut value = value;
    value["backend"] = json!("qemu-hvf");
    let qemu: Config = serde_json::from_value(value.clone()).unwrap();
    assert!(qemu.validate().is_ok());
    let mut window_config = value.clone();
    window_config["window"] = json!(true);
    assert!(serde_json::from_value::<Config>(window_config)
        .unwrap()
        .validate()
        .is_ok());
    let disk = temp.path().canonicalize().unwrap().join("disk.raw");
    fs::write(&disk, vec![0u8; 4096]).unwrap();
    let mut disk_config = value.clone();
    disk_config["disk"] = json!(disk);
    assert!(serde_json::from_value::<Config>(disk_config)
        .unwrap()
        .validate()
        .is_ok());
    // Syntax can request a window; actual create/start must separately have
    // a registered helper with a validated display-only capability.
    assert!(!backend::window_supported(&Value::Null));
    for (field, unsupported) in [
        ("disk", json!(kernel)),
        ("shared_dir", json!(temp.path())),
        ("width", json!(1280)),
    ] {
        let mut bad = value.clone();
        bad[field] = unsupported;
        assert!(
            serde_json::from_value::<Config>(bad)
                .unwrap()
                .validate()
                .is_err(),
            "{field}"
        );
    }
    value["backend"] = json!("qemu-software");
    assert!(serde_json::from_value::<Config>(value).is_err());
}
#[test]
fn stale_requests_expire_but_disconnected_cleanup_still_runs() {
    fn await_disconnection(server: &UnixStream) {
        // Other parallel tests spawn real processes. Between fork and exec,
        // their child can briefly retain our CLOEXEC test-client socket.
        // Establish actual EOF before asserting disconnected-client behavior.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !peer_disconnected(&[server.as_raw_fd()]) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            peer_disconnected(&[server.as_raw_fd()]),
            "test client EOF deadline"
        );
    }
    let temp = tempfile::tempdir().unwrap();
    let mut manager = Manager::load(
        test_private_state(temp.path()),
        "/unused/apple".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    manager.configs.insert(
        "test".into(),
        serde_json::from_value(json!({"id":"test","guest":"linux","cpus":1,"memory_mib":512}))
            .unwrap(),
    );
    let owner = "ab".repeat(32);
    manager
        .file_sessions
        .insert(owner.clone(), (std::time::Instant::now(), BTreeSet::new()));
    let manager = Arc::new(Mutex::new(manager));
    let (mut client, server) = UnixStream::pair().unwrap();
    write_json(
        &mut client,
        &json!({"method":"status","arguments":{"id":"test"}}),
    )
    .unwrap();
    handle_connection(
        server,
        manager.clone(),
        std::time::Instant::now() - Duration::from_secs(31),
    )
    .unwrap();
    let reply = read_frame(&mut BufReader::new(client)).unwrap().unwrap();
    assert!(String::from_utf8(reply)
        .unwrap()
        .contains("expired before dispatch"));
    // Cleanup is deliberately processed despite caller disconnect.
    let (mut client, server) = UnixStream::pair().unwrap();
    write_json(
        &mut client,
        &json!({"method":"session_end","arguments":{"owner":owner}}),
    )
    .unwrap();
    drop(client);
    await_disconnection(&server);
    let cleanup = handle_connection(server, manager.clone(), std::time::Instant::now());
    assert!(cleanup.is_err()); // Response write fails, but cleanup is already committed.
    assert!(manager.lock().unwrap().file_sessions.is_empty());
}
#[test]
fn qemu_observer_rejects_mutation_before_retaining_cleanup_routes() {
    let mut fixture = FilePipeFixture::macos();
    fixture.manager.configs.get_mut("mac").unwrap().backend = Backend::QemuHvf;
    for method in [
        "file_upload_begin",
        "file_session_end",
        "exec_start",
        "exec_poll",
        "exec_cancel",
        "exec",
    ] {
        let mut deferred = None;
        let error = fixture
            .manager
            .dispatch(
                json!({"method":method,"arguments":{"id":"mac","owner":"a".repeat(64)}}),
                &mut deferred,
            )
            .unwrap_err();
        assert!(error.to_string().contains("observer-only"), "{error}");
        assert!(deferred.is_none());
        assert!(fixture.manager.file_sessions.is_empty());
    }
    let mut deferred = None;
    let result = fixture
        .manager
        .dispatch(
            json!({"method":"session_end","arguments":{"owner":"a".repeat(64)}}),
            &mut deferred,
        )
        .unwrap();
    assert!(result.is_null());
    let (peer, _other) = UnixStream::pair().unwrap();
    let reply = deferred.unwrap().wait(&peer).unwrap();
    assert_eq!(reply["closed"], true);
    assert_eq!(reply["file_cancellation"]["attempted"], 0);
}
#[test]
fn unregistered_qemu_cannot_be_created() {
    let temp = tempfile::tempdir().unwrap();
    let kernel = temp.path().join("Image");
    fs::write(&kernel, b"fixture").unwrap();
    let mut manager = Manager::load(
        test_private_state(temp.path()),
        "/unused/apple".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    let mut deferred = None;
    let error=manager.dispatch(json!({"method":"create","arguments":{"id":"test","backend":"qemu-hvf","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel}}),&mut deferred).unwrap_err();
    assert!(error.to_string().contains("not registered"));
    assert!(!temp.path().join("registry.json").exists());
}
#[test]
fn windows_linux_and_windows_return_typed_unsupported_before_worker_lookup() {
    let tools = tools_list(false, false);
    let windows = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "vm_windows")
        .unwrap();
    assert!(windows["description"]
        .as_str()
        .unwrap()
        .contains("Linux and Windows Guests return unsupported"));
    let temp = tempfile::tempdir().unwrap();
    let mut manager = Manager::load(
        test_private_state(temp.path()),
        "/unused".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    // Inject refused Guest kinds to test dispatch without registering or starting a VM.
    for (backend, guest) in [
        (Backend::AppleVirtualization, "linux"),
        (Backend::QemuHvf, "linux"),
        (Backend::QemuHvf, "windows"),
        (Backend::QemuHvf, "macos"),
    ] {
        manager.configs.insert(
            "observe".into(),
            serde_json::from_value(
                json!({"id":"observe","backend":backend,"guest":guest,"cpus":1,"memory_mib":512}),
            )
            .unwrap(),
        );
        let error = manager
            .dispatch(
                json!({"method":"windows","arguments":{"id":"observe"}}),
                &mut None,
            )
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "unsupported: window enumeration requires enabled macOS Guest observation service"
        );
    }
    assert!(manager.running.is_empty());
}
#[test]
fn control_features_state_every_cell_with_status_reason_and_real_guest_evidence() {
    // One row per feature; columns are Apple macOS, Apple Linux, QEMU Linux, QEMU Windows.
    const S: &str = "supported";
    const I: &str = "implemented_unverified";
    const P: &str = "requires_guest_permission";
    const N: &str = "unsupported";
    let table: [(&str, [&str; 4]); 23] = [
        ("lifecycle", [S, S, S, I]),
        ("command_execution", [N, S, S, I]),
        ("async_jobs", [N, S, S, N]),
        ("file_transfer", [S, S, N, I]),
        ("screenshot", [S, I, S, N]),
        ("screenshot_scaled_jpeg", [N, N, I, N]),
        ("pointer_key_input", [P, I, S, N]),
        ("unicode_text_input", [N, N, I, N]),
        ("window_metadata", [S, I, N, I]),
        ("accessibility_metadata", [I, I, N, I]),
        ("application_tools", [I, I, N, I]),
        ("clipboard", [N, N, N, N]),
        ("snapshot", [N, I, N, N]),
        ("memory_reclaim", [N, N, I, I]),
        ("multi_display_input_mapping", [N, N, N, N]),
        ("retina_input_mapping", [I, N, N, N]),
        ("shared_folders", [S, I, N, N]),
        ("guest_shutdown", [N, S, N, N]),
        ("balloon", [N, N, I, I]),
        ("gpu_3d", [S, N, S, I]),
        ("application_launch_list", [I, I, N, I]),
        ("accessibility_tree", [I, I, N, I]),
        ("window_list", [S, I, N, I]),
    ];
    assert_eq!(table.map(|(name, _)| name), CONTROL_FEATURES);
    let pairs = [
        (Backend::AppleVirtualization, "macos"),
        (Backend::AppleVirtualization, "linux"),
        (Backend::QemuHvf, "linux"),
        (Backend::QemuHvf, "windows"),
    ];
    for (column, (backend, guest)) in pairs.into_iter().enumerate() {
        let cells = control_features(backend, guest);
        assert_eq!(cells.as_object().unwrap().len(), table.len(), "{guest}");
        for (name, expected) in table {
            let cell = &cells[name];
            let at = format!("{backend:?} {guest} {name}: {cell}");
            assert_eq!(cell["status"], expected[column], "{at}");
            assert!(FEATURE_STATUSES.contains(&cell["status"].as_str().unwrap()));
            let reason = cell["reason"].as_str().expect(&at);
            assert!(reason.len() > 20 && reason.len() < 260, "{at}");
            // Supported means a real-Guest record that exists in this tree, and nothing else carries one.
            assert_eq!(
                cell.get("evidence").is_some(),
                expected[column] == S,
                "{at}"
            );
            assert_eq!(
                cell.as_object().unwrap().len(),
                2 + usize::from(expected[column] == S)
            );
            if let Some(evidence) = cell.get("evidence") {
                crate::capabilities_tests::assert_archived(evidence.as_str().unwrap());
            }
            // The older report never claims more than this one.
            let old = &product_capabilities(backend, guest)[name];
            if PRODUCT_CAPABILITIES.contains(&name) && old["status"] == N {
                assert_eq!(cell, old, "{at}");
            }
        }
    }
    // Memory reclaim: Apple Virtualization returns no Guest memory (and says what was measured);
    // QEMU Linux returns what free page reporting hands back.
    let reason = control_features(Backend::AppleVirtualization, "linux")["memory_reclaim"]
        ["reason"]
        .to_string();
    for part in [
        "measured 2026-10-12",
        "raised the host footprint by 933 MiB",
    ] {
        assert!(reason.contains(part), "{reason}");
    }
    let reason =
        control_features(Backend::QemuHvf, "linux")["memory_reclaim"]["reason"].to_string();
    assert!(reason.contains("free page reporting"), "{reason}");
    let mac = control_features(Backend::AppleVirtualization, "macos");
    assert!(mac["pointer_key_input"]["reason"]
        .to_string()
        .contains("accessibility_permission_required"));
    assert!(mac["retina_input_mapping"]["reason"]
        .to_string()
        .contains("unit test"));
    assert!(
        control_features(Backend::QemuHvf, "windows")["clipboard"]["reason"]
            .to_string()
            .contains("not implemented for Windows guests")
    );

    // A registered VM reports the same cells, narrowed by its registration and by what this daemon has registered.
    let temp = tempfile::tempdir().unwrap();
    let kernel = temp.path().join("kernel");
    fs::write(&kernel, b"registry only, never started").unwrap();
    let mut manager = Manager::load(
        test_private_state(temp.path()),
        "/unused".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    for (id, config) in [
        (
            "exec",
            json!({"backend":"qemu-hvf","guest":"linux","qemu_guest_exec":true}),
        ),
        ("observer", json!({"backend":"qemu-hvf","guest":"linux"})),
        ("vzlinux", json!({"guest":"linux"})),
        ("mac", json!({"guest":"macos"})),
    ] {
        let mut config = config;
        config["id"] = json!(id);
        config["cpus"] = json!(1);
        config["memory_mib"] = json!(512);
        manager
            .configs
            .insert(id.into(), serde_json::from_value(config).unwrap());
    }
    let cells = |manager: &mut Manager, id: &str| {
        manager.view(id).unwrap()["capabilities"]["control_features"].clone()
    };
    assert_eq!(cells(&mut manager, "mac"), mac);
    // Known limits appear in a macOS Guest's status and in no other VM's.
    assert_eq!(
        manager.view("mac").unwrap()["capabilities"]["known_limits"],
        known_limits(Backend::AppleVirtualization, "macos")
    );
    for id in ["vzlinux", "exec"] {
        assert!(
            manager.view(id).unwrap()["capabilities"]
                .get("known_limits")
                .is_none(),
            "{id}"
        );
    }
    let mut without_identity = control_features(Backend::AppleVirtualization, "linux");
    without_identity["snapshot"] = json!({"status":"unsupported","reason":"This VM has no explicit persistent Linux machine_identity_dir. Snapshot capture refuses it; existing VMs are never silently assigned a new snapshot identity."});
    assert_eq!(cells(&mut manager, "vzlinux"), without_identity);
    assert_eq!(
        manager.view("vzlinux").unwrap()["capabilities"]["snapshot_kinds"]["full_state"],
        without_identity["snapshot"]
    );
    // Explicit registration opts in; capability reporting must not create or assign an identity.
    let identity = manager.dir.join("test-linux-identity");
    assert!(!identity.exists());
    manager
        .configs
        .get_mut("vzlinux")
        .unwrap()
        .machine_identity_dir = Some(identity.clone());
    assert_eq!(
        cells(&mut manager, "vzlinux"),
        control_features(Backend::AppleVirtualization, "linux")
    );
    let kinds = manager.view("vzlinux").unwrap()["capabilities"]["snapshot_kinds"].clone();
    assert_eq!(kinds["full_state"]["status"], I);
    assert_eq!(kinds["disk_rollback"]["named_create"], false);
    assert_eq!(kinds["disk_rollback"]["restore"], false);
    assert_eq!(kinds["disk_rollback"]["automatic_restore_protection"], true);
    assert_eq!(kinds["suspend"]["save"], false);
    assert_eq!(kinds["suspend"]["resume"], false);
    assert_eq!(kinds["operation_fenced"], false);
    assert!(!identity.exists());
    // No QEMU registration: screenshot and input are refused by this daemon, and the report says so.
    let bare = cells(&mut manager, "exec");
    assert_eq!(bare["gpu_3d"]["status"], N);
    assert!(bare["gpu_3d"]["reason"]
        .as_str()
        .unwrap()
        .contains("no registered QEMU runtime"));
    for name in [
        "screenshot",
        "screenshot_scaled_jpeg",
        "pointer_key_input",
        "unicode_text_input",
    ] {
        assert_eq!(bare[name]["status"], N, "{name}");
        assert!(bare[name]["reason"]
            .to_string()
            .contains("this daemon has no registered QEMU"));
    }
    assert_eq!(bare["command_execution"]["status"], S);
    manager.qemu = Some(QemuRegistration {
        worker: kernel.clone(),
        launcher: kernel.clone(),
        library: kernel.clone(),
        resources: temp.path().into(),
        spice_library: None,
        screenshot_helper: Some(kernel.clone()),
        console_broker: Some(kernel.clone()),
        console_helper: Some(kernel.clone()),
        venus_runtime: None,
    });
    assert_eq!(
        cells(&mut manager, "exec"),
        control_features(Backend::QemuHvf, "linux")
    );
    assert_eq!(
        manager.view("exec").unwrap()["capabilities"]["screenshot_scaled_jpeg"],
        true
    );
    assert_eq!(
        cells(&mut manager, "exec")["screenshot_scaled_jpeg"]["status"],
        I
    );
    manager.qemu.as_mut().unwrap().console_broker = None;
    let without_broker = manager.view("exec").unwrap();
    assert_eq!(without_broker["capabilities"]["screenshot"], true);
    assert_eq!(
        without_broker["capabilities"]["screenshot_scaled_jpeg"],
        false
    );
    assert_eq!(
        without_broker["capabilities"]["control_features"]["screenshot_scaled_jpeg"]["status"],
        N
    );
    manager.qemu.as_mut().unwrap().console_broker = Some(kernel.clone());
    let observer = cells(&mut manager, "observer");
    for (name, _) in table {
        let narrowed = ["command_execution", "async_jobs", "unicode_text_input"].contains(&name);
        if narrowed {
            assert_eq!(observer[name]["status"], N, "{name}");
            assert!(observer[name]["reason"]
                .to_string()
                .contains("without qemu_guest_exec"));
        } else {
            assert_eq!(
                observer[name],
                control_features(Backend::QemuHvf, "linux")[name],
                "{name}"
            );
        }
    }
    // The daemon refuses what the cells call unsupported, by name, before any worker is consulted.
    for (id, method, arguments) in [
        ("vzlinux", "windows", json!({})),
        ("exec", "windows", json!({})),
        ("exec", "file_upload_begin", json!({})),
        ("exec", "shutdown", json!({"mode":"guest"})),
        ("observer", "exec_start", json!({})),
    ] {
        let mut arguments = arguments;
        arguments["id"] = json!(id);
        let error = manager
            .dispatch(json!({"method":method,"arguments":arguments}), &mut None)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("unsupported: "), "{id} {method}: {error}");
    }
}
#[test]
fn text_input_is_gated_by_vm_kind_and_never_echoes_text() {
    // Pipe/protocol fixture, not VM acceptance: the "worker" is cat.
    let temp = tempfile::tempdir().unwrap();
    let kernel = temp.path().join("kernel");
    fs::write(&kernel, b"protocol fixture").unwrap();
    let mut manager = Manager::load(
        test_private_state(temp.path()),
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
        screenshot_helper: None,
        console_broker: Some(kernel.clone()),
        console_helper: Some(kernel.clone()),
        venus_runtime: None,
    });
    for (id, config) in [
        (
            "exec",
            json!({"backend":"qemu-hvf","guest":"linux","qemu_guest_exec":true}),
        ),
        ("observer", json!({"backend":"qemu-hvf","guest":"linux"})),
        ("vzlinux", json!({"guest":"linux"})),
        ("mac", json!({"guest":"macos"})),
    ] {
        let mut config = config;
        config["id"] = json!(id);
        config["cpus"] = json!(1);
        config["memory_mib"] = json!(512);
        manager
            .configs
            .insert(id.into(), serde_json::from_value(config).unwrap());
    }
    let secret = "correct horse \u{4f60}\u{597d}";
    let frame = json!({"frame_id":"f".repeat(36),"width":1280,"height":800,"source_generation":1});
    let arguments = |id: &str| {
        let mut value = frame.clone();
        value["id"] = json!(id);
        value["text"] = json!(secret);
        value
    };
    // Every other backend/Guest pair: the same explicit refusal, before any console check.
    for id in ["observer", "vzlinux", "mac"] {
        for method in ["input_text", input::TEXT_STEP_METHOD] {
            let mut deferred = None;
            let error = manager
                .dispatch(
                    json!({"method":method,"arguments":arguments(id)}),
                    &mut deferred,
                )
                .unwrap_err()
                .to_string();
            assert_eq!(error, input::TEXT_UNSUPPORTED_VM, "{id} {method}");
            assert!(deferred.is_none() && !error.contains("horse"));
        }
        assert_eq!(
            manager.view(id).unwrap()["capabilities"]["product_capabilities"]["unicode_text_input"]
                ["status"],
            "unsupported",
            "{id}"
        );
    }
    assert_eq!(
        manager.view("exec").unwrap()["capabilities"]["product_capabilities"]["unicode_text_input"]
            ["status"],
        "supported"
    );
    // Invalid text is refused before anything is deferred, and the refusal does not quote it.
    let mut bad = arguments("exec");
    bad["text"] = json!(format!("{secret}\u{1b}"));
    let mut deferred = None;
    let error = manager
        .dispatch(
            json!({"method":"input_text","arguments":bad}),
            &mut deferred,
        )
        .unwrap_err()
        .to_string();
    assert!(deferred.is_none() && !error.contains("horse"), "{error}");
    let mut deferred = None;
    manager
        .dispatch(
            json!({"method":"input_text","arguments":arguments("exec")}),
            &mut deferred,
        )
        .unwrap();
    assert!(matches!(deferred, Some(Completion::Input { .. })));
    // A chunk for a VM whose process is not active goes nowhere.
    assert!(manager
        .dispatch(
            json!({"method":input::TEXT_STEP_METHOD,"arguments":arguments("exec")}),
            &mut None
        )
        .unwrap_err()
        .to_string()
        .contains("not active"));

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
        "exec".into(),
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
    let mut step = arguments("exec");
    step["operation_id"] = json!("b".repeat(32));
    step["begin"] = json!(true);
    step["index"] = json!(0);
    let mut first = None;
    let mut second = None;
    let sent = [&mut first, &mut second].map(|completion| {
        manager.dispatch(
            json!({"method":input::TEXT_STEP_METHOD,"arguments":step}),
            completion,
        )
    });
    let mut reader = BufReader::new(stdout);
    let mut descriptor = libc::pollfd {
        fd: reader.get_ref().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut descriptor, 1, 1000) };
    let wire = if ready > 0 {
        read_frame(&mut reader)
    } else {
        Err(anyhow::anyhow!("fixture output deadline"))
    };
    manager
        .running
        .get_mut("exec")
        .unwrap()
        .force_stop()
        .unwrap();
    for result in sent {
        result.unwrap();
    }
    let wire: Value = serde_json::from_slice(&wire.unwrap().unwrap()).unwrap();
    assert_eq!(wire["method"], input::TEXT_STEP_METHOD);
    assert_eq!(wire["arguments"], step);
    // A Guest refusal reaches the caller as a coded reply for delivery accounting, and a
    // success is reduced to counts: the Guest's per-key diagnostics never leave the daemon.
    let keysym = 0x0100_4f60u32;
    pending.lock().unwrap().remove(&1).unwrap().sender.send(json!({"id":1,"result":{"id":"text","error":{"code":"not_ready","message":"no window has keyboard focus; nothing was injected"}}})).unwrap();
    pending.lock().unwrap().remove(&2).unwrap().sender.send(json!({"id":2,"result":{"id":"text","result":{"injected_scalars":16,"injected_bytes":20,"backend":"x11-xtest-keymap","keymap_restore":"deferred","guest_delivery_confirmed":false,"batches":1,"waited_ms":0,"pool_keycodes":40,"bindings":[{"keycode":255,"keysym":keysym,"readback":[keysym,keysym],"reused":false}],"echo":secret}}})).unwrap();
    let (peer, _other) = UnixStream::pair().unwrap();
    let refused = first.unwrap().wait(&peer).unwrap();
    assert_eq!(
        refused,
        json!({"error":{"code":"not_ready","message":"no window has keyboard focus; nothing was injected"}})
    );
    let accepted = second.unwrap().wait(&peer).unwrap();
    assert_eq!(
        accepted,
        json!({"result":{"injected_scalars":16,"injected_bytes":20,"backend":"x11-xtest-keymap","keymap_restore":"deferred","guest_delivery_confirmed":false,"batches":1,"waited_ms":0,"pool_keycodes":40}})
    );
    // Nothing the daemon wrote to its state directory holds the text.
    fn scan(path: &Path, needle: &[u8]) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap().path();
            if entry.is_dir() {
                scan(&entry, needle)
            } else if entry.is_file() {
                let bytes = fs::read(&entry).unwrap();
                assert!(
                    !bytes.windows(needle.len()).any(|w| w == needle),
                    "{} holds the text",
                    entry.display()
                );
            }
        }
    }
    scan(temp.path(), b"correct horse");
    scan(temp.path(), b"636f727265637420686f727365");
}
#[test]
fn desktop_text_chunks_accounts_and_cleans_up_against_a_scripted_daemon() {
    // The daemon side of the socket is scripted: this checks the per-call loop only
    // (capability query, chunk requests, refusal classes, cleanup), not a VM.
    use std::os::unix::net::UnixListener;
    type Script = Box<dyn Fn(&Value, usize) -> Value + Send>;
    let info = json!({"id":"1","result":{"capabilities":{"text_input":true},"text_input":{"enabled":true,"available":true,"backend":"x11-xtest-keymap","max_scalars_per_request":16,"max_bytes_per_request":64}}});
    let accepted = |chunk: &str| json!({"result":{"result":{"injected_scalars":chunk.chars().count(),"injected_bytes":chunk.len(),"backend":"x11-xtest-keymap","keymap_restore":"deferred","guest_delivery_confirmed":false}}});
    let text = format!("{}\u{4f60}\u{597d}", "a".repeat(38));
    let run = |script: Script| -> (Result<Value>, Vec<Value>) {
        let temp = tempfile::Builder::new()
            .prefix("vdt-")
            .tempdir_in("/tmp")
            .unwrap();
        let listener = UnixListener::bind(temp.path().join("control.sock")).unwrap();
        let (done, finished) = mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let mut seen = Vec::new();
            listener.set_nonblocking(true).unwrap();
            loop {
                match listener.accept() {
                    Ok((socket, _)) => {
                        socket.set_nonblocking(false).unwrap();
                        let mut reader = BufReader::new(socket.try_clone().unwrap());
                        let request: Value =
                            serde_json::from_slice(&read_frame(&mut reader).unwrap().unwrap())
                                .unwrap();
                        let steps = seen
                            .iter()
                            .filter(|r: &&Value| r["method"] == input::TEXT_STEP_METHOD)
                            .count();
                        let reply = script(&request, steps);
                        seen.push(request);
                        let mut socket = socket;
                        write_json(&mut socket, &reply).unwrap();
                    }
                    Err(_) if finished.try_recv().is_ok() => return seen,
                    Err(_) => thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        let (peer, _held) = UnixStream::pair().unwrap();
        let arguments = json!({"id":"vm","frame_id":"f".repeat(36),"width":1280,"height":800,"source_generation":1,"text":text});
        let result = desktop_text(temp.path(), arguments, &peer);
        done.send(()).unwrap();
        (result, server.join().unwrap())
    };
    // Success: one capability query, three chunks of 16/16/8 under one operation, one cleanup without text.
    let reply_info = info.clone();
    let (result, seen) = run(Box::new(move |request, _| {
        match request["method"].as_str().unwrap() {
            "guest_info" => json!({"result":reply_info}),
            "input_cleanup" => json!({"result":{"released":true}}),
            _ => accepted(request["arguments"]["text"].as_str().unwrap()),
        }
    }));
    let result = result.unwrap();
    assert_eq!(
        (
            result["chunks"].as_u64(),
            result["scalars"].as_u64(),
            result["bytes"].as_u64()
        ),
        (Some(3), Some(40), Some(44))
    );
    let methods: Vec<&str> = seen.iter().map(|r| r["method"].as_str().unwrap()).collect();
    assert_eq!(
        methods,
        [
            "guest_info",
            "input_text_step",
            "input_text_step",
            "input_text_step",
            "input_cleanup"
        ]
    );
    assert_eq!(seen[0]["arguments"], json!({"id":"vm"}));
    let operation = seen[1]["arguments"]["operation_id"].as_str().unwrap();
    assert!(operation.len() == 32 && operation.bytes().all(|b| b.is_ascii_hexdigit()));
    let mut joined = String::new();
    for (index, request) in seen[1..4].iter().enumerate() {
        let arguments = &request["arguments"];
        assert_eq!(arguments["begin"], index == 0);
        assert_eq!(arguments["index"], index);
        assert_eq!(arguments["operation_id"], operation);
        assert!(arguments["text"].as_str().unwrap().chars().count() <= input::TEXT_CHUNK_SCALARS);
        joined += arguments["text"].as_str().unwrap();
    }
    assert_eq!(joined, text);
    assert!(
        seen[4]["arguments"]["text"].is_null() && seen[4]["arguments"]["operation_id"] == operation
    );
    // Guest without the service: explicit unsupported, no chunk and no cleanup request.
    let (result, seen) = run(Box::new(
        |_, _| json!({"result":{"id":"1","result":{"capabilities":{"text_input":false},"text_input":{"enabled":false,"available":false,"reason":"text input disabled at guest service launch (--allow-text-input)"}}}}),
    ));
    let error = result.unwrap_err().to_string();
    assert!(
        error.starts_with("unsupported: Guest text input service unavailable")
            && error.contains("0 of 40 characters delivered"),
        "{error}"
    );
    assert_eq!(seen.len(), 1);
    // The daemon refuses the first chunk (busy worker): nothing was sent, nothing to clean up.
    let reply_info = info.clone();
    let (result, seen) = run(Box::new(move |request, _| {
        match request["method"].as_str().unwrap() {
            "guest_info" => json!({"result":reply_info}),
            _ => json!({"error":"input busy: another operation is active"}),
        }
    }));
    let error = result.unwrap_err().to_string();
    assert!(
        error.starts_with(
            "busy: not sent: input busy: another operation is active (0 of 40 characters delivered;"
        ),
        "{error}"
    );
    assert_eq!(seen.len(), 2);
    // The worker refuses the second chunk: 16 delivered, none uncertain, cleanup still runs.
    let reply_info = info.clone();
    let (result, seen) = run(Box::new(move |request, steps| {
        match request["method"].as_str().unwrap() {
            "guest_info" => json!({"result":reply_info}),
            "input_cleanup" => json!({"result":{"released":true}}),
            _ if steps == 0 => accepted(request["arguments"]["text"].as_str().unwrap()),
            _ => json!({"error":"backend: \"screenshot observation expired\""}),
        }
    }));
    let error = result.unwrap_err().to_string();
    assert!(
        error.starts_with("not_delivered: not sent: backend:")
            && error.contains("(16 of 40 characters delivered; the remainder was not sent)"),
        "{error}"
    );
    assert_eq!(seen.last().unwrap()["method"], "input_cleanup");
    // The Guest refuses the second chunk, and a lost worker answer on it stays uncertain.
    for (reply, expected) in [
        (json!({"result":{"error":{"code":"not_ready","message":"no window has keyboard focus"}}}), "not_ready: Guest: no window has keyboard focus (16 of 40 characters delivered; the remainder was not sent)"),
        (json!({"result":{"error":{"code":"partial_delivery","message":"injected 3 of 16"}}}), "partial_delivery: Guest: injected 3 of 16 (16 of 40 characters delivered, up to 16 more unconfirmed; the remainder was not sent)"),
        (json!({"error":"backend response unavailable (timed out); query status before retrying"}), "guest_protocol: text chunk transport failed: backend response unavailable (timed out); query status before retrying (16 of 40 characters delivered, up to 16 more unconfirmed; the remainder was not sent)"),
    ] {
        let reply_info = info.clone();
        let (result, seen) = run(Box::new(move |request, steps| match request["method"].as_str().unwrap() {
            "guest_info" => json!({"result":reply_info}),
            "input_cleanup" => json!({"result":{"released":true}}),
            _ if steps == 0 => accepted(request["arguments"]["text"].as_str().unwrap()),
            _ => reply.clone(),
        }));
        let error = result.unwrap_err().to_string();
        assert_eq!(error, expected);
        assert_eq!(seen.len(), 4);
        assert!(!error.contains("aaaa"));
    }
    // Unconfirmed cleanup is reported with the delivery result, still without the text.
    let reply_info = info.clone();
    let (result, _) = run(Box::new(move |request, _| {
        match request["method"].as_str().unwrap() {
            "guest_info" => json!({"result":reply_info}),
            "input_cleanup" => json!({"error":"backend: unknown input operation"}),
            _ => accepted(request["arguments"]["text"].as_str().unwrap()),
        }
    }));
    let error = result.unwrap_err().to_string();
    assert_eq!(
        error,
        "input cleanup unconfirmed: backend: unknown input operation; text result: delivered"
    );
}
#[test]
fn input_permission_is_independent() {
    let only = tools_with_input(false, false, false, true).to_string();
    assert!(only.contains("vm_input_click") && !only.contains("vm_acquire_control"));
    assert!(
        only.contains("vm_window_action")
            && only.contains("vm_app_launch")
            && only.contains("vm_accessibility_action")
    );
    assert!(
        !only.contains("vm_exec") && !only.contains("vm_file_") && !only.contains("vm_force_stop")
    );
    assert!(!tools_with_input(true, true, true, false)
        .to_string()
        .contains("vm_input_"));
    // Text input is an input tool: granted by the input permission and by nothing else.
    assert!(only.contains("vm_input_text"));
    let tools = tools_with_input(false, false, false, true);
    let text = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "vm_input_text")
        .unwrap();
    let key = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "vm_input_key")
        .unwrap();
    let fences = |tool: &Value| {
        let mut required: Vec<String> = tool["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| name.as_str().unwrap().to_string())
            .filter(|name| name != "text" && name != "keys")
            .collect();
        required.sort();
        required
    };
    assert_eq!(fences(text), fences(key));
    assert_eq!(text["inputSchema"]["additionalProperties"], false);
    assert!(text["inputSchema"]["properties"]["text"]["maxLength"].is_u64());
}
#[test]
fn tool_permissions() {
    let list = tools_list(false, false);
    // 14 observe vm_* tools plus 5 read-only container/image tools.
    assert_eq!(list["tools"].as_array().unwrap().len(), 19);
    assert!(
        list.to_string().contains("vm_screen_wait")
            && list.to_string().contains("vm_screen_text")
            && list.to_string().contains("vm_desktop_state")
            && list.to_string().contains("vm_desktop_apps")
    );
    assert!(
        !list.to_string().contains("vm_window_action")
            && !list.to_string().contains("vm_app_launch")
            && !list.to_string().contains("vm_accessibility_action")
    );
    let names: BTreeSet<_> = list["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(names.contains("vm_snapshot_list") && names.contains("vm_snapshot_status"));
    let lifecycle = tools_list(true, false);
    let lifecycle_names: BTreeSet<_> = lifecycle["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for method in [
        "vm_snapshot_create",
        "vm_snapshot_restore",
        "vm_snapshot_delete",
        "vm_snapshot_reconcile",
        "vm_snapshot_resume_source",
    ] {
        assert!(
            !names.contains(method),
            "observation alone cannot mutate snapshots"
        );
        assert!(lifecycle_names.contains(method));
    }
    assert!(list.to_string().contains("vm_screenshot"));
    assert!(!list.to_string().contains("vm_start"));
    assert!(tools_list(true, false).to_string().contains("vm_start"));
    assert!(!tools_list(true, true)
        .to_string()
        .contains("vm_file_upload_begin"));
    let files = tools_with_files(false, false, true).to_string();
    assert!(files.contains("vm_file_upload_begin") && files.contains("vm_file_download_read"));
    assert!(!files.contains("vm_exec") && !files.contains("vm_acquire_control"));
}

// Real child pipes exercise transport framing only; cat is not a Guest and
// these controlled replies do not establish macOS file-service acceptance.
struct FilePipeFixture {
    manager: Manager,
    output: BufReader<std::process::ChildStdout>,
    _directory: tempfile::TempDir,
}
impl FilePipeFixture {
    fn macos() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut manager = Manager::load(
            test_private_state(directory.path()),
            "/unused".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        manager.configs.insert(
            "mac".into(),
            serde_json::from_value(json!({"id":"mac","guest":"macos","cpus":1,"memory_mib":512}))
                .unwrap(),
        );
        let mut child = Command::new("/bin/cat")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        assert!(unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } >= 0);
        manager.running.insert(
            "mac".into(),
            Running {
                bridge: Arc::new(Mutex::new(bridge::State::new().unwrap())),
                child,
                stdin,
                pending: Arc::new(Mutex::new(BTreeMap::new())),
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
        Self {
            manager,
            output: BufReader::new(output),
            _directory: directory,
        }
    }
    fn forward(&mut self, method: &str, args: Value, reply: Value) -> (Value, Result<Value>) {
        self.forward_envelope(method, args, |number| json!({"id":number,"result":reply}))
    }
    /// As `forward`, but the caller writes the whole worker reply, so a worker-level error (the Guest was never
    /// reached) can be told apart from an error the Guest itself returned inside `result`.
    fn forward_envelope(
        &mut self,
        method: &str,
        args: Value,
        envelope: impl FnOnce(u64) -> Value,
    ) -> (Value, Result<Value>) {
        let mut completion = None;
        self.manager
            .dispatch(json!({"method":method,"arguments":args}), &mut completion)
            .unwrap();
        // serde_json::to_writer emits separate writes; a readable pipe may
        // contain only a prefix. Retain it across EAGAIN under one deadline.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut bytes = Vec::new();
        while bytes.last() != Some(&b'\n') {
            assert!(
                std::time::Instant::now() < deadline && bytes.len() <= MAX_MESSAGE,
                "fixture transport deadline or size bound"
            );
            let mut byte = [0];
            match self.output.read(&mut byte) {
                Ok(1) => bytes.push(byte[0]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let mut descriptor = libc::pollfd {
                        fd: self.output.get_ref().as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    assert!(unsafe { libc::poll(&mut descriptor, 1, 20) } >= 0);
                }
                other => panic!("fixture pipe ended unexpectedly: {other:?}"),
            }
        }
        let wire: Value = serde_json::from_slice(
            &read_frame(&mut std::io::Cursor::new(bytes))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let number = wire["id"].as_u64().unwrap();
        self.manager.running["mac"]
            .pending
            .lock()
            .unwrap()
            .remove(&number)
            .unwrap()
            .send(envelope(number))
            .unwrap();
        let (peer, _other) = UnixStream::pair().unwrap();
        (wire, completion.unwrap().wait(&peer))
    }
}
impl Drop for FilePipeFixture {
    fn drop(&mut self) {
        let _ = self.manager.running.get_mut("mac").unwrap().force_stop();
    }
}
// Exercise the same child-pipe fixture without a Unix socket: this route needs only Pending::wait.
fn stream_status_reply(
    fixture: &mut FilePipeFixture,
    method: &str,
    reply: Value,
) -> (Value, Result<Value>) {
    let mut completion = None;
    fixture
        .manager
        .dispatch(
            json!({"method":method,"arguments":{"id":"mac"}}),
            &mut completion,
        )
        .unwrap();
    let Some(Completion::Backend(pending)) = completion else {
        panic!("expected backend completion")
    };
    if method == "guest_stream_status" {
        assert_eq!(pending.timeout, Duration::from_secs(3));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let mut bytes = Vec::new();
    while bytes.last() != Some(&b'\n') {
        assert!(std::time::Instant::now() < deadline && bytes.len() <= MAX_MESSAGE);
        let mut byte = [0];
        match fixture.output.read(&mut byte) {
            Ok(1) => bytes.push(byte[0]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            other => panic!("fixture pipe ended unexpectedly: {other:?}"),
        }
    }
    let wire = serde_json::from_slice(&bytes).unwrap();
    fixture.manager.running["mac"]
        .pending
        .lock()
        .unwrap()
        .remove(&pending.number)
        .unwrap()
        .send(reply)
        .unwrap();
    (wire, pending.wait())
}
#[test]
fn guest_stream_status_forwards_one_probe_and_preserves_typed_status() {
    let mut fixture = FilePipeFixture::macos();
    for (index, error) in [
        Value::Null,
        json!("guest_unavailable"),
        json!("timeout"),
        json!("invalid_state"),
        json!("busy"),
    ]
    .into_iter()
    .enumerate()
    {
        let result = json!({"reachable":error.is_null(),"guest_port":5001,"error":error});
        let (wire, got) = stream_status_reply(
            &mut fixture,
            "guest_stream_status",
            json!({"result":result}),
        );
        assert_eq!(wire, json!({"id":index+1,"method":"guest_stream_probe"}));
        assert_eq!(got.unwrap(), result);
    }
    let info = json!({"id":"6","result":{"screenshot_available":true}});
    let (wire, got) = stream_status_reply(&mut fixture, "guest_info", json!({"result":info}));
    assert_eq!(
        wire,
        json!({"id":6,"method":"guest_request","request":{"id":"6","method":"info"}})
    );
    assert_eq!(
        got.unwrap(),
        info,
        "integration info stays a pure pass-through"
    );
}
#[test]
fn guest_stream_status_old_worker_is_unknown_not_unreachable() {
    let mut fixture = FilePipeFixture::macos();
    for error in [
        json!({"code":"unsupported","message":"Unsupported native backend method"}),
        json!({"code":"unknown_command","message":"unknown command"}),
        json!("unknown command"),
    ] {
        let started = std::time::Instant::now();
        let (_, result) =
            stream_status_reply(&mut fixture, "guest_stream_status", json!({"error":error}));
        assert_eq!(
            result.unwrap(),
            json!({"reachable":null,"reason":"worker_does_not_support_probe"})
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    let (_, result) = stream_status_reply(
        &mut fixture,
        "guest_stream_status",
        json!({"error":{"code":"backend_error","message":"failure"}}),
    );
    assert!(result.unwrap_err().to_string().starts_with("backend: "));
}
#[test]
fn guest_stream_status_rejects_other_backends_guests_and_extra_arguments() {
    let mut fixture = FilePipeFixture::macos();
    for (backend, guest, args) in [
        (Backend::QemuHvf, "linux", json!({"id":"mac"})),
        (Backend::AppleVirtualization, "linux", json!({"id":"mac"})),
        (
            Backend::AppleVirtualization,
            "macos",
            json!({"id":"mac","guest_port":5000}),
        ),
    ] {
        let config = fixture.manager.configs.get_mut("mac").unwrap();
        config.backend = backend;
        config.guest = guest.into();
        let mut completion = None;
        assert!(fixture
            .manager
            .dispatch(
                json!({"method":"guest_stream_status","arguments":args}),
                &mut completion
            )
            .is_err());
        assert!(completion.is_none());
        assert_eq!(fixture.manager.running["mac"].next, 1);
    }
}
/// The macOS Guest answers `info`, `screenshot` and `windows` from one service (vsock port 5000). The second Guest
/// service (the video stream, port 5001) is not asked: each call is exactly one worker `guest_request`. When the
/// worker cannot connect it answers `guest_unavailable` at once; the controller must pass that code on as a
/// `backend:` error as soon as it arrives, not wait out its own bound (10 s info, 17 s screenshot, 12 s windows)
/// and not turn it into a result.
#[test]
fn mac_guest_service_unreachable_is_a_prompt_typed_backend_error_for_info_screenshot_windows() {
    let mut fixture = FilePipeFixture::macos();
    for (number, (method, guest_method)) in [
        ("guest_info", "info"),
        ("screenshot", "screenshot"),
        ("windows", "windows"),
    ]
    .into_iter()
    .enumerate()
    {
        let number = number as u64 + 1;
        let started = std::time::Instant::now();
        let (wire, result) = fixture.forward_envelope(method, json!({"id":"mac"}), |id| {
            json!({"id":id,"error":{"code":"guest_unavailable","message":"Connection reset by peer"}})
        });
        assert_eq!(
            wire,
            json!({"id":number,"method":"guest_request","request":{"id":number.to_string(),"method":guest_method}}),
            "{method}: one request to the integration service and nothing else"
        );
        let text = result.unwrap_err().to_string();
        assert!(
            text.starts_with("backend: ") && text.contains("\"code\":\"guest_unavailable\""),
            "{method}: {text}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{method}: a worker refusal must not wait for the controller timeout"
        );
    }
    // A Guest that was reached and refused is a different, also typed, error.
    let (_, refused) = fixture.forward(
        "screenshot",
        json!({"id":"mac"}),
        json!({"id":"4","error":{"code":"screen_recording_permission_required"}}),
    );
    let text = refused.unwrap_err().to_string();
    assert!(
        text.starts_with("guest: ") && text.contains("screen_recording_permission_required"),
        "{text}"
    );
}
#[test]
fn mac_windows_observe_forwards_only_guest_method_and_validates_result() {
    let mut fixture = FilePipeFixture::macos();
    let (wire, result) = fixture.forward("windows", json!({"id":"mac"}), windows::tests::fixture());
    assert_eq!(
        wire,
        json!({"id":1,"method":"guest_request","request":{"id":"1","method":"windows"}})
    );
    assert_eq!(result.unwrap()["titles_included"], false);
    assert!(fixture.manager.file_sessions.is_empty());
    let (_, denied) = fixture.forward(
        "windows",
        json!({"id":"mac"}),
        json!({"id":"2","error":{"code":"screen_recording_permission_required"}}),
    );
    assert!(denied
        .unwrap_err()
        .to_string()
        .contains("screen_recording_permission_required"));
    let mut bad = windows::tests::fixture();
    bad["result"]["windows"][0]["title"] = json!("must not surface");
    let (_, bad) = fixture.forward("windows", json!({"id":"mac"}), bad);
    assert!(bad.is_err());
    for (backend, guest, args) in [
        (Backend::AppleVirtualization, "linux", json!({"id":"mac"})),
        (Backend::QemuHvf, "linux", json!({"id":"mac"})),
        (
            Backend::AppleVirtualization,
            "macos",
            json!({"id":"mac","include_titles":true}),
        ),
    ] {
        let config = fixture.manager.configs.get_mut("mac").unwrap();
        config.backend = backend;
        config.guest = guest.into();
        let mut pending = None;
        assert!(fixture
            .manager
            .dispatch(json!({"method":"windows","arguments":args}), &mut pending)
            .is_err());
        assert!(pending.is_none());
    }
    assert!(tools_with_input(false, false, false, false)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "vm_windows"));
}

#[test]
fn mac_files_forward_flat_owner_bound_wire_and_route_session_cleanup() {
    let mut fixture = FilePipeFixture::macos();
    let owner = "a".repeat(64);
    for (method, fields) in [
        (
            "file_upload_begin",
            json!({"path":"artifact.txt","size":3,"sha256":"b".repeat(64)}),
        ),
        (
            "file_upload_write",
            json!({"token":"c".repeat(32),"offset":0,"data":"YWJj"}),
        ),
        ("file_upload_commit", json!({"token":"c".repeat(32)})),
        ("file_download_begin", json!({"path":"artifact.txt"})),
        (
            "file_download_read",
            json!({"token":"d".repeat(32),"offset":0}),
        ),
        ("file_cancel", json!({"token":"d".repeat(32)})),
    ] {
        let mut args = fields;
        args["id"] = json!("mac");
        args["owner"] = json!(owner);
        let reply = json!({"id":"fixture","result":{"fixture_reply":true}});
        let (wire, result) = fixture.forward(method, args.clone(), reply.clone());
        assert_eq!(wire["method"], "guest_request");
        args["id"] = json!(wire["id"].as_u64().unwrap().to_string());
        args["method"] = json!(method);
        assert_eq!(wire["request"], args);
        assert_eq!(result.unwrap(), reply);
    }
    assert!(fixture.manager.file_sessions[&owner].1.contains("mac"));
    let mut completion = None;
    fixture
        .manager
        .dispatch(
            json!({"method":"session_end","arguments":{"owner":owner}}),
            &mut completion,
        )
        .unwrap();
    match completion.unwrap() {
        Completion::CancelJobs { file_sessions, .. } => {
            assert_eq!(file_sessions, vec![json!({"id":"mac","owner":owner})])
        }
        _ => panic!("missing file session cleanup"),
    }
    assert!(!fixture.manager.file_sessions.contains_key(&owner));
    let (wire, result) = fixture.forward(
        "file_session_end",
        json!({"id":"mac","owner":owner}),
        json!({"id":"cleanup","result":{"cancelled":0}}),
    );
    assert_eq!(wire["request"]["method"], "file_session_end");
    assert!(result.is_ok());
    assert!(
        fixture.manager.file_sessions.is_empty(),
        "cleanup must not resurrect routing"
    );
}
// Exercise dispatch, reply completion and status without a listening socket or a Guest.
fn complete_health_info(fixture: &mut FilePipeFixture, reply: Value) -> Result<Value> {
    let mut completion = None;
    fixture.manager.dispatch(
        json!({"method":"guest_info","arguments":{"id":"mac"}}),
        &mut completion,
    )?;
    let Some(Completion::Backend(pending)) = completion else {
        panic!("guest_info must use backend completion");
    };
    fixture.manager.running["mac"]
        .pending
        .lock()
        .unwrap()
        .remove(&pending.number)
        .unwrap()
        .send(reply)
        .unwrap();
    pending.wait()
}
fn reported_metal_info() -> Value {
    // The current Swift service's public-device schema, not workload evidence.
    json!({"id":"info","result":{
        "metal_device":"Apple Paravirtual device",
        "graphics":{"schema":1,"api":"metal","status":"reported",
            "device_selection":"system-default","evidence":"public-device-properties",
            "queried_at_unix_ms":5678,
            "device":{"name":"Apple Paravirtual device","registry_id":"18446744073709551615"}}
    }})
}
#[test]
fn guest_metal_device_present_has_no_alert() {
    let mut fixture = FilePipeFixture::macos();
    let info = reported_metal_info();
    assert_eq!(
        complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
        info
    );
    let runtime = fixture.manager.view("mac").unwrap()["runtime"].clone();
    assert!(runtime.get("guest_metal_device_alert").is_none());
    assert_eq!(
        runtime["guest_metal_device_observation"]["status"],
        "reported"
    );
    assert_eq!(
        runtime["guest_metal_device_observation"]["queried_at_unix_ms"],
        5678
    );
}
#[test]
fn guest_metal_device_lost_alert_is_in_status_and_list_without_changing_reply() {
    let mut fixture = FilePipeFixture::macos();
    let info = json!({"id":"info","result":{"metal_device":"none","graphics":{"status":"unavailable","reason":"no_metal_device","queried_at_unix_ms":1234}}});
    assert_eq!(
        complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
        info
    );
    let status = fixture.manager.view("mac").unwrap();
    let alert = &status["runtime"]["guest_metal_device_alert"];
    assert_eq!(alert["reason"], "no_metal_device");
    assert_eq!(alert["source"], "guest_info");
    assert_eq!(alert["queried_at_unix_ms"], 1234);
    assert!(alert["scope"].as_str().unwrap().contains("not polled"));
    assert_eq!(alert["operator_guidance"]["automatic_recovery"], false);
    assert!(alert["operator_guidance"]["action"]
        .as_str()
        .unwrap()
        .contains("normal Guest shutdown"));
    assert_eq!(
        status["runtime"]["guest_metal_device_observation"]["status"],
        "unavailable"
    );
    assert_eq!(status["state"], "running");
    let list = fixture
        .manager
        .dispatch(json!({"method":"list"}), &mut None)
        .unwrap();
    assert_eq!(list[0]["runtime"]["guest_metal_device_alert"], *alert);
}
#[test]
fn guest_metal_device_old_agent_has_no_alert_or_error() {
    let mut fixture = FilePipeFixture::macos();
    let info = json!({"id":"info","result":{"metal_device":"Apple Paravirtual device","capabilities":{"files":false}}});
    assert_eq!(
        complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
        info
    );
    let runtime = fixture.manager.view("mac").unwrap()["runtime"].clone();
    assert!(runtime.get("guest_metal_device_alert").is_none());
    assert_eq!(
        runtime["guest_metal_device_observation"]["status"],
        "unknown"
    );
    assert_eq!(
        runtime["guest_metal_device_observation"]["reason"],
        "graphics_not_reported"
    );
}
#[test]
fn guest_metal_device_alert_is_absent_for_other_backend_guest_pairs() {
    let mut fixture = FilePipeFixture::macos();
    let info = json!({"id":"info","result":{"graphics":{"status":"unavailable","reason":"no_metal_device"}}});
    for (backend, guest) in [
        (Backend::AppleVirtualization, "linux"),
        (Backend::QemuHvf, "linux"),
        (Backend::QemuHvf, "windows"),
        (Backend::QemuHvf, "macos"),
    ] {
        let config = fixture.manager.configs.get_mut("mac").unwrap();
        config.backend = backend;
        config.guest = guest.into();
        assert_eq!(
            complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
            info
        );
        let runtime = fixture.manager.view("mac").unwrap()["runtime"].clone();
        assert!(
            runtime.get("guest_metal_device_alert").is_none(),
            "{backend:?} {guest}"
        );
        assert!(
            runtime.get("guest_metal_device_observation").is_none(),
            "{backend:?} {guest}"
        );
    }
}
#[test]
fn guest_metal_device_alert_survives_state_events_and_failed_queries() {
    let mut fixture = FilePipeFixture::macos();
    let info = json!({"id":"info","result":{"graphics":{"status":"unavailable","reason":"no_metal_device"}}});
    complete_health_info(&mut fixture, json!({"result":info})).unwrap();
    let alert = fixture.manager.view("mac").unwrap()["runtime"]["guest_metal_device_alert"].clone();
    let observation =
        fixture.manager.view("mac").unwrap()["runtime"]["guest_metal_device_observation"].clone();
    replace_runtime_state(
        &mut fixture.manager.running["mac"].state.lock().unwrap(),
        json!({"state":"running","event":"state"}),
    );
    for reply in [
        json!({"error":{"code":"guest_unavailable"}}),
        json!({"result":{"id":"info","error":{"code":"unsupported"}}}),
    ] {
        assert!(complete_health_info(&mut fixture, reply).is_err());
        assert_eq!(
            fixture.manager.view("mac").unwrap()["runtime"]["guest_metal_device_alert"],
            alert
        );
        assert_eq!(
            fixture.manager.view("mac").unwrap()["runtime"]["guest_metal_device_observation"],
            observation
        );
    }
    let mut fresh = FilePipeFixture::macos();
    assert!(fresh.manager.view("mac").unwrap()["runtime"]
        .get("guest_metal_device_alert")
        .is_none());
}
#[test]
fn guest_metal_device_alert_clears_only_on_a_valid_device_report() {
    let mut fixture = FilePipeFixture::macos();
    complete_health_info(&mut fixture, json!({"result":{"result":{"graphics":{"status":"unavailable","reason":"no_metal_device","queried_at_unix_ms":1234}}}})).unwrap();
    let alert = fixture.manager.view("mac").unwrap()["runtime"]["guest_metal_device_alert"].clone();
    for info in [
        json!({"result":{}}),
        json!({"result":{"metal_device":"Apple Paravirtual device"}}),
        json!({"result":{"graphics":{"status":"reported"}}}),
        json!({"result":{"graphics":{"status":"unavailable","reason":"unsupported_query"}}}),
        json!({"result":{"graphics":false}}),
    ] {
        assert_eq!(
            complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
            info
        );
        let runtime = fixture.manager.view("mac").unwrap()["runtime"].clone();
        assert_eq!(
            runtime["guest_metal_device_alert"], alert,
            "unknown is not recovery: {info}"
        );
        assert_eq!(
            runtime["guest_metal_device_observation"]["status"],
            "unknown"
        );
    }
    let info = reported_metal_info();
    assert_eq!(
        complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
        info
    );
    let runtime = fixture.manager.view("mac").unwrap()["runtime"].clone();
    assert!(runtime.get("guest_metal_device_alert").is_none());
    assert_eq!(
        runtime["guest_metal_device_observation"]["status"],
        "reported"
    );
}
#[test]
fn guest_metal_device_invalid_identity_or_schema_cannot_clear_loss() {
    let mut fixture = FilePipeFixture::macos();
    complete_health_info(&mut fixture, json!({"result":{"result":{"graphics":{"status":"unavailable","reason":"no_metal_device"}}}})).unwrap();
    let alert = fixture.manager.view("mac").unwrap()["runtime"]["guest_metal_device_alert"].clone();
    for (pointer, value) in [
        ("/result/graphics/schema", json!(2)),
        ("/result/graphics/api", json!("opengl")),
        ("/result/graphics/device_selection", json!("other")),
        ("/result/graphics/evidence", json!("test-fixture")),
        ("/result/graphics/queried_at_unix_ms", json!(-1)),
        ("/result/graphics/device", Value::Null),
        ("/result/graphics/device/name", json!(" ")),
        (
            "/result/graphics/device/registry_id",
            json!("18446744073709551616"),
        ),
        ("/result/graphics/device/registry_id", json!("+1")),
        ("/result/graphics/device/registry_id", json!(1)),
        ("/result/metal_device", json!("different device")),
    ] {
        let mut info = reported_metal_info();
        *info.pointer_mut(pointer).unwrap() = value;
        assert_eq!(
            complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
            info
        );
        let runtime = fixture.manager.view("mac").unwrap()["runtime"].clone();
        assert_eq!(
            runtime["guest_metal_device_alert"], alert,
            "{pointer}: {info}"
        );
        assert_eq!(
            runtime["guest_metal_device_observation"]["status"],
            "unknown"
        );
    }
}
#[test]
fn guest_metal_device_retained_real_swift_wire_is_a_valid_presence_report() {
    // Replay the independently recorded actual Guest/MCP public properties.
    // This is wire compatibility evidence, not a fresh Guest recovery test.
    let evidence: Value = serde_json::from_str(include_str!(
        "../testdata/macos-graphics-stage8-evidence.json"
    ))
    .unwrap();
    assert_eq!(evidence["validation"]["actual_mcp_query"], true);
    let observation = &evidence["validation"]["observation"];
    assert_eq!(
        observation["graphics"]["assessment"]["performance"],
        "not-measured"
    );
    let info = json!({"id":"retained-wire","result":observation});
    let mut fixture = FilePipeFixture::macos();
    complete_health_info(&mut fixture, json!({"result":{"result":{"graphics":{"status":"unavailable","reason":"no_metal_device"}}}})).unwrap();
    assert_eq!(
        complete_health_info(&mut fixture, json!({"result":info})).unwrap(),
        info
    );
    let runtime = fixture.manager.view("mac").unwrap()["runtime"].clone();
    assert!(runtime.get("guest_metal_device_alert").is_none());
    assert_eq!(
        runtime["guest_metal_device_observation"]["status"],
        "reported"
    );
    assert_eq!(
        runtime["guest_metal_device_observation"]["queried_at_unix_ms"],
        observation["graphics"]["queried_at_unix_ms"]
    );
}
#[test]
fn mac_file_availability_is_guest_authority_and_denial_is_not_success() {
    let mut fixture = FilePipeFixture::macos();
    let info = json!({"id":"info","result":{"capabilities":{"files":false}}});
    let (_, result) = fixture.forward("guest_info", json!({"id":"mac"}), info.clone());
    assert_eq!(result.unwrap(), info);
    for code in [
        "unsupported",
        "files_disabled",
        "files_worker_unavailable",
        "session_retired",
        "owner_mismatch",
    ] {
        let owner = "a".repeat(64);
        let (wire, result) = fixture.forward(
            "file_download_begin",
            json!({"id":"mac","owner":owner,"path":"artifact.txt"}),
            json!({"id":"request","error":{"code":code,"message":"denied by Guest"}}),
        );
        assert_eq!(wire["request"]["owner"], owner);
        assert!(result.unwrap_err().to_string().contains(code));
    }
    // Failed requests still need EOF cleanup: a lost/denied reply is not
    // proof that the Guest created no temporary transfer state.
    assert!(fixture.manager.file_sessions.contains_key(&"a".repeat(64)));
}
#[test]
fn mac_file_owner_size_and_backend_guards_fail_before_worker_write() {
    let mut fixture = FilePipeFixture::macos();
    for owner in [
        Value::Null,
        json!("A".repeat(64)),
        json!("a".repeat(63)),
        json!("g".repeat(64)),
    ] {
        let mut completion = None;
        assert!(fixture.manager.dispatch(json!({"method":"file_upload_begin","arguments":{"id":"mac","owner":owner,"path":"x","size":0,"sha256":"b".repeat(64)}}),&mut completion).is_err());
        assert!(completion.is_none());
    }
    assert!(fixture.manager.file_sessions.is_empty());
    let mut completion = None;
    let error=fixture.manager.dispatch(json!({"method":"file_upload_write","arguments":{"id":"mac","owner":"a".repeat(64),"token":"b".repeat(32),"offset":0,"data":"A".repeat(65536)}}),&mut completion).unwrap_err();
    assert!(error.to_string().contains("64KiB"));
    assert!(completion.is_none());
    fixture.manager.configs.get_mut("mac").unwrap().backend = Backend::QemuHvf;
    let error=fixture.manager.dispatch(json!({"method":"file_download_begin","arguments":{"id":"mac","owner":"a".repeat(64),"path":"x"}}),&mut completion).unwrap_err();
    assert!(error.to_string().contains("QEMU/HVF"));
    assert_eq!(
        fixture.manager.running["mac"].next, 2,
        "only bounded rejected payload allocates a request number"
    );
    assert!(fixture.manager.running["mac"]
        .pending
        .lock()
        .unwrap()
        .is_empty());
    let mut descriptor = libc::pollfd {
        fd: fixture.output.get_ref().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe { libc::poll(&mut descriptor, 1, 40) },
        0,
        "rejected operation reached worker"
    );
}
#[test]
fn file_tools_keep_independent_permission_and_state_macos_boundary() {
    assert!(!tools_with_input(true, true, false, true)
        .to_string()
        .contains("vm_file_"));
    let tools = tools_with_files(false, false, true);
    let begin = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "vm_file_upload_begin")
        .unwrap();
    let description = begin["description"].as_str().unwrap();
    assert!(
        description.contains("Linux")
            && description.contains("macOS")
            && description.contains("same-user")
    );
    assert!(begin["inputSchema"]["properties"].get("owner").is_none());
    assert_eq!(
        begin["inputSchema"]["properties"]["size"]["maximum"],
        8388608
    );
    assert!(
        !tools.to_string().contains("vm_exec") && !tools.to_string().contains("vm_acquire_control")
    );
}
