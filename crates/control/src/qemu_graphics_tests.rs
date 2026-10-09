use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};
fn artifact(path: PathBuf) -> Artifact {
    Artifact {
        path,
        sha256: "0".repeat(64),
    }
}
fn empty() -> Registration {
    Registration {
        schema: 1,
        profile_revision: "owned-group-v1".into(),
        frameworks_dir: "/abs".into(),
        loader: artifact("/abs/loader".into()),
        host_library: artifact("/abs/host".into()),
        render_server: artifact("/abs/server".into()),
        qemu_library: artifact("/abs/qemu".into()),
        spice_library: artifact("/abs/spice".into()),
        icd: artifact("/abs/icd".into()),
        firmware: artifact("/abs/firmware".into()),
    }
}
#[test]
fn config_profile_is_strict_local_paths_never_in_vm_config() {
    let raw = json!({"id":"fixture","backend":"qemu-hvf","guest":"linux","cpus":2,"memory_mib":1024,"kernel":"/dev/null"});
    let default: crate::Config = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(default.qemu_graphics_profile, Profile::Virgl);
    assert!(serde_json::to_value(default)
        .unwrap()
        .get("qemu_graphics_profile")
        .is_none());
    let mut venus = raw;
    venus["qemu_graphics_profile"] = json!("venus-moltenvk-experimental");
    let config: crate::Config = serde_json::from_value(venus.clone()).unwrap();
    assert!(config
        .qemu_graphics_profile
        .validate(crate::backend::Backend::QemuHvf, "linux")
        .is_ok());
    assert!(config
        .qemu_graphics_profile
        .validate(crate::backend::Backend::AppleVirtualization, "linux")
        .is_err());
    assert!(config
        .qemu_graphics_profile
        .validate(crate::backend::Backend::QemuHvf, "macos")
        .is_err());
    venus["qemu_graphics_profile"] = json!("venus");
    assert!(serde_json::from_value::<crate::Config>(venus.clone()).is_err());
    venus["qemu_graphics_profile"] = json!("virgl");
    venus["render_server"] = json!("/injected");
    assert!(serde_json::from_value::<crate::Config>(venus).is_err());
}
#[test]
fn argv_preserves_default_and_exact_venus_granule_device() {
    assert_eq!(
        Profile::Virgl.machine_arguments(),
        ["-machine", "virt,accel=hvf"]
    );
    assert_eq!(
        Profile::Virgl.device(None, None).unwrap(),
        "virtio-gpu-gl-pci"
    );
    assert_eq!(
        Profile::VenusMoltenvkExperimental.machine_arguments(),
        ["-machine", "virt", "-accel", "hvf,ipa-granule-size=4096"]
    );
    assert_eq!(
        Profile::VenusMoltenvkExperimental
            .device(Some(1280), Some(720))
            .unwrap(),
        "virtio-gpu-gl-pci,xres=1280,yres=720,id=veynode-gpu,blob=on,hostmem=256M,venus=on"
    );
    assert!(Profile::VenusMoltenvkExperimental
        .device(Some(1279), Some(720))
        .is_err());
}
#[test]
fn registered_environment_replaces_injection_without_changing_spice() {
    let mut c = Command::new("/unused");
    for key in [
        "DYLD_INSERT_LIBRARIES",
        "VK_LAYER_PATH",
        "VK_ICD_FILENAMES",
        "MVK_CONFIG_DEBUG",
        "VIRGL_RENDER_SERVER",
        "ANGLE_DEFAULT_PLATFORM",
        "RENDER_SERVER_EXEC_PATH",
        "VEYNODE_SPICE_LIBRARY",
        "VEYNODE_VIRGL_LIBRARY",
    ] {
        c.env(key, "untrusted");
    }
    c.env("UNCHANGED_SAFE_FIXTURE", "yes");
    clean_environment(&mut c);
    empty().configure(&mut c);
    c.env("VEYNODE_SPICE_LIBRARY", "/registered/spice")
        .env("ANGLE_DEFAULT_PLATFORM", "metal");
    let env: BTreeMap<_, _> = c
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    assert_eq!(env["VK_DRIVER_FILES"], Some("/abs/icd".into()));
    assert_eq!(env["RENDER_SERVER_EXEC_PATH"], Some("/abs/server".into()));
    assert_eq!(
        env["VEYNODE_SPICE_LIBRARY"],
        Some("/registered/spice".into())
    );
    assert_eq!(env["ANGLE_DEFAULT_PLATFORM"], Some("metal".into()));
    for key in [
        "DYLD_INSERT_LIBRARIES",
        "VK_LAYER_PATH",
        "VK_ICD_FILENAMES",
        "MVK_CONFIG_DEBUG",
    ] {
        assert_eq!(env[key], None);
    }
    assert_eq!(env["UNCHANGED_SAFE_FIXTURE"], Some("yes".into()));
    for key in [
        "DYLD_LIBRARY_PATH",
        "VK_ADD_DRIVER_FILES",
        "MESA_LOADER_DRIVER_OVERRIDE",
        "GALLIUM_DRIVER",
    ] {
        assert!(blocked_environment_key(OsStr::new(key)));
    }
}
#[test]
fn private_manifest_unknown_duplicate_and_unreviewed_artifacts_reject() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().canonicalize().unwrap();
    let path = dir.join("registration.json");
    let mut value = serde_json::to_value(empty()).unwrap();
    value["environment"] = json!({});
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(Registration::load(&path).is_err());
    fs::write(&path, b"{\"schema\":1,\"schema\":1}").unwrap();
    assert!(Registration::load(&path).is_err());
    fs::write(&path, serde_json::to_vec(&empty()).unwrap()).unwrap();
    let value = Registration::load(&path).unwrap();
    assert!(value
        .validate(
            Path::new("/abs/qemu"),
            Path::new("/abs"),
            Some(Path::new("/abs/spice"))
        )
        .unwrap_err()
        .to_string()
        .contains("unreviewed"));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(Registration::load(&path).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let link = dir.join("alias");
    symlink(&path, &link).unwrap();
    assert!(Registration::load(&link).is_err());
    fs::write(&path, vec![0; 65537]).unwrap();
    assert!(Registration::load(&path).is_err());
}
#[test]
fn artifact_actual_file_modification_link_and_truncation_fail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("runtime");
    fs::write(&path, b"approved artifact").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let expected = format!("{:x}", Sha256::digest(b"approved artifact"));
    hash_file(&path, &expected).unwrap();
    fs::write(&path, b"different artifact").unwrap();
    assert!(hash_file(&path, &expected).is_err());
    fs::write(&path, b"").unwrap();
    assert!(hash_file(&path, &expected).is_err());
    fs::write(&path, b"approved artifact").unwrap();
    let alias = path.with_file_name("hardlink");
    fs::hard_link(&path, &alias).unwrap();
    assert!(hash_file(&path, &expected).is_err());
}
#[test]
fn qmp_gpu_type_value_and_precont_status_strict() {
    for bad in ["none", "venus", "blob", "hostmem", "status"] {
        let result = verify_gpu_properties(|method, args| {
            Ok(if method == "query-status" {
                json!({"running":bad=="status","status":if bad=="status"{"running"}else{"prelaunch"}})
            } else {
                let p = args["property"].as_str().unwrap();
                if p == bad {
                    json!(1)
                } else if p == "hostmem" {
                    json!(268435456)
                } else {
                    json!(true)
                }
            })
        });
        assert_eq!(result.is_ok(), bad == "none");
    }
    assert!(verify_gpu_properties(|_, _| bail!("unsupported QOM property")).is_err());
}
#[test]
fn catalog_pins_actual_owned_group_not_manifest_attestation() {
    let c = catalog();
    assert_eq!(
        c.roles["host_library"],
        "471cffbff498c5048d54246ee1efd106455a8c1a5d0613bd391f195cb0abc1b3"
    );
    assert_eq!(
        c.roles["loader"],
        "2533628fb78ec7eed058c63ecb1deca406ec73c1614c3411fda5174350e827d6"
    );
    assert_eq!(c.frameworks.len(), 27);
    assert!(!c.frameworks.keys().any(|k| k.contains("D3DMetal")));
}
#[test]
#[ignore = "explicit local registration validation and public HV config probe; no VM created"]
fn actual_registered_files_and_public_configuration_only() {
    let path =
        PathBuf::from(std::env::var_os("VEYNODE_VENUS_REGISTRATION").expect("registration path"));
    let reg = Registration::load(&path).unwrap();
    let resources = reg.firmware.path.parent().unwrap();
    reg.validate(
        &reg.qemu_library.path,
        resources,
        Some(&reg.spice_library.path),
    )
    .unwrap();
    println!("{}", host_admission().unwrap());
}

#[test]
fn fifo_registration_and_artifact_reject_without_waiting_for_writer() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().canonicalize().unwrap().join("fifo");
    let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(Registration::load(&fifo).is_err());
    assert!(hash_file(&fifo, &"0".repeat(64)).is_err());
}

#[test]
fn manager_rejects_unregistered_venus_before_persist_or_spawn_default_stays_available() {
    let dir = tempfile::tempdir().unwrap();
    let kernel = dir.path().join("kernel");
    fs::write(&kernel, b"fixture").unwrap();
    let mut manager = crate::Manager::load(
        dir.path().into(),
        "/unused".into(),
        crate::resources::MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    manager.qemu = Some(crate::backend::QemuRegistration {
        worker: kernel.clone(),
        launcher: kernel.clone(),
        library: kernel.clone(),
        resources: dir.path().into(),
        spice_library: None,
        screenshot_helper: None,
        console_broker: None,
        console_helper: None,
        venus_runtime: None,
    });
    let args = json!({"id":"venus","backend":"qemu-hvf","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel,"qemu_graphics_profile":"venus-moltenvk-experimental"});
    assert!(manager
        .dispatch(json!({"method":"create","arguments":args}), &mut None)
        .unwrap_err()
        .to_string()
        .contains("not registered"));
    assert!(!dir.path().join("registry.json").exists());
    assert!(manager.running.is_empty());
    let mut default = args.clone();
    default["qemu_graphics_profile"] = json!("virgl");
    let view = manager
        .dispatch(json!({"method":"create","arguments":default}), &mut None)
        .unwrap();
    assert_eq!(
        view["capabilities"]["graphics_profile"]["configured"],
        "virgl"
    );
    assert_eq!(
        view["capabilities"]["graphics_profile"]["guest_verified"],
        false
    );
    manager
        .configs
        .insert("venus".into(), serde_json::from_value(args).unwrap());
    assert!(manager
        .dispatch(
            json!({"method":"start","arguments":{"id":"venus"}}),
            &mut None
        )
        .unwrap_err()
        .to_string()
        .contains("not registered"));
    assert!(manager.running.is_empty());
    assert!(!dir.path().join("venus.json").exists());
}
