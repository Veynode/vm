//! Actual localhost SRTP plus core-generated NACK retransmission after its AU deadline.
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
async fn exercise() -> Result<()> {
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
    let (responder, monitor) = webrtc::bounded_nack::BoundedNack::new();
    let build = |handler, responder: Option<webrtc::bounded_nack::BoundedNack>| {
        let mut engine = media_engine();
        let registry = register_default_interceptors(Registry::new(), &mut engine)
            .expect("production default interceptors");
        let registry = if let Some(responder) = responder {
            registry.with(rtc::interceptor::Slot::NackResponder, responder)
        } else {
            registry
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
    monitor.attach(gate.clone())?;
    let result = timeout(Duration::from_secs(25), async {
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
        ensure!(sender.set_parameters(parameters.clone(), None).await.is_err(),
            "public no-op parameter setter must refuse after AU binding");
        let mut changed_parameters = parameters.clone();
        changed_parameters.encodings[0].rtp_coding_parameters.ssrc = Some(SSRC + 1);
        ensure!(sender.set_parameters(changed_parameters, None).await.is_err(),
            "public setter changed the bound primary SSRC");
        ensure!(sender.replace_track(track.clone() as Arc<dyn TrackLocal>).await.is_err(),
            "public track replacement must refuse even the same bound track");
        let still_bound = sender.get_parameters().await.context("read unchanged bound sender")?;
        ensure!(still_bound.encodings.len() == 1
            && still_bound.encodings[0].rtp_coding_parameters.ssrc == Some(SSRC)
            && still_bound.encodings[0].rtp_coding_parameters.rtx.as_ref().map(|r| r.ssrc)
                == parameters.encodings[0].rtp_coding_parameters.rtx.as_ref().map(|r| r.ssrc),
            "refused setter mutated the actual sender binding");
        ensure!(!gate.snapshot().failed && !gate.snapshot().fenced,
            "refused public mutations damaged the valid bound session");
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
        sleep(deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(25)).await;
        // Register a fresh second AU after the first expired, but do not send it.
        // This must not erase the known deadline of the older cached packet.
        writer.register(packet(101).header.timestamp, Instant::now() + Duration::from_millis(250))
            .context("register fresh AU while retaining previous expired identity")?;
        ensure!(gate.snapshot().registered_access_units == 2 && gate.snapshot().au_records == 2,
            "fresh registration discarded the known expired original");
        // No second write_rtp: only the real remote RTCP request can cause this retransmission.
        remote_track.write_rtcp(vec![Box::new(TransportLayerNack {
            sender_ssrc: 0x0111_3333,
            media_ssrc: SSRC,
            nacks: vec![NackPair::new(original.header.sequence_number)],
        })]).await?;
        timeout(Duration::from_secs(3), async {
            while monitor.snapshot().counters.repair_expired_before_enqueue == 0 {
                ensure!(!gate.snapshot().failed, "unexpected classifier failure before expiry");
                sleep(Duration::from_millis(5)).await;
            }
            Ok::<(), anyhow::Error>(())
        }).await.context("actual NACK responder did not reach AU deadline gate")??;
        let expired = gate.snapshot();
        ensure!(!expired.fenced && !expired.failed && expired.rejected_datagrams == 0
            && expired.unknown_au_datagrams == 0 && expired.expired_datagrams == 0
            && expired.policy_stop_reason.is_none(),
            "stale repair must be suppressed before any attempt/core handoff");
        ensure!(expired.media_submitted_datagrams == before.media_submitted_datagrams,
            "expired actual retransmission reached socket");
        gate.fence(); // Separate explicit stop remains irreversible after stale repair.
        ensure!(track.write_rtp(packet(101)).await.is_err(), "new RTP escaped explicit stop fence");
        for tag in [31, 51, 71] {
            roundtrip(&*dc, &*remote_dc, tag).await?;
            ensure!(gate.snapshot().media_submitted_datagrams == before.media_submitted_datagrams,
                "post-expiry media socket submission");
        }
        ensure!(gate.snapshot().control_submitted_datagrams > expired.control_submitted_datagrams,
            "no actual control socket submissions after expiry");
        ensure!(bpr.try_recv().is_err(), "unexpected RTP after the sole original");
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
    result.context("actual AU deadline Peer test deadline")??;
    eprintln!("both actual localhost Peers closed after AU expiry; no codec decode or RTX claim");
    Ok(())
}

#[test]
fn real_nack_retransmission_keeps_original_au_deadline_and_control_survives() {
    block_on(exercise()).expect("actual localhost AU deadline and control path");
}
