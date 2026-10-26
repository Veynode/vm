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
#[test]
fn a_lossless_2560_wide_frame_of_several_mib_passes_every_bound() {
    // About 5.5 MiB of PNG: over the old 2 MiB cap, under 8 MiB. Its base64 reply exceeds the old 3 MiB response bound.
    let value = screenshot_test_support::value_sized(2560, 720);
    let png_len = value["byte_length"].as_u64().unwrap() as usize;
    assert!(
        png_len > 4 * 1024 * 1024 && png_len < MAX_SCREENSHOT_BYTES,
        "{png_len}"
    );
    let wire = serde_json::to_vec(&json!({"result":value})).unwrap();
    assert!(wire.len() > 3 * 1024 * 1024 && wire.len() < MAX_SCREENSHOT_RESPONSE);
    let line = read_frame_bounded(
        &mut BufReader::new(std::io::Cursor::new(
            [wire.clone(), b"\n".to_vec()].concat(),
        )),
        MAX_SCREENSHOT_RESPONSE,
    )
    .unwrap()
    .unwrap();
    let parsed: Value = serde_json::from_slice(&line).unwrap();
    let normalized = normalize_screenshot(parsed["result"].clone(), false).unwrap();
    assert_eq!(normalized, value);
    assert_eq!(response_limit("screenshot"), MAX_SCREENSHOT_RESPONSE);
}
/// Counts what reaches the descriptor; fails every write once `fail_after` bytes were accepted.
struct CountingWriter {
    bytes: Vec<u8>,
    writes: usize,
    flushes: usize,
    fail_after: usize,
}
impl Write for CountingWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.writes += 1;
        if self.bytes.len() >= self.fail_after {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}
#[test]
fn frames_keep_their_exact_bytes_and_reach_the_descriptor_in_few_writes() {
    let screenshot = json!({"result":screenshot_test_support::value()});
    for value in [
        json!({"result":{"state":"running","n":[1,2,3]}}),
        json!({"error":"x"}),
        screenshot,
    ] {
        let mut writer = CountingWriter {
            bytes: Vec::new(),
            writes: 0,
            flushes: 0,
            fail_after: usize::MAX,
        };
        write_json(&mut writer, &value).unwrap();
        let mut expected = serde_json::to_vec(&value).unwrap();
        expected.push(b'\n');
        assert!(writer.bytes == expected, "frame bytes changed");
        // Head, picture string and tail at most; the unbuffered serializer made one write per token.
        assert!(
            writer.writes <= 3 && writer.flushes == 1,
            "{} writes",
            writer.writes
        );
    }
    // A write that fails is not attempted again when the buffer is dropped.
    let mut writer = CountingWriter {
        bytes: Vec::new(),
        writes: 0,
        flushes: 0,
        fail_after: 0,
    };
    assert!(write_json(&mut writer, &json!({"result":true})).is_err());
    assert_eq!((writer.writes, writer.bytes.len()), (1, 0));
}
#[test]
fn timing_line_is_numbers_only_and_takes_nothing_but_integers_from_the_worker() {
    let reply = json!({"id":4,"result":{"data_base64":"guest"},"screenshot_timing":{"broker_wait_us":11,"broker_read_us":"/private/path","worker_parse_us":13,"worker_wake_us":-1,"pipe_us":15,"parse_us":16,"bytes":17,"extra":"text"}});
    let timing = ShotTiming::from_reply(&reply, 10, 18);
    assert_eq!(
        timing.line(20, 19),
        "screenshot timing total_us=20 backend_us=10 broker_wait_us=11 broker_read_us=0 worker_parse_us=13 \
         worker_wake_us=0 pipe_us=15 parse_us=16 normalize_us=18 write_us=19 bytes=17"
    );
    assert_eq!(
        ShotTiming::from_reply(&json!({"id":4,"result":{}}), 1, 2),
        ShotTiming {
            backend_us: 1,
            normalize_us: 2,
            ..Default::default()
        }
    );
}
#[test]
fn screenshot_reply_connection_gets_the_larger_send_buffer_and_others_do_not_ask_for_it() {
    let size = |socket: &UnixStream| {
        let mut size: libc::c_int = 0;
        let mut length = std::mem::size_of_val(&size) as libc::socklen_t;
        assert_eq!(
            unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&mut size as *mut libc::c_int).cast(),
                    &mut length,
                )
            },
            0
        );
        size as usize
    };
    let (socket, _peer) = UnixStream::pair().unwrap();
    assert!(size(&socket) < LARGE_REPLY_SEND_BUFFER);
    widen_send_buffer(&socket);
    assert!(size(&socket) >= LARGE_REPLY_SEND_BUFFER);
    const { assert!(LARGE_REPLY_SEND_BUFFER <= MAX_SCREENSHOT_RESPONSE / 8) };
}
