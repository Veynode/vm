//! Real media-service stderr during refused session negotiation, with a scripted
//! capability probe and no native encoder/Guest. This exercises SDP error logging,
//! not authenticated DataChannel screenshot/file delivery or bearer authorization.
#![cfg(unix)]
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{
    fs::{self, File},
    os::unix::fs::PermissionsExt,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::Command,
    time::timeout,
};

const SCREEN: &[u8] = b"VNMARK-SCREEN-01!";
const TOKEN: &[u8] = b"VNMARK-TOKEN-0123456789abcdef";
const BODY: &[u8] = b"VNMARK-FILE-BODY-0123456789abcdef";

fn representations(marker: &[u8]) -> Vec<Vec<u8>> {
    let mut forms = vec![marker.to_vec(), STANDARD.encode(marker).into_bytes()];
    for upper in [false, true] {
        forms.push(
            marker
                .iter()
                .map(|b| {
                    if upper {
                        format!("{b:02X}")
                    } else {
                        format!("{b:02x}")
                    }
                })
                .collect::<String>()
                .into_bytes(),
        );
    }
    // Complete base64 groups detect an embedded canary at every alignment,
    // including inside a full screenshot body where marker padding is absent.
    for offset in 0..3 {
        let end = offset + (marker.len() - offset) / 3 * 3;
        forms.push(STANDARD.encode(&marker[offset..end]).into_bytes());
    }
    forms
}
fn contains_marker(log: &[u8], marker: &[u8]) -> bool {
    representations(marker)
        .iter()
        .any(|form| log.windows(form.len()).any(|window| window == form))
}
fn assert_clean(log: &[u8]) {
    for (name, marker) in [
        ("screenshot", SCREEN),
        ("token", TOKEN),
        ("file body", BODY),
    ] {
        assert!(
            !contains_marker(log, marker),
            "{name} canary leaked into media stderr"
        );
    }
}

#[test]
fn scanner_detects_raw_base64_hex_and_embedded_picture() {
    for marker in [SCREEN, TOKEN, BODY] {
        for form in representations(marker) {
            assert!(contains_marker(
                &[b"prefix ".as_slice(), &form, b" suffix"].concat(),
                marker
            ));
        }
        assert!(!contains_marker(
            b"media session failed: remote description rejected",
            marker
        ));
    }
    let picture = [b"\x89PNG\r\n\x1a\n".as_slice(), &SCREEN.repeat(128)].concat();
    assert!(contains_marker(STANDARD.encode(picture).as_bytes(), SCREEN));
}

async fn post(host: &str, route: &str, body: Value) -> (u16, Vec<u8>) {
    let mut socket = TcpStream::connect(host).await.unwrap();
    let body = serde_json::to_vec(&body).unwrap();
    let header = format!("POST {route} HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        std::str::from_utf8(TOKEN).unwrap(), body.len());
    socket.write_all(header.as_bytes()).await.unwrap();
    socket.write_all(&body).await.unwrap();
    let mut response = Vec::new();
    socket
        .take(16 * 1024 + 1)
        .read_to_end(&mut response)
        .await
        .unwrap();
    assert!(
        response.len() <= 16 * 1024,
        "oversized HTTP fixture response"
    );
    let split = response.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
    let headers = std::str::from_utf8(&response[..split]).unwrap();
    let status = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (status, response[split + 4..].to_vec())
}

#[tokio::test]
async fn refused_media_sessions_keep_request_canaries_out_of_stderr() {
    let tmp = tempfile::Builder::new()
        .prefix("media-log-hygiene-")
        .tempdir_in("/tmp")
        .unwrap();
    let encoder = tmp.path().join("encoder.sh");
    // Like the console/encoder integration fixtures, this is synthetic. Only
    // the probe may execute: refused negotiation must never launch an encoder.
    fs::write(&encoder, b"#!/bin/sh\nif [ \"$1\" = --probe-codecs ]; then\n  printf '%s\\n' '{\"h264\":\"verified-hardware\",\"h265\":\"unavailable\"}'\nelse\n  echo unexpected-encoder-start >&2\n  exit 91\nfi\n").unwrap();
    fs::set_permissions(&encoder, fs::Permissions::from_mode(0o700)).unwrap();
    let placeholder = tmp.path().join("unused-console");
    fs::write(&placeholder, b"protocol fixture; no VM socket").unwrap();
    let stderr_path = tmp.path().join("media.stderr");
    // security.md lists inherited stderr, with no default media log file. A
    // private file captures the complete stream without pipe backpressure or
    // tail truncation, including negotiation cleanup and process shutdown.
    let mut child = Command::new(env!("CARGO_BIN_EXE_vm-media-local"))
        .arg(&encoder)
        .arg(&placeholder)
        .arg(tmp.path())
        .arg("0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(File::create(&stderr_path).unwrap())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result = timeout(Duration::from_secs(6), async {
        let mut stdout = BufReader::new(child.stdout.take().unwrap()).take(8193);
        let mut ready = String::new();
        stdout.read_line(&mut ready).await.unwrap();
        assert!(
            !ready.is_empty() && ready.len() <= 8192,
            "media service failed before readiness: {}",
            fs::read_to_string(&stderr_path).unwrap()
        );
        let ready: Value = serde_json::from_str(&ready).unwrap();
        let host = ready["url"]
            .as_str()
            .unwrap()
            .strip_prefix("http://")
            .unwrap();
        let picture =
            STANDARD.encode([b"\x89PNG\r\n\x1a\n".as_slice(), &SCREEN.repeat(128)].concat());
        // The unsigned diagnostic has no bearer-token authentication and no
        // screenshot/file route. It does receive arbitrary SDP and HTTP headers.
        // Put canaries in that actual request body; these are not transferred
        // screenshots/files. Exercise raw, base64 and both hex error inputs.
        for variant in 0..4 {
            let fields: Vec<String> = [SCREEN, TOKEN, BODY]
                .iter()
                .map(|marker| String::from_utf8(representations(marker)[variant].clone()).unwrap())
                .collect();
            // Invalid SDP's first line can be quoted by a parser error. The
            // service must replace it with a fixed diagnostic, not log it.
            let sdp = format!("{} {} {} {picture}\r\n", fields[0], fields[1], fields[2]);
            let id = uuid::Uuid::new_v4().to_string();
            let (status, body) = post(
                host,
                "/offer",
                json!({"request_id":id,
                "type":"offer","codec":"h264","sdp":sdp}),
            )
            .await;
            assert_eq!(status, 400, "expected admitted negotiation to fail");
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap(),
                json!({"error":"WebRTC negotiation failed"})
            );
            // Wait for the session task's log write and cleanup receipt, not an
            // arbitrary sleep. This prevents a clean but prematurely read log.
            let (status, body) = post(host, "/close", json!({"request_id":id})).await;
            assert_eq!(status, 200);
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap()["closed"],
                true
            );
        }
        let log = fs::read(&stderr_path).unwrap();
        assert_clean(&log);
        let text = std::str::from_utf8(&log).unwrap();
        assert_eq!(
            text.matches("media session failed: remote description rejected")
                .count(),
            4,
            "all four requests must reach session negotiation and its log sink"
        );
        assert!(!text.contains("unexpected-encoder-start"));
        assert!(child.try_wait().unwrap().is_none());
        // Signal only the service this fixture spawned, using its installed
        // graceful-shutdown handler; kill_on_drop handles assertion failures.
        assert_eq!(
            unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGTERM) },
            0
        );
        assert!(child.wait().await.unwrap().success());
        assert_clean(&fs::read(&stderr_path).unwrap());
    })
    .await;
    if result.is_err() {
        child.kill().await.unwrap();
    }
    assert!(result.is_ok(), "media log fixture exceeded six seconds");
}
