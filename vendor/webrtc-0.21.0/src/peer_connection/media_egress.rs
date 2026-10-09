//! Veynode opt-in, irreversible UDP media stop fence. Not a delivery receipt.
use super::au_deadline::{Binding, Identity, Table};
pub use super::au_provenance::Snapshot as ProvenanceSnapshot;
use super::au_provenance::{Attempt, Budget, WireKey};
/// Logical proof / send-attempt / plaintext-attempt-byte caps, not total RTC memory.
pub fn provenance_caps() -> (usize, usize, usize) {
    (
        super::au_provenance::PROOFS,
        super::au_provenance::ATTEMPTS,
        super::au_provenance::ATTEMPT_BYTES,
    )
}
use rtc::peer_connection::message::LocalPacketContext;

use crate::runtime::{AsyncUdpSocket, Runtime, Transmit};
use futures::task::AtomicWaker;
use rtc::rtp_transceiver::{RTCRtpSenderId, rtp_sender::RTCRtpSendParameters};
use std::{
    io,
    sync::Mutex,
    task::{Context, Poll},
};
use std::{sync::Arc, time::Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Media,
    Control,
}

// Per-send typed result, never inferred from global sticky state or ErrorKind.
#[derive(Debug)]
struct LocalPolicyStop;
impl std::fmt::Display for LocalPolicyStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("local AU submission deadline expired")
    }
}
impl std::error::Error for LocalPolicyStop {}
pub(crate) fn is_local_policy_stop(error: &rtc::shared::error::Error) -> bool {
    matches!(error, rtc::shared::error::Error::Io(e) if e.0.get_ref().is_some_and(|e| e.is::<LocalPolicyStop>()))
}
struct SendTiming {
    started: Instant,
    turn: bool,
    first_remaining_us: Option<i64>,
    pending_polls: u64,
}
impl SendTiming {
    fn new(now: Instant, turn: bool) -> Self {
        Self {
            started: now,
            turn,
            first_remaining_us: None,
            pending_polls: 0,
        }
    }
    fn first_poll(&mut self, now: Instant, deadline: Option<Instant>) {
        if self.first_remaining_us.is_none() {
            self.first_remaining_us = deadline.map(|d| {
                let magnitude = if d >= now {
                    d.duration_since(now)
                } else {
                    now.duration_since(d)
                };
                let us = magnitude.as_micros().min(i64::MAX as u128) as i64;
                if d >= now { us } else { -us }
            });
        }
    }
    fn record(&self, s: &mut Snapshot, now: Instant, stage: &'static str) {
        if s.first_expiry_stage.is_none() {
            s.first_expiry_stage = Some(stage);
            s.first_expiry_turn = Some(self.turn);
            s.first_expiry_first_poll_remaining_us = self.first_remaining_us;
            s.first_expiry_socket_pending_polls = self.pending_polls;
            s.first_expiry_send_elapsed_us = now
                .saturating_duration_since(self.started)
                .as_micros()
                .min(u64::MAX as u128) as u64;
        }
    }
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "unsupported media egress framing",
    )
}
fn u16_at(bytes: &[u8], n: usize) -> usize {
    u16::from_be_bytes([bytes[n], bytes[n + 1]]) as usize
}

/// Only called for owned RTC output. TURN envelopes are legal only on a known
/// TURN server transport; a nested envelope is never accepted as peer control.
pub(crate) fn classify(bytes: &[u8], turn_server: bool) -> Result<Class, &'static str> {
    inspect(bytes, turn_server).map(|id| {
        if id.is_some() {
            Class::Media
        } else {
            Class::Control
        }
    })
}
fn inspect(bytes: &[u8], turn_server: bool) -> Result<Option<WireKey>, &'static str> {
    if bytes.is_empty() || bytes.len() > 65535 {
        return Err("datagram_size");
    }
    if (64..=79).contains(&bytes[0]) {
        if !turn_server || bytes.len() < 4 {
            return Err("turn_channel_data");
        }
        let n = u16_at(bytes, 2);
        // UDP ChannelData can omit padding, or carry up to its 4-byte alignment.
        if n == 0 || (bytes.len() != 4 + n && bytes.len() != 4 + n.div_ceil(4) * 4) {
            return Err("turn_channel_data");
        }
        return inspect(&bytes[4..4 + n], false);
    }
    if bytes[0] <= 3 {
        if bytes.len() < 20
            || bytes[4..8] != [0x21, 0x12, 0xa4, 0x42]
            || !u16_at(bytes, 2).is_multiple_of(4)
            || u16_at(bytes, 2) + 20 != bytes.len()
        {
            return Err("stun_framing_or_method");
        }
        let typ = u16_at(bytes, 0);
        let mut pos = 20;
        let mut data = None;
        let mut peer = false;
        while pos < bytes.len() {
            if pos + 4 > bytes.len() {
                return Err("stun_framing_or_method");
            }
            let attr = u16_at(bytes, pos);
            let n = u16_at(bytes, pos + 2);
            let start = pos + 4;
            if start + n > bytes.len() {
                return Err("stun_framing_or_method");
            }
            if attr == 0x13 {
                if data.is_some() {
                    return Err("stun_framing_or_method");
                }
                data = Some(&bytes[start..start + n]);
            }
            if attr == 0x12 {
                if peer || !matches!(n, 8 | 20) {
                    return Err("stun_framing_or_method");
                }
                peer = true;
            }
            pos = start + n.div_ceil(4) * 4;
        }
        if pos != bytes.len() {
            return Err("stun_framing_or_method");
        }
        if typ == 0x16 {
            if !turn_server || !peer {
                return Err("stun_framing_or_method");
            }
            return inspect(data.ok_or("stun_framing_or_method")?, false);
        }
        if data.is_some()
            || !matches!(
                typ,
                0x0001
                    | 0x0101
                    | 0x0111
                    | 0x0011
                    | 0x0003
                    | 0x0103
                    | 0x0113
                    | 0x0004
                    | 0x0104
                    | 0x0114
                    | 0x0008
                    | 0x0108
                    | 0x0118
                    | 0x0009
                    | 0x0109
                    | 0x0119
            )
        {
            return Err("stun_framing_or_method");
        }
        return Ok(None);
    }
    if (20..=25).contains(&bytes[0]) {
        let mut pos = 0;
        while pos < bytes.len() {
            if pos + 13 > bytes.len()
                || !(20..=25).contains(&bytes[pos])
                || bytes[pos + 1] != 0xfe
                || !matches!(bytes[pos + 2], 0xff | 0xfd)
            {
                return Err("dtls_record");
            }
            pos = pos
                .checked_add(13 + u16_at(bytes, pos + 11))
                .ok_or("dtls_record")?;
        }
        return if pos == bytes.len() {
            Ok(None)
        } else {
            Err("dtls_record")
        };
    }
    if bytes[0] >> 6 == 2 {
        if bytes.len() < 8 {
            return Err("srtcp_header");
        }
        if (192..=223).contains(&bytes[1]) {
            let first_len = (u16_at(bytes, 2) + 1) * 4;
            return if first_len >= 8 && first_len <= bytes.len() {
                Ok(None)
            } else {
                Err("srtcp_header")
            };
        }
        let mut header = 12 + usize::from(bytes[0] & 15) * 4;
        if bytes.len() < header {
            return Err("srtp_header");
        }
        if bytes[0] & 0x10 != 0 {
            if bytes.len() < header + 4 {
                return Err("srtp_header");
            }
            header += 4 + u16_at(bytes, header + 2) * 4;
        }
        return if header < bytes.len() {
            Ok(Some(WireKey {
                identity: Identity {
                    ssrc: u32::from_be_bytes(bytes[8..12].try_into().unwrap()),
                    payload: bytes[1] & 127,
                    timestamp: u32::from_be_bytes(bytes[4..8].try_into().unwrap()),
                },
                sequence: u16::from_be_bytes([bytes[2], bytes[3]]),
                marker: bytes[1] & 128 != 0,
            }))
        } else {
            Err("srtp_header")
        };
    }
    Err("unknown_datagram")
}

#[derive(Clone, Copy, Debug, Default)]
/// Cumulative local socket-poll evidence, never remote delivery evidence.
pub struct Snapshot {
    /// Opt-in classification enabled.
    pub enabled: bool,
    /// Irreversible media stop latch.
    pub fenced: bool,
    /// Unsupported framing or transport encountered.
    pub failed: bool,
    /// Media datagrams accepted by a completed local socket poll.
    pub media_submitted_datagrams: u64,
    /// Control datagrams accepted by a completed local socket poll.
    pub control_submitted_datagrams: u64,
    /// Actual mDNS socket poll errors; not submissions or failed stop proofs.
    pub mdns_send_errors: u64,
    /// Media datagrams deliberately discarded after fencing.
    pub blocked_media_datagrams: u64,
    /// Unsupported datagrams or transports rejected.
    pub rejected_datagrams: u64,
    /// First fixed rejection category, never payload or address text.
    pub rejection_reason: Option<&'static str>,
    /// A media socket future is pending locally.
    pub pending_media_send: bool,
    /// Local AU deadline mode; never an end-to-end latency guarantee.
    pub au_deadlines_enabled: bool,
    /// Successfully registered immutable AU deadlines.
    pub registered_access_units: u64,
    /// Current metadata entries; bounded to 128.
    pub au_records: usize,
    /// Maximum simultaneous metadata entries.
    pub au_records_high_water: usize,
    /// Segments suppressed because their batch's earliest AU expired; includes
    /// still-eligible siblings in the same GSO batch.
    pub expired_datagrams: u64,
    /// Full socket polls which returned after the checked deadline. Submission
    /// is still counted; actual kernel acceptance time is unknown.
    pub completion_overrun_datagrams: u64,
    /// Unregistered/retired AU identities rejected before socket submission.
    pub unknown_au_datagrams: u64,
    /// Known local policy failure, separate from uncertain stop-proof failures.
    pub policy_stop_reason: Option<&'static str>,
    /// First expiry's fixed boundary category; no packet/address/source text.
    pub first_expiry_stage: Option<&'static str>,
    /// Whether the expired final datagram used a validated TURN envelope route.
    pub first_expiry_turn: Option<bool>,
    /// Original deadline minus first final poll instant, signed microseconds.
    pub first_expiry_first_poll_remaining_us: Option<i64>,
    /// Actual socket Pending returns before first expiry (excludes fault holds).
    pub first_expiry_socket_pending_polls: u64,
    /// Time spent in this final send future through first expiry, microseconds.
    pub first_expiry_send_elapsed_us: u64,
    /// Explicit local diagnostic; disabled by default.
    pub fault_au_hold_armed: bool,
    /// One final media poll was deliberately made Pending.
    pub fault_au_hold_triggered: bool,
    /// Trigger instant relative to first successfully completed full media send.
    pub fault_au_hold_after_first_submit_ms: Option<u64>,
}
#[derive(Default)]
/// Per-Peer UDP RTP stop latch; disabled until explicitly configured.
pub struct MediaEgress {
    state: Mutex<State>,
    pending: AtomicWaker,
    runtime: Option<Arc<dyn Runtime>>,
}
#[derive(Default)]
struct State {
    snapshot: Snapshot,
    media_started: bool,
    unsafe_binding: bool,
    bound_sender: Option<RTCRtpSenderId>,
    multiple_senders: bool,
    deadlines: Option<Table>,
    proof_budget: Option<Arc<Budget>>,
    owned_responder: Option<Binding>,
    first_media_submission: Option<Instant>,
}
impl std::ops::Deref for State {
    type Target = Snapshot;
    fn deref(&self) -> &Snapshot {
        &self.snapshot
    }
}
impl std::ops::DerefMut for State {
    fn deref_mut(&mut self) -> &mut Snapshot {
        &mut self.snapshot
    }
}
/// Exclusive metadata producer for one configured encoding. Not clonable and
/// never given to the NACK/RTX path; every AU gets exactly its original deadline.
pub struct AuDeadlineWriter {
    gate: Arc<MediaEgress>,
}
impl AuDeadlineWriter {
    /// Install once before any original RTP enqueue; duplicates cannot refresh age.
    pub fn register(&mut self, timestamp: u32, deadline: Instant) -> io::Result<()> {
        let result = {
            let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.fenced {
                return Err(invalid());
            }
            let table = state.deadlines.as_mut().ok_or_else(invalid)?;
            let result = table.register(timestamp, deadline, self.gate.now());
            let (registered, len, high) = (table.registered, table.len(), table.high_water);
            state.registered_access_units = registered;
            state.au_records = len;
            state.au_records_high_water = high;
            result
        };
        result.map_err(|reason| {
            if reason == "au_deadline_expired" {
                let mut s = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
                s.fenced = true;
                s.policy_stop_reason.get_or_insert(reason);
                drop(s);
                self.gate.pending.wake();
                io::Error::new(io::ErrorKind::TimedOut, "local AU expired before enqueue")
            } else {
                self.gate.reject_reason(reason)
            }
        })
    }
}
impl MediaEgress {
    pub(crate) fn with_runtime(runtime: Arc<dyn Runtime>) -> Self {
        Self {
            runtime: Some(runtime),
            ..Self::default()
        }
    }
    fn now(&self) -> Instant {
        self.runtime.as_ref().map_or_else(Instant::now, |r| r.now())
    }
    /// Configures a single, actually negotiated sender before any RTP. No FEC or
    /// simulcast. A later sender/binding change fails closed, not a new epoch.
    pub(crate) fn enable_au_deadlines(
        self: &Arc<Self>,
        parameters: &RTCRtpSendParameters,
        primary_ssrc: u32,
        primary_pt: u8,
    ) -> io::Result<AuDeadlineWriter> {
        let binding = Binding::from_parameters(parameters, primary_ssrc, primary_pt)
            .map_err(|r| self.reject_reason(r))?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.enabled
            || state.fenced
            || state.media_started
            || state.deadlines.is_some()
            || state.multiple_senders
            || state.bound_sender.is_none()
            || self.runtime.is_none()
        {
            return Err(invalid());
        }
        if state.owned_responder != Some(binding) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "owned AU responder binding required",
            ));
        }
        let table = Table::new(binding);
        state.proof_budget = Some(table.budget.clone());
        state.deadlines = Some(table);
        state.au_deadlines_enabled = true;
        Ok(AuDeadlineWriter { gate: self.clone() })
    }
    // Called only by the installed responder's actual bind_local_stream callback.
    pub(crate) fn register_owned_responder(
        &self,
        primary: (u32, u8),
        rtx: Option<(u32, u8)>,
    ) -> io::Result<()> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.owned_responder.is_some() || s.au_deadlines_enabled || s.media_started {
            return Err(invalid());
        }
        s.owned_responder = Some(Binding { primary, rtx });
        Ok(())
    }
    /// Finite localhost diagnostic configured by the application before RTP:
    /// hold one media send at the final poll after ten seconds of actual sends.
    /// Its ORIGINAL deadline remains active and causes a policy stop.
    pub fn arm_au_hold_diagnostic(&self) -> io::Result<()> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !s.au_deadlines_enabled || s.media_started || s.fenced || s.fault_au_hold_armed {
            return Err(invalid());
        }
        s.fault_au_hold_armed = true;
        Ok(())
    }
    // Call only while holding the SAME Peer core lock used by sender enable.
    // This serializes the check with both configuration and the core mutation.
    pub(crate) fn ensure_mutable_binding(&self) -> io::Result<()> {
        if self.snapshot().au_deadlines_enabled {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "sender bindings immutable with AU deadlines",
            ))
        } else {
            Ok(())
        }
    }
    // Gate-enabled sends retain socket ownership across Pending. Transport
    // replacement is deliberately unsupported for that Peer, even without AU
    // deadlines. Call under the core lock, before changing core configuration.
    pub(crate) fn ensure_mutable_transport(&self) -> io::Result<()> {
        if self.snapshot().enabled {
            Err(io::Error::new(io::ErrorKind::Unsupported,"transport immutable with media egress gate"))
        } else { Ok(()) }
    }
    pub(crate) fn register_sender(
        &self,
        id: RTCRtpSenderId,
        p: &RTCRtpSendParameters,
    ) -> io::Result<()> {
        let bad = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.multiple_senders |= state.bound_sender.is_some_and(|old| old != id);
            state.bound_sender.get_or_insert(id);
            state.deadlines.as_ref().is_some_and(|t| {
                state.multiple_senders
                    || Binding::from_parameters(p, t.binding.primary.0, t.binding.primary.1)
                        != Ok(t.binding)
            })
        };
        if bad {
            Err(self.reject_reason("au_binding_changed"))
        } else {
            Ok(())
        }
    }
    /// Enable before the first RTP submission. Payloads64..95 cannot be used
    /// with this RTP/RTCP mux classifier. The production codec uses dynamic PTs.
    pub fn enable(&self, payload_type: u8) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.enabled
            || state.fenced
            || state.media_started
            || state.unsafe_binding
            || payload_type > 127
            || (64..=95).contains(&payload_type)
        {
            return Err(invalid());
        }
        state.enabled = true;
        Ok(())
    }
    /// Read cumulative counters without changing the stop latch.
    pub fn snapshot(&self) -> Snapshot {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot
    }
    /// Returns only after any synchronous send poll has returned. No mutex is
    /// held across await. Bytes previously accepted by the socket cannot be recalled.
    pub fn fence(&self) -> Snapshot {
        let snapshot = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.fenced = true;
            s.snapshot
        };
        self.pending.wake();
        snapshot
    }
    pub(crate) fn register_payloads(&self, payloads: impl Iterator<Item = u8>) -> io::Result<()> {
        let unsafe_binding = payloads
            .into_iter()
            .any(|pt| pt > 127 || (64..=95).contains(&pt));
        let reject = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.unsafe_binding |= unsafe_binding;
            s.enabled && s.unsafe_binding
        };
        if reject { Err(self.reject()) } else { Ok(()) }
    }
    // Only the owning bounded responder may suppress a repair here. Unknown or
    // foreign metadata never becomes an expired-success decision. Use this
    // gate's current runtime clock, not the RTCP packet's queued receive time.
    pub(crate) fn repair_expired(
        &self,
        proof: Option<&super::au_provenance::Proof>,
    ) -> io::Result<bool> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !s.au_deadlines_enabled {
            return Ok(false);
        }
        let proof = proof.ok_or_else(invalid)?;
        let budget = s.proof_budget.as_ref().ok_or_else(invalid)?;
        let table = s.deadlines.as_ref().ok_or_else(invalid)?;
        if !proof.issued_by(budget) || proof.binding != table.binding {
            return Err(invalid());
        }
        Ok(self.now() >= proof.deadline)
    }

    /// Capture before the actual driver queue; no later timestamp lookup can replace this owner.
    pub(crate) fn capture_attempt(
        &self,
        packet: &rtc::rtp::packet::Packet,
    ) -> io::Result<Option<LocalPacketContext>> {
        let result = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if s.fenced {
                return Err(invalid());
            }
            let Some(table) = &s.deadlines else {
                return Ok(None);
            };
            let key = WireKey::from_packet(packet);
            table
                .proof(key.identity)
                .and_then(|proof| {
                    let bytes = crate::bounded_nack::rtp_size(packet).ok_or("au_packet_size")?;
                    Attempt::new(proof, key, bytes)
                })
                .map(|a| Some(a.context()))
        };
        result.map_err(|r| {
            if r == "au_unknown" {
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                s.unknown_au_datagrams = s.unknown_au_datagrams.saturating_add(1);
            }
            self.reject_reason(r)
        })
    }
    pub(crate) fn validate_context(
        &self,
        packet: &rtc::rtp::packet::Packet,
        context: Option<&LocalPacketContext>,
    ) -> io::Result<()> {
        let result = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match (&s.proof_budget, context) {
                (Some(b), Some(c)) => Attempt::from_context(c)
                    .ok_or("au_context_type")
                    .and_then(|a| a.validate(b, WireKey::from_packet(packet)))
                    .map(|_| ()),
                (Some(_), None) => Err("au_context_missing"),
                (None, None) => Ok(()),
                (None, Some(_)) => Err("au_context_unexpected"),
            }
        };
        result.map_err(|r| self.reject_reason(r))
    }
    /// Logical ownership counters, independent of cache gauges and RTC buffer copies.
    pub fn provenance_snapshot(&self) -> ProvenanceSnapshot {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .proof_budget
            .as_ref()
            .map_or_else(Default::default, |b| b.snapshot())
    }
    pub(crate) fn release_registration_owners(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.deadlines = None;
        s.au_records = 0;
    }
    pub(crate) fn admit_rtp(&self, payload: u8) -> io::Result<()> {
        let reject = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if s.fenced {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "media egress fenced",
                ));
            }
            s.media_started = true;
            s.enabled && (payload > 127 || (64..=95).contains(&payload))
        };
        if reject { Err(self.reject()) } else { Ok(()) }
    }
    pub(crate) fn reject(&self) -> io::Error {
        self.reject_reason("unsupported_transport_or_payload")
    }
    pub(crate) fn reject_reason(&self, reason: &'static str) -> io::Error {
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.failed = true;
            s.fenced = true;
            s.rejection_reason.get_or_insert(reason);
            s.rejected_datagrams = s.rejected_datagrams.saturating_add(1);
        }
        self.pending.wake();
        invalid()
    }
    pub(crate) fn class(&self, bytes: &[u8], turn: bool) -> io::Result<Class> {
        if !self.snapshot().enabled {
            return Ok(Class::Control);
        }
        classify(bytes, turn).map_err(|reason| self.reject_reason(reason))
    }
    pub(crate) async fn send(
        &self,
        socket: &dyn AsyncUdpSocket,
        transmit: &Transmit<'_>,
        turn: bool,
    ) -> io::Result<usize> {
        self.send_owned(socket, transmit, turn, &[]).await
    }
    pub(crate) async fn send_owned(
        &self,
        socket: &dyn AsyncUdpSocket,
        transmit: &Transmit<'_>,
        turn: bool,
        contexts: &[Option<&LocalPacketContext>],
    ) -> io::Result<usize> {
        if !self.snapshot().enabled {
            return futures::future::poll_fn(|cx| socket.poll_send(cx, transmit)).await;
        }
        let size = transmit.segment_size.unwrap_or(transmit.contents.len());
        if size == 0 {
            return Err(self.reject());
        }
        let mut class = None;
        let mut count = 0u64;
        let mut deadline: Option<Instant> = None;
        let mut attempts: Vec<&Attempt> = Vec::new();
        if !contexts.is_empty() && contexts.len() != transmit.contents.len().div_ceil(size) {
            return Err(self.reject_reason("au_context_count"));
        }
        for (index, part) in transmit.contents.chunks(size).enumerate() {
            let next = self.class(part, turn)?;
            if class.is_some_and(|c| c != next) {
                return Err(self.reject());
            }
            class = Some(next);
            count += 1;
            let context = contexts.get(index).copied().flatten();
            if next == Class::Control {
                if context.is_some() {
                    return Err(self.reject_reason("au_context_on_control"));
                }
                continue;
            }
            let key = inspect(part, turn)
                .map_err(|r| self.reject_reason(r))?
                .ok_or_else(invalid)?;
            let result = {
                let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if s.fenced {
                    Ok(None)
                } else if let Some(budget) = &s.proof_budget {
                    context
                        .ok_or("au_context_missing")
                        .and_then(|c| Attempt::from_context(c).ok_or("au_context_type"))
                        .and_then(|a| {
                            if attempts.iter().any(|held| std::ptr::eq(*held, a)) {
                                return Err("au_attempt_duplicate_batch");
                            }
                            let d = a.validate(budget, key)?;
                            attempts.push(a);
                            Ok(Some(d))
                        })
                } else if context.is_some() {
                    Err("au_context_unexpected")
                } else {
                    Ok(None)
                }
            };
            let at = result.map_err(|reason| self.reject_reason(reason))?;
            if let Some(at) = at {
                deadline = Some(deadline.map_or(at, |old| old.min(at)));
            }
        }
        let media = class == Some(Class::Media);
        struct PendingGuard<'a>(&'a MediaEgress, bool);
        impl Drop for PendingGuard<'_> {
            fn drop(&mut self) {
                if self.1 {
                    self.0
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .pending_media_send = false;
                }
            }
        }
        let _pending = PendingGuard(self, media);
        let mut timer = deadline.map(|at| {
            self.runtime
                .as_ref()
                .expect("deadline runtime")
                .sleep(at.saturating_duration_since(self.now()))
        });
        let mut timing = SendTiming::new(self.now(), turn);
        futures::future::poll_fn(|cx| {
            // Arm even when UDP is not writable. The subsequent check under the
            // submission mutex uses the original absolute deadline, never this wake time.
            if let Some(timer) = &mut timer {
                let _ = timer.as_mut().poll(cx);
            }
            self.poll_send_owned(
                socket,
                cx,
                transmit,
                (media, count, false, deadline),
                &attempts,
                &mut timing,
            )
        })
        .await
    }
    pub(crate) async fn send_mdns(
        &self,
        socket: &dyn AsyncUdpSocket,
        transmit: &Transmit<'_>,
        transport: &rtc::shared::TransportContext,
    ) -> io::Result<usize> {
        if self.snapshot().enabled
            && (transmit.segment_size.is_some()
                || transmit.destination != transport.peer_addr
                || !super::mdns_egress::validate(
                    transmit.contents,
                    transport,
                    socket
                        .local_addr()
                        .map_err(|_| self.reject_reason("mdns_socket_unavailable"))?,
                ))
        {
            return Err(self.reject_reason("mdns_framing_or_transport"));
        }
        if !self.snapshot().enabled {
            return futures::future::poll_fn(|cx| socket.poll_send(cx, transmit)).await;
        }
        futures::future::poll_fn(|cx| self.poll_send(socket, cx, transmit, (false, 1, true, None)))
            .await
    }
    fn poll_send(
        &self,
        socket: &dyn AsyncUdpSocket,
        cx: &mut Context<'_>,
        transmit: &Transmit<'_>,
        context: (bool, u64, bool, Option<Instant>),
    ) -> Poll<io::Result<usize>> {
        self.poll_send_owned(
            socket,
            cx,
            transmit,
            context,
            &[],
            &mut SendTiming::new(self.now(), false),
        )
    }
    fn poll_send_owned(
        &self,
        socket: &dyn AsyncUdpSocket,
        cx: &mut Context<'_>,
        transmit: &Transmit<'_>,
        context: (bool, u64, bool, Option<Instant>),
        attempts: &[&Attempt],
        timing: &mut SendTiming,
    ) -> Poll<io::Result<usize>> {
        let (media, count, mdns, deadline) = context;
        // Single driver writer per Peer. Registration precedes checking the latch.
        if media {
            self.pending.register(cx.waker());
        }
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if media && s.fenced {
            s.pending_media_send = false;
            s.blocked_media_datagrams = s.blocked_media_datagrams.saturating_add(count);
            // Deliberate discard, not a claimed kernel submission. Only the
            // internal transport drain uses this return value; counters distinguish it.
            return Poll::Ready(Ok(transmit.contents.len()));
        }
        let now = self.now();
        timing.first_poll(now, deadline);
        if media && deadline.is_some_and(|at| now >= at) {
            s.pending_media_send = false;
            s.fenced = true;
            s.expired_datagrams = s.expired_datagrams.saturating_add(count);
            s.policy_stop_reason.get_or_insert("au_deadline_expired");
            let stage = if timing.pending_polls > 0 {
                "after_socket_pending"
            } else if s.fault_au_hold_triggered {
                "diagnostic_hold"
            } else {
                "before_socket_poll"
            };
            timing.record(&mut s, now, stage);
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                LocalPolicyStop,
            )));
        }
        if media
            && s.fault_au_hold_armed
            && s.first_media_submission.is_some_and(|at| {
                self.now().saturating_duration_since(at) >= std::time::Duration::from_secs(10)
            })
        {
            if !s.fault_au_hold_triggered {
                s.fault_au_hold_triggered = true;
                s.fault_au_hold_after_first_submit_ms = s.first_media_submission.map(|at| {
                    self.now()
                        .saturating_duration_since(at)
                        .as_millis()
                        .min(u64::MAX as u128) as u64
                });
            }
            s.pending_media_send = true;
            return Poll::Pending;
        }
        if attempts.iter().any(|a| {
            s.proof_budget
                .as_ref()
                .is_none_or(|b| a.validate(b, a.key).is_err())
        }) {
            s.failed = true;
            s.fenced = true;
            s.rejection_reason.get_or_insert("au_attempt_reused");
            s.rejected_datagrams = s.rejected_datagrams.saturating_add(count);
            return Poll::Ready(Err(invalid()));
        }
        match socket.poll_send(cx, transmit) {
            Poll::Pending => {
                timing.pending_polls = timing.pending_polls.saturating_add(1);
                if media {
                    s.pending_media_send = true;
                }
                Poll::Pending
            }
            Poll::Ready(result) => {
                if mdns && result.is_err() {
                    s.mdns_send_errors = s.mdns_send_errors.saturating_add(1);
                }
                if media {
                    s.pending_media_send = false;
                }
                if result.is_ok() {
                    for attempt in attempts {
                        attempt.mark_submitted();
                    }
                }
                if result.as_ref().is_ok_and(|n| *n != transmit.contents.len()) {
                    s.failed = true;
                    s.fenced = true;
                    s.rejected_datagrams = s.rejected_datagrams.saturating_add(count);
                    s.rejection_reason.get_or_insert("partial_udp_submission");
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "partial UDP media egress send",
                    )));
                }
                if result.as_ref().is_ok_and(|n| *n == transmit.contents.len()) {
                    if media {
                        s.first_media_submission.get_or_insert(self.now());
                    }
                    let counter = if media {
                        &mut s.media_submitted_datagrams
                    } else {
                        &mut s.control_submitted_datagrams
                    };
                    *counter = counter.saturating_add(count);
                    if media && deadline.is_some_and(|at| self.now() >= at) {
                        s.completion_overrun_datagrams =
                            s.completion_overrun_datagrams.saturating_add(count);
                        s.fenced = true;
                        s.policy_stop_reason.get_or_insert("au_deadline_expired");
                        timing.record(&mut s, self.now(), "completion_overrun");
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            LocalPolicyStop,
                        )));
                    }
                }
                Poll::Ready(result)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RecvMeta;
    use std::{
        io::IoSliceMut,
        net::SocketAddr,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        task::Waker,
        time::Duration,
    };
    #[derive(Debug, Default)]
    struct Socket {
        result_mode: std::sync::atomic::AtomicU8,
        blocked: AtomicBool,
        waker: Mutex<Option<Waker>>,
        sent: Mutex<Vec<Vec<u8>>>,
    }
    impl AsyncUdpSocket for Socket {
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok("127.0.0.1:30000".parse().unwrap())
        }
        fn poll_send(&self, cx: &mut Context<'_>, tx: &Transmit<'_>) -> Poll<io::Result<usize>> {
            match self.result_mode.load(Ordering::Acquire) {
                1 => return Poll::Ready(Ok(0)),
                2 => return Poll::Ready(Ok(tx.contents.len() - 1)),
                3 => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        "test socket refused",
                    )));
                }
                _ => {}
            }
            if self.blocked.load(Ordering::Acquire) {
                *self.waker.lock().unwrap() = Some(cx.waker().clone());
                return Poll::Pending;
            }
            self.sent.lock().unwrap().extend(
                tx.contents
                    .chunks(tx.segment_size.unwrap_or(tx.contents.len()))
                    .map(|v| v.to_vec()),
            );
            Poll::Ready(Ok(tx.contents.len()))
        }
        fn poll_recv(
            &self,
            _: &mut Context<'_>,
            _: &mut [IoSliceMut<'_>],
            _: &mut [RecvMeta],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }
        fn max_gso_segments(&self) -> usize {
            8
        }
    }
    fn rtp() -> Vec<u8> {
        let mut b = vec![0; 24];
        b[0] = 0x80;
        b[1] = 108;
        b
    }
    fn rtcp() -> Vec<u8> {
        let mut b = vec![0; 24];
        b[0] = 0x80;
        b[1] = 200;
        b[3] = 1;
        b
    }
    fn dtls() -> Vec<u8> {
        let mut b = vec![0; 24];
        b[0] = 23;
        b[1] = 0xfe;
        b[2] = 0xfd;
        b[12] = 11;
        b
    }
    fn stun(typ: u16, attrs: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut b = vec![0; 20];
        b[..2].copy_from_slice(&typ.to_be_bytes());
        b[4..8].copy_from_slice(&[0x21, 0x12, 0xa4, 0x42]);
        for (typ, value) in attrs {
            b.extend_from_slice(&typ.to_be_bytes());
            b.extend_from_slice(&(value.len() as u16).to_be_bytes());
            b.extend_from_slice(value);
            while !b.len().is_multiple_of(4) {
                b.push(0)
            }
        }
        let n = b.len() - 20;
        b[2..4].copy_from_slice(&(n as u16).to_be_bytes());
        b
    }
    fn channel(data: &[u8]) -> Vec<u8> {
        let mut b = vec![0x40, 0, 0, 0];
        b[2..4].copy_from_slice(&(data.len() as u16).to_be_bytes());
        b.extend_from_slice(data);
        b
    }
    fn indication(data: &[u8]) -> Vec<u8> {
        stun(
            0x16,
            &[(0x12, vec![0, 1, 0, 0, 0, 0, 0, 0]), (0x13, data.to_vec())],
        )
    }
    async fn send(
        g: &MediaEgress,
        s: &Socket,
        b: &[u8],
        seg: Option<usize>,
        turn: bool,
    ) -> io::Result<usize> {
        g.send(
            s,
            &Transmit {
                destination: "127.0.0.1:3478".parse().unwrap(),
                ecn: None,
                contents: b,
                segment_size: seg,
                src_ip: None,
            },
            turn,
        )
        .await
    }
    #[tokio::test]
    async fn mdns_send_error_is_counted_without_submission_and_cancellation_is_not_error() {
        #[derive(Debug, Default)]
        struct MdnsSocket(Socket);
        impl AsyncUdpSocket for MdnsSocket {
            fn local_addr(&self) -> io::Result<SocketAddr> {
                Ok("0.0.0.0:5353".parse().unwrap())
            }
            fn poll_send(&self, cx: &mut Context<'_>, t: &Transmit<'_>) -> Poll<io::Result<usize>> {
                self.0.poll_send(cx, t)
            }
            fn poll_recv(
                &self,
                _: &mut Context<'_>,
                _: &mut [IoSliceMut<'_>],
                _: &mut [RecvMeta],
            ) -> Poll<io::Result<usize>> {
                Poll::Pending
            }
        }
        use rtc::sansio::Protocol;
        let now = std::time::Instant::now();
        let mut mdns = rtc::mdns::Mdns::new(now, rtc::mdns::MdnsConfig::default());
        mdns.query_now(now, "synthetic-error.local");
        let q = mdns.poll_write().unwrap();
        let t = Transmit {
            destination: q.transport.peer_addr,
            contents: &q.message,
            segment_size: None,
            ecn: None,
            src_ip: None,
        };
        let g = MediaEgress::default();
        g.enable(108).unwrap();
        g.fence();
        let s = MdnsSocket::default();
        s.0.result_mode.store(3, Ordering::Release);
        for _ in 0..2 {
            assert!(g.send_mdns(&s, &t, &q.transport).await.is_err());
        }
        assert_eq!(g.snapshot().mdns_send_errors, 2);
        assert_eq!(g.snapshot().control_submitted_datagrams, 0);
        assert!(!g.snapshot().failed);
        assert!(g.snapshot().fenced);
        assert_eq!(g.snapshot().rejection_reason, None);
        s.0.result_mode.store(0, Ordering::Release);
        s.0.blocked.store(true, Ordering::Release);
        {
            let future = g.send_mdns(&s, &t, &q.transport);
            futures::pin_mut!(future);
            assert!(futures::poll!(future).is_pending());
        }
        assert_eq!(g.snapshot().mdns_send_errors, 2);
        assert!(!g.snapshot().pending_media_send);
        s.0.blocked.store(false, Ordering::Release);
        g.send_mdns(&s, &t, &q.transport).await.unwrap();
        assert_eq!(g.snapshot().control_submitted_datagrams, 1);
        assert_eq!(g.snapshot().mdns_send_errors, 2);
        s.0.result_mode.store(2, Ordering::Release);
        assert!(g.send_mdns(&s, &t, &q.transport).await.is_err());
        assert!(g.snapshot().failed);
        assert_eq!(
            g.snapshot().rejection_reason,
            Some("partial_udp_submission")
        );
        assert_eq!(g.snapshot().mdns_send_errors, 2);
        assert_eq!(g.snapshot().control_submitted_datagrams, 1);
    }

    #[tokio::test]
    async fn mdns_socket_identity_error_is_sticky_before_any_send() {
        #[derive(Debug)]
        struct Unavailable;
        impl AsyncUdpSocket for Unavailable {
            fn local_addr(&self) -> io::Result<SocketAddr> {
                Err(io::Error::from(io::ErrorKind::NotConnected))
            }
            fn poll_send(&self, _: &mut Context<'_>, _: &Transmit<'_>) -> Poll<io::Result<usize>> {
                panic!("must not send without socket identity")
            }
            fn poll_recv(
                &self,
                _: &mut Context<'_>,
                _: &mut [std::io::IoSliceMut<'_>],
                _: &mut [crate::runtime::RecvMeta],
            ) -> Poll<io::Result<usize>> {
                Poll::Pending
            }
        }
        let gate = MediaEgress::default();
        gate.enable(108).unwrap();
        let tp = rtc::shared::TransportContext {
            local_addr: "0.0.0.0:5353".parse().unwrap(),
            peer_addr: rtc::mdns::MDNS_DEST_ADDR,
            ..Default::default()
        };
        let transmit = Transmit {
            destination: tp.peer_addr,
            contents: &[],
            segment_size: None,
            ecn: None,
            src_ip: None,
        };
        assert!(gate.send_mdns(&Unavailable, &transmit, &tp).await.is_err());
        assert_eq!(
            gate.snapshot().rejection_reason,
            Some("mdns_socket_unavailable")
        );
        assert_eq!(gate.snapshot().control_submitted_datagrams, 0);
        assert!(gate.snapshot().failed);
    }

    #[test]
    fn first_fixed_rejection_reason_is_retained_without_payload() {
        let gate = MediaEgress::default();
        gate.enable(108).unwrap();
        assert_eq!(gate.snapshot().rejection_reason, None);
        assert!(gate.class(&[0], false).is_err());
        assert_eq!(
            gate.snapshot().rejection_reason,
            Some("stun_framing_or_method")
        );
        assert!(gate.class(&[255], false).is_err());
        assert_eq!(
            gate.snapshot().rejection_reason,
            Some("stun_framing_or_method")
        );
        assert_eq!(gate.snapshot().rejected_datagrams, 2);
    }

    #[test]
    fn real_srtcp_sender_report_and_sdes_cipher_outputs_are_control() {
        use rtc::rtcp::{
            sender_report::SenderReport,
            source_description::{
                SdesType, SourceDescription, SourceDescriptionChunk, SourceDescriptionItem,
            },
        };
        use rtc::srtp::{context::Context, protection_profile::ProtectionProfile};
        let packets: Vec<Box<dyn rtc::rtcp::packet::Packet>> = vec![
            Box::new(SenderReport {
                ssrc: 12345678,
                packet_count: 199,
                octet_count: 240000,
                ..Default::default()
            }),
            Box::new(SourceDescription {
                chunks: vec![SourceDescriptionChunk {
                    source: 12345678,
                    items: vec![SourceDescriptionItem {
                        sdes_type: SdesType::SdesCname,
                        text: bytes::Bytes::from_static(b"synthetic-test"),
                    }],
                }],
            }),
        ];
        let crypto = rtc::crypto::default_provider().unwrap();
        for profile in [
            ProtectionProfile::Aes128CmHmacSha1_80,
            ProtectionProfile::AeadAes128Gcm,
        ] {
            let mut ctx = Context::new(
                &vec![1; profile.key_len()],
                &vec![2; profile.salt_len()],
                profile,
                None,
                None,
                crypto.crypto(),
            )
            .unwrap();
            let encrypted_rtp = ctx.encrypt_rtp(&rtp()).unwrap();
            assert_eq!(classify(&encrypted_rtp, false).unwrap(), Class::Media);
            assert_eq!(
                classify(&channel(&encrypted_rtp), true).unwrap(),
                Class::Media
            );
            for plain in [
                rtc::rtcp::packet::marshal(&packets[..1]).unwrap(),
                rtc::rtcp::packet::marshal(&packets[1..]).unwrap(),
                rtc::rtcp::packet::marshal(&packets).unwrap(),
            ] {
                let encrypted = ctx.encrypt_rtcp(&plain).unwrap();
                assert_eq!(classify(&encrypted, false).unwrap(), Class::Control);
                assert_eq!(
                    classify(&channel(&encrypted), true).unwrap(),
                    Class::Control
                );
                assert_eq!(
                    classify(&indication(&encrypted), true).unwrap(),
                    Class::Control
                );
            }
        }
    }

    #[test]
    fn exact_mux_and_scoped_turn_classification() {
        for b in [rtp(), channel(&rtp()), indication(&rtp())] {
            assert_eq!(classify(&b, b[0] < 128).unwrap(), Class::Media)
        }
        for b in [rtcp(), dtls(), stun(1, &[]), stun(4, &[])] {
            assert_eq!(classify(&b, false).unwrap(), Class::Control);
            assert_eq!(classify(&channel(&b), true).unwrap(), Class::Control);
            assert_eq!(classify(&indication(&b), true).unwrap(), Class::Control);
        }
        assert!(classify(&channel(&rtp()), false).is_err());
        assert!(classify(&indication(&rtp()), false).is_err());
        assert!(classify(&channel(&channel(&rtp())), true).is_err());
        assert!(classify(&indication(&indication(&rtp())), true).is_err());
    }
    #[test]
    fn malformed_duplicate_trailing_and_unknown_are_not_control() {
        let mut tail = channel(&rtp());
        tail.push(0);
        let duplicate = stun(0x16, &[(0x12, vec![0; 8]), (0x13, rtp()), (0x13, rtp())]);
        let mut invalid_dtls = dtls();
        invalid_dtls[12] = 12;
        let mut invalid_stun = stun(1, &[]);
        invalid_stun.push(0);
        for b in [
            vec![],
            vec![0x80],
            vec![255; 24],
            stun(0x18, &[]),
            tail,
            duplicate,
            invalid_dtls,
            invalid_stun,
            stun(1, &[(0x13, rtp())]),
            stun(0x16, &[(0x13, rtp())]),
        ] {
            assert!(classify(&b, true).is_err());
        }
        let mut partial = rtp();
        partial.push(0);
        let mut padded = channel(&partial);
        while !padded.len().is_multiple_of(4) {
            padded.push(0)
        }
        assert_eq!(classify(&padded, true).unwrap(), Class::Media);
        padded.extend([0; 4]);
        assert!(classify(&padded, true).is_err());
    }
    #[test]
    fn ambiguous_payload_and_reenable_refused() {
        for pt in [64, 72, 95, 128, 255] {
            assert!(MediaEgress::default().enable(pt).is_err())
        }
        let g = MediaEgress::default();
        g.enable(108).unwrap();
        g.fence();
        assert!(g.enable(108).is_err());
    }
    #[tokio::test]
    async fn pending_send_woken_by_fence_without_socket_writability() {
        let g = Arc::new(MediaEgress::default());
        g.enable(108).unwrap();
        let s = Arc::new(Socket::default());
        s.blocked.store(true, Ordering::Release);
        let (gg, ss) = (g.clone(), s.clone());
        let task = tokio::spawn(async move { send(&gg, &ss, &rtp(), None, false).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !g.snapshot().pending_media_send {
                tokio::task::yield_now().await
            }
        })
        .await
        .unwrap();
        let before = g.fence();
        assert!(before.pending_media_send);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let after = g.snapshot();
        assert!(!after.pending_media_send);
        assert_eq!(after.blocked_media_datagrams, 1);
        assert_eq!(after.media_submitted_datagrams, 0);
        assert!(s.sent.lock().unwrap().is_empty());
        s.blocked.store(false, Ordering::Release);
        for b in [rtcp(), dtls(), stun(4, &[])] {
            send(&g, &s, &b, None, false).await.unwrap();
        }
        assert_eq!(g.snapshot().control_submitted_datagrams, 3);
    }
    #[tokio::test]
    async fn queued_retransmissions_and_turn_wrapping_are_fenced_control_survives() {
        let g = MediaEgress::default();
        g.enable(108).unwrap();
        let s = Socket::default();
        send(&g, &s, &rtp(), None, false).await.unwrap();
        let before = g.fence();
        for _ in 0..4 {
            for b in [rtp(), channel(&rtp()), indication(&rtp())] {
                send(&g, &s, &b, None, b[0] < 128).await.unwrap();
            }
        }
        for b in [channel(&dtls()), indication(&rtcp()), stun(4, &[])] {
            send(&g, &s, &b, None, true).await.unwrap();
        }
        assert_eq!(
            g.snapshot().media_submitted_datagrams,
            before.media_submitted_datagrams
        );
        assert_eq!(g.snapshot().blocked_media_datagrams, 12);
        assert_eq!(s.sent.lock().unwrap().len(), 4);
        let other = MediaEgress::default();
        other.enable(108).unwrap();
        send(&other, &s, &rtp(), None, false).await.unwrap();
        assert_eq!(other.snapshot().media_submitted_datagrams, 1);
    }
    #[tokio::test]
    async fn homogeneous_gso_and_fallback_count_individual_datagrams_mixed_rejected() {
        let g = MediaEgress::default();
        g.enable(108).unwrap();
        let s = Socket::default();
        let media = rtp().repeat(4);
        send(&g, &s, &media, Some(24), false).await.unwrap();
        g.fence();
        send(&g, &s, &media, Some(24), false).await.unwrap();
        send(&g, &s, &rtcp().repeat(3), Some(24), false)
            .await
            .unwrap();
        send(&g, &s, &rtcp(), None, false).await.unwrap();
        assert_eq!(g.snapshot().media_submitted_datagrams, 4);
        assert_eq!(g.snapshot().blocked_media_datagrams, 4);
        assert_eq!(g.snapshot().control_submitted_datagrams, 4);
        let mut mixed = rtcp();
        mixed.extend(rtp());
        assert!(send(&g, &s, &mixed, Some(24), false).await.is_err());
        assert_eq!(s.sent.lock().unwrap().len(), 8);
        assert!(g.snapshot().failed);
    }
    #[tokio::test]
    async fn dropped_pending_future_clears_only_pending_marker() {
        let g = MediaEgress::default();
        g.enable(108).unwrap();
        let s = Socket::default();
        s.blocked.store(true, Ordering::Release);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), send(&g, &s, &rtp(), None, false))
                .await
                .is_err()
        );
        assert!(!g.snapshot().pending_media_send);
        assert_eq!(g.snapshot().media_submitted_datagrams, 0);
    }
    #[test]
    fn unsafe_binding_or_rtp_admission_and_late_enable_fail_closed() {
        let g = MediaEgress::default();
        g.admit_rtp(108).unwrap();
        assert!(g.enable(108).is_err());
        let g = MediaEgress::default();
        g.register_payloads([72].into_iter()).unwrap();
        assert!(g.enable(108).is_err());
        let g = MediaEgress::default();
        g.enable(108).unwrap();
        assert!(g.admit_rtp(72).is_err());
        assert!(g.snapshot().failed);
        let g = MediaEgress::default();
        g.enable(108).unwrap();
        assert!(g.register_payloads([108, 72].into_iter()).is_err());
        assert!(g.snapshot().failed);
    }
    #[tokio::test]
    async fn short_udp_success_is_fatal_and_socket_error_is_not_submission() {
        for mode in 1..=3 {
            let g = MediaEgress::default();
            g.enable(108).unwrap();
            let s = Socket::default();
            s.result_mode.store(mode, Ordering::Release);
            assert!(send(&g, &s, &rtp(), None, false).await.is_err());
            assert_eq!(g.snapshot().media_submitted_datagrams, 0);
            assert_eq!(g.snapshot().failed, mode != 3);
            assert_eq!(g.snapshot().fenced, mode != 3);
        }
    }
    #[test]
    fn synchronous_poll_finishes_before_fence_returns() {
        #[derive(Debug)]
        struct Held {
            entered: Arc<std::sync::Barrier>,
            leave: Arc<std::sync::Barrier>,
        }
        impl AsyncUdpSocket for Held {
            fn local_addr(&self) -> io::Result<SocketAddr> {
                Ok("127.0.0.1:1".parse().unwrap())
            }
            fn poll_send(&self, _: &mut Context<'_>, t: &Transmit<'_>) -> Poll<io::Result<usize>> {
                self.entered.wait();
                self.leave.wait();
                Poll::Ready(Ok(t.contents.len()))
            }
            fn poll_recv(
                &self,
                _: &mut Context<'_>,
                _: &mut [IoSliceMut<'_>],
                _: &mut [RecvMeta],
            ) -> Poll<io::Result<usize>> {
                Poll::Pending
            }
        }
        let g = Arc::new(MediaEgress::default());
        g.enable(108).unwrap();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let leave = Arc::new(std::sync::Barrier::new(2));
        let gg = g.clone();
        let socket = Held {
            entered: entered.clone(),
            leave: leave.clone(),
        };
        let send = std::thread::spawn(move || {
            futures::executor::block_on(async {
                let b = rtp();
                gg.send(
                    &socket,
                    &Transmit {
                        destination: "127.0.0.1:2".parse().unwrap(),
                        ecn: None,
                        contents: &b,
                        segment_size: None,
                        src_ip: None,
                    },
                    false,
                )
                .await
                .unwrap()
            })
        });
        entered.wait();
        let gg = g.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let fence = std::thread::spawn(move || tx.send(gg.fence()).unwrap());
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
        leave.wait();
        send.join().unwrap();
        let s = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        fence.join().unwrap();
        assert!(s.fenced);
        assert_eq!(s.media_submitted_datagrams, 1);
    }
}

#[cfg(test)]
#[path = "au_deadline_send_tests.rs"]
mod deadline_tests;

#[cfg(test)]
#[path = "au_owned_send_tests.rs"]
mod owned_send_tests;
