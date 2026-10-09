// Production Manager dispatch and real worker/Unix pipes; controlled Guest replies.
// These tests prove routing and receipt semantics, not actual Guest staging cleanup.

#[test]
fn file_routes_expired_guest_deadline_still_requires_first_cleanup_ack() {
    let mut fixture = FilePipeFixture::macos();
    let owner = "a".repeat(64);
    let (_, result) = fixture.forward(
        "file_upload_begin",
        json!({"id":"mac","owner":owner,"path":"partial","size":3,"sha256":"b".repeat(64)}),
        json!({"id":"begin","result":{"token":"c".repeat(32)}}),
    );
    assert!(result.is_ok());
    // Advance only the routing clock, past the removed 330s eviction boundary.
    fixture.manager.file_sessions.get_mut(&owner).unwrap().0 =
        std::time::Instant::now() - Duration::from_secs(331);
    let (_, result) = fixture.forward(
        "file_download_begin",
        json!({"id":"mac","owner":"d".repeat(64),"path":"other"}),
        json!({"id":"other","error":{"code":"unavailable","message":"worker stopped"}}),
    );
    assert!(result.is_err());
    assert!(fixture.manager.file_sessions[&owner].1.contains("mac"));

    let listener = UnixListener::bind(fixture.manager.dir.join("control.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let expected_owner = owner.clone();
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "cleanup RPC missing");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("cleanup accept failed: {e}"),
            }
        };
        // Darwin accepted sockets may inherit listener O_NONBLOCK.
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let request: Value = serde_json::from_slice(
            &read_frame(&mut BufReader::new(socket.try_clone().unwrap()))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(request["method"], "file_session_end");
        assert_eq!(
            request["arguments"],
            json!({"id":"mac","owner":expected_owner})
        );
        write_json(&mut socket, &json!({"result":{"id":"cleanup","error":{"code":"unavailable","message":"worker stopped"}}})).unwrap();
    });
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
    server.join().unwrap();
    assert_eq!(receipt["file_cancellation"]["attempted"], 1);
    assert!(
        validate_session_cleanup(&receipt).is_err(),
        "failed Guest cleanup must not become successful stdio EOF"
    );
}

#[test]
fn file_routes_capacity_rejects_new_owner_without_evicting_old_unconfirmed_routes() {
    let mut fixture = FilePipeFixture::macos();
    for n in 0..256 {
        let owner = format!("{n:064x}");
        let (_, result) = fixture.forward(
            "file_download_begin",
            json!({"id":"mac","owner":owner,"path":"artifact"}),
            json!({"id":"request","error":{"code":"unavailable","message":"no cleanup proof"}}),
        );
        assert!(result.is_err());
        fixture.manager.file_sessions.get_mut(&owner).unwrap().0 =
            std::time::Instant::now() - Duration::from_secs(86400);
    }
    let before = fixture.manager.file_sessions.clone();
    let next = fixture.manager.running["mac"].next;
    let mut completion = None;
    let error = fixture.manager.dispatch(
        json!({"method":"file_upload_begin","arguments":{"id":"mac","owner":format!("{:064x}",256),"path":"new","size":0,"sha256":"b".repeat(64)}}),
        &mut completion,
    ).unwrap_err();
    assert!(error.to_string().contains("file session capacity reached"));
    assert!(completion.is_none());
    assert_eq!(
        fixture.manager.running["mac"].next, next,
        "capacity denial must precede worker dispatch"
    );
    assert_eq!(
        fixture.manager.file_sessions, before,
        "no old route may be evicted, even after one day"
    );
    // Existing owners can still operate or clean up while admission is full.
    let (_, result) = fixture.forward(
        "file_cancel",
        json!({"id":"mac","owner":format!("{:064x}",0),"token":"c".repeat(32)}),
        json!({"id":"cancel","error":{"code":"unavailable","message":"worker stopped"}}),
    );
    assert!(result.is_err());
    assert_eq!(fixture.manager.file_sessions.len(), 256);
}
