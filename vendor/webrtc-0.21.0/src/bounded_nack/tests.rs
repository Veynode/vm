use super::*;
use rtc::rtcp::{
    payload_feedbacks::picture_loss_indication::PictureLossIndication,
    transport_feedbacks::transport_layer_nack::NackPair,
};
fn info(rtx: bool) -> StreamInfo {
    StreamInfo {
        ssrc: 7,
        payload_type: 108,
        mime_type: "video/H264".into(),
        rtcp_feedback: vec![rtc::interceptor::RTCPFeedback {
            typ: "nack".into(),
            parameter: String::new(),
        }],
        ssrc_rtx: rtx.then_some(8),
        payload_type_rtx: rtx.then_some(109),
        ..Default::default()
    }
}
fn fixture(rtx: bool) -> (BoundedNack, Monitor, Arc<MediaEgress>) {
    let (mut n, m) = BoundedNack::new();
    let gate = Arc::new(MediaEgress::default());
    gate.enable(108).unwrap();
    m.attach(gate.clone()).unwrap();
    n.bind_local_stream(&info(rtx));
    assert!(!m.snapshot().counters.failed);
    (n, m, gate)
}
fn packet(sequence: u16, len: usize) -> RtpPacket {
    RtpPacket {
        header: rtc::rtp::header::Header {
            version: 2,
            ssrc: 7,
            payload_type: 108,
            sequence_number: sequence,
            timestamp: 123,
            marker: true,
            ..Default::default()
        },
        payload: vec![0xaa; len].into(),
    }
}
fn tag(packet: Packet) -> TaggedPacket {
    TaggedPacket {
        now: Instant::now(),
        transport: TransportContext::default(),
        message: AttributedPacket::new(packet),
    }
}
fn original(n: &mut BoundedNack, p: RtpPacket) {
    n.handle_write(tag(Packet::Rtp(p))).unwrap();
    assert!(n.poll_write().is_some());
}
fn nack(ssrc: u32, pairs: Vec<NackPair>) -> Box<dyn rtc::rtcp::Packet> {
    Box::new(TransportLayerNack {
        media_ssrc: ssrc,
        sender_ssrc: 9,
        nacks: pairs,
    })
}
fn pair(id: u16, bits: u16) -> NackPair {
    NackPair {
        packet_id: id,
        lost_packets: bits,
    }
}
fn send_nack(n: &mut BoundedNack, pairs: Vec<NackPair>) {
    n.handle_read(tag(Packet::Rtcp(vec![nack(7, pairs)])))
        .unwrap();
    assert!(n.poll_read().is_some());
}
fn drain(n: &mut BoundedNack) -> Vec<RtpPacket> {
    let mut out = vec![];
    while let Some(msg) = n.poll_write() {
        if let Packet::Rtp(p) = msg.message.packet {
            out.push(p);
        }
    }
    out
}
#[test]
fn compound_and_pending_dedup_allow_later_retry() {
    let (mut n, m, _) = fixture(false);
    for seq in 10..15 {
        original(&mut n, packet(seq, 20));
    }
    n.handle_read(tag(Packet::Rtcp(vec![
        nack(7, vec![pair(10, 15)]),
        nack(7, vec![pair(11, 7)]),
    ])))
    .unwrap();
    n.poll_read();
    send_nack(&mut n, vec![pair(10, 15)]);
    assert_eq!(m.snapshot().counters.pending_repair_packets, 5);
    assert_eq!(m.snapshot().counters.duplicate_requests, 9);
    assert_eq!(drain(&mut n).len(), 5);
    send_nack(&mut n, vec![pair(10, 0)]);
    assert_eq!(drain(&mut n).len(), 1);
}
#[test]
fn repair_byte_and_packet_limits_are_independent() {
    for len in [10, 1500] {
        let (mut n, m, _) = fixture(false);
        for seq in 0..200 {
            original(&mut n, packet(seq, len));
        }
        send_nack(&mut n, (0..200).map(|s| pair(s, 0)).collect());
        let s = m.snapshot().counters;
        assert!(s.pending_repair_packets <= 128);
        assert!(s.pending_repair_bytes <= REPAIR_BYTES);
        assert!(s.repair_suppressed_packets > 0);
        if len == 10 {
            assert_eq!(s.pending_repair_packets, 128);
        } else {
            assert!(s.pending_repair_packets < 128);
        }
        assert!(!s.failed);
        assert_eq!(drain(&mut n).len(), s.pending_repair_packets);
    }
}
#[test]
fn cache_byte_pressure_and_wire_copy_sever_large_backing() {
    let (mut n, m, _) = fixture(false);
    let big = Bytes::from(vec![0xab; 4 * 1024 * 1024]);
    let mut p = packet(0, 0);
    p.payload = big.slice(100..1600);
    let oldptr = p.payload.as_ptr();
    original(&mut n, p);
    {
        let s = m.0.state.lock().unwrap();
        let e = s.cache[0].as_ref().unwrap();
        assert_eq!(e.wire.len(), 1512);
        assert_ne!(e.wire.as_ptr(), oldptr);
    }
    for seq in 1..1000 {
        original(&mut n, packet(seq, 1500));
    }
    let s = m.snapshot().counters;
    assert!(s.cache_packets < 1024);
    assert!(s.cache_bytes <= CACHE_BYTES);
    assert!(s.cache_evictions > 0);
    assert_eq!(s.cache_bytes, s.cache_packets * 1512);
}
#[test]
fn oversized_cache_original_still_forwarded_and_old_sequence_cannot_replace_new() {
    let (mut n, m, _) = fixture(false);
    original(&mut n, packet(0, 3000));
    assert_eq!(m.snapshot().counters.cache_skipped_oversize, 1);
    original(&mut n, packet(1024, 10));
    original(&mut n, packet(0, 10));
    send_nack(&mut n, vec![pair(1024, 0), pair(0, 0)]);
    let out = drain(&mut n);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].header.sequence_number, 1024);
}
#[test]
fn wrap_gap_and_pending_generation_do_not_alias() {
    let (mut n, m, _) = fixture(false);
    for s in [65534, 65535, 0, 1] {
        original(&mut n, packet(s, 10));
    }
    send_nack(&mut n, vec![pair(65534, 7)]);
    assert_eq!(drain(&mut n).len(), 4);
    original(&mut n, packet(30000, 10));
    assert_eq!(m.snapshot().counters.cache_packets, 1);
    send_nack(&mut n, vec![pair(0, 0), pair(30000, 0)]);
    assert_eq!(drain(&mut n).len(), 1);
}
#[test]
fn compound_budget_unknown_ssrc_and_control_forwarding() {
    let (mut n, m, _) = fixture(false);
    original(&mut n, packet(1, 10));
    n.handle_read(tag(Packet::Rtcp(vec![
        nack(99, vec![pair(1, 0); 200]),
        nack(7, vec![pair(1, 0); 200]),
        nack(7, vec![pair(1, 0); 200]),
        Box::new(PictureLossIndication {
            sender_ssrc: 9,
            media_ssrc: 7,
        }),
    ])))
    .unwrap();
    let msg = n.poll_read().unwrap();
    assert!(matches!(msg.message.packet,Packet::Rtcp(ref p) if p.len()==4));
    let s = m.snapshot().counters;
    assert_eq!(s.nack_pairs_processed, 256);
    assert_eq!(s.nack_budget_exhaustions, 1);
    assert_eq!(s.unknown_stream_nacks, 1);
    assert_eq!(drain(&mut n).len(), 1);
}
#[test]
fn padding_extensions_roundtrip_and_rtx_identity() {
    for rtx in [false, true] {
        let (mut n, _, _) = fixture(rtx);
        let mut p = packet(4, 23);
        p.header.padding = true;
        p.header.csrc = vec![12, 13];
        p.header
            .set_extension(1, Bytes::from_static(b"abcd"))
            .unwrap();
        let expected = p.marshal().unwrap();
        original(&mut n, p.clone());
        send_nack(&mut n, vec![pair(4, 0)]);
        let out = drain(&mut n).remove(0);
        assert_eq!(out.header.timestamp, 123);
        assert!(out.header.marker);
        if rtx {
            assert_eq!((out.header.ssrc, out.header.payload_type), (8, 109));
            assert_eq!(&out.payload[..2], &4u16.to_be_bytes());
            assert_eq!(&out.payload[2..], &p.payload[..]);
            assert_eq!(out.header.csrc, p.header.csrc);
            assert_eq!(out.header.extensions, p.header.extensions);
            assert!(!out.header.padding);
        } else {
            assert_eq!(out.marshal().unwrap(), expected);
        }
    }
}
#[test]
fn fatal_forward_count_and_bytes_fence_even_if_chain_swallows_errors() {
    for len in [10, 10000] {
        let (mut n, m, gate) = fixture(false);
        let count = if len == 10 {
            PASS_PACKETS
        } else {
            PASS_BYTES / (len + 12)
        };
        for seq in 0..count {
            n.handle_write(tag(Packet::Rtp(packet(seq as u16, len))))
                .unwrap();
        }
        assert!(
            n.handle_write(tag(Packet::Rtp(packet(count as u16, len))))
                .is_err()
        );
        assert!(m.snapshot().counters.failed);
        assert!(gate.snapshot().fenced);
    }
    let (n, m, gate) = fixture(false);
    let mut chain = rtc::interceptor::Registry::new()
        .with(rtc::interceptor::Slot::NackResponder, n)
        .build();
    let mut wrong = packet(0, 10);
    wrong.header.ssrc = 99;
    assert!(chain.handle_write(tag(Packet::Rtp(wrong))).is_ok());
    assert!(m.snapshot().counters.failed);
    assert!(gate.snapshot().fenced);
}
#[test]
fn unbind_close_clear_actual_resources_and_conserve_repairs() {
    let (mut n, m, _) = fixture(true);
    original(&mut n, packet(1, 100));
    send_nack(&mut n, vec![pair(1, 0)]);
    n.unbind_local_stream(&info(true));
    assert_eq!(m.snapshot().counters.repair_discarded_on_unbind, 1);
    n.close().unwrap();
    let s = m.snapshot().counters;
    assert!(s.closed);
    assert!(!s.bound);
    assert_eq!(
        (
            s.cache_bytes,
            s.cache_packets,
            s.pending_repair_bytes,
            s.pending_repair_packets,
            s.pending_read_bytes,
            s.pending_forward_bytes
        ),
        (0, 0, 0, 0, 0, 0)
    );
    assert_eq!(
        s.repair_enqueued_packets,
        s.repair_dequeued_packets + s.repair_discarded_on_unbind
    );
}
#[test]
fn unsupported_binding_and_custom_attributes_fail_closed() {
    let (mut n, m, gate) = fixture(false);
    n.bind_local_stream(&info(false));
    assert!(m.snapshot().counters.failed);
    assert!(gate.snapshot().fenced);
    let (mut n, m, gate) = fixture(false);
    let mut msg = tag(Packet::Rtp(packet(1, 10)));
    msg.message
        .attributes
        .push(Attribute::Custom(Arc::new(vec![0; 1_000_000])));
    assert!(n.handle_write(msg).is_err());
    assert!(m.snapshot().counters.failed);
    assert!(gate.snapshot().fenced);
}
#[test]
fn unbind_fences_and_discards_queued_original_but_preserves_control() {
    let (mut n, m, gate) = fixture(false);
    n.handle_write(tag(Packet::Rtp(packet(1, 100)))).unwrap();
    n.handle_write(tag(Packet::Rtcp(vec![Box::new(PictureLossIndication {
        sender_ssrc: 9,
        media_ssrc: 7,
    })])))
    .unwrap();
    send_nack(&mut n, vec![pair(1, 0)]);
    n.unbind_local_stream(&info(false));
    assert!(gate.snapshot().fenced);
    let msg = n.poll_write().unwrap();
    assert!(matches!(msg.message.packet, Packet::Rtcp(_)));
    assert!(n.poll_write().is_none());
    let s = m.snapshot().counters;
    assert_eq!(s.pending_forward_packets, 0);
    assert_eq!(s.pending_repair_packets, 0);
    assert_eq!(s.repair_discarded_on_unbind, 1);
}
#[test]
fn compact_forward_backing_and_attribute_carrier() {
    let (mut n, m, _) = fixture(false);
    let backing = Bytes::from(vec![4; 4 * 1024 * 1024]);
    let mut p = packet(1, 0);
    p.payload = backing.slice(100..200);
    let old = p.payload.as_ptr();
    n.handle_write(tag(Packet::Rtp(p))).unwrap();
    let out = n.poll_write().unwrap();
    let Packet::Rtp(out) = out.message.packet else {
        panic!()
    };
    assert_ne!(out.payload.as_ptr(), old);
    assert_eq!(out.payload.len(), 100);
    let mut carrier = tag(Packet::Rtcp(vec![]));
    carrier
        .message
        .attributes
        .push(Attribute::TargetBitrateChanged {
            bits_per_second: 1234.,
        });
    n.handle_write(carrier).unwrap();
    let out = n.poll_write().unwrap();
    assert!(matches!(out.message.packet,Packet::Rtcp(ref p) if p.is_empty()));
    assert_eq!(out.message.attributes.len(), 1);
    assert!(!m.snapshot().counters.failed);
}
#[test]
fn read_queue_limit_and_excess_compound_are_sticky_not_silent() {
    let (mut n, m, gate) = fixture(false);
    for _ in 0..PASS_PACKETS {
        n.handle_read(tag(Packet::Rtcp(vec![Box::new(PictureLossIndication {
            sender_ssrc: 9,
            media_ssrc: 7,
        })])))
        .unwrap();
    }
    assert!(n.handle_read(tag(Packet::Rtcp(vec![]))).is_err());
    assert_eq!(m.snapshot().counters.failure_reason, Some("read_capacity"));
    assert!(gate.snapshot().fenced);
    let (mut n, m, gate) = fixture(false);
    let packets = (0..MESSAGES + 1)
        .map(|_| {
            Box::new(PictureLossIndication {
                sender_ssrc: 9,
                media_ssrc: 7,
            }) as Box<dyn rtc::rtcp::Packet>
        })
        .collect();
    assert!(n.handle_read(tag(Packet::Rtcp(packets))).is_err());
    assert!(m.snapshot().counters.failed);
    assert!(gate.snapshot().fenced);
}
#[test]
fn early_binding_failure_is_latched_at_gate_attachment() {
    let (mut n, m) = BoundedNack::new();
    n.bind_local_stream(&info(false));
    assert!(m.snapshot().counters.failed);
    let gate = Arc::new(MediaEgress::default());
    gate.enable(108).unwrap();
    m.attach(gate.clone()).unwrap();
    assert!(gate.snapshot().fenced);
}
#[test]
fn nack_not_negotiated_forwards_without_cache_or_repair() {
    let (mut n, m) = BoundedNack::new();
    let gate = Arc::new(MediaEgress::default());
    m.attach(gate).unwrap();
    let mut i = info(false);
    i.rtcp_feedback.clear();
    n.bind_local_stream(&i);
    original(&mut n, packet(1, 100));
    send_nack(&mut n, vec![pair(1, 0)]);
    assert!(drain(&mut n).is_empty());
    assert_eq!(m.snapshot().counters.cache_packets, 0);
    assert!(!m.snapshot().counters.failed);
}
#[test]
fn rtx_declared_rid_without_value_is_allowed_but_actual_rid_is_refused() {
    let (mut n, m) = BoundedNack::new();
    let gate = Arc::new(MediaEgress::default());
    gate.enable(108).unwrap();
    m.attach(gate.clone()).unwrap();
    let mut i = info(true);
    i.rtp_header_extensions
        .push(rtc::interceptor::RTPHeaderExtension {
            id: 3,
            uri: "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id".into(),
        });
    n.bind_local_stream(&i);
    original(&mut n, packet(1, 100));
    send_nack(&mut n, vec![pair(1, 0)]);
    assert_eq!(drain(&mut n).len(), 1);
    let mut p = packet(2, 100);
    p.header
        .set_extension(3, Bytes::from_static(b"rid"))
        .unwrap();
    assert!(n.handle_write(tag(Packet::Rtp(p))).is_err());
    assert_eq!(
        m.snapshot().counters.failure_reason,
        Some("rtx_rid_unsupported")
    );
    assert!(gate.snapshot().fenced);
}
#[test]
fn rtx_osn_is_reserved_before_allocation_and_sequence_wraps() {
    let (mut n, m, _) = fixture(true);
    for seq in 0..128 {
        original(&mut n, packet(seq, 1012));
    } // wire 1024; RTX 1026.
    send_nack(&mut n, (0..128).map(|s| pair(s, 0)).collect());
    let c = m.snapshot().counters;
    assert_eq!(c.pending_repair_packets, 127);
    assert_eq!(c.pending_repair_bytes, 127 * 1026);
    assert_eq!(c.repair_suppressed_packets, 1);
    drain(&mut n);
    {
        m.0.state.lock().unwrap().rtx_sequence = 65535;
    }
    send_nack(&mut n, vec![pair(0, 1)]);
    let packets = drain(&mut n);
    assert_eq!(
        packets
            .iter()
            .map(|p| p.header.sequence_number)
            .collect::<Vec<_>>(),
        vec![65535, 0]
    );
}
#[tokio::test]
async fn fatal_wakes_pending_monitor_and_cannot_be_cleared_by_close() {
    let (mut n, m, gate) = fixture(false);
    let observer = m.clone();
    let waiter = tokio::spawn(async move {
        observer.failed().await;
    });
    tokio::task::yield_now().await;
    let mut p = packet(1, 10);
    p.header.ssrc = 77;
    assert!(n.handle_write(tag(Packet::Rtp(p))).is_err());
    tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap();
    n.close().unwrap();
    assert!(m.snapshot().counters.failed);
    assert!(gate.snapshot().fenced);
}

#[cfg(feature = "runtime-mock")]
mod expiry_tests {
    use super::*;
    // Virtual-clock coverage: receive timestamps deliberately remain stale while
    // the exact gate clock advances. This does not sleep or alter original budgets.
    fn expiry_fixture(
        rtx: bool,
    ) -> (
        BoundedNack,
        Monitor,
        Arc<MediaEgress>,
        Arc<crate::runtime::mock::MockRuntime>,
    ) {
        use crate::runtime::Runtime;
        use rtc::rtp_transceiver::rtp_sender::{
            RTCRtpCodec, RTCRtpCodecParameters, RTCRtpRtxParameters,
        };
        let clock = Arc::new(crate::runtime::mock::MockRuntime::new());
        let gate = Arc::new(MediaEgress::with_runtime(clock.clone()));
        let (mut n, m) = BoundedNack::new();
        m.attach(gate.clone()).unwrap();
        n.bind_local_stream(&info(rtx));
        let mut p = rtc::rtp_transceiver::rtp_sender::RTCRtpSendParameters::default();
        p.encodings
            .push(rtc::rtp_transceiver::rtp_sender::RTCRtpEncodingParameters {
                rtp_coding_parameters: rtc::rtp_transceiver::rtp_sender::RTCRtpCodingParameters {
                    ssrc: Some(7),
                    ..Default::default()
                },
                ..Default::default()
            });
        p.rtp_parameters.codecs.push(RTCRtpCodecParameters {
            payload_type: 108,
            rtp_codec: RTCRtpCodec {
                mime_type: "video/H264".into(),
                clock_rate: 90000,
                ..Default::default()
            },
        });
        if rtx {
            p.encodings[0].rtp_coding_parameters.rtx = Some(RTCRtpRtxParameters { ssrc: 8 });
            p.rtp_parameters.codecs.push(RTCRtpCodecParameters {
                payload_type: 109,
                rtp_codec: RTCRtpCodec {
                    mime_type: "video/rtx".into(),
                    clock_rate: 90000,
                    sdp_fmtp_line: "apt=108".into(),
                    ..Default::default()
                },
            });
        }
        gate.register_sender(0usize.into(), &p).unwrap();
        gate.enable(108).unwrap();
        let mut w = gate.enable_au_deadlines(&p, 7, 108).unwrap();
        w.register(123, clock.now() + std::time::Duration::from_millis(250))
            .unwrap();
        (n, m, gate, clock)
    }
    fn owned_original(n: &mut BoundedNack, g: &Arc<MediaEgress>, seq: u16, drain: bool) {
        let p = packet(seq, 200);
        let context = g.capture_attempt(&p).unwrap().unwrap();
        let mut msg = tag(Packet::Rtp(p));
        msg.message
            .attributes
            .push(Attribute::Custom(Arc::new(context)));
        n.handle_write(msg).unwrap();
        if drain {
            drop(n.poll_write().unwrap());
        }
    }
    fn conserved(c: &Counters) {
        assert_eq!(
            c.repair_enqueued_packets,
            c.repair_dequeued_packets
                + c.repair_discarded_on_unbind
                + c.repair_expired_before_dequeue
                + c.pending_repair_packets as u64
        );
    }
    #[test]
    fn expired_candidates_create_no_attempt_even_with_old_rtcp_timestamp_and_wrap_overlap() {
        for rtx in [false, true] {
            let (mut n, m, g, clock) = expiry_fixture(rtx);
            owned_original(&mut n, &g, 65535, true);
            owned_original(&mut n, &g, 0, true);
            let before = g.provenance_snapshot();
            clock.clock().advance(std::time::Duration::from_millis(250));
            send_nack(&mut n, vec![pair(65535, 1), pair(0, 0)]);
            assert!(n.poll_write().is_none());
            let c = m.snapshot().counters;
            assert_eq!(c.repair_expired_before_enqueue, 2);
            assert_eq!(c.duplicate_requests, 1);
            assert_eq!(c.repair_enqueued_packets, 0);
            assert_eq!(
                g.provenance_snapshot().attempts_created,
                before.attempts_created
            );
            assert!(!g.snapshot().fenced && !c.failed);
            conserved(&c);
            n.close().unwrap();
            g.release_registration_owners();
            assert_eq!(g.provenance_snapshot().live_proofs, 0);
        }
    }
    #[test]
    fn queued_repair_expires_before_handoff_original_and_control_are_preserved() {
        for rtx in [false, true] {
            let (mut n, m, g, clock) = expiry_fixture(rtx);
            owned_original(&mut n, &g, 7, true);
            send_nack(&mut n, vec![pair(7, 0)]);
            assert_eq!(m.snapshot().counters.pending_repair_packets, 1);
            // A still-owned original expires too, but must reach the unchanged final gate.
            owned_original(&mut n, &g, 8, false);
            n.handle_write(tag(Packet::Rtcp(vec![Box::new(
                PictureLossIndication::default(),
            )])))
            .unwrap();
            clock.clock().advance(std::time::Duration::from_millis(250));
            let original = n.poll_write().unwrap();
            assert!(
                matches!(&original.message.packet,Packet::Rtp(p) if p.header.sequence_number==8)
            );
            assert!(matches!(
                n.poll_write().unwrap().message.packet,
                Packet::Rtcp(_)
            ));
            assert!(n.poll_write().is_none());
            let c = m.snapshot().counters;
            assert_eq!(c.repair_expired_before_dequeue, 1);
            assert_eq!(c.repair_dequeued_packets, 0);
            assert_eq!(c.pending_repair_packets, 0);
            assert_eq!(c.pending_repair_bytes, 0);
            conserved(&c);
            assert_eq!(g.provenance_snapshot().live_attempts, 1);
            drop(original);
            assert_eq!(g.provenance_snapshot().live_attempts, 0);
            assert!(!g.snapshot().fenced && !c.failed);
            n.close().unwrap();
            g.release_registration_owners();
            assert_eq!(g.provenance_snapshot().live_proofs, 0);
        }
    }
    #[test]
    fn missing_or_foreign_expired_cache_proof_is_not_a_successful_drop() {
        for foreign in [false, true] {
            let (mut n, m, g, clock) = expiry_fixture(false);
            owned_original(&mut n, &g, 7, true);
            let (other, _, other_gate, _) = expiry_fixture(false);
            let foreign_context = other_gate
                .capture_attempt(&packet(7, 200))
                .unwrap()
                .unwrap();
            let proof = Attempt::from_context(&foreign_context)
                .unwrap()
                .proof
                .clone();
            n.shared.state.lock().unwrap().cache[7]
                .as_mut()
                .unwrap()
                .proof = foreign.then_some(proof);
            clock.clock().advance(std::time::Duration::from_secs(1));
            n.handle_read(tag(Packet::Rtcp(vec![nack(7, vec![pair(7, 0)])])))
                .unwrap();
            let c = m.snapshot().counters;
            assert!(c.failed && g.snapshot().fenced);
            assert_eq!(c.failure_reason, Some("repair_provenance"));
            assert_eq!(c.repair_expired_before_enqueue, 0);
            assert_eq!(c.repair_expired_before_dequeue, 0);
            drop(other);
        }
    }
}
