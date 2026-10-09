//! Lifetime witness: only descriptor close on Drop, never LOCK_UN.
//! The original locked OFD is inherited through worker/guardian/QEMU.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    ffi::CString,
    fs::File,
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::MetadataExt, process::CommandExt},
    },
    path::Path,
    process::Command,
    sync::Arc,
};
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub vm: String,
    pub worker_instance: String,
    pub device: u64,
    pub inode: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Content {
    version: u32,
    vm: String,
    worker_instance: String,
}
impl Identity {
    pub fn validate(&self) -> Result<()> {
        crate::validate_id(&self.vm)?;
        ensure!(
            crate::native_input::hex(&self.worker_instance, 64) && self.inode > 0,
            "invalid incarnation witness identity"
        );
        Ok(())
    }
    fn name(&self) -> CString {
        CString::new(format!("{}.witness", self.worker_instance)).unwrap()
    }
    fn bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&Content {
            version: 1,
            vm: self.vm.clone(),
            worker_instance: self.worker_instance.clone(),
        })?)
    }
    fn opaque(&self) -> String {
        format!(
            "veynode-exec-witness:{}:{}:{}",
            self.worker_instance, self.device, self.inode
        )
    }
}
fn owned(fd: i32) -> Result<File> {
    if fd < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn directory(path: &Path) -> Result<File> {
    use std::os::unix::ffi::OsStrExt;
    ensure!(
        path.is_absolute() && std::fs::canonicalize(path)? == path,
        "witness directory must be canonical"
    );
    let c = CString::new(path.as_os_str().as_bytes())?;
    let file = owned(unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })?;
    let m = file.metadata()?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o777 == 0o700,
        "unsafe witness directory"
    );
    Ok(file)
}
fn check(file: &File, identity: &Identity) -> Result<()> {
    identity.validate()?;
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o777 == 0o600
            && m.nlink() == 1
            && m.dev() == identity.device
            && m.ino() == identity.inode
            && m.len() <= 512,
        "unsafe or replaced incarnation witness"
    );
    use std::os::unix::fs::FileExt;
    let mut b = vec![0; m.len() as usize];
    file.read_exact_at(&mut b, 0)?;
    ensure!(
        b == identity.bytes()?,
        "incarnation witness content mismatch"
    );
    Ok(())
}
#[derive(Clone)]
pub struct Witness {
    file: Arc<File>,
    pub identity: Identity,
}
impl Witness {
    pub fn create(path: &Path, vm: &str, instance: &str) -> Result<Self> {
        let dir = directory(path)?;
        let mut id = Identity {
            vm: vm.into(),
            worker_instance: instance.into(),
            device: 0,
            inode: 1,
        };
        id.validate()?;
        let mut file = owned(unsafe {
            libc::openat(
                dir.as_raw_fd(),
                id.name().as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        })?;
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "cannot lock new incarnation witness"
        );
        file.write_all(&id.bytes()?)?;
        file.sync_all()?;
        dir.sync_all()?;
        let m = file.metadata()?;
        id.device = m.dev();
        id.inode = m.ino();
        let pinned = owned(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) })?;
        check(&pinned, &id)?;
        Ok(Self {
            file: Arc::new(pinned),
            identity: id,
        })
    }
    /// Only used with a daemon-supplied inherited descriptor, never by reopening a path.
    pub fn inherited(fd: i32, vm: &str, instance: &str) -> Result<Self> {
        ensure!(fd >= 10, "invalid inherited witness descriptor");
        let file = owned(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) })?;
        unsafe { libc::close(fd) };
        let m = file.metadata()?;
        let identity = Identity {
            vm: vm.into(),
            worker_instance: instance.into(),
            device: m.dev(),
            inode: m.ino(),
        };
        check(&file, &identity)?;
        // Requiring the inherited OFD's EX lock succeeds without changing/unlocking it.
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "inherited witness not exclusively held"
        );
        Ok(Self {
            file: Arc::new(file),
            identity,
        })
    }
    pub fn configure_worker(&self, command: &mut Command) -> Result<()> {
        self.inherit(command)?;
        command
            .arg("--exec-witness-fd")
            .arg(self.file.as_raw_fd().to_string());
        Ok(())
    }
    fn inherit(&self, command: &mut Command) -> Result<()> {
        check(&self.file, &self.identity)?;
        let file = self.file.clone();
        unsafe {
            command.pre_exec(move || {
                let fd = file.as_raw_fd();
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        Ok(())
    }
    pub fn configure_qemu(&self, command: &mut Command) -> Result<()> {
        self.inherit(command)?;
        command.arg("-add-fd").arg(format!(
            "fd={},set=3,opaque={}",
            self.file.as_raw_fd(),
            self.identity.opaque()
        ));
        Ok(())
    }
    pub fn verify_fdsets(&self, value: &Value) -> Result<()> {
        let sets = value.as_array().context("missing QEMU fdsets")?;
        let found: Vec<_> = sets.iter().filter(|s| s["fdset-id"] == 3).collect();
        ensure!(
            found.len() == 1,
            "incarnation witness QEMU fdset missing or duplicated"
        );
        let fds = found[0]["fds"]
            .as_array()
            .context("missing witness fdset descriptors")?;
        ensure!(
            fds.len() == 1
                && fds[0]["fd"].as_i64().is_some_and(|f| f > 2)
                && fds[0]["opaque"].as_str() == Some(self.identity.opaque().as_str()),
            "QEMU incarnation witness proof mismatch"
        );
        Ok(())
    }
}
/// Held through durable journal removal; absence of all original OFD holders only.
pub struct ReconciliationGuard {
    file: File,
    directory: File,
    identity: Identity,
}
impl ReconciliationGuard {
    pub fn acquire(path: &Path, identity: &Identity) -> Result<Self> {
        identity.validate()?;
        let directory = directory(path)?;
        let file = owned(unsafe {
            libc::openat(
                directory.as_raw_fd(),
                identity.name().as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        check(&file, identity)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("original execution incarnation remains locked; recovery unconfirmed")
        }
        let guard = Self {
            file,
            directory,
            identity: identity.clone(),
        };
        guard.verify()?;
        Ok(guard)
    }
    pub fn verify(&self) -> Result<()> {
        check(&self.file, &self.identity)?;
        let mut at: libc::stat = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe {
                libc::fstatat(
                    self.directory.as_raw_fd(),
                    self.identity.name().as_ptr(),
                    &mut at,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == 0
                && at.st_dev as u64 == self.identity.device
                && at.st_ino == self.identity.inode,
            "incarnation witness path changed"
        );
        Ok(())
    }
}

/// GC is permitted only after the caller has a durable completed receipt for this
/// identity. Missing means prior collection, never evidence for a pending launch.
pub(crate) fn collect_completed(path: &Path, identity: &Identity) -> Result<()> {
    identity.validate()?;
    let dir = directory(path)?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            identity.name().as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
        dir.sync_all()?;
        return Ok(());
    }
    let file = owned(fd)?;
    check(&file, identity)?;
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "completed witness remains locked; metadata collection deferred"
    );
    let guard = ReconciliationGuard {
        file,
        directory: dir,
        identity: identity.clone(),
    };
    guard.verify()?;
    if unsafe { libc::unlinkat(guard.directory.as_raw_fd(), identity.name().as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    guard.directory.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::{BufRead, BufReader},
        os::unix::{
            fs::{symlink, PermissionsExt},
            net::UnixStream,
        },
        process::Stdio,
        time::{Duration, Instant},
    };
    fn dir() -> tempfile::TempDir {
        let d = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o700)).unwrap();
        d
    }
    #[test]
    fn completed_collection_rejects_lock_alias_symlink_fifo_and_changed_content() {
        for mode in ["locked", "hardlink", "symlink", "fifo", "changed", "mode"] {
            let d = dir();
            let witness = Witness::create(d.path(), "vm", &"a".repeat(64)).unwrap();
            let id = witness.identity.clone();
            let path = d.path().join(format!("{}.witness", id.worker_instance));
            if mode == "locked" {
                assert!(collect_completed(d.path(), &id).is_err());
                assert!(path.exists());
                drop(witness);
                continue;
            }
            drop(witness);
            match mode {
                "hardlink" => fs::hard_link(&path, d.path().join("alias")).unwrap(),
                "symlink" => {
                    fs::rename(&path, d.path().join("original")).unwrap();
                    symlink(d.path().join("original"), &path).unwrap();
                }
                "fifo" => {
                    fs::remove_file(&path).unwrap();
                    use std::os::unix::ffi::OsStrExt;
                    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
                    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
                }
                "changed" => fs::write(&path, b"changed").unwrap(),
                "mode" => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
                _ => unreachable!(),
            }
            let start = Instant::now();
            assert!(collect_completed(d.path(), &id).is_err(), "{mode}");
            assert!(start.elapsed() < Duration::from_secs(1), "{mode}");
            assert!(fs::symlink_metadata(&path).is_ok(), "{mode}");
        }
        let d = dir();
        let witness = Witness::create(d.path(), "vm", &"b".repeat(64)).unwrap();
        let id = witness.identity.clone();
        drop(witness);
        collect_completed(d.path(), &id).unwrap();
        collect_completed(d.path(), &id).unwrap();
        // The same missing file can never be pending-incarnation proof.
        assert!(ReconciliationGuard::acquire(d.path(), &id).is_err());
    }
    fn available(path: &Path, id: &Identity) -> bool {
        ReconciliationGuard::acquire(path, id).is_ok()
    }
    fn await_available(path: &Path, id: &Identity) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !available(path, id) {
            assert!(
                Instant::now() < deadline,
                "inherited witness remains locked"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn parent_drop_preserves_inherited_ofd_until_child_reaped() {
        let d = dir();
        let witness = Witness::create(d.path(), "vm", &"a".repeat(64)).unwrap();
        let id = witness.identity.clone();
        assert!(!available(d.path(), &id));
        let mut command = Command::new("/usr/bin/python3");
        command.args(["-c","import os,sys,subprocess; child=subprocess.Popen(['/bin/cat'],stdin=subprocess.PIPE,stdout=subprocess.DEVNULL,close_fds=False); os.close(int(sys.argv[1])); print('ready',flush=True); __import__('select').select([sys.stdin],[],[],10); child.stdin.close(); assert child.wait(timeout=2)==0"])
 .stdin(Stdio::piped()).stdout(Stdio::piped());
        command.arg(witness.file.as_raw_fd().to_string());
        witness.inherit(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        drop(command);
        drop(witness);
        let result = (|| -> Result<()> {
            let mut ready = String::new();
            BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            ensure!(ready == "ready\n", "child not ready");
            ensure!(!available(d.path(), &id), "child did not retain witness");
            Ok(())
        })();
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        await_available(d.path(), &id);
        result.unwrap();
    }
    #[test]
    fn replacements_missing_links_permissions_and_foreign_identity_rejected() {
        for kind in [
            "missing", "replace", "symlink", "hardlink", "mode", "content", "foreign",
        ] {
            let d = dir();
            let witness = Witness::create(d.path(), "vm", &"b".repeat(64)).unwrap();
            let mut id = witness.identity.clone();
            let path = d.path().join(format!("{}.witness", id.worker_instance));
            drop(witness);
            match kind {
                "missing" => {
                    fs::remove_file(&path).unwrap();
                }
                "replace" => {
                    fs::rename(&path, d.path().join("old")).unwrap();
                    fs::write(&path, id.bytes().unwrap()).unwrap();
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                }
                "symlink" => {
                    fs::rename(&path, d.path().join("old")).unwrap();
                    symlink("old", &path).unwrap();
                }
                "hardlink" => fs::hard_link(&path, d.path().join("alias")).unwrap(),
                "mode" => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
                "content" => fs::write(&path, b"{}").unwrap(),
                "foreign" => id.vm = "other".into(),
                _ => unreachable!(),
            }
            assert!(!available(d.path(), &id), "accepted {kind}");
        }
    }
    #[test]
    fn inherited_descriptor_checks_instance_and_stays_cloexec_in_parent() {
        let d = dir();
        let witness = Witness::create(d.path(), "vm", &"c".repeat(64)).unwrap();
        let fd = unsafe { libc::fcntl(witness.file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
        assert!(Witness::inherited(fd, "other", &"c".repeat(64)).is_err());
        let fd = unsafe { libc::fcntl(witness.file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
        let inherited = Witness::inherited(fd, "vm", &"c".repeat(64)).unwrap();
        assert_ne!(
            unsafe { libc::fcntl(inherited.file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        assert_eq!(inherited.identity, witness.identity);
    }
    fn connect(path: &Path) -> Result<BufReader<UnixStream>> {
        let s = UnixStream::connect(path)?;
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        s.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut r = BufReader::new(s);
        let mut line = String::new();
        r.read_line(&mut line)?;
        ensure!(
            serde_json::from_str::<Value>(&line)?["QMP"].is_object(),
            "missing QMP greeting"
        );
        rpc(&mut r, "qmp_capabilities")?;
        Ok(r)
    }
    fn rpc(r: &mut BufReader<UnixStream>, name: &str) -> Result<Value> {
        writeln!(
            r.get_mut(),
            "{}",
            serde_json::json!({"execute":name,"id":name})
        )?;
        for _ in 0..32 {
            let mut line = String::new();
            ensure!(
                r.read_line(&mut line)? > 0 && line.len() < 65536,
                "invalid QMP frame"
            );
            let value: Value = serde_json::from_str(&line)?;
            if value["id"] == name {
                ensure!(value.get("error").is_none(), "QMP failed");
                return Ok(value["return"].clone());
            }
        }
        bail!("QMP reply missing")
    }
    #[test]
    #[ignore = "explicit fixed QEMU paths; diskless machine-none, no Guest"]
    fn actual_diskless_qemu_retains_witness_across_cont_and_monitor_reconnect() {
        let options: Vec<String> = serde_json::from_slice(
            &fs::read(std::env::var("VEYNODE_QEMU_PROBE_ARGUMENTS").unwrap()).unwrap(),
        )
        .unwrap();
        let opt = |name: &str| options[options.iter().position(|s| s == name).unwrap() + 1].clone();
        let d = dir();
        let witness = Witness::create(d.path(), "probe", &"d".repeat(64)).unwrap();
        let id = witness.identity.clone();
        let socket = d.path().join("qmp.sock");
        let guardian_pair = std::env::var("VEYNODE_WITNESS_WORKER")
            .ok()
            .map(|worker| (worker, UnixStream::pair().unwrap()));
        let mut command = if let Some((worker, (_, child))) = &guardian_pair {
            let fd = child.as_raw_fd();
            let mut c = Command::new(worker);
            c.args(["--supervise-qemu", &fd.to_string(), "--"])
                .arg(opt("--qemu-launcher"));
            unsafe {
                c.pre_exec(move || {
                    if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
            c
        } else {
            Command::new(opt("--qemu-launcher"))
        };
        command
            .arg(opt("--qemu-library"))
            .env("VEYNODE_SPICE_LIBRARY", opt("--qemu-spice-library"))
            .args([
                "-machine",
                "none",
                "-accel",
                "tcg",
                "-m",
                "16M",
                "-S",
                "-nodefaults",
                "-display",
                "none",
                "-qmp",
            ])
            .arg(format!("unix:{},server=on,wait=off", socket.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        witness.configure_qemu(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        drop(command);
        let result = (|| -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !socket.exists() {
                ensure!(
                    child.try_wait()?.is_none() && Instant::now() < deadline,
                    "QEMU startup failed"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            let mut qmp = connect(&socket)?;
            witness.verify_fdsets(&rpc(&mut qmp, "query-fdsets")?)?;
            rpc(&mut qmp, "cont")?;
            ensure!(
                rpc(&mut qmp, "query-status")?["running"] == true,
                "not running"
            );
            drop(qmp);
            std::thread::sleep(Duration::from_millis(30));
            let mut qmp = connect(&socket)?;
            witness.verify_fdsets(&rpc(&mut qmp, "query-fdsets")?)?;
            drop(witness);
            ensure!(
                !available(d.path(), &id),
                "QEMU lost witness after parent drop"
            );
            rpc(&mut qmp, "quit")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        result.unwrap();
        assert!(status.success());
        await_available(d.path(), &id);
    }
}
