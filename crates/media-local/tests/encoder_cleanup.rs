//! Actual shim/child and OS pipes; synthetic broker/AUs, no VM or TURN proof.
#[path = "../src/encoder_cleanup.rs"]
mod encoder_cleanup;
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::Command,
};

async fn record(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    kind: u8,
    body: &[u8],
) -> std::io::Result<()> {
    writer.write_all(&(body.len() as u32).to_be_bytes()).await?;
    writer.write_all(&[kind]).await?;
    writer.write_all(&1u64.to_be_bytes()).await?;
    writer.write_all(body).await
}

async fn shim_case(drain: bool) {
    let directory = tempfile::Builder::new()
        .prefix("vm-clean-")
        .tempdir_in("/tmp")
        .unwrap();
    let path = directory.path().join("b.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let broker = tokio::spawn(async move {
        let work = async {
            let (socket, _) = listener.accept().await?;
            let (read, mut write) = socket.into_split();
            let mut lines = BufReader::new(read).lines();
            let start: Value = serde_json::from_str(&lines.next_line().await?.unwrap()).unwrap();
            assert_eq!(start["method"], "start");
            record(
                &mut write,
                1,
                &serde_json::to_vec(&json!({"request_id":"start","result":{"epoch":1}})).unwrap(),
            )
            .await?;
            let mut body = vec![1; 1024 * 1024 + 9];
            body[..8].copy_from_slice(&0u64.to_be_bytes());
            record(&mut write, 2, &body).await?;
            // The first AU blocks the shim's stdout without a reader. Further
            // paced records exhaust its actual four-event queue without a
            // drain; this must fail media without destroying the stop-ACK path.
            let mut tick = tokio::time::interval(Duration::from_millis(30));
            loop {
                tokio::select! {
                    line = lines.next_line() => {
                        let Some(line) = line? else { return Ok::<bool, std::io::Error>(false); };
                        let request:Value=serde_json::from_str(&line).unwrap();
                        if request["method"] == "stop" {
                            record(&mut write,1,&serde_json::to_vec(&json!({"request_id":"stop","result":{"state":"stopped"}})).unwrap()).await?;
                            // Keep the socket open until the shim consumes ACK.
                            let _ = lines.next_line().await;
                            return Ok(true);
                        }
                    }
                    _ = tick.tick() => {
                        record(&mut write,2,&body[..65536]).await?;
                    }
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(4), work)
            .await
            .unwrap()
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_vm-console-stream"))
        .arg(path)
        .args(["--codec", "h264"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut output = child.stdout.take().unwrap();
    let input = child.stdin.take().unwrap();
    // Only consume the 13-byte header. An AU is actively pending in the pipe.
    let mut header = [0; 13];
    tokio::time::timeout(Duration::from_secs(2), output.read_exact(&mut header))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        u32::from_be_bytes(header[..4].try_into().unwrap()),
        1024 * 1024
    );
    let outcome = if drain {
        encoder_cleanup::stop_and_wait(&mut child, input, &mut output).await
    } else {
        // Exercise wait-only teardown too: the shim's own output deadline must
        // terminate this partial AU but still request and receive its stop ACK.
        // The original frozen shim failed here; that result remains archived.
        let mut input = input;
        input.write_all(b"stop\n").await.unwrap();
        drop(input);
        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .unwrap()
            .unwrap();
        encoder_cleanup::Outcome {
            graceful_status: Some(status),
            child_reaped: true,
        }
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .await
        .unwrap();
    let broker_result = broker.await.unwrap();
    assert!(outcome.child_reaped);
    let status = outcome.graceful_status.unwrap();
    if drain {
        assert!(status.success(), "actual shim failed: {stderr}");
        assert!(broker_result.unwrap(), "stop ACK not sent");
    } else {
        assert!(
            status.success(),
            "bounded media failure lost its stop ACK: {stderr}"
        );
        assert!(
            stderr.contains("console media ended: media pipe write deadline"),
            "unexpected error: {stderr}"
        );
        assert!(broker_result.unwrap(), "stop ACK not sent");
        let mut remaining = Vec::new();
        output.read_to_end(&mut remaining).await.unwrap();
        assert!(
            !remaining.is_empty() && remaining.len() < 1024 * 1024,
            "wait-only case must retain a partial AU, not successful video"
        );
    }
    eprintln!(
        "actual_shim drain={drain} child_exit={:?} cleanup_ack=true",
        status.code()
    );
}

#[tokio::test]
async fn actual_shim_wait_only_bounds_partial_video_and_preserves_stop_ack() {
    shim_case(false).await;
}
#[tokio::test]
async fn actual_shim_draining_pending_frames_preserves_stop_ack() {
    shim_case(true).await;
}

async fn child_case(script: &str) -> encoder_cleanup::Outcome {
    let mut child = Command::new("python3")
        .args(["-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    encoder_cleanup::stop_and_wait(&mut child, input, &mut output).await
}
#[tokio::test]
async fn large_output_child_can_finish_stop_without_pipe_deadlock() {
    let outcome=child_case("import sys; sys.stdout.buffer.write(b'x'*(4*1024*1024)); sys.stdout.buffer.flush(); assert sys.stdin.readline()=='stop\\n'").await;
    assert!(outcome.child_reaped);
    assert!(outcome.graceful_status.unwrap().success());
}
#[tokio::test]
async fn stdout_eof_is_not_cleanup_and_timeout_kill_is_not_graceful() {
    let began = std::time::Instant::now();
    let outcome = child_case("import os,time; os.close(1); time.sleep(20)").await;
    assert!(outcome.child_reaped);
    assert!(outcome.graceful_status.is_none());
    assert!(began.elapsed() >= Duration::from_secs(2));
    assert!(began.elapsed() < Duration::from_secs(4));
}
#[tokio::test]
async fn drained_output_does_not_replace_nonzero_exit_with_success() {
    let outcome=child_case("import sys; sys.stdout.buffer.write(b'x'*(4*1024*1024)); sys.stdout.buffer.flush(); sys.exit(17)").await;
    assert!(outcome.child_reaped);
    assert_eq!(outcome.graceful_status.unwrap().code(), Some(17));
}
