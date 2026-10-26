// Actual Manager dispatch / worker pipe / Unix cleanup RPC, with deliberately
// injected Guest outcomes. Not a Guest/cgroup process-termination acceptance.
#[test]
fn finished_containment_failure_retains_old_job_and_every_cleanup_request() {
    let mut fixture = FilePipeFixture::macos();
    fixture.manager.configs.get_mut("mac").unwrap().guest = "linux".into();
    let owner = "a".repeat(64);
    let job = "b".repeat(32);
    let now = std::time::Instant::now();
    jobs::Jobs::reserve(&fixture.manager.jobs, now - Duration::from_secs(331))
        .unwrap()
        .register("mac", &job, &owner, now - Duration::from_secs(331))
        .unwrap();
    let (_,reply)=fixture.forward("exec_poll",json!({"id":"mac","job_id":job}),
        json!({"id":"poll","result":{"job_id":job,"state":"finished","error":{"code":"containment_failed","message":"fixture cgroup still populated"}}}));
    assert_eq!(
        reply.unwrap()["result"]["error"]["code"],
        "containment_failed"
    );
    assert_eq!(
        fixture
            .manager
            .jobs
            .lock()
            .unwrap()
            .owned_running(&owner, None, now)
            .len(),
        1
    );
    let listener = UnixListener::bind(fixture.manager.dir.join("control.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let expected_job = job.clone();
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(p) => break p,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "old job cancel was skipped"
                        );
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let wire: Value = serde_json::from_slice(
                &read_frame(&mut BufReader::new(socket.try_clone().unwrap()))
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(wire["method"], "exec_cancel");
            assert_eq!(
                wire["arguments"],
                json!({"id":"mac","job_id":expected_job})
            );
            write_json(
                &mut socket,
                &json!({"error":"fixture Guest service unavailable; cleanup unconfirmed"}),
            )
            .unwrap();
        }
    });
    for _ in 0..2 {
        let mut completion = None;
        fixture
            .manager
            .dispatch(
                json!({"method":"session_end","arguments":{"owner":owner}}),
                &mut completion,
            )
            .unwrap();
        let (peer, _other) = UnixStream::pair().unwrap();
        let receipt = completion.unwrap().wait(&peer).unwrap();
        assert_eq!(receipt["job_cancellation"]["attempted"], 1);
        assert!(validate_session_cleanup(&receipt).is_err());
        assert_eq!(
            fixture
                .manager
                .jobs
                .lock()
                .unwrap()
                .owned_running(&owner, None, std::time::Instant::now())
                .len(),
            1
        );
    }
    server.join().unwrap();
}

#[test]
fn malformed_finished_poll_cannot_release_retention_but_valid_terminal_can() {
    let mut fixture = FilePipeFixture::macos();
    fixture.manager.configs.get_mut("mac").unwrap().guest = "linux".into();
    let owner = "a".repeat(64);
    let job = "c".repeat(32);
    let now = std::time::Instant::now();
    jobs::Jobs::reserve(&fixture.manager.jobs, now)
        .unwrap()
        .register("mac", &job, &owner, now)
        .unwrap();
    let args = json!({"id":"mac","job_id":job});
    for requested in [true, false] {
        let (_, cancelled) = fixture.forward(
            "exec_cancel",
            args.clone(),
            json!({"id":"cancel","result":{"cancel_requested":requested}}),
        );
        assert!(cancelled.is_ok());
        assert_eq!(
            fixture
                .manager
                .jobs
                .lock()
                .unwrap()
                .running("mac", now)
                .len(),
            1,
            "cancel acknowledgement is not terminal containment proof"
        );
    }
    let (_, malformed) = fixture.forward(
        "exec_poll",
        args.clone(),
        json!({"id":"poll","result":{"state":"finished","result":{}}}),
    );
    assert!(malformed.is_err());
    assert_eq!(
        fixture
            .manager
            .jobs
            .lock()
            .unwrap()
            .running("mac", now)
            .len(),
        1
    );
    let (_, running) = fixture.forward(
        "exec_poll",
        args.clone(),
        json!({"id":"poll","result":{"state":"running"}}),
    );
    assert!(running.is_ok());
    assert_eq!(
        fixture
            .manager
            .jobs
            .lock()
            .unwrap()
            .running("mac", now)
            .len(),
        1
    );
    let(_,finished)=fixture.forward("exec_poll",args,json!({"id":"poll","result":{"state":"finished","result":{"exit_code":-1,"cancelled":true}}}));
    assert!(finished.is_ok());
    assert!(fixture
        .manager
        .jobs
        .lock()
        .unwrap()
        .running("mac", now)
        .is_empty());
}
