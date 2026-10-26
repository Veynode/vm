//! Per-hop latency bookkeeping. Each hop is measured on one clock: host
//! times come in the video header, gateway residence in the route header,
//! browser times on the browser. Network legs are half the measured round
//! trip of the ping channel, which is an estimate and shown as one.
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Hop {
    /// Capture (PTS) to encoded access unit, host clock.
    Encode,
    /// Waiting in the host pacer.
    HostQueue,
    /// Host to gateway, half the tunnel RTT.
    Tunnel,
    /// Inside the gateway.
    Gateway,
    /// Gateway to browser, half the browser RTT minus the tunnel half.
    Edge,
    /// Received to decoded, browser clock.
    Decode,
    /// Decoded to painted.
    Paint,
    /// Key press to the first frame showing its effect, end to end.
    KeyToPaint,
}

/// Running p50/p95 over the last `N` samples of one hop.
#[derive(Clone, Debug)]
pub struct Window<const N: usize> {
    samples: Vec<u32>,
    next: usize,
}

impl<const N: usize> Default for Window<N> {
    fn default() -> Self {
        Self {
            samples: Vec::with_capacity(N),
            next: 0,
        }
    }
}

impl<const N: usize> Window<N> {
    pub fn push(&mut self, micros: u32) {
        if self.samples.len() < N {
            self.samples.push(micros);
        } else {
            self.samples[self.next] = micros;
        }
        self.next = (self.next + 1) % N;
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Nearest-rank percentile in microseconds.
    pub fn percentile(&self, p: f64) -> Option<u32> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
        Some(sorted[rank.min(sorted.len()) - 1])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub hop: Hop,
    pub p50_us: u32,
    pub p95_us: u32,
    pub samples: usize,
}

pub fn summary<const N: usize>(hop: Hop, w: &Window<N>) -> Option<Summary> {
    Some(Summary {
        hop,
        p50_us: w.percentile(50.0)?,
        p95_us: w.percentile(95.0)?,
        samples: w.len(),
    })
}

/// Clock offset and round trip from one ping: `t0` sent and `t3` received
/// on the local clock, `t_peer` stamped by the peer. Offset is peer minus
/// local, assuming a symmetric path (NTP's estimate with one peer stamp).
pub fn offset_and_rtt(t0_us: u64, t_peer_us: u64, t3_us: u64) -> (i64, u64) {
    let rtt = t3_us.saturating_sub(t0_us);
    let offset = t_peer_us as i64 - (t0_us + rtt / 2) as i64;
    (offset, rtt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_over_a_ring() {
        let mut w = Window::<10>::default();
        assert_eq!(w.percentile(50.0), None);
        for v in 1..=20 {
            w.push(v * 1000);
        }
        assert_eq!(w.len(), 10);
        assert_eq!(w.percentile(50.0), Some(15_000));
        assert_eq!(w.percentile(95.0), Some(20_000));
        let s = summary(Hop::KeyToPaint, &w).unwrap();
        assert_eq!(s.samples, 10);
    }

    #[test]
    fn ping_offset_assumes_symmetric_path() {
        assert_eq!(offset_and_rtt(1000, 6000, 3000), (4000, 2000));
    }
}
