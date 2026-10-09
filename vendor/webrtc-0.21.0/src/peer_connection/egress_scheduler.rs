//! Single-driver, bounded UDP scheduling. No rate estimate or pacing delay.
use super::media_egress::{Class, MediaEgress};
use crate::runtime::{AsyncUdpSocket, EcnCodepoint, Transmit};
use futures::future::BoxFuture;
use rtc::peer_connection::message::OwnedTransmit;
use rtc::shared::{FourTuple, error::Result};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

pub(super) const MEDIA_PACKETS: usize = 256;
pub(super) const MEDIA_BYTES: usize = 512 * 1024;
pub(super) const CONTROL_PACKETS: usize = 64;
pub(super) const CONTROL_BYTES: usize = 256 * 1024;
pub(super) const WORK_PACKETS: usize = 32;
pub(super) const WORK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
pub(super) enum Origin {
    Direct,
    Turn(FourTuple),
    Stun(FourTuple),
    Mdns,
}
pub(super) struct Completion {
    pub origin: Origin,
    pub result: Result<usize>,
    pub bytes: usize,
    pub packets: usize,
}

/// Owns every byte/context for one datagram or homogeneous GSO batch. The
/// future is created once and never recreated on Pending: its gate timer and
/// timing state survive every driver selection. Only one front per class is
/// ever polled, so the gate's single pending-media slot remains valid.
pub(super) struct PreparedSend {
    future: BoxFuture<'static, Result<usize>>,
    origin: Origin,
    media: bool,
    packets: usize,
    bytes: usize,
}
impl PreparedSend {
    pub fn new(
        gate: Arc<MediaEgress>,
        socket: Arc<dyn AsyncUdpSocket>,
        mut packets: Vec<OwnedTransmit>,
        segment: usize,
        turn: bool,
        origin: Origin,
    ) -> Result<Self> {
        let first = packets
            .first()
            .ok_or_else(|| gate.reject_reason("egress_empty_batch"))?;
        let mdns = matches!(origin, Origin::Mdns);
        let class = if mdns {
            Class::Control
        } else {
            gate.class(&first.message, turn)?
        };
        let transport = first.transport;
        let wire_bytes = packets.iter().try_fold(0usize, |total, p| {
            if p.transport != transport || (!mdns && gate.class(&p.message, turn)? != class) {
                return Err(gate.reject_reason("egress_batch_identity"));
            }
            total
                .checked_add(p.message.len())
                .ok_or_else(|| gate.reject_reason("egress_queue_capacity"))
        })?;
        // Account for both original wire buffers and a GSO concat, not only
        // the syscall payload. Each source is compacted so slices cannot pin
        // an arbitrarily larger backing allocation in this scheduler.
        let bytes = wire_bytes
            .checked_mul(if packets.len() > 1 { 2 } else { 1 })
            .ok_or_else(|| gate.reject_reason("egress_queue_capacity"))?;
        let count = packets.len();
        if count > WORK_PACKETS || bytes > WORK_BYTES {
            return Err(gate.reject_reason("egress_batch_capacity").into());
        }
        for p in &mut packets {
            p.message = p.message.as_ref().into();
        }
        let future = Box::pin(async move {
            let mut joined = Vec::new();
            let data = if packets.len() == 1 {
                &packets[0].message[..]
            } else {
                joined.reserve_exact(wire_bytes);
                for p in &packets {
                    joined.extend_from_slice(&p.message);
                }
                &joined
            };
            let tx = Transmit {
                destination: transport.peer_addr,
                ecn: transport
                    .ecn
                    .map(|e| e as u8)
                    .and_then(EcnCodepoint::from_bits),
                contents: data,
                segment_size: (segment != 0).then_some(segment),
                src_ip: None,
            };
            if mdns {
                if packets.len() != 1 || packets[0].local_context.is_some() {
                    return Err(gate.reject_reason("au_context_on_control").into());
                }
                gate.send_mdns(&*socket, &tx, &transport)
                    .await
                    .map_err(Into::into)
            } else {
                let contexts: Vec<_> = packets.iter().map(|p| p.local_context.as_ref()).collect();
                gate.send_owned(&*socket, &tx, turn, &contexts)
                    .await
                    .map_err(Into::into)
            }
        });
        Ok(Self {
            future,
            origin,
            media: class == Class::Media,
            packets: count,
            bytes,
        })
    }
}

#[derive(Default)]
struct Lane {
    queue: VecDeque<PreparedSend>,
    packets: usize,
    bytes: usize,
}
#[derive(Default)]
struct State {
    media: Lane,
    control: Lane,
    media_first: bool,
}
#[derive(Clone, Default)]
pub(super) struct Scheduler(Arc<Mutex<State>>);
impl Scheduler {
    pub fn enqueue(&self, send: PreparedSend, gate: &MediaEgress) -> Result<()> {
        let mut s = self.0.lock().unwrap();
        let (lane, packets, bytes) = if send.media {
            (&mut s.media, MEDIA_PACKETS, MEDIA_BYTES)
        } else {
            (&mut s.control, CONTROL_PACKETS, CONTROL_BYTES)
        };
        if lane.packets + send.packets > packets || lane.bytes + send.bytes > bytes {
            return Err(gate.reject_reason("egress_queue_capacity").into());
        }
        lane.packets += send.packets;
        lane.bytes += send.bytes;
        lane.queue.push_back(send);
        Ok(())
    }
    pub fn poll_completion(&self, cx: &mut Context<'_>) -> Poll<Completion> {
        self.poll_budgeted(cx, WORK_PACKETS, WORK_BYTES)
            .map(|done| done.expect("one prepared send fits full work budget"))
    }
    /// Ready(None) asks the caller to yield and replenish its per-pass budget.
    /// Pending means socket/timer readiness is registered, not a spin request.
    pub fn poll_budgeted(
        &self,
        cx: &mut Context<'_>,
        packets: usize,
        bytes: usize,
    ) -> Poll<Option<Completion>> {
        let mut s = self.0.lock().unwrap();
        let order = if s.media_first {
            [true, false]
        } else {
            [false, true]
        };
        let mut budget_blocked = false;
        for media in order {
            let lane = if media { &mut s.media } else { &mut s.control };
            let Some(front) = lane.queue.front_mut() else {
                continue;
            };
            if front.packets > packets || front.bytes > bytes {
                budget_blocked = true;
                continue;
            }
            if let Poll::Ready(result) = front.future.as_mut().poll(cx) {
                let send = lane.queue.pop_front().unwrap();
                lane.packets -= send.packets;
                lane.bytes -= send.bytes;
                s.media_first = !media;
                return Poll::Ready(Some(Completion {
                    origin: send.origin,
                    result,
                    bytes: send.bytes,
                    packets: send.packets,
                }));
            }
        }
        if budget_blocked {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
    pub fn clear(&self) {
        let mut s = self.0.lock().unwrap();
        *s = State::default();
    }
    pub fn retire_turn(&self, tuple: FourTuple) {
        let mut s = self.0.lock().unwrap();
        let State { media, control, .. } = &mut *s;
        for lane in [media, control] {
            lane.queue
                .retain(|send| !matches!(send.origin,Origin::Turn(t) if t==tuple));
            lane.packets = lane.queue.iter().map(|p| p.packets).sum();
            lane.bytes = lane.queue.iter().map(|p| p.bytes).sum();
        }
    }
    #[cfg(test)]
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let s = self.0.lock().unwrap();
        (
            s.media.packets,
            s.media.bytes,
            s.control.packets,
            s.control.bytes,
        )
    }
}
