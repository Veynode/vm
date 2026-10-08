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
            dir: path.canonicalize()?,
            _lock: lock,
        })
    }
    pub fn load(&self) -> Result<Registry> {
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
