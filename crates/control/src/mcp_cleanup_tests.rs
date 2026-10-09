use super::*;

fn clean() -> Value {
    json!({"closed":true,"retired_owner":true,
        "job_cancellation":{"attempted":1,"unconfirmed_requests":0,"outcomes":[{"result":{"result":{"cancel_requested":true}}}]},
        "file_cancellation":{"attempted":1,"unconfirmed_requests":0,"outcomes":[{"result":{"result":{"cancelled_transfers":2}}}]},
        "input_release":{"confirmed":true}})
}

#[test]
fn cleanup_requires_explicit_retirement_and_typed_guest_outcomes() {
    assert!(validate_session_cleanup(&clean()).is_ok());
    for pointer in ["/closed", "/retired_owner", "/input_release/confirmed"] {
        let mut result = clean();
        *result.pointer_mut(pointer).unwrap() = json!(false);
        assert!(validate_session_cleanup(&result).is_err());
    }
    for key in ["job_cancellation", "file_cancellation"] {
        for mutation in ["missing", "error", "count", "unconfirmed", "malformed"] {
            let mut result = clean();
            match mutation {
                "missing" => {
                    result.as_object_mut().unwrap().remove(key);
                }
                "error" => result[key]["outcomes"][0] = json!({"error":"fixture"}),
                "count" => result[key]["attempted"] = json!(2),
                "unconfirmed" => result[key]["unconfirmed_requests"] = json!(1),
                _ => result[key]["outcomes"][0]["result"] = json!({"result":{}}),
            }
            assert!(
                validate_session_cleanup(&result).is_err(),
                "{key}/{mutation}"
            );
        }
    }
    let mut result = clean();
    result["file_cancellation"]["outcomes"][0]["result"]["result"]["cancelled_transfers"] =
        json!(true);
    assert!(validate_session_cleanup(&result).is_err());
    result = clean();
    result["job_cancellation"]["outcomes"][0]["result"]["result"]["cancel_requested"] =
        json!(false);
    assert!(validate_session_cleanup(&result).is_ok()); // Valid ACK; not a terminal-job assertion.
}

#[test]
fn concurrent_finish_retains_first_failed_cleanup_without_second_rpc() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("control.sock")).unwrap();
    fs::set_permissions(
        dir.path().join("control.sock"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let state = Arc::new(SessionCleanupState {
        dir: dir.path().into(),
        owner: "a".repeat(64),
        stopped: AtomicBool::new(false),
        outcome: Mutex::new(None),
    });
    let server = thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(peer.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], "session_end");
        thread::sleep(Duration::from_millis(50));
        write_json(&mut peer, &json!({"error":"fixture unavailable"})).unwrap();
        listener.set_nonblocking(true).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert!(
            listener.accept().is_err(),
            "duplicate retirement request can erase failure evidence"
        );
    });
    let second = state.clone();
    let waiter = thread::spawn(move || second.finish());
    assert!(state.finish().is_err());
    assert!(waiter.join().unwrap().is_err());
    assert!(state.finish().is_err());
    server.join().unwrap();
}
