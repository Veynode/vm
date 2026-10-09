use super::*;
#[test]
fn screenshot_actual_pipe_and_socket_preserve_large_png_and_request_bound() {
    let value = screenshot_test_support::value();
    let wire = serde_json::to_vec(&json!({"result":value})).unwrap();
    assert!(wire.len() > MAX_MESSAGE && wire.len() < MAX_SCREENSHOT_RESPONSE);
    let mut child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let data = wire.clone();
    let writer = thread::spawn(move || {
        input.write_all(&data).unwrap();
        input.write_all(b"\n").unwrap();
    });
    let line = read_frame_bounded(&mut BufReader::new(output), MAX_SCREENSHOT_RESPONSE)
        .unwrap()
        .unwrap();
    writer.join().unwrap();
    assert!(child.wait().unwrap().success());
    let parsed: Value = serde_json::from_slice(&line).unwrap();
    let normalized = normalize_screenshot(parsed["result"].clone(), false).unwrap();
    assert_eq!(normalized, value);
    let content = screenshot_content(normalized).unwrap();
    assert_eq!(content["content"][0]["data"], value["data_base64"]);
    let dir = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("control.sock")).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_frame(&mut BufReader::new(stream.try_clone().unwrap()))
            .unwrap()
            .unwrap();
        let request: Value = serde_json::from_slice(&request).unwrap();
        assert_eq!(request["method"], "screenshot");
        stream.write_all(&wire).unwrap();
        stream.write_all(b"\n").unwrap();
    });
    assert_eq!(
        call(dir.path(), "screenshot", json!({"id":"fixture"})).unwrap(),
        value
    );
    server.join().unwrap();
    assert!(read_frame(&mut std::io::Cursor::new(vec![b'a'; MAX_MESSAGE + 1])).is_err());
}
#[test]
fn screenshot_bounds_reject_oversize_truncation_and_unrelated_replies() {
    use base64::Engine;
    for data in [vec![b'a'; MAX_SCREENSHOT_RESPONSE + 1], b"{}".to_vec()] {
        assert!(
            read_frame_bounded(&mut std::io::Cursor::new(data), MAX_SCREENSHOT_RESPONSE).is_err()
        );
    }
    let mut bad = screenshot_test_support::value();
    bad["byte_length"] = json!(0);
    assert!(normalize_screenshot(bad, false).is_err());
    let mut bad = screenshot_test_support::value();
    bad["data_base64"] = json!("!not base64");
    assert!(normalize_screenshot(bad, false).is_err());
    let mut bad = screenshot_test_support::value();
    bad["data_base64"] =
        json!(base64::engine::general_purpose::STANDARD.encode(vec![0; MAX_SCREENSHOT_BYTES + 1]));
    bad["byte_length"] = json!(MAX_SCREENSHOT_BYTES + 1);
    assert!(normalize_screenshot(bad, false).is_err());
    let mut jpeg = vec![0u8; 700 * 1024 + 1];
    jpeg[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
    let end = jpeg.len();
    jpeg[end - 2..].copy_from_slice(&[0xff, 0xd9]);
    let mut bad = screenshot_test_support::value();
    bad["mime_type"] = json!("image/jpeg");
    bad["byte_length"] = json!(jpeg.len());
    bad["data_base64"] = json!(base64::engine::general_purpose::STANDARD.encode(jpeg));
    assert!(normalize_screenshot(bad, false).is_err());
    let (tx, _) = mpsc::channel();
    let mut pending = BTreeMap::new();
    pending.insert(
        1,
        PendingReply {
            sender: tx,
            limit: MAX_SCREENSHOT_RESPONSE,
        },
    );
    assert!(validate_backend_response_bound(&json!({"id":1}), MAX_MESSAGE + 1, &pending).is_ok());
    for value in [
        json!({"id":2}),
        json!({"id":1,"event":"state"}),
        json!({"id":null}),
    ] {
        assert!(validate_backend_response_bound(&value, MAX_MESSAGE + 1, &pending).is_err());
    }
    assert_eq!(response_limit("info"), MAX_MESSAGE);
    assert_eq!(
        mcp_response_limit(&json!({"method":"tools/call","params":{"name":"vm_exec"}})),
        MAX_MESSAGE
    );
    assert_eq!(
        mcp_response_limit(&json!({"method":"tools/call","params":{"name":"vm_screenshot"}})),
        MAX_SCREENSHOT_RESPONSE
    );
}
