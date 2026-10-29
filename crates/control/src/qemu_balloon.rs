//! Windows Guests on QEMU: memory Windows does not need goes back to macOS.
//!
//! virtio-win's balloon driver offers no free page reporting (Linux Guests use that instead), so the worker
//! inflates the balloon over what Windows has available beyond a reserve, a step at a time, and deflates it
//! completely the moment Windows comes under pressure. The QEMU loader returns inflated pages to macOS
//! (`native/tools/qemu-ram-reclaim.h`). The figures come from the balloon's statistics, which virtio-win's
//! balloon service (`blnsvr.exe`) sends; without fresh statistics the balloon stays (or goes back) fully
//! deflated.
//!
//! Available memory alone is no measure of pressure: Windows keeps it up by trimming working sets to its page
//! file. Pressure is therefore also any real writing to the page file (except right after an inflation, which
//! makes Windows write some), and nothing is inflated until the page file has been quiet for a while.
use serde_json::Value;
use std::time::{Duration, Instant};

/// The balloon device (`qemu_uefi::BALLOON_DEVICE`).
pub const DEVICE_PATH: &str = "/machine/peripheral/veynode-balloon";
/// How often the Guest sends statistics and the policy looks at them. The balloon service spends ~50 ms of CPU a
/// minute at this rate.
pub const INTERVAL: Duration = Duration::from_secs(1);
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
/// The most one step inflates, and how often, so Windows gives up memory gradually.
const STEP: u64 = GIB;
const STEP_EVERY: Duration = Duration::from_secs(5);
/// Smaller changes are not worth a request.
const SLACK: u64 = 64 * MIB;
/// Spare memory above the reserve below which nothing is inflated.
const HYSTERESIS: u64 = 256 * MIB;
/// More than this written to the page file between two statistics is pressure.
const PAGING: u64 = 32 * MIB;
/// Writing this soon after an inflation is Windows making room for the balloon (measured: 36–61 MiB within a few
/// seconds of a 1 GiB step), not pressure; it still holds off the next step.
const MAKING_ROOM: Duration = Duration::from_secs(10);
/// Nothing is inflated until the page file has been quiet this long.
const QUIET: Duration = Duration::from_secs(30);
/// After a full deflation, nothing is inflated again for this long.
const HOLD: Duration = Duration::from_secs(60);
/// A target the Guest has not reached by then is given up on.
const SETTLE: Duration = Duration::from_secs(30);
/// Running this long without new statistics gives the Guest everything back.
const STALE: Duration = Duration::from_secs(15);

/// One set of the Guest's statistics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// Bytes Windows has available: free, zeroed and standby pages.
    pub available: u64,
    /// Bytes written to the page file since boot, if reported.
    pub paged_out: Option<u64>,
    /// When QEMU received them, in seconds since the epoch.
    pub updated: u64,
}

/// `None` while the Guest has sent none: QEMU reports unavailable values as -1.
pub fn guest_stats(value: &Value) -> Option<Stats> {
    let stat = |name: &str| value["stats"][name].as_u64().filter(|&v| v != u64::MAX);
    Some(Stats {
        available: stat("stat-available-memory")?,
        paged_out: stat("stat-swap-out"),
        updated: value["last-update"].as_u64().filter(|&v| v > 0)?,
    })
}

pub struct Policy {
    ram: u64,
    /// Available memory Windows keeps; falling under half of it deflates the balloon.
    reserve: u64,
    /// The smallest the Guest is ever made.
    floor: u64,
    next: Instant,
    last: Option<Stats>,
    /// The last time statistics arrived (or the policy started or resumed).
    fresh_at: Instant,
    /// The last time the page file was written to (or the policy started).
    paged_at: Instant,
    requested: Option<(u64, Instant)>,
    inflated_at: Option<Instant>,
    hold_until: Option<Instant>,
}

impl Policy {
    pub fn new(ram: u64, now: Instant) -> Self {
        Self {
            ram,
            reserve: (ram / 8).clamp(512 * MIB, 2 * GIB),
            floor: (ram / 4).max(2 * GIB).min(ram),
            next: now,
            last: None,
            fresh_at: now,
            paged_at: now,
            requested: None,
            inflated_at: None,
            hold_until: None,
        }
    }

    pub fn due(&self, now: Instant) -> bool {
        now >= self.next
    }

    /// One step: the balloon's current size (`actual`, the memory the Guest has) and the Guest's statistics, if
    /// they could be read. Returns a new target size for QEMU's `balloon` command, if one is due.
    pub fn decide(
        &mut self,
        now: Instant,
        running: bool,
        actual: u64,
        stats: Option<Stats>,
    ) -> Option<u64> {
        self.next = now + INTERVAL;
        if !running {
            // A paused Guest sends nothing; that is not a lost balloon service.
            self.fresh_at = now;
            return None;
        }
        let previous = self.last;
        let Some(stats) = stats.filter(|s| previous.is_none_or(|p| s.updated != p.updated)) else {
            // Its need is unknown: give it everything back.
            let stale = now.duration_since(self.fresh_at) >= STALE;
            return (stale
                && actual < self.ram
                && self.requested.is_none_or(|(t, _)| t != self.ram))
            .then(|| self.request(self.ram, now));
        };
        self.last = Some(stats);
        self.fresh_at = now;
        let paged = match (previous.and_then(|p| p.paged_out), stats.paged_out) {
            (Some(before), Some(after)) => after.saturating_sub(before),
            _ => 0,
        };
        if paged > 0 {
            self.paged_at = now;
        }
        let making_room = self.inflated_at.is_some_and(|at| now < at + MAKING_ROOM);
        if (paged > PAGING && !making_room) || stats.available < self.reserve / 2 {
            self.hold_until = Some(now + HOLD);
            return (actual < self.ram && self.requested.is_none_or(|(t, _)| t != self.ram))
                .then(|| self.request(self.ram, now));
        }
        if self.hold_until.is_some_and(|until| now < until)
            || now.duration_since(self.paged_at) < QUIET
            || self.inflated_at.is_some_and(|at| now < at + STEP_EVERY)
        {
            return None;
        }
        if let Some((target, at)) = self.requested {
            if actual.abs_diff(target) > SLACK && now < at + SETTLE {
                return None;
            }
        }
        let spare = stats.available.saturating_sub(self.reserve);
        if spare < HYSTERESIS {
            return None;
        }
        let target = actual.saturating_sub(spare.min(STEP)).max(self.floor);
        if target + SLACK >= actual {
            return None;
        }
        self.inflated_at = Some(now);
        Some(self.request(target, now))
    }

    fn request(&mut self, target: u64, now: Instant) -> u64 {
        self.requested = Some((target, now));
        target
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RAM: u64 = 8 * GIB;

    fn stats(available: u64, paged_out: u64, updated: u64) -> Option<Stats> {
        Some(Stats {
            available,
            paged_out: Some(paged_out),
            updated,
        })
    }

    /// A policy whose page file has been quiet long enough to inflate.
    fn settled(start: Instant) -> (Policy, Instant) {
        let mut policy = Policy::new(RAM, start);
        assert_eq!(policy.decide(start, true, RAM, stats(6 * GIB, 0, 1)), None);
        (policy, start + QUIET)
    }

    #[test]
    fn statistics_are_read_only_once_the_guest_sends_them() {
        let none = json!({"stats":{"stat-available-memory":u64::MAX,"stat-swap-out":u64::MAX},"last-update":0});
        assert_eq!(guest_stats(&none), None);
        let some = json!({"stats":{"stat-available-memory":6 * GIB,"stat-swap-out":u64::MAX},"last-update":1793229500u64});
        assert_eq!(
            guest_stats(&some),
            Some(Stats {
                available: 6 * GIB,
                paged_out: None,
                updated: 1793229500
            })
        );
    }

    #[test]
    fn inflates_a_step_at_a_time_down_to_the_reserve() {
        let (mut policy, t) = settled(Instant::now());
        // 6 GiB available, 1 GiB reserve: one 1 GiB step.
        assert_eq!(
            policy.decide(t, true, RAM, stats(6 * GIB, 0, 2)),
            Some(7 * GIB)
        );
        assert!(!policy.due(t) && policy.due(t + INTERVAL));
        // At most one step every STEP_EVERY, and only once the Guest got there.
        let t = t + STEP_EVERY;
        assert_eq!(
            policy.decide(t, true, RAM - 100 * MIB, stats(6 * GIB, 0, 3)),
            None
        );
        assert_eq!(
            policy.decide(t, true, 7 * GIB, stats(5 * GIB, 0, 4)),
            Some(6 * GIB)
        );
        // The last step takes only what is spare.
        let t = t + STEP_EVERY;
        assert_eq!(
            policy.decide(t, true, 6 * GIB, stats(GIB + 512 * MIB, 0, 5)),
            Some(5 * GIB + 512 * MIB)
        );
        // Within the hysteresis: done.
        let t = t + STEP_EVERY;
        assert_eq!(
            policy.decide(t, true, 5 * GIB + 512 * MIB, stats(GIB + 100 * MIB, 0, 6)),
            None
        );
    }

    #[test]
    fn never_shrinks_the_guest_below_the_floor() {
        let (mut policy, t) = settled(Instant::now());
        assert_eq!(
            policy.decide(t, true, 2 * GIB + 512 * MIB, stats(4 * GIB, 0, 2)),
            Some(2 * GIB)
        );
        let t = t + STEP_EVERY;
        assert_eq!(policy.decide(t, true, 2 * GIB, stats(4 * GIB, 0, 3)), None);
    }

    #[test]
    fn low_memory_deflates_completely_and_holds_off() {
        let (mut policy, t) = settled(Instant::now());
        assert_eq!(
            policy.decide(t, true, 4 * GIB, stats(400 * MIB, 0, 2)),
            Some(RAM)
        );
        // Asked once, not again while the Guest gets there.
        assert_eq!(
            policy.decide(t, true, 5 * GIB, stats(300 * MIB, 0, 3)),
            None
        );
        // Plenty available again, but within the hold nothing is inflated.
        assert_eq!(
            policy.decide(t + HOLD / 2, true, RAM, stats(6 * GIB, 0, 4)),
            None
        );
        assert_eq!(
            policy.decide(t + HOLD, true, RAM, stats(6 * GIB, 0, 5)),
            Some(7 * GIB)
        );
    }

    #[test]
    fn paging_deflates_completely_even_with_memory_available() {
        let (mut policy, t) = settled(Instant::now());
        assert_eq!(policy.decide(t, true, 3 * GIB, stats(GIB, 0, 2)), None);
        // Windows trimmed working sets to its page file: available looks fine, it is not.
        assert_eq!(
            policy.decide(t, true, 3 * GIB, stats(2 * GIB, 900 * MIB, 3)),
            Some(RAM)
        );
        // Some writing below the threshold is no pressure, but it keeps the policy from inflating.
        let mut quiet = settled(Instant::now());
        let t = quiet.1;
        assert_eq!(
            quiet.0.decide(t, true, RAM, stats(6 * GIB, 8 * MIB, 2)),
            None
        );
        assert_eq!(
            quiet
                .0
                .decide(t + QUIET / 2, true, RAM, stats(6 * GIB, 8 * MIB, 3)),
            None
        );
        assert_eq!(
            quiet
                .0
                .decide(t + QUIET, true, RAM, stats(6 * GIB, 8 * MIB, 4)),
            Some(7 * GIB)
        );
    }

    #[test]
    fn writing_that_makes_room_for_an_inflation_is_no_pressure() {
        let (mut policy, t) = settled(Instant::now());
        assert_eq!(
            policy.decide(t, true, RAM, stats(6 * GIB, 0, 2)),
            Some(7 * GIB)
        );
        // Windows writes some of the page file to give up the step: the balloon stays, no next step.
        let t = t + INTERVAL * 2;
        assert_eq!(
            policy.decide(t, true, 7 * GIB, stats(5 * GIB, 60 * MIB, 3)),
            None
        );
        assert_eq!(
            policy.decide(t + STEP_EVERY, true, 7 * GIB, stats(5 * GIB, 60 * MIB, 4)),
            None
        );
        // The same writing later is pressure.
        assert_eq!(
            policy.decide(t + MAKING_ROOM, true, 7 * GIB, stats(5 * GIB, 120 * MIB, 5)),
            Some(RAM)
        );
    }

    #[test]
    fn stale_statistics_give_the_guest_everything_back_unless_paused() {
        let (mut policy, t) = settled(Instant::now());
        assert_eq!(
            policy.decide(t, true, RAM, stats(6 * GIB, 0, 2)),
            Some(7 * GIB)
        );
        // Paused: no statistics, and no change.
        assert_eq!(policy.decide(t + STALE * 4, false, 7 * GIB, None), None);
        // Running again: the same statistics (or none) until STALE has passed, then deflate once.
        let t = t + STALE * 4;
        assert_eq!(policy.decide(t, true, 7 * GIB, stats(6 * GIB, 0, 2)), None);
        assert_eq!(policy.decide(t + STALE / 2, true, 7 * GIB, None), None);
        assert_eq!(policy.decide(t + STALE, true, 7 * GIB, None), Some(RAM));
        assert_eq!(
            policy.decide(t + STALE * 2, true, 7 * GIB + 100 * MIB, None),
            None
        );
        // A Guest that never sent any is left alone.
        let start = Instant::now();
        let mut quiet = Policy::new(RAM, start);
        assert_eq!(quiet.decide(start + STALE * 2, true, RAM, None), None);
    }

    #[test]
    fn a_target_the_guest_cannot_reach_is_given_up_after_a_while() {
        let (mut policy, t) = settled(Instant::now());
        assert_eq!(
            policy.decide(t, true, RAM, stats(6 * GIB, 0, 2)),
            Some(7 * GIB)
        );
        let stuck = 7 * GIB + 500 * MIB;
        assert_eq!(
            policy.decide(t + STEP_EVERY, true, stuck, stats(5 * GIB, 0, 3)),
            None
        );
        assert_eq!(
            policy.decide(t + SETTLE, true, stuck, stats(5 * GIB, 0, 4)),
            Some(6 * GIB + 500 * MIB)
        );
    }

    #[test]
    fn small_guests_keep_a_proportionate_reserve() {
        let policy = Policy::new(4 * GIB, Instant::now());
        assert_eq!((policy.reserve, policy.floor), (512 * MIB, 2 * GIB));
        let big = Policy::new(64 * GIB, Instant::now());
        assert_eq!((big.reserve, big.floor), (2 * GIB, 16 * GIB));
        let tiny = Policy::new(GIB, Instant::now());
        assert_eq!(tiny.floor, GIB);
    }
}
