use super::*;
use crate::runtime::{RecvMeta, default_runtime};
use std::{
    io::IoSliceMut,
    net::SocketAddr,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

#[derive(Debug, Default)]
struct Socket {
    pending: AtomicBool,
    calls: AtomicU64,
}
impl AsyncUdpSocket for Socket {
    fn poll_send(&self, _: &mut Context<'_>, t: &Transmit<'_>) -> Poll<io::Result<usize>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.pending.load(Ordering::Relaxed) {
            Poll::Pending
        } else {
            Poll::Ready(Ok(t.contents.len()))
        }
    }
    fn poll_recv(
        &self,
        _: &mut Context<'_>,
        _: &mut [IoSliceMut<'_>],
        _: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        Poll::Pending
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok("127.0.0.1:12345".parse().unwrap())
    }
}
fn gate() -> Arc<MediaEgress> {
    let g = Arc::new(MediaEgress::with_runtime(default_runtime().unwrap()));
    g.enable(108).unwrap();
    let mut state = g.state.lock().unwrap();
    state.deadlines = Some(Table::new(Binding {
        primary: (7, 108),
        rtx: Some((8, 109)),
    }));
    state.proof_budget = state.deadlines.as_ref().map(|t| t.budget.clone());
    state.au_deadlines_enabled = true;
    drop(state);
    g
}
fn packet(ts: u32) -> Vec<u8> {
    let mut b = vec![0; 24];
    b[0] = 128;
    b[1] = 108;
    b[4..8].copy_from_slice(&ts.to_be_bytes());
    b[8..12].copy_from_slice(&7u32.to_be_bytes());
    b
}
fn rtcp() -> Vec<u8> {
    vec![128, 200, 0, 1, 0, 0, 0, 7, 0, 0, 0, 0]
}
fn channel(data: &[u8]) -> Vec<u8> {
    let mut b = vec![0x40, 0, 0, 0];
    b[2..4].copy_from_slice(&(data.len() as u16).to_be_bytes());
    b.extend_from_slice(data);
    b
}
fn indication(data: &[u8]) -> Vec<u8> {
    let mut b = vec![
        0, 0x16, 0, 0, 0x21, 0x12, 0xa4, 0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    for (typ, value) in [(0x12u16, &[0, 1, 0, 0, 0, 0, 0, 0][..]), (0x13, data)] {
        b.extend_from_slice(&typ.to_be_bytes());
        b.extend_from_slice(&(value.len() as u16).to_be_bytes());
        b.extend_from_slice(value);
        while b.len() % 4 != 0 {
            b.push(0);
        }
    }
    let n = b.len() - 20;
    b[2..4].copy_from_slice(&(n as u16).to_be_bytes());
    b
}
async fn send(
    g: &MediaEgress,
    s: &dyn AsyncUdpSocket,
    b: &[u8],
    seg: Option<usize>,
    turn: bool,
) -> io::Result<usize> {
    let mut owners = Vec::new();
    for part in b.chunks(seg.unwrap_or(b.len())) {
        let owner = if g.snapshot().fenced {
            None
        } else if let Some(key) = inspect(part, turn).map_err(|r| g.reject_reason(r))? {
            g.capture_attempt(&rtc::rtp::Packet {
                header: rtc::rtp::header::Header {
                    version: 2,
                    ssrc: key.identity.ssrc,
                    payload_type: key.identity.payload,
                    timestamp: key.identity.timestamp,
                    sequence_number: key.sequence,
                    marker: key.marker,
                    ..Default::default()
                },
                payload: vec![0; 12].into(),
            })?
        } else {
            None
        };
        owners.push(owner);
    }
    g.send_owned(
        s,
        &Transmit {
            destination: "127.0.0.1:12345".parse().unwrap(),
            ecn: None,
            contents: b,
            segment_size: seg,
            src_ip: None,
        },
        turn,
        &owners.iter().map(Option::as_ref).collect::<Vec<_>>(),
    )
    .await
}
fn register(g: &Arc<MediaEgress>, ts: u32, deadline: Instant) {
    AuDeadlineWriter { gate: g.clone() }
        .register(ts, deadline)
        .unwrap();
}

#[tokio::test]
async fn pending_deadline_wakes_without_writable_socket_then_control_survives() {
    let g = gate();
    let socket = Socket::default();
    let deadline = Instant::now() + Duration::from_millis(40);
    register(&g, 1, deadline);
    send(&g, &socket, &packet(1), None, false).await.unwrap(); // first part of AU already submitted on time
    socket.pending.store(true, Ordering::Relaxed);
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            send(&g, &socket, &packet(1), None, false)
        )
        .await
        .unwrap()
        .is_err()
    );
    let count = socket.calls.load(Ordering::Relaxed);
    let s = g.snapshot();
    assert!(s.fenced && !s.failed && !s.pending_media_send);
    assert_eq!(s.policy_stop_reason, Some("au_deadline_expired"));
    assert_eq!(s.media_submitted_datagrams, 1);
    assert_eq!(s.expired_datagrams, 1);
    assert_eq!(s.first_expiry_stage, Some("after_socket_pending"));
    assert_eq!(s.first_expiry_turn, Some(false));
    assert!(s.first_expiry_first_poll_remaining_us.unwrap() > 0);
    assert!(s.first_expiry_socket_pending_polls > 0);
    assert!(s.first_expiry_send_elapsed_us >= 20_000);
    socket.pending.store(false, Ordering::Relaxed);
    send(&g, &socket, &packet(1), None, false).await.unwrap();
    assert_eq!(socket.calls.load(Ordering::Relaxed), count);
    send(&g, &socket, &rtcp(), None, false).await.unwrap();
    assert_eq!(g.snapshot().control_submitted_datagrams, 1);
}
#[tokio::test]
async fn mixed_au_gso_uses_original_earliest_deadline_and_turn_has_same_rule() {
    for mode in 0..3 {
        let g = gate();
        let s = Socket::default();
        s.pending.store(true, Ordering::Relaxed);
        let now = Instant::now();
        register(&g, 1, now + Duration::from_millis(25));
        register(&g, 2, now + Duration::from_millis(200));
        let wrap = |b: Vec<u8>| match mode {
            1 => channel(&b),
            2 => indication(&b),
            _ => b,
        };
        let mut b = wrap(packet(2));
        let n = b.len();
        b.extend_from_slice(&wrap(packet(1)));
        assert!(
            tokio::time::timeout(Duration::from_secs(1), send(&g, &s, &b, Some(n), mode != 0))
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(g.snapshot().expired_datagrams, 2);
        assert_eq!(g.snapshot().media_submitted_datagrams, 0);
        assert!(Instant::now() < now + Duration::from_millis(200));
    }
}
#[tokio::test]
async fn idle_expired_metadata_does_not_stop_but_late_sender_and_cancel_are_bounded() {
    let g = gate();
    let s = Socket::default();
    register(&g, 1, Instant::now() + Duration::from_millis(10));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!g.snapshot().fenced);
    send(&g, &s, &rtcp(), None, false).await.unwrap();
    assert!(!g.snapshot().fenced);
    assert!(send(&g, &s, &packet(1), None, false).await.is_err());
    assert_eq!(s.calls.load(Ordering::Relaxed), 1);
    let g = gate();
    register(&g, 1, Instant::now() + Duration::from_millis(200));
    s.pending.store(true, Ordering::Relaxed);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(5),
            send(&g, &s, &packet(1), None, false)
        )
        .await
        .is_err()
    );
    assert!(!g.snapshot().pending_media_send);
    assert!(!g.snapshot().fenced);
    g.fence();
    send(&g, &s, &packet(1), None, false).await.unwrap();
    assert!(!g.snapshot().failed);
}
#[tokio::test]
async fn already_expired_before_enqueue_is_policy_stop_not_uncertain_cleanup() {
    let g = gate();
    let mut writer = AuDeadlineWriter { gate: g.clone() };
    assert!(
        writer
            .register(1, Instant::now() - Duration::from_millis(1))
            .is_err()
    );
    let s = g.snapshot();
    assert!(s.fenced && !s.failed);
    assert_eq!(s.registered_access_units, 0);
    assert_eq!(s.policy_stop_reason, Some("au_deadline_expired"));
    let g = gate();
    assert!(
        AuDeadlineWriter { gate: g.clone() }
            .register(1, Instant::now() + Duration::from_secs(1))
            .is_err()
    );
    assert!(g.snapshot().failed);
}
#[tokio::test]
async fn actual_nack_responder_rtx_retains_deadline_through_srtp_and_turn() {
    use rtc::interceptor::{
        Attribute, AttributedPacket, Interceptor, Packet, RTCPFeedback, Registry, Slot, StreamInfo,
        TaggedPacket,
    };
    use rtc::rtcp::transport_feedbacks::transport_layer_nack::{NackPair, TransportLayerNack};
    use rtc::sansio::Protocol;
    use rtc::shared::{TransportContext, marshal::Marshal};
    use rtc::srtp::{context::Context as SrtpContext, protection_profile::ProtectionProfile};
    for rtx in [false, true] {
        let g = Arc::new(MediaEgress::with_runtime(default_runtime().unwrap()));
        g.enable(108).unwrap();
        let (responder, monitor) = crate::bounded_nack::BoundedNack::new();
        monitor.attach(g.clone()).unwrap();
        let mut chain = Registry::new().with(Slot::NackResponder, responder).build();
        chain.bind_local_stream(&StreamInfo {
            ssrc: 7,
            payload_type: 108,
            ssrc_rtx: rtx.then_some(8),
            payload_type_rtx: rtx.then_some(109),
            clock_rate: 90000,
            mime_type: "video/H264".into(),
            rtcp_feedback: vec![RTCPFeedback {
                typ: "nack".into(),
                parameter: String::new(),
            }],
            ..Default::default()
        });
        {
            let mut state = g.state.lock().unwrap();
            let table = Table::new(state.owned_responder.unwrap());
            state.proof_budget = Some(table.budget.clone());
            state.deadlines = Some(table);
            state.au_deadlines_enabled = true;
        }
        let now = Instant::now();
        register(&g, 42, now + Duration::from_millis(100));
        let original = rtc::rtp::Packet {
            header: rtc::rtp::header::Header {
                version: 2,
                ssrc: 7,
                payload_type: 108,
                sequence_number: 17,
                timestamp: 42,
                marker: true,
                ..Default::default()
            },
            payload: vec![1, 2, 3].into(),
        };
        chain
            .handle_write(TaggedPacket {
                now,
                transport: TransportContext::default(),
                message: {
                    let mut p = AttributedPacket::new(Packet::Rtp(original.clone()));
                    p.attributes.push(Attribute::Custom(Arc::new(
                        g.capture_attempt(&original).unwrap().unwrap(),
                    )));
                    p
                },
            })
            .unwrap();
        while chain.poll_write().is_some() {}
        let nack_now = Instant::now();
        chain
            .handle_read(TaggedPacket {
                now: nack_now,
                transport: TransportContext::default(),
                message: AttributedPacket::new(Packet::Rtcp(vec![Box::new(TransportLayerNack {
                    sender_ssrc: 9,
                    media_ssrc: 7,
                    nacks: vec![NackPair {
                        packet_id: 17,
                        lost_packets: 0,
                    }],
                })])),
            })
            .unwrap();
        // Hand off while eligible. Once outside responder ownership, the
        // existing final expiry fence remains mandatory, including RTX/TURN.
        let tagged = chain.poll_write().unwrap();
        tokio::time::sleep(Duration::from_millis(110)).await;
        for ts in 43..=170 {
            register(&g, ts, Instant::now() + Duration::from_millis(200));
        }
        assert_eq!(g.snapshot().registered_access_units, 129);
        assert_eq!(g.snapshot().au_records, 128);
        assert_eq!(tagged.now, nack_now);
        let owner = tagged
            .message
            .custom::<LocalPacketContext>()
            .cloned()
            .unwrap();
        let Packet::Rtp(retransmission) = tagged.message.packet else {
            panic!("no RTP")
        };
        assert_eq!(retransmission.header.timestamp, 42);
        assert_eq!(retransmission.header.ssrc, if rtx { 8 } else { 7 });
        assert_eq!(
            retransmission.header.payload_type,
            if rtx { 109 } else { 108 }
        );
        if rtx {
            assert_eq!(&retransmission.payload[..2], &17u16.to_be_bytes());
        }
        let provider = rtc::crypto::default_provider().unwrap();
        let mut crypto = SrtpContext::new(
            &[0x42; 16],
            &[0x24; 14],
            ProtectionProfile::Aes128CmHmacSha1_80,
            None,
            None,
            provider.crypto(),
        )
        .unwrap();
        let encrypted = crypto
            .encrypt_rtp(&retransmission.marshal().unwrap())
            .unwrap();
        let socket = Socket::default();
        assert!(
            g.send_owned(
                &socket,
                &Transmit {
                    destination: "127.0.0.1:12345".parse().unwrap(),
                    ecn: None,
                    contents: &channel(&encrypted),
                    segment_size: None,
                    src_ip: None
                },
                true,
                &[Some(&owner)]
            )
            .await
            .is_err()
        );
        assert_eq!(socket.calls.load(Ordering::Relaxed), 0);
        assert_eq!(g.snapshot().policy_stop_reason, Some("au_deadline_expired"));
        assert!(!g.snapshot().failed);
    }
}

#[tokio::test]
async fn actual_binding_changes_and_duplicate_metadata_fail_closed() {
    let g = Arc::new(MediaEgress::with_runtime(default_runtime().unwrap()));
    let p = super::super::au_deadline::test_parameters();
    g.register_sender(0usize.into(), &p).unwrap();
    g.enable(108).unwrap();
    g.register_owned_responder((7, 108), None).unwrap();
    let mut writer = g.enable_au_deadlines(&p, 7, 108).unwrap();
    writer
        .register(1, Instant::now() + Duration::from_millis(150))
        .unwrap();
    assert!(
        writer
            .register(1, Instant::now() + Duration::from_millis(200))
            .is_err()
    );
    assert!(g.snapshot().failed);
    let g = Arc::new(MediaEgress::with_runtime(default_runtime().unwrap()));
    g.register_sender(0usize.into(), &p).unwrap();
    g.enable(108).unwrap();
    g.register_owned_responder((7, 108), None).unwrap();
    let _writer = g.enable_au_deadlines(&p, 7, 108).unwrap();
    let mut changed = p.clone();
    changed.encodings[0].rtp_coding_parameters.ssrc = Some(9);
    assert!(g.register_sender(0usize.into(), &changed).is_err());
    assert!(g.snapshot().failed);
    let g = Arc::new(MediaEgress::with_runtime(default_runtime().unwrap()));
    g.register_sender(0usize.into(), &p).unwrap();
    g.register_sender(1usize.into(), &p).unwrap();
    g.enable(108).unwrap();
    assert!(g.enable_au_deadlines(&p, 7, 108).is_err());
}
#[tokio::test]
async fn finite_hold_is_final_poll_only_and_never_refreshes_original_deadline() {
    let g = gate();
    g.arm_au_hold_diagnostic().unwrap();
    let s = Socket::default();
    register(&g, 1, Instant::now() + Duration::from_millis(200));
    send(&g, &s, &packet(1), None, false).await.unwrap();
    assert!(!g.snapshot().fault_au_hold_triggered);
    // Move only the diagnostic's relative trigger origin. AU deadline still uses
    // the real runtime clock, and no test callback substitutes the socket boundary.
    g.state.lock().unwrap().first_media_submission = Some(Instant::now() - Duration::from_secs(10));
    register(&g, 2, Instant::now() + Duration::from_millis(25));
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            send(&g, &s, &packet(2), None, false)
        )
        .await
        .unwrap()
        .is_err()
    );
    let state = g.snapshot();
    assert_eq!(s.calls.load(Ordering::Relaxed), 1);
    assert!(state.fault_au_hold_triggered);
    assert!(state.fault_au_hold_after_first_submit_ms.unwrap() >= 10_000);
    assert_eq!(state.expired_datagrams, 1);
    assert!(!state.failed);
    send(&g, &s, &rtcp(), None, false).await.unwrap();
    assert_eq!(g.snapshot().control_submitted_datagrams, 1);
}

#[tokio::test]
async fn full_poll_completion_overrun_counts_real_submission_then_policy_stops() {
    #[derive(Debug)]
    struct SlowSocket;
    impl AsyncUdpSocket for SlowSocket {
        fn poll_send(&self, _: &mut Context<'_>, t: &Transmit<'_>) -> Poll<io::Result<usize>> {
            std::thread::sleep(Duration::from_millis(100));
            Poll::Ready(Ok(t.contents.len()))
        }
        fn poll_recv(
            &self,
            _: &mut Context<'_>,
            _: &mut [IoSliceMut<'_>],
            _: &mut [RecvMeta],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok("127.0.0.1:12345".parse().unwrap())
        }
    }
    let g = gate();
    register(&g, 1, Instant::now() + Duration::from_millis(50));
    let bytes = packet(1);
    assert!(send(&g, &SlowSocket, &bytes, None, false).await.is_err());
    let state = g.snapshot();
    assert_eq!(state.media_submitted_datagrams, 1);
    assert_eq!(state.completion_overrun_datagrams, 1);
    assert_eq!(state.first_expiry_stage, Some("completion_overrun"));
    assert_eq!(state.first_expiry_turn, Some(false));
    assert!(state.first_expiry_first_poll_remaining_us.unwrap() > 0);
    assert_eq!(state.first_expiry_socket_pending_polls, 0);
    assert!(state.first_expiry_send_elapsed_us >= 100_000);
    assert_eq!(state.expired_datagrams, 0);
    assert!(state.fenced && !state.failed);
    assert_eq!(state.policy_stop_reason, Some("au_deadline_expired"));
}
