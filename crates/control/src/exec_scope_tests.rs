fn fixture_launch(f: &mut FilePipeFixture) -> exec_witness::Witness {
    let worker = f.manager.running["mac"]
        .bridge
        .lock()
        .unwrap()
        .worker_instance
        .clone();
    let w = exec_witness::Witness::create(
        &f.manager.dir.join("exec-journal").canonicalize().unwrap(),
        "mac",
        &worker,
    )
    .unwrap();
    f.manager
        .exec_retirements
        .lock()
        .unwrap()
        .launch(w.identity.clone())
        .unwrap();
    // The worker reports its serial execution transport as connected.
    f.manager.running.get_mut("mac").unwrap().state.lock().unwrap()["guest_integration_state"] =
        json!("ready");
    w
}
#[test]
fn unready_serial_transport_refuses_start_without_recording_scope() {
    let mut f = FilePipeFixture::macos();
    let _witness = fixture_launch(&mut f);
    {
        let config = f.manager.configs.get_mut("mac").unwrap();
        config.backend = Backend::QemuHvf;
        config.guest = "linux".into();
        config.qemu_guest_exec = true;
    }
    f.manager.running.get_mut("mac").unwrap().state.lock().unwrap()["guest_integration_state"] =
        json!("connecting");
    let owner = "a".repeat(64);
    let epoch = f
        .manager
        .leases
        .lock()
        .unwrap()
        .acquire("mac", &owner, std::time::Instant::now())
        .unwrap();
    let mut pending = None;
    let error = f.manager.dispatch(json!({"method":"exec_start","arguments":{"id":"mac","owner":owner,"epoch":epoch,"argv":["/bin/true"],"timeout_ms":1000}}),&mut pending).unwrap_err();
    assert!(error.to_string().contains("not ready"));
    assert!(pending.is_none());
    // No scope means release has nothing to retire, so the Guest is never terminated.
    assert!(f
        .manager
        .exec_retirements
        .lock()
        .unwrap()
        .records(None, Some("mac"))
        .is_empty());
}
#[test]
fn serial_start_registers_original_scope_before_any_job_ack_and_fences_reacquire() {
    let mut f = FilePipeFixture::macos();
    let _witness = fixture_launch(&mut f);
    {
        let config = f.manager.configs.get_mut("mac").unwrap();
        config.backend = Backend::QemuHvf;
        config.guest = "linux".into();
        config.qemu_guest_exec = true;
    }
    let owner = "a".repeat(64);
    let now = std::time::Instant::now();
    let epoch = f
        .manager
        .leases
        .lock()
        .unwrap()
        .acquire("mac", &owner, now)
        .unwrap();
    let mut pending = None;
    f.manager.dispatch(json!({"method":"exec_start","arguments":{"id":"mac","owner":owner,"epoch":epoch,"argv":["/bin/true"],"timeout_ms":1000}}),&mut pending).unwrap();
    let scope = f
        .manager
        .exec_retirements
        .lock()
        .unwrap()
        .records(Some(&owner), Some("mac"));
    assert_eq!(scope.len(), 1);
    assert_eq!(
        scope[0].worker_instance,
        f.manager.running["mac"]
            .bridge
            .lock()
            .unwrap()
            .worker_instance
    );
    assert_eq!(scope[0].epoch, epoch);
    // Drop the response waiter before any Guest ACK/job_id. This MUST NOT drop the scope.
    drop(pending);
    assert!(f
        .manager
        .jobs
        .lock()
        .unwrap()
        .owned_running(&owner, None, now)
        .is_empty());
    let mut closed = None;
    f.manager
        .dispatch(
            json!({"method":"session_end","arguments":{"owner":owner}}),
            &mut closed,
        )
        .unwrap();
    match closed.unwrap() {
        Completion::CancelJobs {
            exec_revocations, ..
        } => assert_eq!(exec_revocations, scope),
        _ => panic!("wrong cleanup"),
    }
    let mut next = None;
    let e = f
        .manager
        .dispatch(
            json!({"method":"acquire_control","arguments":{"id":"mac","owner":"b".repeat(64)}}),
            &mut next,
        )
        .unwrap_err();
    assert!(e.to_string().contains("cleanup unconfirmed"));
    let mut again = None;
    f.manager
        .dispatch(
            json!({"method":"session_end","arguments":{"owner":owner}}),
            &mut again,
        )
        .unwrap();
    match again.unwrap() {
        Completion::CancelJobs {
            exec_revocations, ..
        } => assert_eq!(exec_revocations, scope),
        _ => panic!("wrong cleanup"),
    }
}
#[test]
fn serial_scope_cleanup_failure_is_retained_without_erasing_other_incarnation() {
    let dir = tempfile::Builder::new()
        .prefix("scope-")
        .tempdir_in("/tmp")
        .unwrap();
    let listener = UnixListener::bind(dir.path().join("control.sock")).unwrap();
    let ledger = Arc::new(Mutex::new(exec_scope::Ledger::default()));
    let scope = exec_scope::Scope {
        vm: "vm".into(),
        worker_instance: "a".repeat(64),
        owner: "b".repeat(64),
        epoch: 1,
    };
    let mut other = scope.clone();
    other.worker_instance = "c".repeat(64);
    ledger.lock().unwrap().record(scope.clone()).unwrap();
    ledger.lock().unwrap().record(other.clone()).unwrap();
    let s = scope.clone();
    let server = thread::spawn(move || {
        for method in ["exec_scope_revoke", "exec_scope_abort"] {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let request: Value = serde_json::from_slice(
                &read_frame(&mut BufReader::new(socket.try_clone().unwrap()))
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(request["method"], method);
            assert_eq!(request["arguments"]["worker_instance"], s.worker_instance);
            write_json(&mut socket, &json!({"error":"cleanup unconfirmed"})).unwrap();
        }
    });
    assert!(retire_exec_scopes(dir.path(), vec![scope.clone()], &ledger).is_err());
    server.join().unwrap();
    assert_eq!(ledger.lock().unwrap().records(None, None).len(), 2);
    ledger.lock().unwrap().complete(&scope).unwrap();
    assert_eq!(ledger.lock().unwrap().records(None, None), vec![other]);
}
#[test]
fn failed_serial_scope_still_attempts_other_vm_file_cleanup_and_keeps_first_failure() {
    let dir = tempfile::Builder::new()
        .prefix("mixed-scope-")
        .tempdir_in("/tmp")
        .unwrap();
    let listener = UnixListener::bind(dir.path().join("control.sock")).unwrap();
    let ledger = Arc::new(Mutex::new(exec_scope::Ledger::default()));
    let scope = exec_scope::Scope {
        vm: "linux".into(),
        worker_instance: "a".repeat(64),
        owner: "b".repeat(64),
        epoch: 1,
    };
    ledger.lock().unwrap().record(scope.clone()).unwrap();
    let server = thread::spawn(move || {
        for method in ["exec_scope_revoke", "exec_scope_abort", "file_session_end"] {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let request: Value = serde_json::from_slice(
                &read_frame(&mut BufReader::new(socket.try_clone().unwrap()))
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(request["method"], method);
            let result = if method == "file_session_end" {
                assert_eq!(request["arguments"]["id"], "mac");
                json!({"result":{"result":{"cancelled_transfers":1}}})
            } else {
                json!({"error":"original serial failure"})
            };
            write_json(&mut socket, &result).unwrap();
        }
    });
    let completion = Completion::CancelJobs {
        dir: dir.path().into(),
        response: json!({"closed":true,"retired_owner":true}),
        jobs: vec![],
        file_sessions: vec![json!({"id":"mac","owner":"b".repeat(64)})],
        input_revocations: vec![],
        input_retirements: Arc::new(Mutex::new(input_retirement::Retirements::default())),
        exec_retirements: ledger.clone(),
        exec_revocations: vec![scope],
    };
    let (peer, _) = UnixStream::pair().unwrap();
    let error = completion.wait(&peer).unwrap_err();
    assert!(format!("{error:#}").contains("original serial failure"));
    server.join().unwrap();
    assert_eq!(ledger.lock().unwrap().records(None, None).len(), 1);
}
#[test]
fn serial_same_owner_renewal_cannot_skip_expired_epoch_cleanup() {
    for expired in [false, true] {
        let mut f = FilePipeFixture::macos();
        let _witness = fixture_launch(&mut f);
        let owner = "a".repeat(64);
        let now = std::time::Instant::now();
        let admitted = if expired {
            now - Duration::from_secs(61)
        } else {
            now
        };
        let epoch = f
            .manager
            .leases
            .lock()
            .unwrap()
            .acquire("mac", &owner, admitted)
            .unwrap();
        let scope = exec_scope::Scope {
            vm: "mac".into(),
            worker_instance: f.manager.running["mac"]
                .bridge
                .lock()
                .unwrap()
                .worker_instance
                .clone(),
            owner: owner.clone(),
            epoch,
        };
        f.manager
            .exec_retirements
            .lock()
            .unwrap()
            .record(scope)
            .unwrap();
        let mut deferred = None;
        let result = f.manager.dispatch(
            json!({"method":"acquire_control","arguments":{"id":"mac","owner":owner}}),
            &mut deferred,
        );
        if expired {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("previous execution epoch expired"));
        } else {
            assert_eq!(result.unwrap()["epoch"], epoch)
        }
    }
}

#[test]
fn serial_shutdown_requires_all_scopes_empty_then_fences_authority_before_worker_ack() {
    let mut f = FilePipeFixture::macos();
    let _witness = fixture_launch(&mut f);
    let config = f.manager.configs.get_mut("mac").unwrap();
    config.backend = Backend::QemuHvf;
    config.guest = "linux".into();
    config.qemu_guest_exec = true;
    let owner = "a".repeat(64);
    let epoch = f
        .manager
        .leases
        .lock()
        .unwrap()
        .acquire("mac", &owner, std::time::Instant::now())
        .unwrap();
    let scope = exec_scope::Scope {
        vm: "mac".into(),
        worker_instance: f.manager.running["mac"]
            .bridge
            .lock()
            .unwrap()
            .worker_instance
            .clone(),
        owner: owner.clone(),
        epoch,
    };
    f.manager
        .exec_retirements
        .lock()
        .unwrap()
        .record(scope.clone())
        .unwrap();
    let mut pending = None;
    assert!(f
        .manager
        .dispatch(
            json!({"method":"shutdown","arguments":{"id":"mac"}}),
            &mut pending
        )
        .unwrap_err()
        .to_string()
        .contains("cleanup unconfirmed"));
    assert!(!f.manager.running["mac"].execution_quiescing);
    assert_eq!(f.manager.running["mac"].next, 1); // No power-button bytes or pending RPC.
    f.manager
        .exec_retirements
        .lock()
        .unwrap()
        .complete(&scope)
        .unwrap();
    let (wire, result) = f.forward("shutdown", json!({"id":"mac"}), json!({"requested":true}));
    assert_eq!(wire["execution_scopes_empty"], true);
    result.unwrap();
    for (method, args) in [
        ("acquire_control", json!({"id":"mac","owner":owner})),
        (
            "exec_start",
            json!({"id":"mac","owner":owner,"epoch":epoch,"argv":["/bin/true"]}),
        ),
        (
            "input_step",
            json!({"id":"mac","owner":owner,"epoch":epoch}),
        ),
    ] {
        assert!(f
            .manager
            .dispatch(json!({"method":method,"arguments":args}), &mut None)
            .unwrap_err()
            .to_string()
            .contains("quiescing"));
    }
    assert!(f
        .manager
        .leases
        .lock()
        .unwrap()
        .check("mac", &owner, epoch, std::time::Instant::now())
        .is_err());
}

#[test]
fn shutdown_fence_does_not_create_human_hold_for_restarted_worker() {
    let mut leases = lease::Leases::default();
    let owner = "a".repeat(64);
    let now = std::time::Instant::now();
    let old = leases.acquire("vm", &owner, now).unwrap();
    leases.fence_for_shutdown("vm", now).unwrap();
    assert!(leases.check("vm", &owner, old, now).is_err());
    assert!(leases.acquire("vm", &owner, now).unwrap() > old);
}

fn durable_qemu_fixture() -> (FilePipeFixture, String, u64, exec_witness::Witness) {
    let mut f = FilePipeFixture::macos();
    f.manager.dir = f.manager.dir.canonicalize().unwrap();
    let _witness = fixture_launch(&mut f);
    let config = f.manager.configs.get_mut("mac").unwrap();
    config.backend = Backend::QemuHvf;
    config.guest = "linux".into();
    config.qemu_guest_exec = true;
    atomic_json(&f.manager.dir.join("registry.json"), &f.manager.configs).unwrap();
    let owner = "d".repeat(64);
    let epoch = f
        .manager
        .leases
        .lock()
        .unwrap()
        .acquire("mac", &owner, std::time::Instant::now())
        .unwrap();
    (f, owner, epoch, _witness)
}
#[test]
fn journal_sync_failure_emits_no_worker_bytes_and_fences_future_execution() {
    let (mut f, owner, epoch, _witness) = durable_qemu_fixture();
    let journal_dir = f.manager.dir.join("exec-journal");
    fs::set_permissions(&journal_dir, fs::Permissions::from_mode(0o500)).unwrap();
    let mut pending = None;
    let request = json!({"method":"exec_start","arguments":{"id":"mac","owner":owner,"epoch":epoch,"argv":["/bin/true"],"timeout_ms":1000}});
    let outcome = f.manager.dispatch(request, &mut pending);
    fs::set_permissions(&journal_dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(outcome
        .unwrap_err()
        .to_string()
        .contains("durability unconfirmed"));
    let mut bytes = [0; 1];
    assert_eq!(
        f.output.read(&mut bytes).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(f.manager.running["mac"].pending.lock().unwrap().is_empty());
    assert!(f
        .manager
        .dispatch(
            json!({"method":"acquire_control","arguments":{"id":"mac","owner":owner}}),
            &mut None
        )
        .unwrap_err()
        .to_string()
        .contains("recovery required"));
}
#[test]
fn durable_lost_ack_survives_manager_reload_and_old_owner_eof_preserves_other_cleanup() {
    let (mut f, owner, epoch, _witness) = durable_qemu_fixture();
    let mut pending = None;
    f.manager.dispatch(json!({"method":"exec_start","arguments":{"id":"mac","owner":owner,"epoch":epoch,"argv":["/bin/true"],"timeout_ms":1000}}),&mut pending).unwrap();
    // Bytes went to a real child pipe, but no job ACK is delivered before restart.
    let mut poll = libc::pollfd {
        fd: f.output.get_ref().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut poll, 1, 1000) }, 1);
    drop(pending);
    let mut restarted = Manager::load(
        f.manager.dir.clone(),
        "/unused".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    assert_eq!(restarted.view("mac").unwrap()["state"], "recovery_required");
    for method in ["start", "acquire_control", "release_control", "exec_start"] {
        assert!(restarted
            .dispatch(
                json!({"method":method,"arguments":{"id":"mac","owner":owner,"epoch":epoch}}),
                &mut None
            )
            .unwrap_err()
            .to_string()
            .contains("recovery required"));
    }
    // Recovered failure must not bypass independent file cleanup for the same owner.
    restarted.file_sessions.insert(
        owner.clone(),
        (std::time::Instant::now(), BTreeSet::from(["other".into()])),
    );
    let listener = UnixListener::bind(f.manager.dir.join("control.sock")).unwrap();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let request: Value = serde_json::from_slice(
            &read_frame(&mut BufReader::new(socket.try_clone().unwrap()))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(request["method"], "file_session_end");
        assert_eq!(request["arguments"]["id"], "other");
        write_json(
            &mut socket,
            &json!({"result":{"result":{"cancelled_transfers":1}}}),
        )
        .unwrap();
    });
    let mut completion = None;
    restarted
        .dispatch(
            json!({"method":"session_end","arguments":{"owner":owner}}),
            &mut completion,
        )
        .unwrap();
    let (peer, _) = UnixStream::pair().unwrap();
    assert!(completion
        .unwrap()
        .wait(&peer)
        .unwrap_err()
        .to_string()
        .contains("durable prior-daemon scope"));
    server.join().unwrap();
    let mut again = None;
    restarted
        .dispatch(
            json!({"method":"session_end","arguments":{"owner":owner}}),
            &mut again,
        )
        .unwrap();
    assert!(again.unwrap().wait(&peer).is_err());
}
#[test]
fn exact_scope_ack_requires_durable_removal_before_cleanup_success() {
    let dir = tempfile::Builder::new()
        .prefix("journal-ack-")
        .tempdir_in("/tmp")
        .unwrap();
    let ledger = Arc::new(Mutex::new(
        exec_scope::Ledger::open(dir.path(), false).unwrap(),
    ));
    let scope = exec_scope::Scope {
        vm: "vm".into(),
        worker_instance: "a".repeat(64),
        owner: "b".repeat(64),
        epoch: 1,
    };
    let _witness = exec_witness::Witness::create(
        &dir.path().join("exec-journal").canonicalize().unwrap(),
        &scope.vm,
        &scope.worker_instance,
    )
    .unwrap();
    ledger
        .lock()
        .unwrap()
        .launch(_witness.identity.clone())
        .unwrap();
    ledger.lock().unwrap().record(scope.clone()).unwrap();
    ledger.lock().unwrap().fail_journal_sync();
    let listener = UnixListener::bind(dir.path().join("control.sock")).unwrap();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let request: Value = serde_json::from_slice(
            &read_frame(&mut BufReader::new(socket.try_clone().unwrap()))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(request["method"], "exec_scope_revoke");
        write_json(
            &mut socket,
            &json!({"result":{"result":{"revoked":true,"pending_starts":0,"cgroup_empty":true}}}),
        )
        .unwrap();
    });
    assert!(retire_exec_scopes(dir.path(), vec![scope.clone()], &ledger)
        .unwrap_err()
        .to_string()
        .contains("durability unconfirmed"));
    server.join().unwrap();
    assert_eq!(ledger.lock().unwrap().records(None, None), vec![scope]);
    assert!(exec_scope::Ledger::open(dir.path(), true)
        .unwrap()
        .recovery_required(None, Some("vm")));
}

#[test]
fn poisoned_journal_allows_exact_incarnation_revoke_and_abort_without_claiming_cleanup() {
    let (mut f, owner, epoch, _witness) = durable_qemu_fixture();
    let worker = f.manager.running["mac"]
        .bridge
        .lock()
        .unwrap()
        .worker_instance
        .clone();
    let scope = exec_scope::Scope {
        vm: "mac".into(),
        worker_instance: worker.clone(),
        owner: owner.clone(),
        epoch,
    };
    let ledger = f.manager.exec_retirements.clone();
    ledger.lock().unwrap().record(scope.clone()).unwrap();
    ledger.lock().unwrap().fail_journal_sync();
    assert!(ledger.lock().unwrap().complete(&scope).is_err());
    let args = json!({"id":"mac","worker_instance":worker,"owner":owner,"epoch":epoch});
    // A forged incarnation still cannot reach the worker, even during cleanup.
    let mut wrong = args.clone();
    wrong["worker_instance"] = json!("e".repeat(64));
    assert!(f
        .manager
        .dispatch(
            json!({"method":"exec_scope_revoke","arguments":wrong}),
            &mut None
        )
        .is_err());
    let (wire, result) = f.forward(
        "exec_scope_revoke",
        args.clone(),
        json!({"result":{"revoked":true,"pending_starts":0,"cgroup_empty":true}}),
    );
    assert_eq!(wire["method"], "guest_scope_revoke");
    result.unwrap();
    assert!(ledger.lock().unwrap().complete(&scope).is_err());
    // Revoke transport failures use this exact incarnation abort. Poison must
    // not prevent forced containment of another still-live scope/VM.
    let error = f
        .manager
        .dispatch(
            json!({"method":"exec_scope_abort","arguments":args}),
            &mut None,
        )
        .unwrap_err();
    assert!(error.to_string().contains("forced termination requested"));
    assert!(f
        .manager
        .running
        .get_mut("mac")
        .unwrap()
        .poll_exit()
        .unwrap()
        .is_some());
    assert_eq!(ledger.lock().unwrap().records(None, None), vec![scope]);
}

#[test]
fn normal_owned_exit_still_requires_exact_witness_and_durable_completion() {
    for mode in [
        "normal",
        "other_holder",
        "failed_exit",
        "recovered",
        "sync_failure",
        "wrong_instance",
    ] {
        let (mut f, _owner, _epoch, witness) = durable_qemu_fixture();
        let instance = witness.identity.worker_instance.clone();
        let mut holder = None;
        if mode == "other_holder" {
            let mut command = Command::new("/usr/bin/python3");
            command.args(["-c","import sys,select; print('ready',flush=True); select.select([sys.stdin],[],[],10)"])
                .stdin(Stdio::piped()).stdout(Stdio::piped());
            witness.configure_worker(&mut command).unwrap();
            let mut child = command.spawn().unwrap();
            drop(command);
            let mut ready = String::new();
            BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            assert_eq!(ready, "ready\n");
            holder = Some(child);
        }
        drop(witness);
        if mode == "recovered" {
            f.manager.exec_retirements = Arc::new(Mutex::new(
                exec_scope::Ledger::open(&f.manager.dir, true).unwrap(),
            ));
        }
        let run = f.manager.running.get_mut("mac").unwrap();
        run.force_stop().unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                if mode == "failed_exit" {
                    "read marker; exit 7"
                } else {
                    "read marker; exit 0"
                },
            ])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null());
        let mut child = command.spawn().unwrap();
        run.stdin = child.stdin.take().unwrap();
        run.child = child;
        run.expected_exit = false;
        run.exit_status = None;
        if mode == "wrong_instance" {
            run.bridge.lock().unwrap().worker_instance = "e".repeat(64);
        }
        // Live successful child has not exited: no automatic reconciliation.
        f.manager.reconcile_normal_execution_exit("mac").unwrap();
        assert!(f
            .manager
            .exec_retirements
            .lock()
            .unwrap()
            .pending_launch("mac"));
        f.manager
            .running
            .get_mut("mac")
            .unwrap()
            .stdin
            .write_all(b"exit\n")
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while f
            .manager
            .running
            .get_mut("mac")
            .unwrap()
            .poll_exit()
            .unwrap()
            .is_none()
        {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        if mode == "sync_failure" {
            f.manager
                .exec_retirements
                .lock()
                .unwrap()
                .fail_journal_sync();
        }
        let view = f.manager.view("mac").unwrap();
        if mode == "normal" {
            assert!(!f
                .manager
                .exec_retirements
                .lock()
                .unwrap()
                .pending_launch("mac"));
            assert_eq!(view["state"], "stopped");
        } else {
            assert!(
                f.manager
                    .exec_retirements
                    .lock()
                    .unwrap()
                    .pending_launch("mac"),
                "{mode}"
            );
        }
        if let Some(mut holder) = holder {
            assert_eq!(view["execution_reconciliation_status"], "unconfirmed");
            drop(holder.stdin.take());
            assert!(holder.wait().unwrap().success());
            f.manager.reconcile_normal_execution_exit("mac").unwrap();
            assert!(!f
                .manager
                .exec_retirements
                .lock()
                .unwrap()
                .pending_launch("mac"));
        }
        if mode == "normal" {
            let journal: Value = serde_json::from_slice(
                &fs::read(f.manager.dir.join("exec-journal/exec-scopes.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(journal["completed"][0]["worker_instance"], instance);
            f.manager.reconcile_normal_execution_exit("mac").unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(
                    &fs::read(f.manager.dir.join("exec-journal/exec-scopes.json")).unwrap()
                )
                .unwrap(),
                journal
            );
        }
    }
}

#[test]
fn status_observed_exit_is_the_same_sample_used_for_normal_proof() {
    let (mut f, _, _, witness) = durable_qemu_fixture();
    drop(witness);
    let run = f.manager.running.get_mut("mac").unwrap();
    run.force_stop().unwrap();
    let mut child = Command::new("/bin/sh")
        .args(["-c", "read marker; exit 0"])
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    run.stdin = child.stdin.take().unwrap();
    run.child = child;
    run.expected_exit = false;
    run.exit_status = None;
    // Reproduce the ordering deterministically: sample live, then let the actual
    // child exit before using the sample. No second poll may change its meaning.
    let live_sample = run.poll_exit().unwrap();
    assert!(live_sample.is_none());
    run.stdin.write_all(b"exit\n").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let exited = loop {
        if let Some(exit) = run.poll_exit().unwrap() {
            break exit;
        }
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    };
    assert!(exited.success());
    f.manager
        .reconcile_observed_normal_execution_exit("mac", live_sample)
        .unwrap();
    assert!(f
        .manager
        .exec_retirements
        .lock()
        .unwrap()
        .pending_launch("mac"));
    // The first status that actually samples terminal must complete the exact
    // witness proof before returning stopped. No additional status is required.
    let view = f.manager.view("mac").unwrap();
    assert_eq!(view["state"], "stopped");
    assert_eq!(view["exit_code"], 0);
    assert!(view.get("execution_reconciliation_required").is_none());
    assert!(view.get("execution_reconciliation_status").is_none());
    assert!(!f
        .manager
        .exec_retirements
        .lock()
        .unwrap()
        .pending_launch("mac"));
}

#[test]
fn backend_terminal_event_does_not_make_live_owned_child_terminal() {
    let (mut f, _, _, witness) = durable_qemu_fixture();
    drop(witness);
    for event in ["stopped", "crashed", "error"] {
        *f.manager.running["mac"].state.lock().unwrap() = json!({"state":event});
        let view = f.manager.view("mac").unwrap();
        assert_eq!(view["state"], "stopping");
        assert_eq!(view["runtime"]["state"], event);
        assert!(view["pid"].as_u64().is_some());
        assert!(view.get("exit_code").is_none());
        assert_eq!(view["execution_reconciliation_required"], false);
        assert!(f
            .manager
            .exec_retirements
            .lock()
            .unwrap()
            .pending_launch("mac"));
    }
    f.manager
        .running
        .get_mut("mac")
        .unwrap()
        .force_stop()
        .unwrap();
    let view = f.manager.view("mac").unwrap();
    assert_eq!(view["state"], "stopped");
    assert!(view.get("pid").is_none());
    // Force stop is not normal-exit proof: original witness obligation remains.
    assert_eq!(view["execution_reconciliation_required"], true);
}

#[test]
fn terminal_runtime_retains_only_its_own_original_launch_provenance() {
    let original = json!({"state":"paused","vm_id":"a","backend_pid":42,
        "disk_provenance":{"device":7,"inode":9,"qmp_fdsets_verified_at_start":true},
        "execution_witness":{"worker_instance":"original"},
        "execution_guardian":{"guardian_pid":43},"capture_connected":true});
    let mut state = original.clone();
    replace_runtime_state(&mut state, json!({"state":"stopped","exit_code":0}));
    for field in [
        "vm_id",
        "backend_pid",
        "disk_provenance",
        "execution_witness",
        "execution_guardian",
    ] {
        assert_eq!(state[field], original[field]);
    }
    assert!(state.get("capture_connected").is_none());
    replace_runtime_state(
        &mut state,
        json!({"state":"error","disk_provenance":{"inode":999}}),
    );
    assert_eq!(state["disk_provenance"], original["disk_provenance"]);
    let mut replacement = json!({"state":"starting"});
    replace_runtime_state(&mut replacement, json!({"state":"stopped","exit_code":0}));
    assert!(replacement.get("disk_provenance").is_none());
    assert!(replacement.get("execution_witness").is_none());
}
