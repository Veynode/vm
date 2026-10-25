//! Host-local ownership evidence. Counts are not receiver delivery or total RSS.
use serde_json::{json, Value};
use std::time::Instant;
use webrtc::peer_connection::media_egress::{provenance_caps, ProvenanceSnapshot};

fn value(s: &ProvenanceSnapshot) -> Value {
    let (proofs, attempts, attempt_bytes) = provenance_caps();
    json!({
        "live_proofs":s.live_proofs,"proof_high_water":s.proof_high_water,
        "proofs_created":s.proofs_created,"proofs_released":s.proofs_released,
        "live_attempts":s.live_attempts,"attempt_high_water":s.attempt_high_water,
        "attempts_created":s.attempts_created,"attempts_released":s.attempts_released,
        "attempt_bytes":s.attempt_bytes,"attempt_bytes_high_water":s.attempt_bytes_high_water,
        "caps":{"proofs":proofs,"attempts":attempts,"attempt_bytes":attempt_bytes}
    })
}

pub fn log(fence_id: &str, phase: &str, started: Instant, snapshot: ProvenanceSnapshot) {
    eprintln!(
        "{}",
        json!({"version":1,"event":"au_provenance_snapshot","fence_id":fence_id,
            "phase":phase,"elapsed_ms":started.elapsed().as_millis(),"counters":value(&snapshot)})
    );
}

pub fn cleanup_attested(s: &ProvenanceSnapshot) -> bool {
    let (proofs, attempts, attempt_bytes) = provenance_caps();
    s.live_proofs == 0
        && s.live_attempts == 0
        && s.attempt_bytes == 0
        && s.proofs_created == s.proofs_released
        && s.attempts_created == s.attempts_released
        && s.proof_high_water <= proofs
        && s.attempt_high_water <= attempts
        && s.attempt_bytes_high_water <= attempt_bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn released() -> ProvenanceSnapshot {
        ProvenanceSnapshot {
            proofs_created: 4,
            proofs_released: 4,
            proof_high_water: 3,
            attempts_created: 12,
            attempts_released: 12,
            attempt_high_water: 5,
            attempt_bytes_high_water: 6000,
            ..Default::default()
        }
    }

    #[test]
    fn release_receipt_requires_actual_zero_and_balanced_lifetimes() {
        assert!(cleanup_attested(&released()));
        // A setup failure may close without ever enabling deadlines or sending RTP.
        assert!(cleanup_attested(&ProvenanceSnapshot::default()));
        for mutate in [
            |s: &mut ProvenanceSnapshot| s.live_proofs = 1,
            |s: &mut ProvenanceSnapshot| s.live_attempts = 1,
            |s: &mut ProvenanceSnapshot| s.attempt_bytes = 1,
            |s: &mut ProvenanceSnapshot| s.proofs_released -= 1,
            |s: &mut ProvenanceSnapshot| s.attempts_released -= 1,
            |s: &mut ProvenanceSnapshot| s.proofs_released += 1,
            |s: &mut ProvenanceSnapshot| s.attempts_released += 1,
        ] {
            let mut s = released();
            mutate(&mut s);
            assert!(!cleanup_attested(&s));
        }
    }

    #[test]
    fn zero_final_counters_cannot_hide_capacity_overrun() {
        let (proofs, attempts, bytes) = provenance_caps();
        let mut s = released();
        s.proof_high_water = proofs;
        s.attempt_high_water = attempts;
        s.attempt_bytes_high_water = bytes;
        assert!(cleanup_attested(&s));
        for index in 0..3 {
            let mut over = s;
            match index {
                0 => over.proof_high_water += 1,
                1 => over.attempt_high_water += 1,
                _ => over.attempt_bytes_high_water += 1,
            }
            assert!(!cleanup_attested(&over));
        }
        let data = value(&s);
        assert_eq!(data["caps"]["proofs"], proofs);
        assert_eq!(data["caps"]["attempts"], attempts);
        assert_eq!(data["caps"]["attempt_bytes"], bytes);
    }
}
