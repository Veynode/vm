//! Pinned TURN transactions + actual driver, synthetic server/socket only.
use super::super::transport::turn_relayer::owned_tests::{confirm_release, driver_fixture};
use super::*;
use rtc::shared::marshal::Marshal;
use std::sync::atomic::AtomicUsize;
#[derive(Debug, Default)]
pub(super) struct Socket {
    sent: AtomicUsize,
    fail: std::sync::atomic::AtomicUsize,
}
impl AsyncUdpSocket for Socket {
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok("127.0.0.1:50000".parse().unwrap())
    }
    fn poll_send(
        &self,
        _: &mut std::task::Context<'_>,
        t: &Transmit<'_>,
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.fail.load(Ordering::SeqCst) == 1 {
            return std::task::Poll::Ready(Err(std::io::ErrorKind::TimedOut.into()));
        }
        if self.fail.load(Ordering::SeqCst) == 2 {
            return std::task::Poll::Ready(Ok(t.contents.len() - 1));
        }
        self.sent.fetch_add(1, Ordering::SeqCst);
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
pub(super) async fn setup() -> (PeerConnectionDriver, Arc<Socket>, OwnedTransmit) {
    let (inner, _rx) = super::super::tests::new_test_peer_connection().await;
    let gate = inner.media_egress.clone();
    let p = super::super::au_deadline::test_parameters();
    gate.register_sender(0usize.into(), &p).unwrap();
    gate.register_owned_responder((7, 108), None).unwrap();
    gate.enable(108).unwrap();
    let mut writer = gate.enable_au_deadlines(&p, 7, 108).unwrap();
    writer
        .register(1, Instant::now() + Duration::from_millis(250))
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
    let bytes = srtp.encrypt_rtp(&packet.marshal().unwrap()).unwrap();
    let mut driver = PeerConnectionDriver::new(
        inner,
        vec![],
        vec![],
        MulticastDnsMode::Disabled,
        vec![],
        RTCIceTransportPolicy::All,
        crypto,
        false,
        None,
    );
    driver.turn_relayer = driver_fixture(gate).await;
    let socket = Arc::new(Socket::default());
    driver
        .udp_sockets
        .insert(socket.local_addr().unwrap(), socket.clone());
    let msg = OwnedTransmit {
        now: Instant::now(),
        transport: TransportContext {
            local_addr: "127.0.0.1:50001".parse().unwrap(),
            peer_addr: "127.0.0.1:50002".parse().unwrap(),
            ..Default::default()
        },
        message: bytes,
        local_context: context,
    };
    (driver, socket, msg)
}
#[tokio::test]
async fn final_core_batch_turn_output_is_submitted_before_driver_waits() {
    let (mut driver, socket, msg) = setup().await;
    // This is poll_writes' final core-output phase: no next timer/read/write wake.
    driver.flush_writes(vec![msg]).await;
    assert!(
        socket.sent.load(Ordering::SeqCst) > 0,
        "TURN output stranded until unrelated wake"
    );
    assert_eq!(
        driver
            .inner
            .media_egress
            .snapshot()
            .media_submitted_datagrams,
        1
    );
    assert!(driver.turn_relayer.poll_write().is_none());
    confirm_release(&mut driver.turn_relayer);
}
#[tokio::test]
async fn known_deadline_rejection_preserves_turn_allocation_for_authenticated_release() {
    let (mut driver, socket, msg) = setup().await;
    driver.turn_relayer.handle_write(msg).unwrap();
    tokio::time::sleep(Duration::from_millis(270)).await;
    driver.poll_writes().await.unwrap();
    assert_eq!(socket.sent.load(Ordering::SeqCst), 0);
    assert_eq!(
        driver.inner.media_egress.snapshot().policy_stop_reason,
        Some("au_deadline_expired")
    );
    assert!(
        driver
            .turn_relayer
            .contains_local_addr("127.0.0.1:50001".parse().unwrap()),
        "local policy failure incorrectly destroys TURN allocation"
    );
    let d = driver.inner.media_egress.snapshot();
    assert_eq!(d.first_expiry_stage, Some("before_socket_poll"));
    assert_eq!(d.first_expiry_turn, Some(true));
    assert!(d.first_expiry_first_poll_remaining_us.unwrap() < 0);
    assert_eq!(d.first_expiry_socket_pending_polls, 0);
    confirm_release(&mut driver.turn_relayer);
}
#[tokio::test]
async fn actual_socket_timeout_still_removes_turn_allocation_and_is_unconfirmed() {
    let (mut driver, socket, msg) = setup().await;
    socket.fail.store(1, Ordering::SeqCst);
    driver.turn_relayer.handle_write(msg).unwrap();
    driver.poll_writes().await.unwrap();
    assert!(
        !driver
            .turn_relayer
            .contains_local_addr("127.0.0.1:50001".parse().unwrap())
    );
    assert!(!driver.turn_relayer.prepare_deallocation().unwrap());
    assert_eq!(
        driver.inner.media_egress.snapshot().policy_stop_reason,
        None
    );
}

#[tokio::test]
async fn permission_response_releases_media_before_tick_wait_without_new_input() {
    use super::super::transport::turn_relayer::owned_tests::{
        permission_response, unpermitted_driver_fixture,
    };
    let (mut driver, socket, msg) = setup().await;
    driver.turn_relayer = unpermitted_driver_fixture(driver.inner.media_egress.clone()).await;
    driver.turn_relayer.handle_write(msg).unwrap();
    permission_response(&mut driver.turn_relayer, true); // response read, events not yet polled
    driver.poll_ready_io().await.unwrap();
    assert_eq!(
        driver
            .inner
            .media_egress
            .snapshot()
            .media_submitted_datagrams,
        1
    );
    assert!(socket.sent.load(Ordering::SeqCst) > 0);
    assert!(driver.turn_relayer.poll_write().is_none());
    confirm_release(&mut driver.turn_relayer);
}
#[tokio::test]
async fn real_control_socket_timeout_after_policy_stop_is_not_masked() {
    let (mut driver, socket, msg) = setup().await;
    driver.turn_relayer.handle_write(msg).unwrap();
    tokio::time::sleep(Duration::from_millis(270)).await;
    driver.poll_writes().await.unwrap();
    assert!(
        driver
            .turn_relayer
            .contains_local_addr("127.0.0.1:50001".parse().unwrap())
    );
    assert!(!driver.turn_relayer.prepare_deallocation().unwrap());
    socket.fail.store(1, Ordering::SeqCst);
    driver.poll_writes().await.unwrap();
    assert!(
        !driver
            .turn_relayer
            .contains_local_addr("127.0.0.1:50001".parse().unwrap())
    );
    assert!(!driver.turn_relayer.prepare_deallocation().unwrap());
}

#[tokio::test]
async fn partial_socket_success_is_uncertain_and_still_retires_turn_client() {
    let (mut driver, socket, msg) = setup().await;
    socket.fail.store(2, Ordering::SeqCst);
    driver.turn_relayer.handle_write(msg).unwrap();
    driver.poll_writes().await.unwrap();
    let s = driver.inner.media_egress.snapshot();
    assert!(s.failed && s.fenced);
    assert_eq!(s.rejection_reason, Some("partial_udp_submission"));
    assert_eq!(s.media_submitted_datagrams, 0);
    assert!(
        !driver
            .turn_relayer
            .contains_local_addr("127.0.0.1:50001".parse().unwrap())
    );
    assert!(!driver.turn_relayer.prepare_deallocation().unwrap());
}
