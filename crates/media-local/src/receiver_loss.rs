//! Opt-in, per-Peer RR loss response. Not a bandwidth estimator or wire-rate cap.
//! Register at slot 3500, below NACK retransmission generation and above no pacer.
//! Counters describe this interceptor's admission/forwarding, not UDP transmission.
use rtc::interceptor::{Attribute, Interceptor, Packet, StreamInfo, TaggedPacket};
use rtc::rtcp::{receiver_report::ReceiverReport, sender_report::SenderReport};
use rtc::sansio::Protocol;
use rtc::shared::error::Error;
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const INITIAL_BPS: u64 = 2_000_000;
pub const MIN_BPS: u64 = 250_000;
pub const MAX_BPS: u64 = INITIAL_BPS;
const FRESH: Duration = Duration::from_secs(10);
const COOLDOWN: Duration = Duration::from_secs(2);
const QUEUE: usize = 32;
const FAULT_PACKETS: u64 = 16_384;

#[derive(Clone, Debug, Serialize)]
pub struct Diagnostics {
    pub status: &'static str,
    pub observation: u64,
    pub last_report_age_ms: Option<u64>,
    pub last_sr_age_ms: Option<u64>,
    pub delta_expected: u64,
    pub delta_lost: i64,
    pub eligible_windows: u64,
    pub good_windows: u32,
    pub original_packets: u64,
    pub retransmissions: u64,
    pub forwarded_packets: u64,
    pub injected_drops: u64,
    pub fault_state: &'static str,
    pub rejected_reports: u64,
    pub last_rejection_reason: Option<&'static str>,
    pub remote_highest_sequence: Option<u64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct DecisionEvidence {
    pub observation: u64,
    pub delta_expected: u64,
    pub delta_lost: i64,
    pub local_packet_delta: u64,
    pub good_windows: u32,
    pub report_age_ms: u64,
    pub sr_age_ms: u64,
}
#[derive(Clone, Copy)]
struct Cause {
    observation: u64,
    expected: u64,
    lost: i64,
    local: u64,
    good: u32,
    report_at: Instant,
    sr_at: Instant,
}
impl Cause {
    fn evidence(self, now: Instant) -> Option<DecisionEvidence> {
        let report_age = age(now, self.report_at).filter(|a| *a <= FRESH)?;
        let sr_age = age(now, self.sr_at).filter(|a| *a <= FRESH)?;
        Some(DecisionEvidence {
            observation: self.observation,
            delta_expected: self.expected,
            delta_lost: self.lost,
            local_packet_delta: self.local,
            good_windows: self.good,
            report_age_ms: report_age.as_millis() as u64,
            sr_age_ms: sr_age.as_millis() as u64,
        })
    }
}
#[derive(Clone, Debug)]
pub struct RateUpdate {
    pub target_bps: u64,
    pub reason: &'static str,
    pub observation: u64,
    pub cause: DecisionEvidence,
}
pub struct Poll {
    pub update: Option<RateUpdate>,
    pub diagnostics: Diagnostics,
    pub failure: Option<&'static str>,
}
#[derive(Clone)]
pub struct LossFeedback(Arc<Mutex<State>>);
pub struct ReceiverLoss {
    state: Arc<Mutex<State>>,
    read: VecDeque<TaggedPacket>,
    write: VecDeque<TaggedPacket>,
}
#[derive(Clone, Copy)]
struct Sr {
    lsr: u32,
    at: Instant,
}
#[derive(Clone, Copy)]
struct Baseline {
    high: u64,
    lost: i64,
    local: u64,
}
struct State {
    ssrc: u32,
    bound: bool,
    retired: bool,
    reporter: Option<u32>,
    first: Option<u64>,
    high: u64,
    start: Option<Instant>,
    sr: VecDeque<Sr>,
    baseline: Option<Baseline>,
    previous: Option<Baseline>,
    last_report: Option<Instant>,
    last_sr: Option<Instant>,
    last_decision: Option<Instant>,
    pending_loss: Option<Cause>,
    latest_window: Option<Cause>,
    fault: bool,
    fault_first: Option<u64>,
    fault_packets: u64,
    failure: Option<&'static str>,
    d: Diagnostics,
}
fn age(now: Instant, then: Instant) -> Option<Duration> {
    now.checked_duration_since(then)
}
fn signed24(raw: u32) -> Option<i64> {
    if raw > 0xff_ffff || raw == 0x7f_ffff || raw == 0x80_0000 {
        return None;
    }
    Some(((raw << 8) as i32 >> 8) as i64)
}
impl State {
    fn reset_windows(&mut self) {
        self.baseline = None;
        self.d.good_windows = 0;
        self.latest_window = None;
    }
    fn reject(&mut self, reason: &'static str) {
        self.d.rejected_reports += 1;
        self.d.last_rejection_reason = Some(reason);
    }
    fn fault_state(&self, now: Instant) -> &'static str {
        if !self.fault {
            return "disabled";
        }
        if self.failure.is_some() {
            return "budget_exhausted";
        }
        match self.start.and_then(|s| age(now, s)) {
            None => "awaiting_rtp",
            Some(t) if t < Duration::from_secs(5) => "clean_before",
            Some(t) if t < Duration::from_secs(13) => "dropping",
            Some(_) => "complete",
        }
    }
    // Map the report's low 32 bits to this incarnation's known intended sequence space.
    fn report_high(&self, raw: u32) -> Option<u64> {
        let mut value = (self.high & !0xffff_ffff) | raw as u64;
        if value > self.high {
            value = value.checked_sub(1_u64 << 32)?;
        }
        (value >= self.first? && self.high - value < (1_u64 << 31)).then_some(value)
    }
    fn rtp(&mut self, now: Instant, seq: u16, retransmission: bool) -> bool {
        if !self.bound {
            return false;
        }
        let original;
        let extended;
        match self.first {
            None => {
                if retransmission {
                    return false;
                }
                self.first = Some(seq as u64);
                self.high = seq as u64;
                self.start = Some(now);
                original = true;
                extended = self.high;
            }
            Some(first) => {
                let delta = seq.wrapping_sub(self.high as u16);
                if !retransmission && delta > 0 && delta < 0x8000 {
                    self.high += delta as u64;
                    original = true;
                    extended = self.high;
                } else {
                    let behind = (self.high as u16).wrapping_sub(seq) as u64;
                    if behind >= 0x8000 || self.high < behind || self.high - behind < first {
                        return false;
                    }
                    extended = self.high - behind;
                    original = false;
                }
            }
        }
        if original {
            self.d.original_packets += 1;
        } else {
            self.d.retransmissions += 1;
        }
        if self.fault_state(now) == "dropping" {
            // Mathematical sequence selection needs no packet payload/decision buffer.
            // The finite phase cannot cross half of the u16 sequence space.
            let begin = *self.fault_first.get_or_insert(self.high);
            self.fault_packets += 1;
            if self.high.saturating_sub(begin) >= FAULT_PACKETS
                || self.fault_packets > FAULT_PACKETS * 4
            {
                self.failure = Some("rtp_impairment_budget_exhausted");
            } else if extended >= begin && (extended - begin) % 10 == 9 {
                self.d.injected_drops += 1;
                return true;
            }
        }
        self.d.forwarded_packets += 1;
        false
    }
    fn sender_report(&mut self, now: Instant, report: &SenderReport) {
        if !self.bound || report.ssrc != self.ssrc || self.first.is_none() {
            return;
        }
        self.sr
            .retain(|s| age(now, s.at).is_some_and(|a| a <= FRESH));
        let lsr = (report.ntp_time >> 16) as u32;
        if lsr == 0 {
            return;
        }
        // Repeated LSR must not make an old sender report fresh again.
        if self.sr.iter().any(|s| s.lsr == lsr) {
            return;
        }
        if self.sr.len() == 16 {
            self.sr.pop_front();
        }
        self.sr.push_back(Sr { lsr, at: now });
    }
    fn receiver_report(&mut self, now: Instant, report: &ReceiverReport) {
        if !self.bound {
            return;
        }
        if self.last_report.is_some_and(|last| now < last) {
            self.reject("arrival_time_regressed");
            return;
        }
        if report.reports.len() > 31 {
            self.reject("too_many_report_blocks");
            return;
        }
        let mut matches = report.reports.iter().filter(|r| r.ssrc == self.ssrc);
        let Some(r) = matches.next() else {
            return;
        };
        if matches.next().is_some() {
            self.reject("duplicate_ssrc_blocks");
            return;
        }
        let Some(sr) = self
            .sr
            .iter()
            .find(|s| s.lsr == r.last_sender_report)
            .copied()
        else {
            self.reject("unknown_lsr");
            return;
        };
        let Some(elapsed) = age(now, sr.at).filter(|a| *a <= FRESH) else {
            self.reject("stale_lsr");
            return;
        };
        let dlsr = Duration::from_nanos((r.delay as u64 * 1_000_000_000) / 65_536);
        if dlsr > elapsed {
            self.reject("invalid_dlsr");
            return;
        }
        if self.reporter.is_some_and(|sender| sender != report.ssrc) {
            self.reject("reporter_changed");
            self.reset_windows();
            return;
        }
        let (Some(high), Some(lost)) = (
            self.report_high(r.last_sequence_number),
            signed24(r.total_lost),
        ) else {
            self.reject("invalid_sequence_or_loss");
            self.reset_windows();
            return;
        };
        let current = Baseline {
            high,
            lost,
            local: self.d.original_packets,
        };
        if let Some(previous) = self.previous {
            if high <= previous.high || high - previous.high >= 1_u64 << 31 {
                self.reject("duplicate_or_reordered");
                return;
            }
            if lost < previous.lost {
                self.reset_windows();
                self.baseline = Some(current);
            }
        }
        if self
            .last_report
            .is_some_and(|t| age(now, t).is_none_or(|a| a > FRESH))
        {
            self.reset_windows();
            self.pending_loss = None;
        }
        self.reporter = Some(report.ssrc);
        self.previous = Some(current);
        self.last_report = Some(now);
        self.last_sr = Some(sr.at);
        self.d.observation += 1;
        self.d.remote_highest_sequence = Some(high);
        let Some(base) = self.baseline else {
            self.baseline = Some(current);
            return;
        };
        let expected = high - base.high;
        if expected < 128 || current.local - base.local < 128 {
            return;
        }
        let lost = lost - base.lost;
        self.baseline = Some(current);
        if lost < 0 || lost as u64 > expected {
            self.reset_windows();
            self.baseline = Some(current);
            self.reject("invalid_loss_delta");
            return;
        }
        self.d.delta_expected = expected;
        self.d.delta_lost = lost;
        self.d.eligible_windows += 1;
        if lost as u64 * 100 >= expected * 10 {
            self.d.good_windows = 0;
        } else if lost as u64 * 100 <= expected {
            self.d.good_windows = self.d.good_windows.saturating_add(1).min(3);
        } else {
            self.d.good_windows = 0;
        }
        let cause = Cause {
            observation: self.d.observation,
            expected,
            lost,
            local: current.local - base.local,
            good: self.d.good_windows,
            report_at: now,
            sr_at: sr.at,
        };
        self.latest_window = Some(cause);
        if lost as u64 * 100 >= expected * 10 {
            self.pending_loss = Some(cause);
        }
    }
}
impl ReceiverLoss {
    pub fn new(expected_ssrc: u32, diagnostic_impairment: bool) -> (Self, LossFeedback) {
        let state = Arc::new(Mutex::new(State {
            ssrc: expected_ssrc,
            bound: false,
            retired: false,
            reporter: None,
            first: None,
            high: 0,
            start: None,
            sr: VecDeque::with_capacity(16),
            baseline: None,
            previous: None,
            last_report: None,
            last_sr: None,
            last_decision: None,
            pending_loss: None,
            latest_window: None,
            fault: diagnostic_impairment,
            fault_first: None,
            fault_packets: 0,
            failure: None,
            d: Diagnostics {
                status: "unbound",
                observation: 0,
                last_report_age_ms: None,
                last_sr_age_ms: None,
                delta_expected: 0,
                delta_lost: 0,
                eligible_windows: 0,
                good_windows: 0,
                original_packets: 0,
                retransmissions: 0,
                forwarded_packets: 0,
                injected_drops: 0,
                fault_state: "disabled",
                rejected_reports: 0,
                last_rejection_reason: None,
                remote_highest_sequence: None,
            },
        }));
        (
            Self {
                state: state.clone(),
                read: VecDeque::new(),
                write: VecDeque::new(),
            },
            LossFeedback(state),
        )
    }
}
impl LossFeedback {
    pub fn poll(&self, now: Instant, acknowledged_bps: u64) -> Poll {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if self.0.is_poisoned() {
            s.failure = Some("receiver_loss_state_poisoned");
        }
        let fresh = s
            .last_report
            .is_some_and(|t| age(now, t).is_some_and(|a| a <= FRESH))
            && s.last_sr
                .is_some_and(|t| age(now, t).is_some_and(|a| a <= FRESH));
        if !fresh || !s.bound {
            s.reset_windows();
        }
        if !s.bound {
            s.pending_loss = None;
        }
        if s.pending_loss
            .is_some_and(|cause| cause.evidence(now).is_none())
        {
            s.pending_loss = None;
        }
        s.d.status = if s.retired {
            "retired"
        } else if !s.bound {
            "unbound"
        } else if fresh {
            "fresh"
        } else {
            "feedback_unavailable"
        };
        s.d.last_report_age_ms = s
            .last_report
            .and_then(|t| age(now, t))
            .map(|d| d.as_millis() as u64);
        s.d.last_sr_age_ms = s
            .last_sr
            .and_then(|t| age(now, t))
            .map(|d| d.as_millis() as u64);
        s.d.fault_state = s.fault_state(now);
        let mut update = None;
        if !(MIN_BPS..=MAX_BPS).contains(&acknowledged_bps) {
            s.failure = Some("receiver_loss_ack_target_invalid");
        }
        if s.bound
            && (fresh || s.pending_loss.is_some())
            && s.failure.is_none()
            && s.last_decision
                .is_none_or(|t| age(now, t).is_some_and(|d| d >= COOLDOWN))
        {
            let target = if let Some(cause) = s.pending_loss.take().and_then(|c| c.evidence(now)) {
                s.d.good_windows = 0;
                Some(((acknowledged_bps / 2).max(MIN_BPS), "receiver_loss", cause))
            } else if s.d.good_windows >= 3 {
                s.d.good_windows = 0;
                s.latest_window.and_then(|c| c.evidence(now)).map(|cause| {
                    (
                        (acknowledged_bps + acknowledged_bps / 10).min(MAX_BPS),
                        "receiver_healthy_probe",
                        cause,
                    )
                })
            } else {
                None
            };
            if let Some((target_bps, reason, cause)) =
                target.filter(|(t, _, _)| *t != acknowledged_bps)
            {
                s.last_decision = Some(now);
                update = Some(RateUpdate {
                    target_bps,
                    reason,
                    observation: cause.observation,
                    cause,
                });
            }
        }
        Poll {
            update,
            diagnostics: s.d.clone(),
            failure: s.failure,
        }
    }
}
impl Protocol<TaggedPacket, TaggedPacket, ()> for ReceiverLoss {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;
    fn handle_read(&mut self, msg: TaggedPacket) -> Result<(), Error> {
        if self.read.len() >= QUEUE {
            return Err(Error::ErrBufferFull);
        }
        if let Packet::Rtcp(packets) = &msg.message.packet {
            let mut s = self
                .state
                .lock()
                .map_err(|_| Error::Other("receiver loss state poisoned".into()))?;
            if packets.len() <= 64 {
                // Multiple matching RR packets in one compound message are ambiguous.
                let matching: Vec<_> = packets
                    .iter()
                    .filter_map(|p| p.as_any().downcast_ref::<ReceiverReport>())
                    .filter(|r| r.reports.len() > 31 || r.reports.iter().any(|b| b.ssrc == s.ssrc))
                    .collect();
                if matching.len() == 1 {
                    s.receiver_report(msg.now, matching[0]);
                } else if matching.len() > 1 {
                    s.reject("ambiguous_compound_rr");
                } else if packets.iter().any(|p| p.as_any().is::<ReceiverReport>()) {
                    s.reject("unrelated_media_ssrc");
                }
            } else {
                s.reject("compound_too_large");
            }
        }
        self.read.push_back(msg);
        Ok(())
    }
    fn poll_read(&mut self) -> Option<TaggedPacket> {
        self.read.pop_front()
    }
    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Error> {
        if self.write.len() >= QUEUE {
            return Err(Error::ErrBufferFull);
        }
        let mut s = self
            .state
            .lock()
            .map_err(|_| Error::Other("receiver loss state poisoned".into()))?;
        if let Packet::Rtp(packet) = &msg.message.packet {
            if packet.header.ssrc == s.ssrc
                && s.rtp(
                    msg.now,
                    packet.header.sequence_number,
                    msg.message.has(&Attribute::Retransmission),
                )
            {
                return Ok(());
            }
        }
        self.write.push_back(msg);
        // This means successfully admitted to this interceptor's bounded output,
        // not proof the network driver sent the datagram.
        let msg = self.write.back().unwrap();
        if let Packet::Rtcp(packets) = &msg.message.packet {
            if packets.len() <= 64 {
                for p in packets {
                    if let Some(sr) = p.as_any().downcast_ref::<SenderReport>() {
                        s.sender_report(msg.now, sr);
                    }
                }
            }
        }
        Ok(())
    }
    fn poll_write(&mut self) -> Option<TaggedPacket> {
        self.write.pop_front()
    }
}
impl Interceptor for ReceiverLoss {
    fn bind_local_stream(&mut self, info: &StreamInfo) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if info.ssrc != 0 && info.ssrc == s.ssrc && !s.retired {
            s.bound = true;
        }
    }
    fn unbind_local_stream(&mut self, info: &StreamInfo) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if info.ssrc == s.ssrc {
            s.bound = false;
            s.retired = true;
            s.reset_windows();
            s.pending_loss = None;
            s.sr.clear();
            self.read.clear();
            self.write.clear();
        }
    }
    fn bind_remote_stream(&mut self, _: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _: &StreamInfo) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc::interceptor::{AttributedPacket, Registry, Slot};
    use rtc::rtcp::{
        payload_feedbacks::picture_loss_indication::PictureLossIndication,
        reception_report::ReceptionReport,
    };
    const SSRC: u32 = 12345678;
    fn info() -> StreamInfo {
        StreamInfo {
            ssrc: SSRC,
            ..Default::default()
        }
    }
    fn packet(now: Instant, p: Packet) -> TaggedPacket {
        TaggedPacket {
            now,
            transport: Default::default(),
            message: AttributedPacket::new(p),
        }
    }
    fn rtp(now: Instant, seq: u16) -> TaggedPacket {
        packet(
            now,
            Packet::Rtp(rtc::rtp::Packet {
                header: rtc::rtp::header::Header {
                    ssrc: SSRC,
                    sequence_number: seq,
                    ..Default::default()
                },
                payload: vec![1; 16].into(),
            }),
        )
    }
    fn sr(now: Instant, lsr: u32) -> TaggedPacket {
        packet(
            now,
            Packet::Rtcp(vec![Box::new(SenderReport {
                ssrc: SSRC,
                ntp_time: (lsr as u64) << 16,
                ..Default::default()
            })]),
        )
    }
    fn rr(now: Instant, high: u32, lost: i32, lsr: u32) -> TaggedPacket {
        packet(
            now,
            Packet::Rtcp(vec![Box::new(ReceiverReport {
                ssrc: 99,
                reports: vec![ReceptionReport {
                    ssrc: SSRC,
                    last_sequence_number: high,
                    total_lost: (lost as u32) & 0xff_ffff,
                    last_sender_report: lsr,
                    ..Default::default()
                }],
                ..Default::default()
            })]),
        )
    }
    struct Fixture {
        observer: ReceiverLoss,
        feedback: LossFeedback,
        now: Instant,
        seq: u32,
        lsr: u32,
    }
    impl Fixture {
        fn new() -> Self {
            let (mut observer, feedback) = ReceiverLoss::new(SSRC, false);
            observer.bind_local_stream(&info());
            Self {
                observer,
                feedback,
                now: Instant::now(),
                seq: 100,
                lsr: 1,
            }
        }
        fn send(&mut self, n: u32) {
            for _ in 0..n {
                self.observer
                    .handle_write(rtp(self.now, self.seq as u16))
                    .unwrap();
                assert!(self.observer.poll_write().is_some());
                self.seq += 1;
            }
        }
        fn report(&mut self, lost: i32) {
            self.observer.handle_write(sr(self.now, self.lsr)).unwrap();
            self.observer.poll_write().unwrap();
            self.now += Duration::from_millis(100);
            self.observer
                .handle_read(rr(self.now, self.seq - 1, lost, self.lsr))
                .unwrap();
            self.observer.poll_read().unwrap();
            self.lsr += 1;
        }
        fn window(&mut self, lost: i32) {
            self.now += Duration::from_secs(1);
            self.send(128);
            self.report(lost);
        }
        fn poll(&self, target: u64) -> Poll {
            self.feedback.poll(self.now, target)
        }
    }
    #[test]
    fn loss_halves_only_acknowledged_target_and_three_healthy_windows_probe() {
        let mut f = Fixture::new();
        f.send(1);
        f.report(0);
        assert!(f.poll(INITIAL_BPS).update.is_none());
        f.window(13);
        let decision = f.poll(INITIAL_BPS).update.unwrap();
        assert_eq!(decision.target_bps, 1_000_000);
        f.window(13);
        assert!(f.poll(1_000_000).update.is_none());
        f.window(13);
        assert!(f.poll(1_000_000).update.is_none());
        f.window(13);
        assert_eq!(f.poll(1_000_000).update.unwrap().target_bps, 1_100_000);
        assert!(f.poll(1_100_000).update.is_none());
    }
    #[test]
    fn latest_loss_survives_consumer_ack_wait_and_overwrites_recovery() {
        let mut f = Fixture::new();
        f.send(1);
        f.report(0);
        for _ in 0..3 {
            f.window(0);
        }
        f.window(20); // No poll during the preceding feedback burst.
        let p = f.poll(1_000_000);
        assert_eq!(p.update.unwrap().target_bps, 500_000);
        assert_eq!(p.diagnostics.good_windows, 0);
        f.window(40);
        assert!(f.poll(500_000).update.is_none()); // cooldown
        f.now += Duration::from_secs(1);
        assert_eq!(f.poll(500_000).update.unwrap().target_bps, MIN_BPS);
    }
    #[test]
    fn floor_and_ceiling_do_not_create_fake_updates() {
        let mut f = Fixture::new();
        f.send(1);
        f.report(0);
        f.window(20);
        assert!(f.poll(MIN_BPS).update.is_none());
        for _ in 0..3 {
            f.window(20);
        }
        assert!(f.poll(MAX_BPS).update.is_none());
    }
    #[test]
    fn signed_loss_correction_cannot_be_zero_loss_recovery() {
        assert_eq!(signed24(0xff_ffff), Some(-1));
        assert_eq!(signed24(0x80_0001), Some(-8_388_607));
        assert_eq!(signed24(0x7f_ffff), None);
        assert_eq!(signed24(0x100_0000), None);
        let mut f = Fixture::new();
        f.send(1);
        f.report(0);
        f.window(0);
        f.window(0);
        f.window(-1);
        assert_eq!(f.poll(1_000_000).diagnostics.good_windows, 0);
        f.window(-1);
        f.window(-1);
        assert!(f.poll(1_000_000).update.is_none());
        f.window(-1);
        assert_eq!(f.poll(1_000_000).update.unwrap().target_bps, 1_100_000);
    }
    #[test]
    fn retransmission_is_not_new_original_or_recovery_activity() {
        let mut f = Fixture::new();
        f.send(1);
        f.report(0);
        for _ in 0..200 {
            let mut p = rtp(f.now, 100);
            p.message.add(Attribute::Retransmission);
            f.observer.handle_write(p).unwrap();
            f.observer.poll_write().unwrap();
        }
        let p = f.poll(1_000_000);
        assert_eq!(p.diagnostics.original_packets, 1);
        assert_eq!(p.diagnostics.retransmissions, 200);
        assert!(p.update.is_none());
        f.send(127);
        f.report(0);
        assert_eq!(f.poll(1_000_000).diagnostics.eligible_windows, 0);
    }
    #[test]
    fn no_feedback_stale_feedback_and_small_windows_never_recover() {
        let mut f = Fixture::new();
        f.send(1);
        assert_eq!(f.poll(1_000_000).diagnostics.status, "feedback_unavailable");
        f.report(0);
        f.window(0);
        f.window(0);
        f.window(0);
        f.now += Duration::from_secs(11);
        let p = f.poll(1_000_000);
        assert!(p.update.is_none());
        assert!(p.failure.is_none());
        assert_eq!(p.diagnostics.good_windows, 0);
        f.send(1);
        f.report(0);
        f.send(1);
        f.report(0);
        assert!(f.poll(1_000_000).update.is_none());
    }
    #[test]
    fn duplicates_backward_future_and_wrong_ssrc_do_not_refresh() {
        let mut f = Fixture::new();
        f.send(1);
        f.report(0);
        let at = f.now;
        f.now += Duration::from_secs(2);
        for high in [100, 99, 101] {
            f.observer.handle_read(rr(f.now, high, 0, 1)).unwrap();
            f.observer.poll_read().unwrap();
        }
        let mut wrong = rr(f.now, 100, 0, 1);
        if let Packet::Rtcp(ref mut packets) = wrong.message.packet {
            packets[0] = Box::new(ReceiverReport {
                ssrc: 99,
                reports: vec![ReceptionReport {
                    ssrc: SSRC + 1,
                    ..Default::default()
                }],
                ..Default::default()
            });
        }
        f.observer.handle_read(wrong).unwrap();
        f.observer.poll_read().unwrap();
        assert_eq!(f.poll(1_000_000).diagnostics.observation, 1);
        assert_eq!(
            f.poll(1_000_000).diagnostics.last_report_age_ms,
            Some(f.now.duration_since(at).as_millis() as u64)
        );
    }
    #[test]
    fn invalid_dlsr_unknown_lsr_and_stale_lsr_rejected_and_map_bounded() {
        let mut f = Fixture::new();
        f.send(1);
        f.observer.handle_read(rr(f.now, 100, 0, 999)).unwrap();
        f.observer.poll_read();
        for lsr in 1..=30 {
            f.observer.handle_write(sr(f.now, lsr)).unwrap();
            f.observer.poll_write();
        }
        assert_eq!(f.feedback.0.lock().unwrap().sr.len(), 16);
        let mut report = ReceiverReport {
            ssrc: 99,
            reports: vec![ReceptionReport {
                ssrc: SSRC,
                last_sequence_number: 100,
                last_sender_report: 30,
                delay: 65_536,
                ..Default::default()
            }],
            ..Default::default()
        };
        f.observer
            .handle_read(packet(f.now, Packet::Rtcp(vec![Box::new(report.clone())])))
            .unwrap();
        f.observer.poll_read();
        f.now += Duration::from_secs(11);
        report.reports[0].delay = 0;
        f.observer
            .handle_read(packet(f.now, Packet::Rtcp(vec![Box::new(report)])))
            .unwrap();
        f.observer.poll_read();
        assert_eq!(f.poll(1_000_000).diagnostics.observation, 0);
        assert_eq!(f.poll(1_000_000).diagnostics.rejected_reports, 3);
    }
    #[test]
    fn rtp_wrap_and_report_extended_wrap_are_incarnation_bounded() {
        let mut f = Fixture::new();
        f.seq = 65_530;
        f.send(1);
        f.report(0);
        f.window(20);
        assert_eq!(f.poll(INITIAL_BPS).update.unwrap().target_bps, 1_000_000);
        // Seed a long-lived counter at the u32 boundary; no billion-packet loop.
        let mut s = f.feedback.0.lock().unwrap();
        s.first = Some(10);
        s.high = (1_u64 << 32) + 10;
        assert_eq!(s.report_high(5), Some((1_u64 << 32) + 5));
        assert_eq!(s.report_high(u32::MAX - 2), Some((1_u64 << 32) - 3));
        assert_eq!(s.report_high(11), None);
    }
    #[test]
    fn unbind_permanently_retires_pending_update_and_other_peer_isolated() {
        let mut f = Fixture::new();
        f.send(1);
        f.report(0);
        f.window(20);
        let g = Fixture::new();
        assert_eq!(g.poll(INITIAL_BPS).diagnostics.observation, 0);
        f.observer.unbind_local_stream(&info());
        f.observer.bind_local_stream(&info());
        assert!(f.poll(INITIAL_BPS).update.is_none());
        assert_eq!(f.poll(INITIAL_BPS).diagnostics.status, "retired");
    }
    #[test]
    fn fault_is_finite_rtp_only_and_retransmission_preserves_drop_selection() {
        let (mut o, fb) = ReceiverLoss::new(SSRC, true);
        o.bind_local_stream(&info());
        let start = Instant::now();
        o.handle_write(rtp(start, 100)).unwrap();
        assert!(o.poll_write().is_some());
        let active = start + Duration::from_secs(5);
        for seq in 101..=120 {
            o.handle_write(rtp(active, seq)).unwrap();
            assert_eq!(o.poll_write().is_none(), seq == 110 || seq == 120);
        }
        let mut retransmit = rtp(active, 110);
        retransmit.message.add(Attribute::Retransmission);
        o.handle_write(retransmit).unwrap();
        assert!(o.poll_write().is_none());
        o.handle_write(sr(active, 1)).unwrap();
        assert!(o.poll_write().is_some());
        let end = start + Duration::from_secs(13);
        o.handle_write(rtp(end, 120)).unwrap();
        assert!(o.poll_write().is_some());
        let p = fb.poll(end, INITIAL_BPS);
        assert_eq!(p.diagnostics.injected_drops, 3);
        assert_eq!(p.diagnostics.fault_state, "complete");
    }
    #[test]
    fn fault_packet_budget_and_queue_budget_fail_explicitly() {
        let (mut o, fb) = ReceiverLoss::new(SSRC, true);
        o.bind_local_stream(&info());
        let start = Instant::now();
        o.handle_write(rtp(start, 1)).unwrap();
        o.poll_write();
        for n in 2..=FAULT_PACKETS + 3 {
            o.handle_write(rtp(start + Duration::from_secs(5), n as u16))
                .unwrap();
            o.poll_write();
        }
        assert_eq!(
            fb.poll(start + Duration::from_secs(6), INITIAL_BPS).failure,
            Some("rtp_impairment_budget_exhausted")
        );
        for n in 0..QUEUE {
            o.handle_write(sr(start, n as u32 + 1)).unwrap();
        }
        assert!(o.handle_write(sr(start, 99)).is_err());
    }
    #[test]
    fn real_default_chain_reports_survive_until_observer_but_not_application_filter() {
        use rtc::peer_connection::configuration::{
            interceptor_registry::register_default_interceptors, media_engine::MediaEngine,
        };
        let (observer, fb) = ReceiverLoss::new(SSRC, false);
        let mut engine = MediaEngine::default();
        let mut chain = register_default_interceptors(Registry::new(), &mut engine)
            .unwrap()
            .with(Slot::from(3500), observer)
            .with(
                Slot::from(14000),
                crate::rtcp_forwarder::RtcpForwarder::new(SSRC),
            )
            .build();
        chain.bind_local_stream(&info());
        let start = Instant::now();
        chain.handle_write(rtp(start, 100)).unwrap();
        while chain.poll_write().is_some() {}
        chain.handle_write(sr(start, 1)).unwrap();
        while chain.poll_write().is_some() {}
        chain
            .handle_read(rr(start + Duration::from_millis(100), 100, 0, 1))
            .unwrap();
        assert!(chain.poll_read().is_none());
        assert_eq!(
            fb.poll(start + Duration::from_millis(100), INITIAL_BPS)
                .diagnostics
                .observation,
            1
        );
        for seq in 101..=228 {
            chain
                .handle_write(rtp(start + Duration::from_secs(1), seq))
                .unwrap();
            while chain.poll_write().is_some() {}
        }
        let mut report = rr(start + Duration::from_secs(2), 228, 20, 1);
        if let Packet::Rtcp(ref mut packets) = report.message.packet {
            packets.push(Box::new(PictureLossIndication {
                sender_ssrc: 99,
                media_ssrc: SSRC,
            }));
        }
        chain.handle_read(report).unwrap();
        let delivered = chain.poll_read().unwrap();
        let Packet::Rtcp(packets) = delivered.message.packet else {
            panic!("RTCP expected")
        };
        assert!(crate::rtcp_forwarder::requests_keyframe(&packets, SSRC));
        assert_eq!(packets.len(), 1);
        assert_eq!(
            fb.poll(start + Duration::from_secs(2), INITIAL_BPS)
                .update
                .unwrap()
                .target_bps,
            1_000_000
        );
    }
    #[test]
    fn small_reports_aggregate_cumulative_loss_not_last_fraction() {
        let mut f = Fixture::new();
        f.send(1);
        f.report(0);
        // Four receiver reports, each only 32 new positions. One policy window.
        for lost in [4, 8, 12, 16] {
            f.now += Duration::from_millis(500);
            f.send(32);
            f.report(lost);
        }
        let p = f.poll(INITIAL_BPS);
        assert_eq!(p.diagnostics.delta_expected, 128);
        assert_eq!(p.diagnostics.delta_lost, 16);
        assert_eq!(p.diagnostics.eligible_windows, 1);
        assert_eq!(p.update.unwrap().target_bps, 1_000_000);
    }
    #[test]
    fn new_reporter_and_duplicate_compound_do_not_reuse_good_history() {
        let mut f = Fixture::new();
        f.send(1);
        f.report(0);
        f.window(0);
        f.window(0);
        f.send(128);
        let mut changed = rr(f.now, f.seq - 1, 0, f.lsr - 1);
        if let Packet::Rtcp(ref mut packets) = changed.message.packet {
            let mut r = packets[0]
                .as_any()
                .downcast_ref::<ReceiverReport>()
                .unwrap()
                .clone();
            r.ssrc = 777;
            packets[0] = Box::new(r);
        }
        f.observer.handle_read(changed).unwrap();
        f.observer.poll_read();
        let p = f.poll(1_000_000);
        assert!(p.update.is_none());
        assert_eq!(p.diagnostics.good_windows, 0);
        assert_eq!(
            p.diagnostics.last_rejection_reason,
            Some("reporter_changed")
        );
        let mut duplicate = rr(f.now, f.seq - 1, 0, f.lsr - 1);
        if let Packet::Rtcp(ref mut packets) = duplicate.message.packet {
            packets.push(packets[0].clone());
        }
        let before = f.poll(1_000_000).diagnostics.observation;
        f.observer.handle_read(duplicate).unwrap();
        f.observer.poll_read();
        assert_eq!(f.poll(1_000_000).diagnostics.observation, before);
        assert_eq!(
            f.poll(1_000_000).diagnostics.last_rejection_reason,
            Some("ambiguous_compound_rr")
        );
    }
    #[test]
    fn queue_refusal_does_not_record_sender_report_and_repeated_lsr_does_not_refresh() {
        let mut f = Fixture::new();
        f.send(1);
        for n in 1..=QUEUE {
            f.observer.handle_write(sr(f.now, n as u32)).unwrap();
        }
        assert!(f.observer.handle_write(sr(f.now, 99)).is_err());
        while f.observer.poll_write().is_some() {}
        assert!(!f.feedback.0.lock().unwrap().sr.iter().any(|s| s.lsr == 99));
        f.now += Duration::from_secs(9);
        f.observer.handle_write(sr(f.now, 32)).unwrap();
        f.observer.poll_write();
        f.now += Duration::from_secs(2);
        f.observer.handle_read(rr(f.now, 100, 0, 32)).unwrap();
        f.observer.poll_read();
        assert_eq!(f.poll(INITIAL_BPS).diagnostics.observation, 0);
        assert_eq!(
            f.poll(INITIAL_BPS).diagnostics.last_rejection_reason,
            Some("stale_lsr")
        );
    }
    #[test]
    fn accepted_loss_cause_survives_invalid_and_corrected_reports_until_consumed() {
        let mut f = Fixture::new();
        f.send(1);
        f.report(0);
        f.window(20);
        let bad_observation = f.poll_metadata().observation;
        // An impossible report must not erase the already accepted loss.
        f.observer
            .handle_read(rr(f.now, f.seq + 1, 20, f.lsr - 1))
            .unwrap();
        f.observer.poll_read();
        f.window(10); // late correction clears healthy history, not accepted loss.
        for _ in 0..3 {
            f.window(10);
        }
        let p = f.poll(INITIAL_BPS);
        let update = p.update.unwrap();
        assert_eq!(update.reason, "receiver_loss");
        assert_eq!(update.target_bps, 1_000_000);
        assert_eq!(update.observation, bad_observation);
        assert_eq!(update.cause.delta_expected, 128);
        assert_eq!(update.cause.delta_lost, 20);
        assert_eq!(update.cause.local_packet_delta, 128);
        assert_eq!(update.cause.good_windows, 0);
        assert_eq!(p.diagnostics.delta_lost, 0); // latest diagnostic is not the decision cause.
    }
    #[test]
    fn recovery_cause_preserves_three_windows_before_consumption() {
        let mut f = Fixture::new();
        f.send(1);
        f.report(0);
        for _ in 0..3 {
            f.window(0);
        }
        let p = f.poll(1_000_000);
        let cause = p.update.unwrap().cause;
        assert_eq!(cause.good_windows, 3);
        assert_eq!(cause.delta_lost, 0);
        assert_eq!(p.diagnostics.good_windows, 0);
    }
    impl Fixture {
        fn poll_metadata(&self) -> Diagnostics {
            self.feedback.0.lock().unwrap().d.clone()
        }
    }
    #[test]
    fn latest_older_sr_cannot_expire_a_newer_pending_loss_cause() {
        let mut f = Fixture::new();
        let start = f.now;
        f.send(1);
        f.report(0); // SR1 @0
        f.now = start + Duration::from_secs(8);
        f.send(128);
        f.report(20); // SR2 @8, fresh bad cause
        f.now = start + Duration::from_secs(9);
        f.send(128);
        f.observer.handle_read(rr(f.now, f.seq - 1, 20, 1)).unwrap();
        f.observer.poll_read();
        f.now = start + Duration::from_millis(10_100); // latest SR1 stale, bad SR2 still fresh
        let p = f.poll(INITIAL_BPS);
        assert_eq!(p.diagnostics.status, "feedback_unavailable");
        let update = p.update.unwrap();
        assert_eq!(update.reason, "receiver_loss");
        assert_eq!(update.cause.sr_age_ms, 2100);
        assert_eq!(update.cause.delta_lost, 20);
    }
}
