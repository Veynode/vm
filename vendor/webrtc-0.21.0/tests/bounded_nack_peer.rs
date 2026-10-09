//! Actual localhost SRTP, bounded compound NACK repair, AU expiry and surviving SCTP.
//! Payloads are synthetic transport probes, not valid pictures or codec-decode evidence.
//! No fake socket, external server, TURN, Guest, encoder or global network changes.
use anyhow::{Context, Result, ensure};
use bytes::{Bytes, BytesMut};
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::{
    Registry, register_default_interceptors,
};
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MediaEngine};
use rtc::peer_connection::configuration::setting_engine::SctpMaxMessageSize;
use rtc::rtcp::transport_feedbacks::transport_layer_nack::{NackPair, TransportLayerNack};
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use webrtc::bounded_nack::BoundedNack;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_local::{TrackLocal, static_rtp::TrackLocalStaticRTP};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::SettingEngineBuilder;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};
use webrtc::runtime::{Runtime, Sender, channel};

mod common;
use common::{block_on, runtime, sleep, timeout};

const PT: u8 = 108;
const SSRC: u32 = 0x045E_1234;

struct Events {
    gathered: Sender<()>,
    connected: Sender<()>,
    data_channel: Sender<Arc<dyn DataChannel>>,
    packets: Sender<rtc::rtp::packet::Packet>,
    tracks: Sender<Arc<dyn TrackRemote>>,
    runtime: Arc<dyn Runtime>,
}
#[async_trait::async_trait]
impl PeerConnectionEventHandler for Events {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.try_send(());
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if state == RTCPeerConnectionState::Connected {
            let _ = self.connected.try_send(());
        }
    }
    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        let _ = self.data_channel.try_send(channel);
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.tracks.try_send(track.clone());
        let packets = self.packets.clone();
        self.runtime.spawn(Box::pin(async move {
            while let Some(event) = track.poll().await {
                match event {
                    TrackRemoteEvent::OnRtpPacket(packet) => {
                        let _ = packets.try_send(packet);
                    }
                    TrackRemoteEvent::OnEnded => break,
                    _ => {}
                }
            }
        }));
    }
}
fn codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
            .into(),
        rtcp_feedback: vec![],
    }
}
fn media_engine() -> MediaEngine {
    let mut engine = MediaEngine::default();
    engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: codec(),
                payload_type: PT,
            },
            RtpCodecKind::Video,
        )
        .expect("register test codec");
    engine
}
fn packet(sequence: u16) -> rtc::rtp::packet::Packet {
    rtc::rtp::packet::Packet {
        header: rtc::rtp::header::Header {
            version: 2,
            payload_type: PT,
            sequence_number: sequence,
            timestamp: u32::from(sequence) * 3000,
            ssrc: SSRC,
            ..Default::default()
        },
        payload: Bytes::from(vec![0xA5; 200]),
    }
}
async fn message(channel: &dyn DataChannel) -> Result<Vec<u8>> {
    loop {
        match channel.poll().await.context("DC ended without message")? {
            DataChannelEvent::OnMessage(message) => return Ok(message.data.to_vec()),
            DataChannelEvent::OnClose => anyhow::bail!("DC closed without message"),
            _ => {}
        }
    }
}
async fn roundtrip(a: &dyn DataChannel, b: &dyn DataChannel, tag: u8) -> Result<()> {
    // Exercise fragmented, reliable SCTP in both directions, not only its handshake.
    let request = vec![tag; 16 * 1024];
    a.send(BytesMut::from(request.as_slice())).await?;
    ensure!(message(b).await? == request, "wrong forward DC receipt");
    let response = vec![tag + 1; 16 * 1024];
    b.send(BytesMut::from(response.as_slice())).await?;
    ensure!(message(a).await? == response, "wrong reverse DC receipt");
    Ok(())
}
async fn exercise(retire_original: bool) -> Result<()> {
    let rt = runtime();
    let (ag, mut agr) = channel(1);
    let (ac, mut acr) = channel(1);
    let (ad, _adr) = channel(1);
    let (ap, _apr) = channel(8);
    let (at, _atr) = channel(1);
    let (bg, mut bgr) = channel(1);
    let (bc, mut bcr) = channel(1);
    let (bd, mut bdr) = channel(1);
    let (bp, mut bpr) = channel(8);
    let (bt, mut btr) = channel(1);
    let (responder, monitor) = BoundedNack::new();
    let build = |handler, responder: Option<BoundedNack>| {
        let mut engine = media_engine();
        let registry = register_default_interceptors(Registry::new(), &mut engine)
            .expect("production default NACK responder");
        let registry = match responder {
            Some(responder) => registry.with(rtc::interceptor::Slot::NackResponder, responder),
            None => registry,
        };
        PeerConnectionBuilder::new()
            .with_handler(handler)
            .with_runtime(rt.clone())
            .with_media_engine(engine)
            .with_interceptor_registry(registry)
            .with_setting_engine(
                SettingEngineBuilder::new()
                    .with_sctp_max_message_size(SctpMaxMessageSize::Bounded(16 * 1024))
                    .with_sctp_max_receive_buffer_size(64 * 1024)
                    .build(),
            )
            .with_udp_addrs(vec!["127.0.0.1:0".to_owned()])
    };
    let a = timeout(
        Duration::from_secs(4),
        build(
            Arc::new(Events {
                gathered: ag,
                connected: ac,
                data_channel: ad,
                packets: ap,
                tracks: at,
                runtime: rt.clone(),
            }),
            Some(responder),
        )
        .build(),
    )
    .await
    .context("sender build deadline")??;
    let b_result = timeout(
        Duration::from_secs(4),
        build(
            Arc::new(Events {
                gathered: bg,
                connected: bc,
                data_channel: bd,
                packets: bp,
                tracks: bt,
                runtime: rt.clone(),
            }),
            None,
        )
        .build(),
    )
    .await
    .context("receiver build deadline")
    .and_then(|v| v.map_err(Into::into));
    let b = match b_result {
        Ok(peer) => peer,
        Err(error) => {
            timeout(Duration::from_secs(4), a.close())
                .await
                .context("early close deadline")??;
            return Err(error);
        }
    };
    let gate = a.media_egress();
    let result = timeout(Duration::from_secs(25), async {
        monitor.attach(gate.clone()).context("attach gate before binding the only stream")?;
        let track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
            "deadline-stream".into(), "deadline-track".into(), "transport-only".into(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(SSRC), ..Default::default() },
                codec: codec(), ..Default::default()
            }],
        )));
        let sender = a.add_track(track.clone() as Arc<dyn TrackLocal>).await?;
        let dc = a.create_data_channel("deadline-control", None).await?;
        a.set_local_description(a.create_offer(None).await?).await?;
        agr.recv().await.context("sender gathering ended")?;
        b.set_remote_description(a.local_description().await.context("missing offer")?).await?;
        b.set_local_description(b.create_answer(None).await?).await?;
        bgr.recv().await.context("receiver gathering ended")?;
        let answer = b.local_description().await.context("missing answer")?;
        ensure!(answer.sdp.contains("a=rtpmap:108 H264/90000"), "PT not negotiated");
        ensure!(answer.sdp.contains("a=rtcp-fb:108 nack"), "NACK not negotiated");
        a.set_remote_description(answer).await?;
        acr.recv().await.context("sender not connected")?;
        bcr.recv().await.context("receiver not connected")?;
        loop {
            match dc.poll().await.context("DC ended before open")? {
                DataChannelEvent::OnOpen => break,
                DataChannelEvent::OnClose => anyhow::bail!("DC closed before open"),
                _ => {}
            }
        }
        let remote_dc = bdr.recv().await.context("remote DC missing")?;
        roundtrip(&*dc, &*remote_dc, 11).await?;
        let parameters = sender.get_parameters().await.context("negotiated sender parameters")?;
        eprintln!("negotiated AU test: encodings={} codecs={}", parameters.encodings.len(), parameters.rtp_parameters.codecs.len());
        for encoding in &parameters.encodings {
            eprintln!("encoding primary_ssrc={:?} rtx_ssrc={:?}",
                encoding.rtp_coding_parameters.ssrc,
                encoding.rtp_coding_parameters.rtx.as_ref().map(|r|r.ssrc));
        }
        for codec in &parameters.rtp_parameters.codecs {
            eprintln!("codec pt={} mime={}", codec.payload_type, codec.rtp_codec.mime_type);
        }
        ensure!(!parameters.rtp_parameters.codecs.iter().any(|c|
            c.rtp_codec.mime_type.eq_ignore_ascii_case("video/rtx")),
            "this test must cover ordinary NACK, not claim RTX");
        gate.enable(PT).context("enable selective media gate")?;
        let mut writer = sender.enable_au_deadlines(SSRC, PT).await
            .context("bind actual negotiated H264 AU deadlines under sender lock")?;
        let original = packet(100);
        let deadline = Instant::now() + Duration::from_millis(250);
        writer.register(original.header.timestamp, deadline).context("register original AU deadline")?;
        track.write_rtp(original.clone()).await.context("send sole original RTP")?;
        let received = bpr.recv().await.context("no actual original RTP receipt")?;
        ensure!(received.header.ssrc == SSRC && received.header.payload_type == PT
            && received.header.sequence_number == original.header.sequence_number
            && received.header.timestamp == original.header.timestamp
            && received.payload == original.payload, "original RTP identity changed");
        let remote_track = btr.recv().await.context("remote track missing")?;
        let before = gate.snapshot();
        ensure!(before.au_deadlines_enabled && before.registered_access_units == 1
            && before.media_submitted_datagrams == 1 && before.expired_datagrams == 0
            && !before.fenced && !before.failed, "initial media submission not healthy");
        // One actual compound request: two identical PID entries plus a bitmap
        // overlapping the same cached sequence in a second RTCP block. PID 99
        // was never sent; the bit requests PID 100. Dedup spans both blocks.
        remote_track.write_rtcp(vec![
            Box::new(TransportLayerNack {
                sender_ssrc: 0x0111_3333, media_ssrc: SSRC,
                nacks: vec![NackPair::new(100), NackPair::new(100)],
            }),
            Box::new(TransportLayerNack {
                sender_ssrc: 0x0111_3333, media_ssrc: SSRC,
                nacks: vec![NackPair { packet_id: 99, lost_packets: 1 }],
            }),
        ]).await.context("send actual duplicate compound NACK")?;
        timeout(Duration::from_millis(150), async {
            while gate.snapshot().media_submitted_datagrams < 2 {
                ensure!(!monitor.snapshot().counters.failed && !gate.snapshot().fenced,
                    "bounded responder failed before eligible repair");
                sleep(Duration::from_millis(2)).await;
            }
            Ok::<(), anyhow::Error>(())
        }).await.context("eligible actual NACK repair did not reach socket before AU deadline")??;
        let repaired = gate.snapshot();
        let budget = monitor.snapshot();
        ensure!(repaired.media_submitted_datagrams == 2 && !repaired.failed && !repaired.fenced
            && repaired.registered_access_units == 1, "compound repair socket/AU accounting");
        ensure!(budget.counters.repair_enqueued_packets == 1
            && budget.counters.repair_dequeued_packets == 1
            && budget.counters.duplicate_requests >= 2
            && budget.counters.cache_misses == 1 && !budget.counters.failed,
            "duplicate/overlap compound generated more than one repair");
        // Ordinary (non-RTX) duplicate sequence packets may be discarded by the
        // receiver SRTP replay window. Socket admission + responder counters
        // prove one repair, not a second application delivery or codec decode.
        sleep(deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(25)).await;
        // Register a fresh second AU after the first expired, but do not send it.
        // This must not erase the known deadline of the older cached packet.
        let new_access_units = if retire_original { 128u16 } else { 1u16 };
        for n in 1..=new_access_units {
            writer.register(packet(100+n).header.timestamp, Instant::now() + Duration::from_millis(250))
                .context("register new AUs while original remains in bounded NACK cache")?;
        }
        let retired = gate.snapshot();
        ensure!(retired.registered_access_units == 1 + u64::from(new_access_units)
            && retired.au_records == usize::from(new_access_units).min(127)+1,
            "wrong bounded AU retirement accounting");
        eprintln!("before delayed NACK: retired={retired:?}; nack={:?}", monitor.snapshot());
        // No second write_rtp: only the real remote RTCP request reaches suppression.
        let attempts_before_late = gate.provenance_snapshot().attempts_created;
        remote_track.write_rtcp(vec![Box::new(TransportLayerNack {
            sender_ssrc: 0x0111_3333,
            media_ssrc: SSRC,
            nacks: vec![NackPair::new(original.header.sequence_number)],
        })]).await?;
        timeout(Duration::from_secs(3), async {
            while monitor.snapshot().counters.nack_pairs_processed <= budget.counters.nack_pairs_processed {
                sleep(Duration::from_millis(5)).await;
            }
        }).await.context("actual delayed NACK did not reach responder")?;
        let expired = gate.snapshot();
        ensure!(gate.provenance_snapshot().attempts_created == attempts_before_late,
            "expired NACK allocated a new Attempt");
        ensure!(monitor.snapshot().counters.repair_expired_before_enqueue == 1
            && monitor.snapshot().counters.repair_expired_before_dequeue == 0,
            "stale NACK was not classified at owned admission");
        ensure!(!expired.fenced && !expired.failed && expired.rejected_datagrams == 0
            && expired.unknown_au_datagrams == 0 && expired.expired_datagrams == 0
            && expired.policy_stop_reason.is_none(),
            "stale repair still terminated healthy session");
        ensure!(expired.media_submitted_datagrams == repaired.media_submitted_datagrams,
            "expired actual retransmission reached socket");
        let mut fresh=packet(101);
        // Use the newest already-registered AU and its unchanged deadline.
        // The retirement case intentionally has all 128 admission slots live.
        fresh.header.timestamp=packet(100+new_access_units).header.timestamp;
        track.write_rtp(fresh.clone()).await.context("fresh original after stale NACK")?;
        loop {
            let received=bpr.recv().await.context("no fresh actual RTP receipt")?;
            if received.header.sequence_number==fresh.header.sequence_number {
                ensure!(received.header.timestamp==fresh.header.timestamp && received.payload==fresh.payload,
                    "fresh RTP identity changed");
                break;
            }
            ensure!(received.header.sequence_number==original.header.sequence_number,
                "unexpected sequence before fresh receipt");
        }
        for tag in [31, 51, 71] {
            roundtrip(&*dc, &*remote_dc, tag).await?;
            ensure!(!gate.snapshot().fenced && gate.snapshot().media_submitted_datagrams == 3,
                "stale repair disturbed fresh media or control");
        }
        ensure!(gate.snapshot().control_submitted_datagrams > expired.control_submitted_datagrams,
            "no actual control socket submissions after stale repair suppression");
        let final_budget = monitor.snapshot();
        ensure!(final_budget.counters.repair_enqueued_packets == 1
            && final_budget.counters.repair_dequeued_packets == 1
            && !final_budget.counters.failed,
            "stale retry created a new transmission attempt");
        ensure!(final_budget.counters.cache_bytes_high_water <= final_budget.caps.cache_bytes
            && final_budget.counters.cache_packets_high_water <= final_budget.caps.cache_packets
            && final_budget.counters.pending_repair_bytes_high_water <= final_budget.caps.repair_bytes
            && final_budget.counters.pending_repair_packets_high_water <= final_budget.caps.repair_packets,
            "actual bounded responder exceeded its budgets");
        eprintln!("actual bounded compound repair: first={budget:?}; final={final_budget:?}; repaired={repaired:?}");
        eprintln!("actual ordinary-NACK AU expiry: original={before:?}; expired={expired:?}; after_control={:?}", gate.snapshot());
        remote_dc.close().await?;
        dc.close().await?;
        Ok::<(), anyhow::Error>(())
    }).await;
    if !matches!(&result, Ok(Ok(()))) {
        eprintln!("AU test failed before cleanup: gate={:?}", gate.snapshot());
    }
    // Unconditional bounded cleanup, including an operation timeout or assertion error.
    let (a_closed, b_closed) = futures::join!(
        timeout(Duration::from_secs(4), a.close()),
        timeout(Duration::from_secs(4), b.close()),
    );
    a_closed.context("sender close deadline")??;
    b_closed.context("receiver close deadline")??;
    ensure!(
        !gate.snapshot().pending_media_send,
        "media send pending after Peer close"
    );
    let ownership = gate.provenance_snapshot();
    eprintln!("actual post-close AU ownership: {ownership:?}");
    ensure!(
        ownership.live_proofs == 0
            && ownership.live_attempts == 0
            && ownership.attempt_bytes == 0
            && ownership.proofs_created == ownership.proofs_released
            && ownership.attempts_created == ownership.attempts_released,
        "closed Peer retained AU owners"
    );
    let closed = monitor.snapshot();
    ensure!(
        closed.counters.closed
            && !closed.counters.bound
            && closed.counters.cache_packets == 0
            && closed.counters.cache_bytes == 0
            && closed.counters.pending_repair_packets == 0
            && closed.counters.pending_repair_bytes == 0
            && closed.counters.pending_forward_packets == 0
            && closed.counters.pending_forward_bytes == 0
            && closed.counters.pending_read_packets == 0
            && closed.counters.pending_read_bytes == 0,
        "bounded responder retained cache/queues after both Peer closes"
    );
    eprintln!("actual bounded responder closed: {closed:?}");
    result.context("actual bounded NACK Peer test deadline")??;
    eprintln!("both actual localhost Peers closed after AU expiry; no codec decode or RTX claim");
    Ok(())
}

#[test]
fn actual_compound_nack_deduplicates_and_expired_retry_preserves_control() {
    block_on(exercise(false)).expect("actual localhost AU deadline and control path");
}

// Finite feedback delay: wait for the immutable original deadline, fill all
// 128 AU slots, then issue one actual remote NACK for a still-cached original.
// This isolates the lifetime fault without a Guest or a global loss emulator.
#[test]
fn actual_cached_nack_after_au_retirement_keeps_original_expiry_identity() {
    block_on(exercise(true)).expect("cached original must retain its original expiry identity");
}
