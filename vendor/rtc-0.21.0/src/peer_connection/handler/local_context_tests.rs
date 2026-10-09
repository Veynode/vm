//! Actual queue/drop behavior, retaining the core object after close or a drop.
use super::*;
use crate::peer_connection::message::internal::{
    RTCMessageInternal, RTPMessage, TaggedRTCMessageInternal,
};
use crate::peer_connection::{RTCPeerConnectionBuilder, message::LocalPacketContext};
use shared::TransportContext;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Witness(Arc<AtomicUsize>);
impl Drop for Witness {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn context(counter: &Arc<AtomicUsize>) -> LocalPacketContext {
    LocalPacketContext(Arc::new(Witness(counter.clone())))
}
fn tagged(counter: &Arc<AtomicUsize>, raw: bool) -> TaggedRTCMessageInternal {
    TaggedRTCMessageInternal {
        now: Instant::now(),
        transport: TransportContext::default(),
        local_context: Some(context(counter)),
        message: RTCMessageInternal::Rtp(if raw {
            RTPMessage::Raw((&[0x80, 108, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7][..]).into())
        } else {
            RTPMessage::Packet(::interceptor::Packet::Rtp(rtp::Packet::default()))
        }),
    }
}
#[test]
fn close_drops_real_endpoint_and_completed_outputs_while_core_stays_alive() {
    let mut pc = RTCPeerConnectionBuilder::new()
        .build(Instant::now())
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    pc.get_endpoint_handler()
        .handle_write(tagged(&drops, false))
        .unwrap();
    pc.pipeline_context
        .srtp_handler_context
        .write_outs
        .push_back(tagged(&drops, true));
    pc.pipeline_context.write_outs.push_back(OwnedTransmit {
        now: Instant::now(),
        transport: TransportContext::default(),
        message: (&[1][..]).into(),
        local_context: Some(context(&drops)),
    });
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    pc.close().unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    assert!(
        pc.pipeline_context
            .endpoint_handler_context
            .write_outs
            .is_empty()
    );
    assert!(
        pc.pipeline_context
            .srtp_handler_context
            .write_outs
            .is_empty()
    );
    pc.close().unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 3);
}
#[test]
fn srtp_missing_key_and_ice_no_selected_pair_drop_owned_contexts() {
    let mut pc = RTCPeerConnectionBuilder::new()
        .build(Instant::now())
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    assert!(
        pc.get_srtp_handler()
            .handle_write(tagged(&drops, false))
            .is_err()
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    pc.get_ice_handler()
        .handle_write(tagged(&drops, true))
        .unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert!(pc.get_ice_handler().poll_write().is_none());
    pc.close().unwrap();
}
