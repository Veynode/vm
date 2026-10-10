//! Offline wrapper contract for the native console helper. A `/bin/sh` exec
//! wrapper points at a fake REAL helper that logs argv, records its PID and
//! prints fixed probe JSON. It never reads stdin or loops, so no real helper,
//! VM, daemon, QEMU, SPICE, GPU, input or persistent serving loop runs.
use serde_json::{json, Value};
use std::{
    fs,
    io::Read,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};
use vm_control::{
    backend::{
        probe_native_window, probe_process, validate_window_capabilities, window_supported,
        QemuRegistration,
    },
    native_input::hex,
    MAX_MESSAGE,
};
use vm_media_local::{
    console_wire::{read_record, write_record, Record, MAX_BODY},
    console_write::write_record_deadline,
};

const CAPABILITIES: &str = r#"{"native_window_protocol":1,"native_window":true,"display_only":true,"input_enabled":false}"#;
const CODECS: &str = r#"{"h264":"unavailable","h265":"unavailable"}"#;
const WORKER_INSTANCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const DRAW_TIMING_BYTES: usize = 56 * 1024;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    wrapper: PathBuf,
    argv_log: PathBuf,
    pid_file: PathBuf,
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', r"'\''"))
}

fn write_executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

// The wrapper contract under test: only `$2 == --persistent` adds the flag.
fn wrapper_text(real: &Path) -> String {
    let real = sh_quote(real);
    format!(
        "#!/bin/sh\nif [ \"$2\" = \"--persistent\" ]; then\n  exec {real} \"$@\" --draw-timing-diagnostic\nfi\nexec {real} \"$@\"\n"
    )
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let real = root.join("real-helper");
    let wrapper = root.join("native-helper");
    let argv_log = root.join("argv.log");
    let pid_file = root.join("helper.pid");
    let fake = format!(
        "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\"; done >> {log}\nprintf '%s\\n' \"$$\" > {pid}\ncase \"$1\" in\n--probe-capabilities) printf '%s\\n' '{caps}' ;;\n--probe-codecs) printf '%s\\n' '{codecs}' ;;\nesac\nexit 0\n",
        log = sh_quote(&argv_log),
        pid = sh_quote(&pid_file),
        caps = CAPABILITIES,
        codecs = CODECS,
    );
    write_executable(&real, &fake);
    write_executable(&wrapper, &wrapper_text(&real));
    Fixture {
        _dir: dir,
        root,
        wrapper,
        argv_log,
        pid_file,
    }
}

fn recorded(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .expect("fake REAL helper did not run")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn reset(fx: &Fixture) {
    let _ = fs::remove_file(&fx.argv_log);
}

#[test]
fn probes_pass_through_the_wrapper_unchanged() {
    let fx = fixture();
    let (status, stdout, stderr) = probe_process(&fx.wrapper, "--probe-capabilities").unwrap();
    assert!(status.success() && stderr.is_empty());
    assert_eq!(stdout, format!("{CAPABILITIES}\n").into_bytes());
    assert_eq!(recorded(&fx.argv_log), ["--probe-capabilities"]);

    // Production capability validation (backend.rs probe_native_window) on the wrapper.
    reset(&fx);
    let value = probe_native_window(&fx.wrapper).unwrap();
    assert_eq!(value, serde_json::from_str::<Value>(CAPABILITIES).unwrap());
    assert!(window_supported(&value));
    assert_eq!(recorded(&fx.argv_log), ["--probe-capabilities"]);

    // vm-console probe() codec call: no $2, so no diagnostic flag.
    reset(&fx);
    let (status, stdout, _) = probe_process(&fx.wrapper, "--probe-codecs").unwrap();
    assert!(status.success());
    assert_eq!(stdout, format!("{CODECS}\n").into_bytes());
    assert_eq!(recorded(&fx.argv_log), ["--probe-codecs"]);

    // The rule keys on $2 alone: --persistent in $1 does not add the flag.
    reset(&fx);
    let status = Command::new(&fx.wrapper)
        .args(["--persistent", "SPICE"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(recorded(&fx.argv_log), ["--persistent", "SPICE"]);
}

#[test]
fn persistent_argv_is_spice_then_control_args_then_diagnostic_flag() {
    let fx = fixture();
    let spice_path = fx.root.join("spice.sock");
    let control_path = fx.root.join("control.sock");
    let spice = spice_path.to_str().unwrap();
    let control = control_path.to_str().unwrap();
    let native = [
        "--control-socket",
        control,
        "--vm-id",
        "vm-test",
        "--worker-instance",
        WORKER_INSTANCE,
    ];
    assert!(hex(WORKER_INSTANCE, 64)); // same check as vm-console.rs:697
                                       // Same order as vm-console.rs:718-722: encoder SPICE --persistent <native args>.
    let mut child = Command::new(&fx.wrapper)
        .arg(spice)
        .arg("--persistent")
        .args(native)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let spawned_pid = child.id();
    assert!(child.wait().unwrap().success());
    assert_eq!(
        recorded(&fx.argv_log),
        [
            spice,
            "--persistent",
            "--control-socket",
            control,
            "--vm-id",
            "vm-test",
            "--worker-instance",
            WORKER_INSTANCE,
            "--draw-timing-diagnostic",
        ]
    );
    // exec keeps the spawned PID on the real helper, the PID vm-console's OwnedChild kills.
    let helper_pid: u32 = fs::read_to_string(&fx.pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(helper_pid, spawned_pid);
}

#[test]
fn window_status_with_draw_timing_reply_is_not_size_or_key_rejected() {
    // Synthetic ~56 KiB payload; the real native draw_timing schema is deliberately not inspected.
    let mut samples = Vec::new();
    let mut estimate = 2; // "[]"
    while estimate < DRAW_TIMING_BYTES {
        let frame = samples.len() as u64;
        let sample = json!({"frame": frame, "submit_ns": frame * 16_666_667, "present_ns": frame * 16_666_667 + 2_500_000});
        estimate += sample.to_string().len() + 1;
        samples.push(sample);
    }
    let draw_timing = json!({"schema": 1, "samples": samples});
    let draw_len = serde_json::to_vec(&draw_timing).unwrap().len();
    assert!(
        (DRAW_TIMING_BYTES..DRAW_TIMING_BYTES + 1024).contains(&draw_len),
        "draw_timing bytes: {draw_len}"
    );

    // Native window_status shape (protocol 2), plus the review-named keys and an unknown key.
    let status = json!({
        "native_window": true,
        "native_window_protocol": 2,
        "native_input_protocol": 1,
        "native_input_supported": true,
        "native_input_available": true,
        "display_only": true,
        "input_enabled": false,
        "window_open": true,
        "display_instance": "d".repeat(32),
        "draw_timing": draw_timing.clone(),
        // Placeholders: only the absence of key-level rejection is under test.
        "copy_timing": {"placeholder": true},
        "presented_time_samples": [1, 2, 3],
        "input_frame_diagnostic": {"placeholder": true},
        "unknown_future_key": {"nested": [true]}
    });

    // Native -> broker: receive_native framing and id routing (vm-console.rs:146-163).
    let mut wire = Vec::new();
    write_record(
        &mut wire,
        &Record::json(0, &json!({"id": "c1", "result": status})),
    )
    .unwrap();
    let record = read_record(&mut wire.as_slice()).unwrap();
    assert_eq!(record.epoch, 0);
    assert!(record.body.len() >= DRAW_TIMING_BYTES && record.body.len() <= MAX_BODY);
    let native = record.value().unwrap();
    assert_eq!(native["id"], "c1");
    // Native::call result extraction (vm-console.rs:278-279) and window arm (547-559).
    let result = native
        .get("result")
        .filter(|r| r.is_object())
        .cloned()
        .unwrap();
    let result = validate_window_capabilities(result).unwrap();
    assert!(window_supported(&result) && result["window_open"].is_boolean());
    assert_eq!(result["draw_timing"], draw_timing);

    // Broker -> client over a real socket pair: write_record_deadline (vm-console.rs:482, 621-627).
    let (broker_end, mut client_end) = UnixStream::pair().unwrap();
    broker_end.set_nonblocking(true).unwrap();
    client_end
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let client = thread::spawn(move || {
        // Header and bound checks as in vm-qemu.rs:697-705, bounded by MAX_MESSAGE.
        let mut header = [0u8; 13];
        client_end.read_exact(&mut header).unwrap();
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        assert!(header[4] == 1 && header[5..13] == [0; 8] && length > 0 && length <= MAX_MESSAGE);
        let mut body = vec![0; length];
        client_end.read_exact(&mut body).unwrap();
        serde_json::from_slice::<Value>(&body).unwrap()
    });
    let reply = json!({"request_id": "native-window", "result": result});
    write_record_deadline(
        &broker_end,
        &Record::json(0, &reply),
        Duration::from_millis(500),
    )
    .unwrap();
    let reply = client.join().unwrap();

    // Client side: console_window validation (vm-qemu.rs:740-748).
    assert_eq!(reply["request_id"], "native-window");
    let client_result = validate_window_capabilities(reply["result"].clone()).unwrap();
    assert!(window_supported(&client_result) && client_result["window_open"].is_boolean());
    assert_eq!(client_result["draw_timing"], draw_timing);
}

#[test]
fn wrapper_is_accepted_as_the_console_helper_registration() {
    let fx = fixture();
    let file = |name: &str| {
        let path = fx.root.join(name);
        fs::write(&path, b"").unwrap();
        path
    };
    let resources = fx.root.join("resources");
    fs::create_dir(&resources).unwrap();
    // QemuRegistration::validate (backend.rs:29-43) requires absolute existing files.
    QemuRegistration {
        worker: file("worker"),
        launcher: file("launcher"),
        library: file("library"),
        resources,
        spice_library: None,
        screenshot_helper: None,
        console_broker: Some(file("console-broker")),
        console_helper: Some(fx.wrapper.clone()),
        venus_runtime: None,
    }
    .validate()
    .unwrap();
}
