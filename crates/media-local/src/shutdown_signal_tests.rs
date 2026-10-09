//! Each OS signal goes only to a freshly spawned, owned test process. The child
//! uses the real HTTP shutdown function with explicitly synthetic session state.
use super::*;
use crate::{serve_with_shutdown, App, Session, SessionBook};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{watch, Mutex};

#[test]
#[ignore = "isolated subprocess helper; run by actual_unix_signals_reconcile_bounded_shutdown"]
fn isolated_signal_child() {
    let directory = PathBuf::from(
        std::env::var("VEYNODE_SIGNAL_CHILD_DIR").expect("helper requires owned fixture"),
    );
    let mode = std::env::var("VEYNODE_SIGNAL_CHILD_MODE").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result=runtime.block_on(async {
        let shutdown=install().unwrap();
        let app=App {
            origin:"http://127.0.0.1:1".into(),host:"127.0.0.1:1".into(),encoder:PathBuf::from("never-executed-signal-fixture"),socket:PathBuf::new(),web:PathBuf::new(),capabilities:json!({}),fault_au_egress_hold:false,fault_drop_first_keyframe:false,fault_suppress_hevc_frames:false,active:Arc::new(Mutex::new(SessionBook::default())),auth:None,cleanup_failure_exit:None,guest_binding:None,encoder_rate:None,
        };
        let mut retained_sender=None;
        if mode.starts_with("active") {
            let (cancel,mut cancelled)=watch::channel(false);
            let (done,finished)=watch::channel(None);
            app.active.lock().await.active=Some(Session {request:"signal-request".into(),id:"signal-session".into(),cancel,finished,confirmed:watch::channel(false).0,input:None,files:None});
            let task_app=app.clone();let task_directory=directory.clone();let task_mode=mode.clone();
            if mode=="active_hung" {retained_sender=Some(done);}
            else {tokio::spawn(async move {
                cancelled.wait_for(|value| *value).await.unwrap();
                fs::write(task_directory.join("cancel-received"),b"yes").unwrap();
                let clean=task_mode=="active_clean";
                let mut book=task_app.active.lock().await;
                book.active=None;
                book.completed.insert("signal-request".into(),(Instant::now(),clean,"signal-session".into()));
                drop(book);done.send(Some(clean)).unwrap();
            });}
        } else if mode=="retained_failed" {
            app.active.lock().await.completed.insert("old-request".into(),(Instant::now(),false,"old-session".into()));
        } else {assert_eq!(mode,"idle");}
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        fs::write(directory.join("ready"),serde_json::to_vec(&json!({"pid":std::process::id(),"address":address.to_string()})).unwrap()).unwrap();
        let result=serve_with_shutdown(listener,axum::Router::new(),&app,shutdown,Duration::from_millis(180)).await;
        let accepting=tokio::net::TcpStream::connect(address).await.is_ok();
        let report=json!({"success":result.is_ok(),"error":result.as_ref().err().map(ToString::to_string),"stopping":app.active.lock().await.stopping,"listener_accepting":accepting,"cancel_received":directory.join("cancel-received").exists()});
        fs::write(directory.join("result"),serde_json::to_vec(&report).unwrap()).unwrap();
        drop(retained_sender);
        result
    });
    // Exit code intentionally tests success vs a failed cleanup, not signal death.
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
#[test]
fn actual_unix_signals_reconcile_bounded_shutdown() {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        for mode in [
            "idle",
            "active_clean",
            "active_failed",
            "retained_failed",
            "active_hung",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "shutdown_signal::tests::isolated_signal_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("VEYNODE_SIGNAL_CHILD_DIR", directory.path())
                .env("VEYNODE_SIGNAL_CHILD_MODE", mode)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut child = OwnedChild(child);
            let startup = Instant::now();
            while !directory.path().join("ready").exists() {
                assert!(
                    startup.elapsed() < Duration::from_secs(3),
                    "signal fixture ready timeout"
                );
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "signal fixture exited early"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            // File is small but its publication may still be in progress.
            let ready: Value = loop {
                if let Ok(value) =
                    serde_json::from_slice(&fs::read(directory.path().join("ready")).unwrap())
                {
                    break value;
                }
                assert!(startup.elapsed() < Duration::from_secs(3));
                std::thread::sleep(Duration::from_millis(5));
            };
            assert_eq!(ready["pid"], child.0.id());
            assert!(child.0.try_wait().unwrap().is_none());
            let began = Instant::now();
            assert_eq!(
                unsafe { libc::kill(child.0.id() as libc::pid_t, signal) },
                0
            );
            let status = loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    break status;
                }
                assert!(
                    began.elapsed() < Duration::from_secs(3),
                    "bounded signal cleanup failed {mode}"
                );
                std::thread::sleep(Duration::from_millis(5));
            };
            let success = matches!(mode, "idle" | "active_clean");
            assert_eq!(
                status.code(),
                Some(if success { 0 } else { 1 }),
                "signal={signal} mode={mode}"
            );
            let report: Value =
                serde_json::from_slice(&fs::read(directory.path().join("result")).unwrap())
                    .unwrap();
            assert_eq!(report["success"], success);
            assert_eq!(report["stopping"], true);
            assert_eq!(report["listener_accepting"], false);
            if matches!(mode, "active_clean" | "active_failed") {
                assert_eq!(report["cancel_received"], true);
            }
            if mode == "active_hung" {
                assert!(report["error"].as_str().unwrap().contains("deadline"));
            }
            if mode == "retained_failed" {
                assert!(report["error"]
                    .as_str()
                    .unwrap()
                    .contains("unresolved earlier cleanup"));
            }
        }
    }
}
