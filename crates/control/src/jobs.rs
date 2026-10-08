//! Host ownership bookkeeping for opaque guest jobs; no command bodies or output retained.
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
    pub epoch: u64,
    created: Instant,
    finished: bool,
}
#[derive(Default)]
pub struct Jobs {
    entries: BTreeMap<(String, String), Job>,
    reservations: usize,
}
// Used by both daemon ownership records and the MCP adapter's original epochs.
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
    fn prune(&mut self, now: Instant) {
        self.entries
            .retain(|_, job| now.saturating_duration_since(job.created) < Duration::from_secs(330));
    }
    pub fn reserve(table: &Arc<Mutex<Self>>, now: Instant) -> Result<Reservation> {
        let mut jobs = table.lock().unwrap();
        jobs.prune(now);
        let reservations = jobs.reservations;
        if !make_room(&mut jobs.entries, reservations, |job| {
            (job.created, job.finished)
        }) {
            bail!("host job ownership capacity reached; wait for jobs to expire")
        }
        jobs.reservations += 1;
        Ok(Reservation {
            table: table.clone(),
            active: true,
        })
    }
    pub fn owned(
        &mut self,
        vm: &str,
        id: &str,
        owner: &str,
        epoch: u64,
        now: Instant,
    ) -> Result<Job> {
        validate_job_id(id)?;
        self.prune(now);
        let job = self
            .entries
            .get(&(vm.into(), id.into()))
            .filter(|job| job.owner == owner && job.epoch == epoch)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "job is unknown, expired or not owned by this client and lease epoch"
                )
            })?;
        Ok(job.clone())
    }
    pub fn finish(&mut self, vm: &str, id: &str) {
        if let Some(job) = self.entries.get_mut(&(vm.into(), id.into())) {
            job.finished = true
        }
    }
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
    pub fn register(
        mut self,
        vm: &str,
        id: &str,
        owner: &str,
        epoch: u64,
        now: Instant,
    ) -> Result<()> {
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
                epoch,
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
    fn jobs_are_bound_to_vm_owner_and_original_epoch() {
        let jobs = Arc::new(Mutex::new(Jobs::default()));
        let now = Instant::now();
        let id = "a".repeat(32);
        Jobs::reserve(&jobs, now)
            .unwrap()
            .register("vm", &id, "owner", 4, now)
            .unwrap();
        let mut table = jobs.lock().unwrap();
        assert!(table.owned("vm", &id, "owner", 4, now).is_ok());
        assert!(table.owned("other-vm", &id, "owner", 4, now).is_err());
        assert!(table.owned("vm", &id, "other-owner", 4, now).is_err());
        assert!(table.owned("vm", &id, "owner", 5, now).is_err());
        assert!(table
            .owned("vm", &id, "owner", 4, now + Duration::from_secs(331))
            .is_err());
    }
    #[test]
    fn finished_jobs_are_not_cancelled_by_takeover() {
        let jobs = Arc::new(Mutex::new(Jobs::default()));
        let now = Instant::now();
        let id = "a".repeat(32);
        Jobs::reserve(&jobs, now)
            .unwrap()
            .register("vm", &id, "owner", 1, now)
            .unwrap();
        let mut table = jobs.lock().unwrap();
        assert_eq!(table.running("vm", now).len(), 1);
        table.finish("vm", &id);
        assert!(table.running("vm", now).is_empty());
        assert!(table.owned("vm", &id, "owner", 1, now).is_ok());
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
    fn capacity_evicts_oldest_finished_without_losing_active_jobs_or_reservations() {
        let jobs = Arc::new(Mutex::new(Jobs::default()));
        let now = Instant::now();
        for i in 0..256 {
            let created = now + Duration::from_millis(i);
            Jobs::reserve(&jobs, created)
                .unwrap()
                .register("vm", &format!("{i:032x}"), "owner", 1, created)
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
            assert!(table
                .owned("vm", &format!("{:032x}", 10), "owner", 1, now)
                .is_err());
            assert!(table
                .owned("vm", &format!("{:032x}", 30), "owner", 1, now)
                .is_ok());
            assert!(table
                .owned("vm", &format!("{:032x}", 0), "owner", 1, now)
                .is_ok());
        }
        let second = Jobs::reserve(&jobs, now).unwrap();
        assert!(Jobs::reserve(&jobs, now).is_err());
        {
            let mut table = jobs.lock().unwrap();
            assert_eq!(table.reservations, 2);
            assert_eq!(table.running("vm", now).len(), 254);
            assert!(table
                .owned("vm", &format!("{:032x}", 30), "owner", 1, now)
                .is_err());
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
}
