//! Scripted protocol/transaction tests, never a real VM acceptance substitute.
use super::*;
use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    os::unix::fs::OpenOptionsExt,
};

struct Peer {
    replies: VecDeque<(&'static str, Value)>,
    sent: Vec<(String, Value)>,
    fdset: i64,
    fail_start: bool,
}
impl Peer {
    fn new(replies: Vec<(&'static str, Value)>) -> Self {
        Self {
            replies: replies.into(),
            sent: vec![],
            fdset: 16,
            fail_start: false,
        }
    }
    fn done(&self) {
        assert!(
            self.replies.is_empty(),
            "unconsumed replies: {:?}",
            self.replies
        );
    }
}
impl Qmp for Peer {
    fn command(&mut self, method: &str, args: Value) -> Result<Value> {
        self.sent.push((method.to_owned(), args));
        if method == "migrate" && self.fail_start {
            bail!("injected lost migration ACK");
        }
        let (expected, result) = self.replies.pop_front().expect("unexpected QMP call");
        assert_eq!(method, expected);
        Ok(result)
    }
    fn add_fd(&mut self, _: &File) -> Result<i64> {
        Ok(self.fdset)
    }
}
fn commands() -> Value {
    json!([
        "add-fd",
        "remove-fd",
        "query-fdsets",
        "query-status",
        "query-migrate",
        "query-migrate-capabilities",
        "query-migrate-parameters",
        "migrate",
        "migrate-incoming",
        "migrate_cancel",
        "cont",
        "stop"
    ]
    .map(|name| json!({"name":name})))
}
fn preamble(incoming: bool) -> Vec<(&'static str, Value)> {
    vec![
        ("query-commands", commands()),
        (
            "query-migrate-capabilities",
            json!([{"capability":"events","state":true},{"capability":"release-ram","state":false}]),
        ),
        (
            "query-migrate-parameters",
            json!({"mode":"normal","tls-creds":""}),
        ),
        ("query-migrate", json!({})),
        (
            "query-status",
            json!({"running":false,"status":if incoming {"inmigrate"} else {"paused"}}),
        ),
        ("query-fdsets", json!([{"fdset-id":16,"fds":[{"fd":10}]}])),
    ]
}
fn capture_file() -> (tempfile::TempDir, File) {
    let dir = tempfile::tempdir().unwrap();
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.path().join("state"))
        .unwrap();
    (dir, file)
}
fn incoming_file() -> (tempfile::TempDir, File) {
    let (dir, write) = capture_file();
    write.write_at(b"QEVM\0\0\0\x03", 0).unwrap();
    drop(write);
    let read = File::open(dir.path().join("state")).unwrap();
    (dir, read)
}
fn capture(peer: &mut Peer, file: &File, now: Instant) -> Capture {
    Capture::prepare(peer, Profile::Virtio2d, file, Duration::from_secs(30), now).unwrap()
}

#[test]
fn accelerated_profiles_refuse_without_any_qmp_mutation() {
    for profile in [Profile::Virgl, Profile::VenusMoltenvkExperimental] {
        let mut peer = Peer::new(vec![]);
        assert!(preflight(&mut peer, profile)
            .unwrap_err()
            .to_string()
            .contains("GPU state is not migratable"));
        assert!(peer.sent.is_empty());
    }
}
#[test]
fn runtime_blocker_is_independent_of_profile() {
    let mut sequence = preamble(false);
    sequence.truncate(4);
    sequence[3].1 = json!({"blocked-reasons":["unmigratable-device"]});
    let mut peer = Peer::new(sequence);
    assert!(preflight(&mut peer, Profile::Virtio2d)
        .unwrap_err()
        .to_string()
        .contains("unmigratable-device"));
    peer.done();
}
#[test]
fn enabled_unsafe_capability_and_malformed_capability_refuse() {
    for cap in [
        json!([{"capability":"release-ram","state":true}]),
        json!([{"capability":"events","state":null}]),
        json!([{"capability":"events","state":false},{"capability":"events","state":false}]),
    ] {
        let mut peer = Peer::new(vec![
            ("query-commands", commands()),
            ("query-migrate-capabilities", cap),
        ]);
        assert!(preflight(&mut peer, Profile::Virtio2d).is_err());
        peer.done();
    }
}
#[test]
fn missing_command_cpr_mode_and_active_migration_refuse() {
    let mut peer = Peer::new(vec![("query-commands", json!([]))]);
    assert!(preflight(&mut peer, Profile::Virtio2d).is_err());
    for (index, replacement) in [
        (2, json!({"mode":"cpr-transfer"})),
        (3, json!({"status":"active"})),
    ] {
        let mut sequence = preamble(false);
        sequence.truncate(index + 1);
        sequence[index].1 = replacement;
        let mut peer = Peer::new(sequence);
        assert!(preflight(&mut peer, Profile::Virtio2d).is_err());
        peer.done();
    }
}
#[test]
fn state_fd_requires_exact_access_mode_and_empty_capture_target() {
    let (dir, file) = capture_file();
    let rw = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.path().join("state"))
        .unwrap();
    assert!(check_file(&rw, false)
        .unwrap_err()
        .to_string()
        .contains("access mode"));
    file.write_at(b"old snapshot", 0).unwrap();
    assert!(check_file(&file, false)
        .unwrap_err()
        .to_string()
        .contains("new empty"));
}
#[test]
fn state_fd_rejects_shared_links_and_public_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, file) = capture_file();
    fs::hard_link(dir.path().join("state"), dir.path().join("alias")).unwrap();
    assert!(check_file(&file, false).is_err());
    fs::remove_file(dir.path().join("alias")).unwrap();
    fs::set_permissions(dir.path().join("state"), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(check_file(&file, false).is_err());
}
#[test]
fn fdset_missing_duplicate_or_merged_refuse() {
    for value in [
        json!([]),
        json!([{"fdset-id":16,"fds":[{"fd":10}]},{"fdset-id":16,"fds":[{"fd":11}]}]),
        json!([{"fdset-id":16,"fds":[{"fd":10},{"fd":11}]}]),
    ] {
        let mut peer = Peer::new(vec![("query-fdsets", value)]);
        assert!(verify_fdset(&mut peer, 16).is_err());
        peer.done();
    }
    assert!(verify_fdset(&mut Peer::new(vec![]), 1).is_err());
}
#[test]
fn capture_ack_pending_and_finish_migrate_are_not_disk_capture() {
    let (_dir, file) = capture_file();
    let now = Instant::now();
    let mut sequence = preamble(false);
    sequence.extend([
        ("migrate", json!({})),
        ("query-migrate", json!({"status":"active"})),
        ("query-migrate", json!({"status":"completed"})),
        (
            "query-status",
            json!({"running":false,"status":"finish-migrate"}),
        ),
    ]);
    let mut peer = Peer::new(sequence);
    let mut op = capture(&mut peer, &file, now);
    op.start(&mut peer).unwrap();
    assert_eq!(peer.sent.last().unwrap().1, file_channel(16));
    assert!(op.resume_source(&mut peer).is_err());
    assert!(op.release_channel(&mut peer).is_err());
    assert_eq!(op.poll(&mut peer, now).unwrap(), Progress::Pending);
    assert_eq!(op.poll(&mut peer, now).unwrap(), Progress::Pending);
    assert!(op.resume_source(&mut peer).is_err());
    peer.done();
}
#[test]
fn completed_capture_requires_postmigrate_then_explicit_resume_and_release() {
    let (_dir, file) = capture_file();
    let now = Instant::now();
    let mut sequence = preamble(false);
    sequence.extend([
        ("migrate", json!({})),
        ("query-migrate", json!({"status":"completed"})),
        (
            "query-status",
            json!({"running":false,"status":"postmigrate"}),
        ),
        (
            "query-status",
            json!({"running":false,"status":"postmigrate"}),
        ),
        ("cont", json!({})),
        ("query-status", json!({"running":true,"status":"running"})),
        ("query-fdsets", json!([{"fdset-id":16,"fds":[{"fd":10}]}])),
        ("remove-fd", json!({})),
    ]);
    let mut peer = Peer::new(sequence);
    let mut op = capture(&mut peer, &file, now);
    op.start(&mut peer).unwrap();
    file.write_at(b"QEVM\0\0\0\x03", 0).unwrap();
    assert_eq!(op.poll(&mut peer, now).unwrap(), Progress::CapturedPaused);
    assert!(!peer.sent.iter().any(|v| v.0 == "cont"));
    // Caller is responsible for the actual disk clone/durable commit here.
    op.resume_source(&mut peer).unwrap();
    op.release_channel(&mut peer).unwrap();
    assert_eq!(peer.sent.last().unwrap().1, json!({"fdset-id":16}));
    assert!(op.release_channel(&mut peer).is_err());
    peer.done();
}
#[test]
fn completed_capture_running_or_empty_artifact_refuses() {
    for state in [
        json!({"running":true,"status":"running"}),
        json!({"running":false,"status":"postmigrate"}),
    ] {
        let (_dir, file) = capture_file();
        let now = Instant::now();
        let mut sequence = preamble(false);
        sequence.extend([
            ("migrate", json!({})),
            ("query-migrate", json!({"status":"completed"})),
            ("query-status", state),
        ]);
        let mut peer = Peer::new(sequence);
        let mut op = capture(&mut peer, &file, now);
        op.start(&mut peer).unwrap();
        assert!(op.poll(&mut peer, now).is_err());
        assert!(op.resume_source(&mut peer).is_err());
        peer.done();
    }
}
#[test]
fn timeout_requires_explicit_bounded_cancellation_and_preserves_fence() {
    let (_dir, file) = capture_file();
    let now = Instant::now();
    let mut sequence = preamble(false);
    sequence.extend([
        ("migrate", json!({})),
        ("migrate_cancel", json!({})),
        ("migrate_cancel", json!({})),
        ("query-migrate", json!({"status":"cancelled"})),
        ("query-status", json!({"running":false,"status":"paused"})),
    ]);
    let mut peer = Peer::new(sequence);
    let mut op = capture(&mut peer, &file, now);
    op.start(&mut peer).unwrap();
    assert!(op.poll(&mut peer, now + Duration::from_secs(30)).is_err());
    assert!(op.resume_source(&mut peer).is_err());
    op.request_cancel(&mut peer, now + Duration::from_secs(31))
        .unwrap();
    op.request_cancel(&mut peer, now + Duration::from_secs(35))
        .unwrap();
    assert_eq!(op.0.deadline, now + Duration::from_secs(41));
    assert_eq!(
        op.poll(&mut peer, now + Duration::from_secs(36)).unwrap(),
        Progress::Cancelled
    );
    assert!(!peer.sent.iter().any(|v| v.0 == "cont"));
    peer.done();
}
#[test]
fn failed_ack_does_not_discard_capture_ownership() {
    let (_dir, file) = capture_file();
    let now = Instant::now();
    let mut sequence = preamble(false);
    sequence.push(("query-migrate", json!({"status":"active"})));
    let mut peer = Peer::new(sequence);
    peer.fail_start = true;
    let mut op = capture(&mut peer, &file, now);
    assert!(op.start(&mut peer).is_err());
    assert_eq!(op.poll(&mut peer, now).unwrap(), Progress::Pending);
    assert!(op.release_channel(&mut peer).is_err());
    peer.done();
}
#[test]
fn failed_transfer_is_typed_and_never_auto_resumes() {
    let (_dir, file) = capture_file();
    let now = Instant::now();
    let mut sequence = preamble(false);
    sequence.extend([
        ("migrate", json!({})),
        (
            "query-migrate",
            json!({"status":"failed","error-desc":"disk full"}),
        ),
        ("query-status", json!({"running":false,"status":"paused"})),
    ]);
    let mut peer = Peer::new(sequence);
    let mut op = capture(&mut peer, &file, now);
    op.start(&mut peer).unwrap();
    assert_eq!(
        op.poll(&mut peer, now).unwrap(),
        Progress::Failed {
            reason: "disk full".into()
        }
    );
    assert!(!peer.sent.iter().any(|v| v.0 == "cont"));
    peer.done();
}
#[test]
fn incoming_validates_header_before_channel_registration() {
    let (dir, read) = incoming_file();
    check_file(&read, true).unwrap();
    fs::write(dir.path().join("state"), b"QEVM\0\0\0\x02").unwrap();
    assert!(check_file(&read, true)
        .unwrap_err()
        .to_string()
        .contains("header"));
}
#[test]
fn incoming_restores_paused_and_only_explicitly_resumes() {
    let (_dir, file) = incoming_file();
    let now = Instant::now();
    let mut sequence = preamble(true);
    sequence.extend([
        ("migrate-incoming", json!({})),
        ("query-migrate", json!({"status":"completed"})),
        ("query-status", json!({"running":false,"status":"paused"})),
        ("query-status", json!({"running":false,"status":"paused"})),
        ("cont", json!({})),
        ("query-status", json!({"running":true,"status":"running"})),
    ]);
    let mut peer = Peer::new(sequence);
    let mut op = Incoming::prepare(
        &mut peer,
        Profile::Virtio2d,
        &file,
        Duration::from_secs(30),
        now,
    )
    .unwrap();
    assert!(op.resume_destination(&mut peer).is_err());
    op.start(&mut peer).unwrap();
    let args = &peer.sent.last().unwrap().1;
    assert_eq!(args["exit-on-error"], false);
    assert_eq!(args["channels"], file_channel(16)["channels"]);
    assert_eq!(op.poll(&mut peer, now).unwrap(), Progress::RestoredPaused);
    assert!(!peer.sent.iter().any(|v| v.0 == "cont"));
    op.resume_destination(&mut peer).unwrap();
    peer.done();
}
#[test]
fn unknown_postcopy_or_missing_start_state_is_not_success() {
    for status in [json!({"status":"postcopy-active"}), json!({})] {
        let (_dir, file) = capture_file();
        let now = Instant::now();
        let mut sequence = preamble(false);
        sequence.extend([("migrate", json!({})), ("query-migrate", status)]);
        let mut peer = Peer::new(sequence);
        let mut op = capture(&mut peer, &file, now);
        op.start(&mut peer).unwrap();
        assert!(op.poll(&mut peer, now).is_err());
        assert!(op.resume_source(&mut peer).is_err());
        peer.done();
    }
}
