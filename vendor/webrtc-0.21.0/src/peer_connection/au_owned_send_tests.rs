use super::*;
use crate::runtime::{RecvMeta, default_runtime};
use rtc::shared::marshal::Marshal;
use std::{
    io::IoSliceMut,
    net::SocketAddr,
    sync::atomic::{AtomicU8, AtomicUsize, Ordering},
    time::Duration,
};
#[derive(Debug, Default)]
struct Socket {
    mode: AtomicU8,
    calls: AtomicUsize,
}
impl AsyncUdpSocket for Socket {
    fn poll_send(&self, _: &mut Context<'_>, t: &Transmit<'_>) -> Poll<io::Result<usize>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.mode.load(Ordering::SeqCst) {
            1 => Poll::Pending,
            2 => Poll::Ready(Err(io::Error::from(io::ErrorKind::WouldBlock))),
            3 => Poll::Ready(Ok(t.contents.len() - 1)),
            _ => Poll::Ready(Ok(t.contents.len())),
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
        Ok("127.0.0.1:32123".parse().unwrap())
    }
}
fn gate() -> Arc<MediaEgress> {
    let g = Arc::new(MediaEgress::with_runtime(default_runtime().unwrap()));
    g.enable(108).unwrap();
    let mut state = g.state.lock().unwrap();
    let t = Table::new(Binding {
        primary: (7, 108),
        rtx: None,
    });
    state.proof_budget = Some(t.budget.clone());
    state.deadlines = Some(t);
    state.au_deadlines_enabled = true;
    drop(state);
    g
}
fn packet(ts: u32) -> rtc::rtp::Packet {
    rtc::rtp::Packet {
        header: rtc::rtp::header::Header {
            version: 2,
            ssrc: 7,
            payload_type: 108,
            timestamp: ts,
            sequence_number: 9,
            marker: true,
            ..Default::default()
        },
        payload: vec![1; 32].into(),
    }
}
fn owner(g: &Arc<MediaEgress>, ts: u32, ms: u64) -> (Vec<u8>, LocalPacketContext) {
    AuDeadlineWriter { gate: g.clone() }
        .register(ts, Instant::now() + Duration::from_millis(ms))
        .unwrap();
    let p = packet(ts);
    let c = g.capture_attempt(&p).unwrap().unwrap();
    (p.marshal().unwrap().to_vec(), c)
}
async fn send(
    g: &MediaEgress,
    s: &Socket,
    b: &[u8],
    seg: Option<usize>,
    contexts: &[Option<&LocalPacketContext>],
) -> io::Result<usize> {
    g.send_owned(
        s,
        &Transmit {
            destination: "127.0.0.1:12345".parse().unwrap(),
            ecn: None,
            contents: b,
            segment_size: seg,
            src_ip: None,
        },
        false,
        contexts,
    )
    .await
}
#[tokio::test]
async fn queued_original_survives_registration_retirement_without_nack() {
    let g = gate();
    let s = Socket::default();
    let (bytes, context) = owner(&g, 1, 10);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut writer = AuDeadlineWriter { gate: g.clone() };
    for ts in 2..=129 {
        writer
            .register(ts, Instant::now() + Duration::from_millis(200))
            .unwrap();
    }
    assert!(
        g.state
            .lock()
            .unwrap()
            .deadlines
            .as_ref()
            .unwrap()
            .proof(WireKey::from_packet(&packet(1)).identity)
            .is_err()
    );
    assert!(send(&g, &s, &bytes, None, &[Some(&context)]).await.is_err());
    assert_eq!(s.calls.load(Ordering::SeqCst), 0);
    let state = g.snapshot();
    assert_eq!(state.policy_stop_reason, Some("au_deadline_expired"));
    assert!(!state.failed);
    assert_eq!(state.unknown_au_datagrams, 0);
    drop(context);
    g.release_registration_owners();
    let p = g.provenance_snapshot();
    assert_eq!((p.live_proofs, p.live_attempts, p.attempt_bytes), (0, 0, 0));
    assert_eq!(p.proofs_created, p.proofs_released);
    assert_eq!(p.attempts_created, p.attempts_released);
}
#[tokio::test]
async fn gso_duplicate_attempt_is_rejected_before_any_segment_submission() {
    let g = gate();
    let s = Socket::default();
    let (b, c) = owner(&g, 1, 200);
    let bytes = [b.clone(), b.clone()].concat();
    assert!(
        send(&g, &s, &bytes, Some(b.len()), &[Some(&c), Some(&c)])
            .await
            .is_err()
    );
    assert_eq!(s.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        g.snapshot().rejection_reason,
        Some("au_attempt_duplicate_batch")
    );
}
#[tokio::test]
async fn pending_cancellation_and_definite_error_do_not_mark_submitted_or_leak() {
    let g = gate();
    let s = Socket::default();
    let (b, c) = owner(&g, 1, 200);
    s.mode.store(1, Ordering::SeqCst);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(5),
            send(&g, &s, &b, None, &[Some(&c)])
        )
        .await
        .is_err()
    );
    assert!(!g.snapshot().pending_media_send);
    assert!(
        Attempt::from_context(&c)
            .unwrap()
            .validate(
                g.state.lock().unwrap().proof_budget.as_ref().unwrap(),
                WireKey::from_packet(&packet(1))
            )
            .is_ok()
    );
    s.mode.store(2, Ordering::SeqCst);
    assert!(send(&g, &s, &b, None, &[Some(&c)]).await.is_err());
    assert_eq!(g.snapshot().media_submitted_datagrams, 0);
    s.mode.store(0, Ordering::SeqCst);
    send(&g, &s, &b, None, &[Some(&c)]).await.unwrap();
    assert!(send(&g, &s, &b, None, &[Some(&c)]).await.is_err());
    assert_eq!(g.snapshot().rejection_reason, Some("au_attempt_reused"));
    drop(c);
    g.release_registration_owners();
    assert_eq!(g.provenance_snapshot().live_attempts, 0);
}
#[tokio::test]
async fn short_success_consumes_attempt_and_records_uncertain_failure() {
    let g = gate();
    let s = Socket::default();
    let (b, c) = owner(&g, 1, 200);
    s.mode.store(3, Ordering::SeqCst);
    assert!(send(&g, &s, &b, None, &[Some(&c)]).await.is_err());
    assert!(g.snapshot().failed);
    assert_eq!(g.snapshot().media_submitted_datagrams, 0);
    assert!(
        Attempt::from_context(&c)
            .unwrap()
            .validate(
                g.state.lock().unwrap().proof_budget.as_ref().unwrap(),
                WireKey::from_packet(&packet(1))
            )
            .is_err()
    );
}
#[tokio::test]
async fn missing_foreign_mismatched_and_control_contexts_fail_before_poll() {
    for mode in 0..4 {
        let g = gate();
        let s = Socket::default();
        let (mut b, c) = owner(&g, 1, 200);
        let foreign = gate();
        let (_, fc) = owner(&foreign, 1, 200);
        let context = match mode {
            0 => None,
            1 => Some(&fc),
            _ => Some(&c),
        };
        if mode == 2 {
            b[3] ^= 1;
        } // sequence differs from the held attempt
        if mode == 3 {
            b = vec![128, 200, 0, 1, 0, 0, 0, 7, 0, 0, 0, 0];
        }
        assert!(send(&g, &s, &b, None, &[context]).await.is_err());
        assert_eq!(s.calls.load(Ordering::SeqCst), 0);
        assert!(g.snapshot().failed);
        assert_eq!(g.snapshot().unknown_au_datagrams, 0);
    }
}
#[test]
fn missing_registration_is_counted_only_at_original_admission_and_enable_requires_real_responder() {
    let g = gate();
    assert!(g.capture_attempt(&packet(77)).is_err());
    assert_eq!(g.snapshot().unknown_au_datagrams, 1);
    let g = Arc::new(MediaEgress::with_runtime(default_runtime().unwrap()));
    let p = super::super::au_deadline::test_parameters();
    g.register_sender(0usize.into(), &p).unwrap();
    g.enable(108).unwrap();
    assert!(g.enable_au_deadlines(&p, 7, 108).is_err());
    assert!(!g.snapshot().au_deadlines_enabled);
    assert!(!g.snapshot().failed);
}
