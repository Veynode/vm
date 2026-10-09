//! Product single-video-stream NACK responder with bounded cache and repair work.
//!
//! Replaces `Slot::NackResponder`; does not pace, estimate bandwidth, or bound
//! queues after `poll_write`. RTP timestamps are preserved for the independent
//! final-socket AU deadline gate. Counters describe owned wire bytes, not RSS.
//!
//! RTX construction follows rtc-interceptor 0.21.0 `src/nack/responder.rs`
//! (webrtc-rs/rtc contributors, MIT OR Apache-2.0). No upstream dependency changes.
use crate::peer_connection::au_provenance::{Attempt, Proof, WireKey};
use crate::peer_connection::media_egress::MediaEgress;
use bytes::Bytes;
use futures::{future::poll_fn, task::AtomicWaker};
use rtc::peer_connection::message::LocalPacketContext;
use rtc::{
    interceptor::{Attribute, AttributedPacket, Interceptor, Packet, StreamInfo, TaggedPacket},
    rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack,
    rtp::packet::Packet as RtpPacket,
    sansio::Protocol,
    shared::{
        TransportContext,
        error::Error,
        marshal::{Marshal, MarshalSize, Unmarshal},
    },
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    task::Poll,
    time::Instant,
};

const CACHE_PACKETS: usize = 1024;
const CACHE_BYTES: usize = 1024 * 1024;
const PACKET_BYTES: usize = 2048;
const REPAIR_PACKETS: usize = 128;
const REPAIR_BYTES: usize = 128 * 1024;
const PASS_PACKETS: usize = 32;
const PASS_BYTES: usize = 256 * 1024;
const PAIRS: usize = 256;
const MESSAGES: usize = 256;

/// Fixed product budget values. Byte limits charge wire length, excluding fixed
/// table/queue structs and allocator overhead (bounded separately by counts).
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    /// Cache slots.
    pub cache_packets: usize,
    /// Owned serialized cache bytes.
    pub cache_bytes: usize,
    /// Largest cacheable serialized RTP packet.
    pub cached_packet_bytes: usize,
    /// Pending retransmission packet limit.
    pub repair_packets: usize,
    /// Pending retransmission wire-byte limit (RTX OSN included).
    pub repair_bytes: usize,
    /// Ordinary forwarding slots, independent of repairs.
    pub forward_packets: usize,
    /// Ordinary forwarding wire bytes.
    pub forward_bytes: usize,
    /// Inbound passthrough slots.
    pub read_packets: usize,
    /// Inbound passthrough wire bytes.
    pub read_bytes: usize,
    /// Maximum expanded NACK pairs per compound input.
    pub nack_pairs_per_compound: usize,
    /// Maximum RTCP objects inspected per compound input.
    pub rtcp_messages_inspected: usize,
}
const CAPS: Caps = Caps {
    cache_packets: CACHE_PACKETS,
    cache_bytes: CACHE_BYTES,
    cached_packet_bytes: PACKET_BYTES,
    repair_packets: REPAIR_PACKETS,
    repair_bytes: REPAIR_BYTES,
    forward_packets: PASS_PACKETS,
    forward_bytes: PASS_BYTES,
    read_packets: PASS_PACKETS,
    read_bytes: PASS_BYTES,
    nack_pairs_per_compound: PAIRS,
    rtcp_messages_inspected: MESSAGES,
};

/// Fixed, non-sensitive counters. A dequeue is not UDP submission or delivery.
#[derive(Clone, Debug, Default)]
pub struct Counters {
    /// Whether one stream is currently bound.
    pub bound: bool,
    /// Actual interceptor close callback has run.
    pub closed: bool,
    /// Sticky original/binding/queue failure, independent of swallowed chain errors.
    pub failed: bool,
    /// Fixed reason, never underlying packet/error text.
    pub failure_reason: Option<&'static str>,
    /// Current cache packet count.
    pub cache_packets: usize,
    /// Current owned cache bytes.
    pub cache_bytes: usize,
    /// Cache packet peak.
    pub cache_packets_high_water: usize,
    /// Cache byte peak.
    pub cache_bytes_high_water: usize,
    /// Current pending repairs.
    pub pending_repair_packets: usize,
    /// Current pending repair wire bytes.
    pub pending_repair_bytes: usize,
    /// Repair packet peak.
    pub pending_repair_packets_high_water: usize,
    /// Repair byte peak.
    pub pending_repair_bytes_high_water: usize,
    /// Current ordinary forward packets.
    pub pending_forward_packets: usize,
    /// Current ordinary forward wire bytes.
    pub pending_forward_bytes: usize,
    /// Ordinary forward packet peak.
    pub pending_forward_packets_high_water: usize,
    /// Ordinary forward byte peak.
    pub pending_forward_bytes_high_water: usize,
    /// Current inbound passthrough packets.
    pub pending_read_packets: usize,
    /// Current inbound passthrough wire bytes.
    pub pending_read_bytes: usize,
    /// Inbound packet peak.
    pub pending_read_packets_high_water: usize,
    /// Inbound byte peak.
    pub pending_read_bytes_high_water: usize,
    /// Cache admissions.
    pub cached_packets: u64,
    /// Cache entries removed by sequence/byte pressure.
    pub cache_evictions: u64,
    /// Originals forwarded without caching because too large.
    pub cache_skipped_oversize: u64,
    /// NACK pairs expanded, at most the fixed limit per input.
    pub nack_pairs_processed: u64,
    /// Same-compound or still-pending duplicate requests.
    pub duplicate_requests: u64,
    /// NACK messages for an unbound SSRC.
    pub unknown_stream_nacks: u64,
    /// Requested sequences absent from cache.
    pub cache_misses: u64,
    /// Repair queue admissions.
    pub repair_enqueued_packets: u64,
    /// Repair wire bytes admitted (including RTX OSN).
    pub repair_enqueued_bytes: u64,
    /// Candidate repairs rejected by byte/count budget.
    pub repair_suppressed_packets: u64,
    /// Deduplicated NACK candidates with trusted expired proof; no attempt/enqueue.
    pub repair_expired_before_enqueue: u64,
    /// Enqueued repairs actually dropped here on expiry, before core handoff.
    pub repair_expired_before_dequeue: u64,
    /// Compound inputs reaching the inspection/expansion budget.
    pub nack_budget_exhaustions: u64,
    /// Repairs handed to downstream chain (not sent).
    pub repair_dequeued_packets: u64,
    /// Pending repairs discarded on unbind/close.
    pub repair_discarded_on_unbind: u64,
    /// Actual unbind callbacks.
    pub unbinds: u64,
    /// Actual close callbacks.
    pub closes: u64,
}
/// Complete snapshot with fixed budgets.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Current/cumulative values.
    pub counters: Counters,
    /// Configured bounds.
    pub caps: Caps,
}
#[derive(Clone, PartialEq, Eq)]
struct Binding {
    ssrc: u32,
    pt: u8,
    rtx: Option<(u32, u8)>,
    rid_ids: Vec<u8>,
    nack: bool,
}
struct Entry {
    sequence: u16,
    generation: u64,
    wire: Box<[u8]>,
    proof: Option<Arc<Proof>>,
}
struct Queued {
    packet: TaggedPacket,
    bytes: usize,
    repair: Option<u64>,
}
struct State {
    counters: Counters,
    gate: Option<Arc<MediaEgress>>,
    binding: Option<Binding>,
    ever_bound: bool,
    cache: Vec<Option<Entry>>,
    highest: Option<u16>,
    generation: u64,
    rtx_sequence: u16,
    writes: VecDeque<Queued>,
    reads: VecDeque<(TaggedPacket, usize)>,
}
struct Shared {
    state: Mutex<State>,
    wake: AtomicWaker,
}
/// Read-only telemetry/failure observer plus a one-time attachment to this Peer gate.
#[derive(Clone)]
pub struct Monitor(Arc<Shared>);
impl Monitor {
    /// Attach before any stream is bound. Failure synchronously fences this gate.
    pub fn attach(&self, gate: Arc<MediaEgress>) -> Result<(), Error> {
        let mut state = self.0.state.lock().unwrap();
        if state.gate.is_some() || state.ever_bound {
            state.fail("gate_attachment", &self.0.wake);
            return Err(Error::ErrBufferTooSmall);
        }
        if state.counters.failed {
            gate.fence();
        }
        state.gate = Some(gate);
        Ok(())
    }
    /// Fixed counters, with no packet or peer identifiers.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            counters: self.0.state.lock().unwrap().counters.clone(),
            caps: CAPS,
        }
    }
    /// Wait for sticky failure; cancellation does not reset it. Single waiter.
    pub async fn failed(&self) {
        poll_fn(|cx| {
            self.0.wake.register(cx.waker());
            if self.0.state.lock().unwrap().counters.failed {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }
}
/// One-stream bounded replacement for the default responder.
pub struct BoundedNack {
    shared: Arc<Shared>,
}
impl BoundedNack {
    /// Create a responder and monitor. Attach the actual Peer gate before add_track.
    pub fn new() -> (Self, Monitor) {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                counters: Counters::default(),
                gate: None,
                binding: None,
                ever_bound: false,
                cache: (0..CACHE_PACKETS).map(|_| None).collect(),
                highest: None,
                generation: 0,
                rtx_sequence: 0,
                writes: VecDeque::new(),
                reads: VecDeque::new(),
            }),
            wake: AtomicWaker::new(),
        });
        (
            Self {
                shared: shared.clone(),
            },
            Monitor(shared),
        )
    }
}
impl State {
    fn fail(&mut self, reason: &'static str, wake: &AtomicWaker) {
        self.counters.failed = true;
        self.counters.failure_reason.get_or_insert(reason);
        if let Some(gate) = &self.gate {
            gate.fence();
        }
        wake.wake();
    }
    fn remove(&mut self, index: usize) {
        if let Some(entry) = self.cache[index].take() {
            self.counters.cache_packets -= 1;
            self.counters.cache_bytes -= entry.wire.len();
            self.counters.cache_evictions = self.counters.cache_evictions.saturating_add(1);
        }
    }
    fn cache(
        &mut self,
        packet: &RtpPacket,
        bytes: usize,
        proof: Option<Arc<Proof>>,
    ) -> Result<(), ()> {
        let sequence = packet.header.sequence_number;
        if let Some(highest) = self.highest {
            let forward = sequence.wrapping_sub(highest);
            if forward == 0 {
                return Ok(());
            }
            if forward < 32768 {
                // At most 1024 slots, even for a huge forward jump.
                for i in 0..CACHE_PACKETS {
                    if self.cache[i].as_ref().is_some_and(|e| {
                        sequence.wrapping_sub(e.sequence) as usize >= CACHE_PACKETS
                    }) {
                        self.remove(i);
                    }
                }
                self.highest = Some(sequence);
            } else if highest.wrapping_sub(sequence) as usize >= CACHE_PACKETS {
                return Ok(());
            }
        } else {
            self.highest = Some(sequence);
        }
        let index = sequence as usize % CACHE_PACKETS;
        if self.cache[index]
            .as_ref()
            .is_some_and(|e| e.sequence == sequence)
        {
            return Ok(());
        }
        self.remove(index);
        if bytes > PACKET_BYTES {
            self.counters.cache_skipped_oversize =
                self.counters.cache_skipped_oversize.saturating_add(1);
            return Ok(());
        }
        while self.counters.cache_bytes + bytes > CACHE_BYTES {
            let oldest = self
                .cache
                .iter()
                .enumerate()
                .filter_map(|(i, e)| e.as_ref().map(|e| (i, e.generation)))
                .min_by_key(|(_, g)| *g)
                .ok_or(())?
                .0;
            self.remove(oldest);
        }
        let generation = self.generation.checked_add(1).ok_or(())?;
        // Exact owned serialization severs all Bytes slices from larger backing.
        let mut wire = vec![0; bytes].into_boxed_slice();
        if packet.marshal_to(&mut wire).map_err(|_| ())? != bytes {
            return Err(());
        }
        self.generation = generation;
        self.cache[index] = Some(Entry {
            proof,
            sequence,
            generation,
            wire,
        });
        self.counters.cache_packets += 1;
        self.counters.cache_bytes += bytes;
        self.counters.cached_packets = self.counters.cached_packets.saturating_add(1);
        self.counters.cache_packets_high_water = self
            .counters
            .cache_packets_high_water
            .max(self.counters.cache_packets);
        self.counters.cache_bytes_high_water = self
            .counters
            .cache_bytes_high_water
            .max(self.counters.cache_bytes);
        Ok(())
    }
    fn request(&mut self, sequence: u16, now: Instant) -> Result<(), &'static str> {
        let Some(entry) = self.cache[sequence as usize % CACHE_PACKETS]
            .as_ref()
            .filter(|e| e.sequence == sequence)
        else {
            self.counters.cache_misses = self.counters.cache_misses.saturating_add(1);
            return Ok(());
        };
        if self
            .writes
            .iter()
            .any(|q| q.repair == Some(entry.generation))
        {
            self.counters.duplicate_requests = self.counters.duplicate_requests.saturating_add(1);
            return Ok(());
        }
        if self
            .gate
            .as_ref()
            .map(|g| g.repair_expired(entry.proof.as_deref()))
            .transpose()
            .map_err(|_| "repair_provenance")?
            .unwrap_or(false)
        {
            self.counters.repair_expired_before_enqueue = self
                .counters
                .repair_expired_before_enqueue
                .saturating_add(1);
            return Ok(());
        }
        let binding = self.binding.as_ref().ok_or("repair_binding")?;
        // Charge the original serialized size plus OSN before any decode/clone.
        // RTX preserves extensions/CSRC but omits RTP padding; +2 includes OSN.
        let reserved = entry.wire.len() + usize::from(binding.rtx.is_some()) * 2;
        if self.counters.pending_repair_packets >= REPAIR_PACKETS
            || self.counters.pending_repair_bytes + reserved > REPAIR_BYTES
        {
            self.counters.repair_suppressed_packets =
                self.counters.repair_suppressed_packets.saturating_add(1);
            return Ok(());
        }
        let generation = entry.generation;
        let proof = entry.proof.clone();
        let mut wire = Bytes::copy_from_slice(&entry.wire);
        let original = RtpPacket::unmarshal(&mut wire).map_err(|_| "cache_decode")?;
        let packet = if let Some((ssrc, pt)) = binding.rtx {
            let mut payload = Vec::with_capacity(original.payload.len() + 2);
            payload.extend_from_slice(&sequence.to_be_bytes());
            payload.extend_from_slice(&original.payload);
            let seq = self.rtx_sequence;
            self.rtx_sequence = self.rtx_sequence.wrapping_add(1);
            RtpPacket {
                header: rtc::rtp::header::Header {
                    version: 2,
                    ssrc,
                    payload_type: pt,
                    sequence_number: seq,
                    padding: false,
                    ..original.header
                },
                payload: payload.into(),
            }
        } else {
            original
        };
        let bytes = packet.marshal_size();
        if bytes > reserved {
            return Err("cache_decode");
        }
        let context = proof
            .map(|p| Attempt::new(p, WireKey::from_packet(&packet), bytes).map(|a| a.context()))
            .transpose()
            .map_err(|_| "cache_decode")?;
        let mut attributed =
            AttributedPacket::new(Packet::Rtp(packet)).with(Attribute::Retransmission);
        if let Some(context) = context {
            attributed
                .attributes
                .push(Attribute::Custom(Arc::new(context)));
        }
        self.writes.push_back(Queued {
            packet: TaggedPacket {
                now,
                transport: TransportContext::default(),
                message: attributed,
            },
            bytes,
            repair: Some(generation),
        });
        self.counters.pending_repair_packets += 1;
        self.counters.pending_repair_bytes += bytes;
        self.counters.pending_repair_packets_high_water = self
            .counters
            .pending_repair_packets_high_water
            .max(self.counters.pending_repair_packets);
        self.counters.pending_repair_bytes_high_water = self
            .counters
            .pending_repair_bytes_high_water
            .max(self.counters.pending_repair_bytes);
        self.counters.repair_enqueued_packets =
            self.counters.repair_enqueued_packets.saturating_add(1);
        self.counters.repair_enqueued_bytes = self
            .counters
            .repair_enqueued_bytes
            .saturating_add(bytes as u64);
        Ok(())
    }
    fn clear_binding(&mut self) {
        self.counters.repair_discarded_on_unbind = self
            .counters
            .repair_discarded_on_unbind
            .saturating_add(self.counters.pending_repair_packets as u64);
        if let Some(gate) = &self.gate {
            gate.fence();
        }
        self.writes
            .retain(|q| !matches!(q.packet.message.packet, Packet::Rtp(_)));
        self.counters.pending_forward_packets = self.writes.len();
        self.counters.pending_forward_bytes = self.writes.iter().map(|q| q.bytes).sum();
        for entry in &mut self.cache {
            *entry = None;
        }
        self.counters.cache_packets = 0;
        self.counters.cache_bytes = 0;
        self.counters.pending_repair_packets = 0;
        self.counters.pending_repair_bytes = 0;
        self.binding = None;
        self.counters.bound = false;
        self.highest = None;
    }
}
// Guard scalar and collection lengths before marshal_size arithmetic; this is
// after upstream parsing, not a claim about bounds on that parser's allocation.
pub(crate) fn rtp_size(packet: &RtpPacket) -> Option<usize> {
    let h = &packet.header;
    if h.version != 2
        || h.csrc.len() > 15
        || h.extensions.len() > 255
        || h.extensions_padding > PASS_BYTES
    {
        return None;
    }
    let mut total = 12usize
        .checked_add(h.csrc.len() * 4)?
        .checked_add(h.extensions_padding)?
        .checked_add(packet.payload.len())?;
    for e in &h.extensions {
        total = total.checked_add(e.payload.len())?.checked_add(2)?;
    }
    if total > PASS_BYTES {
        return None;
    }
    let size = packet.marshal_size();
    (size <= PASS_BYTES).then_some(size)
}
fn size(packet: &Packet) -> Option<usize> {
    match packet {
        Packet::Rtp(p) => rtp_size(p),
        Packet::Rtcp(packets) => {
            if packets.len() > MESSAGES {
                return None;
            }
            packets.iter().try_fold(0usize, |n, p| {
                n.checked_add(p.marshal_size()).filter(|n| *n <= PASS_BYTES)
            })
        }
        _ => None,
    }
}
fn compact(mut msg: TaggedPacket, bytes: usize) -> Result<TaggedPacket, ()> {
    if msg.message.attributes.len() > 4
        || msg.message.attributes.iter().any(|a| {
            !matches!(
                a,
                Attribute::Retransmission
                    | Attribute::DeliverToApplication
                    | Attribute::TargetBitrateChanged { .. }
            ) && !matches!(a, Attribute::Custom(v) if v.downcast_ref::<LocalPacketContext>().is_some_and(|c|Attempt::from_context(c).is_some()))
        })
    {
        return Err(());
    }
    // Do not retain oversized Vec capacities or arbitrary Custom backing.
    msg.message.attributes = msg.message.attributes.to_vec();
    msg.message.packet = match &msg.message.packet {
        Packet::Rtp(packet) => {
            let mut raw = vec![0; bytes];
            if packet.marshal_to(&mut raw).map_err(|_| ())? != bytes {
                return Err(());
            };
            Packet::Rtp(RtpPacket::unmarshal(&mut Bytes::from(raw)).map_err(|_| ())?)
        }
        Packet::Rtcp(packets) if packets.is_empty() => Packet::Rtcp(Vec::new()),
        Packet::Rtcp(packets) => {
            let raw = rtc::rtcp::packet::marshal(packets).map_err(|_| ())?;
            if raw.len() != bytes {
                return Err(());
            };
            Packet::Rtcp(rtc::rtcp::packet::unmarshal(&mut raw.freeze()).map_err(|_| ())?)
        }
        _ => return Err(()),
    };
    Ok(msg)
}
fn safe_pt(pt: u8) -> bool {
    pt <= 127 && !(64..=95).contains(&pt)
}
impl Protocol<TaggedPacket, TaggedPacket, ()> for BoundedNack {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;
    fn handle_read(&mut self, msg: TaggedPacket) -> Result<(), Error> {
        let mut s = self.shared.state.lock().unwrap();
        let Some(bytes) = size(&msg.message.packet) else {
            s.fail("read_size", &self.shared.wake);
            return Err(Error::ErrBufferTooSmall);
        };
        if s.counters.pending_read_packets >= PASS_PACKETS
            || s.counters.pending_read_bytes + bytes > PASS_BYTES
        {
            s.fail("read_capacity", &self.shared.wake);
            return Err(Error::ErrBufferTooSmall);
        }
        if !s.counters.failed
            && !s.counters.closed
            && let Packet::Rtcp(packets) = &msg.message.packet
        {
            let mut seen = [0u64; 1024];
            let mut pairs = 0;
            let mut exhausted = packets.len() > MESSAGES;
            for packet in packets.iter().take(MESSAGES) {
                let Some(nack) = packet.as_any().downcast_ref::<TransportLayerNack>() else {
                    continue;
                };
                if !s
                    .binding
                    .as_ref()
                    .is_some_and(|b| b.ssrc == nack.media_ssrc)
                {
                    s.counters.unknown_stream_nacks =
                        s.counters.unknown_stream_nacks.saturating_add(1);
                    continue;
                }
                for pair in &nack.nacks {
                    if pairs == PAIRS {
                        exhausted = true;
                        break;
                    }
                    pairs += 1;
                    s.counters.nack_pairs_processed =
                        s.counters.nack_pairs_processed.saturating_add(1);
                    for bit in 0..=16 {
                        if bit > 0 && pair.lost_packets & (1 << (bit - 1)) == 0 {
                            continue;
                        }
                        let sequence = pair.packet_id.wrapping_add(bit);
                        let (index, mask) = (sequence as usize / 64, 1u64 << (sequence % 64));
                        if seen[index] & mask != 0 {
                            s.counters.duplicate_requests =
                                s.counters.duplicate_requests.saturating_add(1);
                            continue;
                        }
                        seen[index] |= mask;
                        if let Err(reason) = s.request(sequence, msg.now) {
                            s.fail(reason, &self.shared.wake);
                            break;
                        }
                    }
                }
            }
            if exhausted {
                s.counters.nack_budget_exhaustions =
                    s.counters.nack_budget_exhaustions.saturating_add(1);
            }
        }
        let msg = match compact(msg, bytes) {
            Ok(msg) => msg,
            Err(()) => {
                s.fail("read_representation", &self.shared.wake);
                return Err(Error::ErrBufferTooSmall);
            }
        };
        s.counters.pending_read_packets += 1;
        s.counters.pending_read_bytes += bytes;
        s.counters.pending_read_packets_high_water = s
            .counters
            .pending_read_packets_high_water
            .max(s.counters.pending_read_packets);
        s.counters.pending_read_bytes_high_water = s
            .counters
            .pending_read_bytes_high_water
            .max(s.counters.pending_read_bytes);
        s.reads.push_back((msg, bytes));
        Ok(())
    }
    fn poll_read(&mut self) -> Option<TaggedPacket> {
        let mut s = self.shared.state.lock().unwrap();
        let (msg, bytes) = s.reads.pop_front()?;
        s.counters.pending_read_packets -= 1;
        s.counters.pending_read_bytes -= bytes;
        Some(msg)
    }
    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Error> {
        let mut s = self.shared.state.lock().unwrap();
        let Some(bytes) = size(&msg.message.packet) else {
            s.fail("forward_size", &self.shared.wake);
            return Err(Error::ErrBufferTooSmall);
        };
        if s.counters.pending_forward_packets >= PASS_PACKETS
            || s.counters.pending_forward_bytes + bytes > PASS_BYTES
        {
            s.fail("forward_capacity", &self.shared.wake);
            return Err(Error::ErrBufferTooSmall);
        }
        if let Packet::Rtp(packet) = &msg.message.packet {
            if s.counters.failed
                || s.counters.closed
                || !s.binding.as_ref().is_some_and(|b| {
                    b.ssrc == packet.header.ssrc && b.pt == packet.header.payload_type
                })
            {
                s.fail("original_binding", &self.shared.wake);
                return Err(Error::ErrBufferTooSmall);
            }
            if s.binding.as_ref().is_some_and(|b| {
                b.rtx.is_some()
                    && packet
                        .header
                        .extensions
                        .iter()
                        .any(|e| b.rid_ids.contains(&e.id))
            }) {
                s.fail("rtx_rid_unsupported", &self.shared.wake);
                return Err(Error::ErrBufferTooSmall);
            }
            let context = msg.message.custom::<LocalPacketContext>();
            if s.gate
                .as_ref()
                .is_some_and(|g| g.validate_context(packet, context).is_err())
            {
                s.fail("original_provenance", &self.shared.wake);
                return Err(Error::ErrBufferTooSmall);
            }
            let proof = context
                .and_then(Attempt::from_context)
                .map(|a| a.proof.clone());
            if s.binding.as_ref().is_some_and(|b| b.nack) && s.cache(packet, bytes, proof).is_err()
            {
                s.fail("cache_encode", &self.shared.wake);
                return Err(Error::ErrBufferTooSmall);
            }
        }
        let msg = match compact(msg, bytes) {
            Ok(msg) => msg,
            Err(()) => {
                s.fail("forward_representation", &self.shared.wake);
                return Err(Error::ErrBufferTooSmall);
            }
        };
        s.counters.pending_forward_packets += 1;
        s.counters.pending_forward_bytes += bytes;
        s.counters.pending_forward_packets_high_water = s
            .counters
            .pending_forward_packets_high_water
            .max(s.counters.pending_forward_packets);
        s.counters.pending_forward_bytes_high_water = s
            .counters
            .pending_forward_bytes_high_water
            .max(s.counters.pending_forward_bytes);
        s.writes.push_back(Queued {
            packet: msg,
            bytes,
            repair: None,
        });
        Ok(())
    }
    fn poll_write(&mut self) -> Option<TaggedPacket> {
        let mut s = self.shared.state.lock().unwrap();
        // At most the existing bounded queue length: no new allocation or
        // background loop. Original/control packets are never expiry-dropped.
        while let Some(q) = s.writes.pop_front() {
            if q.repair.is_some() {
                let context = q.packet.message.custom::<LocalPacketContext>();
                let proof = context
                    .and_then(Attempt::from_context)
                    .map(|a| a.proof.as_ref());
                let expired = s
                    .gate
                    .as_ref()
                    .map(|g| {
                        match &q.packet.message.packet {
                            Packet::Rtp(packet) => g.validate_context(packet, context)?,
                            _ => return Err(std::io::Error::other("repair packet type")),
                        }
                        g.repair_expired(proof)
                    })
                    .transpose();
                if expired.is_err() {
                    // Keep this reservation in the queue for the existing
                    // failed-session close/unbind cleanup accounting.
                    s.writes.push_front(q);
                    s.fail("repair_provenance", &self.shared.wake);
                    return None;
                }
                s.counters.pending_repair_packets -= 1;
                s.counters.pending_repair_bytes -= q.bytes;
                if expired.unwrap().unwrap_or(false) {
                    s.counters.repair_expired_before_dequeue =
                        s.counters.repair_expired_before_dequeue.saturating_add(1);
                    drop(q); // real Attempt owner release, not a fabricated counter reset
                    continue;
                }
                s.counters.repair_dequeued_packets =
                    s.counters.repair_dequeued_packets.saturating_add(1);
            } else {
                s.counters.pending_forward_packets -= 1;
                s.counters.pending_forward_bytes -= q.bytes;
            }
            return Some(q.packet);
        }
        None
    }
    fn handle_timeout(&mut self, _: Instant) -> Result<(), Error> {
        Ok(())
    }
    fn poll_timeout(&mut self) -> Option<Instant> {
        None
    }
    fn close(&mut self) -> Result<(), Error> {
        let mut s = self.shared.state.lock().unwrap();
        s.clear_binding();
        s.writes.clear();
        s.reads.clear();
        s.counters.pending_forward_packets = 0;
        s.counters.pending_forward_bytes = 0;
        s.counters.pending_read_packets = 0;
        s.counters.pending_read_bytes = 0;
        s.counters.closed = true;
        s.counters.closes = s.counters.closes.saturating_add(1);
        Ok(())
    }
}
impl Interceptor for BoundedNack {
    fn bind_local_stream(&mut self, info: &StreamInfo) {
        let mut s = self.shared.state.lock().unwrap();
        let rtx = match (info.ssrc_rtx, info.payload_type_rtx) {
            (None, None) => None,
            (Some(ssrc), Some(pt))
                if ssrc != 0 && ssrc != info.ssrc && safe_pt(pt) && pt != info.payload_type =>
            {
                Some((ssrc, pt))
            }
            _ => {
                s.fail("binding_rtx", &self.shared.wake);
                return;
            }
        };
        if s.gate.is_none()
            || s.ever_bound
            || s.counters.closed
            || info.rtp_header_extensions.len() > 32
            || info
                .rtp_header_extensions
                .iter()
                .any(|e| e.id == 0 || e.id > 255 || e.uri.len() > 256)
            || info.ssrc == 0
            || !safe_pt(info.payload_type)
            || info.ssrc_fec.is_some()
            || info.payload_type_fec.is_some()
            || !(info.mime_type.eq_ignore_ascii_case("video/H264")
                || info.mime_type.eq_ignore_ascii_case("video/H265"))
        {
            let reason = if s.gate.is_none() {
                "binding_gate_missing"
            } else if s.ever_bound {
                "binding_repeated"
            } else if s.counters.closed {
                "binding_after_close"
            } else if info.ssrc_fec.is_some() || info.payload_type_fec.is_some() {
                "binding_fec"
            } else if info
                .rtp_header_extensions
                .iter()
                .any(|e| e.uri.contains("rtp-stream-id"))
            {
                "binding_rid"
            } else {
                "binding_unsupported"
            };
            s.fail(reason, &self.shared.wake);
            return;
        }
        if s.gate.as_ref().is_none_or(|g| {
            g.register_owned_responder((info.ssrc, info.payload_type), rtx)
                .is_err()
        }) {
            s.fail("binding_provenance", &self.shared.wake);
            return;
        }
        s.ever_bound = true;
        s.binding = Some(Binding {
            ssrc: info.ssrc,
            pt: info.payload_type,
            rtx,
            nack: info
                .rtcp_feedback
                .iter()
                .any(|f| f.typ == "nack" && f.parameter.is_empty()),
            rid_ids: info
                .rtp_header_extensions
                .iter()
                .filter(|e| e.uri.contains("rtp-stream-id"))
                .map(|e| e.id as u8)
                .collect(),
        });
        s.counters.bound = true;
    }
    fn unbind_local_stream(&mut self, info: &StreamInfo) {
        let mut s = self.shared.state.lock().unwrap();
        if s.binding.as_ref().is_some_and(|b| b.ssrc == info.ssrc) {
            s.clear_binding();
            s.counters.unbinds = s.counters.unbinds.saturating_add(1);
        }
    }
    fn bind_remote_stream(&mut self, _: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _: &StreamInfo) {}
}
#[cfg(test)]
mod tests;
