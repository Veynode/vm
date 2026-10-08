//! Per-VM arbitration. Epochs fence future commands; they cannot retract an already dispatched guest command.
use anyhow::{bail, Result};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
#[derive(Default)]
pub struct Leases {
    entries: BTreeMap<String, Lease>,
    retired: BTreeMap<String, Instant>,
    retirement_overflow_until: Option<Instant>,
}
struct Lease {
    epoch: u64,
    owner: Option<String>,
    expires: Instant,
    human: bool,
}
const RETIRED_OWNER_TTL: Duration = Duration::from_secs(120);
const RETIRED_OWNER_CAPACITY: usize = 4096;
impl Leases {
    /// Session-end fencing prevents a cancelled in-flight acquire from obtaining
    /// control after cleanup. Controller admission rejects requests older than 30s.
    pub fn retire_owner(&mut self, owner: &str, now: Instant) -> Vec<String> {
        let released = self.release_owner(owner);
        self.retired.retain(|_, until| *until > now);
        if self.retired.len() < RETIRED_OWNER_CAPACITY || self.retired.contains_key(owner) {
            self.retired.insert(owner.into(), now + RETIRED_OWNER_TTL);
        } else {
            // Never evict a still-live fence. Block all new acquisitions for the
            // complete retirement interval instead, while still releasing control.
            self.retirement_overflow_until = Some(now + RETIRED_OWNER_TTL);
        }
        released
    }

    pub fn acquire(&mut self, id: &str, owner: &str, now: Instant) -> Result<u64> {
        if owner.len() != 64 || !owner.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!("invalid control owner token")
        }
        self.retired.retain(|_, until| *until > now);
        if self.retired.contains_key(owner)
            || self
                .retirement_overflow_until
                .is_some_and(|until| until > now)
        {
            bail!("control session ended or owner retirement capacity reached")
        }
        let lease = self.entries.entry(id.into()).or_insert(Lease {
            epoch: 0,
            owner: None,
            expires: now,
            human: false,
        });
        if lease.human {
            bail!("human controls this VM; local human_release is required")
        }
        if lease.expires > now
            && lease
                .owner
                .as_deref()
                .is_some_and(|current| current != owner)
        {
            bail!("another client holds VM control")
        }
        if lease.owner.as_deref() != Some(owner) || lease.expires <= now {
            lease.epoch = lease
                .epoch
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("control epoch exhausted"))?;
        }
        lease.owner = Some(owner.into());
        lease.expires = now + Duration::from_secs(60);
        Ok(lease.epoch)
    }
    pub fn check(&self, id: &str, owner: &str, epoch: u64, now: Instant) -> Result<()> {
        let lease = self
            .entries
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("acquire VM control first"))?;
        if lease.human
            || lease.owner.as_deref() != Some(owner)
            || lease.epoch != epoch
            || lease.expires <= now
        {
            bail!("control lease expired, revoked or belongs to another client")
        }
        Ok(())
    }
    pub fn release(&mut self, id: &str, owner: &str, epoch: u64, now: Instant) -> Result<()> {
        self.check(id, owner, epoch, now)?;
        let lease = self.entries.get_mut(id).unwrap();
        lease.owner = None;
        Ok(())
    }
    pub fn release_owner(&mut self, owner: &str) -> Vec<String> {
        let mut released = Vec::new();
        for (id, lease) in &mut self.entries {
            if lease.owner.as_deref() == Some(owner) {
                lease.owner = None;
                released.push(id.clone());
            }
        }
        released
    }
    pub fn human(&mut self, id: &str, take: bool, now: Instant) -> Result<u64> {
        let lease = self.entries.entry(id.into()).or_insert(Lease {
            epoch: 0,
            owner: None,
            expires: now,
            human: false,
        });
        lease.epoch = lease
            .epoch
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("control epoch exhausted"))?;
        lease.owner = None;
        lease.human = take;
        lease.expires = now;
        Ok(lease.epoch)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn human_takeover_fences_stale_holder() {
        let mut l = Leases::default();
        let now = Instant::now();
        let owner = "a".repeat(64);
        let first = l.acquire("vm", &owner, now).unwrap();
        l.check("vm", &owner, first, now).unwrap();
        l.human("vm", true, now).unwrap();
        assert!(l.check("vm", &owner, first, now).is_err());
        assert!(l.acquire("vm", &owner, now).is_err());
        l.human("vm", false, now).unwrap();
        let second = l.acquire("vm", &owner, now).unwrap();
        assert!(second > first);
        assert!(l.check("vm", &owner, first, now).is_err());
    }
    #[test]
    fn competing_clients_expiry_and_release() {
        let mut l = Leases::default();
        let now = Instant::now();
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let one = l.acquire("vm", &a, now).unwrap();
        assert_eq!(l.acquire("vm", &a, now).unwrap(), one);
        assert!(l.acquire("vm", &b, now).is_err());
        assert!(l.release("vm", &b, one, now).is_err());
        let later = now + Duration::from_secs(61);
        assert!(l.check("vm", &a, one, later).is_err());
        let two = l.acquire("vm", &b, later).unwrap();
        assert!(two > one);
        l.release("vm", &b, two, later).unwrap();
        assert!(l.check("vm", &b, two, later).is_err());
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::*;
    #[test]
    fn late_acquire_cannot_resurrect_closed_session() {
        let mut leases = Leases::default();
        let now = Instant::now();
        let owner = "a".repeat(64);
        let epoch = leases.acquire("vm", &owner, now).unwrap();
        assert_eq!(leases.retire_owner(&owner, now), vec!["vm"]);
        assert!(leases.check("vm", &owner, epoch, now).is_err());
        assert!(leases
            .acquire("vm", &owner, now + Duration::from_secs(40))
            .is_err());
        assert!(leases
            .acquire("other-vm", &owner, now + Duration::from_secs(119))
            .is_err());
        leases.acquire("vm", &"b".repeat(64), now).unwrap();
        // Tombstones are finite; callers must always use fresh random owners for
        // new sessions. Controller's 30s admission bound excludes old queued RPCs.
        assert!(leases
            .acquire("vm", &owner, now + Duration::from_secs(121))
            .is_ok());
    }
    #[test]
    fn retirement_capacity_never_evicts_live_fence_or_prevents_release() {
        let mut leases = Leases::default();
        let now = Instant::now();
        for i in 0..RETIRED_OWNER_CAPACITY {
            leases.retire_owner(&format!("{i:064x}"), now);
        }
        let owner = "f".repeat(64);
        let epoch = leases.acquire("vm", &owner, now).unwrap();
        assert_eq!(leases.retire_owner(&owner, now), vec!["vm"]);
        assert!(leases.check("vm", &owner, epoch, now).is_err());
        assert_eq!(leases.retired.len(), RETIRED_OWNER_CAPACITY);
        assert!(leases.acquire("vm", &owner, now).is_err());
        assert!(leases.acquire("vm", &"a".repeat(64), now).is_err());
        assert!(leases
            .acquire("vm", &"a".repeat(64), now + Duration::from_secs(121))
            .is_ok());
    }
}
