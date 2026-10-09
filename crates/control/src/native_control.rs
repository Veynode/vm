//! Same-UID private persistent native console channel, distinct from MCP/RPC EOF.
use super::*;
use native_input::{Attach, Message};
use std::time::Instant;
const RPC_TIMEOUT: Duration = Duration::from_secs(4);
#[derive(Debug)]
struct CleanupUnconfirmed;
impl std::fmt::Display for CleanupUnconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("native input cleanup unconfirmed")
    }
}
impl std::error::Error for CleanupUnconfirmed {}
struct Session {
    manager: Arc<Mutex<Manager>>,
    attach: Attach,
    owner: String,
    epoch: Option<u64>,
    active: bool,
}
impl Session {
    fn bound<'a>(&self, m: &'a mut Manager) -> Result<&'a mut Running> {
        if m.native_connections
            .get(&self.attach.id)
            .is_none_or(|(owner, worker)| {
                owner != &self.owner || worker != &self.attach.worker_instance
            })
        {
            bail!("native connection replaced")
        }
        let run = m
            .running
            .get_mut(&self.attach.id)
            .context("native worker unavailable")?;
        if run.poll_exit()?.is_some()
            || run.bridge.lock().unwrap().worker_instance != self.attach.worker_instance
        {
            bail!("native worker replaced")
        }
        Ok(run)
    }
    fn rpc(&self, method: &str, arguments: Value) -> Result<Value> {
        let (rx, number, pending) = {
            let mut m = self.manager.lock().unwrap();
            // Serialize the final authority check with the actual worker write.
            // A prior check outside this lock cannot fence a concurrent human action.
            if ["native_begin", "native_event", "native_heartbeat"].contains(&method) {
                m.leases.lock().unwrap().native_check(
                    &self.attach.id,
                    &self.owner,
                    arguments["epoch"]
                        .as_u64()
                        .context("native epoch missing")?,
                    Instant::now(),
                    true,
                )?;
            }
            let run = self.bound(&mut m)?;
            let number = run.next;
            run.next = run
                .next
                .checked_add(1)
                .context("worker request id exhausted")?;
            let (tx, rx) = mpsc::channel();
            run.pending.lock().unwrap().insert(number, tx.into());
            if let Err(e) = write_json(
                &mut run.stdin,
                &json!({"id":number,"method":method,"arguments":arguments}),
            ) {
                run.pending.lock().unwrap().remove(&number);
                return Err(e);
            }
            (rx, number, run.pending.clone())
        };
        let result = rx.recv_timeout(RPC_TIMEOUT);
        pending.lock().unwrap().remove(&number);
        let value = result.context("native worker response unavailable")?;
        if value.get("error").is_some() {
            bail!("native worker rejected operation")
        }
        value
            .get("result")
            .filter(|r| r.is_object())
            .cloned()
            .context("invalid native worker result")
    }
    fn cleanup(&self, all: bool, stream: &UnixStream) -> Result<()> {
        let completion = {
            let mut m = self.manager.lock().unwrap();
            let exec_revocations = m.exec_retirements.lock().unwrap().begin(
                if all { None } else { Some(&self.owner) },
                Some(&self.attach.id),
            );
            Completion::CancelJobs {
                dir: m.dir.clone(),
                file_sessions: Vec::new(),
                input_revocations: m.input_revocations(
                    if all { None } else { Some(&self.owner) },
                    Some(&self.attach.id),
                )?,
                input_retirements: m.input_retirements.clone(),
                exec_retirements: m.exec_retirements.clone(),
                exec_revocations,
                response: json!({}),
                jobs: if all {
                    m.jobs
                        .lock()
                        .unwrap()
                        .running(&self.attach.id, Instant::now())
                } else {
                    Vec::new()
                },
            }
        };
        completion.wait(stream).context(CleanupUnconfirmed)?;
        Ok(())
    }
    fn lease(&self, epoch: u64, active: bool) -> Result<()> {
        let mut m = self.manager.lock().unwrap();
        self.bound(&mut m)?;
        let r = m.leases.lock().unwrap().native_check(
            &self.attach.id,
            &self.owner,
            epoch,
            Instant::now(),
            active,
        );
        r
    }
    fn pause(&mut self) {
        self.active = false;
        if let Some(epoch) = self.epoch {
            let m = self.manager.lock().unwrap();
            let _ = m.leases.lock().unwrap().native_pause(
                &self.attach.id,
                &self.owner,
                epoch,
                Instant::now(),
            );
        }
    }
    fn handle(&mut self, message: &Message, stream: &UnixStream) -> Result<Value> {
        let sequence = message.sequence();
        match message {
            Message::TakeControl { .. } => {
                self.active = false;
                let epoch = {
                    let mut m = self.manager.lock().unwrap();
                    self.bound(&mut m)?;
                    let e = m.leases.lock().unwrap().native_take(
                        &self.attach.id,
                        &self.owner,
                        Instant::now(),
                    )?;
                    e
                };
                self.epoch = Some(epoch);
                self.cleanup(true, stream)?;
                {
                    let mut m = self.manager.lock().unwrap();
                    self.bound(&mut m)?;
                    // Cleanup can take seconds. Rearm only this still-owned epoch.
                    let mut leases = m.leases.lock().unwrap();
                    leases.native_check(
                        &self.attach.id,
                        &self.owner,
                        epoch,
                        Instant::now(),
                        false,
                    )?;
                    leases.native_activate(&self.attach.id, &self.owner, epoch, Instant::now())?;
                    m.input_retirements
                        .lock()
                        .unwrap()
                        .record(InputRetirement {
                            owner: self.owner.clone(),
                            vm: self.attach.id.clone(),
                            epoch,
                            worker_instance: self.attach.worker_instance.clone(),
                        })?;
                }
                let reply=self.rpc("native_begin",json!({"owner":self.owner,"epoch":epoch,"sequence":sequence,"display_instance":self.attach.display_instance}))?;
                self.lease(epoch, true)?;
                if reply["ready"] != true
                    || reply["epoch"].as_u64() != Some(epoch)
                    || peer_disconnected(&[stream.as_raw_fd()])
                {
                    bail!("native begin unconfirmed")
                }
                self.active = true;
                Ok(json!({"ready":true,"epoch":epoch}))
            }
            Message::Heartbeat { epoch, .. } => {
                if !self.active {
                    self.bound(&mut self.manager.lock().unwrap())?;
                    return Ok(json!({"ready":false}));
                }
                let epoch = epoch.context("active heartbeat requires epoch")?;
                self.lease(epoch, true)?;
                let reply = self.rpc(
                    "native_heartbeat",
                    json!({"owner":self.owner,"epoch":epoch,"sequence":sequence}),
                )?;
                if reply["ready"] != true || reply["epoch"].as_u64() != Some(epoch) {
                    bail!("native heartbeat rejected")
                }
                self.manager
                    .lock()
                    .unwrap()
                    .leases
                    .lock()
                    .unwrap()
                    .native_renew(&self.attach.id, &self.owner, epoch, Instant::now())?;
                Ok(json!({"ready":true,"epoch":epoch}))
            }
            Message::Event {
                epoch,
                frame,
                event,
                ..
            } => {
                if !self.active {
                    bail!("native input suspended")
                };
                self.lease(*epoch, true)?;
                if frame.display_instance != self.attach.display_instance {
                    bail!("foreign display")
                }
                event.qmp(frame)?;
                let result=self.rpc("native_event",json!({"owner":self.owner,"epoch":epoch,"sequence":sequence,"frame":frame,"event":event}))?;
                self.lease(*epoch, true)?;
                if result["dispatched"] != true
                    || result["guest_delivery_confirmed"] != false
                    || result["epoch"].as_u64() != Some(*epoch)
                {
                    bail!("invalid native event acknowledgement")
                }
                self.manager
                    .lock()
                    .unwrap()
                    .leases
                    .lock()
                    .unwrap()
                    .native_renew(&self.attach.id, &self.owner, *epoch, Instant::now())?;
                Ok(result)
            }
            Message::Suspend { epoch, .. } | Message::ReturnAgent { epoch, .. } => {
                self.lease(*epoch, false)?;
                self.pause();
                self.cleanup(false, stream)?;
                let m = self.manager.lock().unwrap();
                let mut leases = m.leases.lock().unwrap();
                let next = if matches!(message, Message::ReturnAgent { .. }) {
                    leases.native_return(&self.attach.id, &self.owner, *epoch, Instant::now())?
                } else {
                    leases.native_suspend(&self.attach.id, &self.owner, *epoch, Instant::now())?
                };
                self.epoch = if matches!(message, Message::ReturnAgent { .. }) {
                    None
                } else {
                    Some(next)
                };
                Ok(json!({"ready":false,"released":true,"epoch":next}))
            }
            Message::Detach { .. } => {
                self.pause();
                self.cleanup(false, stream)?;
                Ok(json!({"ready":false,"released":true}))
            }
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let mut m = self.manager.lock().unwrap();
        m.leases
            .lock()
            .unwrap()
            .native_detach(&self.attach.id, &self.owner, Instant::now());
        if m.native_connections
            .get(&self.attach.id)
            .is_some_and(|(owner, worker)| {
                owner == &self.owner && worker == &self.attach.worker_instance
            })
        {
            m.native_connections.remove(&self.attach.id);
        }
    }
}
fn line(stream: &mut UnixStream) -> Result<Option<Vec<u8>>> {
    // One absolute deadline for the entire frame; a slow byte stream cannot renew it.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    loop {
        stream.set_read_timeout(Some(
            deadline
                .checked_duration_since(Instant::now())
                .context("native channel idle")?,
        ))?;
        let mut b = [0];
        match stream.read(&mut b) {
            Ok(0) => {
                if bytes.is_empty() {
                    return Ok(None);
                }
                bail!("partial native frame")
            }
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
        if b[0] == b'\n' {
            return Ok(Some(bytes));
        }
        bytes.push(b[0]);
        if bytes.len() > native_input::MAX_LINE {
            bail!("native request exceeds bound")
        }
    }
}
fn same_uid(stream: &UnixStream) -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let mut uid = 0;
        let mut gid = 0;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(uid == unsafe { libc::geteuid() })
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
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(cred.uid == unsafe { libc::geteuid() })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = stream;
        bail!("native peer identity unsupported")
    }
}
pub(super) fn serve(
    mut stream: UnixStream,
    manager: Arc<Mutex<Manager>>,
    request: Value,
    admitted: Instant,
) -> Result<()> {
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let setup = (|| -> Result<Session> {
        if admitted.elapsed() > Duration::from_secs(30)
            || serde_json::to_vec(&request)?.len() > native_input::MAX_LINE
            || request.as_object().is_none_or(|o| o.len() != 2)
        {
            bail!("invalid native attach")
        }
        let attach: Attach = serde_json::from_value(request["arguments"].clone())?;
        if attach.protocol_version != native_input::PROTOCOL
            || !native_input::hex(&attach.worker_instance, 64)
            || !native_input::hex(&attach.display_instance, 32)
        {
            bail!("invalid native binding")
        }
        if !same_uid(&stream)? {
            bail!("native peer identity mismatch")
        }
        let owner = bridge::State::new()?.worker_instance;
        {
            let mut m = manager.lock().unwrap();
            if admitted.elapsed() > Duration::from_secs(30)
                || peer_disconnected(&[stream.as_raw_fd()])
            {
                bail!("native attach expired or disconnected")
            }
            if m.configs
                .get(&attach.id)
                .is_none_or(|c| c.backend != Backend::QemuHvf)
                || m.qemu_window["native_input_protocol"] != 1
                || m.native_connections
                    .get(&attach.id)
                    .is_some_and(|(_, worker)| worker == &attach.worker_instance)
            {
                bail!("native control unavailable")
            }
            let run = m
                .running
                .get_mut(&attach.id)
                .context("worker unavailable")?;
            if run.poll_exit()?.is_some()
                || run.bridge.lock().unwrap().worker_instance != attach.worker_instance
            {
                bail!("worker binding mismatch")
            }
            m.native_connections.insert(
                attach.id.clone(),
                (owner.clone(), attach.worker_instance.clone()),
            );
        }
        let session = Session {
            manager: manager.clone(),
            attach,
            owner,
            epoch: None,
            active: false,
        };
        let status = session.rpc("window_status", json!({}))?;
        if status["display_instance"] != session.attach.display_instance
            || status["native_input_protocol"] != 1
        {
            bail!("display binding mismatch")
        }
        Ok(session)
    })();
    let mut session = match setup {
        Ok(s) => s,
        Err(_) => {
            write_json(
                &mut stream,
                &json!({"error":{"code":"unavailable","message":"Native console binding unavailable"}}),
            )?;
            return Ok(());
        }
    };
    let outcome = (|| -> Result<()> {
        write_json(
            &mut stream,
            &json!({"result":{"native_control_protocol":1,"native_session":session.owner,"ready":false}}),
        )?;
        let mut expected = 1u64;
        while let Some(bytes) = line(&mut stream)? {
            let message: Message = serde_json::from_slice(&bytes)?;
            if message.sequence() != expected {
                bail!("native sequence mismatch")
            };
            expected = expected
                .checked_add(1)
                .context("native sequence exhausted")?;
            let result = session.handle(&message, &stream);
            let detach = matches!(message, Message::Detach { .. });
            match result {
                Ok(result) => write_json(
                    &mut stream,
                    &json!({"sequence":message.sequence(),"result":result}),
                )?,
                Err(error) => {
                    session.pause();
                    let clean = error.downcast_ref::<CleanupUnconfirmed>().is_none()
                        && session.cleanup(false, &stream).is_ok();
                    let code = if !clean {
                        "cleanup_unconfirmed"
                    } else if matches!(message, Message::Event { .. }) {
                        "stale_frame"
                    } else {
                        "authority_lost"
                    };
                    write_json(
                        &mut stream,
                        &json!({"sequence":message.sequence(),"error":{"code":code,"message":"Native control operation rejected"}}),
                    )?;
                    if !clean {
                        bail!("native cleanup unconfirmed")
                    }
                }
            }
            if detach {
                break;
            }
        }
        Ok(())
    })();
    session.pause();
    let cleanup = session.cleanup(false, &stream);
    drop(session);
    outcome.and(cleanup)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        manager: Arc<Mutex<Manager>>,
        stop: Arc<AtomicBool>,
        server: Option<thread::JoinHandle<()>>,
        _dir: tempfile::TempDir,
    }
    impl Fixture {
        fn new() -> Self {
            Self::with_revoke_failure(false)
        }
        fn with_revoke_failure(fail: bool) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let listener = UnixListener::bind(dir.path().join("control.sock")).unwrap();
            listener.set_nonblocking(true).unwrap();
            let mut m = Manager::load(
                dir.path().into(),
                "/unused".into(),
                MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
            )
            .unwrap();
            m.configs.insert("vm".into(),serde_json::from_value(json!({"id":"vm","backend":"qemu-hvf","guest":"linux","cpus":1,"memory_mib":512})).unwrap());
            m.qemu = Some(backend::QemuRegistration {
                worker: "/unused".into(),
                launcher: "/unused".into(),
                library: "/unused".into(),
                resources: "/unused".into(),
                spice_library: None,
                screenshot_helper: None,
                console_broker: Some("/unused".into()),
                console_helper: Some("/unused".into()),
                venus_runtime: None,
            });
            m.qemu_window = json!({"native_input_protocol":1});
            let script = r#"import json,sys
for line in sys.stdin:
 r=json.loads(line);a=r.get('arguments',{});method=r['method']
 if method=='input_revoke' and sys.argv[1]=='fail':
  print(json.dumps({'id':r['id'],'error':'injected release failure'}),flush=True);continue
 if method=='window_status': out={'display_instance':'d'*32,'native_input_protocol':1}
 elif method=='input_revoke': out={'released':True,'synthetic_release_dispatched':True,'through_epoch':a['through_epoch'],'guest_delivery_confirmed':False}
 else: out={'ready':True,'epoch':a['epoch'],'dispatched':True,'guest_delivery_confirmed':False}
 print(json.dumps({'id':r['id'],'result':out}),flush=True)
"#;
            let mut child = Command::new("/usr/bin/python3")
                .args(["-u", "-c", script, if fail { "fail" } else { "okay" }])
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            let pending = Arc::new(Mutex::new(BTreeMap::<u64, PendingReply>::new()));
            let sink = pending.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                while let Ok(Some(bytes)) = read_frame(&mut reader) {
                    let v: Value = serde_json::from_slice(&bytes).unwrap();
                    if let Some(tx) = sink.lock().unwrap().remove(&v["id"].as_u64().unwrap()) {
                        let _ = tx.send(v);
                    }
                }
            });
            m.running.insert(
                "vm".into(),
                Running {
                    bridge: Arc::new(Mutex::new(bridge::State::new().unwrap())),
                    child,
                    stdin,
                    pending,
                    next: 1,
                    assigned_memory_mib: 512,
                    expected_exit: false,
                    execution_quiescing: false,
                    exit_status: None,
                    disk_identity: None,
                    state: Arc::new(Mutex::new(json!({"state":"running"}))),
                },
            );
            let manager = Arc::new(Mutex::new(m));
            let serve_manager = manager.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let shutdown = stop.clone();
            let server = thread::spawn(move || {
                while !shutdown.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            let m = serve_manager.clone();
                            thread::spawn(move || {
                                let _ = handle_connection(stream, m, Instant::now());
                            });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(e) => panic!("{e}"),
                    }
                }
            });
            Self {
                manager,
                stop,
                server: Some(server),
                _dir: dir,
            }
        }
        fn attach(
            &self,
            worker: Option<String>,
        ) -> (BufReader<UnixStream>, thread::JoinHandle<Result<()>>) {
            let (worker_instance, manager) = {
                let m = self.manager.lock().unwrap();
                (
                    worker.unwrap_or_else(|| {
                        m.running["vm"]
                            .bridge
                            .lock()
                            .unwrap()
                            .worker_instance
                            .clone()
                    }),
                    self.manager.clone(),
                )
            };
            let (client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(8)))
                .unwrap();
            let thread = thread::spawn(move || {
                serve(
                    server,
                    manager,
                    json!({"method":"native_attach","arguments":{"protocol_version":1,"id":"vm","worker_instance":worker_instance,"display_instance":"d".repeat(32)}}),
                    Instant::now(),
                )
            });
            (BufReader::new(client), thread)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            self.server.take().unwrap().join().unwrap();
            for run in self.manager.lock().unwrap().running.values_mut() {
                let _ = run.force_stop();
            }
        }
    }
    fn read(reader: &mut BufReader<UnixStream>) -> Value {
        serde_json::from_slice(&read_frame(reader).unwrap().unwrap()).unwrap()
    }
    fn request(reader: &mut BufReader<UnixStream>, value: Value) -> Value {
        write_json(reader.get_mut(), &value).unwrap();
        read(reader)
    }
    #[test]
    fn persistent_native_suspend_retains_hold_return_is_explicit_and_eof_releases() {
        let f = Fixture::new();
        let (mut client, thread) = f.attach(None);
        assert_eq!(read(&mut client)["result"]["ready"], false);
        let a = request(&mut client, json!({"sequence":1,"method":"take_control"}));
        let epoch = a["result"]["epoch"].as_u64().unwrap();
        assert_eq!(a["result"]["ready"], true);
        assert!(f
            .manager
            .lock()
            .unwrap()
            .leases
            .lock()
            .unwrap()
            .acquire("vm", &"a".repeat(64), Instant::now())
            .is_err());
        let s = request(
            &mut client,
            json!({"sequence":2,"method":"suspend","epoch":epoch}),
        );
        let next = s["result"]["epoch"].as_u64().unwrap();
        assert!(next > epoch);
        assert_eq!(s["result"]["released"], true);
        assert_eq!(
            request(&mut client, json!({"sequence":3,"method":"heartbeat"}))["result"]["ready"],
            false
        );
        assert!(f
            .manager
            .lock()
            .unwrap()
            .leases
            .lock()
            .unwrap()
            .acquire("vm", &"a".repeat(64), Instant::now())
            .is_err());
        assert_eq!(
            request(
                &mut client,
                json!({"sequence":4,"method":"return_agent","epoch":next})
            )["result"]["released"],
            true
        );
        f.manager
            .lock()
            .unwrap()
            .leases
            .lock()
            .unwrap()
            .acquire("vm", &"a".repeat(64), Instant::now())
            .unwrap();
        let second = request(&mut client, json!({"sequence":5,"method":"take_control"}));
        assert_eq!(second["result"]["ready"], true);
        drop(client);
        thread.join().unwrap().unwrap();
        let m = f.manager.lock().unwrap();
        assert!(m
            .input_retirements
            .lock()
            .unwrap()
            .records(None, None)
            .is_empty());
        assert!(m
            .leases
            .lock()
            .unwrap()
            .acquire("vm", &"a".repeat(64), Instant::now())
            .is_err());
    }
    #[test]
    fn wrong_incarnation_and_replayed_sequence_fail_closed() {
        let f = Fixture::new();
        let (mut c, t) = f.attach(Some("f".repeat(64)));
        assert_eq!(read(&mut c)["error"]["code"], "unavailable");
        t.join().unwrap().unwrap();
        let (mut c, t) = f.attach(None);
        read(&mut c);
        assert_eq!(
            request(&mut c, json!({"sequence":1,"method":"take_control"}))["result"]["ready"],
            true
        );
        write_json(c.get_mut(), &json!({"sequence":1,"method":"heartbeat"})).unwrap();
        assert!(read_frame(&mut c).unwrap().is_none());
        assert!(t.join().unwrap().is_err());
        assert!(f
            .manager
            .lock()
            .unwrap()
            .input_retirements
            .lock()
            .unwrap()
            .records(None, None)
            .is_empty());
    }
    #[test]
    fn native_failed_suspend_never_reports_release_and_keeps_obligation() {
        let f = Fixture::with_revoke_failure(true);
        let (mut c, t) = f.attach(None);
        read(&mut c);
        let epoch = request(&mut c, json!({"sequence":1,"method":"take_control"}))["result"]
            ["epoch"]
            .as_u64()
            .unwrap();
        let reply = request(
            &mut c,
            json!({"sequence":2,"method":"suspend","epoch":epoch}),
        );
        assert_eq!(reply["error"]["code"], "cleanup_unconfirmed");
        assert!(reply.get("result").is_none());
        assert!(read_frame(&mut c).unwrap().is_none());
        assert!(t.join().unwrap().is_err());
        let m = f.manager.lock().unwrap();
        assert_eq!(
            m.input_retirements
                .lock()
                .unwrap()
                .records(None, None)
                .len(),
            1
        );
        assert!(m
            .leases
            .lock()
            .unwrap()
            .acquire("vm", &"b".repeat(64), Instant::now())
            .is_err());
    }
    #[test]
    fn native_idle_channel_closes_and_releases_without_returning_agent() {
        let f = Fixture::new();
        let (mut c, t) = f.attach(None);
        read(&mut c);
        assert_eq!(
            request(&mut c, json!({"sequence":1,"method":"take_control"}))["result"]["ready"],
            true
        );
        assert!(read_frame(&mut c).unwrap().is_none());
        assert!(t.join().unwrap().is_err());
        let m = f.manager.lock().unwrap();
        assert!(m
            .input_retirements
            .lock()
            .unwrap()
            .records(None, None)
            .is_empty());
        assert!(m
            .leases
            .lock()
            .unwrap()
            .acquire("vm", &"b".repeat(64), Instant::now())
            .is_err());
    }
    #[test]
    fn takeover_between_precheck_and_worker_dispatch_cannot_write_old_event() {
        let f = Fixture::new();
        let owner = "b".repeat(64);
        let (worker, epoch) = {
            let mut m = f.manager.lock().unwrap();
            let instance = m.running["vm"]
                .bridge
                .lock()
                .unwrap()
                .worker_instance
                .clone();
            m.native_connections
                .insert("vm".into(), (owner.clone(), instance));
            let epoch = m
                .leases
                .lock()
                .unwrap()
                .native_take("vm", &owner, Instant::now())
                .unwrap();
            let worker = m.running["vm"]
                .bridge
                .lock()
                .unwrap()
                .worker_instance
                .clone();
            (worker, epoch)
        };
        let s = Session {
            manager: f.manager.clone(),
            attach: Attach {
                protocol_version: 1,
                id: "vm".into(),
                worker_instance: worker,
                display_instance: "d".repeat(32),
            },
            owner,
            epoch: Some(epoch),
            active: true,
        };
        s.lease(epoch, true).unwrap();
        // Deterministic interleaving: the earlier check passed, then the local
        // human action changes authority before rpc obtains the dispatch lock.
        f.manager
            .lock()
            .unwrap()
            .leases
            .lock()
            .unwrap()
            .human("vm", false, Instant::now())
            .unwrap();
        let before = f.manager.lock().unwrap().running["vm"].next;
        for method in ["native_event", "native_heartbeat", "native_begin"] {
            assert!(s.rpc(method, json!({"epoch":epoch,"sequence":9})).is_err());
        }
        assert_eq!(
            f.manager.lock().unwrap().running["vm"].next,
            before,
            "no worker request allocated or written"
        );
    }
    #[test]
    fn general_human_release_waits_for_original_worker_proof_and_cannot_clear_new_hold() {
        for concurrent in [false, true] {
            let f = Fixture::new();
            let (mut c, t) = f.attach(None);
            read(&mut c);
            assert_eq!(
                request(&mut c, json!({"sequence":1,"method":"take_control"}))["result"]["ready"],
                true
            );
            let mut pending = None;
            f.manager
                .lock()
                .unwrap()
                .dispatch(
                    json!({"method":"human_release","arguments":{"id":"vm"}}),
                    &mut pending,
                )
                .unwrap();
            assert!(f
                .manager
                .lock()
                .unwrap()
                .leases
                .lock()
                .unwrap()
                .acquire("vm", &"a".repeat(64), Instant::now())
                .is_err());
            if concurrent {
                f.manager
                    .lock()
                    .unwrap()
                    .leases
                    .lock()
                    .unwrap()
                    .human("vm", true, Instant::now())
                    .unwrap();
            }
            let (peer, _) = UnixStream::pair().unwrap();
            let result = pending.unwrap().wait(&peer);
            if concurrent {
                assert!(result.is_err());
                assert!(f
                    .manager
                    .lock()
                    .unwrap()
                    .leases
                    .lock()
                    .unwrap()
                    .acquire("vm", &"a".repeat(64), Instant::now())
                    .is_err());
            } else {
                let result = result.unwrap();
                assert_eq!(result["human_control"], false);
                assert_eq!(result["input_release"]["confirmed"], true);
                f.manager
                    .lock()
                    .unwrap()
                    .leases
                    .lock()
                    .unwrap()
                    .acquire("vm", &"a".repeat(64), Instant::now())
                    .unwrap();
            }
            drop(c);
            t.join().unwrap().unwrap();
        }
    }
    #[test]
    fn general_human_release_failure_keeps_human_hold_and_exact_obligation() {
        let f = Fixture::with_revoke_failure(true);
        let (mut c, t) = f.attach(None);
        read(&mut c);
        request(&mut c, json!({"sequence":1,"method":"take_control"}));
        let original = f
            .manager
            .lock()
            .unwrap()
            .input_retirements
            .lock()
            .unwrap()
            .records(None, None);
        let mut pending = None;
        f.manager
            .lock()
            .unwrap()
            .dispatch(
                json!({"method":"human_release","arguments":{"id":"vm"}}),
                &mut pending,
            )
            .unwrap();
        let (peer, _) = UnixStream::pair().unwrap();
        assert!(pending.unwrap().wait(&peer).is_err());
        assert_eq!(
            f.manager
                .lock()
                .unwrap()
                .input_retirements
                .lock()
                .unwrap()
                .records(None, None),
            original
        );
        assert!(f
            .manager
            .lock()
            .unwrap()
            .leases
            .lock()
            .unwrap()
            .acquire("vm", &"a".repeat(64), Instant::now())
            .is_err());
        drop(c);
        assert!(t.join().unwrap().is_err());
    }
    #[test]
    fn new_worker_attach_survives_old_connection_finalization() {
        let f = Fixture::new();
        let owner = "f".repeat(64);
        let old_worker = "e".repeat(64);
        f.manager
            .lock()
            .unwrap()
            .native_connections
            .insert("vm".into(), (owner.clone(), old_worker.clone()));
        let old = Session {
            manager: f.manager.clone(),
            attach: Attach {
                protocol_version: 1,
                id: "vm".into(),
                worker_instance: old_worker,
                display_instance: "d".repeat(32),
            },
            owner,
            epoch: None,
            active: false,
        };
        let (mut c, t) = f.attach(None);
        assert_eq!(read(&mut c)["result"]["ready"], false);
        let epoch = request(&mut c, json!({"sequence":1,"method":"take_control"}))["result"]
            ["epoch"]
            .as_u64()
            .unwrap();
        drop(old);
        assert_eq!(
            request(
                &mut c,
                json!({"sequence":2,"method":"heartbeat","epoch":epoch})
            )["result"]["ready"],
            true
        );
        drop(c);
        t.join().unwrap().unwrap();
    }
}
