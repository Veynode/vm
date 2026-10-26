use super::*;
use std::os::unix::fs::PermissionsExt;

fn fixture_app(dir: &Path, control_bin: PathBuf) -> Arc<App> {
    Arc::new(App {
        config: Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            tls_cert: dir.join("unused"),
            tls_key: dir.join("unused"),
            allowed_origins: vec![],
            principals: vec![],
            session_idle_seconds: 900,
        },
        state_dir: dir.into(),
        control_bin,
        sessions: Mutex::new(BTreeMap::new()),
        requests: Semaphore::new(64),
        closing: AtomicBool::new(false),
        cleanup_failed: AtomicBool::new(false),
        changed: Notify::new(),
        screens: std::sync::Mutex::new(BTreeMap::new()),
    })
}

// Real owned child, actual dispatch cancellation, and pipe backpressure. The
// child is a synthetic protocol fixture, not vm-control or a Guest service.
async fn cancelled_exchange(initializing: bool) {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("stdio-fixture");
    let marker = dir.path().join("request-received");
    fs::write(&script, format!("#!/usr/bin/env python3\nimport sys,json\nr=json.loads(sys.stdin.readline())\nopen({},'w').close()\nsys.stdin.read()\nprint(json.dumps({{'jsonrpc':'2.0','id':r['id'],'result':{{'padding':'x'*700000}}}}),flush=True)\n", serde_json::to_string(&marker.to_string_lossy()).unwrap())).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let app = fixture_app(dir.path(), script.clone());
    let principal = Principal {
        name: "fixture".into(),
        token_sha256: "a".repeat(64),
        scopes: vec!["observe".into()],
        vm_ids: vec![],
        tools: vec![],
    };
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert(
        "accept",
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    headers.insert(
        "mcp-protocol-version",
        HeaderValue::from_static(vm_control::VERSION),
    );
    if !initializing {
        let child = Command::new(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let lifecycle = Lifecycle::new(child, principal.clone());
        let io = Arc::new(Mutex::new(Bridge {
            lifecycle: lifecycle.clone(),
        }));
        app.sessions.lock().await.insert(
            "fixture".into(),
            Session {
                principal: principal.name.clone(),
                created: Instant::now(),
                touched: Instant::now(),
                io,
                lifecycle,
            },
        );
        headers.insert("mcp-session-id", HeaderValue::from_static("fixture"));
    }
    let request =
        json!({"jsonrpc":"2.0","id":1,"method":if initializing {"initialize"} else {"ping"}});
    let copy = app.clone();
    let operation = tokio::spawn(async move {
        dispatch(
            &copy,
            &principal,
            &Method::POST,
            &headers,
            &serde_json::to_vec(&request).unwrap(),
        )
        .await
    });
    timeout(Duration::from_secs(3), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let lifecycle = app
        .sessions
        .lock()
        .await
        .values()
        .next()
        .unwrap()
        .lifecycle
        .clone();
    assert_eq!(app.sessions.lock().await.len(), 1);
    operation.abort();
    assert!(operation.await.unwrap_err().is_cancelled());
    timeout(Duration::from_secs(3), async {
        loop {
            let changed = app.changed.notified();
            if app.sessions.lock().await.is_empty() {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("cancelled HTTP future must retire and drain the large child response");
    assert!(lifecycle.retired.load(Ordering::Acquire));
    assert_eq!(lifecycle.outcome.get(), Some(&true));
    assert!(!app.cleanup_failed.load(Ordering::Acquire));
    assert!(lifecycle
        .child
        .lock()
        .await
        .try_wait()
        .unwrap()
        .unwrap()
        .success());
}

#[tokio::test]
async fn cancelled_initialization_keeps_cleanup_owner_and_drains_response() {
    cancelled_exchange(true).await;
}
#[tokio::test]
async fn cancelled_request_retires_session_and_drains_response() {
    cancelled_exchange(false).await;
}

#[path = "../../screenshot_test_support.rs"]
mod screenshot_test_support;
#[tokio::test]
async fn screenshot_bridge_large_png_is_method_scoped_and_rejects_truncation_and_oversize() {
    let png = screenshot_test_support::value();
    let good=serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"image","mimeType":"image/png","data":png["data_base64"]}],"isError":false}})).unwrap();
    assert!(good.len() > vm_control::MAX_MESSAGE);
    for (method, terminated, oversize, expected) in [
        ("vm_screenshot", true, false, true),
        ("vm_info", true, false, false),
        ("vm_screenshot", false, false, false),
        ("vm_screenshot", true, true, false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response");
        let mut data = if oversize {
            vec![b'x'; vm_control::MAX_SCREENSHOT_RESPONSE + 1]
        } else {
            good.clone()
        };
        if terminated {
            data.push(b'\n');
        }
        fs::write(&path, data).unwrap();
        let child=Command::new("/usr/bin/python3").args(["-c","import sys; sys.stdin.readline(); sys.stdout.buffer.write(open(sys.argv[1],'rb').read()); sys.stdout.flush()",path.to_str().unwrap()]).stdin(Stdio::piped()).stdout(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
        let lifecycle = Lifecycle::new(
            child,
            Principal {
                name: "fixture".into(),
                token_sha256: "a".repeat(64),
                scopes: vec!["observe".into()],
                vm_ids: vec![],
                tools: vec![],
            },
        );
        let mut bridge = Bridge {
            lifecycle: lifecycle.clone(),
        };
        let result=timeout(Duration::from_secs(5),bridge.exchange(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":method,"arguments":{"id":"fixture"}}}),None)).await.unwrap();
        assert_eq!(result.is_ok(), expected, "{method}/{terminated}/{oversize}");
        if let Ok(Some(reply)) = result {
            assert_eq!(reply["result"]["content"][0]["data"], png["data_base64"]);
        }
        assert!(lifecycle.close().await);
    }
}
