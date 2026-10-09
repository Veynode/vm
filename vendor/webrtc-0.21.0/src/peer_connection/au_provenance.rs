//! Immutable local AU ownership. Logical-attempt budgets are not total RTC RSS.
use super::au_deadline::{Binding, Identity};
use rtc::{peer_connection::message::LocalPacketContext, rtp::packet::Packet};
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

pub(crate) const PROOFS: usize = 2048;
pub(crate) const ATTEMPTS: usize = 384;
pub(crate) const ATTEMPT_BYTES: usize = 384 * 1024;
/// Actual live logical owners and monotone allocation/release accounting.
/// Arc aliases share one logical reservation; these are not buffer-copy/RSS gauges.
#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    /// Unique live immutable AU proofs, including retired but referenced AUs.
    pub live_proofs: usize,
    /// Peak unique proofs.
    pub proof_high_water: usize,
    /// Successfully created proofs.
    pub proofs_created: u64,
    /// Proofs destroyed on their last actual Arc drop.
    pub proofs_released: u64,
    /// Unique live logical transmission attempts.
    pub live_attempts: usize,
    /// Peak logical attempts.
    pub attempt_high_water: usize,
    /// Successfully reserved logical attempts.
    pub attempts_created: u64,
    /// Attempts destroyed on their last actual Arc drop.
    pub attempts_released: u64,
    /// Original plaintext RTP wire bytes reserved by live logical attempts.
    pub attempt_bytes: usize,
    /// Peak reserved plaintext wire bytes.
    pub attempt_bytes_high_water: usize,
}
#[derive(Default)]
pub(crate) struct Budget(Mutex<Snapshot>);
impl Budget {
    pub fn snapshot(&self) -> Snapshot {
        *self.0.lock().unwrap()
    }
}
pub(crate) struct Proof {
    budget: Arc<Budget>,
    pub binding: Binding,
    pub timestamp: u32,
    pub deadline: Instant,
}
impl Proof {
    pub fn new(
        budget: Arc<Budget>,
        binding: Binding,
        timestamp: u32,
        deadline: Instant,
    ) -> Result<Arc<Self>, &'static str> {
        {
            let mut s = budget.0.lock().unwrap();
            if s.live_proofs == PROOFS {
                return Err("au_proof_capacity");
            }
            s.live_proofs += 1;
            s.proofs_created = s.proofs_created.saturating_add(1);
            s.proof_high_water = s.proof_high_water.max(s.live_proofs);
        }
        Ok(Arc::new(Self {
            budget,
            binding,
            timestamp,
            deadline,
        }))
    }
    pub fn issued_by(&self, budget: &Arc<Budget>) -> bool {
        Arc::ptr_eq(&self.budget, budget)
    }
}
impl Drop for Proof {
    fn drop(&mut self) {
        let mut s = self.budget.0.lock().unwrap();
        s.live_proofs -= 1;
        s.proofs_released = s.proofs_released.saturating_add(1);
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WireKey {
    pub identity: Identity,
    pub sequence: u16,
    pub marker: bool,
}
impl WireKey {
    pub fn from_packet(p: &Packet) -> Self {
        Self {
            identity: Identity {
                ssrc: p.header.ssrc,
                payload: p.header.payload_type,
                timestamp: p.header.timestamp,
            },
            sequence: p.header.sequence_number,
            marker: p.header.marker,
        }
    }
}
pub(crate) struct Attempt {
    pub proof: Arc<Proof>,
    pub key: WireKey,
    bytes: usize,
    submitted: std::sync::atomic::AtomicBool,
}
impl Attempt {
    pub fn new(proof: Arc<Proof>, key: WireKey, bytes: usize) -> Result<Arc<Self>, &'static str> {
        if key.identity.timestamp != proof.timestamp
            || !proof.binding.accepts(key.identity)
            || bytes == 0
            || bytes > 256 * 1024
        {
            return Err("au_attempt_binding");
        }
        {
            let mut s = proof.budget.0.lock().unwrap();
            if s.live_attempts == ATTEMPTS
                || s.attempt_bytes
                    .checked_add(bytes)
                    .is_none_or(|n| n > ATTEMPT_BYTES)
            {
                return Err("au_attempt_capacity");
            }
            s.live_attempts += 1;
            s.attempt_bytes += bytes;
            s.attempts_created = s.attempts_created.saturating_add(1);
            s.attempt_high_water = s.attempt_high_water.max(s.live_attempts);
            s.attempt_bytes_high_water = s.attempt_bytes_high_water.max(s.attempt_bytes);
        }
        Ok(Arc::new(Self {
            proof,
            key,
            bytes,
            submitted: std::sync::atomic::AtomicBool::new(false),
        }))
    }
    pub fn context(self: Arc<Self>) -> LocalPacketContext {
        LocalPacketContext(self)
    }
    pub fn from_context(c: &LocalPacketContext) -> Option<&Self> {
        c.0.downcast_ref::<Self>()
    }
    pub fn validate(&self, budget: &Arc<Budget>, key: WireKey) -> Result<Instant, &'static str> {
        if !self.proof.issued_by(budget) || key != self.key {
            return Err("au_attempt_binding");
        }
        if self.submitted.load(std::sync::atomic::Ordering::Acquire) {
            return Err("au_attempt_reused");
        }
        Ok(self.proof.deadline)
    }
    pub fn mark_submitted(&self) -> bool {
        !self
            .submitted
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        let mut s = self.proof.budget.0.lock().unwrap();
        s.live_attempts -= 1;
        s.attempt_bytes -= self.bytes;
        s.attempts_released = s.attempts_released.saturating_add(1);
    }
}

#[cfg(test)]
#[path = "au_provenance_contract_tests.rs"]
mod contract_tests;
