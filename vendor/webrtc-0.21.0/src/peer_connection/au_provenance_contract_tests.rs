//! Resource/identity contracts; real transport and send-boundary tests are separate.
use super::*;
use std::time::Duration;

fn binding() -> Binding {
    Binding {
        primary: (7, 108),
        rtx: Some((8, 109)),
    }
}

fn proof(budget: &Arc<Budget>, timestamp: u32) -> Arc<Proof> {
    Proof::new(
        budget.clone(),
        binding(),
        timestamp,
        Instant::now() + Duration::from_millis(250),
    )
    .unwrap()
}

fn key(timestamp: u32) -> WireKey {
    WireKey {
        identity: Identity {
            ssrc: 7,
            payload: 108,
            timestamp,
        },
        sequence: 41,
        marker: true,
    }
}

#[test]
fn proof_capacity_rejection_preserves_accounting_until_actual_last_owner_drops() {
    let budget = Arc::new(Budget::default());
    let mut retained: Vec<_> = (0..PROOFS)
        .map(|timestamp| proof(&budget, timestamp as u32))
        .collect();
    let rejected = Proof::new(budget.clone(), binding(), 9000, Instant::now());
    assert!(matches!(rejected, Err("au_proof_capacity")));
    let full = budget.snapshot();
    assert_eq!(full.live_proofs, PROOFS);
    assert_eq!(full.proof_high_water, PROOFS);
    assert_eq!(full.proofs_created, PROOFS as u64);
    assert_eq!(full.proofs_released, 0);

    let last = retained.pop().unwrap();
    let alias = last.clone();
    drop(last);
    assert_eq!(budget.snapshot().live_proofs, PROOFS);
    drop(alias);
    assert_eq!(budget.snapshot().live_proofs, PROOFS - 1);
    retained.push(proof(&budget, 9000));
    drop(retained);
    let final_state = budget.snapshot();
    assert_eq!(final_state.live_proofs, 0);
    assert_eq!(final_state.proofs_created, (PROOFS + 1) as u64);
    assert_eq!(final_state.proofs_created, final_state.proofs_released);
    assert_eq!(final_state.proof_high_water, PROOFS);
}

#[test]
fn attempt_count_capacity_is_independent_from_shared_proof_and_aliases() {
    let budget = Arc::new(Budget::default());
    let original = proof(&budget, 31);
    let mut attempts: Vec<_> = (0..ATTEMPTS)
        .map(|_| Attempt::new(original.clone(), key(31), 12).unwrap())
        .collect();
    assert!(matches!(
        Attempt::new(original.clone(), key(31), 12),
        Err("au_attempt_capacity")
    ));
    let full = budget.snapshot();
    assert_eq!(full.live_proofs, 1);
    assert_eq!(full.live_attempts, ATTEMPTS);
    assert_eq!(full.attempt_high_water, ATTEMPTS);
    assert_eq!(full.attempt_bytes, ATTEMPTS * 12);
    assert_eq!(full.attempts_created, ATTEMPTS as u64);

    let alias = attempts[0].clone();
    drop(attempts.remove(0));
    assert_eq!(budget.snapshot().live_attempts, ATTEMPTS);
    drop(alias);
    attempts.push(Attempt::new(original.clone(), key(31), 12).unwrap());
    drop(original);
    assert_eq!(budget.snapshot().live_proofs, 1);
    drop(attempts);
    let final_state = budget.snapshot();
    assert_eq!(final_state.live_attempts, 0);
    assert_eq!(final_state.attempt_bytes, 0);
    assert_eq!(final_state.live_proofs, 0);
    assert_eq!(final_state.attempts_created, (ATTEMPTS + 1) as u64);
    assert_eq!(final_state.attempts_created, final_state.attempts_released);
    assert_eq!(final_state.proofs_created, final_state.proofs_released);
}

#[test]
fn attempt_byte_budget_rejects_before_count_capacity_and_recovers_on_drop() {
    let budget = Arc::new(Budget::default());
    let original = proof(&budget, 32);
    let first_bytes = 200 * 1024;
    let first = Attempt::new(original.clone(), key(32), first_bytes).unwrap();
    assert!(matches!(
        Attempt::new(original.clone(), key(32), first_bytes),
        Err("au_attempt_capacity")
    ));
    assert_eq!(budget.snapshot().attempts_created, 1);
    assert_eq!(budget.snapshot().attempt_bytes, first_bytes);
    let second = Attempt::new(original.clone(), key(32), ATTEMPT_BYTES - first_bytes).unwrap();
    assert!(matches!(
        Attempt::new(original.clone(), key(32), 1),
        Err("au_attempt_capacity")
    ));
    assert_eq!(budget.snapshot().live_attempts, 2);
    assert_eq!(budget.snapshot().attempt_bytes_high_water, ATTEMPT_BYTES);
    drop(first);
    let replacement = Attempt::new(original.clone(), key(32), first_bytes).unwrap();
    drop(second);
    drop(replacement);
    let final_state = budget.snapshot();
    assert_eq!(final_state.attempt_bytes, 0);
    assert_eq!(final_state.live_attempts, 0);
    assert_eq!(final_state.attempts_created, 3);
    assert_eq!(final_state.attempts_released, 3);
    assert_eq!(final_state.attempt_high_water, 2);
}

#[test]
fn malformed_attempt_reservations_do_not_consume_capacity() {
    let budget = Arc::new(Budget::default());
    let original = proof(&budget, 33);
    let mut wrong_stream = key(33);
    wrong_stream.identity.ssrc = 99;
    for (candidate, bytes) in [
        (key(33), 0),
        (key(33), 256 * 1024 + 1),
        (key(34), 12),
        (wrong_stream, 12),
    ] {
        assert!(matches!(
            Attempt::new(original.clone(), candidate, bytes),
            Err("au_attempt_binding")
        ));
    }
    let state = budget.snapshot();
    assert_eq!(state.live_attempts, 0);
    assert_eq!(state.attempt_bytes, 0);
    assert_eq!(state.attempts_created, 0);
    assert_eq!(state.attempts_released, 0);
}

#[test]
fn exact_peer_and_wire_binding_cannot_be_reused_after_submission() {
    let budget = Arc::new(Budget::default());
    let other_peer = Arc::new(Budget::default());
    let original = proof(&budget, 34);
    let attempt = Attempt::new(original.clone(), key(34), 24).unwrap();
    assert_eq!(attempt.validate(&budget, key(34)), Ok(original.deadline));
    assert_eq!(
        attempt.validate(&other_peer, key(34)),
        Err("au_attempt_binding")
    );
    let mut variants = [key(34); 5];
    variants[0].sequence += 1;
    variants[1].marker = false;
    variants[2].identity.timestamp += 1;
    variants[3].identity.ssrc = 8;
    variants[4].identity.payload = 109;
    for candidate in variants {
        assert_eq!(
            attempt.validate(&budget, candidate),
            Err("au_attempt_binding")
        );
    }
    let context = attempt.clone().context();
    assert_eq!(
        Attempt::from_context(&context)
            .unwrap()
            .validate(&budget, key(34)),
        Ok(original.deadline)
    );
    assert!(attempt.mark_submitted());
    assert!(!attempt.mark_submitted());
    assert_eq!(
        Attempt::from_context(&context)
            .unwrap()
            .validate(&budget, key(34)),
        Err("au_attempt_reused")
    );
    let unknown = LocalPacketContext(Arc::new(17_u32));
    assert!(Attempt::from_context(&unknown).is_none());
}

#[test]
fn rtx_attempt_gets_original_deadline_and_survives_original_owner_drop() {
    let budget = Arc::new(Budget::default());
    // A past instant is intentional: the proof stores identity, while the final
    // send gate enforces expiry. Repair creation must never refresh that instant.
    let deadline = Instant::now() - Duration::from_millis(1);
    let original = Proof::new(budget.clone(), binding(), 35, deadline).unwrap();
    let first = Attempt::new(original.clone(), key(35), 24).unwrap();
    let mut rtx_key = key(35);
    rtx_key.identity.ssrc = 8;
    rtx_key.identity.payload = 109;
    rtx_key.sequence = 500;
    let repair = Attempt::new(original.clone(), rtx_key, 26).unwrap();
    assert!(first.mark_submitted());
    drop(original);
    drop(first);
    assert_eq!(budget.snapshot().live_proofs, 1);
    assert_eq!(repair.validate(&budget, rtx_key), Ok(deadline));
    assert_eq!(repair.validate(&budget, key(35)), Err("au_attempt_binding"));
    drop(repair);
    let final_state = budget.snapshot();
    assert_eq!(final_state.live_proofs, 0);
    assert_eq!(final_state.live_attempts, 0);
    assert_eq!(final_state.attempt_bytes, 0);
    assert_eq!(final_state.proofs_created, final_state.proofs_released);
    assert_eq!(final_state.attempts_created, final_state.attempts_released);
}
