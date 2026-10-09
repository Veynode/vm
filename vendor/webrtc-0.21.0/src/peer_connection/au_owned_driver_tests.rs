//! Driver-owned encrypted original held across AU-table retirement, without NACK.
use super::*;
use rtc::shared::marshal::Marshal;
use std::sync::atomic::AtomicUsize;
#[derive(Debug, Default)]
struct Socket(AtomicUsize);
impl AsyncUdpSocket for Socket {
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok("127.0.0.1:23451".parse().unwrap())
    }
    fn poll_send(
        &self,
        _: &mut std::task::Context<'_>,
        t: &Transmit<'_>,
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::task::Poll::Ready(Ok(t.contents.len()))
    }
    fn poll_recv(
        &self,
        _: &mut std::task::Context<'_>,
        _: &mut [IoSliceMut<'_>],
        _: &mut [RecvMeta],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Pending
    }
}
#[tokio::test]
async fn encrypted_original_waiting_in_driver_keeps_retired_au_owner() {
    let (inner, _rx) = super::super::tests::new_test_peer_connection().await;
    let gate = inner.media_egress.clone();
    let p = super::super::au_deadline::test_parameters();
    gate.register_sender(0usize.into(), &p).unwrap();
    gate.register_owned_responder((7, 108), None).unwrap();
    gate.enable(108).unwrap();
    let mut writer = gate.enable_au_deadlines(&p, 7, 108).unwrap();
    writer
        .register(1, Instant::now() + Duration::from_millis(10))
        .unwrap();
    let packet = rtp::Packet {
        header: rtp::header::Header {
            version: 2,
            ssrc: 7,
            payload_type: 108,
            timestamp: 1,
            sequence_number: 7,
            ..Default::default()
        },
        payload: vec![1, 2, 3].into(),
    };
    let context = gate.capture_attempt(&packet).unwrap();
    let crypto = rtc::crypto::default_provider().unwrap();
    let mut srtp = rtc::srtp::context::Context::new(
        &[0x42; 16],
        &[0x24; 14],
        rtc::srtp::protection_profile::ProtectionProfile::Aes128CmHmacSha1_80,
        None,
        None,
        crypto.crypto(),
    )
    .unwrap();
    let encrypted = srtp.encrypt_rtp(&packet.marshal().unwrap()).unwrap();
    let socket = Arc::new(Socket::default());
    let local = socket.local_addr().unwrap();
    let queued = vec![OwnedTransmit {
        now: Instant::now(),
        transport: TransportContext {
            local_addr: local,
            peer_addr: "127.0.0.1:23452".parse().unwrap(),
            ..Default::default()
        },
        message: encrypted,
        local_context: context,
    }];
    let mut driver = PeerConnectionDriver::new(
        inner,
        Vec::<SocketAddr>::new(),
        vec![],
        MulticastDnsMode::Disabled,
        vec![],
        RTCIceTransportPolicy::All,
        crypto,
        false,
        None,
    );
    driver.udp_sockets.insert(local, socket.clone());
    tokio::time::sleep(Duration::from_millis(20)).await;
    for ts in 2..=129 {
        writer
            .register(ts, Instant::now() + Duration::from_millis(200))
            .unwrap();
    }
    driver.flush_writes(queued).await;
    assert_eq!(socket.0.load(Ordering::SeqCst), 0);
    let s = gate.snapshot();
    assert!(!s.failed);
    assert_eq!(s.unknown_au_datagrams, 0);
    assert_eq!(s.expired_datagrams, 1);
    assert_eq!(s.policy_stop_reason, Some("au_deadline_expired"));
    assert_eq!(gate.provenance_snapshot().live_attempts, 0);
    gate.release_registration_owners();
    assert_eq!(gate.provenance_snapshot().live_proofs, 0);
}
