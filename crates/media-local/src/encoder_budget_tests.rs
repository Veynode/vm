//! In-memory admission and encoded-frame seams; no sockets, capture or VM.
use super::*;
use frame_schedule::Decision;
use vm_media_local::rate_control::MAX_ENCODED_ACCESS_UNIT_BYTES as LIMIT;

fn app() -> App {
    App {
        origin: "http://127.0.0.1:1".into(),
        host: "127.0.0.1:1".into(),
        encoder: PathBuf::from("must-not-start"),
        socket: PathBuf::new(),
        web: PathBuf::new(),
        capabilities: json!({"h264":"verified-hardware"}),
        fault_au_egress_hold: false,
        fault_drop_first_keyframe: false,
        fault_suppress_hevc_frames: false,
        active: Arc::new(Mutex::new(SessionBook::default())),
        auth: None,
        cleanup_failure_exit: None,
        guest_binding: None,
        encoder_rate: None,
    }
}
fn offer_request() -> Offer {
    Offer {
        request_id: uuid::Uuid::new_v4().to_string(),
        sdp: String::new(),
        r#type: "offer".into(),
        codec: "h264".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encoder_budget_atomic_reservations_and_cleanup_accounting() {
    assert_eq!(MAX_CONCURRENT_MEDIA_SESSIONS, 2);
    let budget = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_MEDIA_SESSIONS));
    let mut attempts = Vec::new();
    for _ in 0..16 {
        let budget = budget.clone();
        attempts.push(tokio::spawn(async move { reserve_encoder(&budget) }));
    }
    let mut slots = Vec::new();
    let mut refusals = 0;
    for attempt in attempts {
        match attempt.await.unwrap() {
            Ok(slot) => slots.push(slot),
            Err(_) => refusals += 1,
        }
    }
    assert_eq!(slots.len(), 2);
    assert_eq!(refusals, 14);
    assert_eq!(budget.available_permits(), 0);
    let mut book = SessionBook::default();
    book.finish_encoder_slot("clean", slots.pop().unwrap(), true);
    let replacement = reserve_encoder(&budget).unwrap();
    book.finish_encoder_slot("unconfirmed", replacement, false);
    assert!(reserve_encoder(&budget).is_err());
    book.finish_encoder_slot("clean", slots.pop().unwrap(), true);
    assert_eq!(
        budget.available_permits(),
        1,
        "unconfirmed cleanup retains capacity"
    );
    book.retained_encoders.remove("different-session");
    assert_eq!(budget.available_permits(), 1);
    book.retained_encoders.remove("unconfirmed");
    assert_eq!(budget.available_permits(), 2);
}

#[tokio::test]
async fn encoder_budget_independent_apps_do_not_share_capacity() {
    let first = app();
    let second = app();
    let first_budget = first.encoder_budget().await;
    let second_budget = second.encoder_budget().await;
    assert!(!Arc::ptr_eq(&first_budget, &second_budget));
    assert!(Arc::ptr_eq(
        &first_budget,
        &first.clone().encoder_budget().await
    ));
    let first_slots = [
        reserve_encoder(&first_budget).unwrap(),
        reserve_encoder(&first_budget).unwrap(),
    ];
    let response = offer_inner(first, offer_request(), None).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"error":"encoder_budget_exceeded"})
    );
    assert_eq!(
        second_budget.available_permits(),
        MAX_CONCURRENT_MEDIA_SESSIONS
    );
    let second_slots = [
        reserve_encoder(&second_budget).unwrap(),
        reserve_encoder(&second_budget).unwrap(),
    ];
    assert!(reserve_encoder(&second_budget).is_err());
    drop(first_slots);
    assert_eq!(
        first_budget.available_permits(),
        MAX_CONCURRENT_MEDIA_SESSIONS
    );
    assert_eq!(second_budget.available_permits(), 0);
    drop(second_slots);
    assert_eq!(
        second_budget.available_permits(),
        MAX_CONCURRENT_MEDIA_SESSIONS
    );
}

#[tokio::test]
async fn encoder_budget_refusal_precedes_session_and_encoder_start() {
    let app = app();
    let budget = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_MEDIA_SESSIONS));
    let first = reserve_encoder(&budget).unwrap();
    let second = reserve_encoder(&budget).unwrap();
    let response = offer_with_encoder_budget(app.clone(), offer_request(), None, &budget).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"error":"encoder_budget_exceeded"})
    );
    let book = app.active.lock().await;
    assert!(book.active.is_none());
    assert!(book.completed.is_empty());
    assert!(book.cancelled.is_empty());
    assert_eq!(
        budget.available_permits(),
        0,
        "refusal cannot release another session's slot"
    );
    drop((first, second));
    assert_eq!(budget.available_permits(), 2);
}

#[tokio::test]
async fn encoder_budget_preserves_active_session_and_cleanup_guards() {
    let app = app();
    let budget = Arc::new(tokio::sync::Semaphore::new(2));
    let (cancel, cancelled) = watch::channel(false);
    app.active.lock().await.active = Some(Session {
        request: "existing".into(),
        id: "existing".into(),
        cancel,
        finished: watch::channel(None).1,
        confirmed: watch::channel(true).0,
        input: None,
        files: None,
    });
    let response = offer_with_encoder_budget(app.clone(), offer_request(), None, &budget).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        &axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap()[..],
        b"one diagnostic session already active"
    );
    assert!(!*cancelled.borrow());
    assert_eq!(
        app.active.lock().await.active.as_ref().unwrap().id,
        "existing"
    );
    assert_eq!(budget.available_permits(), 2);
    {
        let mut book = app.active.lock().await;
        book.active = None;
        book.completed
            .insert("old".into(), (Instant::now(), false, "old".into()));
    }
    let response = offer_with_encoder_budget(app.clone(), offer_request(), None, &budget).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        &axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap()[..],
        b"previous session cleanup unconfirmed"
    );
    assert_eq!(budget.available_permits(), 2);
}

#[tokio::test]
async fn encoder_budget_capabilities_are_additive() {
    let app = app();
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, app.host.parse().unwrap());
    let response = get_capabilities(State(app), headers).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["h264"], "verified-hardware");
    assert_eq!(value["authentication"]["required"], false);
    assert_eq!(
        value["encoder_budget"],
        json!({"max_concurrent_media_sessions":2,"max_encoded_access_unit_bytes":LIMIT})
    );
}

#[test]
fn encoder_frame_size_recorded_hardware_sequences_are_admitted() {
    for (raw, maximum) in [
        (
            include_str!("../testdata/encoder-rate-live/h264-result.json"),
            987_750,
        ),
        (
            include_str!("../testdata/encoder-rate-live/hevc-result.json"),
            1_687_652,
        ),
    ] {
        let evidence: Value = serde_json::from_str(raw).unwrap();
        let records = evidence["records"].as_array().unwrap();
        assert_eq!(records.len(), 541);
        assert_eq!(evidence["decoded_frames"], records.len());
        assert_eq!(evidence["summary"]["hardware_verified"], true);
        assert_eq!(evidence["initial_status"]["target_bps"], 8_000_000);
        assert_eq!(
            records.iter().map(|r| r["bytes"].as_u64().unwrap()).max(),
            Some(maximum)
        );
        let now = Instant::now();
        let mut schedule = EncoderFrameSchedule::default();
        for (index, record) in records.iter().enumerate() {
            let pts = record["pts_us"].as_u64().unwrap();
            let key = record["keyframe"].as_bool().unwrap();
            let size = usize::try_from(record["bytes"].as_u64().unwrap()).unwrap();
            // The evidence has no transport arrival times. Replay at the
            // recorded cadence to isolate size admission from relative lag.
            let arrived = now + Duration::from_micros(pts);
            assert_eq!(
                schedule.classify(pts, key, size, arrived).unwrap(),
                Decision::Send {
                    deadline: arrived + Duration::from_millis(250),
                },
                "{} record {index}",
                evidence["codec"]
            );
        }
        assert_eq!(schedule.oversized_dropped, 0);
        assert_eq!(schedule.consecutive_oversized, 0);
        assert_eq!(schedule.dependency_dropped, 0);
        assert!(schedule.recovery_deadline().is_none());
    }
}

#[test]
fn encoder_frame_size_boundary_and_dependency_recovery() {
    assert_eq!(LIMIT, 2 * 1024 * 1024);
    let now = Instant::now();
    let mut schedule = EncoderFrameSchedule::default();
    assert!(matches!(
        schedule.classify(0, true, LIMIT - 1, now).unwrap(),
        Decision::Send { .. }
    ));
    assert!(matches!(
        schedule.classify(33_333, false, LIMIT, now).unwrap(),
        Decision::Send { .. }
    ));
    assert_eq!(
        schedule.classify(66_666, false, LIMIT + 1, now).unwrap(),
        Decision::Drop {
            request_keyframe: true
        }
    );
    assert_eq!(
        schedule.classify(99_999, false, LIMIT, now).unwrap(),
        Decision::Drop {
            request_keyframe: false
        }
    );
    assert_eq!(
        schedule.consecutive_oversized, 0,
        "every in-bound AU resets the consecutive count"
    );
    assert!(matches!(
        schedule.classify(133_332, true, LIMIT, now).unwrap(),
        Decision::Send { .. }
    ));
    assert!(schedule.recovery_deadline().is_none());
    assert_eq!(schedule.oversized_dropped, 1);
    assert_eq!(schedule.dependency_dropped, 1);
    assert_eq!(schedule.recoveries, 1);
}

#[test]
fn encoder_frame_size_third_consecutive_unit_has_typed_teardown_reason() {
    for key in [false, true] {
        let mut schedule = EncoderFrameSchedule::default();
        let now = Instant::now();
        for i in 0..2 {
            assert!(matches!(
                schedule.classify(i, key, LIMIT + 1, now).unwrap(),
                Decision::Drop { .. }
            ));
        }
        let error = schedule.classify(2, key, LIMIT + 1, now).unwrap_err();
        assert_eq!(
            error.downcast_ref::<EncoderFrameFailure>(),
            Some(&EncoderFrameFailure::ConsecutiveOversizedUnits)
        );
        assert_eq!(error.to_string(), "encoder_frame_size_limit_exceeded");
        assert_eq!(schedule.oversized_dropped, 3);
        assert_eq!(schedule.consecutive_oversized, 3);
    }
}

#[test]
fn encoder_frame_size_nonconsecutive_oversize_does_not_terminate_early() {
    let mut schedule = EncoderFrameSchedule::default();
    let now = Instant::now();
    for (i, size) in [LIMIT + 1, LIMIT + 1, LIMIT, LIMIT + 1, LIMIT + 1]
        .into_iter()
        .enumerate()
    {
        assert!(matches!(
            schedule.classify(i as u64, false, size, now).unwrap(),
            Decision::Drop { .. }
        ));
    }
    assert_eq!(schedule.oversized_dropped, 4);
    assert_eq!(schedule.consecutive_oversized, 2);
    assert!(matches!(
        schedule.classify(5, true, LIMIT, now).unwrap(),
        Decision::Send { .. }
    ));
}

#[test]
fn encoder_frame_size_recovery_preserves_deadline_drop_and_request_bounds() {
    let now = Instant::now();
    let mut schedule = EncoderFrameSchedule::default();
    assert_eq!(
        schedule.classify(0, true, LIMIT + 1, now).unwrap(),
        Decision::Drop {
            request_keyframe: true
        }
    );
    assert_eq!(
        schedule.recovery_deadline(),
        Some(now + frame_schedule::RECOVERY_LIMIT)
    );
    assert_eq!(
        schedule
            .classify(1, false, LIMIT, now + Duration::from_millis(499))
            .unwrap(),
        Decision::Drop {
            request_keyframe: false
        }
    );
    assert_eq!(
        schedule
            .classify(2, false, LIMIT, now + Duration::from_millis(500))
            .unwrap(),
        Decision::Drop {
            request_keyframe: true
        }
    );
    let error = schedule
        .classify(3, true, LIMIT, now + frame_schedule::RECOVERY_LIMIT)
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<EncoderFrameFailure>(),
        Some(&EncoderFrameFailure::KeyframeRecoveryExhausted)
    );
    let mut schedule = EncoderFrameSchedule::default();
    schedule.classify(0, true, LIMIT + 1, now).unwrap();
    for pts in 1..frame_schedule::MAX_RECOVERY_DROPS {
        assert!(matches!(
            schedule.classify(pts, false, LIMIT, now).unwrap(),
            Decision::Drop { .. }
        ));
    }
    assert!(schedule
        .classify(frame_schedule::MAX_RECOVERY_DROPS, true, LIMIT, now)
        .is_err());
}

#[test]
fn encoder_frame_size_stale_keyframe_cannot_clear_recovery() {
    let now = Instant::now();
    let mut schedule = EncoderFrameSchedule::default();
    schedule.classify(0, true, LIMIT, now).unwrap();
    schedule.classify(1, false, LIMIT + 1, now).unwrap();
    assert!(matches!(
        schedule
            .classify(2, true, LIMIT, now + Duration::from_millis(300))
            .unwrap(),
        Decision::Drop { .. }
    ));
    assert!(schedule.recovery_deadline().is_some());
    assert_eq!(schedule.recoveries, 0);
    assert!(matches!(
        schedule
            .classify(300_000, true, LIMIT, now + Duration::from_millis(300))
            .unwrap(),
        Decision::Send { .. }
    ));
    assert!(schedule.recovery_deadline().is_none());
    assert_eq!(
        schedule.recoveries + schedule.schedule.stats().recoveries,
        1
    );
}

#[tokio::test]
async fn encoder_frame_size_pipe_recovery_uses_existing_keyframe_command() {
    use tokio::io::AsyncReadExt;
    let now = Instant::now();
    let mut schedule = EncoderFrameSchedule::default();
    let mut bytes = Vec::new();
    for (pts, key, size) in [
        (0_u64, true, LIMIT + 1),
        (33_333, false, 10),
        (66_666, true, 10),
    ] {
        bytes.extend_from_slice(&(size as u32).to_be_bytes());
        bytes.extend_from_slice(&pts.to_be_bytes());
        bytes.push(u8::from(key));
        bytes.resize(bytes.len() + size, 0);
    }
    let mut reader = bytes.as_slice();
    let (mut writer, mut commands) = tokio::io::duplex(32);
    let mut keyframes = frame_schedule::KeyframeWriter::new(&mut writer);
    let mut previous = None;
    let mut admitted = Vec::new();
    for _ in 0..3 {
        let frame = frame_schedule::read_frame(&mut reader, previous, schedule.recovery_deadline())
            .await
            .unwrap();
        previous = Some(frame.pts);
        match schedule
            .classify(frame.pts, frame.key, frame.data.len(), now)
            .unwrap()
        {
            Decision::Drop {
                request_keyframe: true,
            } => keyframes.request().await.unwrap(),
            Decision::Drop {
                request_keyframe: false,
            } => {}
            Decision::Send { .. } => admitted.push(frame.pts),
        }
    }
    let mut command = [0; 9];
    commands.read_exact(&mut command).await.unwrap();
    assert_eq!(&command, b"keyframe\n");
    assert_eq!(admitted, vec![66_666]);
    assert_eq!(schedule.oversized_dropped, 1);
    assert_eq!(schedule.dependency_dropped, 1);
}

#[tokio::test(start_paused = true)]
async fn cleanup_watchdog_signal_precedes_simultaneous_expiry() {
    for _ in 0..32 {
        let app = app();
        app.active
            .lock()
            .await
            .completed
            .insert("failed".into(), (Instant::now(), false, "failed".into()));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            drain_with_shutdown(
                async {
                    stopped.await.unwrap();
                    Ok(())
                },
                stop,
                &app,
                async {
                    tokio::time::sleep(CLEANUP_CONFIRMATION_GRACE).await;
                    Ok(())
                },
                Duration::from_secs(1),
            )
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(CLEANUP_CONFIRMATION_GRACE).await;
        let error = task.await.unwrap().unwrap_err();
        assert_eq!(service_error_exit_code(&error), 1);
        assert_eq!(error.to_string(), "shutdown has unresolved earlier cleanup");
    }
}

#[tokio::test(start_paused = true)]
async fn cleanup_watchdog_existing_stop_uses_signal_status() {
    let app = app();
    {
        let mut book = app.active.lock().await;
        book.stopping = true;
        book.completed
            .insert("failed".into(), (Instant::now(), false, "failed".into()));
    }
    assert!(!wait_for_unconfirmed_cleanup(&app, Duration::ZERO).await);
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let error = drain_with_shutdown(
        async {
            stopped.await.unwrap();
            Ok(())
        },
        stop,
        &app,
        std::future::pending(),
        Duration::from_secs(1),
    )
    .await
    .unwrap_err();
    assert_eq!(service_error_exit_code(&error), 1);
    assert_eq!(error.to_string(), "shutdown has unresolved earlier cleanup");
}

async fn watchdog_drain_result(confirm: bool, stall: bool) -> Result<()> {
    let app = app();
    app.active
        .lock()
        .await
        .completed
        .insert("failed".into(), (Instant::now(), false, "failed".into()));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    drain_with_shutdown(
        async {
            stopped.await.unwrap();
            assert!(app.active.lock().await.stopping);
            tokio::task::yield_now().await;
            if confirm {
                app.active
                    .lock()
                    .await
                    .completed
                    .get_mut("failed")
                    .unwrap()
                    .1 = true;
            }
            if stall {
                std::future::pending::<()>().await;
            }
            Ok(())
        },
        stop,
        &app,
        std::future::pending(),
        Duration::from_secs(1),
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn cleanup_watchdog_recovery_during_drain_exits_successfully() {
    watchdog_drain_result(true, false).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn cleanup_watchdog_unconfirmed_after_drain_keeps_exit_75() {
    for stall in [false, true] {
        let error = watchdog_drain_result(false, stall).await.unwrap_err();
        assert_eq!(service_error_exit_code(&error), 75);
    }
}

#[tokio::test(start_paused = true)]
async fn cleanup_watchdog_recovery_preserves_drain_deadline_error() {
    let error = watchdog_drain_result(true, true).await.unwrap_err();
    assert_eq!(service_error_exit_code(&error), 1);
    assert!(error
        .to_string()
        .contains("HTTP shutdown drain deadline exceeded"));
}

async fn panic_session_slot_fixture(contended: bool) -> App {
    let app = app();
    let budget = Arc::new(tokio::sync::Semaphore::new(1));
    let request = uuid::Uuid::new_v4().to_string();
    let session = uuid::Uuid::new_v4().to_string();
    let slot = SessionSlotGuard {
        book: app.active.clone(),
        request: request.clone(),
        session: session.clone(),
        encoder: Some(reserve_encoder(&budget).unwrap()),
    };
    let (installed, installation) = tokio::sync::oneshot::channel();
    let (panic_now, panic_requested) = tokio::sync::oneshot::channel();
    let book = app.active.clone();
    let task = tokio::spawn(async move {
        let _slot = slot;
        book.lock().await.active = Some(Session {
            request: _slot.request.clone(),
            id: _slot.session.clone(),
            cancel: watch::channel(false).0,
            finished: watch::channel(None).1,
            confirmed: watch::channel(true).0,
            input: None,
            files: None,
        });
        installed.send(()).unwrap();
        panic_requested.await.unwrap();
        panic!("session task panic after installing active slot");
    });
    installation.await.unwrap();
    let held = if contended {
        Some(app.active.lock().await)
    } else {
        None
    };
    panic_now.send(()).unwrap();
    assert!(task.await.unwrap_err().is_panic());
    drop(held);
    tokio::task::yield_now().await;
    {
        let book = app.active.lock().await;
        assert!(book.active.is_none());
        assert_eq!(book.completed.len(), 1);
        assert!(!book.completed.get(&request).unwrap().1);
        assert_eq!(book.completed.get(&request).unwrap().2, session);
        assert!(book.retained_encoders.contains_key(&session));
        assert!(!book.stopping);
    }
    assert_eq!(budget.available_permits(), 0);
    let response = offer_with_encoder_budget(app.clone(), offer_request(), None, &budget).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"previous session cleanup unconfirmed");
    app
}

async fn panicked_session_slot(contended: bool) {
    let app = panic_session_slot_fixture(contended).await;
    let began = tokio::time::Instant::now();
    assert!(wait_for_unconfirmed_cleanup(&app, CLEANUP_CONFIRMATION_GRACE).await);
    assert_eq!(began.elapsed(), CLEANUP_CONFIRMATION_GRACE);
    assert!(app.active.lock().await.stopping);
}

#[tokio::test(start_paused = true)]
async fn session_slot_guard_panic_clears_active_and_arms_cleanup_watchdog() {
    panicked_session_slot(false).await;
}

#[tokio::test(start_paused = true)]
async fn session_slot_guard_panic_with_contended_book_arms_cleanup_watchdog() {
    panicked_session_slot(true).await;
}

#[tokio::test(start_paused = true)]
async fn session_slot_guard_panic_cleanup_watchdog_drains_with_exit_75() {
    for contended in [false, true] {
        let app = panic_session_slot_fixture(contended).await;
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let began = tokio::time::Instant::now();
        let error = drain_with_shutdown(
            async {
                stopped.await.unwrap();
                assert!(app.active.lock().await.stopping);
                Ok(())
            },
            stop,
            &app,
            std::future::pending(),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(began.elapsed(), Duration::from_secs(12));
        assert_eq!(service_error_exit_code(&error), 75);
        assert!(error.is::<CleanupUnconfirmedExit>());
    }
}

#[tokio::test]
async fn session_slot_guard_normal_finish_preserves_confirmed_cleanup() {
    let app = app();
    let budget = Arc::new(tokio::sync::Semaphore::new(1));
    let mut slot = SessionSlotGuard {
        book: app.active.clone(),
        request: "request".into(),
        session: "session".into(),
        encoder: Some(reserve_encoder(&budget).unwrap()),
    };
    {
        let mut book = app.active.lock().await;
        book.active = Some(Session {
            request: slot.request.clone(),
            id: slot.session.clone(),
            cancel: watch::channel(false).0,
            finished: watch::channel(None).1,
            confirmed: watch::channel(true).0,
            input: None,
            files: None,
        });
        slot.finish(&mut book, true);
        drop(slot);
        assert!(book.active.is_none());
        assert!(book.completed.get("request").unwrap().1);
        assert!(book.retained_encoders.is_empty());
    }
    tokio::task::yield_now().await;
    assert!(app.active.lock().await.completed.get("request").unwrap().1);
    assert_eq!(budget.available_permits(), 1);
}

#[tokio::test]
async fn cleanup_watchdog_signal_path_recovery_during_drain_exits_successfully() {
    let app = app();
    app.active
        .lock()
        .await
        .completed
        .insert("failed".into(), (Instant::now(), false, "failed".into()));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    drain_with_shutdown(
        async {
            stopped.await.unwrap();
            assert!(app.active.lock().await.stopping);
            app.active
                .lock()
                .await
                .completed
                .get_mut("failed")
                .unwrap()
                .1 = true;
            Ok(())
        },
        stop,
        &app,
        async { Ok(()) },
        Duration::from_secs(1),
    )
    .await
    .unwrap();
}
