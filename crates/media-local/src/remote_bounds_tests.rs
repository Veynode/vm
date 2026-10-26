//! Numeric bounds on the remote offer, ICE and control-channel paths. Named constants are read
//! directly. Inline literals are pinned through the behaviour the code already exposes, and none
//! were refactored. A change to any value here must fail a test and be decided on purpose.
//! Synthetic clocks, owner-only temp files and in-memory state only: no sleeps, sockets, encoder
//! or peer.
use super::{
    media_session_cap_elapsed, IceSource, MEDIA_SESSION_CAP_SECONDS, SESSION_END_MARGIN_SECONDS,
};
use crate::control_channel::{CHUNK_BYTES, HEADER_BYTES, JSON_BYTES, SEND_BUFFER_BYTES};
use axum::{body::to_bytes, http::StatusCode, response::Response};
use serde_json::json;
use std::{
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

/// Owner-only regular file, the only kind `read_private` accepts (ice_config.rs:144-158).
fn private_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
    path
}

/// A credential issued at 100 and expiring at `expires_at`, with no servers.
fn credential(expires_at: u64) -> Vec<u8> {
    format!(
        r#"{{"version":1,"transport_policy":"all","issued_at":100,"expires_at":{expires_at},"ice_servers":[]}}"#
    )
    .into_bytes()
}

fn source_expiring_at(dir: &Path, expires_at: u64) -> IceSource {
    IceSource::open(private_file(dir, "ice.json", &credential(expires_at)), 100).unwrap()
}

/// A loopback App with no authentication, no ICE source and no encoder. Each test sets `stopping`
/// so that an offer which passes the gates cannot start a session.
fn test_app() -> crate::App {
    crate::App {
        origin: "http://127.0.0.1:8787".into(),
        host: "127.0.0.1:8787".into(),
        encoder: PathBuf::from("must-not-start-encoder"),
        socket: PathBuf::new(),
        web: PathBuf::new(),
        capabilities: json!({}),
        fault_au_egress_hold: false,
        fault_drop_first_keyframe: false,
        fault_suppress_hevc_frames: false,
        active: Arc::new(Mutex::new(crate::SessionBook::default())),
        auth: None,
        cleanup_failure_exit: None,
        guest_binding: None,
        encoder_rate: None,
    }
}

fn offer(codec: &str, sdp: String) -> crate::Offer {
    crate::Offer {
        request_id: uuid::Uuid::new_v4().to_string(),
        sdp,
        r#type: "offer".into(),
        codec: codec.into(),
    }
}

async fn text(response: Response) -> (StatusCode, String) {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// SESSION_END_MARGIN_SECONDS (ice_config.rs:23) ends an admitted session 30 s before its
/// credential expires (ice_config.rs:79). The doc comment's relation, 210 - 30 = 180, is checked
/// arithmetically, now against the named `MEDIA_SESSION_CAP_SECONDS` that run_session consumes
/// (`media_session_cap_is_the_named_constant_and_run_session_uses_it` pins that wiring).
#[test]
fn session_ends_thirty_seconds_before_credential_expiry() {
    assert_eq!(SESSION_END_MARGIN_SECONDS, 30);
    assert_eq!(210 - SESSION_END_MARGIN_SECONDS, 180);
    assert_eq!(210 - SESSION_END_MARGIN_SECONDS, MEDIA_SESSION_CAP_SECONDS);
    let dir = tempfile::tempdir().unwrap();
    let source = source_expiring_at(dir.path(), 700);
    source.admit_offer("offer", 100).unwrap();
    source.bind_session("offer", "session").unwrap();
    assert!(source.check_session("session", 669).is_ok());
    assert!(source.check_session("session", 670).is_err());
}

/// Minimum credential time left at admission: 210 s. The check lives in session-auth (ice.rs:90)
/// and is reached through `IceSource::read`. 210 s remaining is admitted; 209 s and expiry are
/// refused before any session slot exists.
#[test]
fn admission_needs_two_hundred_ten_seconds_of_credential_left() {
    let dir = tempfile::tempdir().unwrap();
    let source = source_expiring_at(dir.path(), 700);
    source.admit_offer("at-210", 490).unwrap();
    assert_eq!(
        source.admit_offer("at-209", 491).err().unwrap().to_string(),
        "ICE credentials expired or insufficient lifetime"
    );
    assert!(source.admit_offer("expired", 700).is_err());
}

/// Credential file size: at most 16 KiB (ice_config.rs:156, 160, 161). Trailing whitespace is valid
/// JSON, so a file of exactly 16384 bytes is read and one of 16385 bytes is refused.
#[test]
fn credential_file_is_capped_at_sixteen_kib() {
    let dir = tempfile::tempdir().unwrap();
    let mut exact = credential(700);
    exact.resize(16 * 1024, b' ');
    assert!(IceSource::open(private_file(dir.path(), "exact.json", &exact), 100).is_ok());
    let mut over = credential(700);
    over.resize(16 * 1024 + 1, b' ');
    assert!(IceSource::open(private_file(dir.path(), "over.json", &over), 100).is_err());
}

/// Admissions per device: at most eight in any 60 s window (ice_config.rs:60, 68). The window is
/// wall-clock time, and these nine calls take microseconds.
#[test]
fn device_admissions_are_capped_at_eight_per_minute() {
    let dir = tempfile::tempdir().unwrap();
    let source = source_expiring_at(dir.path(), 700);
    for _ in 0..8 {
        source.admit("device-a").unwrap();
    }
    assert_eq!(
        source.admit("device-a").err().unwrap().to_string(),
        "ICE request rate exceeded"
    );
}

/// Active device entries and admitted offers: at most 256 each (ice_config.rs:64 and 87). A known
/// device is not a new entry, so it is still admitted when the device table is full.
#[test]
fn device_and_deadline_tables_hold_256_entries() {
    let dir = tempfile::tempdir().unwrap();
    let source = source_expiring_at(dir.path(), 700);
    for i in 0..256 {
        source.admit(&format!("device-{i}")).unwrap();
    }
    assert_eq!(
        source.admit("device-256").err().unwrap().to_string(),
        "ICE admission capacity"
    );
    source.admit("device-0").unwrap();
    for i in 0..256 {
        source.admit_offer(&format!("offer-{i}"), 100).unwrap();
    }
    assert_eq!(
        source
            .admit_offer("offer-256", 100)
            .err()
            .unwrap()
            .to_string(),
        "ICE admission capacity"
    );
}

/// Unsigned-route SDP cap: 200000 bytes (main.rs:393). Exactly 200000 passes that check and reaches
/// the capability gate, then the stopping flag (503, nothing started). One more byte is a 400.
/// On the signed route the session-auth MAX_SDP bound (lib.rs:20) is reached first.
#[tokio::test]
async fn unsigned_offer_sdp_is_capped_at_200000_bytes() {
    let mut app = test_app();
    app.capabilities = json!({"h265":"verified-hardware"});
    app.active.lock().await.stopping = true;
    let exact = crate::offer_inner(app.clone(), offer("h265", "a".repeat(200_000)), None).await;
    assert_eq!(
        text(exact).await,
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "service stopping".to_string()
        )
    );
    let over = crate::offer_inner(app.clone(), offer("h265", "a".repeat(200_001)), None).await;
    assert_eq!(
        text(over).await,
        (StatusCode::BAD_REQUEST, "invalid offer".to_string())
    );
}

/// Tombstones: at most 256 cancelled request ids (main.rs:121) and at most 256 completed ones
/// (main.rs:421). When either table is full, a new offer is refused with 429 after the codec and
/// capability gates and before any session slot exists.
#[tokio::test]
async fn cancelled_and_completed_tombstones_hold_256_entries() {
    let mut app = test_app();
    app.capabilities = json!({"h265":"verified-hardware"});
    {
        let mut book = app.active.lock().await;
        for i in 0..256 {
            assert!(book.cancel_id(&format!("cancelled-{i}")));
        }
        assert!(!book.cancel_id("cancelled-256"));
    }
    let refused = crate::offer_inner(app.clone(), offer("h265", "v=0".into()), None).await;
    assert_eq!(
        text(refused).await,
        (
            StatusCode::TOO_MANY_REQUESTS,
            "cancellation capacity reached".to_string()
        )
    );
    {
        let mut book = app.active.lock().await;
        book.cancelled.clear();
        for i in 0..256 {
            book.completed.insert(
                format!("completed-{i}"),
                (Instant::now(), true, format!("session-{i}")),
            );
        }
    }
    let refused = crate::offer_inner(app.clone(), offer("h265", "v=0".into()), None).await;
    assert_eq!(
        text(refused).await,
        (
            StatusCode::TOO_MANY_REQUESTS,
            "cancellation capacity reached".to_string()
        )
    );
    assert!(app.active.lock().await.active.is_none());
}

/// Control-channel bounds (control_channel.rs:25-28). A fragment is HEADER_BYTES of header plus at
/// most CHUNK_BYTES - HEADER_BYTES of payload, and a reassembled JSON message is at most
/// JSON_BYTES. SEND_BUFFER_BYTES is the SCTP receive window and the data-channel send limit
/// (control_channel.rs:103 and 640). It must hold one full CHUNK_BYTES message and clear the
/// RFC 9260 floor of 1500 bytes (rtc setting_engine.rs:1363-1378). bounded_settings() consumes
/// these values, but rtc 0.21 keeps the SCTP sizes pub(crate), so bounded_settings() itself cannot
/// be read back from here.
#[test]
fn control_channel_frame_and_buffer_bounds_are_pinned() {
    assert_eq!(CHUNK_BYTES, 16 * 1024);
    assert_eq!(HEADER_BYTES, 17);
    assert_eq!(JSON_BYTES, 1024 * 1024);
    assert_eq!(SEND_BUFFER_BYTES, 64 * 1024);
    const { assert!(SEND_BUFFER_BYTES >= CHUNK_BYTES) };
    const { assert!(SEND_BUFFER_BYTES >= 1500) };
}

/// The 180 s media cap is the named constant `MEDIA_SESSION_CAP_SECONDS`, and the future that
/// `run_session` races (`media_session_cap_elapsed`) ends at exactly that value. Paused tokio
/// time: no real sleep. A change of the value, of the sleep it drives, or of the unit fails here.
#[tokio::test(start_paused = true)]
async fn media_session_cap_ends_at_exactly_one_hundred_eighty_seconds() {
    assert_eq!(MEDIA_SESSION_CAP_SECONDS, 180);
    let cap = Duration::from_secs(MEDIA_SESSION_CAP_SECONDS);
    let one_ms = Duration::from_millis(1);
    assert!(
        tokio::time::timeout(cap - one_ms, media_session_cap_elapsed())
            .await
            .is_err(),
        "the cap must not elapse 1 ms before 180 s"
    );
    let started = tokio::time::Instant::now();
    tokio::time::timeout(cap + one_ms, media_session_cap_elapsed())
        .await
        .expect("the cap must elapse within 180 s + 1 ms");
    assert_eq!(tokio::time::Instant::now() - started, cap);
}

/// run_session (main.rs) must race the named cap future, not an inline literal. main.rs is the
/// binary root and run_session needs a real peer and encoder, so the production wiring is pinned
/// from its source: exactly one cap arm inside run_session, and no `from_secs(180)` literal
/// anywhere in the production part of main.rs.
#[test]
fn media_session_cap_is_the_named_constant_and_run_session_uses_it() {
    let main = include_str!("main.rs");
    let production = &main[..main
        .find("#[cfg(test)]\nmod tests {")
        .expect("main.rs test module marker")];
    let start = production
        .find("async fn run_session(")
        .expect("run_session in main.rs");
    let end = production[start..]
        .find("\nfn nack_budget_value(")
        .expect("end of run_session")
        + start;
    let run_session = &production[start..end];
    assert_eq!(
        run_session
            .matches("_=ice_config::media_session_cap_elapsed()=>Ok(())")
            .count(),
        1,
        "run_session must race ice_config::media_session_cap_elapsed() exactly once"
    );
    assert_eq!(
        production.matches("media_session_cap_elapsed").count(),
        1,
        "the cap future must be consumed only by run_session"
    );
    for literal in ["from_secs(180)", "from_secs( 180 )", "from_secs(180_"] {
        assert!(
            !production.contains(literal),
            "inline 180 s literal {literal:?} found in main.rs"
        );
    }
}
