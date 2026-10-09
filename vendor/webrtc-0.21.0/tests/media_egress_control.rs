//! Real localhost UDP/ICE/DTLS/SRTP/SCTP with a per-Peer media stop latch.
//! Payloads are synthetic transport probes, not valid pictures or codec-decode evidence.
//! No fake socket, external server, TURN, Guest, encoder or global network changes.
use anyhow::{Context, Result, ensure};
use bytes::{Bytes, BytesMut};
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::{
    Registry, register_default_interceptors,
};
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_VP8, MediaEngine};
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use std::{sync::Arc, time::Duration};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_local::{TrackLocal, static_rtp::TrackLocalStaticRTP};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
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
        mime_type: MIME_TYPE_VP8.to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: String::new(),
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
    let request = vec![tag; 64 * 1024];
    a.send(BytesMut::from(request.as_slice())).await?;
    ensure!(message(b).await? == request, "wrong forward DC receipt");
    let response = vec![tag + 1; 64 * 1024];
    b.send(BytesMut::from(response.as_slice())).await?;
    ensure!(message(a).await? == response, "wrong reverse DC receipt");
    Ok(())
}
async fn exercise(periodic_reports: bool) -> Result<()> {
    let before_packets = if periodic_reports { 120 } else { 24 };
    let packet_period = Duration::from_millis(if periodic_reports { 25 } else { 5 });
    let rt = runtime();
    let (ag, mut agr) = channel(1);
    let (ac, mut acr) = channel(1);
    let (ad, _adr) = channel(1);
    let (ap, _apr) = channel(128);
    let (bg, mut bgr) = channel(1);
    let (bc, mut bcr) = channel(1);
    let (bd, mut bdr) = channel(1);
    let (bp, mut bpr) = channel(128);
    let build = |handler| {
        let mut engine = media_engine();
        let registry = if periodic_reports {
            register_default_interceptors(Registry::new(), &mut engine)
                .expect("production default interceptor registry")
        } else {
            Registry::new()
        };
        PeerConnectionBuilder::new()
            .with_interceptor_registry(registry)
            .with_handler(handler)
            .with_runtime(rt.clone())
            .with_media_engine(engine)
            .with_udp_addrs(vec!["127.0.0.1:0".to_owned()])
    };
    let a = build(Arc::new(Events {
        gathered: ag,
        connected: ac,
        data_channel: ad,
        packets: ap,
        runtime: rt.clone(),
    }))
    .build()
    .await?;
    let b_result = build(Arc::new(Events {
        gathered: bg,
        connected: bc,
        data_channel: bd,
        packets: bp,
        runtime: rt.clone(),
    }))
    .build()
    .await;
    let b = match b_result {
        Ok(b) => b,
        Err(error) => {
            a.close().await?;
            return Err(error.into());
        }
    };
    let gate = a.media_egress();
    // Keep cleanup outside the timed body: even a failed assertion or timeout closes both Peers.
    let result = timeout(Duration::from_secs(25), async {
        gate.enable(PT)?;
        let track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
            "egress-stream".into(),
            "egress-track".into(),
            "transport-only".into(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(SSRC),
                    ..Default::default()
                },
                codec: codec(),
                ..Default::default()
            }],
        )));
        a.add_track(track.clone() as Arc<dyn TrackLocal>).await?;
        let dc = a.create_data_channel("egress-control", None).await?;
        a.set_local_description(a.create_offer(None).await?).await?;
        agr.recv().await.context("sender gathering ended")?;
        b.set_remote_description(a.local_description().await.context("missing offer")?)
            .await?;
        b.set_local_description(b.create_answer(None).await?)
            .await?;
        bgr.recv().await.context("receiver gathering ended")?;
        let answer = b.local_description().await.context("missing answer")?;
        ensure!(
            answer.sdp.contains("a=rtpmap:108 VP8/90000"),
            "test PT not negotiated"
        );
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
        let remote = bdr.recv().await.context("remote DC missing")?;
        roundtrip(&*dc, &*remote, 11).await?;
        for seq in 0..before_packets {
            track.write_rtp(packet(seq)).await?;
            sleep(packet_period).await;
        }
        let received = bpr.recv().await.context("no actual SRTP receipt")?;
        ensure!(
            received.header.ssrc == SSRC
                && received.header.payload_type == PT
                && received.header.sequence_number < before_packets
                && received.payload == packet(0).payload,
            "wrong actual RTP receipt"
        );
        let before = gate.fence();
        ensure!(
            before.enabled && before.fenced && !before.failed,
            "gate not healthy"
        );
        ensure!(
            before.media_submitted_datagrams > 0,
            "no media reached real local socket"
        );
        for tag in [31, 51, 71] {
            for seq in before_packets..before_packets + 16 {
                ensure!(
                    track.write_rtp(packet(seq)).await.is_err(),
                    "post-fence RTP accepted"
                );
            }
            roundtrip(&*dc, &*remote, tag).await?;
            let now = gate.snapshot();
            ensure!(
                now.media_submitted_datagrams == before.media_submitted_datagrams,
                "media reached local socket after fence"
            );
            ensure!(
                now.fenced && !now.failed,
                "control traffic failed gate classification"
            );
        }
        // Older already-submitted datagrams can legitimately arrive after fence.
        // Any post-fence probe sequence, however, must never arrive.
        while let Ok(packet) = bpr.try_recv() {
            ensure!(
                packet.header.sequence_number < before_packets,
                "post-fence RTP arrived"
            );
        }
        ensure!(
            gate.snapshot().control_submitted_datagrams > before.control_submitted_datagrams,
            "no post-fence local control socket submissions"
        );
        eprintln!(
            "actual localhost SRTP receipt: ssrc={} pt={} seq={}; fence={before:?}; after_three_bidirectional_sctp_roundtrips={:?}",
            received.header.ssrc,
            received.header.payload_type,
            received.header.sequence_number,
            gate.snapshot(),
        );
        remote.close().await?;
        dc.close().await?;
        Ok::<(), anyhow::Error>(())
    })
    .await;
    let (closed_a, closed_b) = futures::join!(
        timeout(Duration::from_secs(4), a.close()),
        timeout(Duration::from_secs(4), b.close()),
    );
    closed_a.context("sender close deadline")??;
    closed_b.context("receiver close deadline")??;
    ensure!(
        !gate.snapshot().pending_media_send,
        "media send pending after close"
    );
    result.context("real localhost media/control test deadline")??;
    eprintln!("both actual localhost Peers closed successfully; no pending media send");
    Ok(())
}
#[test]
fn actual_rtp_fenced_while_bidirectional_sctp_survives() {
    block_on(exercise(false)).expect("real localhost media/control fence");
}

#[test]
fn actual_periodic_rtcp_and_rtp_fence_preserve_bidirectional_sctp() {
    // The short test cannot reach the production default one-second SR/SDES timer.
    // Use the real default interceptors and keep RTP active for at least 3 seconds.
    block_on(exercise(true)).expect("actual periodic RTCP plus media/control fence");
}
