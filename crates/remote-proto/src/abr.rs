//! Adaptive bitrate and picture size for one encoder track.
//!
//! The thresholds and the bitrate-to-scale anchors start from veynode/core's
//! RFB relay (`apps/control/src/transport/rfb/adaptive.ts`, same author),
//! whose anchors come from M3 Pro VideoToolbox measurements of a text-heavy
//! desktop: full size at 1.5 Mbps and above, about 0.76 near 1 Mbps, half
//! size at 300 kbps. Signals here are per 500 ms window: RTT and loss from
//! QUIC or the ping channel, the bytes in flight, and the trend of the host
//! queue delay. Bounds match `media-local`'s `rate_control` (250 kbps to
//! 40 Mbps).
use std::time::{Duration, Instant};

pub const MIN_BPS: u64 = 250_000;
pub const MAX_BPS: u64 = 40_000_000;
pub const START_REMOTE_BPS: u64 = 4_000_000;
pub const START_LAN_BPS: u64 = 8_000_000;

/// In flight past this, frames are dropped (the pacer does it).
pub const SCREEN_BACKLOG: u64 = 2 * 1024 * 1024;
/// In flight past this, the bitrate comes down...
pub const SLOW_BACKLOG: u64 = 256 * 1024;
/// ...and goes up again only while less than this is in flight.
pub const CALM_BACKLOG: u64 = 64 * 1024;
/// On a full link, the time one frame may take it.
pub const FRAME_MS: f64 = 40.0;
pub const LEAST_FITTED_KBPS: f64 = 300.0;
/// Smallest share of the source size a picture is shrunk to.
pub const LEAST_SCALE: f64 = 0.4;

pub const WINDOW: Duration = Duration::from_millis(500);
pub const COOLDOWN: Duration = Duration::from_secs(1);
pub const CLEAN_WINDOWS_TO_RISE: u32 = 3;
pub const RISING_WINDOWS_TO_FALL: u32 = 5;
/// A smaller picture is kept this long after the bitrate could afford more.
pub const UPSCALE_HOLD: Duration = Duration::from_secs(10);

const SCALE_ANCHORS: [(f64, f64); 5] = [
    (300.0, 0.5),
    (1000.0, 1094.0 / 1440.0),
    (1500.0, 1.0),
    (2000.0, 1.0),
    (3000.0, 1.0),
];

/// Kilobits a second that keep each of `fps` frames within [`FRAME_MS`] of
/// a link delivering `capacity` bytes a millisecond, and the stream within
/// three quarters of it.
pub fn fitted_kbps(capacity_bytes_per_ms: f64, fps: f64) -> f64 {
    LEAST_FITTED_KBPS.max(
        (capacity_bytes_per_ms * 6.0)
            .min(capacity_bytes_per_ms * FRAME_MS * fps.max(2.0) * 8.0 / 1000.0),
    )
}

/// Share of the source size worth encoding at `kbps`.
pub fn scale_for(kbps: f64) -> f64 {
    let kbps = if kbps.is_finite() {
        kbps.max(0.0)
    } else {
        5000.0
    };
    let Some(i) = SCALE_ANCHORS.iter().position(|(b, _)| kbps <= *b) else {
        return 1.0;
    };
    if i == 0 {
        return SCALE_ANCHORS[0].1.max(LEAST_SCALE);
    }
    let ((b0, s0), (b1, s1)) = (SCALE_ANCHORS[i - 1], SCALE_ANCHORS[i]);
    (s0 + (s1 - s0) * (kbps - b0) / (b1 - b0)).max(LEAST_SCALE)
}

fn even(n: f64) -> u32 {
    ((n / 2.0).floor() as u32 * 2).max(2)
}

/// `size` times `scale`, aspect kept, even, never under 320 x 200 or above `size`.
pub fn shrunk(size: (u32, u32), scale: f64) -> (u32, u32) {
    let fit = scale.max(320.0 / size.0 as f64).max(200.0 / size.1 as f64);
    if fit >= 1.0 {
        return size;
    }
    (even(size.0 as f64 * fit), even(size.1 as f64 * fit))
}

/// Output size for a crop view: no larger than the crop, no larger than the
/// viewer's physical pixels for it, and never more pixels than the whole
/// screen would get at the same bitrate (`budget_pixels`). Zooming in on a
/// phone therefore costs at most what the full screen costs, usually less.
pub fn crop_output(crop: (u32, u32), viewer: (u32, u32), budget_pixels: u64) -> (u32, u32) {
    let (cw, ch) = (crop.0.max(2) as f64, crop.1.max(2) as f64);
    let mut scale: f64 = 1.0;
    if viewer.0 > 0 && viewer.1 > 0 {
        scale = scale.min(viewer.0 as f64 / cw).min(viewer.1 as f64 / ch);
    }
    let pixels = cw * ch * scale * scale;
    if pixels > budget_pixels as f64 {
        scale *= (budget_pixels as f64 / pixels).sqrt();
    }
    let scale = scale.max(64.0 / cw).max(64.0 / ch).min(1.0);
    (
        even(cw * scale).min(crop.0.max(2)),
        even(ch * scale).min(crop.1.max(2)),
    )
}

/// One window of measurements.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    pub rtt_ms: f64,
    /// Lost fraction of packets or frames in the window, 0..1.
    pub loss: f64,
    /// Bytes sent and not yet acknowledged (QUIC) or not yet echoed (ping).
    pub in_flight: u64,
    /// Median host pacer queue delay in the window.
    pub queue_ms: f64,
    pub fps: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Target {
    pub bps: u64,
    /// Share of the source (or crop) size to encode, `LEAST_SCALE..=1`.
    pub scale: f64,
}

pub struct Controller {
    bps: u64,
    scale: f64,
    last_change: Option<Instant>,
    last_upscale_block: Option<Instant>,
    clean: u32,
    rising: u32,
    last_queue: f64,
}

impl Controller {
    pub fn new(start_bps: u64) -> Self {
        let bps = start_bps.clamp(MIN_BPS, MAX_BPS);
        Self {
            bps,
            scale: scale_for(bps as f64 / 1000.0),
            last_change: None,
            last_upscale_block: None,
            clean: 0,
            rising: 0,
            last_queue: 0.0,
        }
    }

    pub fn target(&self) -> Target {
        Target {
            bps: self.bps,
            scale: self.scale,
        }
    }

    /// Feeds one window; returns a new target when it changed.
    pub fn update(&mut self, s: Sample, now: Instant) -> Option<Target> {
        let before = self.target();
        if s.queue_ms > self.last_queue + 1.0 {
            self.rising += 1;
        } else if s.queue_ms + 1.0 < self.last_queue || s.queue_ms < 2.0 {
            self.rising = 0;
        }
        self.last_queue = s.queue_ms;
        let cooled = self
            .last_change
            .is_none_or(|t| now.duration_since(t) >= COOLDOWN);
        let factor = if s.loss >= 0.10 {
            Some(0.5)
        } else if s.loss >= 0.02
            || s.in_flight > SLOW_BACKLOG
            || self.rising >= RISING_WINDOWS_TO_FALL
        {
            Some(0.85)
        } else {
            None
        };
        match factor {
            // Heavy loss acts at once; milder signals respect the cooldown.
            Some(f) if f < 0.6 || cooled => {
                self.bps = ((self.bps as f64 * f) as u64).clamp(MIN_BPS, MAX_BPS);
                self.clean = 0;
                self.rising = 0;
                self.last_change = Some(now);
            }
            Some(_) => {}
            None if s.in_flight < CALM_BACKLOG && s.loss < 0.005 => {
                self.clean += 1;
                if self.clean >= CLEAN_WINDOWS_TO_RISE && cooled {
                    self.bps = ((self.bps as f64 * 1.08) as u64).clamp(MIN_BPS, MAX_BPS);
                    self.clean = 0;
                    self.last_change = Some(now);
                }
            }
            None => self.clean = 0,
        }
        let want = scale_for(self.bps as f64 / 1000.0);
        if want < self.scale - 0.02 {
            self.scale = want;
            self.last_upscale_block = Some(now);
        } else if want > self.scale + 0.02
            && self
                .last_upscale_block
                .is_none_or(|t| now.duration_since(t) >= UPSCALE_HOLD)
        {
            self.scale = want;
        }
        let after = self.target();
        (after != before).then_some(after)
    }
}

/// Picks whose measurements steer a shared encoder track: the person holding
/// the control lease when they watch it, else the median viewer.
pub fn steering_index(samples: &[Sample], lease_holder: Option<usize>) -> Option<usize> {
    if let Some(i) = lease_holder.filter(|&i| i < samples.len()) {
        return Some(i);
    }
    if samples.is_empty() {
        return None;
    }
    let mut order: Vec<usize> = (0..samples.len()).collect();
    order.sort_by(|&a, &b| samples[a].rtt_ms.total_cmp(&samples[b].rtt_ms));
    Some(order[order.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_follows_core_anchors() {
        assert_eq!(scale_for(5000.0), 1.0);
        assert_eq!(scale_for(1500.0), 1.0);
        assert!((scale_for(1000.0) - 1094.0 / 1440.0).abs() < 1e-9);
        assert_eq!(scale_for(300.0), 0.5);
        assert_eq!(scale_for(10.0), 0.5);
        let mid = scale_for(1250.0);
        assert!(mid > 0.76 && mid < 1.0);
    }

    #[test]
    fn fitted_bitrate_has_floor_and_link_share() {
        assert_eq!(fitted_kbps(0.0, 60.0), LEAST_FITTED_KBPS);
        // 1 MB/s link = 1000 bytes/ms: three quarters of it is 6000 kbps.
        assert_eq!(fitted_kbps(1000.0, 60.0), 6000.0);
    }

    #[test]
    fn shrunk_keeps_aspect_and_minimum() {
        assert_eq!(shrunk((1280, 800), 0.5), (640, 400));
        assert_eq!(shrunk((1280, 800), 0.1), (320, 200));
        assert_eq!(shrunk((1280, 800), 1.2), (1280, 800));
    }

    #[test]
    fn crop_output_is_bounded_by_crop_viewer_and_budget() {
        let full = 1920u64 * 1080;
        // A phone zoomed on a quarter of the screen: the crop's own pixels.
        assert_eq!(crop_output((960, 540), (1170, 2532), full), (960, 540));
        // Viewer smaller than the crop: its physical pixels.
        assert_eq!(crop_output((960, 540), (480, 480), full), (480, 270));
        // Budget smaller than the crop.
        let (w, h) = crop_output((1920, 1080), (4000, 4000), full / 4);
        assert!(u64::from(w) * u64::from(h) <= full / 4 && w % 2 == 0 && h % 2 == 0);
    }

    #[test]
    fn heavy_loss_halves_at_once_mild_signals_respect_cooldown() {
        let t = Instant::now();
        let mut c = Controller::new(8_000_000);
        let bad = Sample {
            loss: 0.2,
            ..Default::default()
        };
        assert_eq!(c.update(bad, t).unwrap().bps, 4_000_000);
        assert_eq!(
            c.update(bad, t + Duration::from_millis(500)).unwrap().bps,
            2_000_000
        );
        let mild = Sample {
            loss: 0.03,
            ..Default::default()
        };
        assert!(
            c.update(mild, t + Duration::from_millis(1000)).is_none(),
            "cooldown"
        );
        assert_eq!(
            c.update(mild, t + Duration::from_millis(1600)).unwrap().bps,
            1_700_000
        );
    }

    #[test]
    fn backlog_lowers_and_clean_windows_raise() {
        let t = Instant::now();
        let mut c = Controller::new(4_000_000);
        let full = Sample {
            in_flight: SLOW_BACKLOG + 1,
            ..Default::default()
        };
        assert_eq!(c.update(full, t).unwrap().bps, 3_400_000);
        let clean = Sample {
            in_flight: 1000,
            ..Default::default()
        };
        let mut now = t;
        let mut raised = None;
        for _ in 0..6 {
            now += WINDOW;
            raised = raised.or(c.update(clean, now));
        }
        assert_eq!(raised.unwrap().bps, 3_672_000);
    }

    #[test]
    fn rising_queue_delay_lowers_bitrate() {
        let t = Instant::now();
        let mut c = Controller::new(4_000_000);
        let mut out = None;
        for i in 0..5 {
            let s = Sample {
                queue_ms: 5.0 + 5.0 * i as f64,
                in_flight: 100_000,
                ..Default::default()
            };
            out = out.or(c.update(s, t + WINDOW * i));
        }
        assert_eq!(out.unwrap().bps, 3_400_000);
    }

    #[test]
    fn picture_shrinks_at_once_and_grows_after_hold() {
        let t = Instant::now();
        let mut c = Controller::new(1_000_000);
        let start = c.target().scale;
        let bad = Sample {
            loss: 0.5,
            ..Default::default()
        };
        c.update(bad, t);
        assert!(c.target().scale < start);
        let small = c.target().scale;
        let clean = Sample::default();
        let mut now = t;
        while now < t + Duration::from_secs(9) {
            now += WINDOW;
            c.update(clean, now);
        }
        assert!(c.target().bps > 500_000);
        assert_eq!(c.target().scale, small, "held below 10 s");
        for _ in 0..40 {
            now += WINDOW;
            c.update(clean, now);
        }
        assert!(c.target().scale > small);
    }

    #[test]
    fn steering_prefers_lease_holder_then_median() {
        let s = |rtt| Sample {
            rtt_ms: rtt,
            ..Default::default()
        };
        let v = [s(10.0), s(90.0), s(40.0)];
        assert_eq!(steering_index(&v, Some(1)), Some(1));
        assert_eq!(steering_index(&v, None), Some(2));
        assert_eq!(steering_index(&[], None), None);
    }
}
