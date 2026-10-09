//! Original v2 scope obligation registered BEFORE start bytes, including a lost job ACK.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct Scope {
    pub vm: String,
    pub worker_instance: String,
    pub owner: String,
    pub epoch: u64,
}
#[derive(Default)]
pub(crate) struct Ledger {
    entries: BTreeSet<Scope>,
    retiring: BTreeSet<Scope>,
    journal: Option<crate::exec_journal::Journal>,
}
impl Ledger {
    pub fn open(dir: &std::path::Path, require_existing: bool) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let path = dir.join("exec-journal");
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => std::fs::File::open(dir)?.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
        Ok(Self {
            journal: Some(crate::exec_journal::Journal::open(&path, require_existing)?),
            ..Self::default()
        })
    }
    pub fn admit_launch(&mut self, vm: &str, instance: &str) -> Result<()> {
        self.journal
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("durable execution journal required"))?
            .admit_launch(vm, instance)
    }
    pub fn launch(&mut self, identity: crate::exec_witness::Identity) -> Result<()> {
        self.journal
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("durable execution journal required"))?
            .launch(identity)
    }
    pub fn current_launch(&self, vm: &str, instance: &str) -> bool {
        self.journal
            .as_ref()
            .is_some_and(|j| j.current_launch(vm, instance))
    }
    pub fn incarnations(&self, vm: &str) -> Vec<crate::exec_witness::Identity> {
        self.journal
            .as_ref()
            .map(|j| j.incarnations(vm))
            .unwrap_or_default()
    }
    pub fn pending_launch(&self, vm: &str) -> bool {
        self.journal.as_ref().is_some_and(|j| j.pending_launch(vm))
    }
    pub fn reconcile(&mut self, dir: &std::path::Path, vm: &str, instance: &str) -> Result<()> {
        self.journal
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("durable execution journal required"))?
            .reconcile(&dir.join("exec-journal"), vm, instance)?;
        self.entries
            .retain(|s| !(s.vm == vm && s.worker_instance == instance));
        self.retiring
            .retain(|s| !(s.vm == vm && s.worker_instance == instance));
        Ok(())
    }
    pub fn recovery_required(&self, owner: Option<&str>, vm: Option<&str>) -> bool {
        self.journal
            .as_ref()
            .is_some_and(|j| j.recovery_required(owner, vm))
    }
    #[cfg(test)]
    pub fn fail_journal_sync(&mut self) {
        self.journal.as_mut().unwrap().fail_before_sync = true;
    }

    pub fn record(&mut self, s: Scope) -> Result<()> {
        if self.recovery_required(None, Some(&s.vm)) {
            bail!("execution recovery required; cleanup unconfirmed")
        }
        if self.retiring.contains(&s) {
            bail!("execution scope is retiring; cleanup unconfirmed")
        }
        if self.entries.contains(&s) {
            if let Some(journal) = &mut self.journal {
                journal.record(&s)?;
            }
            return Ok(());
        };
        if self.entries.len() >= 256 {
            bail!("execution scope cleanup capacity reached")
        };
        if let Some(journal) = &mut self.journal {
            journal.record(&s)?;
        }
        self.entries.insert(s);
        Ok(())
    }
    pub fn records(&self, owner: Option<&str>, vm: Option<&str>) -> Vec<Scope> {
        self.entries
            .iter()
            .filter(|s| owner.is_none_or(|o| o == s.owner) && vm.is_none_or(|v| v == s.vm))
            .cloned()
            .collect()
    }
    pub fn begin(&mut self, owner: Option<&str>, vm: Option<&str>) -> Vec<Scope> {
        let records = self.records(owner, vm);
        self.retiring.extend(records.iter().cloned());
        records
    }
    pub fn acquire_allowed(&self, vm: &str, owner: &str) -> bool {
        !self
            .entries
            .iter()
            .any(|s| s.vm == vm && (s.owner != owner || self.retiring.contains(s)))
    }
    pub fn complete(&mut self, s: &Scope) -> Result<()> {
        if !self.entries.contains(s) {
            return Ok(());
        }
        if let Some(journal) = &mut self.journal {
            journal.complete(s)?;
        }
        self.retiring.remove(s);
        self.entries.remove(s);
        Ok(())
    }
}

pub fn monotonic_ns() -> Result<u64> {
    let mut t: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let s: u64 = t.tv_sec.try_into()?;
    s.checked_mul(1_000_000_000)
        .and_then(|v| v.checked_add(t.tv_nsec as u64))
        .ok_or_else(|| anyhow::anyhow!("host monotonic overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unresolved_capacity_and_retirement_never_evict_original_scopes() {
        let mut ledger = Ledger::default();
        for epoch in 1..=256 {
            ledger
                .record(Scope {
                    vm: "vm".into(),
                    worker_instance: "a".repeat(64),
                    owner: "b".repeat(64),
                    epoch,
                })
                .unwrap();
        }
        let foreign = Scope {
            vm: "vm".into(),
            worker_instance: "c".repeat(64),
            owner: "b".repeat(64),
            epoch: 1,
        };
        assert!(ledger.record(foreign.clone()).is_err());
        let records = ledger.begin(Some(&"b".repeat(64)), Some("vm"));
        assert_eq!(records.len(), 256);
        assert!(!ledger.acquire_allowed("vm", &"b".repeat(64)));
        assert!(ledger.record(records[0].clone()).is_err());
        ledger.complete(&foreign).unwrap();
        assert_eq!(ledger.records(None, None).len(), 256);
        ledger.complete(&records[0]).unwrap();
        assert_eq!(ledger.records(None, None).len(), 255);
        ledger.record(foreign).unwrap();
        assert_eq!(ledger.records(None, None).len(), 256);
    }
}
