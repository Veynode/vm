//! Admission limits assigned guest RAM, not host RSS, swap or VM overhead.
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::{fs, os::unix::fs::MetadataExt, path::Path};
const MIB: u64 = 1024 * 1024;
const HOST_RESERVE_MIB: u64 = 4096;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct MemoryBudget {
    pub physical_memory_mib: u64,
    pub vm_memory_budget_mib: u64,
    pub explicitly_configured: bool,
}
impl MemoryBudget {
    pub fn detect(override_mib: Option<u64>) -> Result<Self> {
        Self::new(physical_memory_bytes()?, override_mib)
    }
    pub fn new(physical_bytes: u64, override_mib: Option<u64>) -> Result<Self> {
        let physical_memory_mib = physical_bytes / MIB;
        if physical_memory_mib == 0 {
            bail!("physical host memory unavailable")
        }
        let vm_memory_budget_mib = if let Some(budget) = override_mib {
            if budget == 0 || budget > physical_memory_mib {
                bail!("explicit memory budget must be positive and no greater than physical host memory ({physical_memory_mib} MiB)")
            }
            budget
        } else {
            // Divide before multiplying, retain the remainder, and avoid arithmetic overflow.
            let three_quarters = (physical_memory_mib / 4) * 3 + (physical_memory_mib % 4) * 3 / 4;
            three_quarters.min(physical_memory_mib.saturating_sub(HOST_RESERVE_MIB))
        };
        Ok(Self {
            physical_memory_mib,
            vm_memory_budget_mib,
            explicitly_configured: override_mib.is_some(),
        })
    }
    pub fn admit(&self, assigned_mib: u64, requested_mib: u64) -> Result<()> {
        let total = assigned_mib
            .checked_add(requested_mib)
            .context("assigned memory arithmetic overflow")?;
        if total > self.vm_memory_budget_mib {
            bail!("memory admission denied: {assigned_mib} MiB active + {requested_mib} MiB requested exceeds {} MiB VM budget ({} MiB physical); adjust allocation or the local daemon memory budget explicitly", self.vm_memory_budget_mib, self.physical_memory_mib)
        }
        Ok(())
    }
}
#[cfg(target_os = "macos")]
fn physical_memory_bytes() -> Result<u64> {
    let mut bytes = 0u64;
    let mut size = std::mem::size_of::<u64>();
    let result = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut bytes as *mut u64).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || size != std::mem::size_of::<u64>() {
        bail!(
            "cannot query macOS physical memory: {}",
            std::io::Error::last_os_error()
        )
    }
    Ok(bytes)
}
#[cfg(not(target_os = "macos"))]
fn physical_memory_bytes() -> Result<u64> {
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if pages <= 0 || page_size <= 0 {
        bail!("cannot query host physical memory")
    }
    (pages as u64)
        .checked_mul(page_size as u64)
        .context("physical memory arithmetic overflow")
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiskIdentity {
    device: u64,
    inode: u64,
}
impl DiskIdentity {
    pub fn read(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path).context("cannot inspect VM disk identity")?;
        if !metadata.is_file() {
            bail!("VM disk must be a regular file")
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn budget_reserves_host_and_rejects_22_gib_on_16_gib() {
        let budget = MemoryBudget::new(16 * 1024 * MIB, None).unwrap();
        assert_eq!(budget.vm_memory_budget_mib, 12 * 1024);
        budget.admit(8 * 1024, 4 * 1024).unwrap();
        assert!(budget.admit(8 * 1024, 8 * 1024).is_err());
        assert!(budget.admit(16 * 1024, 6 * 1024).is_err());
        assert_eq!(
            MemoryBudget::new(8 * 1024 * MIB, None)
                .unwrap()
                .vm_memory_budget_mib,
            4096
        );
        assert_eq!(
            MemoryBudget::new(4 * 1024 * MIB, None)
                .unwrap()
                .vm_memory_budget_mib,
            0
        );
    }
    #[test]
    fn boundaries_explicit_override_and_overflow_fail_closed() {
        let budget = MemoryBudget::new(16 * 1024 * MIB, Some(16384)).unwrap();
        budget.admit(16384, 0).unwrap();
        assert!(budget.admit(16384, 1).is_err());
        assert!(budget.admit(u64::MAX, 1).is_err());
        assert!(MemoryBudget::new(16 * 1024 * MIB, Some(16385)).is_err());
        assert!(MemoryBudget::new(16 * 1024 * MIB, Some(0)).is_err());
        assert!(MemoryBudget::new(0, None).is_err());
        let largest = MemoryBudget::new(u64::MAX, None).unwrap();
        assert!(largest.vm_memory_budget_mib < largest.physical_memory_mib);
    }
    #[test]
    fn disk_hardlinks_share_identity_and_replacement_changes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("disk");
        let b = tmp.path().join("hardlink");
        fs::write(&a, b"fixture").unwrap();
        fs::hard_link(&a, &b).unwrap();
        let active = DiskIdentity::read(&a).unwrap();
        assert_eq!(active, DiskIdentity::read(&b).unwrap());
        fs::remove_file(&a).unwrap();
        fs::write(&a, b"replacement").unwrap();
        assert_ne!(active, DiskIdentity::read(&a).unwrap());
        assert_eq!(active, DiskIdentity::read(&b).unwrap());
    }
}
