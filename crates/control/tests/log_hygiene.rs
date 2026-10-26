//! Real daemon/QEMU worker, scripted QMP and console peers; no VM or native capture.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const SCREEN: &[u8] = b"VNMARK-SCREEN-01!";
const TOKEN: &[u8] = b"VNMARK-TOKEN-0123456789abcdef";
const BODY: &[u8] = b"VNMARK-FILE-BODY-0123456789abcdef";
const LIMIT: Duration = Duration::from_secs(3);

fn representations(marker: &[u8]) -> Vec<Vec<u8>> {
    let mut forms = vec![marker.to_vec(), STANDARD.encode(marker).into_bytes()];
    for upper in [false, true] {
        forms.push(
            marker
                .iter()
                .map(|b| {
                    if upper {
                        format!("{b:02X}")
                    } else {
                        format!("{b:02x}")
                    }
                })
                .collect::<String>()
                .into_bytes(),
        );
    }
    // A marker embedded in a larger base64 body has no padding and can start
    // at any of three byte alignments. Match only its complete base64 groups.
    for offset in 0..3 {
        let end = offset + (marker.len() - offset) / 3 * 3;
        forms.push(STANDARD.encode(&marker[offset..end]).into_bytes());
    }
    forms
}

fn contains_marker(log: &[u8], marker: &[u8]) -> bool {
    representations(marker)
        .iter()
        .any(|form| log.windows(form.len()).any(|window| window == form))
}

fn assert_clean(path: &Path) {
    let log = fs::read(path).unwrap();
    for (name, marker) in [
        ("screenshot", SCREEN),
        ("token argument", TOKEN),
        ("file body argument", BODY),
    ] {
        assert!(
            !contains_marker(&log, marker),
            "{name} canary leaked into {}",
            path.display()
        );
    }
}

#[test]
fn scanner_detects_raw_base64_hex_and_embedded_picture() {
    for marker in [SCREEN, TOKEN, BODY] {
        for form in representations(marker) {
            assert!(contains_marker(
                &[b"prefix ".as_slice(), &form, b" suffix"].concat(),
                marker
            ));
        }
        assert!(!contains_marker(
            b"screenshot timing bytes=2056 total_us=12",
            marker
        ));
    }
    let png = [b"\x89PNG\r\n\x1a\n".as_slice(), &SCREEN.repeat(128)].concat();
    assert!(contains_marker(STANDARD.encode(png).as_bytes(), SCREEN));
}

struct Rig {
    daemon: Child,
    dir: PathBuf,
    stderr: PathBuf,
    runtime: Option<PathBuf>,
    _tmp: tempfile::TempDir,
}
impl Drop for Rig {
    fn drop(&mut self) {
        // Only the daemon started here is signalled. EOF makes its own worker
        // quit the scripted QMP peer, reap the broker and remove its runtime.
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        if let Some(runtime) = &self.runtime {
            let until = Instant::now() + LIMIT;
            while runtime.exists() && Instant::now() < until {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
impl Rig {
    fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("log-hygiene-")
            .tempdir_in("/tmp")
            .unwrap();
        let peers = tmp.path().join("peers.py");
        fs::write(&peers, include_bytes!("log_hygiene_peers.py")).unwrap();
        fs::set_permissions(&peers, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(tmp.path().join("resources")).unwrap();
        fs::write(tmp.path().join("kernel"), b"not bootable").unwrap();
        let dir = tmp.path().join("state");
        let stderr = tmp.path().join("daemon.stderr");
        let daemon = Command::new(env!("CARGO_BIN_EXE_vm-control"))
            .args(["daemon", "--state-dir"])
            .arg(&dir)
            .args(["--backend", "/usr/bin/false", "--qemu-worker"])
            .arg(env!("CARGO_BIN_EXE_vm-qemu"))
            .arg("--qemu-launcher")
            .arg(&peers)
            .arg("--qemu-library")
            .arg(&peers)
            .arg("--qemu-resources")
            .arg(tmp.path().join("resources"))
            .arg("--qemu-console-broker")
            .arg(&peers)
            .arg("--qemu-console-helper")
            .arg(&peers)
            .stdout(Stdio::null())
            .stderr(File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let mut rig = Self {
            daemon,
            dir,
            stderr,
            runtime: None,
            _tmp: tmp,
        };
        rig.until("daemon socket", |rig| {
            UnixStream::connect(rig.dir.join("control.sock")).is_ok()
        });
        assert_eq!(rig.rpc("list", json!({}))["result"], json!([]));
        let created = rig.rpc(
            "create",
            json!({"id":"hygiene","backend":"qemu-hvf","guest":"linux",
            "cpus":1,"memory_mib":256,"kernel":rig._tmp.path().join("kernel")}),
        );
        assert!(created.get("error").is_none(), "{created}");
        let started = rig.rpc("start", json!({"id":"hygiene"}));
        assert!(started.get("error").is_none(), "{started}");
        rig.until("scripted capture", |rig| {
            let status = rig.rpc("status", json!({"id":"hygiene"}));
            assert!(status.get("error").is_none(), "{status}");
            assert_ne!(status["result"]["state"], "error", "{status}");
            if let Some(socket) = status["result"]["runtime"]["spice_socket"].as_str() {
                rig.runtime = Some(Path::new(socket).parent().unwrap().to_owned());
            }
            status["result"]["state"] == "running"
                && status["result"]["runtime"]["capture_connected"] == true
        });
        rig
    }
    fn until(&mut self, label: &str, mut ready: impl FnMut(&mut Self) -> bool) {
        let until = Instant::now() + LIMIT;
        while !ready(self) {
            let status = self.daemon.try_wait().unwrap();
            assert!(
                status.is_none(),
                "daemon exited waiting for {label}: {status:?}: {}",
                fs::read_to_string(&self.stderr).unwrap()
            );
            assert!(Instant::now() < until, "timeout waiting for {label}");
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn rpc(&self, method: &str, arguments: Value) -> Value {
        let mut stream = UnixStream::connect(self.dir.join("control.sock")).unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        stream.set_write_timeout(Some(LIMIT)).unwrap();
        writeln!(stream, "{}", json!({"method":method,"arguments":arguments})).unwrap();
        let mut frame = Vec::new();
        BufReader::new(stream)
            .take(vm_control::MAX_SCREENSHOT_RESPONSE as u64 + 1)
            .read_until(b'\n', &mut frame)
            .unwrap();
        assert_eq!(frame.last(), Some(&b'\n'));
        serde_json::from_slice(&frame).unwrap()
    }
}

fn check_state_logs(dir: &Path) -> usize {
    let mut checked = 0;
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            checked += check_state_logs(&entry.path());
        } else if entry.path().extension().is_some_and(|ext| ext == "log") {
            assert!(kind.is_file(), "log must be a regular file");
            assert_clean(&entry.path());
            checked += 1;
        }
    }
    checked
}

#[test]
fn daemon_screenshot_and_refused_arguments_stay_out_of_logs() {
    let mut rig = Rig::new();
    // Bearer authentication belongs to vm-mcp-http, not this daemon. The
    // daemon sees this token-looking string only as request content. QEMU's
    // observer-only transport refuses exec/files; no Guest command is executed.
    let token = std::str::from_utf8(TOKEN).unwrap();
    let body = std::str::from_utf8(BODY).unwrap();
    for (method, arguments) in [
        (
            "exec_start",
            json!({"id":"hygiene","argv":["/bin/echo",format!("Bearer {token}"),body]}),
        ),
        (
            "file_upload_begin",
            json!({"id":"hygiene","path":body,"size":BODY.len(),"sha256":"0".repeat(64)}),
        ),
        (
            "file_upload_write",
            json!({"id":"hygiene","token":token,"offset":0,"data":STANDARD.encode(BODY)}),
        ),
    ] {
        let reply = rig.rpc(method, arguments);
        assert!(
            reply["error"].as_str().unwrap().contains("observer-only"),
            "{reply}"
        );
    }
    let shot = rig.rpc("screenshot", json!({"id":"hygiene"}));
    assert!(shot.get("error").is_none(), "{shot}");
    let png = STANDARD
        .decode(shot["result"]["data_base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        png,
        [b"\x89PNG\r\n\x1a\n".as_slice(), &SCREEN.repeat(128)].concat()
    );
    // The timing record is queued after writing the screenshot response; wait
    // for its actual emission before examining the entire, untruncated stderr.
    rig.until("screenshot timing log", |rig| {
        fs::read_to_string(&rig.stderr)
            .unwrap()
            .contains("screenshot timing ")
    });
    assert_clean(&rig.stderr);
    // security.md documents worker *.stderr.log and *.console.log here. No
    // serial_log is configured. Registry/journals are state, not routine logs.
    assert!(
        check_state_logs(&rig.dir) >= 2,
        "expected QEMU and console log sinks"
    );
    let runtime = rig.runtime.clone().expect("worker runtime observed");
    rig.daemon.kill().unwrap();
    rig.daemon.wait().unwrap();
    let until = Instant::now() + LIMIT;
    while runtime.exists() && Instant::now() < until {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!runtime.exists(), "scripted worker cleanup did not finish");
    assert_clean(&rig.stderr);
    assert!(check_state_logs(&rig.dir) >= 2);
}
