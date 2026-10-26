//! Same-UID private persistent native console channel, distinct from MCP/RPC EOF.
//! The window sends `begin` when it gains keyboard focus and `suspend` when it
//! loses it; while begun, its input goes straight to the worker. Each begin
//! gets a new activation number (`epoch` on the wire) that binds the window's
//! frame acquisition receipts to that focus period.
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
    epoch: u64,
    began: bool,
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
            if let Some(diagnostic) =
                native_input::RejectionDiagnostic::from_wire(&value["native_input_diagnostic"])
            {
                return Err(diagnostic.into());
            }
            bail!("native worker rejected operation")
        }
        value
            .get("result")
            .filter(|r| r.is_object())
            .cloned()
            .context("invalid native worker result")
    }
    /// Releases whatever the window still holds in the guest.
    fn release(&mut self) -> Result<()> {
        self.active = false;
        if !self.began {
            return Ok(());
        }
        let reply = self
            .rpc("native_end", json!({"owner":self.owner}))
            .context(CleanupUnconfirmed)?;
        if reply["released"] != true {
            return Err(anyhow::anyhow!("native release unconfirmed").context(CleanupUnconfirmed));
        }
        self.began = false;
        Ok(())
    }
    fn handle(&mut self, message: &Message) -> Result<Value> {
        let sequence = message.sequence();
        match message {
            Message::Begin { .. } => {
                self.release()?;
                self.epoch = self
                    .epoch
                    .checked_add(1)
                    .context("native activation exhausted")?;
                let epoch = self.epoch;
                let mut args = json!({"owner":self.owner,"epoch":epoch,"sequence":sequence,"display_instance":self.attach.display_instance});
                if let Some(mode) = self.attach.frame_acquisition_protocol {
                    args["frame_acquisition_protocol"] = json!(mode);
                }
                self.began = true;
                let reply = self.rpc("native_begin", args)?;
                if reply["ready"] != true
                    || reply["epoch"].as_u64() != Some(epoch)
                    || native_input::frame_acquisition_protocol(
                        reply.get("frame_acquisition_protocol"),
                    )? != self.attach.frame_acquisition_protocol
                {
                    bail!("native begin unconfirmed")
                }
                self.active = true;
                let mut result = json!({"ready":true,"epoch":epoch});
                if let Some(mode) = self.attach.frame_acquisition_protocol {
                    result["frame_acquisition_protocol"] = json!(mode);
                }
                Ok(result)
            }
            Message::Heartbeat { epoch, .. } => {
                if !self.active {
                    self.bound(&mut self.manager.lock().unwrap())?;
                    return Ok(json!({"ready":false}));
                }
                if *epoch != Some(self.epoch) {
                    bail!("native heartbeat for another activation")
                }
                let reply = self.rpc(
                    "native_heartbeat",
                    json!({"owner":self.owner,"epoch":self.epoch,"sequence":sequence}),
                )?;
                if reply["ready"] != true || reply["epoch"].as_u64() != Some(self.epoch) {
                    bail!("native heartbeat rejected")
                }
                Ok(json!({"ready":true,"epoch":self.epoch}))
            }
            Message::Event {
                epoch,
                frame,
                event,
                diagnostic_id,
                acquisition_receipt,
                ..
            } => {
                if !self.active || *epoch != self.epoch {
                    bail!("native input suspended")
                };
                if frame.display_instance != self.attach.display_instance {
                    bail!("foreign display")
                }
                if acquisition_receipt.is_some() != self.attach.frame_acquisition_protocol.is_some()
                {
                    bail!("native frame acquisition mode mismatch")
                }
                event.qmp(frame)?;
                let mut args = json!({"owner":self.owner,"epoch":epoch,"sequence":sequence,"frame":frame,"event":event});
                if let Some(id) = diagnostic_id {
                    args["diagnostic_id"] = json!(id);
                }
                if let Some(receipt) = acquisition_receipt {
                    args["acquisition_receipt"] = json!(receipt);
                }
                let result = self.rpc("native_event", args)?;
                if result["dispatched"] != true
                    || result["guest_delivery_confirmed"] != false
                    || result["epoch"].as_u64() != Some(*epoch)
                {
                    bail!("invalid native event acknowledgement")
                }
                Ok(result)
            }
            Message::Suspend { epoch, .. } => {
                if *epoch != self.epoch {
                    bail!("native suspend for another activation")
                }
                self.release()?;
                Ok(json!({"ready":false,"released":true,"epoch":self.epoch}))
            }
            Message::Detach { .. } => {
                self.release()?;
                Ok(json!({"ready":false,"released":true}))
            }
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let mut m = self.manager.lock().unwrap();
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
            epoch: 0,
            began: false,
            active: false,
        };
        let status = session.rpc("window_status", json!({}))?;
        if status["display_instance"] != session.attach.display_instance
            || status["native_input_protocol"] != 1
        {
            bail!("display binding mismatch")
        }
        if session.attach.frame_acquisition_protocol.is_some()
            && native_input::frame_acquisition_protocol(
                status.get("native_frame_acquisition_protocol"),
            )? != Some(1)
        {
            bail!("native frame acquisition unsupported")
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
        let mut result =
            json!({"native_control_protocol":1,"native_session":session.owner,"ready":false});
        if let Some(mode) = session.attach.frame_acquisition_protocol {
            result["frame_acquisition_protocol"] = json!(mode);
        }
        write_json(&mut stream, &json!({"result":result}))?;
        let mut expected = 1u64;
        while let Some(bytes) = line(&mut stream)? {
            let message: Message = serde_json::from_slice(&bytes)?;
            if message.sequence() != expected {
                bail!("native sequence mismatch")
            };
            expected = expected
                .checked_add(1)
                .context("native sequence exhausted")?;
            let result = session.handle(&message);
            let detach = matches!(message, Message::Detach { .. });
            match result {
                Ok(result) => write_json(
                    &mut stream,
                    &json!({"sequence":message.sequence(),"result":result}),
                )?,
                Err(error) => {
                    let clean = error.downcast_ref::<CleanupUnconfirmed>().is_none()
                        && session.release().is_ok();
                    let code = if !clean {
                        "cleanup_unconfirmed"
                    } else if matches!(message, Message::Event { .. }) {
                        "stale_frame"
                    } else {
                        "authority_lost"
                    };
                    write_json(
                        &mut stream,
                        &json!({"sequence":message.sequence(),"error":{"code":code,"message":"Native control operation rejected","diagnostic":error.downcast_ref::<native_input::RejectionDiagnostic>()}}),
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
    let cleanup = session.release();
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
            Self::with_release_failure(false)
        }
        fn with_release_failure(fail: bool) -> Self {
            Self::with_diagnostic(fail, Value::Null)
        }
        fn with_diagnostic(fail: bool, diagnostic: Value) -> Self {
            Self::with_acquisition(fail, diagnostic, Value::Null, Value::Null)
        }
        fn with_acquisition(fail: bool, diagnostic: Value, capability: Value, echo: Value) -> Self {
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
 with open(sys.argv[3],'a') as log: log.write(json.dumps(r)+'\n')
 if method=='native_end' and sys.argv[1]=='fail':
  print(json.dumps({'id':r['id'],'error':'injected release failure'}),flush=True);continue
 if method=='native_event' and sys.argv[2]!='null':
  print(json.dumps({'id':r['id'],'error':'SECRET must not leave worker boundary','native_input_diagnostic':json.loads(sys.argv[2])}),flush=True);continue
 if method=='window_status': out={'display_instance':'d'*32,'native_input_protocol':1}
 elif method=='native_end': out={'ready':False,'released':True}
 else: out={'ready':True,'epoch':a['epoch'],'dispatched':True,'guest_delivery_confirmed':False}
 if method=='window_status' and sys.argv[4]!='null': out['native_frame_acquisition_protocol']=json.loads(sys.argv[4])
 if method=='native_begin' and sys.argv[5]!='null': out['frame_acquisition_protocol']=json.loads(sys.argv[5])
 print(json.dumps({'id':r['id'],'result':out}),flush=True)
"#;
            let mut child = Command::new("/usr/bin/python3")
                .args(["-u", "-c", script, if fail { "fail" } else { "okay" }])
                .arg(diagnostic.to_string())
                .arg(dir.path().join("worker-requests.jsonl"))
                .arg(capability.to_string())
                .arg(echo.to_string())
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
            self.attach_mode(worker, None)
        }
        fn attach_mode(
            &self,
            worker: Option<String>,
            mode: Option<Value>,
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
            let mut attach = json!({"method":"native_attach","arguments":{"protocol_version":1,"id":"vm","worker_instance":worker_instance,"display_instance":"d".repeat(32)}});
            if let Some(mode) = mode {
                attach["arguments"]["frame_acquisition_protocol"] = mode;
            }
            let thread = thread::spawn(move || serve(server, manager, attach, Instant::now()));
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
    fn worker_requests(f: &Fixture, method: &str) -> Vec<Value> {
        std::fs::read_to_string(f._dir.path().join("worker-requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|r| r["method"] == method)
            .collect()
    }
    #[test]
    fn acquisition_negotiation_requires_helper_and_worker_echo_before_ready() {
        for (cap, echo, attach_ok, begin_ok) in [
            (Value::Null, json!(1), false, false),
            (json!(true), json!(1), false, false),
            (json!(1), Value::Null, true, false),
            (json!(1), json!(1.0), true, false),
            (json!(1), json!(2), true, false),
            (json!(1), json!(1), true, true),
        ] {
            let f = Fixture::with_acquisition(false, Value::Null, cap, echo);
            let (mut client, thread) = f.attach_mode(None, Some(json!(1)));
            if attach_ok {
                let attached = read(&mut client);
                assert_eq!(attached["result"]["frame_acquisition_protocol"], 1);
                assert_eq!(attached["result"]["ready"], false);
                let take = request(&mut client, json!({"sequence":1,"method":"begin"}));
                assert_eq!(take["result"]["ready"] == true, begin_ok);
                if begin_ok {
                    assert_eq!(take["result"]["frame_acquisition_protocol"], 1);
                } else {
                    assert_eq!(take["error"]["code"], "authority_lost");
                }
            } else {
                assert_eq!(read(&mut client)["error"]["code"], "unavailable");
            }
            drop(client);
            assert!(thread.join().unwrap().is_ok());
            assert_eq!(
                worker_requests(&f, "native_begin").len(),
                usize::from(attach_ok)
            );
            assert!(worker_requests(&f, "native_event").is_empty());
        }
    }
    #[test]
    fn acquisition_real_unix_event_requires_exact_negotiated_receipt() {
        for (mode, receipt, accepted, malformed) in [
            (Some(json!(1)), Some(json!("a".repeat(32))), true, false),
            (Some(json!(1)), None, false, false),
            (None, None, true, false),
            (None, Some(json!("a".repeat(32))), false, false),
            (Some(json!(1)), Some(json!("A".repeat(32))), false, true),
            (Some(json!(1)), Some(Value::Null), false, true),
        ] {
            let echo = mode.clone().unwrap_or(Value::Null);
            let f = Fixture::with_acquisition(false, Value::Null, json!(1), echo);
            let (mut client, thread) = f.attach_mode(None, mode.clone());
            read(&mut client);
            let take = request(&mut client, json!({"sequence":1,"method":"begin"}));
            assert_eq!(take["result"]["ready"], true);
            let mut event = json!({"sequence":2,"method":"event","epoch":take["result"]["epoch"],
                "frame":{"display_instance":"d".repeat(32),"presentation_serial":1,"window_epoch":1,"invalidation_epoch":0,"source_generation":7,"source_sequence":u64::MAX,"width":1280,"height":800},
                "diagnostic_id":9007199254740991u64,"event":{"kind":"key","qcode":"ret","down":true}});
            if let Some(receipt) = &receipt {
                event["acquisition_receipt"] = receipt.clone();
            }
            write_json(client.get_mut(), &event).unwrap();
            if malformed {
                assert!(read_frame(&mut client).unwrap().is_none());
            } else {
                assert_eq!(read(&mut client)["result"]["dispatched"] == true, accepted);
            }
            drop(client);
            assert_eq!(thread.join().unwrap().is_ok(), !malformed);
            let begins = worker_requests(&f, "native_begin");
            assert_eq!(
                begins[0]["arguments"].get("frame_acquisition_protocol"),
                mode.as_ref()
            );
            let events = worker_requests(&f, "native_event");
            assert_eq!(events.len(), usize::from(accepted));
            if accepted {
                assert_eq!(
                    events[0]["arguments"].get("acquisition_receipt"),
                    receipt.as_ref()
                );
                assert_eq!(events[0]["arguments"]["frame"], event["frame"]);
                assert_eq!(
                    events[0]["arguments"]["diagnostic_id"],
                    event["diagnostic_id"]
                );
            }
        }
    }
    #[test]
    fn acquisition_malformed_attach_never_reaches_worker() {
        for mode in [
            Value::Null,
            json!(true),
            json!(1.0),
            json!(0),
            json!(2),
            json!("1"),
        ] {
            let f = Fixture::new();
            let (mut client, thread) = f.attach_mode(None, Some(mode));
            assert_eq!(read(&mut client)["error"]["code"], "unavailable");
            drop(client);
            assert!(thread.join().unwrap().is_ok());
            assert!(worker_requests(&f, "native_begin").is_empty());
        }
    }
    #[test]
    fn real_unix_event_diagnostic_id_is_exact_or_absent_and_malformed_never_dispatched() {
        for id in [
            None,
            Some(json!(1)),
            Some(json!(9_007_199_254_740_991u64)),
            Some(Value::Null),
            Some(json!(true)),
            Some(json!(1.0)),
            Some(json!(0)),
            Some(json!(-1)),
            Some(json!("1")),
            Some(json!(9_007_199_254_740_992u64)),
        ] {
            let f = Fixture::new();
            let (mut client, thread) = f.attach(None);
            read(&mut client);
            let take = request(&mut client, json!({"sequence":1,"method":"begin"}));
            let mut event = json!({"sequence":2,"method":"event","epoch":take["result"]["epoch"],
                "frame":{"display_instance":"d".repeat(32),"presentation_serial":1,"window_epoch":1,"invalidation_epoch":0,"source_generation":7,"source_sequence":42,"width":1280,"height":800},
                "event":{"kind":"key","qcode":"ret","down":true}});
            if let Some(value) = &id {
                event["diagnostic_id"] = value.clone();
            }
            let valid = native_input::diagnostic_id(id.as_ref()).is_ok();
            write_json(client.get_mut(), &event).unwrap();
            if valid {
                assert_eq!(read(&mut client)["result"]["dispatched"], true);
            } else {
                assert!(read_frame(&mut client).unwrap().is_none());
            }
            drop(client);
            assert_eq!(thread.join().unwrap().is_ok(), valid);
            let log = std::fs::read_to_string(f._dir.path().join("worker-requests.jsonl")).unwrap();
            let requests: Vec<Value> = log
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .filter(|r: &Value| r["method"] == "native_event")
                .collect();
            assert_eq!(requests.len(), usize::from(valid));
            if valid {
                assert_eq!(requests[0]["arguments"].get("diagnostic_id"), id.as_ref());
                assert_eq!(
                    requests[0]["arguments"]["frame"].as_object().unwrap().len(),
                    8
                );
            }
        }
    }
    #[test]
    fn worker_diagnostic_crosses_pipe_only_when_allowlisted_and_cleanup_stays_required() {
        let good = json!({"version":1,"stage":"display_validation_rejected","elapsed_ms":17});
        for diagnostic in [
            good.clone(),
            json!({"version":1,"stage":"SECRET","elapsed_ms":17}),
            json!({"version":1,"stage":"authority","elapsed_ms":17,"key":"SECRET"}),
            json!({"version":1,"stage":"authority","elapsed_ms":60001}),
        ] {
            let f = Fixture::with_diagnostic(false, diagnostic.clone());
            let (mut client, thread) = f.attach(None);
            read(&mut client);
            let take = request(&mut client, json!({"sequence":1,"method":"begin"}));
            let epoch = take["result"]["epoch"].as_u64().unwrap();
            let reply = request(
                &mut client,
                json!({"sequence":2,"method":"event","epoch":epoch,
                "frame":{"display_instance":"d".repeat(32),"presentation_serial":1,"window_epoch":1,"invalidation_epoch":1,"source_generation":1,"source_sequence":1,"width":1280,"height":800},
                "event":{"kind":"key","qcode":"slash","down":true}}),
            );
            assert_eq!(reply["error"]["code"], "stale_frame");
            assert_eq!(
                reply["error"]["diagnostic"],
                if diagnostic == good {
                    good.clone()
                } else {
                    Value::Null
                }
            );
            assert!(!reply.to_string().contains("SECRET"));
            // A rejected event releases what the window held.
            assert_eq!(worker_requests(&f, "native_end").len(), 1);
            drop(client);
            thread.join().unwrap().unwrap();
        }
    }
    #[test]
    fn focus_begin_suspend_and_eof_release() {
        let f = Fixture::new();
        let (mut client, thread) = f.attach(None);
        assert_eq!(read(&mut client)["result"]["ready"], false);
        let a = request(&mut client, json!({"sequence":1,"method":"begin"}));
        let epoch = a["result"]["epoch"].as_u64().unwrap();
        assert_eq!(a["result"]["ready"], true);
        let s = request(
            &mut client,
            json!({"sequence":2,"method":"suspend","epoch":epoch}),
        );
        assert_eq!(s["result"]["released"], true);
        assert_eq!(s["result"]["epoch"], epoch);
        assert_eq!(
            request(&mut client, json!({"sequence":3,"method":"heartbeat"}))["result"]["ready"],
            false
        );
        let second = request(&mut client, json!({"sequence":4,"method":"begin"}));
        assert_eq!(second["result"]["ready"], true);
        assert_eq!(second["result"]["epoch"], epoch + 1);
        assert_eq!(
            request(
                &mut client,
                json!({"sequence":5,"method":"heartbeat","epoch":epoch})
            )["error"]["code"],
            "authority_lost",
            "a heartbeat from the previous focus period is refused"
        );
        drop(client);
        thread.join().unwrap().unwrap();
        // Suspend and the refused heartbeat release; EOF finds nothing held.
        assert_eq!(worker_requests(&f, "native_end").len(), 2);
        assert_eq!(worker_requests(&f, "native_begin").len(), 2);
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
            request(&mut c, json!({"sequence":1,"method":"begin"}))["result"]["ready"],
            true
        );
        write_json(c.get_mut(), &json!({"sequence":1,"method":"heartbeat"})).unwrap();
        assert!(read_frame(&mut c).unwrap().is_none());
        assert!(t.join().unwrap().is_err());
        assert_eq!(worker_requests(&f, "native_end").len(), 1);
    }
    #[test]
    fn native_failed_suspend_never_reports_release() {
        let f = Fixture::with_release_failure(true);
        let (mut c, t) = f.attach(None);
        read(&mut c);
        let epoch = request(&mut c, json!({"sequence":1,"method":"begin"}))["result"]["epoch"]
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
    }
    #[test]
    fn native_idle_channel_closes_and_releases() {
        let f = Fixture::new();
        let (mut c, t) = f.attach(None);
        read(&mut c);
        assert_eq!(
            request(&mut c, json!({"sequence":1,"method":"begin"}))["result"]["ready"],
            true
        );
        assert!(read_frame(&mut c).unwrap().is_none());
        assert!(t.join().unwrap().is_err());
        assert_eq!(worker_requests(&f, "native_end").len(), 1);
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
                frame_acquisition_protocol: None,
                protocol_version: 1,
                id: "vm".into(),
                worker_instance: old_worker,
                display_instance: "d".repeat(32),
            },
            owner,
            epoch: 0,
            began: false,
            active: false,
        };
        let (mut c, t) = f.attach(None);
        assert_eq!(read(&mut c)["result"]["ready"], false);
        let epoch = request(&mut c, json!({"sequence":1,"method":"begin"}))["result"]["epoch"]
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
