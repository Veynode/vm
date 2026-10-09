//! Real pinned TURN client transactions with synthetic server responses; no network.
use super::*;
use rtc::peer_connection::message::LocalPacketContext;
use rtc::stun::{
    attributes::{ATTR_NONCE, ATTR_REALM},
    error_code::CODE_UNAUTHORIZED,
    message::{METHOD_ALLOCATE, METHOD_CREATE_PERMISSION, MessageType},
    textattrs::{Nonce, Realm},
};
use rtc::turn::proto::{lifetime::Lifetime, relayaddr::RelayedAddress};
use std::sync::atomic::{AtomicUsize, Ordering};
struct DropWitness(Arc<AtomicUsize>);
impl Drop for DropWitness {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn local() -> SocketAddr {
    "127.0.0.1:50000".parse().unwrap()
}
fn server() -> SocketAddr {
    "127.0.0.1:3478".parse().unwrap()
}
fn relay() -> SocketAddr {
    "127.0.0.1:50001".parse().unwrap()
}
fn peer() -> SocketAddr {
    "127.0.0.1:50002".parse().unwrap()
}
fn tagged(bytes: &[u8]) -> TaggedBytesMut {
    TaggedBytesMut {
        now: Instant::now(),
        transport: TransportContext {
            local_addr: local(),
            peer_addr: server(),
            ..Default::default()
        },
        message: bytes.into(),
    }
}
fn owned(bytes: &[u8], drops: Option<&Arc<AtomicUsize>>) -> OwnedTransmit {
    OwnedTransmit {
        now: Instant::now(),
        transport: TransportContext {
            local_addr: relay(),
            peer_addr: peer(),
            ..Default::default()
        },
        message: bytes.into(),
        local_context: drops.map(|d| LocalPacketContext(Arc::new(DropWitness(d.clone())))),
    }
}
fn decode(bytes: &[u8]) -> StunMessage {
    let mut m = StunMessage::new();
    m.raw = bytes.to_vec();
    m.decode().unwrap();
    m
}
fn rtp() -> Vec<u8> {
    let mut b = vec![0; 24];
    b[0] = 128;
    b[1] = 108;
    b
}
async fn allocated() -> RTCTurnRelayer {
    let gate = Arc::new(MediaEgress::default());
    gate.enable(108).unwrap();
    allocated_with_gate(gate).await
}
async fn allocated_with_gate(gate: Arc<MediaEgress>) -> RTCTurnRelayer {
    let mut r = RTCTurnRelayer::new(
        vec![local()],
        vec![RTCIceServer {
            urls: vec![format!("turn:{}?transport=udp", server())],
            username: "fixture".into(),
            credential: "fixture".into(),
        }],
        RTCIceTransportPolicy::Relay,
        None,
        crate::runtime::default_runtime().unwrap(),
        rtc::crypto::default_provider().unwrap(),
    )
    .with_gate(gate);
    r.gather().await.unwrap();
    let first = r.poll_write().unwrap();
    let request = decode(&first.message);
    let mut response = StunMessage::new();
    response
        .build(&[
            Box::new(request.transaction_id),
            Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_ERROR_RESPONSE)),
            Box::new(CODE_UNAUTHORIZED),
            Box::new(Realm::new(ATTR_REALM, "fixture".into())),
            Box::new(Nonce::new(ATTR_NONCE, "fixture".into())),
        ])
        .unwrap();
    r.handle_read(tagged(&response.raw)).unwrap();
    let request = decode(&r.poll_write().unwrap().message);
    response
        .build(&[
            Box::new(request.transaction_id),
            Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_SUCCESS_RESPONSE)),
            Box::new(RelayedAddress {
                ip: relay().ip(),
                port: relay().port(),
            }),
            Box::new(Lifetime(Duration::from_secs(600))),
        ])
        .unwrap();
    r.handle_read(tagged(&response.raw)).unwrap();
    while r.poll_event().is_some() {}
    assert!(r.contains_local_addr(relay()));
    r
}
fn permission(r: &mut RTCTurnRelayer, success: bool) {
    permission_response(r, success);
    while r.poll_event().is_some() {}
}
pub(crate) fn permission_response(r: &mut RTCTurnRelayer, success: bool) {
    let request = decode(&r.poll_write().expect("permission request").message);
    assert_eq!(request.typ.method, METHOD_CREATE_PERMISSION);
    let mut response = StunMessage::new();
    response
        .build(&[
            Box::new(request.transaction_id),
            Box::new(MessageType::new(
                METHOD_CREATE_PERMISSION,
                if success {
                    CLASS_SUCCESS_RESPONSE
                } else {
                    CLASS_ERROR_RESPONSE
                },
            )),
        ])
        .unwrap();
    r.handle_read(tagged(&response.raw)).unwrap();
}
#[tokio::test]
async fn actual_turn_permission_encapsulation_moves_one_owner_and_keeps_controls_unannotated() {
    let mut r = allocated().await;
    let drops = Arc::new(AtomicUsize::new(0));
    let bytes = rtp();
    r.handle_write(owned(&bytes, Some(&drops))).unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(
        r.pending_packets.values().map(VecDeque::len).sum::<usize>(),
        1
    );
    permission(&mut r, true);
    let mut media = 0;
    let mut controls = 0;
    while let Some(msg) = r.poll_write() {
        if envelope_payload(&msg.message).unwrap() == Some(bytes.as_slice()) {
            assert!(msg.local_context.is_some());
            media += 1;
        } else {
            assert!(msg.local_context.is_none());
            controls += 1;
        }
    }
    assert_eq!(media, 1);
    assert!(controls >= 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    // DTLS and RTCP are also exact matched inner datagrams, with no media owner.
    for bytes in [
        vec![22, 0xfe, 0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        vec![128, 200, 0, 1, 0, 0, 0, 7, 0, 0, 0, 0],
    ] {
        r.handle_write(owned(&bytes, None)).unwrap();
        let mut matches = 0;
        while let Some(msg) = r.poll_write() {
            if envelope_payload(&msg.message).unwrap() == Some(bytes.as_slice()) {
                matches += 1;
            }
            assert!(msg.local_context.is_none());
        }
        assert_eq!(matches, 1);
    }
    r.close().unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn pending_overflow_permission_failure_and_close_drop_real_owners() {
    let mut r = allocated().await;
    let drops = Arc::new(AtomicUsize::new(0));
    for _ in 0..65 {
        r.handle_write(owned(&rtp(), Some(&drops))).unwrap();
    }
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    permission(&mut r, false);
    assert_eq!(drops.load(Ordering::SeqCst), 65);
    let mut r = allocated().await;
    r.handle_write(owned(&rtp(), Some(&drops))).unwrap();
    r.close().unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 66);
}
#[tokio::test]
async fn prior_same_ciphertext_cannot_claim_next_owned_attempt() {
    let mut r = allocated().await;
    r.handle_write(owned(&rtp(), None)).unwrap();
    permission(&mut r, true);
    while r.poll_write().is_some() {}
    let key = *r.relay_addrs.get(&relay()).unwrap();
    r.clients
        .get_mut(&key)
        .unwrap()
        .client
        .relay(relay())
        .unwrap()
        .send_to(Instant::now(), &rtp(), peer())
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    assert!(r.handle_write(owned(&rtp(), Some(&drops))).is_err());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        r.gate.as_ref().unwrap().snapshot().rejection_reason,
        Some("turn_context_prior_output")
    );
    assert!(r.wouts.is_empty());
    r.close().unwrap();
}

// Test-only actual TURN adapter fixture shared with driver lifecycle regressions.
pub(crate) async fn driver_fixture(gate: Arc<MediaEgress>) -> RTCTurnRelayer {
    let mut r = allocated_with_gate(gate).await;
    r.handle_write(owned(&rtp(), None)).unwrap();
    permission(&mut r, true);
    while r.poll_write().is_some() {}
    r
}
pub(crate) fn confirm_release(r: &mut RTCTurnRelayer) {
    assert!(!r.prepare_deallocation().unwrap());
    let request = decode(&r.poll_write().expect("real Refresh(0)").message);
    assert_eq!(request.typ.method, rtc::stun::message::METHOD_REFRESH);
    assert_eq!(
        request.get(rtc::stun::attributes::ATTR_LIFETIME).unwrap(),
        [0, 0, 0, 0]
    );
    let integrity = rtc::stun::integrity::MessageIntegrity::long_term_integrity_key(
        "fixture".into(),
        "fixture".into(),
        "fixture".into(),
        r.crypto_provider.crypto(),
    )
    .unwrap();
    let mut response = StunMessage::new();
    response
        .build(&[
            Box::new(request.transaction_id),
            Box::new(MessageType::new(
                rtc::stun::message::METHOD_REFRESH,
                CLASS_SUCCESS_RESPONSE,
            )),
            Box::new(Lifetime(Duration::ZERO)),
            Box::new(
                rtc::stun::integrity::MessageIntegrity::new_raw_integrity_with_provider(
                    integrity,
                    r.crypto_provider.crypto(),
                ),
            ),
        ])
        .unwrap();
    r.handle_deallocation_read(tagged(&response.raw)).unwrap();
    assert!(r.prepare_deallocation().unwrap());
}

pub(crate) async fn unpermitted_driver_fixture(gate: Arc<MediaEgress>) -> RTCTurnRelayer {
    allocated_with_gate(gate).await
}
