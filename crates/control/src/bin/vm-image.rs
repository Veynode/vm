//! APFS raw-disk CoW images. No full-copy fallback, live snapshot or guest consistency claim.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
#[cfg(target_os = "macos")]
use std::ffi::CString;
use std::{
    ffi::CStr,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    id: String,
    kind: String,
    format: String,
    virtual_size_bytes: u64,
    initial_sha256: String,
    created_unix_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_id: Option<String>,
    clone_mechanism: String,
}
fn private_dir(path: &Path) -> Result<File> {
    if !path.exists() {
        DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .context("image store directory must not be a symlink")?;
    let m = file.metadata()?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
        bail!("image store directory must be owned by current user and private (0700)")
    }
    Ok(file)
}
fn open_file(path: &Path, write: bool, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("cannot safely open {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("image input must be a regular file")
    }
    Ok(file)
}
fn lock(file: &File, exclusive: bool) -> Result<()> {
    if unsafe {
        libc::flock(
            file.as_raw_fd(),
            (if exclusive {
                libc::LOCK_EX
            } else {
                libc::LOCK_SH
            }) | libc::LOCK_NB,
        )
    } != 0
    {
        bail!("image or store is busy; active VM disks cannot be imported or cloned")
    }
    Ok(())
}
fn global_disk_lock(source: &File) -> Result<File> {
    // Match the native worker's sidecar lock to fence its startup before VZ owns the disk.
    let entry = unsafe { libc::getpwuid(libc::geteuid()) };
    if entry.is_null() {
        bail!("cannot resolve user home for VM disk lock")
    }
    let home = PathBuf::from(unsafe { CStr::from_ptr((*entry).pw_dir) }.to_str()?);
    let root = home.join("Library/Application Support/VeynodeVM");
    // Existing product directory may be 0755, but must not be group/other writable.
    if !root.exists() {
        DirBuilder::new().mode(0o700).create(&root)?;
    }
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&root)?;
    let m = dir.metadata()?;
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
        bail!("unsafe native lock directory")
    }
    let locks = root.join("locks");
    let _directory = private_dir(&locks)?;
    let m = source.metadata()?;
    let file = open_file(
        &locks.join(format!("{}-{}.lock", m.dev(), m.ino())),
        true,
        true,
    )?;
    let m = file.metadata()?;
    if m.uid() != unsafe { libc::geteuid() } || m.nlink() != 1 || m.mode() & 0o077 != 0 {
        bail!("unsafe disk lock sidecar")
    }
    lock(&file, false)?;
    Ok(file)
}
fn hash(file: &mut File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
#[cfg(target_os = "macos")]
fn check_space(dir: &File, min_free_bytes: u64) -> Result<u64> {
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(dir.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stats = unsafe { stats.assume_init() };
    let filesystem = unsafe { CStr::from_ptr(stats.f_fstypename.as_ptr()) }.to_str()?;
    if filesystem != "apfs" {
        bail!("APFS is required; full-copy fallback is intentionally disabled")
    }
    let available = stats
        .f_bavail
        .checked_mul(stats.f_bsize as u64)
        .context("free space arithmetic overflow")?;
    if available < min_free_bytes {
        bail!("insufficient free space: {available} bytes available, {min_free_bytes} bytes minimum requested; base unchanged")
    }
    Ok(available)
}
#[cfg(not(target_os = "macos"))]
fn check_space(_: &File, _: u64) -> Result<u64> {
    bail!("APFS image cloning is supported on macOS only; no full-copy fallback")
}
#[cfg(target_os = "macos")]
fn cow_clone(source: &File, directory: &File, name: &str) -> Result<()> {
    let name = CString::new(name)?;
    // fd source closes pathname replacement races; 0x0002 is CLONE_NOOWNERCOPY in sys/clonefile.h.
    if unsafe {
        libc::fclonefileat(
            source.as_raw_fd(),
            directory.as_raw_fd(),
            name.as_ptr(),
            0x0002,
        )
    } != 0
    {
        bail!(
            "APFS clone failed (no full-copy fallback): {}",
            std::io::Error::last_os_error()
        )
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn cow_clone(_: &File, _: &File, _: &str) -> Result<()> {
    bail!("APFS clone unavailable")
}
#[cfg(target_os = "macos")]
fn publish(parent: &File, staging: &str, id: &str) -> Result<()> {
    let from = CString::new(staging)?;
    let to = CString::new(id)?;
    if unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error())
            .context("publishing image without overwriting existing path");
    }
    parent.sync_all()?;
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn publish(_: &File, _: &str, _: &str) -> Result<()> {
    bail!("APFS publish unavailable")
}
struct Staging {
    path: PathBuf,
    published: bool,
}
impl Drop for Staging {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
struct Store {
    root: PathBuf,
    _lock: File,
}
impl Store {
    fn open(root: &Path) -> Result<Self> {
        if !root.is_absolute() {
            bail!("store path must be absolute")
        }
        let _root = private_dir(root)?;
        let guard = open_file(&root.join("store.lock"), true, true)?;
        let m = guard.metadata()?;
        if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 || m.nlink() != 1 {
            bail!("unsafe image store lock")
        }
        lock(&guard, true)?;
        Ok(Self {
            root: root.canonicalize()?,
            _lock: guard,
        })
    }
    fn add(
        &self,
        id: &str,
        kind: &str,
        mut source: File,
        base_id: Option<String>,
        expected_hash: Option<&str>,
        min_free_bytes: u64,
    ) -> Result<serde_json::Value> {
        vm_control::validate_id(id)?;
        let parent_path = self
            .root
            .join(if kind == "base" { "bases" } else { "clones" });
        let parent = private_dir(&parent_path)?;
        if fs::symlink_metadata(parent_path.join(id)).is_ok() {
            bail!("image id already exists; refusing overwrite")
        }
        let available = check_space(&parent, min_free_bytes)?;
        let metadata = source.metadata()?;
        if metadata.len() == 0 || metadata.len() % 512 != 0 {
            bail!("raw disk size must be a positive multiple of 512 bytes")
        }
        let mut magic = [0u8; 4];
        source.read_exact(&mut magic)?;
        source.seek(SeekFrom::Start(0))?;
        if &magic == b"QFI\xfb" {
            bail!("QCOW2 is not a raw disk and cannot be attached to this backend")
        }
        let _disk_guard = global_disk_lock(&source)?;
        lock(&source, false)?;
        if let Some(expected) = expected_hash {
            if hash(&mut source)? != expected {
                bail!("base integrity check failed; refusing clone")
            }
        }
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let staging_name = format!(".staging-{}-{nonce}", std::process::id());
        let staging_path = parent_path.join(&staging_name);
        DirBuilder::new().mode(0o700).create(&staging_path)?;
        let mut staging = Staging {
            path: staging_path.clone(),
            published: false,
        };
        let directory = private_dir(&staging_path)?;
        cow_clone(&source, &directory, "disk.raw")?;
        let mut disk = open_file(&staging_path.join("disk.raw"), false, false)?;
        // fclonefileat does not copy ACLs, and mode is narrowed before publication.
        disk.set_permissions(fs::Permissions::from_mode(if kind == "base" {
            0o400
        } else {
            0o600
        }))?;
        let fingerprint = hash(&mut disk)?;
        if expected_hash.is_some_and(|expected| expected != fingerprint) {
            bail!("clone integrity differs from base; publication aborted")
        }
        let after = source.metadata()?;
        if after.len() != metadata.len()
            || after.mtime() != metadata.mtime()
            || after.mtime_nsec() != metadata.mtime_nsec()
            || after.ctime() != metadata.ctime()
            || after.ctime_nsec() != metadata.ctime_nsec()
        {
            bail!("source changed during import; publication aborted")
        }
        disk.sync_all()?;
        let manifest = Manifest {
            version: 1,
            id: id.into(),
            kind: kind.into(),
            format: "raw".into(),
            virtual_size_bytes: metadata.len(),
            initial_sha256: fingerprint,
            created_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
            base_id,
            clone_mechanism: "APFS fclonefileat copy-on-write".into(),
        };
        let mut manifest_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(staging_path.join("manifest.json"))?;
        serde_json::to_writer_pretty(&mut manifest_file, &manifest)?;
        manifest_file.write_all(b"\n")?;
        manifest_file.set_permissions(fs::Permissions::from_mode(0o400))?;
        manifest_file.sync_all()?;
        directory.sync_all()?;
        publish(&parent, &staging_name, id)?;
        staging.published = true;
        Ok(
            json!({"manifest":manifest,"disk_path":parent_path.join(id).join("disk.raw"),"available_bytes_before_clone":available,"allocation_note":"APFS CoW shares blocks initially; future writes allocate space and may fail ENOSPC. Logical size is not physical allocation."}),
        )
    }
    fn clone_base(
        &self,
        base_id: &str,
        id: &str,
        min_free_bytes: u64,
    ) -> Result<serde_json::Value> {
        vm_control::validate_id(base_id)?;
        vm_control::validate_id(id)?;
        let _bases = private_dir_existing(&self.root.join("bases"))?;
        let base = self.root.join("bases").join(base_id);
        let _dir = private_dir_existing(&base)?;
        let manifest_file = open_file(&base.join("manifest.json"), false, false)?;
        if manifest_file.metadata()?.len() > 65536 {
            bail!("base manifest exceeds limit")
        }
        let manifest: Manifest = serde_json::from_reader(manifest_file)?;
        if manifest.version != 1
            || manifest.kind != "base"
            || manifest.format != "raw"
            || manifest.id != base_id
            || manifest.base_id.is_some()
            || manifest.initial_sha256.len() != 64
        {
            bail!("invalid base manifest")
        }
        let source = open_file(&base.join("disk.raw"), false, false)?;
        let meta = source.metadata()?;
        if meta.uid() != unsafe { libc::geteuid() }
            || meta.nlink() != 1
            || meta.mode() & 0o277 != 0
            || meta.len() != manifest.virtual_size_bytes
        {
            bail!("base disk metadata or read-only permissions invalid")
        }
        self.add(
            id,
            "clone",
            source,
            Some(base_id.into()),
            Some(&manifest.initial_sha256),
            min_free_bytes,
        )
    }
}
fn private_dir_existing(path: &Path) -> Result<File> {
    if fs::symlink_metadata(path).is_err() {
        bail!("base does not exist")
    }
    private_dir(path)
}
fn main() {
    if let Err(e) = run() {
        eprintln!("vm-image: {e:#}");
        std::process::exit(1)
    }
}
fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let action=args.next().context("usage: vm-image import|clone --store PATH --id ID [--source PATH --source-stopped | --base ID] [--min-free-mib 64]")?;
    let mut store = None;
    let mut id = None;
    let mut source = None;
    let mut base = None;
    let mut stopped = false;
    let mut min_free_mib = 64u64;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--store" => store = args.next().map(PathBuf::from),
            "--id" => id = args.next(),
            "--source" => source = args.next().map(PathBuf::from),
            "--base" => base = args.next(),
            "--source-stopped" => stopped = true,
            "--min-free-mib" => {
                min_free_mib = args.next().context("missing space threshold")?.parse()?
            }
            _ => bail!("unknown argument: {arg}"),
        }
    }
    let id = id.context("--id required")?;
    vm_control::validate_id(&id)?;
    let min_free_bytes = min_free_mib
        .checked_mul(1024 * 1024)
        .context("space threshold overflow")?;
    let store = Store::open(&store.context("--store required")?)?;
    let output = match action.as_str() {
        "import" => {
            if !stopped {
                bail!("import requires --source-stopped assertion after a clean guest shutdown; active disk locks are still checked")
            }
            if base.is_some() {
                bail!("import does not accept --base")
            }
            let path = source.context("--source required")?;
            if !path.is_absolute() {
                bail!("source must be an absolute path")
            }
            store.add(
                &id,
                "base",
                open_file(&path, false, false)?,
                None,
                None,
                min_free_bytes,
            )?
        }
        "clone" => {
            if source.is_some() || stopped {
                bail!("clone accepts --base, not --source or --source-stopped")
            };
            store.clone_base(&base.context("--base required")?, &id, min_free_bytes)?
        }
        _ => bail!("unknown command"),
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

#[cfg(test)]
mod hash_tests {
    use super::*;
    #[test]
    fn sha256_known_vector_matches_with_platform_acceleration() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"abc").unwrap();
        assert_eq!(
            hash(&mut file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
