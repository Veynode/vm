//! Default product locations and the local runtime registration file.
//!
//! Everything lives under one private per-user root so that the daemon, CLI,
//! web UI and MCP adapters agree without repeating absolute paths:
//! `~/Library/Application Support/VeynodeVM` unless `VEYNODEVM_HOME` is set.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, DirBuilder},
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub const RUNTIME_FILE: &str = "runtime.json";

pub fn root() -> Result<PathBuf> {
    veynodevm_api::home()
}

/// Create (or verify) a private directory owned by this user.
pub fn private_dir(path: &Path) -> Result<PathBuf> {
    if !path.exists() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o077 != 0 {
        bail!(
            "{} must be a private directory owned by this user",
            path.display()
        )
    }
    Ok(path.to_path_buf())
}

pub fn state_dir() -> Result<PathBuf> {
    private_dir(&private_dir(&root()?)?.join("state"))
}
pub fn images_dir() -> Result<PathBuf> {
    private_dir(&private_dir(&root()?)?.join("images"))
}
pub fn containers_dir() -> Result<PathBuf> {
    private_dir(&private_dir(&root()?)?.join("containers"))
}
/// Full-distribution machines (UEFI boot, installer ISO, persistent disk).
pub fn machines_dir() -> Result<PathBuf> {
    private_dir(&private_dir(&root()?)?.join("machines"))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Boot {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    /// Kernel module squashfs matching `kernel`, attached read-only.
    pub modloop: PathBuf,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QemuPaths {
    pub worker: PathBuf,
    pub launcher: PathBuf,
    pub library: PathBuf,
    pub resources: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spice_library: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_helper: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_broker: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_helper: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venus_runtime: Option<PathBuf>,
}

impl Boot {
    fn exists(&self) -> bool {
        [&self.kernel, &self.initrd, &self.modloop]
            .iter()
            .all(|p| p.exists())
    }
}

impl QemuPaths {
    /// Whether every path QEMU cannot run without exists.
    fn exists(&self) -> bool {
        [&self.worker, &self.launcher, &self.library, &self.resources]
            .iter()
            .all(|p| p.exists())
    }
    fn fill_optional(&mut self, other: Self, notes: &mut Vec<String>) {
        for (name, mine, theirs) in [
            (
                "qemu.spice_library",
                &mut self.spice_library,
                other.spice_library,
            ),
            (
                "qemu.screenshot_helper",
                &mut self.screenshot_helper,
                other.screenshot_helper,
            ),
            (
                "qemu.console_broker",
                &mut self.console_broker,
                other.console_broker,
            ),
            (
                "qemu.console_helper",
                &mut self.console_helper,
                other.console_helper,
            ),
            (
                "qemu.venus_runtime",
                &mut self.venus_runtime,
                other.venus_runtime,
            ),
        ] {
            *mine = prefer_existing(name, mine.take(), theirs, notes);
        }
    }
}

/// `mine` unless it is gone and `theirs` exists.
fn prefer_existing(
    name: &str,
    mine: Option<PathBuf>,
    theirs: Option<PathBuf>,
    notes: &mut Vec<String>,
) -> Option<PathBuf> {
    match (mine, theirs) {
        (Some(mine), Some(theirs)) if !mine.exists() && theirs.exists() => {
            notes.push(format!(
                "the registered {name} {} is missing; using the bundled {}",
                mine.display(),
                theirs.display()
            ));
            Some(theirs)
        }
        (mine, theirs) => mine.or(theirs),
    }
}

/// Locally registered runtime pieces. Written by the trusted local operator
/// (for development: `scripts/install-dev-runtime.py`); never by MCP clients.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    /// Apple Virtualization worker executable (`vm-apple`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<PathBuf>,
    /// Kernel and initramfs used for OCI micro-VMs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<Boot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qemu: Option<QemuPaths>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_budget_mib: Option<u64>,
    /// Default container DNS servers. Empty: the NAT gateway forwarder from
    /// DHCP, which cannot reach host resolvers bound only to loopback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns: Vec<std::net::IpAddr>,
    /// Default container NIC MTU (576..9000); unset keeps 1500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
    /// Web UI Node host (`web/server.mjs`, next to its built `dist/`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web: Option<PathBuf>,
    /// Host encoder for Apple Virtualization Linux browser desktops
    /// (`native/tools/guest-video-stream.m`: guest NV12 -> VideoToolbox).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vz_desktop_stream: Option<PathBuf>,
    /// Host text recognition for `vm_screen_text` (`native/tools/vm-vision.swift`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<PathBuf>,
    /// Desktop and accessibility helper that macOS containers run over SSH
    /// (`native/tools/vm-macos-desktop.swift`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub macos_desktop: Option<PathBuf>,
    /// `runtime/fetch-cua-driver.sh` output: `<platform>/<binary>` of the
    /// pinned cua-driver (`runtime/cua-driver.json`), which the daemon
    /// copies into Windows and macOS Guests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cua_driver: Option<PathBuf>,
    /// The macOS Guest's vsock port relay (`native/tools/build-macos-relay.sh`),
    /// which `machine.create` writes into a macOS machine's disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub macos_relay: Option<PathBuf>,
}

impl Runtime {
    pub fn path() -> Result<PathBuf> {
        Ok(root()?.join(RUNTIME_FILE))
    }
    pub fn load() -> Result<Self> {
        Ok(Self::load_with_notes()?.0)
    }

    /// The runtime, with a note for each registered piece it set aside. A
    /// registration that cannot be read, or a registered path that no longer
    /// exists while the bundled one does (a development path left behind by
    /// an upgrade), gives way to the bundled runtime.json, so a stale
    /// registration never stops the daemon.
    pub fn load_with_notes() -> Result<(Self, Vec<String>)> {
        let mut notes = Vec::new();
        let mut runtime = match Self::load_registered() {
            Ok(runtime) => runtime,
            Err(error) => {
                notes.push(format!("ignored the registered runtime.json: {error:#}"));
                Self::default()
            }
        };
        if let Some(bundled) = Self::bundled()? {
            runtime.fill_from(bundled, &mut notes);
        }
        Ok((runtime, notes))
    }

    /// The runtime.json shipped next to this executable, with its relative
    /// paths resolved against its own directory: `Contents/Resources` of an
    /// app bundle that runs it from `Contents/MacOS`, or `share/` of a
    /// release directory that runs it from `bin/`
    /// (scripts/package-release.py). `None` when neither has one.
    pub fn bundled() -> Result<Option<Self>> {
        let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|e| e.canonicalize().ok())
            .and_then(|e| e.parent().map(Path::to_path_buf))
        else {
            return Ok(None);
        };
        for shipped in ["../Resources", "../share"] {
            if let Ok(shipped) = dir.join(shipped).canonicalize() {
                if let Some(runtime) = Self::resolved(&shipped)? {
                    return Ok(Some(runtime));
                }
            }
        }
        Ok(None)
    }

    /// `DIR/runtime.json` with its relative paths joined to `DIR`.
    pub fn resolved(dir: &Path) -> Result<Option<Self>> {
        let file = dir.join("runtime.json");
        let Ok(bytes) = fs::read(&file) else {
            return Ok(None);
        };
        if bytes.len() > 64 * 1024 {
            bail!("{} exceeds size limit", file.display())
        }
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid {}", file.display()))?;
        absolute_paths(&mut value, dir);
        let runtime: Self =
            serde_json::from_value(value).with_context(|| format!("invalid {}", file.display()))?;
        runtime.validate()?;
        Ok(Some(runtime))
    }

    /// Takes every piece `other` has and this registration lacks: a
    /// registered runtime.json wins field by field, except where its path is
    /// gone and `other`'s exists (noted in `notes`).
    fn fill_from(&mut self, other: Self, notes: &mut Vec<String>) {
        let mut pick = |name: &str, mine: Option<PathBuf>, theirs: Option<PathBuf>| {
            prefer_existing(name, mine, theirs, notes)
        };
        self.backend = pick("backend", self.backend.take(), other.backend);
        self.web = pick("web", self.web.take(), other.web);
        self.vz_desktop_stream = pick(
            "vz_desktop_stream",
            self.vz_desktop_stream.take(),
            other.vz_desktop_stream,
        );
        self.vision = pick("vision", self.vision.take(), other.vision);
        self.macos_desktop = pick(
            "macos_desktop",
            self.macos_desktop.take(),
            other.macos_desktop,
        );
        self.cua_driver = pick("cua_driver", self.cua_driver.take(), other.cua_driver);
        self.macos_relay = pick("macos_relay", self.macos_relay.take(), other.macos_relay);
        self.boot = match (self.boot.take(), other.boot) {
            (Some(mine), Some(theirs)) if !mine.exists() && theirs.exists() => {
                notes.push(format!(
                    "the registered boot files ({}) are missing; using the bundled ones",
                    mine.kernel.display()
                ));
                Some(theirs)
            }
            (mine, theirs) => mine.or(theirs),
        };
        self.qemu = match (self.qemu.take(), other.qemu) {
            (Some(mine), Some(theirs)) if !mine.exists() && theirs.exists() => {
                notes.push(format!(
                    "the registered QEMU runtime ({}) is missing; using the bundled one",
                    mine.worker.display()
                ));
                Some(theirs)
            }
            (Some(mut mine), Some(theirs)) => {
                mine.fill_optional(theirs, notes);
                Some(mine)
            }
            (mine, theirs) => mine.or(theirs),
        };
        self.memory_budget_mib = self.memory_budget_mib.or(other.memory_budget_mib);
        if self.dns.is_empty() {
            self.dns = other.dns;
        }
        self.mtu = self.mtu.or(other.mtu);
    }

    fn load_registered() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let meta = fs::symlink_metadata(&path)?;
        if !meta.is_file() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o022 != 0 {
            bail!(
                "{} must be a regular file writable only by this user",
                path.display()
            )
        }
        let bytes = fs::read(&path)?;
        if bytes.len() > 64 * 1024 {
            bail!("runtime.json exceeds size limit")
        }
        let runtime: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid {}", path.display()))?;
        runtime.validate()?;
        Ok(runtime)
    }
    pub fn validate(&self) -> Result<()> {
        let mut paths: Vec<&Path> = Vec::new();
        paths.extend(self.backend.as_deref());
        if let Some(boot) = &self.boot {
            paths.extend([
                boot.kernel.as_path(),
                boot.initrd.as_path(),
                boot.modloop.as_path(),
            ]);
        }
        if let Some(q) = &self.qemu {
            paths.extend([
                q.worker.as_path(),
                q.launcher.as_path(),
                q.library.as_path(),
                q.resources.as_path(),
            ]);
            for p in [
                &q.spice_library,
                &q.screenshot_helper,
                &q.console_broker,
                &q.console_helper,
                &q.venus_runtime,
            ]
            .into_iter()
            .flatten()
            {
                paths.push(p);
            }
        }
        paths.extend(self.web.as_deref());
        paths.extend(self.vz_desktop_stream.as_deref());
        paths.extend(self.vision.as_deref());
        paths.extend(self.macos_desktop.as_deref());
        paths.extend(self.cua_driver.as_deref());
        paths.extend(self.macos_relay.as_deref());
        for p in paths {
            if !p.is_absolute() {
                bail!("runtime paths must be absolute: {}", p.display())
            }
        }
        Ok(())
    }
}

/// Every relative path in a bundled runtime.json (each string but the DNS
/// servers), joined to `dir`.
fn absolute_paths(value: &mut serde_json::Value, dir: &Path) {
    match value {
        serde_json::Value::String(s) if !s.starts_with('/') => {
            *s = dir.join(&*s).display().to_string();
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|v| absolute_paths(v, dir)),
        serde_json::Value::Object(fields) => fields
            .iter_mut()
            .filter(|(k, _)| k.as_str() != "dns")
            .for_each(|(_, v)| absolute_paths(v, dir)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bundled_runtime_paths_resolve_against_resources_and_registrations_win() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("runtime.json"),
            r#"{"backend":"../MacOS/vm-apple","vz_desktop_stream":"../MacOS/vz-desktop-stream","dns":["1.1.1.1"],
               "qemu":{"worker":"../MacOS/vm-qemu","launcher":"runtime/qemu/probe-loader","library":"runtime/qemu/Frameworks/q","resources":"runtime/qemu/share"}}"#,
        )
        .unwrap();
        let bundled = Runtime::resolved(dir.path()).unwrap().unwrap();
        assert_eq!(bundled.backend, Some(dir.path().join("../MacOS/vm-apple")));
        assert_eq!(
            bundled.qemu.as_ref().unwrap().resources,
            dir.path().join("runtime/qemu/share")
        );
        assert_eq!(
            bundled.dns,
            ["1.1.1.1".parse::<std::net::IpAddr>().unwrap()]
        );
        let mut registered = Runtime {
            backend: Some("/dev/vm-apple".into()),
            ..Default::default()
        };
        let mut notes = Vec::new();
        registered.fill_from(bundled, &mut notes);
        assert_eq!(registered.backend, Some(PathBuf::from("/dev/vm-apple")));
        assert!(notes.is_empty(), "{notes:?}");
        assert!(registered.qemu.is_some() && registered.vz_desktop_stream.is_some());
        assert!(Runtime::resolved(&dir.path().join("none"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_registered_path_that_is_gone_gives_way_to_the_bundled_one() {
        let dir = tempfile::tempdir().unwrap();
        let shipped = dir.path().join("vm-apple");
        fs::write(&shipped, b"").unwrap();
        let share = dir.path().join("share");
        fs::create_dir(&share).unwrap();
        let file = |name: &str| {
            let path = dir.path().join(name);
            fs::write(&path, b"").unwrap();
            path
        };
        let qemu = QemuPaths {
            worker: file("vm-qemu"),
            launcher: file("launcher"),
            library: file("qemu"),
            resources: share.clone(),
            console_helper: Some(file("console")),
            ..Default::default()
        };
        let bundled = Runtime {
            backend: Some(shipped.clone()),
            vision: Some(shipped.clone()),
            qemu: Some(qemu.clone()),
            ..Default::default()
        };
        // A development checkout's paths, removed since.
        let gone = PathBuf::from("/nonexistent/dev/vm-apple");
        let mut registered = Runtime {
            backend: Some(gone.clone()),
            vision: Some(gone.clone()),
            qemu: Some(QemuPaths {
                worker: gone.clone(),
                ..qemu.clone()
            }),
            ..Default::default()
        };
        let mut notes = Vec::new();
        registered.fill_from(bundled.clone(), &mut notes);
        assert_eq!(registered.backend, Some(shipped.clone()));
        assert_eq!(registered.vision, Some(shipped.clone()));
        assert_eq!(registered.qemu.unwrap().worker, qemu.worker);
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes[0].contains("backend") && notes[0].contains("/nonexistent/dev/vm-apple"));

        // QEMU itself is there; only an optional helper is gone.
        let mut registered = Runtime {
            qemu: Some(QemuPaths {
                console_helper: Some(gone.clone()),
                ..qemu.clone()
            }),
            ..Default::default()
        };
        let mut notes = Vec::new();
        registered.fill_from(bundled, &mut notes);
        assert_eq!(registered.qemu.unwrap().console_helper, qemu.console_helper);
        assert_eq!(notes.len(), 1, "{notes:?}");
    }

    #[test]
    fn relative_runtime_paths_are_rejected() {
        let runtime: Runtime = serde_json::from_value(serde_json::json!({
            "backend": "vm-apple"
        }))
        .unwrap();
        assert!(runtime.validate().is_err());
        let runtime: Runtime = serde_json::from_value(serde_json::json!({
            "backend": "/opt/vm-apple",
            "boot": {"kernel": "/k", "initrd": "/i", "modloop": "/m"}
        }))
        .unwrap();
        runtime.validate().unwrap();
    }
    #[test]
    fn unknown_runtime_fields_are_rejected() {
        assert!(serde_json::from_value::<Runtime>(serde_json::json!({"backnd": "/x"})).is_err());
    }
}
