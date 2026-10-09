//! Private Unix protocol fixtures exercise the adapter and real crypto, not Guest input delivery.
use super::*;
use p256::ecdsa::{signature::Signer, SigningKey};
use protocol::{control::ControlRequest, Identity};
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, sync::Mutex as StdMutex};
use std::{os::unix::fs::MetadataExt, time::Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
struct TestKey(SigningKey);
impl TestKey {
    fn new(n: u8) -> Self {
        let mut v = [0; 32];
        v[31] = n;
        Self(SigningKey::from_bytes((&v).into()).unwrap())
    }
}
impl Identity for TestKey {
    fn public_key(&self) -> Result<String> {
        Ok(protocol::hex(
            self.0.verifying_key().to_encoded_point(false).as_bytes(),
        ))
    }
    fn sign(&self, b: &[u8]) -> Result<String> {
        let s: p256::ecdsa::Signature = self.0.sign(b);
        Ok(protocol::hex(&s.to_bytes()))
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    _console: UnixListener,
    task: tokio::task::JoinHandle<()>,
    session: Arc<SessionFiles>,
    key: TestKey,
    calls: Arc<StdMutex<Vec<Value>>>,
    reply_mode: Arc<std::sync::atomic::AtomicUsize>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().canonicalize().unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let console = UnixListener::bind(dir.join("console.sock")).unwrap();
        std::fs::set_permissions(
            dir.join("console.sock"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let listener = UnixListener::bind(dir.join("control.sock")).unwrap();
        std::fs::set_permissions(
            dir.join("control.sock"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let host = TestKey::new(1);
        let key = TestKey::new(2);
        let public = host.public_key().unwrap();
        let grant = protocol::SignedGrant::new(
            protocol::Grant {
                host_id: protocol::identity_id(&public).unwrap(),
                device_id: protocol::identity_id(&key.public_key().unwrap()).unwrap(),
                device_public_key: key.public_key().unwrap(),
                vm_id: "mac-files-test".into(),
                scopes: vec!["files".into(), "observe".into()],
                version: 1,
            },
            &host,
        )
        .unwrap();
        let state = dir.join("registry");
        protocol::store::Store::open(&state)
            .unwrap()
            .save(&protocol::store::Registry {
                host_public_key: public.clone(),
                grants: BTreeMap::from([(grant.grant.key(), grant.clone())]),
                versions: BTreeMap::from([(grant.grant.key(), 1)]),
            })
            .unwrap();
        let auth = Arc::new(Auth {
            ice_config: None,
            test_identity: None,
            registry_read: std::sync::Mutex::new(()),
            host_public_key: public.clone(),
            state,
            namespace: "never-keychain".into(),
            vm_id: grant.grant.vm_id.clone(),
            gate: protocol::SessionGate::new(&public, std::slice::from_ref(&grant)).unwrap(),
            control_state: None,
            files_enabled: true,
            close_proofs: StdMutex::new(BTreeMap::new()),
        });
        let sdp = format!(
            "v=0\r\na=fingerprint:sha-256 {}\r\nm=video 9 UDP/TLS/RTP/SAVPF 98\r\n",
            vec!["11"; 32].join(":")
        );
        let intent = protocol::OfferIntent {
            challenge: auth
                .gate
                .challenge(
                    &grant.grant.device_id,
                    &grant.grant.vm_id,
                    grant.grant.scopes.clone(),
                    now().unwrap(),
                )
                .unwrap(),
            request_id: uuid::Uuid::new_v4().to_string(),
            codec: "h265".into(),
            offer: protocol::SdpBinding::from_sdp(&sdp).unwrap(),
        };
        let offer = protocol::SignedOffer {
            signature: key.sign(&intent.canonical().unwrap()).unwrap(),
            intent,
        };
        let accepted = auth
            .gate
            .accept_offer(&offer, &sdp, now().unwrap())
            .unwrap();
        let answer = auth
            .gate
            .answer(
                accepted,
                uuid::Uuid::new_v4().to_string(),
                &sdp,
                &host,
                now().unwrap(),
            )
            .unwrap();
        let digest = answer.transcript.digest().unwrap();
        auth.gate
            .confirm(
                &answer.transcript.session_id,
                &protocol::Confirmation {
                    signature: key
                        .sign(&protocol::confirmation_bytes(&digest).unwrap())
                        .unwrap(),
                    transcript_sha256: digest,
                },
                now().unwrap(),
            )
            .unwrap();
        auth.close_proofs.lock().unwrap().insert(
            answer.transcript.session_id.clone(),
            super::super::CloseProof {
                transcript: answer.transcript.clone(),
                device_public_key: key.public_key().unwrap(),
                issued: Instant::now(),
            },
        );
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let seen = calls.clone();
        let reply_mode = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mode = reply_mode.clone();
        let meta = std::fs::metadata(dir.join("console.sock")).unwrap();
        let binding = json!({"version":1,"vm_id":"mac-files-test","worker_instance":"b".repeat(64),"bridge_instance":"c".repeat(64),"worker_pid":123,"socket_path":dir.join("console.sock"),"socket_device":meta.dev(),"socket_inode":meta.ino(),"guest_port":5001,"scope":"host-worker-listener-only","input_authorized":false});
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let seen = seen.clone();
                let mode = mode.clone();
                let binding = binding.clone();
                tokio::spawn(async move {
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let req: Value = serde_json::from_str(&line).unwrap();
                    seen.lock().unwrap().push(req.clone());
                    let response = match req["method"].as_str().unwrap() {
                        "guest_stream_bridge_binding" => {
                            let mut binding = binding;
                            if mode.load(Ordering::SeqCst) == 3 {
                                binding["worker_instance"] = json!("d".repeat(64));
                            }
                            json!({"result":{"binding":binding}})
                        }
                        "capabilities" => {
                            json!({"result":{"control_session_fencing":{"retired_owner_ttl_seconds":120,"request_dispatch_deadline_seconds":30}}})
                        }
                        "guest_info" => {
                            json!({"result":{"id":"f","result":{"console_identity":{"version":1,"security_session_id":42,"uid":501,"console_set":null,"display_id":1,"physical_width":2560,"physical_height":1440,"display_geometry_sha256":"a".repeat(64)},"console_identity_status":{"version":1,"identity_eligible":true},"capabilities":{"files":true},"file_policy":{"max_file_bytes":8388608,"chunk_bytes":32768,"idle_timeout_seconds":60,"absolute_timeout_seconds":300,"path_scope":"single filename","ownership":"same UID","root":"SECRET_PATH"}}}})
                        }
                        "file_upload_begin" => {
                            if mode.load(Ordering::SeqCst) == 4 {
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                            json!({"result":{"id":"f","result":{"token":"a".repeat(32),"size":req["arguments"]["size"],"sha256":req["arguments"]["sha256"],"chunk_bytes":32768,"idle_timeout_seconds":60}}})
                        }
                        "file_cancel" => {
                            json!({"error":"guest: {\"code\":\"not_found\",\"message\":\"SECRET_PATH\"}"})
                        }
                        "session_end" => {
                            tokio::time::sleep(Duration::from_millis(40)).await;
                            let mut result = cleanup();
                            if mode.load(Ordering::SeqCst) == 2 {
                                result["file_cancellation"]["outcomes"][0] =
                                    json!({"error":"failed"});
                            }
                            json!({"result":result})
                        }
                        _ => panic!("unexpected fixture method"),
                    };
                    let _ = stream
                        .get_mut()
                        .write_all(format!("{response}\n").as_bytes())
                        .await;
                });
            }
        });
        let (options, _) = crate::guest_binding::parse_options(&[
            "--guest-binding-state".into(),
            dir.to_str().unwrap().into(),
            "--guest-binding-vm".into(),
            "mac-files-test".into(),
            "--guest-worker-instance".into(),
            "b".repeat(64),
            "--guest-bridge-instance".into(),
            "c".repeat(64),
        ])
        .unwrap();
        let source = Arc::new(
            Source::open(
                options.unwrap(),
                &dir.join("console.sock"),
                "mac-files-test",
                false,
                true,
                &json!({"console_identity_binding_version":1}),
            )
            .await
            .unwrap(),
        );
        let session = SessionFiles::attach(auth, &answer.transcript.session_id, source)
            .await
            .unwrap();
        Self {
            _dir: directory,
            _console: console,
            task,
            session,
            key,
            calls,
            reply_mode,
        }
    }
    fn signed(&self, sequence: u64, action: FileAction) -> SignedControl {
        let request = ControlRequest {
            session_id: self.session.session_id.clone(),
            transcript_sha256: self.session.transcript.clone(),
            sequence,
            expires_at: now().unwrap() + 5,
            action: ControlAction::File { operation: action },
        };
        SignedControl {
            signature: self.key.sign(&request.canonical().unwrap()).unwrap(),
            request,
        }
    }
    fn count(&self, method: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == method)
            .count()
    }
}
fn cleanup() -> Value {
    json!({"closed":true,"retired_owner":true,"retired_owner_ttl_seconds":120,"job_cancellation":{"attempted":0,"unconfirmed_requests":0,"outcomes":[]},"file_cancellation":{"attempted":1,"unconfirmed_requests":0,"outcomes":[{"result":{"id":"f","result":{"cancelled_transfers":1}}}]}})
}
#[tokio::test]
async fn files_need_no_input_lease_and_errors_are_sanitized() {
    let f = Fixture::new().await;
    assert!(f.session.auth.control_state.is_none());
    let info = f
        .session
        .handle(f.signed(1, FileAction::Info))
        .await
        .unwrap();
    assert!(!info.to_string().contains("SECRET_PATH"));
    let begin = f
        .session
        .handle(f.signed(
            2,
            FileAction::UploadBegin {
                path: "ok.txt".into(),
                size: 3,
                sha256: protocol::hash(b"abc"),
            },
        ))
        .await
        .unwrap();
    assert_eq!(begin["result"]["value"]["size"], 3);
    let missing = f
        .session
        .handle(f.signed(
            3,
            FileAction::Cancel {
                token: "a".repeat(32),
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        missing["result"],
        json!({"ok":false,"error":{"code":"not_found"}})
    );
    assert!(f
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|v| v["method"] != "acquire_control"));
    f.session.end().await.unwrap();
    assert_eq!(f.count("session_end"), 1);
}
#[tokio::test]
async fn replay_foreign_session_revocation_and_changed_worker_fail_before_file_dispatch() {
    let f = Fixture::new().await;
    let signed = f.signed(1, FileAction::Info);
    f.session.handle(signed.clone()).await.unwrap();
    assert!(f.session.handle(signed).await.is_err());
    let mut foreign = f.signed(2, FileAction::Info);
    foreign.request.session_id = "other".into();
    assert!(f.session.handle(foreign).await.is_err());
    f.reply_mode.store(3, Ordering::SeqCst);
    assert!(f
        .session
        .handle(f.signed(
            3,
            FileAction::UploadBegin {
                path: "x".into(),
                size: 0,
                sha256: protocol::hash(b"")
            }
        ))
        .await
        .is_err());
    assert_eq!(f.count("file_upload_begin"), 0);
    f.session.auth.gate.close(&f.session.session_id);
    assert!(f
        .session
        .handle(f.signed(4, FileAction::Info))
        .await
        .is_err());
    f.session.end().await.unwrap();
}
#[tokio::test]
async fn failed_cleanup_is_sticky_and_cancelled_waiter_does_not_repeat_it() {
    let f = Fixture::new().await;
    f.reply_mode.store(2, Ordering::SeqCst);
    let s = f.session.clone();
    let task = tokio::spawn(async move { s.end().await });
    while f.count("session_end") == 0 {
        tokio::task::yield_now().await;
    }
    task.abort();
    tokio::time::sleep(Duration::from_millis(60)).await;
    f.reply_mode.store(0, Ordering::SeqCst);
    assert!(f.session.end().await.is_err());
    assert!(f.session.end().await.is_err());
    assert_eq!(f.count("session_end"), 1);
}
#[tokio::test]
async fn end_fences_inflight_and_releases_only_this_owner() {
    let f = Fixture::new().await;
    f.reply_mode.store(4, Ordering::SeqCst);
    let s = f.session.clone();
    let signed = f.signed(
        1,
        FileAction::UploadBegin {
            path: "x".into(),
            size: 0,
            sha256: protocol::hash(b""),
        },
    );
    let task = tokio::spawn(async move { s.handle(signed).await });
    while f.count("file_upload_begin") == 0 {
        tokio::task::yield_now().await;
    }
    f.session.end().await.unwrap();
    assert!(task.await.unwrap().is_err());
    assert!(f
        .session
        .handle(f.signed(2, FileAction::Info))
        .await
        .is_err());
    let calls = f.calls.lock().unwrap();
    let owner = &calls
        .iter()
        .find(|v| v["method"] == "file_upload_begin")
        .unwrap()["arguments"]["owner"];
    assert_eq!(
        owner,
        &calls.iter().find(|v| v["method"] == "session_end").unwrap()["arguments"]["owner"]
    );
}
#[test]
fn cleanup_and_success_schemas_do_not_accept_empty_ack() {
    assert!(validate_cleanup(&cleanup()).is_ok());
    let mut malformed = cleanup();
    malformed["file_cancellation"]["outcomes"][0] = json!({"result":{"result":{}}});
    assert!(validate_cleanup(&malformed).is_err());
    assert!(normalize(
        &FileAction::Cancel {
            token: "a".repeat(32)
        },
        json!({"result":{"result":{"cancelled":false}}})
    )
    .is_err());
}
#[tokio::test]
async fn registry_revocation_denies_files_even_with_a_valid_device_signature() {
    let f = Fixture::new().await;
    let store = protocol::store::Store::open(&f.session.auth.state).unwrap();
    let mut registry = store.load().unwrap();
    let grant = registry.grants.values().next().unwrap().grant.clone();
    registry.revoke(&grant.device_id, &grant.vm_id).unwrap();
    store.save(&registry).unwrap();
    drop(store);
    assert!(f
        .session
        .handle(f.signed(
            1,
            FileAction::UploadBegin {
                path: "revoked".into(),
                size: 0,
                sha256: protocol::hash(b"")
            }
        ))
        .await
        .is_err());
    assert_eq!(f.count("file_upload_begin"), 0);
    f.session.end().await.unwrap();
}
#[test]
fn backend_errors_do_not_leak_paths_or_masquerade_as_business_results() {
    let action = FileAction::Cancel {
        token: "a".repeat(32),
    };
    assert!(normalize(&action, json!({"error":"backend: PRIVATE_PATH"})).is_err());
    assert!(normalize(&action, json!({"error":"guest: malformed PRIVATE_PATH"})).is_err());
    assert_eq!(
        normalize(
            &action,
            json!({"error":"guest: {\"code\":\"unknown\",\"message\":\"PRIVATE_PATH\"}"})
        )
        .unwrap(),
        json!({"ok":false,"error":{"code":"io_error"}})
    );
    assert!(normalize(
        &FileAction::DownloadRead {
            token: "a".repeat(32),
            offset: 0
        },
        json!({"result":{"result":{"data":"YWJj","offset":2,"eof":true,"sha256":"a".repeat(64)}}})
    )
    .is_err());
}
