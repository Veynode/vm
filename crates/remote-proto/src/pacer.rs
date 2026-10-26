//! Per-viewer send queue that never lets latency build up: one slot for the
//! newest frame and one for a keyframe. A delta that has to be overwritten
//! breaks the decoder's reference chain, so the viewer then waits for the
//! next keyframe instead of receiving deltas it cannot decode.
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub keyframe: bool,
    pub enqueued: Instant,
    pub bytes: Vec<u8>,
}

/// What the caller should do after offering a frame.
#[derive(Debug, PartialEq, Eq)]
pub enum Offer {
    Queued,
    /// The frame was dropped and this viewer needs a keyframe.
    DroppedNeedKeyframe,
    /// Dropped while already waiting for a keyframe.
    Dropped,
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Stats {
    pub sent: u64,
    pub overwritten: u64,
    pub dropped_waiting: u64,
}

#[derive(Default)]
pub struct Pacer {
    key: Option<Frame>,
    latest: Option<Frame>,
    need_key: bool,
    stats: Stats,
}

impl Pacer {
    /// A new viewer starts waiting for a keyframe.
    pub fn new() -> Self {
        Self {
            need_key: true,
            ..Default::default()
        }
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn need_keyframe(&self) -> bool {
        self.need_key
    }

    /// The transport lost a delta (a reset stream, a failed write).
    pub fn lost(&mut self) {
        self.need_key = true;
        self.latest = None;
    }

    pub fn offer(&mut self, frame: Frame) -> Offer {
        if frame.keyframe {
            self.stats.overwritten +=
                self.latest.take().is_some() as u64 + self.key.is_some() as u64;
            self.key = Some(frame);
            self.need_key = false;
            return Offer::Queued;
        }
        if self.need_key {
            self.stats.dropped_waiting += 1;
            return Offer::Dropped;
        }
        if self.latest.take().is_some() {
            // The transport is behind: the unsent delta and this one, which
            // depends on it, both go. A queued keyframe is still sent.
            self.stats.overwritten += 2;
            self.need_key = true;
            return Offer::DroppedNeedKeyframe;
        }
        self.latest = Some(frame);
        Offer::Queued
    }

    /// The next frame to send, keyframe first.
    pub fn pop(&mut self) -> Option<Frame> {
        let f = self.key.take().or_else(|| self.latest.take());
        self.stats.sent += f.is_some() as u64;
        f
    }

    /// Age of the oldest queued frame.
    pub fn queued_for(&self, now: Instant) -> Option<Duration> {
        self.key
            .as_ref()
            .or(self.latest.as_ref())
            .map(|f| now.saturating_duration_since(f.enqueued))
    }
}

/// Encoder keyframe requests for one VM: at most one per interval and a
/// bounded number per session, as `media-local`'s RTCP forwarder does.
pub struct KeyframeLimiter {
    interval: Duration,
    budget: u32,
    last: Option<Instant>,
    pending: bool,
}

pub const KEYFRAME_INTERVAL: Duration = Duration::from_millis(500);
pub const KEYFRAME_BUDGET: u32 = 512;

impl Default for KeyframeLimiter {
    fn default() -> Self {
        Self {
            interval: KEYFRAME_INTERVAL,
            budget: KEYFRAME_BUDGET,
            last: None,
            pending: false,
        }
    }
}

impl KeyframeLimiter {
    /// Records a request; true when the encoder should be asked now. A
    /// request inside the interval stays pending for `poll`.
    pub fn request(&mut self, now: Instant) -> bool {
        self.pending = true;
        self.poll(now)
    }

    pub fn poll(&mut self, now: Instant) -> bool {
        if !self.pending || self.budget == 0 {
            return false;
        }
        if self
            .last
            .is_some_and(|l| now.duration_since(l) < self.interval)
        {
            return false;
        }
        self.pending = false;
        self.budget -= 1;
        self.last = Some(now);
        true
    }

    /// A keyframe arrived for another reason; it satisfies pending requests.
    pub fn satisfied(&mut self) {
        self.pending = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(keyframe: bool, t: Instant) -> Frame {
        Frame {
            keyframe,
            enqueued: t,
            bytes: vec![keyframe as u8],
        }
    }

    #[test]
    fn new_viewer_waits_for_keyframe() {
        let t = Instant::now();
        let mut p = Pacer::new();
        assert_eq!(p.offer(f(false, t)), Offer::Dropped);
        assert_eq!(p.offer(f(true, t)), Offer::Queued);
        assert_eq!(p.offer(f(false, t)), Offer::Queued);
        assert!(p.pop().unwrap().keyframe);
        assert!(!p.pop().unwrap().keyframe);
        assert!(p.pop().is_none());
    }

    #[test]
    fn overwriting_a_delta_requires_a_keyframe() {
        let t = Instant::now();
        let mut p = Pacer::new();
        p.offer(f(true, t));
        p.pop();
        assert_eq!(p.offer(f(false, t)), Offer::Queued);
        assert_eq!(p.offer(f(false, t)), Offer::DroppedNeedKeyframe);
        assert!(p.pop().is_none());
        assert_eq!(p.offer(f(false, t)), Offer::Dropped);
        p.offer(f(true, t));
        assert!(p.pop().unwrap().keyframe);
        assert_eq!(p.stats().overwritten, 2);
    }

    #[test]
    fn keyframe_replaces_everything_queued() {
        let t = Instant::now();
        let mut p = Pacer::new();
        p.offer(f(true, t));
        p.offer(f(false, t));
        p.offer(f(true, t + Duration::from_millis(16)));
        assert_eq!(
            p.queued_for(t + Duration::from_millis(20)),
            Some(Duration::from_millis(4))
        );
        assert!(p.pop().unwrap().keyframe);
        assert!(p.pop().is_none());
    }

    #[test]
    fn transport_loss_waits_for_keyframe() {
        let t = Instant::now();
        let mut p = Pacer::new();
        p.offer(f(true, t));
        p.pop();
        p.lost();
        assert!(p.need_keyframe());
        assert_eq!(p.offer(f(false, t)), Offer::Dropped);
    }

    #[test]
    fn keyframe_limiter_spaces_and_bounds_requests() {
        let t = Instant::now();
        let mut k = KeyframeLimiter {
            budget: 2,
            ..Default::default()
        };
        assert!(k.request(t));
        assert!(!k.request(t + Duration::from_millis(100)));
        assert!(k.poll(t + Duration::from_millis(500)));
        assert!(!k.request(t + Duration::from_secs(5)), "budget spent");
        let mut k = KeyframeLimiter::default();
        k.request(t);
        k.request(t + Duration::from_millis(1));
        k.satisfied();
        assert!(!k.poll(t + Duration::from_secs(1)));
    }
}
