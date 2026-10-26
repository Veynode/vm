// Real Manager dispatch and owned child pipes, with injected Guest outcomes.
// These CPU tests do not execute a Guest command or attest Guest enforcement.
fn timeout_fixture() -> (FilePipeFixture, String) {
    let mut fixture = FilePipeFixture::macos();
    let config = fixture.manager.configs.get_mut("mac").unwrap();
    config.backend = Backend::QemuHvf;
    config.guest = "linux".into();
    config.qemu_guest_exec = true;
    fixture.manager.running["mac"].state.lock().unwrap()["guest_integration_state"] =
        json!("ready");
    (fixture, "a".repeat(64))
}

fn timeout_advertisement(maximum: Value) -> Value {
    json!({"os":"linux","transport":"virtio-serial-private-exec-v2","transport_reconnect":false,
        "execution_containment":"cgroup-v2-clone3-kill","execution_identity":{"rootless":true},
        "capabilities":{"exec":true,"async_exec":true,"descendant_containment":true,
            "files":false,"shutdown":false,"desktop":false},"max_async_timeout_ms":maximum})
}

fn observe_timeout_policy(fixture: &mut FilePipeFixture, info: Value) {
    let (wire, result) = fixture.forward(
        "guest_info",
        json!({"id":"mac"}),
        json!({"id":"info","result":info}),
    );
    assert_eq!(wire["request"]["method"], "info");
    result.unwrap();
}

fn reject_timeout_without_dispatch(fixture: &mut FilePipeFixture, owner: &str, timeout: u64) {
    let next = fixture.manager.running["mac"].next;
    let mut completion = None;
    let error = fixture
        .manager
        .dispatch(
            json!({"method":"exec_start","arguments":{
        "id":"mac","owner":owner,"argv":["/bin/true"],"timeout_ms":timeout}}),
            &mut completion,
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("observed asynchronous limit"),
        "{error}"
    );
    assert!(completion.is_none());
    assert_eq!(fixture.manager.running["mac"].next, next);
    assert!(fixture.manager.running["mac"]
        .pending
        .lock()
        .unwrap()
        .is_empty());
    assert_eq!(
        fixture.output.read(&mut [0u8; 1]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(fixture
        .manager
        .running
        .get_mut("mac")
        .unwrap()
        .poll_exit()
        .unwrap()
        .is_none());
}

fn pending_timeout_info(fixture: &mut FilePipeFixture) -> Pending {
    let mut completion = None;
    fixture
        .manager
        .dispatch(
            json!({"method":"guest_info","arguments":{"id":"mac"}}),
            &mut completion,
        )
        .unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(1);
    let mut bytes = Vec::new();
    while bytes.last() != Some(&b'\n') {
        assert!(std::time::Instant::now() < until && bytes.len() <= MAX_MESSAGE);
        let mut byte = [0];
        match fixture.output.read(&mut byte) {
            Ok(1) => bytes.push(byte[0]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1))
            }
            other => panic!("info pipe ended unexpectedly: {other:?}"),
        }
    }
    let wire: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(wire["request"]["method"], "info");
    let Some(Completion::Backend(pending)) = completion else {
        panic!("info completion missing")
    };
    assert_eq!(wire["id"], pending.number);
    pending
}

fn complete_timeout_info(pending: Pending, info: Value) {
    let number = pending.number;
    pending
        .pending
        .lock()
        .unwrap()
        .remove(&number)
        .unwrap()
        .send(json!({"id":number,"result":{"id":"info","result":info}}))
        .unwrap();
    pending.wait().unwrap();
}

#[test]
fn async_exec_timeout_schema_preserves_sync_and_permission_boundaries() {
    let tools = tools_with_input(false, true, false, false);
    let find = |name| {
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap()
    };
    assert_eq!(
        find("vm_exec")["inputSchema"]["properties"]["timeout_ms"]["maximum"],
        30000
    );
    assert_eq!(
        find("vm_exec_start")["inputSchema"]["properties"]["timeout_ms"]["maximum"],
        120000
    );
    assert!(find("vm_exec_start")["description"]
        .as_str()
        .unwrap()
        .contains("rejected before Guest dispatch"));
    assert!(!tools_with_input(false, false, false, false)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "vm_exec_start"));
    assert_eq!(
        exec_timeout_ms(&json!({}), SYNC_EXEC_MAX_TIMEOUT_MS).unwrap(),
        5000
    );
    assert_eq!(
        exec_timeout_ms(&json!({}), ASYNC_EXEC_MAX_TIMEOUT_MS).unwrap(),
        5000
    );
}

#[test]
fn async_exec_timeout_direct_rpc_forwards_90_seconds_and_exact_maximum() {
    let (mut fixture, owner) = timeout_fixture();
    observe_timeout_policy(&mut fixture, timeout_advertisement(json!(120000)));
    for (index, timeout) in [90000, 120000].into_iter().enumerate() {
        let job = format!("{:032x}", index + 1);
        let (wire, reply) = fixture.forward(
            "exec_start",
            json!({"id":"mac","owner":owner,"argv":["/bin/true"],"timeout_ms":timeout}),
            json!({"id":"start","result":{"job_id":job,"state":"running"}}),
        );
        assert_eq!(wire["request"]["timeout_ms"], timeout);
        assert_eq!(wire["request"]["method"], "exec_start");
        assert_eq!(reply.unwrap()["result"]["job_id"], job);
        fixture
            .manager
            .jobs
            .lock()
            .unwrap()
            .known("mac", &job, std::time::Instant::now())
            .unwrap();
    }
}

#[test]
fn async_exec_timeout_direct_rpc_rejects_out_of_bounds_before_worker_write() {
    let (mut fixture, owner) = timeout_fixture();
    for timeout in [
        json!(0),
        json!(120001),
        json!(-1),
        json!(1.5),
        json!("90000"),
    ] {
        let mut deferred = None;
        let error = fixture
            .manager
            .dispatch(
                json!({"method":"exec_start","arguments":{
            "id":"mac","owner":owner,"argv":["/bin/true"],"timeout_ms":timeout}}),
                &mut deferred,
            )
            .unwrap_err();
        assert!(error.to_string().contains("timeout_ms"), "{error}");
        assert!(deferred.is_none());
    }
    assert!(fixture.manager.running["mac"]
        .pending
        .lock()
        .unwrap()
        .is_empty());
    let error = fixture.output.read(&mut [0u8; 1]).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert!(fixture
        .manager
        .jobs
        .lock()
        .unwrap()
        .owned_running(&owner, None, std::time::Instant::now())
        .is_empty());
}

#[test]
fn async_exec_timeout_sync_dispatch_and_wrapper_keep_30_seconds() {
    let (mut fixture, owner) = timeout_fixture();
    for timeout in [30001, 90000, 120000] {
        let arguments = json!({"id":"mac","owner":owner,"argv":["/bin/true"],"timeout_ms":timeout});
        let mut deferred = None;
        let error = fixture
            .manager
            .dispatch(
                json!({"method":"exec","arguments":arguments}),
                &mut deferred,
            )
            .unwrap_err();
        assert!(error.to_string().contains("1..30000"));
        assert!(deferred.is_none());
        let (peer, _other) = UnixStream::pair().unwrap();
        let error = synchronous_exec(Path::new("/unused"), arguments, &peer).unwrap_err();
        assert!(error.to_string().contains("1..30000"));
    }
    let mut deferred = None;
    fixture
        .manager
        .dispatch(
            json!({"method":"exec","arguments":{
        "id":"mac","owner":owner,"argv":["/bin/true"],"timeout_ms":30000}}),
            &mut deferred,
        )
        .unwrap();
    assert!(matches!(deferred, Some(Completion::Synchronous { .. })));
    let error = fixture.output.read(&mut [0u8; 1]).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn async_exec_timeout_older_serial_guest_never_receives_long_start_and_peer_survives() {
    let (mut fixture, owner) = timeout_fixture();
    let mut old_info = timeout_advertisement(json!(30000));
    old_info
        .as_object_mut()
        .unwrap()
        .remove("max_async_timeout_ms");
    observe_timeout_policy(&mut fixture, old_info);
    let peer = "d".repeat(32);
    fixture
        .forward(
            "exec_start",
            json!({"id":"mac","owner":owner,
        "argv":["/bin/true"],"timeout_ms":30000}),
            json!({"id":"peer","result":{"job_id":peer,"state":"running"}}),
        )
        .1
        .unwrap();
    for timeout in [30001, 90000, 120000] {
        reject_timeout_without_dispatch(&mut fixture, &owner, timeout);
    }
    fixture
        .manager
        .jobs
        .lock()
        .unwrap()
        .known("mac", &peer, std::time::Instant::now())
        .unwrap();
    // The same owned pipe still carries peer polling after the rejection. The
    // old serial service cannot reject/fence a start which was never emitted.
    let (wire, result) = fixture.forward(
        "exec_poll",
        json!({"id":"mac","owner":owner,"job_id":peer}),
        json!({"id":"poll","result":{"job_id":peer,"state":"running"}}),
    );
    assert_eq!(wire["request"]["method"], "exec_poll");
    assert_eq!(result.unwrap()["result"]["state"], "running");
}

#[test]
fn async_exec_timeout_long_job_keeps_poll_and_cancel() {
    let (mut fixture, owner) = timeout_fixture();
    observe_timeout_policy(&mut fixture, timeout_advertisement(json!(120000)));
    let job = "b".repeat(32);
    fixture
        .forward(
            "exec_start",
            json!({"id":"mac","owner":owner,"argv":["/bin/true"],"timeout_ms":90000}),
            json!({"id":"start","result":{"job_id":job,"state":"running"}}),
        )
        .1
        .unwrap();
    let args = json!({"id":"mac","owner":owner,"job_id":job});
    let (poll, result) = fixture.forward(
        "exec_poll",
        args.clone(),
        json!({"id":"poll","result":{"job_id":job,"state":"running"}}),
    );
    assert_eq!(poll["request"]["job_id"], job);
    assert_eq!(result.unwrap()["result"]["state"], "running");
    let (cancel, result) = fixture.forward(
        "exec_cancel",
        args,
        json!({"id":"cancel","result":{"job_id":job,"cancel_requested":true}}),
    );
    assert_eq!(cancel["request"]["job_id"], job);
    assert_eq!(result.unwrap()["result"]["cancel_requested"], true);
    assert_eq!(
        fixture
            .manager
            .jobs
            .lock()
            .unwrap()
            .owned_running(&owner, None, std::time::Instant::now())
            .len(),
        1
    ); // Cancel ACK is not terminal proof.
}

#[test]
fn async_exec_timeout_unknown_malformed_and_reconnecting_policies_stay_short() {
    let (mut fixture, owner) = timeout_fixture();
    reject_timeout_without_dispatch(&mut fixture, &owner, 90000);
    for maximum in [
        Value::Null,
        json!(true),
        json!("120000"),
        json!(120000.5),
        json!(0),
    ] {
        observe_timeout_policy(&mut fixture, timeout_advertisement(maximum));
        reject_timeout_without_dispatch(&mut fixture, &owner, 90000);
    }
    let mut reconnecting = timeout_advertisement(json!(120000));
    reconnecting["transport_reconnect"] = json!(true);
    observe_timeout_policy(&mut fixture, reconnecting);
    reject_timeout_without_dispatch(&mut fixture, &owner, 90000);
    observe_timeout_policy(&mut fixture, timeout_advertisement(json!(60000)));
    reject_timeout_without_dispatch(&mut fixture, &owner, 90000);
    let config = fixture.manager.configs.get_mut("mac").unwrap();
    config.backend = Backend::AppleVirtualization;
    config.qemu_guest_exec = false;
    let mut vsock = timeout_advertisement(json!(120000));
    vsock["transport"] = json!("vsock-host-only");
    observe_timeout_policy(&mut fixture, vsock);
    reject_timeout_without_dispatch(&mut fixture, &owner, 90000);
}

#[test]
fn async_exec_timeout_info_failure_clears_previous_cap() {
    let (mut fixture, owner) = timeout_fixture();
    observe_timeout_policy(&mut fixture, timeout_advertisement(json!(120000)));
    let (_, reply) = fixture.forward_envelope(
        "guest_info",
        json!({"id":"mac"}),
        |number| json!({"id":number,"error":"Guest unavailable"}),
    );
    assert!(reply.is_err());
    reject_timeout_without_dispatch(&mut fixture, &owner, 90000);
}

#[test]
fn async_exec_timeout_ordinary_state_preserves_cap_terminal_state_rejects_late_info() {
    let (mut fixture, owner) = timeout_fixture();
    observe_timeout_policy(&mut fixture, timeout_advertisement(json!(120000)));
    let policy = fixture.manager.running["mac"].guest_async_timeout.clone();
    for state in ["running", "paused"] {
        policy.lock().unwrap().observe_worker_state(&json!(state));
        assert_eq!(policy.lock().unwrap().maximum, 120000);
    }
    let (_, reply) = fixture.forward(
        "exec_start",
        json!({"id":"mac","owner":owner,
        "argv":["/bin/true"],"timeout_ms":120000}),
        json!({"id":"start","result":{"job_id":"e".repeat(32),"state":"running"}}),
    );
    reply.unwrap();
    let pending = pending_timeout_info(&mut fixture);
    policy.lock().unwrap().observe_worker_state(&json!("error"));
    complete_timeout_info(pending, timeout_advertisement(json!(120000)));
    assert_eq!(policy.lock().unwrap().maximum, 30000);
    reject_timeout_without_dispatch(&mut fixture, &owner, 90000);
}

#[test]
fn async_exec_timeout_late_info_cannot_elevate_replacement_worker_same_vm_id() {
    let (mut fixture, owner) = timeout_fixture();
    let pending = pending_timeout_info(&mut fixture);
    let old_policy = fixture.manager.running["mac"].guest_async_timeout.clone();
    let (mut fresh, _fresh_owner) = timeout_fixture();
    let replacement = fresh.manager.running.remove("mac").unwrap();
    let original = fixture.manager.running.remove("mac").unwrap();
    fixture.manager.started("mac".into(), replacement);
    fresh.manager.started("mac".into(), original);
    std::mem::swap(&mut fixture.output, &mut fresh.output);
    let new_policy = fixture.manager.running["mac"].guest_async_timeout.clone();
    assert!(!Arc::ptr_eq(&old_policy, &new_policy));
    complete_timeout_info(pending, timeout_advertisement(json!(120000)));
    assert_eq!(old_policy.lock().unwrap().maximum, 120000);
    assert_eq!(new_policy.lock().unwrap().maximum, 30000);
    reject_timeout_without_dispatch(&mut fixture, &owner, 90000);
}
