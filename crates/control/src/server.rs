//! The daemon's socket: admitting peers, serving connections, `call`.
use super::*;

pub fn serve(dir: PathBuf, backend: PathBuf, memory_budget_mib: Option<u64>) -> Result<()> {
    serve_with_qemu(dir, backend, memory_budget_mib, None)
}
pub fn serve_with_qemu(
    dir: PathBuf,
    backend: PathBuf,
    memory_budget_mib: Option<u64>,
    qemu: Option<QemuRegistration>,
) -> Result<()> {
    serve_with_worker_key_event_log(dir, backend, memory_budget_mib, qemu, None, Vec::new())
}
pub fn serve_with_worker_key_event_log(
    dir: PathBuf,
    backend: PathBuf,
    memory_budget_mib: Option<u64>,
    qemu: Option<QemuRegistration>,
    worker_key_event_log_dir: Option<PathBuf>,
    notices: Vec<String>,
) -> Result<()> {
    let _log_drain = LogDrain;
    let worker_key_event_log_dir = worker_key_event_log_dir
        .map(|path| validate_worker_key_event_log_dir(&path))
        .transpose()?;
    let mut notices = notices;
    let qemu = qemu.and_then(|registration| usable_qemu(registration, &mut notices));
    let memory = MemoryBudget::detect(memory_budget_mib)?;
    private_dir(&dir)?;
    // Snapshot state is opened component by component without following symlinks.
    let dir = fs::canonicalize(&dir)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join("daemon.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another daemon holds this state directory")
    }
    let socket = dir.join("control.sock");
    if socket.exists() {
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let mut manager = Manager::load(dir, backend, memory)?;
    manager.worker_key_event_log_dir = worker_key_event_log_dir;
    manager.qemu_window = match qemu
        .as_ref()
        .and_then(|q| q.console_broker.as_ref().zip(q.console_helper.as_ref()))
    {
        Some((broker, helper)) => match backend::probe_broker_window(broker, helper) {
            Ok(window) => window,
            Err(error) => {
                notices.push(format!(
                    "QEMU machine windows are off: the console helper did not answer its check ({error:#})"
                ));
                json!({"native_window":false,"display_only":true,"input_enabled":false})
            }
        },
        None => json!({"native_window":false,"display_only":true,"input_enabled":false}),
    };
    manager.qemu = qemu;
    for notice in &notices {
        log_line(&format!("vm-control: {notice}"));
    }
    manager.notices.extend(notices);
    manager.validate_registry();
    let manager = Arc::new(Mutex::new(manager));
    spawn_exit_monitor(&manager);
    idle::spawn_monitor(&manager);
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for connection in listener.incoming() {
        let stream = connection?;
        let admitted_at = std::time::Instant::now();
        if active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 32 {
            active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            drop(stream);
            continue;
        }
        let manager = manager.clone();
        let active = active.clone();
        thread::spawn(move || {
            let _ = serve_connection(stream, manager, admitted_at, peer_euid);
            active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
    }
    Ok(())
}
/// The QEMU registration the daemon can serve: a broken one turns QEMU, its
/// console or its Venus GPUs off, with a notice, instead of stopping the
/// daemon and the machines of the other backend.
pub(crate) fn usable_qemu(
    mut registration: QemuRegistration,
    notices: &mut Vec<String>,
) -> Option<QemuRegistration> {
    if registration.console_broker.is_some() != registration.console_helper.is_some() {
        notices.push(
            "QEMU machine windows and screenshots through the console are off: the console needs both its broker and its helper".into(),
        );
        registration.console_broker = None;
        registration.console_helper = None;
    }
    if let Err(error) = registration.validate_core() {
        notices.push(format!("QEMU machines are off: {error:#}"));
        return None;
    }
    if let Err(error) = registration.validate_venus() {
        // Kept registered: a Venus or Neptune start checks it again and
        // refuses with the reason; virgl and no GPU never read it.
        notices.push(format!("Venus and Neptune GPUs are off: {error:#}"));
    }
    Some(registration)
}
/// Effective uid of the process at the other end of a connected `control.sock` stream, as recorded by the kernel.
pub(crate) fn peer_euid(stream: &UnixStream) -> Result<libc::uid_t> {
    #[cfg(target_os = "macos")]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(uid)
    }
    #[cfg(target_os = "linux")]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut length,
            )
        } != 0
            || length as usize != std::mem::size_of::<libc::ucred>()
        {
            bail!("control peer credential unavailable")
        }
        Ok(cred.uid)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = stream;
        bail!("control peer identity unsupported on this platform")
    }
}
/// Admission for one accepted `control.sock` connection. The socket file mode (0600 inside a 0700 directory) is the first
/// barrier; this is the second, and it does not depend on the file system: the peer's effective uid must equal the
/// daemon's. An unreadable credential is a refusal. `peer_uid` is a parameter only so a test can present a foreign uid
/// without root; the daemon always passes `peer_euid`.
pub(crate) fn admit_peer(
    stream: &UnixStream,
    peer_uid: impl FnOnce(&UnixStream) -> Result<libc::uid_t>,
) -> Result<()> {
    let own = unsafe { libc::geteuid() };
    match peer_uid(stream) {
        Ok(uid) if uid == own => Ok(()),
        Ok(_) => bail!("control connection refused: peer uid differs from daemon uid"),
        Err(_) => bail!("control connection refused: peer credential unavailable"),
    }
}
/// Refuses a foreign or unidentifiable peer before a single request byte is read; the stream is dropped without a reply.
pub(crate) fn serve_connection(
    stream: UnixStream,
    manager: Arc<Mutex<Manager>>,
    admitted_at: std::time::Instant,
    peer_uid: impl FnOnce(&UnixStream) -> Result<libc::uid_t>,
) -> Result<()> {
    admit_peer(&stream, peer_uid)?;
    handle_connection(stream, manager, admitted_at)
}
pub(crate) fn handle_connection(
    mut stream: UnixStream,
    manager: Arc<Mutex<Manager>>,
    admitted_at: std::time::Instant,
) -> Result<()> {
    // Darwin may return EINVAL when setting socket options after its peer closed.
    // A disconnected socket cannot block reads: drain its already-buffered cleanup
    // request. Other option failures are fatal.
    for result in [
        stream.set_read_timeout(Some(Duration::from_secs(15))),
        stream.set_write_timeout(Some(Duration::from_secs(15))),
    ] {
        if let Err(error) = result {
            if !peer_disconnected(&[stream.as_raw_fd()]) {
                return Err(error.into());
            }
        }
    }
    let mut reader = BufReader::new(stream.try_clone()?);
    let frame = read_frame(&mut reader)?.context("empty request")?;
    let request: Value = serde_json::from_slice(&frame)?;
    if request["method"] == "hello" {
        return api_server::serve(stream, reader, manager, request, admitted_at);
    }
    drop(reader);
    if request["method"] == "native_attach" {
        return native_control::serve(stream, manager, request, admitted_at);
    }
    let method = request["method"].as_str().unwrap_or("").to_string();
    let result = vm_result(&stream, &manager, &request, admitted_at);
    write_vm_reply(&mut stream, &method, result, admitted_at, false)
}
/// A VM-layer request dispatched under the manager lock, with its deferred answer awaited.
pub(crate) fn vm_result(
    stream: &UnixStream,
    manager: &Arc<Mutex<Manager>>,
    request: &Value,
    admitted_at: std::time::Instant,
) -> Result<Value> {
    let mut deferred = None;
    let mut waited = false;
    let value = loop {
        let mut manager = manager.lock().unwrap();
        // Include socket reads and queue/manager-lock delay.
        if !waited && admitted_at.elapsed() > Duration::from_secs(30) {
            bail!("controller request expired before dispatch")
        }
        match manager.dispatch(request.clone(), &mut deferred) {
            // A worker saving its machine state answers once it has stopped;
            // the request then restores the VM (`idle.rs`).
            Err(error)
                if error.to_string() == idle::HIBERNATING
                    && admitted_at.elapsed() < idle::SAVE_WAIT =>
            {
                drop(manager);
                waited = true;
                std::thread::sleep(Duration::from_millis(100));
            }
            result => break result?,
        }
    };
    match deferred {
        Some(p) => p.wait(stream),
        None => Ok(value),
    }
}
/// The answer to a VM-layer request; `coded` adds API 1's error code.
pub(crate) fn write_vm_reply(
    stream: &mut UnixStream,
    method: &str,
    result: Result<Value>,
    admitted_at: std::time::Instant,
    coded: bool,
) -> Result<()> {
    if method == "screenshot" {
        widen_send_buffer(stream);
    }
    let writing = std::time::Instant::now();
    let written = write_json(
        stream,
        &match result {
            Ok(value) => json!({"result":value}),
            Err(e) => {
                let mut reply = if coded {
                    veynodevm_api::refusal(&e)
                } else {
                    json!({"error":e.to_string()})
                };
                // Only a refusal of the scaled screenshot form carries a second field.
                screenshot_scaled::Refused::mark(&mut reply, &e);
                reply
            }
        },
    );
    if let Some(timing) = SHOT_TIMING.take() {
        log_line(&timing.line(
            admitted_at.elapsed().as_micros() as u64,
            writing.elapsed().as_micros() as u64,
        ));
    }
    written
}
pub fn call(dir: &Path, method: &str, args: Value) -> Result<Value> {
    call_reply(dir, method, args)?.map_err(|message| anyhow::anyhow!("{message}"))
}
/// `call` with the two failure classes kept apart: the outer error is a failed exchange with the
/// daemon (the request may or may not have been acted on), the inner one is the daemon's answer.
pub(crate) fn call_reply(
    dir: &Path,
    method: &str,
    args: Value,
) -> Result<std::result::Result<Value, String>> {
    let mut socket = UnixStream::connect(dir.join("control.sock"))
        .context("connect daemon; run vm-control daemon first")?;
    // Saving a large Guest's memory can outlast the usual reply deadline.
    let read_timeout = if method == container_state::SAVE {
        container_state::SAVE_TIMEOUT_SECONDS + 10
    } else if method == "driver_rpc" {
        // The daemon ends the call at its own derived deadline.
        cua_bridge::MAX_DEADLINE.as_secs() + 20
    } else {
        40
    };
    socket.set_read_timeout(Some(Duration::from_secs(read_timeout)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    write_json(&mut socket, &json!({"method":method,"arguments":args}))?;
    let capacity = if method == "screenshot" || method == "driver_rpc" {
        LARGE_REPLY_READ_BUFFER
    } else {
        8192
    };
    let bytes = read_frame_bounded(
        &mut BufReader::with_capacity(capacity, socket),
        response_limit(method),
    )?
    .context("daemon disconnected")?;
    let mut reply: Value = serde_json::from_slice(&bytes)?;
    drop(bytes);
    if let Some(error) = reply.get("error") {
        return Ok(Err(match error.as_str() {
            Some(message) => message.to_string(),
            None => error.to_string(),
        }));
    }
    Ok(Ok(reply.get_mut("result").map_or(Value::Null, Value::take)))
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    fn registration(root: &Path) -> QemuRegistration {
        let file = |name: &str| {
            let path = root.join(name);
            fs::write(&path, b"fixture").unwrap();
            path
        };
        let resources = root.join("share");
        fs::create_dir_all(&resources).unwrap();
        QemuRegistration {
            worker: file("vm-qemu"),
            launcher: file("launcher"),
            library: file("qemu.dylib"),
            resources,
            spice_library: None,
            screenshot_helper: None,
            console_broker: None,
            console_helper: None,
            venus_runtime: None,
        }
    }

    #[test]
    fn a_broken_qemu_registration_turns_its_feature_off_instead_of_the_daemon() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut notices = Vec::new();
        assert!(usable_qemu(registration(&root), &mut notices).is_some());
        assert!(notices.is_empty());

        // QEMU itself gone: QEMU off, the daemon serves the rest.
        let gone = QemuRegistration {
            worker: root.join("gone/vm-qemu"),
            ..registration(&root)
        };
        assert!(usable_qemu(gone, &mut notices).is_none());
        assert!(
            notices[0].starts_with("QEMU machines are off") && notices[0].contains("gone/vm-qemu"),
            "{notices:?}"
        );

        // A console broker without its helper: the console is off, QEMU is not.
        let mut notices = Vec::new();
        let half = QemuRegistration {
            console_broker: Some(root.join("vm-qemu")),
            ..registration(&root)
        };
        let usable = usable_qemu(half, &mut notices).unwrap();
        assert!(usable.console_broker.is_none() && usable.console_helper.is_none());
        assert_eq!(notices.len(), 1);

        // A Venus runtime that does not check out: Venus and Neptune off, the
        // registration kept so a GPU start refuses with the reason.
        let mut notices = Vec::new();
        let venus = root.join("venus.json");
        fs::write(&venus, b"{}").unwrap();
        let usable = usable_qemu(
            QemuRegistration {
                venus_runtime: Some(venus.clone()),
                ..registration(&root)
            },
            &mut notices,
        )
        .unwrap();
        assert_eq!(usable.venus_runtime, Some(venus));
        assert!(
            notices[0].starts_with("Venus and Neptune GPUs are off"),
            "{notices:?}"
        );
    }
}
