//! Private durable local-administrator registry. One flock covers read/modify/atomic publication.
use crate::*;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub host_public_key: String,
    pub grants: BTreeMap<String, SignedGrant>,
    pub versions: BTreeMap<String, u64>,
}
pub struct Store {
    dir: PathBuf,
    _lock: File,
    owner_pid: libc::pid_t,
}
impl Drop for Store {
    fn drop(&mut self) {
        // Closing only our FD does not release flock if an unrelated fork/spawn
        // still holds the inherited open-file description before CLOEXEC. End
        // this process's transaction explicitly. A fork copy must not unlock a
        // parent transaction, and cannot use its inherited handle for I/O.
        if self.owner_pid == unsafe { libc::getpid() } {
            loop {
                let result = unsafe {
                    libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self._lock), libc::LOCK_UN)
                };
                if result == 0
                    || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)
                {
                    break;
                }
            }
        }
    }
}
fn private(file: &File, dir: bool) -> Result<()> {
    let m = file.metadata()?;
    ensure!(
        m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
            && if dir {
                m.is_dir()
            } else {
                m.is_file() && m.nlink() == 1
            },
        "state must be private and owner-held"
    );
    Ok(())
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if !path.exists() {
            fs::DirBuilder::new().mode(0o700).create(path)?
        };
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(path)?;
        private(&directory, true)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.join("lock"))?;
        private(&lock, false)?;
        // No fallible operation after taking the lock until Store owns its drop.
        let dir = path.canonicalize()?;
        ensure!(
            unsafe {
                libc::flock(
                    std::os::fd::AsRawFd::as_raw_fd(&lock),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            } == 0,
            "registry busy"
        );
        Ok(Self {
            dir,
            _lock: lock,
            owner_pid: unsafe { libc::getpid() },
        })
    }
    fn require_owner(&self) -> Result<()> {
        ensure!(
            self.owner_pid == unsafe { libc::getpid() },
            "registry handle belongs to another process"
        );
        Ok(())
    }
    pub fn load(&self) -> Result<Registry> {
        self.require_owner()?;
        let path = self.dir.join("grants.json");
        if let Err(e) = fs::symlink_metadata(&path) {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Ok(Registry::default());
            }
            return Err(e.into());
        };
        let mut f = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        private(&f, false)?;
        ensure!(f.metadata()?.len() <= 1024 * 1024, "registry too large");
        let mut data = Vec::new();
        std::io::Read::by_ref(&mut f)
            .take(1024 * 1024 + 1)
            .read_to_end(&mut data)?;
        ensure!(data.len() <= 1024 * 1024, "registry too large");
        let registry: Registry = serde_json::from_slice(&data)?;
        registry.validate()?;
        Ok(registry)
    }
    pub fn save(&self, registry: &Registry) -> Result<()> {
        self.require_owner()?;
        registry.validate()?;
        let data = serde_json::to_vec_pretty(registry)?;
        ensure!(data.len() <= 1024 * 1024, "registry too large");
        let temporary = self.dir.join(format!(".pending-{}", random32()));
        let outcome = (|| {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temporary)?;
            f.write_all(&data)?;
            f.sync_all()?;
            fs::rename(&temporary, self.dir.join("grants.json"))?;
            File::open(&self.dir)?.sync_all()?;
            Ok(())
        })();
        if outcome.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        outcome
    }
}
impl Registry {
    pub fn validate(&self) -> Result<()> {
        identity_id(&self.host_public_key)?;
        ensure!(
            self.versions.len() <= 1024 && self.grants.len() <= 1024,
            "registry capacity"
        );
        for (key, signed) in &self.grants {
            signed.verify(&self.host_public_key)?;
            ensure!(
                key == &signed.grant.key() && self.versions.get(key) == Some(&signed.grant.version),
                "registry grant/version mismatch"
            )
        }
        for (key, version) in &self.versions {
            let (device, vm) = key.split_once(':').context("invalid registry key")?;
            unhex(device, 32)?;
            identifier(vm)?;
            ensure!(
                (1..=MAX_SAFE_INTEGER).contains(version),
                "invalid persisted version"
            )
        }
        Ok(())
    }
    pub fn approve(
        &mut self,
        pairing: &Pairing,
        verified_digest: &str,
        host: &impl Identity,
    ) -> Result<SignedGrant> {
        ensure!(
            pairing.digest()? == verified_digest,
            "independent pairing digest mismatch"
        );
        ensure!(
            self.host_public_key == pairing.host_public_key
                && host.public_key()? == self.host_public_key,
            "host binding mismatch"
        );
        let device_id = identity_id(&pairing.device_public_key)?;
        let key = format!("{device_id}:{}", pairing.vm_id);
        ensure!(
            !self.grants.contains_key(&key),
            "grant already exists; revoke before re-pairing"
        );
        ensure!(
            self.versions.len() < 1024 || self.versions.contains_key(&key),
            "registry capacity"
        );
        let version = self
            .versions
            .get(&key)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .context("version overflow")?;
        let signed = SignedGrant::new(
            Grant {
                host_id: identity_id(&self.host_public_key)?,
                device_id,
                device_public_key: pairing.device_public_key.clone(),
                vm_id: pairing.vm_id.clone(),
                scopes: pairing.scopes.clone(),
                version,
            },
            host,
        )?;
        self.grants.insert(key.clone(), signed.clone());
        self.versions.insert(key, version);
        Ok(signed)
    }
    pub fn revoke(&mut self, device: &str, vm: &str) -> Result<()> {
        unhex(device, 32)?;
        identifier(vm)?;
        let key = format!("{device}:{vm}");
        ensure!(self.grants.remove(&key).is_some(), "grant not active");
        let version = self
            .versions
            .get(&key)
            .copied()
            .context("missing version")?
            .checked_add(1)
            .context("version overflow")?;
        ensure!(version <= MAX_SAFE_INTEGER, "version exhausted");
        self.versions.insert(key, version);
        Ok(())
    }
}

#[cfg(test)]
mod lock_lifetime_tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn inherited_handle_cannot_unlock_or_access_parent_transaction() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry");
        let parent = Store::open(&path).unwrap();
        // Model the fork copy's mismatched process identity using a real dup of
        // the same open-file description; no allocator calls in a fork child.
        let inherited = Store {
            dir: parent.dir.clone(),
            _lock: parent._lock.try_clone().unwrap(),
            owner_pid: parent.owner_pid.checked_add(1).unwrap(),
        };
        assert!(inherited
            .load()
            .err()
            .unwrap()
            .to_string()
            .contains("another process"));
        assert!(inherited
            .save(&Registry::default())
            .unwrap_err()
            .to_string()
            .contains("another process"));
        drop(inherited);
        assert!(
            Store::open(&path).is_err(),
            "fork-copy drop unlocked the parent"
        );
        drop(parent);
        let independent = Store::open(&path).unwrap();
        assert!(
            Store::open(&path).is_err(),
            "external transaction must still fail closed"
        );
        drop(independent);
        Store::open(&path).unwrap();
    }

    // The child executes only async-signal-safe libc calls and never touches
    // Rust locks/allocators. It retains the parent's descriptor before exec.
    #[test]
    fn parent_drop_releases_lock_while_fork_child_still_holds_description() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry");
        let store = Store::open(&path).unwrap();
        let fd = store._lock.as_raw_fd();
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let mut pipe = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                libc::close(pipe[1]);
                libc::alarm(3); // Bounded even if another simultaneous fork inherits the pipe.
                let mut byte = 0_u8;
                loop {
                    let n = libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1);
                    if n >= 0 {
                        break;
                    }
                }
                libc::_exit(0);
            }
        }
        unsafe {
            libc::close(pipe[0]);
        }
        drop(store);
        let reopened = Store::open(&path);
        // Always unblock/reap our still-owned exact child before asserting.
        unsafe {
            libc::close(pipe[1]);
        }
        let mut status = 0;
        loop {
            let result = unsafe { libc::waitpid(pid, &mut status, 0) };
            if result == pid {
                break;
            }
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EINTR)
            );
        }
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        assert!(
            reopened.is_ok(),
            "parent lock remained inherited before exec: {:?}",
            reopened.err()
        );
    }
}
