//! API 1 on a real socket: `hello`, codes, streams, and the legacy path beside it.
use crate::{handle_connection, resources::MemoryBudget, Manager};
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use veynodevm_api::{Client, ErrorCode, Event};

struct Daemon {
    socket: PathBuf,
    state: PathBuf,
    _dir: tempfile::TempDir,
}

fn daemon() -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let state = crate::test_private_state(dir.path());
    let socket = state.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let manager = Manager::load(
        state.clone(),
        "/unused".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    let manager = Arc::new(Mutex::new(manager));
    thread::spawn(move || {
        for stream in listener.incoming() {
            let manager = manager.clone();
            let stream = stream.unwrap();
            thread::spawn(move || {
                let _ = handle_connection(stream, manager, Instant::now());
            });
        }
    });
    Daemon {
        socket,
        state,
        _dir: dir,
    }
}

fn client(daemon: &Daemon) -> Client {
    Client::at_socket(daemon.socket.clone(), "test/1")
}

fn code(error: &anyhow::Error) -> ErrorCode {
    error
        .downcast_ref::<veynodevm_api::ApiError>()
        .unwrap()
        .code
}

#[test]
fn hello_lists_services_and_the_vm_layer() {
    let daemon = daemon();
    let hello = client(&daemon).hello().unwrap();
    assert_eq!(hello.api, 1);
    assert_eq!(hello.daemon, env!("CARGO_PKG_VERSION"));
    for method in [
        "hello",
        "daemon.status",
        "status",
        "exec_start",
        "screenshot",
    ] {
        assert!(hello.methods.iter().any(|m| m == method), "{method}");
    }
    assert!(!hello.runtime.apple_virtualization && !hello.runtime.qemu);
}

#[test]
fn another_api_version_is_refused_with_its_code() {
    let daemon = daemon();
    let mut socket = UnixStream::connect(&daemon.socket).unwrap();
    socket
        .write_all(b"{\"method\":\"hello\",\"arguments\":{\"api\":2}}\n")
        .unwrap();
    let mut line = String::new();
    BufReader::new(socket).read_line(&mut line).unwrap();
    let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["code"], "version");
}

#[test]
fn calls_answer_with_codes() {
    let daemon = daemon();
    let client = client(&daemon);
    let status = client.call("daemon.status", json!({})).unwrap();
    assert_eq!(status["api"], 1);
    assert_eq!(status["vms"], 0);
    assert_eq!(
        code(&client.call("test.refuse", json!({})).unwrap_err()),
        ErrorCode::NotFound
    );
    // The VM layer's errors gain a code once the client said hello.
    assert_eq!(
        code(&client.call("status", json!({"id":"ghost"})).unwrap_err()),
        ErrorCode::NotFound
    );
    assert_eq!(
        code(&client.call("frobnicate", json!({})).unwrap_err()),
        ErrorCode::Unsupported
    );
    assert_eq!(
        code(&client.call("exec_poll", json!({})).unwrap_err()),
        ErrorCode::Invalid
    );
}

#[test]
fn legacy_requests_still_answer_without_a_code() {
    let daemon = daemon();
    assert_eq!(
        crate::call(&daemon.state, "list", json!({})).unwrap(),
        json!([])
    );
    let mut socket = UnixStream::connect(&daemon.socket).unwrap();
    socket
        .write_all(b"{\"method\":\"status\",\"arguments\":{\"id\":\"ghost\"}}\n")
        .unwrap();
    let mut line = String::new();
    BufReader::new(socket).read_line(&mut line).unwrap();
    let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(reply["error"].is_string());
    assert!(reply.get("code").is_none());
}

#[test]
fn a_stream_carries_input_control_and_its_end() {
    let daemon = daemon();
    let stream = client(&daemon)
        .stream("test.echo", json!({"tag":"t"}))
        .unwrap();
    let sender = stream.sender();
    sender
        .control(&json!({"resize":{"rows":3,"cols":4}}))
        .unwrap();
    let big = vec![b'x'; veynodevm_api::FRAME_MAX + 10];
    sender.stdin(&big).unwrap();
    sender.close_stdin().unwrap();
    let (mut out, mut err, mut events) = (Vec::new(), Vec::new(), Vec::new());
    let result = stream
        .finish(|event| {
            match event {
                Event::Stdout(data) => out.extend(data),
                Event::Stderr(data) => err.extend(data),
                Event::Event(value) => events.push(value),
                Event::End(_) => unreachable!(),
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(result, json!({"bytes": big.len(), "tag": "t"}));
    assert_eq!(out, big);
    assert_eq!(err, b"done");
    assert_eq!(events[0], json!({"progress":"echoing"}));
    assert!(events.contains(&json!({"control":{"resize":{"rows":3,"cols":4}}})));
}

#[test]
fn closing_a_stream_is_seen_by_its_handler() {
    let daemon = daemon();
    let stream = client(&daemon).stream("test.wait", json!({})).unwrap();
    stream.sender().cancel();
    drop(stream);
    let marker = daemon.state.join("noticed-close");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "the handler never saw the close");
        thread::sleep(Duration::from_millis(10));
    }
}

/// A guest session server that echoes input as output, reports resizes on
/// stderr and exits 7 at the end of its input.
fn fake_guest() -> (u16, thread::JoinHandle<Vec<u8>>) {
    use crate::services::session::{read_frame, Writer};
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let guest = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let writer = Writer::new(&stream).unwrap();
        let mut reader = BufReader::new(stream);
        let mut kinds = Vec::new();
        loop {
            let Ok((kind, data)) = read_frame(&mut reader) else {
                return kinds;
            };
            kinds.push(kind);
            match kind {
                b'i' => writer.frame(b'o', &data).unwrap(),
                b'r' => {
                    let rows = u16::from_be_bytes([data[0], data[1]]);
                    let cols = u16::from_be_bytes([data[2], data[3]]);
                    writer
                        .frame(b'x', format!("{rows}x{cols}").as_bytes())
                        .unwrap();
                }
                b'e' => {
                    writer.frame(b'q', b"7").unwrap();
                    return kinds;
                }
                _ => {}
            }
        }
    });
    (port, guest)
}

#[test]
fn a_guest_session_relays_input_resizes_and_its_exit() {
    let daemon = daemon();
    let (port, guest) = fake_guest();
    let mut stream = client(&daemon)
        .stream("test.session", json!({ "port": port }))
        .unwrap();
    let sender = stream.sender();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut read = |out: &mut Vec<u8>, err: &mut Vec<u8>| match stream.next_event().unwrap() {
        Event::Stdout(data) => out.extend(data),
        Event::Stderr(data) => err.extend(data),
        other => panic!("{other:?}"),
    };
    // Control and input are separate channels with no order between them,
    // so the input goes once the guest has answered the resize.
    sender
        .control(&json!({"resize":{"rows":40,"cols":120}}))
        .unwrap();
    while err.len() < 6 {
        read(&mut out, &mut err);
    }
    sender.stdin(b"echo hi").unwrap();
    // The guest's answers arrive as the relay passes each frame on.
    while out.len() < 7 {
        read(&mut out, &mut err);
    }
    sender.close_stdin().unwrap();
    let result = stream.finish(|_| Ok(())).unwrap();
    assert_eq!(out, b"echo hi");
    assert_eq!(err, b"40x120");
    assert_eq!(result, json!({ "exitCode": 7 }));
    assert_eq!(guest.join().unwrap(), b"rie");
}

#[test]
fn leaving_a_guest_session_hangs_it_up() {
    let daemon = daemon();
    let (port, guest) = fake_guest();
    let stream = client(&daemon)
        .stream("test.session", json!({ "port": port }))
        .unwrap();
    stream.sender().cancel();
    drop(stream);
    // The relay shut the guest connection: its reader ends without an `e`.
    assert_eq!(guest.join().unwrap(), b"");
}
