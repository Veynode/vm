use super::*;
use std::sync::atomic::AtomicUsize;
use std::task::{Context, Poll};

#[derive(Debug)]
struct BlockSocket {
    block_media: bool,
    controls: AtomicUsize,
    gso: usize,
    fail: AtomicUsize,
}
impl AsyncUdpSocket for BlockSocket {
    fn max_gso_segments(&self) -> usize {
        self.gso
    }
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok("127.0.0.1:50000".parse().unwrap())
    }
    fn poll_send(&self, _: &mut Context<'_>, t: &Transmit<'_>) -> Poll<std::io::Result<usize>> {
        if self.fail.load(Ordering::SeqCst) != 0 {
            return Poll::Ready(Err(std::io::ErrorKind::TimedOut.into()));
        }
        let first = &t.contents[..t.segment_size.unwrap_or(t.contents.len())];
        let media = super::super::media_egress::classify(first, true).unwrap()
            == super::super::media_egress::Class::Media;
        if media == self.block_media {
            return Poll::Pending;
        }
        self.controls.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(Ok(t.contents.len()))
    }
    fn poll_recv(
        &self,
        _: &mut Context<'_>,
        _: &mut [IoSliceMut<'_>],
        _: &mut [RecvMeta],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Pending
    }
}
async fn pending_fixture(media: bool) -> (PeerConnectionDriver, Arc<BlockSocket>, OwnedTransmit) {
    let (mut driver, _, mut msg) = super::turn_driver_deadline_tests::setup().await;
    msg.transport.local_addr = "127.0.0.1:50000".parse().unwrap();
    let socket = Arc::new(BlockSocket {
        block_media: media,
        controls: AtomicUsize::new(0),
        gso: 1,
        fail: AtomicUsize::new(0),
    });
    driver
        .udp_sockets
        .insert(socket.local_addr().unwrap(), socket.clone());
    (driver, socket, msg)
}
fn control(msg: &OwnedTransmit) -> OwnedTransmit {
    // Minimal DTLS record: empty handshake body, valid fixed record framing.
    let mut bytes = vec![22, 0xfe, 0xfd];
    bytes.resize(13, 0);
    OwnedTransmit {
        now: msg.now,
        transport: msg.transport,
        message: bytes.as_slice().into(),
        local_context: None,
    }
}

#[tokio::test]
async fn media_pending_does_not_block_following_control() {
    let (mut driver, socket, msg) = pending_fixture(true).await;
    let ctl = control(&msg);
    assert!(
        driver.flush_writes(vec![msg, ctl]).now_or_never().is_some(),
        "media Pending retained the driver"
    );
    assert_eq!(socket.controls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn media_pending_returns_driver_for_close_and_input() {
    let (mut driver, _, msg) = pending_fixture(true).await;
    let tick = async {
        driver.flush_writes(vec![msg]).await;
        assert!(
            driver
                .handle_driver_event(PeerConnectionDriverEvent::Close)
                .await
        );
    };
    assert!(
        tick.now_or_never().is_some(),
        "Close processing blocked behind media Pending"
    );
}

#[tokio::test]
async fn control_pending_returns_driver_for_input_and_timer() {
    let (mut driver, _, msg) = pending_fixture(false).await;
    let ctl = control(&msg);
    drop(msg);
    let tick = async {
        driver.flush_writes(vec![ctl]).await;
        driver
            .handle_timeout(driver.inner.runtime.now())
            .await
            .unwrap();
        driver.poll_reads().await.unwrap();
    };
    assert!(
        tick.now_or_never().is_some(),
        "control Pending blocked input and protocol timer"
    );
}

#[tokio::test]
async fn pending_owner_is_released_on_close_without_submission() {
    let (mut driver, _, msg) = pending_fixture(true).await;
    let gate = driver.inner.media_egress.clone();
    driver.flush_writes(vec![msg]).await;
    assert_eq!(driver.egress.counts().0, 1);
    assert_eq!(gate.provenance_snapshot().live_attempts, 1);
    assert!(gate.snapshot().pending_media_send);
    gate.fence();
    assert!(
        driver
            .handle_driver_event(PeerConnectionDriverEvent::Close)
            .await
    );
    assert_eq!(driver.egress.counts(), (0, 0, 0, 0));
    assert_eq!(gate.provenance_snapshot().live_attempts, 0);
    assert!(!gate.snapshot().pending_media_send);
    assert_eq!(gate.snapshot().media_submitted_datagrams, 0);
    super::super::transport::turn_relayer::owned_tests::confirm_release(&mut driver.turn_relayer);
}

#[tokio::test]
async fn pending_media_keeps_original_deadline_across_driver_turns() {
    let (mut driver, _, msg) = pending_fixture(true).await;
    let gate = driver.inner.media_egress.clone();
    driver.flush_writes(vec![msg]).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    driver.service_ready_sends().await;
    assert_eq!(driver.egress.counts().0, 1);
    tokio::time::sleep(Duration::from_millis(165)).await;
    driver.service_ready_sends().await;
    assert_eq!(
        gate.snapshot().policy_stop_reason,
        Some("au_deadline_expired")
    );
    assert_eq!(gate.snapshot().expired_datagrams, 1);
    assert_eq!(gate.snapshot().media_submitted_datagrams, 0);
    assert!(!gate.snapshot().failed);
    assert_eq!(gate.provenance_snapshot().live_attempts, 0);
    assert_eq!(driver.egress.counts(), (0, 0, 0, 0));
}

#[tokio::test]
async fn queue_count_and_byte_capacity_are_explicit_failures() {
    for body in [0usize, 40_000] {
        let (mut driver, _, msg) = pending_fixture(false).await;
        let mut controls = Vec::new();
        let count = if body == 0 { 65 } else { 7 };
        for _ in 0..count {
            let mut c = control(&msg);
            c.message.resize(13 + body, 0);
            c.message[11..13].copy_from_slice(&(body as u16).to_be_bytes());
            controls.push(c);
        }
        drop(msg);
        driver.flush_writes(controls).await;
        let (_, _, packets, bytes) = driver.egress.counts();
        assert_eq!(packets, if body == 0 { 64 } else { 6 });
        assert_eq!(bytes, packets * (13 + body));
        assert!(driver.inner.media_egress.snapshot().failed);
        assert_eq!(
            driver.inner.media_egress.snapshot().rejection_reason,
            Some("egress_queue_capacity")
        );
        driver.egress.clear();
        assert_eq!(driver.egress.counts(), (0, 0, 0, 0));
    }
}

#[tokio::test]
async fn ready_burst_has_finite_work_and_preserves_remaining_owners() {
    let (mut driver, socket, msg) = pending_fixture(true).await;
    let controls = (0..40).map(|_| control(&msg)).collect();
    drop(msg);
    driver.flush_writes(controls).await;
    assert_eq!(socket.controls.load(Ordering::SeqCst), WORK_PACKETS);
    assert_eq!(driver.egress.counts().2, 40 - WORK_PACKETS);
    assert!(driver.outbound_more);
    driver.service_ready_sends().await;
    assert_eq!(socket.controls.load(Ordering::SeqCst), 40);
    assert_eq!(driver.egress.counts(), (0, 0, 0, 0));
}

#[derive(Debug, Default)]
struct ReleaseSocket {
    reads: AtomicUsize,
    pending: AtomicUsize,
    response: std::sync::Mutex<Option<Vec<u8>>>,
    sender: futures::task::AtomicWaker,
}
impl AsyncUdpSocket for ReleaseSocket {
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok("127.0.0.1:50000".parse().unwrap())
    }
    fn poll_send(&self, cx: &mut Context<'_>, tx: &Transmit<'_>) -> Poll<std::io::Result<usize>> {
        self.sender.register(cx.waker());
        if self.reads.load(Ordering::SeqCst) == 0 {
            self.pending.fetch_add(1, Ordering::SeqCst);
            return Poll::Pending;
        }
        use rtc::stun::{
            integrity::MessageIntegrity,
            message::{CLASS_SUCCESS_RESPONSE, METHOD_REFRESH, Message, MessageType},
        };
        use rtc::turn::proto::lifetime::Lifetime;
        let mut request = Message::new();
        request.raw = tx.contents.to_vec();
        request.decode().unwrap();
        assert_eq!(request.typ.method, METHOD_REFRESH);
        assert_eq!(
            request.get(rtc::stun::attributes::ATTR_LIFETIME).unwrap(),
            [0, 0, 0, 0]
        );
        let crypto = rtc::crypto::default_provider().unwrap();
        let key = MessageIntegrity::long_term_integrity_key(
            "fixture".into(),
            "fixture".into(),
            "fixture".into(),
            crypto.crypto(),
        )
        .unwrap();
        let mut response = Message::new();
        response
            .build(&[
                Box::new(request.transaction_id),
                Box::new(MessageType::new(METHOD_REFRESH, CLASS_SUCCESS_RESPONSE)),
                Box::new(Lifetime(Duration::ZERO)),
                Box::new(MessageIntegrity::new_raw_integrity_with_provider(
                    key,
                    crypto.crypto(),
                )),
            ])
            .unwrap();
        *self.response.lock().unwrap() = Some(response.raw);
        Poll::Ready(Ok(tx.contents.len()))
    }
    fn poll_recv(
        &self,
        _: &mut Context<'_>,
        buf: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<std::io::Result<usize>> {
        let data = if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
            // An unrelated, valid STUN message makes the fake socket writable.
            // Cleanup must receive it while its Refresh send is still Pending.
            let mut b = vec![0; 20];
            b[0..2].copy_from_slice(&[1, 1]);
            b[4..8].copy_from_slice(&[0x21, 0x12, 0xa4, 0x42]);
            self.sender.wake();
            Some(b)
        } else {
            self.response.lock().unwrap().take()
        };
        if let Some(data) = data {
            buf[0][..data.len()].copy_from_slice(&data);
            meta[0].addr = "127.0.0.1:3478".parse().unwrap();
            meta[0].len = data.len();
            meta[0].stride = data.len();
            Poll::Ready(Ok(1))
        } else {
            Poll::Pending
        }
    }
}

#[tokio::test]
async fn turn_cleanup_reads_while_refresh_send_pending_and_confirms_real_hmac() {
    let (mut driver, _, msg) = pending_fixture(true).await;
    drop(msg);
    let socket = Arc::new(ReleaseSocket::default());
    driver
        .udp_sockets
        .insert(socket.local_addr().unwrap(), socket.clone());
    assert!(driver.shutdown_turn().await);
    assert!(socket.pending.load(Ordering::SeqCst) > 0);
    assert!(socket.reads.load(Ordering::SeqCst) >= 2);
    assert_eq!(driver.egress.counts(), (0, 0, 0, 0));
    assert_eq!(driver.inner.turn_cleanup.load(Ordering::Acquire), 1);
}

fn another_original(
    driver: &PeerConnectionDriver,
    template: &OwnedTransmit,
    sequence: u16,
) -> OwnedTransmit {
    another_with_payload(driver, template, sequence, 3)
}
fn another_with_payload(
    driver: &PeerConnectionDriver,
    template: &OwnedTransmit,
    sequence: u16,
    payload: usize,
) -> OwnedTransmit {
    use rtc::shared::marshal::Marshal;
    let packet = rtp::Packet {
        header: rtp::header::Header {
            version: 2,
            ssrc: 7,
            payload_type: 108,
            timestamp: 1,
            sequence_number: sequence,
            ..Default::default()
        },
        payload: (0..payload)
            .map(|i| (i + 1) as u8)
            .collect::<Vec<_>>()
            .into(),
    };
    let context = driver.inner.media_egress.capture_attempt(&packet).unwrap();
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
    OwnedTransmit {
        now: template.now,
        transport: template.transport,
        message: srtp.encrypt_rtp(&packet.marshal().unwrap()).unwrap(),
        local_context: context,
    }
}

#[tokio::test]
async fn pending_gso_preserves_each_owner_and_allows_mixed_batch_control() {
    let (mut driver, _, first) = pending_fixture(true).await;
    let socket = Arc::new(BlockSocket {
        block_media: true,
        controls: AtomicUsize::new(0),
        gso: 32,
        fail: AtomicUsize::new(0),
    });
    driver
        .udp_sockets
        .insert(socket.local_addr().unwrap(), socket.clone());
    let mut batch = Vec::new();
    for seq in 8..=39 {
        batch.push(another_original(&driver, &first, seq));
    }
    batch.insert(16, control(&first));
    drop(first);
    driver.flush_writes(batch).await;
    let gate = driver.inner.media_egress.clone();
    assert_eq!(socket.controls.load(Ordering::SeqCst), 1);
    assert_eq!(driver.egress.counts().0, 32);
    assert_eq!(driver.egress.counts().1, 32 * 25 * 2); // two real sixteen-datagram GSO batches
    assert_eq!(gate.provenance_snapshot().live_attempts, 32);
    assert!(gate.snapshot().pending_media_send);
    gate.fence();
    driver.service_ready_sends().await;
    assert_eq!(gate.snapshot().media_submitted_datagrams, 0);
    assert_eq!(gate.snapshot().blocked_media_datagrams, 32);
    assert_eq!(gate.provenance_snapshot().live_attempts, 0);
    assert_eq!(driver.egress.counts(), (0, 0, 0, 0));
}

#[tokio::test]
async fn real_turn_socket_failure_releases_other_queued_attempts() {
    let (mut driver, socket, mut msg) = pending_fixture(true).await;
    let gate = driver.inner.media_egress.clone();
    msg.transport.local_addr = "127.0.0.1:50001".parse().unwrap();
    let other = another_original(&driver, &msg, 8);
    driver.flush_writes(vec![msg, other]).await;
    assert!(
        driver
            .turn_relayer
            .contains_local_addr("127.0.0.1:50001".parse().unwrap())
    );
    assert_eq!(driver.egress.counts().0, 2);
    assert_eq!(gate.provenance_snapshot().live_attempts, 2);
    socket.fail.store(1, Ordering::SeqCst);
    driver.service_ready_sends().await;
    assert!(
        !driver
            .turn_relayer
            .contains_local_addr("127.0.0.1:50001".parse().unwrap())
    );
    assert_eq!(driver.egress.counts(), (0, 0, 0, 0));
    assert_eq!(gate.provenance_snapshot().live_attempts, 0);
    assert!(!gate.snapshot().pending_media_send);
    assert_eq!(gate.snapshot().media_submitted_datagrams, 0);
}

#[tokio::test]
async fn media_capacity_rejects_original_admission_and_gso_byte_budget_is_effective() {
    for gso in [1, 32] {
        let (mut driver, _, template) = pending_fixture(true).await;
        let gate = driver.inner.media_egress.clone();
        let socket = Arc::new(BlockSocket {
            block_media: true,
            controls: AtomicUsize::new(0),
            gso,
            fail: AtomicUsize::new(0),
        });
        driver
            .udp_sockets
            .insert(socket.local_addr().unwrap(), socket);
        if gso == 1 {
            for seq in 0..256 {
                let packet = another_original(&driver, &template, seq);
                assert!(driver.handle_write(packet).await.is_ok());
            }
            let excess = another_original(&driver, &template, 256);
            assert!(
                driver.handle_write(excess).await.is_err(),
                "original admission must explicitly fail"
            );
            assert_eq!(driver.egress.counts().0, 256);
            assert_eq!(driver.egress.counts().1, 256 * 25);
        } else {
            // 224 encrypted 1200B packets fit the independent Attempt budget.
            // GSO's original+concat wire ownership reaches this scheduler's
            // byte cap before its 256 packet cap.
            let batch = (0..224)
                .map(|seq| another_with_payload(&driver, &template, seq, 1178))
                .collect();
            driver.flush_writes(batch).await;
            assert!(driver.egress.counts().0 < 224);
            assert!(driver.egress.counts().1 <= super::super::egress_scheduler::MEDIA_BYTES);
        }
        assert!(gate.snapshot().failed);
        assert_eq!(
            gate.snapshot().rejection_reason,
            Some("egress_queue_capacity")
        );
        assert_eq!(gate.snapshot().media_submitted_datagrams, 0);
        drop(template);
        assert!(
            driver
                .handle_driver_event(PeerConnectionDriverEvent::Close)
                .await
        );
        assert_eq!(driver.egress.counts(), (0, 0, 0, 0));
        assert_eq!(gate.provenance_snapshot().live_attempts, 0);
    }
}

#[tokio::test]
async fn enabled_gate_rejects_public_transport_mutation_before_core_change() {
    use super::super::{PeerConnection, PeerConnectionImpl};
    use rtc::peer_connection::configuration::RTCOfferOptions;
    for au in [false, true] {
        let (mut driver, _, mut msg) = pending_fixture(true).await;
        if !au {
            let (inner, _) = super::super::tests::new_test_peer_connection().await;
            inner.media_egress.enable(108).unwrap();
            driver.inner = inner;
            msg.local_context = None;
        }
        driver.flush_writes(vec![msg]).await;
        assert_eq!(driver.egress.counts().0, 1);
        let peer = PeerConnectionImpl {
            inner: driver.inner.clone(),
            driver_handle: crate::runtime::Mutex::new(None),
            dedicated_reactor: false,
        };
        let config = peer.get_configuration().await;
        let before = format!("{config:?}");
        assert!(peer.restart_ice().await.is_err());
        assert!(
            peer.create_offer(Some(RTCOfferOptions {
                ice_restart: true,
            }))
            .await
            .is_err()
        );
        assert!(peer.set_configuration(config).await.is_err());
        assert_eq!(format!("{:?}", peer.get_configuration().await), before);
        assert!(
            !driver
                .inner
                .ice_restart_rebind_pending
                .load(Ordering::Acquire)
        );
        assert_eq!(driver.egress.counts().0, 1);
        assert!(!driver.inner.media_egress.snapshot().failed);
        driver.egress.clear();
    }
}

#[tokio::test]
async fn already_queued_rebind_fences_before_touching_sockets_and_drops_pending_owner() {
    let (mut driver, socket, msg) = pending_fixture(true).await;
    driver.flush_writes(vec![msg]).await;
    let address = socket.local_addr().unwrap();
    let held = driver.udp_sockets[&address].clone();
    assert!(driver.bind_transports().await.is_err()); // must not perform any OS bind
    assert!(Arc::ptr_eq(&held, &driver.udp_sockets[&address]));
    assert_eq!(driver.egress.counts(), (0, 0, 0, 0));
    let gate = &driver.inner.media_egress;
    assert_eq!(
        gate.snapshot().rejection_reason,
        Some("egress_transport_rebind")
    );
    assert_eq!(gate.snapshot().media_submitted_datagrams, 0);
    assert_eq!(gate.provenance_snapshot().live_attempts, 0);
    assert!(!gate.snapshot().pending_media_send);
}
