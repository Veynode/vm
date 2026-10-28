//! What each backend and Guest can do: capabilities, known limits, control features.
use super::*;

/// Product-spec capabilities that a backend/Guest pair either lacks or offers only under stated conditions. Each one is
/// listed by name with an explicit `status`, so a client never has to infer absence from a missing key. An entry is
/// `supported` only where code implements it; `unsupported` entries carry the reason. `guest` may be `windows`, which no
/// backend can register, so that its gaps are stated too.
pub const PRODUCT_CAPABILITIES: [&str; 7] = [
    "clipboard",
    "accessibility_metadata",
    "application_tools",
    "unicode_text_input",
    "retina_input_mapping",
    "multi_display_input_mapping",
    "window_metadata",
];
/// Known limits of a Guest that are not unimplemented features of this product: measured boundaries of the backend
/// the Guest runs on. Stated only for macOS Guests on Apple Virtualization; every other pair gets an empty list and no
/// `known_limits` key in its status. Each entry has exactly the keys in `KNOWN_LIMIT_KEYS`; `code` is the stable
/// identifier, and the wording claims no more than the cited decision established (one Guest version, one host).
pub const KNOWN_LIMIT_KEYS: [&str; 7] = [
    "code",
    "description",
    "effect",
    "classification",
    "measured_on",
    "decision",
    "evidence",
];
pub const KNOWN_LIMIT_CLASSIFICATION: &str =
    "backend limit of Apple's paravirtual graphics, not of this product's implementation";
pub const MACOS_GUEST_METAL_LARGE_COMPUTE: &str = "macos_guest_metal_large_compute_pipelines";
pub const MACOS_GUEST_OPENGL_NOT_ACCELERATED: &str = "macos_guest_opengl_not_accelerated";
pub fn known_limits(backend: Backend, guest: &str) -> Value {
    if backend != Backend::AppleVirtualization || guest != "macos" {
        return json!([]);
    }
    json!([
        {
            "code":MACOS_GUEST_METAL_LARGE_COMPUTE,
            "description":"Large Metal compute pipelines, including the Blender Cycles shading kernels, are not created on the Guest's Metal device, so Cycles rendering on the Guest's Metal device is not available.",
            "effect":"Metal graphics and small compute pipelines work, and Cycles renders on the Guest's CPUs. The largest kernels stall without an error instead of failing. This product has no render request path, so it raises no error of its own and performs no CPU fallback: the application in the Guest sees the failure or the stall.",
            "classification":KNOWN_LIMIT_CLASSIFICATION,
            "measured_on":"macOS 26.6.2 Guest on one host (Apple M1 Pro, macOS 26.4.1); other macOS versions and hosts were not measured, and the limit may move with an Apple update",
            "decision":"docs/decisions/0023-macos-guest-metal-compute-limit.md",
            "evidence":"archive/docs-evidence/blender-cycles-error-propagation/guest-baseline-app-v1/README.md",
        },
        {
            "code":MACOS_GUEST_OPENGL_NOT_ACCELERATED,
            "description":"The Guest has no accelerated OpenGL (CGL) renderer: an application that asks for OpenGL gets Apple Software Renderer.",
            "effect":"Metal applications are not affected. Requests that require an accelerated OpenGL renderer fail in the Guest (legacy, 3.2 core and 4.1 core were tried), and FreeCAD 1.1.4 ran on Apple Software Renderer 2.1; software-rendered results are not accepted as GPU acceleration.",
            "classification":KNOWN_LIMIT_CLASSIFICATION,
            "measured_on":"macOS 26.6.2 Guest with 4 CPUs and 4 GiB on one host (Apple M1 Pro); other macOS versions and hosts were not measured, and this is not a claim about future Apple releases",
            "decision":"docs/decisions/0017-macos-opengl-capability-gap.md",
            "evidence":"archive/docs-evidence/freecad-macos-live/cgl1-no-accelerated-renderer/README.md",
        },
    ])
}
pub fn product_capabilities(backend: Backend, guest: &str) -> Value {
    fn no(reason: &str) -> Value {
        json!({"status":"unsupported","reason":reason})
    }
    if guest == "windows" {
        let qemu = backend == Backend::QemuHvf;
        let mut out: serde_json::Map<String, Value> = PRODUCT_CAPABILITIES
            .iter()
            .map(|name| {
                let reason = if qemu {
                    "not implemented for Windows guests"
                } else {
                    "Apple Virtualization cannot run Windows; Windows guests run on QEMU/HVF (machine create --os windows)"
                };
                (name.to_string(), no(reason))
            })
            .collect();
        if qemu {
            let login = "Windows machines with SSH set up (machine ssh-key) and a logged-on desktop user; runs in that user's session over the Guest's OpenSSH Server";
            for (name, tools) in [
                ("window_metadata", "vm_desktop_state and vm_window_action through Win32"),
                ("accessibility_metadata", "vm_accessibility_tree, vm_accessibility_find and vm_accessibility_action through UI Automation"),
                ("application_tools", "vm_desktop_apps (Start menu entries) and vm_app_launch"),
            ] {
                out.insert(
                    name.into(),
                    json!({"status":"supported","requirements":format!("{tools}: {login}")}),
                );
            }
        }
        return Value::Object(out);
    }
    let mac = backend == Backend::AppleVirtualization && guest == "macos";
    let apple_linux = backend == Backend::AppleVirtualization && !mac;
    fn yes(requirements: &str) -> Value {
        json!({"status":"supported","requirements":requirements})
    }
    json!({
        "clipboard": no("not an MCP capability: no clipboard channel reaches agents through the daemon (cua-driver's clipboard tools are separate); the desktop viewers share a Linux desktop's text clipboard with the person viewing it"),
        "accessibility_metadata": if mac {
            yes("macOS containers: vm_accessibility_tree, vm_accessibility_find and vm_accessibility_action through the vm-macos-desktop helper over SSH (Accessibility API); needs the Guest Accessibility grant")
        } else if apple_linux {
            yes("Linux desktop containers (--desktop): vm_accessibility_tree, vm_accessibility_find and vm_accessibility_action; the guest agent reads AT-SPI over the desktop's accessibility bus")
        } else {
            no("not implemented: no Guest accessibility query exists for this Guest")
        },
        "application_tools": if mac {
            yes("macOS containers: vm_desktop_apps and vm_app_launch through the vm-macos-desktop helper over SSH; launching needs the input permission")
        } else if apple_linux {
            yes("Linux desktop containers (--desktop): vm_desktop_apps lists visible .desktop entries and vm_app_launch starts one as the desktop session's user; launching needs the input permission")
        } else {
            no("not implemented: no application launch or list tool exists; vm_exec_start runs an explicit argv only and is reported separately as command_execution")
        },
        "unicode_text_input": if mac {
            no("not implemented for macOS Guests: vm_input_text is refused; vm_input_key sends physical key positions that the Guest keyboard layout interprets")
        } else if apple_linux {
            no("not implemented for Apple Virtualization Linux Guests: vm_input_text requires a Linux Guest on QEMU; desktop containers take physical key positions through vm_input_key")
        } else {
            json!({"status":"supported","requirements":"vm_input_text: VM registered with qemu_guest_exec, persistent console, Guest service on the execution transport started with --allow-text-input, and a running X11 session with XTEST and at least 8 spare keycodes (no Wayland); a screenshot younger than 5 seconds; at most 256 Unicode scalar values per call, LF and TAB the only control characters; without these the call fails with an explicit unsupported error","evidence":"Guest half verified on real Xorg n=1, record not retained in this tree; host path offline-tested only; the X server acknowledges key events and application acceptance is not observed"})
        },
        "retina_input_mapping": if mac {
            json!({"status":"supported","requirements":"Guest service --allow-observe --allow-input with existing Screen Recording and Accessibility permissions; coordinates are pixels of the bound screenshot and the Guest maps them to Quartz points on its main display","evidence":"Guest unit test of the 2x mapping only; no live Guest input acceptance is recorded"})
        } else if apple_linux {
            no("not implemented: a Linux desktop container has one virtual display; input coordinates are its framebuffer pixels and no display scale factor is carried")
        } else {
            no("not implemented: the input binding carries no display scale factor; coordinates are physical framebuffer pixels of the bound screenshot and are validated only on an unscaled single-display Xorg desktop")
        },
        "multi_display_input_mapping": no(if mac {
            "not implemented: screenshots and input bind to the Guest main display only"
        } else if apple_linux {
            "not implemented: a Linux desktop container has one virtual display"
        } else {
            "not implemented: the QEMU console is one display and one absolute pointer device spans it"
        }),
        "window_metadata": if mac {
            json!({"status":"supported","requirements":"Guest service --allow-observe with existing Screen Recording permission and an eligible console; vm_windows returns no titles, focus, stacking order or Accessibility tree"})
        } else if apple_linux {
            yes("Linux desktop containers (--desktop): vm_desktop_state lists windows top-down with title, application, geometry and state through EWMH; vm_windows stays macOS-only")
        } else {
            no("not implemented on QEMU: vm_windows is refused for Linux Guests and the desktop window tools serve Apple Virtualization desktop containers only")
        },
    })
}
/// Every product-spec control feature, stated for each backend/Guest pair under `control_features`. Each entry has a
/// `status` from `FEATURE_STATUSES` and a `reason`. `supported` is used only with an `evidence` path to a real-Guest
/// record for that pair; code without such a record is `implemented_unverified` (offline tests at most).
/// `requires_guest_permission` means the code exists and only its refusal without the Guest grant is recorded.
/// `product_capabilities` is the older, narrower report and keeps its shape.
pub const CONTROL_FEATURES: [&str; 23] = [
    "lifecycle",
    "command_execution",
    "async_jobs",
    "file_transfer",
    "screenshot",
    "screenshot_scaled_jpeg",
    "pointer_key_input",
    "unicode_text_input",
    "window_metadata",
    "accessibility_metadata",
    "application_tools",
    "clipboard",
    "snapshot",
    "memory_reclaim",
    "multi_display_input_mapping",
    "retina_input_mapping",
    "shared_folders",
    "guest_shutdown",
    "balloon",
    "gpu_3d",
    "application_launch_list",
    "accessibility_tree",
    "window_list",
];
pub const FEATURE_STATUSES: [&str; 5] = [
    "supported",
    "implemented_unverified",
    "requires_guest_permission",
    "external_blocked",
    "unsupported",
];
pub fn control_features(backend: Backend, guest: &str) -> Value {
    let mut out = if backend == Backend::QemuHvf && guest == "macos" {
        Value::Object(CONTROL_FEATURES.iter().map(|name| {
            (name.to_string(), json!({"status":"unsupported","reason":"macOS Guests cannot be registered on QEMU; this backend supports Linux Guests only"}))
        }).collect())
    } else {
        base_control_features(backend, guest)
    };
    out["balloon"] = out["memory_reclaim"].clone();
    out["window_list"] = out["window_metadata"].clone();
    let desktop_tools = (backend == Backend::AppleVirtualization && guest != "windows")
        || (backend == Backend::QemuHvf && guest == "windows");
    out["application_launch_list"] = if desktop_tools {
        out["application_tools"].clone()
    } else {
        json!({"status":"unsupported","reason":if guest == "linux" {
            "not implemented: no vm_app_* launch or list tool exists on this backend; on Linux only, vm_exec_start runs explicit argv through enabled Guest command execution (see command_execution)"
        } else {
            "not implemented: no vm_app_* launch or list tool exists on this backend; command execution is exposed on Linux only"
        }})
    };
    out["accessibility_tree"] = if desktop_tools {
        out["accessibility_metadata"].clone()
    } else {
        json!({"status":"unsupported","reason":"not implemented: no Guest accessibility tree query exists for this backend and Guest"})
    };
    out["gpu_3d"] = if backend == Backend::AppleVirtualization && guest == "macos" {
        json!({"status":"supported","reason":"Narrow: Godot Metal Forward+ sample on one macOS 26.6.2 Guest; no accelerated OpenGL (decision 0017), large Metal compute/Cycles unavailable (decision 0023); see known_limits; no general compatibility or performance claim","evidence":"archive/docs-evidence/godot-macos-live/README.md"})
    } else if backend == Backend::QemuHvf && guest == "linux" {
        qemu_gpu_3d(qemu_graphics::Profile::Virgl)
    } else if backend == Backend::QemuHvf && guest == "windows" {
        qemu_gpu_3d(qemu_graphics::Profile::NeptuneDxmtExperimental)
    } else {
        json!({"status":"unsupported","reason":"no supported 3D acceleration path for this backend/Guest pair; host GPU availability does not establish Guest acceleration"})
    };
    if backend == Backend::QemuHvf && guest == "macos" {
        out["clipboard"] = json!({"status":"unsupported","reason":"not an MCP capability: no clipboard channel reaches agents through the daemon; macOS Guests also cannot be registered on QEMU"});
        out["snapshot"] = json!({"status":"unsupported","reason":"not implemented: no live snapshot or VM save/restore; macOS Guests also cannot be registered on QEMU"});
    }
    out
}
pub(crate) fn qemu_gpu_3d(profile: qemu_graphics::Profile) -> Value {
    match profile {
        qemu_graphics::Profile::Virtio2d => {
            json!({"status":"unsupported","reason":"The explicit virtio-2d profile supplies an unaccelerated 2D console. It does not provide Virgl, Venus or Guest 3D acceleration; snapshot support requires separate backend validation."})
        }
        qemu_graphics::Profile::NeptuneDxmtExperimental => {
            json!({"status":"implemented_unverified","reason":"Experimental: D3D11 through UTM's Triton driver, Neptune and DXMT on Metal, pixel-checked and benchmarked on one Windows machine with guest tools 0.1.273; no D3D12 or game claim (docs/known-issues.md)"})
        }
        qemu_graphics::Profile::Virgl => {
            json!({"status":"supported","reason":"Narrow: registered QEMU virgl/ANGLE Metal, SolveSpace OpenGL 2.1 sample only; requires a matching Guest driver; no general compatibility, per-VM Guest verification or performance claim","evidence":"archive/docs-evidence/linux-cad-interaction-live-v11/README.md"})
        }
        qemu_graphics::Profile::VenusMoltenvkExperimental => {
            json!({"status":"supported","reason":"Narrow: opt-in registered Venus runtime, one Linux Guest raster/vkmark sample; requires a matching Guest driver; registered host Geometry contract is not accepted (see host_api_constraints); no general compatibility, per-VM Guest verification or performance claim","evidence":"archive/docs-evidence/linux-vulkan-control-live/v2-accepted/README.md","host_api_constraints":qemu_graphics::host_api_constraints(profile)})
        }
    }
}
pub(crate) fn unverified_older(older: &Value, name: &str) -> Value {
    json!({"status":"implemented_unverified","reason":older[name]["requirements"]})
}
pub(crate) fn base_control_features(backend: Backend, guest: &str) -> Value {
    fn yes(reason: &str, evidence: &str) -> Value {
        json!({"status":"supported","reason":reason,"evidence":evidence})
    }
    fn st(status: &str, reason: &str) -> Value {
        json!({"status":status,"reason":reason})
    }
    fn no(reason: &str) -> Value {
        st("unsupported", reason)
    }
    // The seven older entries keep their unsupported reasons; those that have code are restated below.
    let mut out = product_capabilities(backend, guest);
    // Supported entries of the older report have code but no real-Guest record in this tree.
    let older = out.clone();
    let unverified = |name: &str| {
        st(
            "implemented_unverified",
            older[name]["requirements"].as_str().unwrap(),
        )
    };
    for entry in out.as_object_mut().unwrap().values_mut() {
        if entry["status"] != "unsupported" {
            *entry = Value::Null;
        }
    }
    let mac = backend == Backend::AppleVirtualization && guest == "macos";
    let qemu = backend == Backend::QemuHvf;
    if guest == "windows" {
        for name in CONTROL_FEATURES {
            out[name] = no(if qemu {
                "not implemented for Windows guests"
            } else {
                "Apple Virtualization cannot run Windows; Windows guests run on QEMU/HVF (machine create --os windows)"
            });
        }
        if !qemu {
            return out;
        }
        // One live Windows 11 26300 machine; the record is docs/known-issues.md, not an archived evidence file.
        out["lifecycle"] = st("implemented_unverified", "machine create --os windows: unattended install from the user's ISO, start, power-button shutdown and cold reboots; live on one Windows 11 ARM64 machine (docs/known-issues.md)");
        out["command_execution"] = st("implemented_unverified", "vm_exec over the Guest's OpenSSH Server (machine ssh-key): argv without a shell as the SSH user, up to 30 s, 64 KiB per stream; live on one machine");
        out["async_jobs"] = no("unsupported: Windows guests run commands with vm_exec only; there is no exec_start, exec_poll or exec_cancel");
        out["file_transfer"] = st("implemented_unverified", "vm_file_* over SSH: one name in %USERPROFILE%\\veynodevm-files, up to 8 MiB, staged on the host, no overwrite; live on one machine");
        for name in [
            "window_metadata",
            "accessibility_metadata",
            "application_tools",
        ] {
            out[name] = unverified_older(&older, name);
        }
        return out;
    }
    out["snapshot"] = if backend == Backend::AppleVirtualization && guest == "linux" {
        st("implemented_unverified", "Narrow: named full-state Apple Linux snapshots; explicit persistent identity, idle Guest work and stopped-target restore; no real-Guest record in this tree; GPU migration and cross-host use unverified")
    } else if qemu {
        no("unsupported: Virgl/Venus GPU state blocks full-state migration; the explicit unaccelerated virtio-2d QMP helper has CPU tests only and is not connected to production snapshot operations")
    } else {
        no("unsupported: macOS Guest snapshot operations are not implemented; offline APFS clones contain no RAM, CPU or device state")
    };
    out["screenshot_scaled_jpeg"] = if qemu && guest == "linux" {
        st("implemented_unverified", "scaled JPEG form (format, max_width, max_height, quality); needs a console broker and a helper that knows it; an older helper returns its full-size PNG unchanged (check mime_type)")
    } else {
        no("unsupported: the scaled JPEG screenshot request form is refused on the Apple backend; requires a Linux Guest on QEMU with a registered console broker")
    };
    if mac {
        out["lifecycle"] = yes("register an installed disk, start, power-button shutdown request, confirmed force stop; a locked Guest does not act on the power-button request", "archive/docs-evidence/apple-macos-installation.json");
        let exec = "not implemented: the macOS Guest service has no exec_start, exec_poll or exec_cancel and its legacy exec is not exposed (decision 0014); the Guest answers unsupported";
        out["command_execution"] = no(exec);
        out["async_jobs"] = no(exec);
        out["file_transfer"] = yes("Guest service --allow-files --files-root with a private same-UID directory; single filenames, 8 MiB", "archive/docs-evidence/macos-files-live/README.md");
        out["screenshot"] = yes("Guest service --allow-observe with existing Guest Screen Recording permission; main display", "archive/docs-evidence/control-macos-screenshot.md");
        out["pointer_key_input"] = st("requires_guest_permission", "implemented; needs Guest service --allow-observe --allow-input and a Guest Accessibility grant; only the refusal accessibility_permission_required is recorded on a real Guest");
        out["window_metadata"] = yes("Guest service --allow-observe with existing Guest Screen Recording permission; no titles, focus, stacking order or Accessibility tree", "archive/docs-evidence/macos-windows-live/README.md");
        out["retina_input_mapping"] = st("implemented_unverified", "Guest maps bound-screenshot pixels to Quartz points on its main display; Guest unit test of the 2x mapping only, and input itself needs the Guest Accessibility grant");
        out["accessibility_metadata"] = unverified("accessibility_metadata");
        out["application_tools"] = unverified("application_tools");
        out["memory_reclaim"] = no("not implemented: no balloon device is configured for macOS Guests and nothing returns Guest memory to the host");
        out["shared_folders"] = yes("shared_dir at registration only: one read-only virtiofs share tagged veynode, mounted from inside the Guest", "archive/docs-evidence/apple-macos-desktop-probe.md");
        out["guest_shutdown"] = no("not implemented: the macOS Guest service has no shutdown method, so vm_shutdown mode guest is answered unsupported; mode backend is the power-button request");
    } else if !qemu {
        out["lifecycle"] = yes("register a direct-boot kernel, start, power-button shutdown request, confirmed force stop; the Guest kernel may ignore the power button", "archive/docs-evidence/apple-linux-lifecycle.txt");
        let exec = "enabled Guest service with asynchronous jobs; argv only; synchronous and unknown/reconnecting Guest host limit 30 s; only opted-in QEMU Linux exec-v2 can use up to 120 s after vm_guest_info reports its bound no-reconnect policy";
        out["command_execution"] = yes(exec, "archive/docs-evidence/control-sync-cancel.md");
        out["async_jobs"] = yes(exec, "archive/docs-evidence/control-sync-cancel.md");
        out["file_transfer"] = yes("Guest service files integration with a dedicated non-root files account; single filenames, 8 MiB", "archive/docs-evidence/control-files.md");
        out["screenshot"] = st("implemented_unverified", "Linux desktop containers (--desktop) only: guest agent DRM scanout, PNG (JPEG when large); vm_guest_info reports capabilities.screenshot");
        out["pointer_key_input"] = st("implemented_unverified", "Linux desktop containers (--desktop) only: guest agent uinput keyboard and absolute tablet, against a fresh screenshot anchor");
        out["window_metadata"] = unverified("window_metadata");
        out["accessibility_metadata"] = unverified("accessibility_metadata");
        out["application_tools"] = unverified("application_tools");
        out["memory_reclaim"] = no("not supported: a virtio balloon device is attached but nothing drives it; measured 2026-10-12, a 512 MiB balloon target raised the host footprint by 933 MiB");
        out["shared_folders"] = st("implemented_unverified", "shared_dir at registration attaches one read-only virtiofs share tagged veynode; no real-Guest record of a Linux Guest mounting it through this daemon");
        out["guest_shutdown"] = yes("vm_shutdown mode guest; Guest service running as root with shutdown enabled; request acceptance is not completion", "archive/docs-evidence/apple-linux-normal-shutdown.json");
    } else {
        out["lifecycle"] = yes("QEMU registered by the daemon operator; direct-boot kernel with optional raw disk; start, QMP power-button request, confirmed force stop", "archive/docs-evidence/installed-qemu-retention-normal-live/README.md");
        let exec = "VM registered with qemu_guest_exec and a rootless cgroup-v2 Guest service on the execution transport; refused with an explicit unsupported error otherwise";
        out["command_execution"] = yes(
            exec,
            "archive/docs-evidence/installed-qemu-exec-live/README.md",
        );
        out["async_jobs"] = yes(
            exec,
            "archive/docs-evidence/installed-qemu-exec-live/README.md",
        );
        out["file_transfer"] = no("not implemented: the QEMU serial transports carry no file capability; the daemon refuses file calls with an explicit unsupported error");
        out["screenshot"] = yes("QEMU capture registered by the daemon operator; refused with an explicit unsupported error otherwise", "archive/docs-evidence/spice-screenshot.md");
        out["pointer_key_input"] = yes("registered QEMU persistent console; absolute click, drag, wheel and physical key codes on an unscaled single-display Xorg desktop", "archive/docs-evidence/control-input.md");
        out["unicode_text_input"] = st("implemented_unverified", "vm_input_text: qemu_guest_exec, Guest service --allow-text-input, X11 with XTEST (no Wayland); full requirements and limits under product_capabilities.unicode_text_input");
        out["memory_reclaim"] = no("not supported: no balloon or free page reporting device is configured; measured 2026-10-12, free page reporting discarded 594 MiB without lowering the host footprint");
        out["shared_folders"] =
            no("not implemented: configuration validation refuses shared_dir for QEMU");
        out["guest_shutdown"] = no("not implemented: the daemon refuses vm_shutdown mode guest for QEMU with an explicit unsupported error; mode backend is the QMP power-button request");
    }
    out
}
