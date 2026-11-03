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
fn host_geometry_observation_is_bound_to_catalog_and_not_guest_acceptance() {
    let constraints = host_api_constraints(Profile::VenusMoltenvkExperimental);
    let gs = &constraints["geometry_shader"];
    assert_eq!(gs["status"], "unsupported");
    assert_eq!(gs["reported_feature"], true);
    assert_eq!(gs["guest_verified"], false);
    assert_eq!(
        gs["driver_sha256"],
        catalog().frameworks["MoltenVK.framework/Versions/A/MoltenVK"]
    );
    assert_eq!(gs["reported_limits"].as_object().unwrap().len(), 5);
    let mut replaced = catalog();
    replaced.frameworks.insert(
        "MoltenVK.framework/Versions/A/MoltenVK".into(),
        "0".repeat(64),
    );
    let unknown = catalog_geometry_constraint(&replaced);
    assert_eq!(unknown["geometry_shader"]["status"], "unknown");
    assert!(unknown["geometry_shader"].get("reported_feature").is_none());
    assert!(host_api_constraints(Profile::Virgl)
        .as_object()
        .unwrap()
        .is_empty());
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
fn virtio_2d_is_explicit_and_requires_linux_qemu() {
    let raw = json!({"id":"two-d","backend":"qemu-hvf","guest":"linux","cpus":2,"memory_mib":1024,"kernel":"/dev/null","qemu_graphics_profile":"virtio-2d"});
    let config: crate::Config = serde_json::from_value(raw).unwrap();
    let profile = config.qemu_graphics_profile;
    assert_eq!(profile, Profile::Virtio2d);
    assert!(!profile.is_default());
    assert!(!profile.venus());
    assert_eq!(
        serde_json::to_value(config).unwrap()["qemu_graphics_profile"],
        "virtio-2d"
    );
    assert!(profile
        .validate(crate::backend::Backend::QemuHvf, "linux")
        .is_ok());
    assert!(profile
        .validate(crate::backend::Backend::AppleVirtualization, "linux")
        .is_err());
    assert!(profile
        .validate(crate::backend::Backend::QemuHvf, "macos")
        .is_err());
    for alias in ["virtio2d", "2d", "virtio-gpu-pci"] {
        assert!(serde_json::from_value::<Profile>(json!(alias)).is_err());
    }
    assert_eq!(Profile::default(), Profile::Virgl);
}
#[test]
fn virtio_2d_uses_separate_non_gl_device_and_preserves_dimension_bounds() {
    assert_eq!(
        Profile::Virtio2d.machine_arguments(),
        ["-machine", "virt,accel=hvf"]
    );
    assert_eq!(
        Profile::Virtio2d.device(None, None).unwrap(),
        "virtio-gpu-pci"
    );
    assert_eq!(
        Profile::Virtio2d.device(Some(1280), Some(720)).unwrap(),
        "virtio-gpu-pci,xres=1280,yres=720"
    );
    for (width, height) in [
        (Some(1279), Some(720)),
        (Some(1280), None),
        (None, Some(720)),
        (Some(318), Some(720)),
        (Some(3842), Some(2160)),
        (Some(3840), Some(2162)),
    ] {
        assert!(Profile::Virtio2d.device(width, height).is_err());
    }
    assert_eq!(
        Profile::Virtio2d.device(Some(320), Some(320)).unwrap(),
        "virtio-gpu-pci,xres=320,yres=320"
    );
    assert_eq!(
        Profile::Virtio2d.device(Some(3840), Some(2160)).unwrap(),
        "virtio-gpu-pci,xres=3840,yres=2160"
    );
}
#[test]
fn only_virtio_2d_turns_off_gl_without_inheriting_venus_host_evidence() {
    assert!(Profile::Virgl.requires_gl());
    assert!(Profile::VenusMoltenvkExperimental.requires_gl());
    assert!(!Profile::Virtio2d.requires_gl());
    assert!(host_api_constraints(Profile::Virtio2d)
        .as_object()
        .unwrap()
        .is_empty());
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
        "9f227ab0689f4674124f5f5dd98975f1251491bfeb1fccb2bed81cf164c5522b"
    );
    assert_eq!(
        c.roles["loader"],
        "f1beede74e3879ca2ed39fa9decdec42252519eb2f3086fc71f7a6b6f0fbb221"
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
        crate::test_private_state(dir.path()),
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

#[test]
fn source_qemu_layout_requires_compiled_exact_path_and_retains_vendor_default() {
    let mut approved = catalog();
    let frameworks = Path::new("/vendor/Frameworks");
    let vendor = frameworks.join("qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu");
    let source = PathBuf::from(
        "/private/reviewed/VeynodeQEMUDevelopment.framework/Versions/A/VeynodeQEMUDevelopment",
    );
    assert!(qemu_dependency_layout(&approved, frameworks, &vendor).is_ok());
    assert!(qemu_dependency_layout(&approved, frameworks, &source).is_err());
    approved.source_qemu_library = Some(source.clone());
    assert!(qemu_dependency_layout(&approved, frameworks, &source).is_ok());
    assert!(qemu_dependency_layout(&approved, frameworks, &vendor).is_err());
    let substituted =
        Path::new("/other/VeynodeQEMUDevelopment.framework/Versions/A/VeynodeQEMUDevelopment");
    assert!(qemu_dependency_layout(&approved, frameworks, substituted).is_err());
    for bad in [
        "relative/VeynodeQEMUDevelopment.framework/Versions/A/VeynodeQEMUDevelopment",
        "/private/../reviewed/VeynodeQEMUDevelopment.framework/Versions/A/VeynodeQEMUDevelopment",
        "/private/not-the-reviewed-framework",
    ] {
        approved.source_qemu_library = Some(bad.into());
        assert!(qemu_dependency_layout(&approved, frameworks, Path::new(bad)).is_err());
    }
}

#[test]
fn registration_cannot_supply_its_own_compiled_source_qemu_policy() {
    let mut value = serde_json::to_value(empty()).unwrap();
    value["source_qemu_library"] =
        json!("/attacker/VeynodeQEMUDevelopment.framework/Versions/A/VeynodeQEMUDevelopment");
    assert!(serde_json::from_value::<Registration>(value).is_err());
}

/// A release package's own runtime: relative registration, MANIFEST.json pins.
fn package_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().canonicalize().unwrap();
    let files: [(&str, &[u8]); 8] = [
        ("venus/probe-loader", b"loader @executable_path/../Frameworks\0"),
        ("venus/libvirglrenderer.1.dylib", b"host @loader_path/../Frameworks\0"),
        ("venus/virgl_render_server", b"server @executable_path/../Frameworks\0"),
        ("Frameworks/qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu", b"qemu"),
        ("Frameworks/libspice-server-fixed.dylib", b"spice"),
        ("Frameworks/MoltenVK.framework/Versions/A/MoltenVK", b"moltenvk"),
        ("Resources/vulkan/icd.d/MoltenVK_icd.json", br#"{"ICD":{"library_path":"../../../Frameworks/MoltenVK.framework/Versions/Current/MoltenVK","is_portability_driver":true}}"#),
        ("share/edk2-aarch64-code.fd", b"firmware"),
    ];
    let mut manifest = vec![];
    for (path, bytes) in files {
        let full = package.join("libexec/qemu").join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(&full, bytes).unwrap();
        manifest.push(json!({"path": format!("libexec/qemu/{path}"), "sha256": format!("{:x}", Sha256::digest(bytes))}));
    }
    symlink(
        "A",
        package.join("libexec/qemu/Frameworks/MoltenVK.framework/Versions/Current"),
    )
    .unwrap();
    manifest.push(json!({"path": "libexec/qemu/Frameworks/MoltenVK.framework/Versions/Current/MoltenVK", "sha256": format!("{:x}", Sha256::digest(b"moltenvk"))}));
    fs::write(
        package.join("MANIFEST.json"),
        serde_json::to_vec(&json!({"files": manifest})).unwrap(),
    )
    .unwrap();
    let sha = |p: &str| {
        format!(
            "{:x}",
            Sha256::digest(fs::read(package.join("libexec/qemu").join(p)).unwrap())
        )
    };
    let artifact = |p: &str| json!({"path": format!("../{p}"), "sha256": sha(p)});
    let registration = package.join("libexec/qemu/venus/registration.json");
    fs::write(&registration, serde_json::to_vec(&json!({
        "schema": 1, "profile_revision": "owned-group-v1", "frameworks_dir": "../Frameworks",
        "loader": artifact("venus/probe-loader"),
        "host_library": artifact("venus/libvirglrenderer.1.dylib"),
        "render_server": artifact("venus/virgl_render_server"),
        "qemu_library": artifact("Frameworks/qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu"),
        "spice_library": artifact("Frameworks/libspice-server-fixed.dylib"),
        "icd": artifact("Resources/vulkan/icd.d/MoltenVK_icd.json"),
        "firmware": artifact("share/edk2-aarch64-code.fd"),
    })).unwrap()).unwrap();
    fs::set_permissions(&registration, fs::Permissions::from_mode(0o600)).unwrap();
    (dir, package)
}

#[test]
fn package_runtime_is_admitted_by_its_own_manifest_only() {
    let (_dir, package) = package_fixture();
    let runtime = package.join("libexec/qemu");
    let reg = Registration::load(&runtime.join("venus/registration.json")).unwrap();
    assert_eq!(reg.frameworks_dir, runtime.join("Frameworks"));
    let qemu =
        runtime.join("Frameworks/qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu");
    let spice = runtime.join("Frameworks/libspice-server-fixed.dylib");
    let validate = |package: Option<&Path>| {
        reg.validate_in(&qemu, &runtime.join("share"), Some(&spice), package)
    };
    validate(Some(&package)).unwrap();
    // Another binary's package, or none: the compiled catalog refuses it.
    assert!(validate(None)
        .unwrap_err()
        .to_string()
        .contains("unreviewed"));
    assert!(validate(Some(Path::new("/elsewhere"))).is_err());
    // A framework file that differs from the manifest.
    fs::write(
        runtime.join("Frameworks/MoltenVK.framework/Versions/A/MoltenVK"),
        b"other",
    )
    .unwrap();
    assert!(validate(Some(&package)).is_err());
    fs::write(
        runtime.join("Frameworks/MoltenVK.framework/Versions/A/MoltenVK"),
        b"moltenvk",
    )
    .unwrap();
    validate(Some(&package)).unwrap();
    // A renderer outside the package has no manifest entry.
    let mut outside = reg.clone();
    outside.render_server.path = package.join("virgl_render_server");
    fs::copy(
        runtime.join("venus/virgl_render_server"),
        &outside.render_server.path,
    )
    .unwrap();
    assert!(outside
        .validate_in(&qemu, &runtime.join("share"), Some(&spice), Some(&package))
        .is_err());
    // The package's run path, not an absolute one, is what a packaged loader carries.
    fs::write(runtime.join("venus/probe-loader"), b"loader /abs\0").unwrap();
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(package.join("MANIFEST.json")).unwrap()).unwrap();
    manifest["files"][0]["sha256"] = json!(format!("{:x}", Sha256::digest(b"loader /abs\0")));
    fs::write(
        package.join("MANIFEST.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let mut moved = reg.clone();
    moved.loader.sha256 = format!("{:x}", Sha256::digest(b"loader /abs\0"));
    assert!(moved
        .validate_in(&qemu, &runtime.join("share"), Some(&spice), Some(&package))
        .unwrap_err()
        .to_string()
        .contains("rpath"));
}
