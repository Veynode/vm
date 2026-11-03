//! Explicit QEMU display profiles and local-only experimental Venus admission.
//! Config contains a profile enum, never executable paths or environment overrides.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    #[default]
    Virgl,
    VenusMoltenvkExperimental,
    /// Windows D3D through UTM 5.0.8's Neptune (virglrenderer) and DXMT
    /// (`dxmt-native.framework` in the registered UTM runtime); the guest needs
    /// UTM's viogpu3d driver (utm-guest-tools 0.1.274).
    NeptuneDxmtExperimental,
    /// Non-GL 2D display; full-state save/restore still requires actual validation.
    #[serde(rename = "virtio-2d")]
    Virtio2d,
}
impl Profile {
    pub fn is_default(&self) -> bool {
        *self == Self::Virgl
    }
    pub fn venus(self) -> bool {
        self == Self::VenusMoltenvkExperimental
    }
    pub fn neptune(self) -> bool {
        self == Self::NeptuneDxmtExperimental
    }

    pub fn requires_gl(self) -> bool {
        self != Self::Virtio2d
    }
    pub fn validate(self, backend: crate::backend::Backend, guest: &str) -> Result<()> {
        if self.neptune() && (backend != crate::backend::Backend::QemuHvf || guest != "windows") {
            bail!("Neptune profile requires a Windows QEMU/HVF guest")
        }
        if self.venus() && (backend != crate::backend::Backend::QemuHvf || guest != "linux") {
            bail!("Venus profile requires Linux QEMU/HVF")
        }
        if self == Self::Virtio2d
            && (backend != crate::backend::Backend::QemuHvf || guest != "linux")
        {
            bail!("virtio-2d profile requires Linux QEMU/HVF")
        }
        Ok(())
    }
    pub fn machine_arguments(self) -> &'static [&'static str] {
        // Neptune shares the device with Venus, as in UTM's default Triton setup.
        if self.venus() || self.neptune() {
            &["-machine", "virt", "-accel", "hvf,ipa-granule-size=4096"]
        } else {
            &["-machine", "virt,accel=hvf"]
        }
    }
    pub fn device(self, width: Option<u32>, height: Option<u32>) -> Result<String> {
        if self == Self::Virtio2d {
            return crate::backend::qemu_2d_graphics_device(width, height);
        }
        let mut value = crate::backend::qemu_graphics_device(width, height)?;
        if self.venus() {
            value.push_str(",id=veynode-gpu,blob=on,hostmem=256M,venus=on");
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub schema: u32,
    pub profile_revision: String,
    pub frameworks_dir: PathBuf,
    pub loader: Artifact,
    pub host_library: Artifact,
    pub render_server: Artifact,
    pub qemu_library: Artifact,
    pub spice_library: Artifact,
    pub icd: Artifact,
    pub firmware: Artifact,
}
#[derive(Deserialize)]
struct Catalog {
    schema: u32,
    profile_revision: String,
    roles: BTreeMap<String, String>,
    frameworks: BTreeMap<String, String>,
    // Compiled admission policy only; never supplied by a runtime registration.
    #[serde(default)]
    source_qemu_library: Option<PathBuf>,
}
fn qemu_dependency_layout(approved: &Catalog, frameworks: &Path, qemu: &Path) -> Result<()> {
    if let Some(expected) = &approved.source_qemu_library {
        if !expected.is_absolute()
            || expected.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
            || !expected
                .ends_with("VeynodeQEMUDevelopment.framework/Versions/A/VeynodeQEMUDevelopment")
            || qemu != expected
        {
            bail!("compiled source QEMU dependency layout mismatch")
        }
    } else if frameworks.join("qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu")
        != qemu
    {
        bail!("QEMU dependency layout mismatch")
    }
    Ok(())
}

fn catalog() -> Catalog {
    serde_json::from_str(include_str!("qemu_venus_catalog.json")).expect("compiled Venus catalog")
}

/// Where a release package keeps its QEMU/GPU runtime, relative to the
/// package (scripts/package-release.py).
const BUNDLED_RUNTIME: &str = "libexec/qemu";

/// The release package this binary runs from: `bin/..`.
fn own_package() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    Some(exe.parent()?.parent()?.to_path_buf())
}

#[derive(Deserialize)]
struct PackageManifest {
    files: Vec<PackageFile>,
}
#[derive(Deserialize)]
struct PackageFile {
    path: PathBuf,
    sha256: String,
}

impl Registration {
    /// The package's own GPU runtime is built and signed with the package, so
    /// no compiled hash can name it: its MANIFEST.json, beside the binary
    /// validating it, pins every role and every file under Frameworks
    /// instead. A role outside the package has no entry and is refused.
    fn bundled_catalog(&self, package: &Path) -> Result<Catalog> {
        let mut bytes = Vec::new();
        safe_file(&package.join("MANIFEST.json"), 16 * 1024 * 1024, false)?
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        let manifest: PackageManifest =
            serde_json::from_slice(&bytes).context("invalid package MANIFEST.json")?;
        let hashes: BTreeMap<PathBuf, String> = manifest
            .files
            .into_iter()
            .map(|f| (package.join(f.path), f.sha256))
            .collect();
        let compiled = catalog();
        let mut roles = BTreeMap::new();
        for (name, artifact) in [
            ("loader", &self.loader),
            ("host_library", &self.host_library),
            ("render_server", &self.render_server),
            ("qemu_library", &self.qemu_library),
            ("spice_library", &self.spice_library),
            ("icd", &self.icd),
            ("firmware", &self.firmware),
        ] {
            let sha = hashes
                .get(&artifact.path)
                .context("packaged GPU runtime file missing from MANIFEST.json")?;
            roles.insert(name.to_owned(), sha.clone());
        }
        // A framework's symlinks (Versions/Current, its top-level name) list
        // their target's bytes again; the target itself is hashed.
        let frameworks: BTreeMap<String, String> = hashes
            .iter()
            .filter_map(|(path, sha)| {
                let relative = path.strip_prefix(&self.frameworks_dir).ok()?;
                let alias = path.canonicalize().is_ok_and(|c| &c != path);
                (!alias).then(|| (relative.to_string_lossy().into_owned(), sha.clone()))
            })
            .collect();
        if frameworks.is_empty() {
            bail!("package MANIFEST.json lists no GPU runtime frameworks")
        }
        Ok(Catalog {
            schema: compiled.schema,
            profile_revision: compiled.profile_revision,
            roles,
            frameworks,
            source_qemu_library: None,
        })
    }
}

/// Observed host constraints for the exact compiled catalog, not live Guest API support.
/// A catalog replacement requires its own real query instead of inheriting this result.
pub fn host_api_constraints(profile: Profile) -> Value {
    if !profile.venus() {
        return json!({});
    }
    catalog_geometry_constraint(&catalog())
}

fn catalog_geometry_constraint(approved: &Catalog) -> Value {
    const DRIVER_SHA: &str = "ba5aaa1d356261cf3cb2f9d89cc8f1c9f1bd56adac5e85833d8083d4bbb0b26f";
    if approved
        .frameworks
        .get("MoltenVK.framework/Versions/A/MoltenVK")
        .map(String::as_str)
        != Some(DRIVER_SHA)
    {
        return json!({"geometry_shader":{"status":"unknown","reason":"this host driver has no matching actual capability-query evidence; do not inherit another binary's Geometry result","guest_verified":false}});
    }
    json!({"geometry_shader":{
        "status":"unsupported",
        "reason":"the exact registered host driver reports geometryShader=true with all five limits zero; its required capability contract is not accepted; Geometry workloads and Zink OpenGL 3.2+ are unverified",
        "driver_sha256":DRIVER_SHA,
        "reported_feature":true,
        "reported_limits":{"maxGeometryShaderInvocations":0,"maxGeometryInputComponents":0,"maxGeometryOutputComponents":0,"maxGeometryOutputVertices":0,"maxGeometryTotalOutputComponents":0},
        "guest_verified":false,
        "scope":"actual pinned host driver query only; not a per-VM Guest query, complete Vulkan conformance or performance result",
        "evidence":"native/tests/zink-catalog-capabilities-v1/README.md"
    }})
}
fn trusted_ancestors(path: &Path) -> Result<()> {
    for parent in path.ancestors().skip(1) {
        let m = fs::symlink_metadata(parent)?;
        let owner = m.uid() == unsafe { libc::geteuid() } || m.uid() == 0;
        let protected = m.mode() & 0o022 == 0 || (m.uid() == 0 && m.mode() & 0o1000 != 0);
        if !m.is_dir() || !owner || !protected {
            bail!("runtime directory is not locally trusted")
        }
    }
    Ok(())
}
fn safe_file(path: &Path, max: u64, private: bool) -> Result<File> {
    if !path.is_absolute() || path.canonicalize()? != path {
        bail!("runtime file must have canonical absolute path")
    }
    trusted_ancestors(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let m = file.metadata()?;
    if !m.is_file()
        || m.nlink() != 1
        || m.len() == 0
        || m.len() > max
        || (m.uid() != unsafe { libc::geteuid() } && m.uid() != 0)
        || m.mode() & if private { 0o077 } else { 0o022 } != 0
    {
        bail!("unsafe or oversized runtime file")
    }
    Ok(file)
}
fn hash_file(path: &Path, expected: &str) -> Result<()> {
    let mut file = safe_file(path, 128 * 1024 * 1024, false)?;
    let before = file.metadata()?;
    let mut hash = Sha256::new();
    let mut buf = [0; 65536];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > 128 * 1024 * 1024 {
            bail!("runtime hash bound")
        }
        hash.update(&buf[..n]);
    }
    let after = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    if total != before.len()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || (before.dev(), before.ino()) != (named.dev(), named.ino())
        || format!("{:x}", hash.finalize()) != expected
    {
        bail!("registered Venus runtime hash/identity mismatch")
    }
    Ok(())
}
impl Registration {
    pub fn load(path: &Path) -> Result<Self> {
        let file = safe_file(path, 64 * 1024, true)?;
        let mut bytes = Vec::new();
        file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 64 * 1024 {
            bail!("Venus registration size bound")
        }
        let mut value: Self =
            serde_json::from_slice(&bytes).context("invalid Venus registration")?;
        // A release package's registration names its files relative to itself.
        let dir = path
            .parent()
            .context("Venus registration has no directory")?;
        let resolve = |p: &mut PathBuf| -> Result<()> {
            if p.is_relative() {
                *p = dir
                    .join(&*p)
                    .canonicalize()
                    .with_context(|| format!("Venus runtime file {} missing", p.display()))?;
            }
            Ok(())
        };
        resolve(&mut value.frameworks_dir)?;
        for artifact in [
            &mut value.loader,
            &mut value.host_library,
            &mut value.render_server,
            &mut value.qemu_library,
            &mut value.spice_library,
            &mut value.icd,
            &mut value.firmware,
        ] {
            resolve(&mut artifact.path)?;
        }
        Ok(value)
    }
    pub fn validate(&self, qemu: &Path, resources: &Path, spice: Option<&Path>) -> Result<()> {
        self.validate_in(qemu, resources, spice, own_package().as_deref())
    }
    /// `package`: the release package this process runs from. Its own GPU
    /// runtime is admitted by the package's MANIFEST.json, any other by the
    /// compiled catalog.
    fn validate_in(
        &self,
        qemu: &Path,
        resources: &Path,
        spice: Option<&Path>,
        package: Option<&Path>,
    ) -> Result<()> {
        let package =
            package.filter(|p| self.frameworks_dir == p.join(BUNDLED_RUNTIME).join("Frameworks"));
        let approved = match package {
            Some(package) => self.bundled_catalog(package)?,
            None => catalog(),
        };
        if self.schema != approved.schema || self.profile_revision != approved.profile_revision {
            bail!("the Venus GPU runtime is a revision this daemon does not support")
        }
        let roles = [
            ("loader", &self.loader),
            ("host_library", &self.host_library),
            ("render_server", &self.render_server),
            ("qemu_library", &self.qemu_library),
            ("spice_library", &self.spice_library),
            ("icd", &self.icd),
            ("firmware", &self.firmware),
        ];
        for (name, artifact) in roles {
            // The manifest cannot authorize a substitute renderer or unpatched PGID owner.
            if approved.roles.get(name) != Some(&artifact.sha256) {
                bail!("the Venus GPU runtime's {name} is not the reviewed build")
            }
            hash_file(&artifact.path, &artifact.sha256)
                .with_context(|| format!("Venus {name} validation"))?;
        }
        if self.qemu_library.path != qemu
            || Some(self.spice_library.path.as_path()) != spice
            || resources.join("edk2-aarch64-code.fd").canonicalize()? != self.firmware.path
        {
            bail!("the Venus GPU runtime belongs to another QEMU runtime than the registered one")
        }
        if !self.frameworks_dir.is_absolute()
            || self.frameworks_dir.canonicalize()? != self.frameworks_dir
        {
            bail!("invalid Venus frameworks directory")
        }
        for (relative, expected) in &approved.frameworks {
            hash_file(&self.frameworks_dir.join(relative), expected)?;
        }
        qemu_dependency_layout(&approved, &self.frameworks_dir, &self.qemu_library.path)?;
        // Reviewed loader/server/host image have this fixed absolute LC_RPATH;
        // a package's have the run path from their directory beside Frameworks.
        // Their exact binary hashes above prevent a different Mach-O layout being admitted.
        let mut absolute = self.frameworks_dir.as_os_str().as_encoded_bytes().to_vec();
        absolute.push(0);
        let mut rpath_artifacts = vec![
            (&self.loader, "@executable_path"),
            (&self.host_library, "@loader_path"),
            (&self.render_server, "@executable_path"),
        ];
        if approved.source_qemu_library.is_some() {
            rpath_artifacts.push((&self.qemu_library, "@loader_path"));
        }
        for (artifact, origin) in rpath_artifacts {
            let rpath = if package.is_some() {
                if artifact.path.parent().and_then(Path::parent) != self.frameworks_dir.parent() {
                    bail!("packaged GPU runtime file outside its runtime directory")
                }
                format!("{origin}/../Frameworks\0").into_bytes()
            } else {
                absolute.clone()
            };
            let mut bytes = Vec::new();
            safe_file(&artifact.path, 128 * 1024 * 1024, false)?
                .take(128 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 128 * 1024 * 1024 {
                bail!("runtime rpath bound")
            }
            if !bytes.windows(rpath.len()).any(|b| b == rpath) {
                bail!("reviewed runtime rpath does not match registered frameworks")
            }
        }
        let icd: Value = serde_json::from_reader(safe_file(&self.icd.path, 8192, false)?)?;
        let relative = icd["ICD"]["library_path"]
            .as_str()
            .context("missing MoltenVK ICD library")?;
        if relative != "../../../Frameworks/MoltenVK.framework/Versions/Current/MoltenVK"
            || icd["ICD"]["is_portability_driver"] != true
            || self
                .icd
                .path
                .parent()
                .unwrap()
                .join(relative)
                .canonicalize()?
                != self
                    .frameworks_dir
                    .join("MoltenVK.framework/Versions/A/MoltenVK")
        {
            bail!("MoltenVK ICD/dependency layout mismatch")
        }
        Ok(())
    }
    pub fn configure(&self, command: &mut Command) {
        command
            .env("VEYNODE_VIRGL_LIBRARY", &self.host_library.path)
            .env("RENDER_SERVER_EXEC_PATH", &self.render_server.path)
            .env("VK_DRIVER_FILES", &self.icd.path);
    }
}

/// UTM's host side of Neptune, found relative to the registered QEMU framework
/// (`UTM.app/Contents/Frameworks/qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu`).
pub struct Neptune {
    /// DXMT, which Neptune loads for D3D11, D3D12 and DXGI.
    pub dxmt: PathBuf,
    /// virglrenderer's render server; without it virglrenderer falls back to
    /// its build-time path and fails with "failed to initialize proxy renderer".
    /// UTM's own `QEMURenderServer` is App-Sandbox-signed and traps at launch
    /// unless UTM's sandboxed QEMUHelper is its parent, so this is the reviewed
    /// Venus registration's server: built from the virglrenderer commit UTM
    /// 5.0.8 ships (451505ad, Neptune wire format 2) with `-Dneptune=true`.
    pub render_server: PathBuf,
    /// UTM's MoltenVK ICD for the Venus side of the device (its default
    /// Vulkan driver); the guest driver package also ships a Venus ICD.
    pub vulkan_icd: PathBuf,
}

impl Neptune {
    pub fn locate(qemu: &Path, registration: &Registration) -> Result<Self> {
        let frameworks = qemu
            .ancestors()
            .nth(4)
            .context("QEMU library is not inside a Frameworks directory")?;
        let contents = frameworks.parent().unwrap_or(frameworks);
        let neptune = Self {
            dxmt: frameworks.join("dxmt-native.framework/Versions/A/dxmt-native"),
            render_server: registration.render_server.path.clone(),
            vulkan_icd: contents.join("Resources/vulkan/icd.d/MoltenVK_icd.json"),
        };
        for path in [&neptune.dxmt, &neptune.render_server, &neptune.vulkan_icd] {
            if !path.is_file() {
                bail!(
                    "Neptune profile needs UTM's runtime around the QEMU framework: {} missing",
                    path.display()
                )
            }
        }
        Ok(neptune)
    }

    /// The environment UTM 5.0.8 gives QEMU for its DXMT DirectX driver
    /// (`Services/UTMQemuSystem.m`; QEMUHelper sets the render server).
    /// DXMT writes `<exe>_dxmt.log` (an empty exe name for QEMU) into `log_dir`
    /// instead of the worker's current directory; its messages also go to stderr.
    pub fn configure(&self, command: &mut Command, log_dir: &Path) {
        for key in [
            "NPT_D3D11_LIBRARY_PATH",
            "NPT_D3D12_LIBRARY_PATH",
            "NPT_DXGI_LIBRARY_PATH",
        ] {
            command.env(key, &self.dxmt);
        }
        command
            .env("NPT_BACKEND", "dxmt")
            .env("DXMT_LOG_PATH", log_dir)
            .env("RENDER_SERVER_EXEC_PATH", &self.render_server)
            .env("VK_DRIVER_FILES", &self.vulkan_icd);
    }
}

/// Clear loader/driver injection for every profile, before setting trusted SPICE and ANGLE.
/// No arbitrary env values are accepted via VM config or MCP.
pub fn clean_environment(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if blocked_environment_key(&key) {
            command.env_remove(key);
        }
    }
    let explicit: Vec<_> = command
        .get_envs()
        .filter(|(k, _)| blocked_environment_key(k))
        .map(|(k, _)| k.to_owned())
        .collect();
    for key in explicit {
        command.env_remove(key);
    }
    // Also remove known keys when absent in our environment but already set on Command.
    for key in [
        "RENDER_SERVER_EXEC_PATH",
        "VEYNODE_VIRGL_LIBRARY",
        "VEYNODE_SPICE_LIBRARY",
        "VK_DRIVER_FILES",
        "VK_ICD_FILENAMES",
        "VK_ADD_DRIVER_FILES",
        "ANGLE_DEFAULT_PLATFORM",
    ] {
        command.env_remove(key);
    }
}
fn blocked_environment_key(key: &OsStr) -> bool {
    let key = key.as_encoded_bytes();
    [
        b"DYLD_".as_slice(),
        b"VK_",
        b"MVK_",
        b"MESA_",
        b"VIRGL_",
        b"ANGLE_",
        b"NPT_",
        b"D3DMETAL_",
    ]
    .iter()
    .any(|p| key.starts_with(p))
        || [
            b"RENDER_SERVER_EXEC_PATH".as_slice(),
            b"VEYNODE_VIRGL_LIBRARY",
            b"VEYNODE_SPICE_LIBRARY",
            b"GALLIUM_DRIVER",
            b"LIBGL_ALWAYS_SOFTWARE",
            b"DRI_PRIME",
        ]
        .contains(&key)
}

/// No VM is created here. This proves only public host configuration support.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn host_admission() -> Result<Value> {
    let mut version = [0u8; 128];
    let mut len = version.len();
    if unsafe {
        libc::sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            version.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        bail!("macOS version query failed")
    }
    if len > version.len() {
        bail!("macOS version size bound")
    }
    let text = std::str::from_utf8(&version[..len])
        .context("invalid macOS version")?
        .trim_end_matches('\0');
    if text
        .split('.')
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .is_none_or(|v| v < 26)
    {
        bail!("Venus profile requires macOS 26 or newer")
    }
    unsafe {
        let handle = libc::dlopen(
            c"/System/Library/Frameworks/Hypervisor.framework/Hypervisor".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        );
        if handle.is_null() {
            bail!("Hypervisor framework unavailable")
        }
        struct Library(*mut libc::c_void);
        impl Drop for Library {
            fn drop(&mut self) {
                unsafe {
                    libc::dlclose(self.0);
                }
            }
        }
        let _library = Library(handle);
        let create = libc::dlsym(handle, c"hv_vm_config_create".as_ptr());
        let set = libc::dlsym(handle, c"hv_vm_config_set_ipa_granule".as_ptr());
        let get = libc::dlsym(handle, c"hv_vm_config_get_ipa_granule".as_ptr());
        let release = libc::dlsym(libc::RTLD_DEFAULT, c"os_release".as_ptr());
        if [create, set, get, release].iter().any(|p| p.is_null()) {
            bail!("public Hypervisor granule configuration API unavailable")
        }
        let create: unsafe extern "C" fn() -> *mut libc::c_void = std::mem::transmute(create);
        let set: unsafe extern "C" fn(*mut libc::c_void, u32) -> i32 = std::mem::transmute(set);
        let get: unsafe extern "C" fn(*mut libc::c_void, *mut u32) -> i32 =
            std::mem::transmute(get);
        let release: unsafe extern "C" fn(*mut libc::c_void) = std::mem::transmute(release);
        let config = create();
        if config.is_null() {
            bail!("Hypervisor configuration allocation failed")
        }
        let mut granule = u32::MAX;
        let set_rc = set(config, 0);
        let get_rc = get(config, &mut granule);
        release(config);
        if set_rc != 0 || get_rc != 0 || granule != 0 {
            bail!("public Hypervisor 4KiB configuration/readback unsupported")
        }
    }
    Ok(
        json!({"architecture":"aarch64","macos_version":text,"public_hv_config_4k_readback":true,"vm_created_by_probe":false}),
    )
}
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub fn host_admission() -> Result<Value> {
    bail!("Venus profile requires Apple Silicon macOS 26 or newer")
}

pub fn verify_gpu_properties(mut query: impl FnMut(&str, Value) -> Result<Value>) -> Result<()> {
    for (property, expected) in [
        ("venus", json!(true)),
        ("blob", json!(true)),
        ("hostmem", json!(268435456u64)),
    ] {
        let value = query(
            "qom-get",
            json!({"path":"/machine/peripheral/veynode-gpu","property":property}),
        )?;
        if value != expected {
            bail!("Venus pre-cont GPU property mismatch: {property}")
        }
    }
    let status = query("query-status", json!({}))?;
    if status["running"] != false
        || !["prelaunch", "paused"].contains(&status["status"].as_str().unwrap_or(""))
    {
        bail!("Venus admission requires paused pre-cont QEMU")
    }
    Ok(())
}

#[cfg(test)]
#[path = "qemu_graphics_tests.rs"]
mod tests;
