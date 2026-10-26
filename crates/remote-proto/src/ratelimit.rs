//! Token buckets for the gateway and host: new connections per IP,
//! handshakes per device, control frames per route. Keys are bounded so a
//! flood of addresses cannot grow memory without limit.
use std::collections::HashMap;
use std::hash::Hash;
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
pub struct Bucket {
    tokens: f64,
    at: Instant,
}

#[derive(Clone, Copy, Debug)]
pub struct Rate {
    /// Tokens a full bucket holds.
    pub burst: f64,
    /// Tokens added a second.
    pub per_second: f64,
}

impl Rate {
    pub fn per_minute(n: u32) -> Self {
        Self {
            burst: f64::from(n),
            per_second: f64::from(n) / 60.0,
        }
    }
    /// veynode/core's relay socket limit: burst 300, 150 a second.
    pub const RELAY_FRAMES: Self = Self {
        burst: 300.0,
        per_second: 150.0,
    };
}

impl Bucket {
    pub fn full(rate: Rate, now: Instant) -> Self {
        Self {
            tokens: rate.burst,
            at: now,
        }
    }

    pub fn take(&mut self, rate: Rate, cost: f64, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * rate.per_second).min(rate.burst);
        self.at = now;
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }
}

/// Buckets by key, forgetting refilled keys when the map is full.
pub struct Keyed<K> {
    rate: Rate,
    max_keys: usize,
    buckets: HashMap<K, Bucket>,
}

impl<K: Eq + Hash + Clone> Keyed<K> {
    pub fn new(rate: Rate, max_keys: usize) -> Self {
        Self {
            rate,
            max_keys: max_keys.max(1),
            buckets: HashMap::new(),
        }
    }

    pub fn allow(&mut self, key: &K, now: Instant) -> bool {
        if !self.buckets.contains_key(key) && self.buckets.len() >= self.max_keys {
            let rate = self.rate;
            self.buckets.retain(|_, b| {
                let mut b = *b;
                b.take(rate, 0.0, now);
                b.tokens < rate.burst
            });
            if self.buckets.len() >= self.max_keys {
                // Still full of active keys: refuse rather than evict them.
                return false;
            }
        }
        let rate = self.rate;
        self.buckets
            .entry(key.clone())
            .or_insert_with(|| Bucket::full(rate, now))
            .take(rate, 1.0, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn bucket_bursts_then_refills() {
        let t = Instant::now();
        let r = Rate {
            burst: 3.0,
            per_second: 1.0,
        };
        let mut b = Bucket::full(r, t);
        assert!((0..3).all(|_| b.take(r, 1.0, t)));
        assert!(!b.take(r, 1.0, t));
        assert!(b.take(r, 1.0, t + Duration::from_secs(1)));
    }

    #[test]
    fn keyed_limits_per_key_and_bounds_memory() {
        let t = Instant::now();
        let mut k = Keyed::new(Rate::per_minute(2), 2);
        assert!(k.allow(&"a", t) && k.allow(&"a", t));
        assert!(!k.allow(&"a", t));
        assert!(k.allow(&"b", t));
        assert!(!k.allow(&"c", t), "map full of active keys");
        assert!(
            k.allow(&"c", t + Duration::from_secs(120)),
            "refilled keys forgotten"
        );
    }
}
