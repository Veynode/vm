use super::*;

/// A cited record is a path in the unpublished vm-archive repository, never a
/// link; when that repository is checked out next to this one, the record must
/// exist there.
pub(crate) fn assert_archived(evidence: &str) {
    let path = evidence
        .strip_prefix("vm-archive/")
        .unwrap_or_else(|| panic!("{evidence}"));
    assert!(path.starts_with("docs-evidence/"), "{evidence}");
    let archive = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../vm-archive");
    if archive.is_dir() {
        assert!(archive.join(path).exists(), "{evidence}");
    }
}
#[test]
fn known_limits_state_the_two_macos_guest_graphics_limits_and_nothing_for_other_guests() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let limits = known_limits(Backend::AppleVirtualization, "macos");
    let limits = limits.as_array().unwrap();
    assert_eq!(
        limits
            .iter()
            .map(|limit| limit["code"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "macos_guest_metal_large_compute_pipelines",
            "macos_guest_opengl_not_accelerated"
        ]
    );
    for limit in limits {
        let object = limit.as_object().unwrap();
        assert_eq!(object.len(), KNOWN_LIMIT_KEYS.len(), "{limit}");
        for key in KNOWN_LIMIT_KEYS {
            assert!(
                limit[key].as_str().is_some_and(|text| !text.is_empty()),
                "{key}: {limit}"
            );
        }
        assert_eq!(
            limit["classification"],
            "backend limit of Apple's paravirtual graphics, not of this product's implementation"
        );
        // Scope of the measurement: one Guest version, one host, and no claim beyond it.
        let measured = limit["measured_on"].as_str().unwrap();
        for scope in [
            "macOS 26.6.2 Guest",
            "one host",
            "other macOS versions and hosts were not measured",
        ] {
            assert!(measured.contains(scope), "{scope}: {measured}");
        }
        let text = limit.to_string().to_lowercase();
        for overclaim in [
            "all macos",
            "any macos",
            "every macos",
            "permanent",
            "never",
            "always",
        ] {
            assert!(!text.contains(overclaim), "{overclaim}: {limit}");
        }
        assert!(
            limit["decision"]
                .as_str()
                .unwrap()
                .starts_with("docs/decisions/"),
            "{limit}"
        );
        assert!(
            repo.join(limit["decision"].as_str().unwrap()).exists(),
            "{limit}"
        );
        if let Some(evidence) = limit.get("evidence") {
            assert_archived(evidence.as_str().unwrap());
        }
    }
    let metal = &limits[0];
    assert_eq!(metal["description"], "Large Metal compute pipelines, including the Blender Cycles shading kernels, are not created on the Guest's Metal device, so Cycles rendering on the Guest's Metal device is not available.");
    assert_eq!(
        metal["decision"],
        "docs/decisions/0023-macos-guest-metal-compute-limit.md"
    );
    for stated in [
        "Cycles renders on the Guest's CPUs",
        "no render request path",
        "no CPU fallback",
        "stall without an error",
    ] {
        assert!(
            metal["effect"].as_str().unwrap().contains(stated),
            "{stated}"
        );
    }
    let opengl = &limits[1];
    assert_eq!(opengl["description"], "The Guest has no accelerated OpenGL (CGL) renderer: an application that asks for OpenGL gets Apple Software Renderer.");
    assert_eq!(
        opengl["decision"],
        "docs/decisions/0017-macos-opengl-capability-gap.md"
    );
    for stated in [
        "Metal applications are not affected",
        "Apple Software Renderer 2.1",
        "not accepted as GPU acceleration",
    ] {
        assert!(
            opengl["effect"].as_str().unwrap().contains(stated),
            "{stated}"
        );
    }
    // No such entries for any other pair.
    for (backend, guest) in [
        (Backend::AppleVirtualization, "linux"),
        (Backend::AppleVirtualization, "windows"),
        (Backend::QemuHvf, "linux"),
        (Backend::QemuHvf, "windows"),
        (Backend::QemuHvf, "macos"),
    ] {
        assert_eq!(
            known_limits(backend, guest),
            json!([]),
            "{backend:?} {guest}"
        );
    }
}
fn assert_control_features_pair(backend: Backend, guest: &str) {
    let cells = control_features(backend, guest);
    assert_eq!(cells.as_object().unwrap().len(), CONTROL_FEATURES.len());
    for name in CONTROL_FEATURES {
        let cell = &cells[name];
        assert!(
            FEATURE_STATUSES.contains(&cell["status"].as_str().unwrap()),
            "{name}: {cell}"
        );
        let reason = cell["reason"].as_str().unwrap();
        assert!(reason.len() > 20 && reason.len() < 260, "{name}: {cell}");
        if cell["status"] == "supported" {
            assert_archived(cell["evidence"].as_str().unwrap());
        }
    }
    for name in ["balloon", "clipboard"] {
        assert_eq!(cells[name]["status"], "unsupported", "{name}");
    }
    assert_eq!(
        cells["snapshot"]["status"],
        if backend == Backend::AppleVirtualization && guest == "linux" {
            "implemented_unverified"
        } else {
            "unsupported"
        }
    );
    assert_eq!(cells["balloon"], cells["memory_reclaim"]);
    assert_eq!(cells["window_list"], cells["window_metadata"]);
    // The desktop tools serve Apple Virtualization desktop containers, macOS containers and Windows machines.
    if (backend == Backend::AppleVirtualization) != (guest == "windows") {
        assert_eq!(cells["application_launch_list"], cells["application_tools"]);
        assert_eq!(cells["accessibility_tree"], cells["accessibility_metadata"]);
        assert_eq!(
            cells["accessibility_tree"]["status"],
            "implemented_unverified"
        );
    } else {
        for name in ["application_launch_list", "accessibility_tree"] {
            assert_eq!(cells[name]["status"], "unsupported", "{name}");
        }
        let apps = cells["application_launch_list"]["reason"].as_str().unwrap();
        assert!(apps.contains("no vm_app_*") && apps.contains("Linux only"));
        if guest == "linux" {
            assert!(apps.contains("vm_exec_start"));
        }
    }
    if backend == Backend::QemuHvf && guest == "macos" {
        for name in CONTROL_FEATURES {
            assert_eq!(cells[name]["status"], "unsupported", "{name}");
        }
    } else {
        // All pre-existing cells retain their exact values, including Windows refusal reasons.
        let original = base_control_features(backend, guest);
        for name in &CONTROL_FEATURES[..18] {
            assert_eq!(cells[*name], original[*name], "{name}");
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let mut manager = Manager::load(
        test_private_state(temp.path()),
        "/unused".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    let report = manager
        .dispatch(json!({"method":"capabilities","arguments":{}}), &mut None)
        .unwrap();
    let backend_name = serde_json::to_value(backend).unwrap();
    assert_eq!(
        report["backends"][backend_name.as_str().unwrap()]["control_features"][guest],
        cells
    );
}
#[test]
fn control_features_apple_macos_completeness() {
    assert_control_features_pair(Backend::AppleVirtualization, "macos");
}
#[test]
fn control_features_apple_linux_completeness() {
    assert_control_features_pair(Backend::AppleVirtualization, "linux");
}
#[test]
fn control_features_qemu_linux_completeness() {
    assert_control_features_pair(Backend::QemuHvf, "linux");
}
#[test]
fn control_features_qemu_macos_completeness() {
    assert_control_features_pair(Backend::QemuHvf, "macos");
}
#[test]
fn control_features_qemu_windows_completeness() {
    assert_control_features_pair(Backend::QemuHvf, "windows");
}
#[test]
fn control_features_macos_gpu_requires_evidence_and_states_limits() {
    for backend in [Backend::AppleVirtualization, Backend::QemuHvf] {
        let cells = control_features(backend, "macos");
        let gpu = &cells["gpu_3d"];
        if gpu["status"] == "supported" {
            assert_archived(gpu["evidence"].as_str().unwrap());
            let reason = gpu["reason"].as_str().unwrap();
            for part in [
                "Narrow",
                "no accelerated OpenGL",
                "0017",
                "0023",
                "large Metal compute/Cycles unavailable",
                "known_limits",
            ] {
                assert!(reason.contains(part), "{reason}");
            }
        }
    }
}
#[test]
fn control_features_gpu_narrows_by_qemu_runtime_and_profile() {
    let temp = tempfile::tempdir().unwrap();
    let mut manager = Manager::load(
        test_private_state(temp.path()),
        "/unused".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    manager.configs.insert(
        "linux".into(),
        serde_json::from_value(
            json!({"id":"linux","backend":"qemu-hvf","guest":"linux","cpus":1,"memory_mib":512}),
        )
        .unwrap(),
    );
    let gpu = |manager: &mut Manager| {
        manager.view("linux").unwrap()["capabilities"]["control_features"]["gpu_3d"].clone()
    };
    assert_eq!(gpu(&mut manager)["status"], "unsupported");
    // Registration metadata only; no runtime is launched or validated by this fixture.
    manager.qemu = Some(QemuRegistration {
        worker: "/unused".into(),
        launcher: "/unused".into(),
        library: "/unused".into(),
        resources: temp.path().into(),
        spice_library: None,
        screenshot_helper: None,
        console_broker: None,
        console_helper: None,
        venus_runtime: None,
    });
    assert_eq!(
        gpu(&mut manager),
        control_features(Backend::QemuHvf, "linux")["gpu_3d"]
    );
    manager
        .configs
        .get_mut("linux")
        .unwrap()
        .qemu_graphics_profile = qemu_graphics::Profile::VenusMoltenvkExperimental;
    assert_eq!(gpu(&mut manager)["status"], "unsupported");
    manager.qemu.as_mut().unwrap().venus_runtime = Some("/unused".into());
    let venus = gpu(&mut manager);
    assert_eq!(venus["status"], "supported");
    assert!(venus["reason"]
        .as_str()
        .unwrap()
        .contains("no general compatibility, per-VM Guest verification or performance claim"));
    assert_archived(venus["evidence"].as_str().unwrap());
    assert_eq!(
        manager.view("linux").unwrap()["capabilities"]["graphics_profile"]["guest_verified"],
        false
    );
    assert!(manager.running.is_empty());
}
