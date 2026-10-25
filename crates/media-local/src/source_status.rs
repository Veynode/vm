//! Bounded diagnostic reduction. Never use stderr as cleanup attestation, and
//! never forward a native/Guest error string, path, domain or credential.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};

const MAX_LINE: usize = 16 * 1024;
const MAX_TOTAL: usize = 128 * 1024;
const MAX_LINES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SourceFailure {
    pub code: &'static str,
}

// This diagnostic is host-local metadata only. It is never cleanup evidence
// and does not extend the intentionally coarse browser SourceFailure contract.
#[derive(Debug, Serialize)]
pub struct Report {
    #[serde(skip_serializing)]
    pub failure: Option<SourceFailure>,
    pub diagnostic: Diagnostic,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Diagnostic {
    Terminal {
        event: TerminalEvent,
        started: bool,
        guest_code: Option<GuestCode>,
        console_identity_diagnostic: Option<IdentityDiagnostic>,
        main_display_active: Option<bool>,
        main_display_asleep: Option<bool>,
        cleanup_basis: Option<CleanupBasis>,
        // A reported boolean, not independent proof of resource retirement.
        reported_guest_cleanup_confirmed: bool,
        source_sequence: u64,
        submitted: u64,
        encoded: u64,
    },
    Absent,
    InvalidOrIncomplete,
    InvalidTerminal,
    ReadFailed,
    CollectionUnavailable,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalEvent {
    Error,
    Stopped,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupBasis {
    NoStreamCreated,
    StopCaptureCompleted,
    DelegateReportedStopped,
}

macro_rules! guest_codes {
    ($($variant:ident => $wire:literal),+ $(,)?) => {
        #[derive(Debug, Serialize)]
        pub enum GuestCode {
            $(#[serde(rename = $wire)] $variant,)+
            #[serde(rename = "unrecognized_guest_code")] Unrecognized,
        }
        impl GuestCode {
            fn from_wire(code: &str) -> Self {
                match code { $($wire => Self::$variant,)+ _ => Self::Unrecognized }
            }
        }
    };
}
// Literal production error codes in native/tools/macos-capture-stream.swift.
// Do not accept prefixes or log an unrecognized input string.
guest_codes! {
    ScreenRecordingPermissionRequired => "screen_recording_permission_required",
    MainDisplayUnavailable => "main_display_unavailable",
    ShareableContentFailed => "shareable_content_failed",
    StartCaptureFailed => "start_capture_failed",
    StartCaptureDeadlineExceeded => "start_capture_deadline_exceeded",
    CaptureStoppedWithError => "capture_stopped_with_error",
    AddStreamOutputFailed => "add_stream_output_failed",
    ConsoleIdentityUnavailable => "console_identity_unavailable",
    ConsoleIdentityMismatch => "console_identity_mismatch",
    ConsoleGeometryMismatch => "console_geometry_mismatch",
    ConsoleIdentityVerificationStale => "console_identity_verification_stale",
    ConsoleSessionTransition => "console_session_transition",
    UnexpectedPixelFormatOrGeometry => "unexpected_pixel_format_or_geometry",
    PixelLockFailed => "pixel_lock_failed",
    InvalidPlaneLayout => "invalid_plane_layout",
    FrameTooLarge => "frame_too_large",
    FrameCopyFailed => "frame_copy_failed",
    InvalidSampleTimestamp => "invalid_sample_timestamp",
    CaptureCleanupQuarantined => "capture_cleanup_quarantined",
    CaptureBusy => "capture_busy",
    InvalidStart => "invalid_start",
    OddDimensions => "odd_dimensions",
    InvalidExpectedConsoleIdentity => "invalid_expected_console_identity",
    InvalidCleanupReceipt => "invalid_cleanup_receipt",
    InvalidCleanupQuery => "invalid_cleanup_query",
    InvalidJson => "invalid_json",
    ExpectedStop => "expected_stop",
    CommandTooLarge => "command_too_large",
    StartDeadlineExceeded => "start_deadline_exceeded",
    StopCaptureFailed => "stop_capture_failed",
}

/// Optional fixed-shape timing metadata; no identity, path or arbitrary string.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityDiagnostic {
    stale_site: Option<StaleSite>,
    verification_age_us: Option<u64>,
    authority_read_max_us: u64,
    identity_publish_lock_wait_max_us: u64,
    sample_copy_max_us: u64,
    sample_lock_wait_max_us: u64,
    identity_timer_lateness_max_us: u64,
    sample_generation: u64,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum StaleSite {
    AuthorityRead,
    AuthorityPublish,
    TimerRead,
    SampleAdmission,
    SamplePublish,
    WriterAdmission,
}
impl IdentityDiagnostic {
    fn bounded(&self) -> bool {
        [
            self.verification_age_us.unwrap_or(0),
            self.authority_read_max_us,
            self.identity_publish_lock_wait_max_us,
            self.sample_copy_max_us,
            self.sample_lock_wait_max_us,
            self.identity_timer_lateness_max_us,
            self.sample_generation,
        ]
        .into_iter()
        .all(|v| v <= 9_007_199_254_740_991)
    }
}

#[derive(Deserialize)]
struct TerminalInput {
    event: TerminalEvent,
    guest_error: Option<GuestErrorInput>,
    cleanup_basis: Option<CleanupBasis>,
    guest_cleanup_confirmed: bool,
    source_sequence: u64,
    submitted: u64,
    encoded: u64,
}
#[derive(Deserialize)]
struct GuestErrorInput {
    code: String,
    console_identity_diagnostic: Option<IdentityDiagnostic>,
    main_display_active: Option<bool>,
    main_display_asleep: Option<bool>,
}
fn terminal_diagnostic(bytes: &[u8], started: bool) -> Diagnostic {
    // Decode the original bytes: typed fields reject duplicate keys, numeric
    // booleans and invalid counters. Arbitrary unused strings are never copied.
    let Ok(value) = serde_json::from_slice::<TerminalInput>(bytes) else {
        return Diagnostic::InvalidTerminal;
    };
    if value
        .guest_error
        .as_ref()
        .is_some_and(|v| v.code.is_empty() || v.code.len() > 64)
    {
        return Diagnostic::InvalidTerminal;
    }
    if value
        .guest_error
        .as_ref()
        .and_then(|e| e.console_identity_diagnostic.as_ref())
        .is_some_and(|d| !d.bounded())
    {
        return Diagnostic::InvalidTerminal;
    }
    Diagnostic::Terminal {
        event: value.event,
        started,
        guest_code: value
            .guest_error
            .as_ref()
            .map(|e| GuestCode::from_wire(&e.code)),
        main_display_active: value
            .guest_error
            .as_ref()
            .and_then(|e| e.main_display_active),
        main_display_asleep: value
            .guest_error
            .as_ref()
            .and_then(|e| e.main_display_asleep),
        cleanup_basis: value.cleanup_basis,
        reported_guest_cleanup_confirmed: value.guest_cleanup_confirmed,
        source_sequence: value.source_sequence,
        submitted: value.submitted,
        encoded: value.encoded,
        console_identity_diagnostic: value
            .guest_error
            .and_then(|e| e.console_identity_diagnostic),
    }
}

#[derive(Default)]
struct Parser {
    line: Vec<u8>,
    total: usize,
    lines: usize,
    invalid: bool,
    started: bool,
    terminal: bool,
    failure: Option<SourceFailure>,
    diagnostic: Option<Diagnostic>,
}
impl Parser {
    fn feed(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len());
        if self.total > MAX_TOTAL {
            self.invalidate();
        }
        if self.invalid {
            return;
        }
        for &byte in bytes {
            if byte == b'\n' {
                self.lines += 1;
                if self.lines > MAX_LINES {
                    self.invalidate();
                    return;
                }
                self.record();
                self.line.clear();
                if self.invalid {
                    return;
                }
            } else {
                self.line.push(byte);
                if self.line.len() > MAX_LINE {
                    self.invalidate();
                    return;
                }
            }
        }
    }
    fn invalidate(&mut self) {
        self.invalid = true;
        self.line.clear();
        self.failure = None;
        self.diagnostic = None;
    }
    fn record(&mut self) {
        let Ok(value) = serde_json::from_slice::<Value>(&self.line) else {
            self.invalidate();
            return;
        };
        let Some(object) = value.as_object() else {
            self.invalidate();
            return;
        };
        let Some(event) = object.get("event").and_then(Value::as_str) else {
            self.invalidate();
            return;
        };
        if self.terminal {
            self.invalidate();
            return;
        }
        if event == "guest-started" {
            self.started = true;
            return;
        }
        if event != "error" && event != "stopped" {
            return;
        }
        // Only the final host summary has all these typed counters. An early
        // encoder failure can be legitimate, but lacks source evidence.
        let counters = ["source_sequence", "submitted", "encoded"].map(|k| value[k].as_u64());
        if !value["guest_cleanup_confirmed"].is_boolean() || counters.iter().any(Option::is_none) {
            return;
        }
        self.terminal = true;
        self.diagnostic = Some(terminal_diagnostic(&self.line, self.started));
        let code = match value.get("guest_error").filter(|v| !v.is_null()) {
            Some(error) => {
                let Some(code) = error.get("code").and_then(Value::as_str) else {
                    self.invalidate();
                    return;
                };
                if code.is_empty() || code.len() > 64 {
                    self.invalidate();
                    return;
                }
                let known = matches!(
                    code,
                    "screen_recording_permission_required"
                        | "main_display_unavailable"
                        | "shareable_content_failed"
                        | "start_capture_failed"
                        | "start_capture_deadline_exceeded"
                        | "capture_stopped_with_error"
                        | "add_stream_output_failed"
                );
                if !known {
                    "guest_capture_failed"
                } else if error["main_display_asleep"] == true {
                    "guest_display_asleep"
                } else if error["main_display_active"] == false {
                    "guest_display_inactive"
                } else {
                    match code {
                        "screen_recording_permission_required" => {
                            "guest_screen_permission_required"
                        }
                        "capture_stopped_with_error" => "guest_capture_stopped",
                        _ => "guest_capture_unavailable",
                    }
                }
            }
            None if self.started && counters == [Some(0), Some(0), Some(0)] => {
                "guest_capture_no_frames"
            }
            None => return,
        };
        self.failure = Some(SourceFailure { code });
    }
    fn finish(self) -> Report {
        if self.invalid || !self.line.is_empty() {
            Report {
                failure: None,
                diagnostic: Diagnostic::InvalidOrIncomplete,
            }
        } else {
            Report {
                failure: self.failure,
                diagnostic: self.diagnostic.unwrap_or(Diagnostic::Absent),
            }
        }
    }
}
/// Drain even after rejecting diagnostics, so excessive stderr cannot deadlock
/// the encoder. Caller bounds lifetime and aborts this reader after child reap.
pub async fn collect_report(mut input: impl AsyncRead + Unpin) -> Report {
    let mut parser = Parser::default();
    let mut buffer = [0u8; 4096];
    loop {
        match input.read(&mut buffer).await {
            Ok(0) => return parser.finish(),
            Ok(n) => parser.feed(&buffer[..n]),
            Err(_) => {
                return Report {
                    failure: None,
                    diagnostic: Diagnostic::ReadFailed,
                };
            }
        }
    }
}
#[cfg(test)]
async fn collect(input: impl AsyncRead + Unpin) -> Option<SourceFailure> {
    collect_report(input).await.failure
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn final_record(error: Value) -> Vec<u8> {
        let mut b = serde_json::to_vec(&json!({"event":"error","guest_error":error,"guest_cleanup_confirmed":true,"source_sequence":0,"submitted":0,"encoded":0})).unwrap();
        b.push(b'\n');
        b
    }
    fn identity_timing() -> Value {
        json!({"stale_site":"sample_publish","verification_age_us":750001,
            "authority_read_max_us":170,"identity_publish_lock_wait_max_us":20,
            "sample_copy_max_us":900,"sample_lock_wait_max_us":12,
            "identity_timer_lateness_max_us":30,"sample_generation":2})
    }
    #[tokio::test]
    async fn identity_timing_preserves_fixed_metadata_and_coarse_failure() {
        for site in [
            "authority_read",
            "authority_publish",
            "timer_read",
            "sample_admission",
            "sample_publish",
            "writer_admission",
        ] {
            let mut timing = identity_timing();
            timing["stale_site"] = json!(site);
            let report = collect_report(
                final_record(json!({
                    "code":"console_identity_verification_stale",
                    "console_identity_diagnostic": timing,
                }))
                .as_slice(),
            )
            .await;
            assert_eq!(report.failure.unwrap().code, "guest_capture_failed");
            let wire = serde_json::to_value(report.diagnostic).unwrap();
            assert_eq!(wire["console_identity_diagnostic"], timing);
            assert_eq!(wire["reported_guest_cleanup_confirmed"], true);
        }
        let mut timing = identity_timing();
        timing["stale_site"] = Value::Null;
        timing["verification_age_us"] = Value::Null;
        let report = collect_report(
            final_record(json!({"code":"console_session_transition",
            "console_identity_diagnostic":timing}))
            .as_slice(),
        )
        .await;
        assert_eq!(
            serde_json::to_value(report.diagnostic).unwrap()["console_identity_diagnostic"],
            timing
        );
    }
    #[tokio::test]
    async fn hostile_identity_timing_never_changes_failure_or_exposes_strings() {
        for (key, value) in [
            ("stale_site", json!("/private/SECRET")),
            ("authority_read_max_us", json!(true)),
            ("sample_copy_max_us", json!(-1)),
            ("sample_lock_wait_max_us", json!(1.5)),
            ("sample_generation", json!(9_007_199_254_740_992u64)),
            ("verification_age_us", json!("SECRET")),
            ("secret", json!("SECRET")),
        ] {
            let mut timing = identity_timing();
            timing[key] = value;
            let report = collect_report(
                final_record(json!({"code":"console_identity_verification_stale",
                "console_identity_diagnostic":timing}))
                .as_slice(),
            )
            .await;
            assert_eq!(report.failure.unwrap().code, "guest_capture_failed");
            assert!(matches!(report.diagnostic, Diagnostic::InvalidTerminal));
            assert!(!serde_json::to_string(&report.diagnostic)
                .unwrap()
                .contains("SECRET"));
        }
        let bytes = String::from_utf8(final_record(
            json!({"code":"console_identity_verification_stale",
            "console_identity_diagnostic":identity_timing()}),
        ))
        .unwrap()
        .replace(
            "\"sample_generation\":2",
            "\"sample_generation\":2,\"sample_generation\":3",
        );
        let report = collect_report(bytes.as_bytes()).await;
        assert!(matches!(report.diagnostic, Diagnostic::InvalidTerminal));
        assert_eq!(report.failure.unwrap().code, "guest_capture_failed");
    }
    #[tokio::test]
    async fn console_and_frame_diagnostics_preserve_public_coarse_failure() {
        for code in [
            "console_identity_unavailable",
            "console_identity_mismatch",
            "console_geometry_mismatch",
            "console_identity_verification_stale",
            "console_session_transition",
            "unexpected_pixel_format_or_geometry",
            "pixel_lock_failed",
            "invalid_plane_layout",
            "frame_too_large",
            "frame_copy_failed",
            "invalid_sample_timestamp",
        ] {
            let report = collect_report(final_record(json!({"code":code})).as_slice()).await;
            assert_eq!(report.failure.unwrap().code, "guest_capture_failed");
            let diagnostic = serde_json::to_value(report.diagnostic).unwrap();
            assert_eq!(diagnostic["status"], "terminal");
            assert_eq!(diagnostic["guest_code"], code);
        }
    }
    #[tokio::test]
    async fn diagnostics_never_copy_unknown_codes_domains_paths_or_error_text() {
        let mut value = json!({"event":"error","guest_error":{"code":"console_SECRET","error_domain":"/private/SECRET","error_code":"SECRET","main_display_asleep":true},"error":"SECRET","cleanup_basis":"stop_capture_completed","guest_cleanup_confirmed":true,"source_sequence":110,"submitted":110,"encoded":110});
        for unknown in ["console_SECRET", "frame_SECRET", "SECRET"] {
            value["guest_error"]["code"] = json!(unknown);
            let mut bytes = serde_json::to_vec(&value).unwrap();
            bytes.push(b'\n');
            let report = collect_report(bytes.as_slice()).await;
            assert_eq!(report.failure.unwrap().code, "guest_capture_failed");
            let wire = serde_json::to_value(report.diagnostic).unwrap();
            assert_eq!(wire["guest_code"], "unrecognized_guest_code");
            assert_eq!(wire["source_sequence"], 110);
            assert_eq!(wire["cleanup_basis"], "stop_capture_completed");
            assert!(!wire.to_string().contains("SECRET"));
            assert!(wire.get("error_domain").is_none());
        }
    }
    #[tokio::test]
    async fn diagnostic_typed_terminal_rejects_duplicate_and_hostile_fields() {
        let valid = String::from_utf8(final_record(
            json!({"code":"console_identity_verification_stale"}),
        ))
        .unwrap();
        for bytes in [
            valid.replace(
                "\"source_sequence\":0",
                "\"source_sequence\":0,\"source_sequence\":1",
            ),
            valid.replace(
                "\"guest_cleanup_confirmed\":true",
                "\"guest_cleanup_confirmed\":true,\"guest_cleanup_confirmed\":false",
            ),
            valid.replace(
                "\"code\":",
                "\"code\":\"console_identity_mismatch\",\"code\":",
            ),
            valid.replace(
                "\"guest_error\":{",
                "\"guest_error\":{\"main_display_active\":1,",
            ),
            valid.replace(
                "\"guest_error\":{",
                "\"cleanup_basis\":\"SECRET\",\"guest_error\":{ ",
            ),
        ] {
            let report = collect_report(bytes.as_bytes()).await;
            assert!(
                matches!(report.diagnostic, Diagnostic::InvalidTerminal),
                "{bytes}"
            );
            assert!(!serde_json::to_string(&report.diagnostic)
                .unwrap()
                .contains("SECRET"));
        }
        for bytes in [
            valid.replace("\"source_sequence\":0", "\"source_sequence\":-1"),
            valid.replace("\"submitted\":0", "\"submitted\":1.5"),
            valid.replace("\"encoded\":0", "\"encoded\":18446744073709551616"),
            valid.replace(
                "\"guest_cleanup_confirmed\":true",
                "\"guest_cleanup_confirmed\":1",
            ),
        ] {
            let report = collect_report(bytes.as_bytes()).await;
            assert!(matches!(report.diagnostic, Diagnostic::Absent));
            assert!(report.failure.is_none());
        }
    }
    #[tokio::test]
    async fn diagnostic_invalid_stream_is_not_a_terminal_or_cleanup_proof() {
        let valid = final_record(json!({"code":"console_identity_verification_stale"}));
        for bytes in [
            valid[..valid.len() - 1].to_vec(),
            [valid.clone(), valid.clone()].concat(),
            [vec![b'a'; MAX_LINE + 1], b"\n".to_vec(), valid].concat(),
            vec![b'\n'; MAX_TOTAL + 1],
        ] {
            let report = collect_report(bytes.as_slice()).await;
            assert!(matches!(report.diagnostic, Diagnostic::InvalidOrIncomplete));
            assert!(report.failure.is_none());
        }
    }
    #[tokio::test]
    async fn fragmented_pipe_waits_for_eof_and_keeps_safe_terminal() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, reader) = tokio::io::duplex(12);
        let collector = tokio::spawn(collect_report(reader));
        writer
            .write_all(b"{\"event\":\"guest-started\"}\n")
            .await
            .unwrap();
        let bytes = final_record(json!({"code":"console_identity_verification_stale"}));
        for chunk in bytes.chunks(7) {
            writer.write_all(chunk).await.unwrap();
        }
        tokio::task::yield_now().await;
        assert!(
            !collector.is_finished(),
            "A terminal diagnostic does not replace EOF"
        );
        writer.shutdown().await.unwrap();
        let report = tokio::time::timeout(std::time::Duration::from_secs(1), collector)
            .await
            .unwrap()
            .unwrap();
        let diagnostic = serde_json::to_value(report.diagnostic).unwrap();
        assert_eq!(diagnostic["started"], true);
        assert_eq!(
            diagnostic["guest_code"],
            "console_identity_verification_stale"
        );
        assert_eq!(report.failure.unwrap().code, "guest_capture_failed");
    }
    #[tokio::test]
    async fn only_fixed_codes_cross_boundary_and_no_frames_does_not_infer_sleep() {
        for (error, expected) in [
            (
                json!({"code":"main_display_unavailable","main_display_asleep":true}),
                "guest_display_asleep",
            ),
            (
                json!({"code":"shareable_content_failed","main_display_active":false}),
                "guest_display_inactive",
            ),
            (
                json!({"code":"screen_recording_permission_required"}),
                "guest_screen_permission_required",
            ),
            (
                json!({"code":"capture_stopped_with_error"}),
                "guest_capture_stopped",
            ),
            (
                json!({"code":"unknown-secret","main_display_asleep":true,"error_domain":"/private/SECRET"}),
                "guest_capture_failed",
            ),
        ] {
            let result = collect(final_record(error).as_slice()).await.unwrap();
            assert_eq!(result.code, expected);
            assert!(!serde_json::to_string(&result).unwrap().contains("SECRET"));
        }
        let mut bytes = b"{\"event\":\"guest-started\"}\n".to_vec();
        bytes.extend(final_record(Value::Null));
        assert_eq!(
            collect(bytes.as_slice()).await.unwrap().code,
            "guest_capture_no_frames"
        );
        assert!(collect(final_record(Value::Null).as_slice())
            .await
            .is_none());
    }
    #[tokio::test]
    async fn malformed_truncated_duplicate_and_excessive_diagnostics_fail_closed() {
        let valid = final_record(json!({"code":"main_display_unavailable"}));
        for bytes in [
            b"SECRET not JSON\n".to_vec(),
            valid[..valid.len() - 1].to_vec(),
            [valid.clone(), valid.clone()].concat(),
            [vec![b'a'; MAX_LINE + 1], b"\n".to_vec(), valid.clone()].concat(),
            [b"{\"event\":\"ready\"}\n".repeat(MAX_LINES), valid.clone()].concat(),
            vec![b'\n'; MAX_TOTAL + 1],
        ] {
            assert!(collect(bytes.as_slice()).await.is_none());
        }
    }
    #[tokio::test]
    async fn actual_child_stderr_is_drained_and_eof_is_required() {
        use std::process::Stdio;
        let mut child = tokio::process::Command::new("/bin/sh").args(["-c", "printf '%s\\n' '{\"event\":\"error\",\"guest_error\":{\"code\":\"main_display_unavailable\"},\"guest_cleanup_confirmed\":false,\"source_sequence\":0,\"submitted\":0,\"encoded\":0}' >&2; exit 1"]).stderr(Stdio::piped()).spawn().unwrap();
        let status = collect(child.stderr.take().unwrap()).await.unwrap();
        assert_eq!(status.code, "guest_capture_unavailable");
        assert!(
            !child.wait().await.unwrap().success(),
            "A diagnostic is never cleanup or media success"
        );
    }
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "requires explicit existing macOS Guest encoder binary; synthetic private vsock bridge only"]
    async fn actual_guest_encoder_summary_keeps_source_and_cleanup_independent() {
        use std::{os::unix::fs::PermissionsExt, process::Stdio, time::Duration};
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::UnixListener,
        };
        let binary =
            std::env::var("VEYNODE_NATIVE_ENCODER").expect("explicit encoder path required");
        for terminal in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let socket = temp.path().join("source.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
            let mut child = tokio::process::Command::new(&binary)
                .arg(&socket)
                .args(["--codec", "h264"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let diagnostics = tokio::spawn(collect(child.stderr.take().unwrap()));
            let (stream, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut stream = BufReader::new(stream);
            let mut start = String::new();
            tokio::time::timeout(Duration::from_secs(3), stream.read_line(&mut start))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&start).unwrap()["id"],
                "host-start"
            );
            let mut events = vec![
                json!({"event":"error","id":"host-start","code":"main_display_unavailable","main_display_asleep":true}),
            ];
            if terminal {
                events.push(json!({"event":"terminated","id":"host-start","reason":"capture_failed","cleanup_confirmed":true,"cleanup_basis":"no_stream_created"}));
            }
            for event in events {
                let data = serde_json::to_vec(&event).unwrap();
                stream
                    .get_mut()
                    .write_all(&((data.len() + 1) as u32).to_be_bytes())
                    .await
                    .unwrap();
                stream.get_mut().write_all(&[1]).await.unwrap();
                stream.get_mut().write_all(&data).await.unwrap();
            }
            stream.get_mut().shutdown().await.unwrap();
            let exit = tokio::time::timeout(Duration::from_secs(4), child.wait())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(exit.code(), Some(if terminal { 78 } else { 1 }));
            assert_eq!(
                crate::cleanup_attested_exit(exit.code(), Some(78)),
                terminal
            );
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), diagnostics)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .code,
                "guest_display_asleep"
            );
            let mut media = Vec::new();
            child
                .stdout
                .take()
                .unwrap()
                .read_to_end(&mut media)
                .await
                .unwrap();
            assert!(
                media.is_empty(),
                "No actual capture or media acceptance in this synthetic test"
            );
        }
    }
}
