//! Trusted host registration is separate from remotely supplied VM configuration.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{path::PathBuf, process::Command};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    #[default]
    AppleVirtualization,
    QemuHvf,
}
#[derive(Clone, Debug)]
pub struct QemuRegistration {
    pub worker: PathBuf,
    pub launcher: PathBuf,
    pub library: PathBuf,
    pub resources: PathBuf,
    pub spice_library: Option<PathBuf>,
    pub screenshot_helper: Option<PathBuf>,
    pub console_broker: Option<PathBuf>,
    pub console_helper: Option<PathBuf>,
    pub venus_runtime: Option<PathBuf>,
}
impl QemuRegistration {
    pub fn has_screenshot(&self) -> bool {
        self.console_broker.is_some() || self.screenshot_helper.is_some()
    }
    pub fn validate(&self) -> Result<()> {
        if self.console_broker.is_some() != self.console_helper.is_some() {
            bail!("QEMU console broker and native helper must be registered together")
        }
        for path in [&self.worker, &self.launcher, &self.library]
            .into_iter()
            .chain(self.spice_library.iter())
            .chain(self.screenshot_helper.iter())
            .chain(self.console_broker.iter())
            .chain(self.console_helper.iter())
        {
            if !path.is_absolute() || !path.is_file() {
                bail!("QEMU runtime paths must be existing absolute files")
            }
        }
        if !self.resources.is_absolute() || !self.resources.is_dir() {
            bail!("QEMU resources must be an existing absolute directory")
        }
        if self.venus_runtime.is_some() {
            self.require_profile(crate::qemu_graphics::Profile::VenusMoltenvkExperimental)?;
        }
        Ok(())
    }
    pub fn require_profile(&self, profile: crate::qemu_graphics::Profile) -> Result<()> {
        if profile.venus() {
            let path = self
                .venus_runtime
                .as_deref()
                .context("Venus runtime not registered by local daemon operator")?;
            crate::qemu_graphics::Registration::load(path)?.validate(
                &self.library,
                &self.resources,
                self.spice_library.as_deref(),
            )?;
            crate::qemu_graphics::host_admission()?;
        }
        Ok(())
    }
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.worker);
        command
            .args(["run", "--launcher"])
            .arg(&self.launcher)
            .arg("--library")
            .arg(&self.library)
            .arg("--resources")
            .arg(&self.resources);
        if let Some(path) = &self.spice_library {
            command.arg("--spice-library").arg(path);
        }
        if let Some(path) = &self.screenshot_helper {
            command.arg("--screenshot-helper").arg(path);
        }
        if let Some(path) = &self.console_broker {
            command.arg("--console-broker").arg(path);
        }
        if let Some(path) = &self.console_helper {
            command.arg("--console-helper").arg(path);
        }
        if let Some(path) = &self.venus_runtime {
            command.arg("--venus-runtime").arg(path);
        }
        crate::qemu_graphics::clean_environment(&mut command);
        command
    }
}
pub fn capabilities(backend: Backend) -> Value {
    match backend {
        Backend::AppleVirtualization => {
            json!({"guest_os":["linux","macos"],"lifecycle":true,"native_window":true,"window_enumeration":true,"window_enumeration_requirements":"macOS Guest --allow-observe with existing Screen Recording permission and eligible console; no titles or input authority; query vm_guest_info capabilities.windows","screenshot":true,"screenshot_requirements":"macOS Guest service --allow-observe with existing Guest Screen Recording permission; Apple Linux screenshot unsupported","input":true,"input_requirements":"macOS Guest --allow-observe --allow-input with existing Screen Recording and Accessibility permissions; Apple Linux input unsupported","guest_integration":"enabled Linux or macOS Guest service over host-only vsock; query vm_guest_info for actual availability","command_execution":true,"command_execution_requirements":"MCP requires enabled Linux async jobs for cancellation; macOS service legacy exec is not exposed by current MCP execution API","gpu":"requires actual Guest measurements"})
        }
        Backend::QemuHvf => {
            json!({"guest_os":["linux"],"lifecycle":true,"native_window":false,"window_enumeration":false,"screenshot":false,"input":false,"guest_integration":"observer-only private virtio-serial; requires enabled Linux service, query vm_guest_info for actual readiness; no reconnect in this worker incarnation","command_execution":false,"file_transfer":false,"guest_ready":false,"normal_shutdown":"QMP power button request only; requires Guest support; acknowledgement is not completion","storage":"Linux direct kernel boot with optional private pinned raw disk; inherited device/inode sidecar lock and QEMU locking, APFS cloning is an independent image operation","initial_display_dimensions":{"supported":true,"default_width":1280,"default_height":800,"maximum_width":3840,"maximum_height":2160,"paired_even_dimensions_required":true,"semantics":"initial EDID preference; actual Guest mode must be measured"},"dynamic_resolution":false,"gpu":"experimental virtio-gpu-gl / virgl / ANGLE Metal; SPICE connection does not establish acceleration or release quality"})
        }
    }
}
pub fn guest_capabilities(backend: Backend, guest: &str) -> Value {
    let mut value = capabilities(backend);
    if backend == Backend::AppleVirtualization {
        value["screenshot"] = json!(guest == "macos");
        value["window_enumeration"] = json!(guest == "macos");
        value["input"] = json!(guest == "macos");
        if guest == "macos" {
            value["input_requirements"] = json!("Guest service --allow-observe --allow-input with existing Guest Screen Recording and Accessibility permissions; fresh screenshot anchor and current control lease required; vm_guest_info reports actual permission availability");
            value["text_input"] = json!(false);
        }
        value["command_execution"] = json!(guest == "linux");
        value["async_commands"] = json!(guest == "linux");
        value["guest_integration"]=json!(format!("requires enabled {guest} Guest vsock service; vm_guest_info reports current permissions and capabilities"));
    }
    value
}
pub fn require_registration(registration: &Option<QemuRegistration>) -> Result<&QemuRegistration> {
    registration
        .as_ref()
        .context("QEMU/HVF backend not registered by the local daemon operator")
}

/// Capabilities are a property of the trusted native helper, not of QEMU itself.
pub fn window_supported(value: &Value) -> bool {
    value["native_window"] == true && validate_window_capabilities(value.clone()).is_ok()
}
pub fn validate_window_capabilities(value: Value) -> Result<Value> {
    let mode = match value["native_window_protocol"].as_u64() {
        Some(1) => value["display_only"] == true && value["input_enabled"] == false,
        Some(2) => {
            value["native_input_protocol"] == 1
                && value["native_input_supported"] == true
                && value["native_input_available"].is_boolean()
                && value["input_enabled"].is_boolean()
                && value["display_only"].as_bool() == value["input_enabled"].as_bool().map(|v| !v)
                && (value["input_enabled"] != true || value["native_input_available"] == true)
        }
        _ => false,
    };
    if !value["native_window"].is_boolean() || !mode {
        bail!("invalid native window capability contract")
    }
    validate_pacing_capabilities(&value)?;
    Ok(value)
}
/// Limits describe configuration ceilings; none are measurements or guarantees.
pub fn validate_pacing_capabilities(value: &Value) -> Result<()> {
    let cap = |key: &str, max: u64| -> Result<u64> {
        let n = value[key]
            .as_u64()
            .context("pacing limit must be an unsigned integer")?;
        if n == 0 || n > max {
            bail!("pacing limit outside supported bound")
        }
        Ok(n)
    };
    if value.get("capture_limit_fps").is_some() {
        cap("capture_limit_fps", 60)?;
    }
    let extended = [
        "native_window_capture_limit_fps",
        "background_capture_limit_fps",
        "encoder_limit_fps",
        "idle_encode_interval_ms",
        "display_refresh_preference_hz",
    ];
    if extended.iter().any(|key| value.get(key).is_some()) {
        let capture = cap("capture_limit_fps", 60)?;
        let window = cap("native_window_capture_limit_fps", 60)?;
        let background = cap("background_capture_limit_fps", 60)?;
        let encoder = cap("encoder_limit_fps", 60)?;
        cap("idle_encode_interval_ms", 10000)?;
        cap("display_refresh_preference_hz", 240)?;
        if window > capture || background > window || encoder > background {
            bail!("inconsistent capture/encoder limits")
        }
        if let Some(current) = value.get("capture_current_limit_fps") {
            let current = current
                .as_u64()
                .context("current capture limit must be an unsigned integer")?;
            if current != background && current != window {
                bail!("current capture limit does not match a configured state")
            }
        }
    } else if value.get("capture_current_limit_fps").is_some() {
        bail!("current pacing limit has no capability contract")
    }
    Ok(())
}
pub fn qemu_display_size(width: Option<u32>, height: Option<u32>) -> Result<Option<(u32, u32)>> {
    match (width, height) {
        (None, None) => Ok(None),
        (Some(w), Some(h))
            if (320..=3840).contains(&w)
                && (320..=2160).contains(&h)
                && w % 2 == 0
                && h % 2 == 0 => Ok(Some((w, h))),
        _ => bail!("QEMU initial display requires paired even width 320..3840 and height 320..2160; dynamic resizing unsupported"),
    }
}
pub fn qemu_graphics_device(width: Option<u32>, height: Option<u32>) -> Result<String> {
    Ok(match qemu_display_size(width, height)? {
        None => "virtio-gpu-gl-pci".into(),
        Some((w, h)) => format!("virtio-gpu-gl-pci,xres={w},yres={h}"),
    })
}
/// Existing pre-window helpers used this exact usage/exit pair. A crash, timeout
/// or malformed successful response must not be downgraded to legacy support.
pub fn probe_native_window(helper: &std::path::Path) -> Result<Value> {
    let (status, stdout, stderr) = probe_process(helper, "--probe-capabilities")?;
    if status.code() == Some(2)
        && stdout.is_empty()
        && stderr == b"Usage: spice-console SOCKET --persistent\n"
    {
        return Ok(
            json!({"native_window_protocol":1,"native_window":false,"display_only":true,"input_enabled":false,"legacy_helper":true}),
        );
    }
    if !status.success() {
        bail!("native window capability probe failed")
    }
    validate_window_capabilities(
        serde_json::from_slice(&stdout).context("invalid native window capability JSON")?,
    )
}
pub fn probe_broker_window(broker: &std::path::Path, helper: &std::path::Path) -> Result<Value> {
    let helper = helper
        .to_str()
        .context("native helper path must be UTF-8")?;
    let (status, stdout, stderr) = probe_process_args(broker, &["--probe-capabilities", helper])?;
    if status.code() == Some(1)
        && stdout.is_empty()
        && stderr
            == b"vm-console: usage: vm-console NATIVE_ENCODER SPICE_SOCKET PRIVATE_CONSOLE_SOCKET\n"
    {
        return Ok(
            json!({"native_window":false,"native_window_protocol":1,"display_only":true,"input_enabled":false,"legacy_broker":true}),
        );
    }
    if !status.success() {
        bail!("console window capability probe failed")
    }
    let value = validate_window_capabilities(
        serde_json::from_slice(&stdout).context("invalid console capability JSON")?,
    )?;
    if value["native_input_supported"] == true && value["broker_native_input_protocol"] != 1 {
        bail!("broker does not attest native input protocol")
    }
    if value["broker_window_protocol"] != 1 {
        bail!("console window protocol was not verified")
    }
    Ok(value)
}
pub fn probe_process(
    helper: &std::path::Path,
    argument: &str,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    probe_process_args(helper, &[argument])
}
fn probe_process_args(
    helper: &std::path::Path,
    arguments: &[&str],
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    use std::{
        io::Read,
        os::fd::AsRawFd,
        process::Stdio,
        time::{Duration, Instant},
    };
    struct Owned(std::process::Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
    let mut child = Owned(
        Command::new(helper)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let mut out = child.0.stdout.take().unwrap();
    let mut err = child.0.stderr.take().unwrap();
    for fd in [out.as_raw_fd(), err.as_raw_fd()] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    fn drain(reader: &mut impl Read, bytes: &mut Vec<u8>, limit: usize) -> Result<bool> {
        let mut buffer = [0u8; 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return Ok(true),
                Ok(n) => {
                    if bytes.len() + n > limit {
                        bail!("native capability output exceeds bound")
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        let out_end = drain(&mut out, &mut stdout, 8192)?;
        let err_end = drain(&mut err, &mut stderr, 2048)?;
        if let Some(status) = child.0.try_wait()? {
            if out_end && err_end {
                return Ok((status, stdout, stderr));
            }
        }
        if Instant::now() >= deadline {
            bail!("native capability probe deadline")
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    fn helper(script: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe.py");
        fs::write(&path, format!("#!/usr/bin/env python3\n{script}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        (dir, path)
    }
    #[test]
    fn actual_pure_probe_validates_window_independently_of_codecs() {
        let (_dir,path)=helper("import sys,json; assert sys.argv[1:] == ['--probe-capabilities']; print(json.dumps({'native_window':True,'native_window_protocol':1,'display_only':True,'input_enabled':False}))");
        let value = probe_native_window(&path).unwrap();
        assert!(window_supported(&value));
        assert!(value.get("h264").is_none());
    }
    #[test]
    fn only_known_legacy_usage_is_unsupported_not_arbitrary_failure() {
        let (_dir,path)=helper("import sys; sys.stderr.write('Usage: spice-console SOCKET --persistent\\n'); sys.exit(2)");
        assert!(!window_supported(&probe_native_window(&path).unwrap()));
        for script in [
            "import sys; sys.exit(2)",
            "import sys; sys.stderr.write('crashed'); sys.exit(1)",
            "print('{}')",
            "print('x'*8193)",
            "print('{bad json}')",
        ] {
            let (_dir, path) = helper(script);
            assert!(probe_native_window(&path).is_err());
        }
    }
    #[test]
    fn broker_adapter_must_attest_its_own_window_protocol() {
        let (_dir,legacy)=helper("import sys; sys.stderr.write('vm-console: usage: vm-console NATIVE_ENCODER SPICE_SOCKET PRIVATE_CONSOLE_SOCKET\\n'); sys.exit(1)");
        assert!(!window_supported(
            &probe_broker_window(&legacy, std::path::Path::new("/unused/helper")).unwrap()
        ));
        let (_dir,incomplete)=helper("print('{\"native_window\":true,\"native_window_protocol\":1,\"display_only\":true,\"input_enabled\":false}')");
        assert!(probe_broker_window(&incomplete, std::path::Path::new("/unused/helper")).is_err());
    }
    #[test]
    fn pacing_limits_are_typed_consistent_ceilings_not_observed_fps() {
        let caps = json!({"capture_limit_fps":60,"native_window_capture_limit_fps":60,"background_capture_limit_fps":30,"encoder_limit_fps":30,"idle_encode_interval_ms":1000,"display_refresh_preference_hz":60,"capture_current_limit_fps":60});
        assert!(validate_pacing_capabilities(&caps).is_ok());
        assert!(validate_pacing_capabilities(&json!({"capture_limit_fps":30})).is_ok());
        for (field, value) in [
            ("encoder_limit_fps", json!(true)),
            ("encoder_limit_fps", json!(30.0)),
            ("encoder_limit_fps", json!(60)),
            ("capture_current_limit_fps", json!(45)),
            ("background_capture_limit_fps", json!(0)),
            ("capture_limit_fps", json!(30)),
        ] {
            let mut invalid = caps.clone();
            invalid[field] = value;
            assert!(validate_pacing_capabilities(&invalid).is_err(), "{field}");
        }
        let mut partial = caps.clone();
        partial
            .as_object_mut()
            .unwrap()
            .remove("idle_encode_interval_ms");
        assert!(validate_pacing_capabilities(&partial).is_err());
    }
    #[test]
    fn initial_qemu_dimensions_are_paired_even_bounded_and_not_dynamic() {
        assert_eq!(
            qemu_graphics_device(None, None).unwrap(),
            "virtio-gpu-gl-pci"
        );
        assert_eq!(
            qemu_graphics_device(Some(2560), Some(1440)).unwrap(),
            "virtio-gpu-gl-pci,xres=2560,yres=1440"
        );
        assert!(qemu_display_size(Some(3840), Some(2160)).is_ok());
        for (w, h) in [
            (Some(2560), None),
            (None, Some(1440)),
            (Some(2561), Some(1440)),
            (Some(2560), Some(1441)),
            (Some(3842), Some(2160)),
            (Some(3840), Some(2162)),
            (Some(0), Some(1440)),
        ] {
            assert!(qemu_display_size(w, h).is_err());
        }
    }
    #[test]
    fn window_contract_rejects_implicit_input_or_unknown_protocol() {
        let valid = json!({"native_window":true,"native_window_protocol":1,"display_only":true,"input_enabled":false});
        for (field, value) in [
            ("native_window_protocol", json!(2)),
            ("input_enabled", json!(true)),
            ("display_only", json!(false)),
            ("native_window", json!("true")),
        ] {
            let mut bad = valid.clone();
            bad[field] = value;
            assert!(validate_window_capabilities(bad.clone()).is_err());
            assert!(!window_supported(&bad));
        }
    }
    #[test]
    fn native_v2_support_is_separate_from_current_input_authority() {
        let mut v = json!({"native_window":true,"native_window_protocol":2,"native_input_protocol":1,"native_input_supported":true,"native_input_available":false,"input_enabled":false,"display_only":true});
        assert!(window_supported(&v));
        v["input_enabled"] = json!(true);
        v["display_only"] = json!(false);
        assert!(!window_supported(&v));
        v["native_input_available"] = json!(true);
        assert!(window_supported(&v));
        v["native_input_protocol"] = json!(2);
        assert!(!window_supported(&v));
    }
}
