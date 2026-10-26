//! Optional WebTransport datagram mode: one access unit split into
//! fragments that each fit a datagram after sealing. The fragment header
//! travels inside the sealed record, so the gateway cannot read or reorder
//! it meaningfully. Lost fragments are not retransmitted: a frame that is
//! still incomplete at its deadline is dropped together with every delta
//! after it, until a keyframe.
use anyhow::{ensure, Result};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Plaintext bytes per datagram: header plus data, leaving room for the
/// 9-byte record header, 16-byte tag and QUIC/WebTransport overhead under
/// a 1200-byte path MTU.
pub const MAX_FRAGMENT: usize = 1100;
pub const HEADER_LEN: usize = 12;
pub const MAX_DATA: usize = MAX_FRAGMENT - HEADER_LEN;
/// Frames the receiver holds while waiting for missing fragments.
pub const MAX_PENDING: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub epoch: u32,
    pub frame_seq: u32,
    pub index: u16,
    pub count: u16,
}

/// Fragments of one record payload (a `VIDEO_V2` payload, header included).
pub fn split(epoch: u32, frame_seq: u32, payload: &[u8]) -> Result<Vec<Vec<u8>>> {
    ensure!(!payload.is_empty(), "empty frame");
    let count = payload.len().div_ceil(MAX_DATA);
    ensure!(
        count <= usize::from(u16::MAX),
        "frame too large for datagrams"
    );
    Ok(payload
        .chunks(MAX_DATA)
        .enumerate()
        .map(|(i, chunk)| {
            let mut out = Vec::with_capacity(HEADER_LEN + chunk.len());
            out.extend(epoch.to_be_bytes());
            out.extend(frame_seq.to_be_bytes());
            out.extend((i as u16).to_be_bytes());
            out.extend((count as u16).to_be_bytes());
            out.extend(chunk);
            out
        })
        .collect())
}

pub fn parse(fragment: &[u8]) -> Result<(Header, &[u8])> {
    ensure!(
        fragment.len() > HEADER_LEN && fragment.len() <= MAX_FRAGMENT,
        "bad fragment size"
    );
    let h = Header {
        epoch: u32::from_be_bytes(fragment[..4].try_into()?),
        frame_seq: u32::from_be_bytes(fragment[4..8].try_into()?),
        index: u16::from_be_bytes([fragment[8], fragment[9]]),
        count: u16::from_be_bytes([fragment[10], fragment[11]]),
    };
    ensure!(h.count > 0 && h.index < h.count, "bad fragment index");
    Ok((h, &fragment[HEADER_LEN..]))
}

struct Partial {
    count: u16,
    parts: Vec<Option<Vec<u8>>>,
    have: u16,
    first: Instant,
}

/// What the receiver should do after a fragment or a deadline sweep.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// A whole frame payload, in frame order.
    Frame { frame_seq: u32, payload: Vec<u8> },
    /// Frames were lost; deltas are dropped until a keyframe arrives.
    NeedKeyframe { lost: u32 },
}

/// Receiver side reassembly. `keyframe` tells whether a completed payload is
/// a keyframe, so the decoder chain can restart after a loss.
pub struct Reassembler {
    epoch: u32,
    next: u32,
    pending: BTreeMap<u32, Partial>,
    need_key: bool,
    deadline: Duration,
}

impl Reassembler {
    pub fn new(deadline: Duration) -> Self {
        Self {
            epoch: 0,
            next: 0,
            pending: BTreeMap::new(),
            need_key: true,
            deadline,
        }
    }

    pub fn need_keyframe(&self) -> bool {
        self.need_key
    }

    pub fn push(
        &mut self,
        fragment: &[u8],
        now: Instant,
        keyframe: impl Fn(&[u8]) -> bool,
    ) -> Result<Vec<Event>> {
        let (h, data) = parse(fragment)?;
        if h.epoch != self.epoch {
            if h.epoch < self.epoch {
                return Ok(vec![]);
            }
            self.epoch = h.epoch;
            self.pending.clear();
            self.next = 0;
            self.need_key = true;
        }
        if h.frame_seq < self.next {
            return Ok(vec![]);
        }
        let partial = self.pending.entry(h.frame_seq).or_insert_with(|| Partial {
            count: h.count,
            parts: vec![None; usize::from(h.count)],
            have: 0,
            first: now,
        });
        ensure!(
            partial.count == h.count,
            "fragment count changed within a frame"
        );
        let slot = &mut partial.parts[usize::from(h.index)];
        if slot.is_none() {
            *slot = Some(data.to_vec());
            partial.have += 1;
        }
        let mut events = self.sweep(now);
        events.extend(self.drain(&keyframe));
        while self.pending.len() > MAX_PENDING {
            let (&oldest, _) = self.pending.iter().next().unwrap();
            self.pending.remove(&oldest);
            self.skip_to(oldest + 1, &mut events);
        }
        Ok(events)
    }

    /// Drops frames older than the deadline; call on a timer too.
    pub fn sweep(&mut self, now: Instant) -> Vec<Event> {
        let mut events = vec![];
        let expired: Vec<u32> = self
            .pending
            .iter()
            .filter(|(_, p)| p.have < p.count && now.duration_since(p.first) >= self.deadline)
            .map(|(&s, _)| s)
            .collect();
        for seq in expired {
            self.pending.remove(&seq);
            self.skip_to(seq + 1, &mut events);
        }
        events
    }

    fn skip_to(&mut self, next: u32, events: &mut Vec<Event>) {
        if next > self.next {
            let lost = next - self.next;
            self.next = next;
            if !self.need_key {
                self.need_key = true;
            }
            events.push(Event::NeedKeyframe { lost });
        }
    }

    fn drain(&mut self, keyframe: &impl Fn(&[u8]) -> bool) -> Vec<Event> {
        let mut events = vec![];
        loop {
            // A complete keyframe ahead of a gap restarts the chain there.
            let ready = self.pending.iter().find(|(&s, p)| {
                p.have == p.count
                    && (s == self.next || keyframe(p.parts[0].as_deref().unwrap_or(&[])))
            });
            let Some((&seq, _)) = ready else { break };
            let p = self.pending.remove(&seq).unwrap();
            let payload: Vec<u8> = p.parts.into_iter().flatten().flatten().collect();
            let key = keyframe(&payload);
            if seq != self.next {
                self.pending.retain(|&s, _| s > seq);
            }
            self.next = seq + 1;
            if key {
                self.need_key = false;
            }
            if !self.need_key {
                events.push(Event::Frame {
                    frame_seq: seq,
                    payload,
                });
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test payloads: first byte 1 marks a keyframe.
    fn key(p: &[u8]) -> bool {
        p.first() == Some(&1)
    }
    fn frame(k: bool, len: usize) -> Vec<u8> {
        let mut v = vec![7u8; len];
        v[0] = k as u8;
        v
    }

    #[test]
    fn split_and_parse_bounds() {
        let parts = split(1, 2, &frame(true, 3000)).unwrap();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.len() <= MAX_FRAGMENT));
        let (h, _) = parse(&parts[2]).unwrap();
        assert_eq!(
            h,
            Header {
                epoch: 1,
                frame_seq: 2,
                index: 2,
                count: 3
            }
        );
        let mut bad = parts[0].clone();
        bad[9] = 9;
        assert!(parse(&bad).is_err());
    }

    #[test]
    fn out_of_order_fragments_reassemble_in_frame_order() {
        let now = Instant::now();
        let mut r = Reassembler::new(Duration::from_millis(100));
        let f0 = split(0, 0, &frame(true, 2500)).unwrap();
        let f1 = split(0, 1, &frame(false, 1500)).unwrap();
        let mut out = vec![];
        for p in [&f1[1], &f0[2], &f1[0], &f0[0], &f0[1]] {
            out.extend(r.push(p, now, key).unwrap());
        }
        let seqs: Vec<_> = out
            .iter()
            .map(|e| match e {
                Event::Frame { frame_seq, payload } => (*frame_seq, payload.len()),
                e => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(seqs, vec![(0, 2500), (1, 1500)]);
        assert!(!r.need_keyframe());
    }

    #[test]
    fn deadline_loss_drops_deltas_until_keyframe() {
        let t0 = Instant::now();
        let mut r = Reassembler::new(Duration::from_millis(50));
        r.push(&split(0, 0, &frame(true, 10)).unwrap()[0], t0, key)
            .unwrap();
        let lost = split(0, 1, &frame(false, 2000)).unwrap();
        r.push(&lost[0], t0, key).unwrap();
        let later = t0 + Duration::from_millis(60);
        let ev = r
            .push(&split(0, 2, &frame(false, 10)).unwrap()[0], later, key)
            .unwrap();
        assert_eq!(ev, vec![Event::NeedKeyframe { lost: 1 }]);
        assert!(r.need_keyframe());
        let ev = r
            .push(&split(0, 3, &frame(true, 10)).unwrap()[0], later, key)
            .unwrap();
        assert!(matches!(ev[..], [Event::Frame { frame_seq: 3, .. }]));
    }

    #[test]
    fn complete_keyframe_skips_a_stuck_gap_and_new_epoch_resets() {
        let t0 = Instant::now();
        let mut r = Reassembler::new(Duration::from_secs(1));
        r.push(&split(0, 0, &frame(true, 10)).unwrap()[0], t0, key)
            .unwrap();
        r.push(&split(0, 1, &frame(false, 2000)).unwrap()[0], t0, key)
            .unwrap();
        let ev = r
            .push(&split(0, 2, &frame(true, 10)).unwrap()[0], t0, key)
            .unwrap();
        assert!(matches!(ev[..], [Event::Frame { frame_seq: 2, .. }]));
        let ev = r
            .push(&split(1, 0, &frame(false, 10)).unwrap()[0], t0, key)
            .unwrap();
        assert!(ev.is_empty(), "a delta cannot start a new epoch");
        assert!(r
            .push(&split(0, 3, &frame(true, 10)).unwrap()[0], t0, key)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn pending_is_bounded() {
        let t0 = Instant::now();
        let mut r = Reassembler::new(Duration::from_secs(1));
        r.push(&split(0, 0, &frame(true, 10)).unwrap()[0], t0, key)
            .unwrap();
        for s in 1..10 {
            r.push(&split(0, s, &frame(false, 2000)).unwrap()[0], t0, key)
                .unwrap();
        }
        assert!(r.pending.len() <= MAX_PENDING);
    }
}
