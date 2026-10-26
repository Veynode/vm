//! Host bookkeeping for opaque guest jobs; no command bodies or output retained.
//! The owner is the MCP session that started a job, so ending that session can
//! cancel what it left running.
use anyhow::{bail, Result};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
#[derive(Clone)]
pub struct Job {
    pub vm: String,
    pub id: String,
    pub owner: String,
    created: Instant,
    finished: bool,
}
#[derive(Default)]
pub struct Jobs {
    entries: BTreeMap<(String, String), Job>,
    reservations: usize,
}
// Only observed terminal jobs are eligible; cancellation acknowledgements are not terminal.
pub(crate) fn make_room<T>(
    entries: &mut BTreeMap<(String, String), T>,
    reservations: usize,
    state: impl Fn(&T) -> (Instant, bool),
) -> bool {
    while entries.len() + reservations >= 256 {
        let oldest = entries
            .iter()
            .filter_map(|(key, entry)| {
                let (created, finished) = state(entry);
                finished.then_some((key, created))
            })
            .min_by_key(|(_, created)| *created)
            .map(|(key, _)| key.clone());
        match oldest {
            Some(key) => {
                entries.remove(&key);
            }
            None => return false,
        }
    }
    true
}
// TTL is a result-retention policy, never evidence that an unobserved job ended.
pub(crate) fn prune_terminal<T>(
    entries: &mut BTreeMap<(String, String), T>,
    now: Instant,
    state: impl Fn(&T) -> (Instant, bool),
) {
    entries.retain(|_, value| {
        let (created, terminal_confirmed) = state(value);
        !terminal_confirmed || now.saturating_duration_since(created) < Duration::from_secs(330)
    });
}

pub fn validate_job_id(id: &str) -> Result<()> {
    if id.len() != 32
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("job_id must be 32 lowercase hexadecimal characters")
    }
    Ok(())
}
impl Jobs {
    /// An unacknowledged start may have left the pending RPC table but not yet
    /// registered its Guest job. Reservations close that admission race.
    pub(crate) fn snapshot_idle(&self, vm: &str) -> bool {
        self.reservations == 0
            && !self
                .entries
                .values()
                .any(|job| job.vm == vm && !job.finished)
    }
    fn prune(&mut self, now: Instant) {
        prune_terminal(&mut self.entries, now, |job| (job.created, job.finished));
    }
    pub fn reserve(table: &Arc<Mutex<Self>>, now: Instant) -> Result<Reservation> {
        let mut jobs = table.lock().unwrap();
        jobs.prune(now);
        let reservations = jobs.reservations;
        if !make_room(&mut jobs.entries, reservations, |job| {
            (job.created, job.finished)
        }) {
            bail!("host job ownership capacity reached; unresolved jobs require terminal proof or VM shutdown/restart")
        }
        jobs.reservations += 1;
        Ok(Reservation {
            table: table.clone(),
            active: true,
        })
    }
    pub fn known(&mut self, vm: &str, id: &str, now: Instant) -> Result<Job> {
        validate_job_id(id)?;
        self.prune(now);
        let job = self
            .entries
            .get(&(vm.into(), id.into()))
            .ok_or_else(|| anyhow::anyhow!("job is unknown or expired on this VM"))?;
        Ok(job.clone())
    }
    pub fn finish(&mut self, vm: &str, id: &str) {
        if let Some(job) = self.entries.get_mut(&(vm.into(), id.into())) {
            job.finished = true
        }
    }
    #[cfg(test)]
    pub fn running(&mut self, vm: &str, now: Instant) -> Vec<Job> {
        self.prune(now);
        self.entries
            .values()
            .filter(|job| job.vm == vm && !job.finished)
            .cloned()
            .collect()
    }
    pub fn owned_running(&mut self, owner: &str, vm: Option<&str>, now: Instant) -> Vec<Job> {
        self.prune(now);
        self.entries
            .values()
            .filter(|job| job.owner == owner && !job.finished && vm.is_none_or(|id| job.vm == id))
            .cloned()
            .collect()
    }
    pub fn clear_vm(&mut self, vm: &str) {
        self.entries.retain(|(id, _), _| id != vm);
    }
}
pub struct Reservation {
    table: Arc<Mutex<Jobs>>,
    active: bool,
}
impl Reservation {
    pub fn register(mut self, vm: &str, id: &str, owner: &str, now: Instant) -> Result<()> {
        validate_job_id(id)?;
        let mut table = self.table.lock().unwrap();
        let key = (vm.into(), id.into());
        if table.entries.contains_key(&key) {
            bail!("guest returned a duplicate job identity")
        }
        table.entries.insert(
            key,
            Job {
                vm: vm.into(),
                id: id.into(),
                owner: owner.into(),
                created: now,
                finished: false,
            },
        );
        table.reservations -= 1;
        self.active = false;
        Ok(())
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if self.active {
            self.table.lock().unwrap().reservations -= 1
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jobs_are_bound_to_their_vm() {
        let jobs = Arc::new(Mutex::new(Jobs::default()));
        let now = Instant::now();
        let id = "a".repeat(32);
        Jobs::reserve(&jobs, now)
            .unwrap()
            .register("vm", &id, "owner", now)
            .unwrap();
        let mut table = jobs.lock().unwrap();
        assert!(table.known("vm", &id, now).is_ok());
        assert!(table.known("other-vm", &id, now).is_err());
        assert!(table.known("vm", &"b".repeat(32), now).is_err());
        assert!(
            table
                .known("vm", &id, now + Duration::from_secs(331))
                .is_ok(),
            "elapsed time cannot erase an unresolved cleanup obligation"
        );
    }
    #[test]
    fn finished_jobs_are_not_running() {
        let jobs = Arc::new(Mutex::new(Jobs::default()));
        let now = Instant::now();
        let id = "a".repeat(32);
        Jobs::reserve(&jobs, now)
            .unwrap()
            .register("vm", &id, "owner", now)
            .unwrap();
        let mut table = jobs.lock().unwrap();
        assert_eq!(table.running("vm", now).len(), 1);
        table.finish("vm", &id);
        assert!(table.running("vm", now).is_empty());
        assert!(table.known("vm", &id, now).is_ok());
    }
    #[test]
    fn in_flight_jobs_reserve_bounded_capacity_and_release_on_failure() {
        let jobs = Arc::new(Mutex::new(Jobs::default()));
        let now = Instant::now();
        let reservations = (0..256)
            .map(|_| Jobs::reserve(&jobs, now).unwrap())
            .collect::<Vec<_>>();
        assert!(Jobs::reserve(&jobs, now).is_err());
        drop(reservations);
        assert!(Jobs::reserve(&jobs, now).is_ok());
        assert!(validate_job_id("../bad").is_err());
    }
    #[test]
    fn snapshot_admission_fences_unregistered_starts_and_unfinished_jobs() {
        let jobs = Arc::new(Mutex::new(Jobs::default()));
        let now = Instant::now();
        assert!(jobs.lock().unwrap().snapshot_idle("vm"));
        let reservation = Jobs::reserve(&jobs, now).unwrap();
        assert!(!jobs.lock().unwrap().snapshot_idle("vm"));
        drop(reservation);
        assert!(jobs.lock().unwrap().snapshot_idle("vm"));
        let id = "a".repeat(32);
        Jobs::reserve(&jobs, now)
            .unwrap()
            .register("vm", &id, "owner", now)
            .unwrap();
        assert!(!jobs.lock().unwrap().snapshot_idle("vm"));
        assert!(jobs.lock().unwrap().snapshot_idle("other-vm"));
        jobs.lock().unwrap().finish("vm", &id);
        assert!(jobs.lock().unwrap().snapshot_idle("vm"));
    }
    #[test]
    fn capacity_evicts_oldest_finished_without_losing_active_jobs_or_reservations() {
        let jobs = Arc::new(Mutex::new(Jobs::default()));
        let now = Instant::now();
        for i in 0..256 {
            let created = now + Duration::from_millis(i);
            Jobs::reserve(&jobs, created)
                .unwrap()
                .register("vm", &format!("{i:032x}"), "owner", created)
                .unwrap();
        }
        let now = now + Duration::from_secs(1);
        {
            let mut table = jobs.lock().unwrap();
            table.finish("vm", &format!("{:032x}", 30));
            table.finish("vm", &format!("{:032x}", 10));
        }
        let first = Jobs::reserve(&jobs, now).unwrap();
        {
            let mut table = jobs.lock().unwrap();
            assert!(table.known("vm", &format!("{:032x}", 10), now).is_err());
            assert!(table.known("vm", &format!("{:032x}", 30), now).is_ok());
            assert!(table.known("vm", &format!("{:032x}", 0), now).is_ok());
        }
        let second = Jobs::reserve(&jobs, now).unwrap();
        assert!(Jobs::reserve(&jobs, now).is_err());
        {
            let mut table = jobs.lock().unwrap();
            assert_eq!(table.reservations, 2);
            assert_eq!(table.running("vm", now).len(), 254);
            assert!(table.known("vm", &format!("{:032x}", 30), now).is_err());
        }
        drop(first);
        drop(second);
    }
    #[test]
    fn session_capacity_preserves_finished_results_until_space_is_needed() {
        let now = Instant::now();
        let mut entries = BTreeMap::new();
        entries.insert(("vm".into(), "done".into()), (now, true));
        assert!(make_room(&mut entries, 0, |entry| *entry));
        assert_eq!(entries.len(), 1);
        for i in 0..255 {
            entries.insert(("vm".into(), format!("active-{i}")), (now, false));
        }
        assert!(make_room(&mut entries, 0, |entry| *entry));
        assert!(!entries.contains_key(&("vm".into(), "done".into())));
        entries.insert(("vm".into(), "new".into()), (now, false));
        assert!(!make_room(&mut entries, 0, |entry| *entry));
        assert_eq!(entries.len(), 256);
    }
    #[test]
    fn unresolved_jobs_survive_ttl_for_owner_and_vm_cleanup_until_terminal_proof() {
        let jobs = Arc::new(Mutex::new(Jobs::default()));
        let now = Instant::now();
        for (id, owner) in [("a".repeat(32), "owner-a"), ("b".repeat(32), "owner-b")] {
            Jobs::reserve(&jobs, now)
                .unwrap()
                .register("vm", &id, owner, now)
                .unwrap();
        }
        let later = now + Duration::from_secs(86400);
        let mut table = jobs.lock().unwrap();
        assert_eq!(table.running("vm", later).len(), 2);
        let old = table.owned_running("owner-a", None, later);
        assert_eq!(old.len(), 1);
        assert_eq!(old[0].id, "a".repeat(32));
        assert!(table.known("vm", &"a".repeat(32), later).is_ok());
        table.finish("vm", &"a".repeat(32));
        assert!(table.owned_running("owner-a", None, later).is_empty());
        assert_eq!(table.owned_running("owner-b", None, later).len(), 1);
        assert!(table.known("vm", &"a".repeat(32), later).is_err());
    }
    #[test]
    fn capacity_never_discards_expired_unresolved_jobs() {
        let jobs = Arc::new(Mutex::new(Jobs::default()));
        let now = Instant::now();
        for i in 0..256 {
            Jobs::reserve(&jobs, now)
                .unwrap()
                .register("vm", &format!("{i:032x}"), "owner", now)
                .unwrap();
        }
        let later = now + Duration::from_secs(86400);
        assert!(Jobs::reserve(&jobs, later).is_err());
        {
            let mut table = jobs.lock().unwrap();
            assert_eq!(table.owned_running("owner", None, later).len(), 256);
            for i in 0..256 {
                assert_eq!(
                    table.known("vm", &format!("{i:032x}"), later).unwrap().id,
                    format!("{i:032x}")
                )
            }
            table.finish("vm", &format!("{:032x}", 17));
        }
        let reservation = Jobs::reserve(&jobs, later).unwrap();
        assert_eq!(jobs.lock().unwrap().entries.len(), 255);
        assert!(Jobs::reserve(&jobs, later).is_err());
        drop(reservation);
    }
    #[test]
    fn prune_retains_unknown_but_expires_confirmed_terminal() {
        let now = Instant::now();
        let mut entries = BTreeMap::from([
            (("vm".into(), "unknown".into()), (5u64, now, false)),
            (("vm".into(), "finished".into()), (7u64, now, true)),
        ]);
        prune_terminal(
            &mut entries,
            now + Duration::from_secs(331),
            |(_, created, terminal)| (*created, *terminal),
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[&("vm".into(), "unknown".into())].0, 5);
    }
}
