//! Worker-side native input fence; the daemon cannot substitute a screenshot anchor.
use super::*;
use vm_control::native_input::{DisplayedFrame, Event, RejectionDiagnostic, RejectionStage};
#[derive(Default)]
pub(super) struct NativeInput {
    owner: Option<String>,
    epoch: u64,
    sequence: u64,
    display: String,
    acquisition: bool,
}
impl NativeInput {
    pub fn active(&self, input: &InputState) -> bool {
        self.owner.is_some() && input.active_epoch == Some(self.epoch)
    }
    pub fn handle(
        &mut self,
        method: &str,
        args: &Value,
        input: &mut InputState,
        console: &Path,
        qmp: &mut Qmp,
    ) -> Result<Value> {
        let started = Instant::now();
        let mut stage = RejectionStage::Admission;
        self.handle_staged(method, args, input, console, qmp, &mut stage)
            .map_err(|error| {
                let stage = error
                    .downcast_ref::<RejectionStage>()
                    .copied()
                    .unwrap_or(stage);
                error.context(RejectionDiagnostic {
                    version: 1,
                    stage,
                    elapsed_ms: started.elapsed().as_millis().min(60_000) as u64,
                })
            })
    }
    fn handle_staged(
        &mut self,
        method: &str,
        args: &Value,
        input: &mut InputState,
        console: &Path,
        qmp: &mut Qmp,
        stage: &mut RejectionStage,
    ) -> Result<Value> {
        let owner = args["owner"]
            .as_str()
            .filter(|s| vm_control::native_input::hex(s, 64))
            .context("invalid native owner")?;
        let epoch = args["epoch"]
            .as_u64()
            .filter(|n| *n > 0)
            .context("invalid native epoch")?;
        let sequence = args["sequence"]
            .as_u64()
            .filter(|n| *n > 0)
            .context("invalid native sequence")?;
        if epoch <= input.revoked_through {
            bail!("native epoch revoked")
        }
        if method == "native_begin" {
            if input.operation.is_some() {
                bail!("input cleanup required")
            }
            let display = args["display_instance"]
                .as_str()
                .filter(|s| vm_control::native_input::hex(s, 32))
                .context("invalid display instance")?;
            let acquisition = match args.get("frame_acquisition_protocol") {
                None => false,
                Some(value) if value.as_u64() == Some(1) => true,
                _ => bail!("unsupported native frame acquisition protocol"),
            };
            *stage = RejectionStage::BeginDisplay;
            let status = console_window(console, "window_status")?;
            if status["display_instance"] != display {
                bail!("native display instance changed")
            }
            if acquisition && status["native_frame_acquisition_protocol"].as_u64() != Some(1) {
                bail!("native frame acquisition unsupported by helper")
            }
            self.acquisition = acquisition;
            self.owner = Some(owner.into());
            self.epoch = epoch;
            self.sequence = sequence;
            self.display = display.into();
            input.operation =
                Some(vm_control::bridge::State::new()?.worker_instance[..32].to_owned());
            input.active_epoch = Some(epoch);
            input.until = Some(Instant::now() + Duration::from_secs(2));
            let mut reply = json!({"ready":true,"epoch":epoch});
            if acquisition {
                reply["frame_acquisition_protocol"] = json!(1);
            }
            return Ok(reply);
        }
        *stage = RejectionStage::Authority;
        if self.owner.as_deref() != Some(owner)
            || self.epoch != epoch
            || sequence <= self.sequence
            || input.active_epoch != Some(epoch)
            || input.until.is_none_or(|until| Instant::now() >= until)
        {
            bail!("native authority lost")
        }
        self.sequence = sequence;
        if method == "native_heartbeat" {
            input.until = Some(Instant::now() + Duration::from_secs(2));
            return Ok(json!({"ready":true,"epoch":epoch}));
        }
        if method != "native_event" {
            bail!("unsupported native method")
        }
        *stage = RejectionStage::FrameDecode;
        let frame: DisplayedFrame = serde_json::from_value(args["frame"].clone())?;
        frame.validate()?;
        *stage = RejectionStage::FrameInstance;
        if frame.display_instance != self.display {
            bail!("native displayed frame belongs to another instance")
        }
        *stage = RejectionStage::EventValidation;
        let diagnostic_id = vm_control::native_input::diagnostic_id(args.get("diagnostic_id"))?;
        let event: Event = serde_json::from_value(args["event"].clone())?;
        let events = event.qmp(&frame)?;
        *stage = RejectionStage::DisplayValidationIo;
        let receipt = args.get("acquisition_receipt");
        match (self.acquisition, receipt) {
            (true, Some(value))
                if value
                    .as_str()
                    .is_some_and(|s| vm_control::native_input::hex(s, 32)) => {}
            (false, None) => {}
            _ => bail!("native acquisition receipt mode mismatch"),
        }
        let mut request =
            json!({"request_id":"native-frame","method":"validate_displayed_frame","frame":frame});
        if let Some(id) = diagnostic_id {
            request["diagnostic_id"] = json!(id);
        }
        if let Some(receipt) = receipt {
            request["acquisition_receipt"] = receipt.clone();
            request["epoch"] = json!(epoch);
            request["sequence"] = json!(sequence);
        }
        // Before connect/write; starting at response receipt would renew eligibility.
        let validation_started = Instant::now();
        let authority_deadline = input.until.context("native authority deadline missing")?;
        let validation = if self.acquisition {
            deadline_io::console(
                console,
                &request,
                "native-frame",
                authority_deadline.min(validation_started + Duration::from_millis(200)),
            )?
        } else {
            let mut socket = UnixStream::connect(console)?;
            socket.set_write_timeout(Some(Duration::from_millis(500)))?;
            serde_json::to_writer(&mut socket, &request)?;
            socket.write_all(b"\n")?;
            read_console_reply(
                &mut socket,
                "native-frame",
                Instant::now() + Duration::from_millis(750),
            )?
        };
        *stage = RejectionStage::DisplayValidationRejected;
        if validation["valid"] != true
            || !frame.matches_record(&validation["frame"])
            || !validation["current"].is_boolean()
        {
            bail!("native displayed frame is stale")
        }
        let delivery_deadline = if self.acquisition {
            let proof: AcquisitionValidation =
                serde_json::from_value(validation["acquisition"].clone())?;
            Some(proof.deadline(
                epoch,
                sequence,
                validation_started,
                authority_deadline,
                Instant::now(),
            )?)
        } else {
            if validation.get("acquisition").is_some() {
                bail!("unexpected acquisition proof in legacy mode")
            }
            None
        };
        *stage = RejectionStage::SourceStatusIo;
        let current = if let Some(deadline) = delivery_deadline {
            deadline_io::console(
                console,
                &json!({"request_id":"frame-input","method":"frame_status"}),
                "frame-input",
                deadline,
            )?
        } else {
            console_frame_status(console)?
        };
        *stage = RejectionStage::SourceChanged;
        if current["capture_connected"] != true
            || current["frame_available"] != true
            || current["source_generation"] != frame.source_generation
            || current["width"] != frame.width
            || current["height"] != frame.height
        {
            bail!("native source changed")
        }
        // Validation RPC and QMP are separate processes: no atomic display/input claim.
        *stage = RejectionStage::ValidationExpired;
        if input.until.is_none_or(|until| Instant::now() >= until)
            || delivery_deadline.is_some_and(|until| Instant::now() >= until)
        {
            bail!("native input expired while validating frame")
        }
        *stage = RejectionStage::QmpDispatch;
        input.send_events_before(&events, qmp, delivery_deadline)?;
        // A complete timely record can receive a late ACK. It remains framed,
        // but receipt mode reports expiry and lets normal revocation release
        // any conservatively held keys; this is not a successful late event.
        *stage = RejectionStage::ValidationExpired;
        if delivery_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            bail!("native acquisition expired before QMP acknowledgement")
        }
        input.until = Some(Instant::now() + Duration::from_secs(2));
        Ok(json!({"ready":true,"epoch":epoch,"dispatched":true,"guest_delivery_confirmed":false}))
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AcquisitionValidation {
    version: u64,
    epoch: u64,
    sequence: u64,
    remaining_us: u64,
}
impl AcquisitionValidation {
    fn deadline(
        &self,
        epoch: u64,
        sequence: u64,
        started: Instant,
        authority: Instant,
        now: Instant,
    ) -> Result<Instant> {
        if self.version != 1
            || self.epoch != epoch
            || self.sequence != sequence
            || !(1..=200_000).contains(&self.remaining_us)
        {
            bail!("invalid native acquisition proof")
        }
        let deadline = (started + Duration::from_micros(self.remaining_us)).min(authority);
        if now >= deadline {
            bail!("native acquisition proof expired during validation")
        }
        Ok(deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_diagnostic_id_never_reaches_console_or_qmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (stream, mut remote) = UnixStream::pair().unwrap();
        remote.set_nonblocking(true).unwrap();
        let mut qmp = Qmp {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
        };
        let mut input = InputState {
            active_epoch: Some(3),
            until: Some(Instant::now() + Duration::from_secs(2)),
            ..Default::default()
        };
        let mut native = NativeInput {
            owner: Some("a".repeat(64)),
            epoch: 3,
            sequence: 0,
            display: "d".repeat(32),
            acquisition: false,
        };
        for (index, invalid) in [
            Value::Null,
            json!(true),
            json!(1.0),
            json!(-1),
            json!(0),
            json!(9_007_199_254_740_992u64),
            json!("1"),
            json!({}),
        ]
        .into_iter()
        .enumerate()
        {
            let args = json!({"owner":"a".repeat(64),"epoch":3,"sequence":index+1,"diagnostic_id":invalid,
                "frame":{"display_instance":"d".repeat(32),"presentation_serial":1,"window_epoch":1,"invalidation_epoch":0,"source_generation":7,"source_sequence":42,"width":1280,"height":800},
                "event":{"kind":"key","qcode":"ret","down":true}});
            let error = native
                .handle("native_event", &args, &mut input, &path, &mut qmp)
                .unwrap_err();
            assert_eq!(
                error.downcast_ref::<RejectionDiagnostic>().unwrap().stage,
                RejectionStage::EventValidation
            );
            assert_eq!(qmp.next, 1);
            assert!(input.held.is_empty());
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
            assert_eq!(
                remote.read(&mut [0; 1]).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }
    #[test]
    fn real_unix_display_proof_and_qmp_epoch_release() {
        for diagnostic_id in [None, Some(1), Some(9_007_199_254_740_991u64)] {
            replay_display_proof(diagnostic_id);
        }
    }
    fn replay_display_proof(diagnostic_id: Option<u64>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = stop.clone();
        let broker = thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                let (mut s, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                s.set_nonblocking(false).unwrap();
                s.set_nonblocking(false).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                let r: Value = serde_json::from_slice(
                    &read_frame(&mut BufReader::new(s.try_clone().unwrap()))
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                let value = match r["method"].as_str().unwrap() {
                    "window_status" => {
                        json!({"native_window":true,"native_window_protocol":2,"native_input_protocol":1,"native_input_supported":true,"native_input_available":true,"display_only":true,"input_enabled":false,"window_open":true,"display_instance":"d".repeat(32)})
                    }
                    "frame_status" => {
                        json!({"capture_connected":true,"frame_available":true,"source_generation":7,"width":1280,"height":800})
                    }
                    "validate_displayed_frame" => {
                        assert_eq!(
                            r.get("diagnostic_id"),
                            diagnostic_id.map(|id| json!(id)).as_ref()
                        );
                        assert_eq!(r["frame"].as_object().unwrap().len(), 8);
                        let mut f = r["frame"].clone();
                        f["source_width"] = f["width"].take();
                        f["source_height"] = f["height"].take();
                        json!({"valid":r["frame"]["presentation_serial"]!=2,"frame":f,"current":if r["frame"]["presentation_serial"]==3 {json!(1)}else{json!(true)}})
                    }
                    _ => panic!(),
                };
                let reply = match r["frame"]["presentation_serial"].as_u64() {
                    Some(4) => {
                        json!({"request_id":r["request_id"],"error":"not forwarded", "display_rejection":"display_history_miss"})
                    }
                    Some(5) => {
                        json!({"request_id":r["request_id"],"error":"SECRET unexpected helper text", "display_rejection":"SECRET"})
                    }
                    _ => json!({"request_id":r["request_id"],"result":value}),
                };
                let body = serde_json::to_vec(&reply).unwrap();
                s.write_all(&(body.len() as u32).to_be_bytes()).unwrap();
                s.write_all(&[1]).unwrap();
                s.write_all(&[0; 8]).unwrap();
                s.write_all(&body).unwrap();
            }
        });
        let (stream, mut qmp_server) = UnixStream::pair().unwrap();
        qmp_server
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let qmp_thread = thread::spawn(move || {
            let mut reader = BufReader::new(qmp_server.try_clone().unwrap());
            for down in [true, false] {
                let r: Value =
                    serde_json::from_slice(&read_frame(&mut reader).unwrap().unwrap()).unwrap();
                assert_eq!(r["execute"], "input-send-event");
                assert_eq!(
                    r["arguments"]["events"].as_array().unwrap().last().unwrap()["data"]["down"],
                    down
                );
                serde_json::to_writer(&mut qmp_server, &json!({"id":r["id"],"return":{}})).unwrap();
                qmp_server.write_all(b"\n").unwrap();
            }
        });
        let mut qmp = Qmp {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
        };
        let mut input = InputState::default();
        let mut native = NativeInput::default();
        let mut args = json!({"owner":"a".repeat(64),"epoch":3,"sequence":1,"display_instance":"d".repeat(32)});
        assert_eq!(
            native
                .handle("native_begin", &args, &mut input, &path, &mut qmp)
                .unwrap()["ready"],
            true
        );
        args["sequence"] = json!(2);
        args["frame"] = json!({"display_instance":"d".repeat(32),"presentation_serial":2,"window_epoch":1,"invalidation_epoch":0,"source_generation":7,"source_sequence":42,"width":1280,"height":800});
        args["event"] = json!({"kind":"button","button":"left","down":true,"x":1279,"y":799});
        if let Some(id) = diagnostic_id {
            args["diagnostic_id"] = json!(id);
        }
        let error = native
            .handle("native_event", &args, &mut input, &path, &mut qmp)
            .unwrap_err();
        let diagnostic = error.downcast_ref::<RejectionDiagnostic>().unwrap();
        assert_eq!(diagnostic.stage, RejectionStage::DisplayValidationRejected);
        assert!(
            RejectionDiagnostic::from_wire(&serde_json::to_value(diagnostic).unwrap()).is_some()
        );
        assert!(input.held.is_empty());
        args["sequence"] = json!(3);
        args["frame"]["presentation_serial"] = json!(1);
        args["frame"]["display_instance"] = json!("e".repeat(32));
        let error = native
            .handle("native_event", &args, &mut input, &path, &mut qmp)
            .unwrap_err();
        let diagnostic = error.downcast_ref::<RejectionDiagnostic>().unwrap();
        assert_eq!(diagnostic.stage, RejectionStage::FrameInstance);
        assert!(
            RejectionDiagnostic::from_wire(&serde_json::to_value(diagnostic).unwrap()).is_some()
        );
        args["sequence"] = json!(4);
        args["frame"]["display_instance"] = json!("d".repeat(32));
        args["frame"]["presentation_serial"] = json!(3);
        assert!(
            native
                .handle("native_event", &args, &mut input, &path, &mut qmp)
                .is_err(),
            "numeric current flag is not a validated frame proof"
        );
        assert!(input.held.is_empty());
        for (sequence, serial, expected) in [
            (5, 4, RejectionStage::DisplayHistoryMiss),
            (6, 5, RejectionStage::DisplayValidationIo),
        ] {
            args["sequence"] = json!(sequence);
            args["frame"]["presentation_serial"] = json!(serial);
            let error = native
                .handle("native_event", &args, &mut input, &path, &mut qmp)
                .unwrap_err();
            let diagnostic = error.downcast_ref::<RejectionDiagnostic>().unwrap();
            assert_eq!(diagnostic.stage, expected);
            assert!(!serde_json::to_string(diagnostic)
                .unwrap()
                .contains("SECRET"));
            assert!(input.held.is_empty());
        }
        args["sequence"] = json!(7);
        args["frame"]["presentation_serial"] = json!(1);
        assert_eq!(
            native
                .handle("native_event", &args, &mut input, &path, &mut qmp)
                .unwrap()["dispatched"],
            true
        );
        assert_eq!(input.held.len(), 1);
        assert_eq!(
            input.revoke(&json!({"through_epoch":3}), &mut qmp).unwrap()["released"],
            true
        );
        args["sequence"] = json!(8);
        let error = native
            .handle("native_event", &args, &mut input, &path, &mut qmp)
            .unwrap_err();
        let diagnostic = error.downcast_ref::<RejectionDiagnostic>().unwrap();
        assert_eq!(diagnostic.stage, RejectionStage::Admission);
        assert!(
            RejectionDiagnostic::from_wire(&serde_json::to_value(diagnostic).unwrap()).is_some()
        );
        assert!(input.held.is_empty());
        qmp_thread.join().unwrap();
        stop.store(true, Ordering::Release);
        broker.join().unwrap();
    }
}

#[cfg(test)]
mod acquisition_tests {
    use super::*;
    #[test]
    fn strict_budget_full_width_identity_and_original_start() {
        let started = Instant::now();
        let good = json!({"version":1,"epoch":u64::MAX,"sequence":u64::MAX,"remaining_us":100000});
        let proof: AcquisitionValidation = serde_json::from_value(good.clone()).unwrap();
        let authority = started + Duration::from_secs(2);
        assert_eq!(
            proof
                .deadline(u64::MAX, u64::MAX, started, authority, started)
                .unwrap(),
            started + Duration::from_millis(100)
        );
        assert!(proof
            .deadline(
                u64::MAX,
                u64::MAX,
                started,
                authority,
                started + Duration::from_millis(100)
            )
            .is_err());
        assert!(proof
            .deadline(1, u64::MAX, started, authority, started)
            .is_err());
        assert!(proof
            .deadline(u64::MAX, 1, started, authority, started)
            .is_err());
        assert_eq!(
            proof
                .deadline(
                    u64::MAX,
                    u64::MAX,
                    started,
                    started + Duration::from_millis(1),
                    started
                )
                .unwrap(),
            started + Duration::from_millis(1)
        );
        for value in [
            json!(0),
            json!(200001),
            json!(true),
            json!(1.0),
            Value::Null,
            json!("100"),
        ] {
            let mut bad = good.clone();
            bad["remaining_us"] = value;
            assert!(serde_json::from_value::<AcquisitionValidation>(bad)
                .and_then(|p| p
                    .deadline(u64::MAX, u64::MAX, started, authority, started)
                    .map_err(serde::de::Error::custom))
                .is_err());
        }
        for key in ["version", "epoch", "sequence"] {
            let mut bad = good.clone();
            bad[key] = json!(true);
            assert!(serde_json::from_value::<AcquisitionValidation>(bad).is_err());
        }
        let mut extra = good;
        extra["receipt"] = json!("a".repeat(32));
        assert!(serde_json::from_value::<AcquisitionValidation>(extra).is_err());
    }
    #[test]
    fn actual_begin_negotiates_helper_capability_and_echoes_mode_only_when_requested() {
        for (requested, supported, valid) in [
            (false, false, true),
            (true, false, false),
            (true, true, true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("console");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let server = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                s.set_nonblocking(false).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                let r: Value = serde_json::from_slice(
                    &read_frame(&mut BufReader::new(s.try_clone().unwrap()))
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(r["method"], "window_status");
                let mut result = json!({"native_window":true,"native_window_protocol":2,"native_input_protocol":1,"native_input_supported":true,"native_input_available":true,"display_only":true,"input_enabled":false,"window_open":true,"display_instance":"d".repeat(32)});
                if supported {
                    result["native_frame_acquisition_protocol"] = json!(1);
                }
                let b = serde_json::to_vec(&json!({"request_id":r["request_id"],"result":result}))
                    .unwrap();
                s.write_all(&(b.len() as u32).to_be_bytes()).unwrap();
                s.write_all(&[1, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
                s.write_all(&b).unwrap();
            });
            let (stream, mut peer) = UnixStream::pair().unwrap();
            peer.set_nonblocking(true).unwrap();
            let mut qmp = Qmp {
                reader: BufReader::new(stream.try_clone().unwrap()),
                stream,
                next: 1,
                poisoned: false,
                reply_prefix: Vec::new(),
            };
            let mut input = InputState::default();
            let mut native = NativeInput::default();
            let mut args = json!({"owner":"a".repeat(64),"epoch":3,"sequence":1,"display_instance":"d".repeat(32)});
            if requested {
                args["frame_acquisition_protocol"] = json!(1);
            }
            let reply = native.handle("native_begin", &args, &mut input, &path, &mut qmp);
            if valid {
                let reply = reply.unwrap();
                assert_eq!(
                    reply.get("frame_acquisition_protocol"),
                    requested.then_some(&json!(1))
                );
                assert_eq!(native.acquisition, requested);
            } else {
                assert!(reply.is_err());
                assert!(input.active_epoch.is_none());
                assert!(native.owner.is_none());
            }
            assert_eq!(
                peer.read(&mut [0; 1]).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
            server.join().unwrap();
        }
    }
    fn run(
        mode: bool,
        receipt: Option<Value>,
        remaining: u64,
        validation_delay: u64,
        source_delay: u64,
    ) -> (Result<Value>, InputState, bool, usize) {
        run_ack(
            mode,
            receipt,
            remaining,
            validation_delay,
            source_delay,
            0,
            false,
        )
    }
    fn run_ack(
        mode: bool,
        receipt: Option<Value>,
        remaining: u64,
        validation_delay: u64,
        source_delay: u64,
        ack_delay: u64,
        cleanup: bool,
    ) -> (Result<Value>, InputState, bool, usize) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let server = thread::spawn(move || {
            let mut count = 0;
            while !done.load(Ordering::Acquire) {
                let (mut s, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                s.set_nonblocking(false).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                let r: Value = serde_json::from_slice(
                    &read_frame(&mut BufReader::new(s.try_clone().unwrap()))
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                count += 1;
                let value = if r["method"] == "validate_displayed_frame" {
                    assert_eq!(r["frame"].as_object().unwrap().len(), 8);
                    if mode {
                        assert_eq!(r["acquisition_receipt"], "a".repeat(32));
                        assert_eq!(r["epoch"], 3);
                        assert_eq!(r["sequence"], 1);
                    }
                    thread::sleep(Duration::from_millis(validation_delay));
                    let mut frame = r["frame"].clone();
                    frame["source_width"] = frame["width"].take();
                    frame["source_height"] = frame["height"].take();
                    json!({"valid":true,"current":false,"frame":frame,"acquisition":{"version":1,"epoch":3,"sequence":1,"remaining_us":remaining}})
                } else {
                    assert_eq!(r["method"], "frame_status");
                    thread::sleep(Duration::from_millis(source_delay));
                    json!({"capture_connected":true,"frame_available":true,"source_generation":7,"width":1280,"height":800})
                };
                let b = serde_json::to_vec(&json!({"request_id":r["request_id"],"result":value}))
                    .unwrap();
                let mut wire = Vec::new();
                wire.extend_from_slice(&(b.len() as u32).to_be_bytes());
                wire.push(1);
                wire.extend_from_slice(&[0; 8]);
                wire.extend(b);
                let _ = s.write_all(&wire); // Consumer correctly may have expired/closed.
            }
            count
        });
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        let qmp_peer = thread::spawn(move || {
            let mut reader = BufReader::new(peer.try_clone().unwrap());
            for down in if cleanup {
                vec![true, false]
            } else {
                vec![true]
            } {
                let r = read_frame(&mut reader);
                let Ok(Some(bytes)) = r else {
                    assert!(!cleanup);
                    return false;
                };
                let v: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(v["arguments"]["events"][0]["data"]["down"], down);
                if down {
                    thread::sleep(Duration::from_millis(ack_delay));
                }
                serde_json::to_writer(&mut peer, &json!({"id":v["id"],"return":{}})).unwrap();
                peer.write_all(b"\n").unwrap();
            }
            true
        });
        let mut qmp = Qmp {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
            next: 1,
            poisoned: false,
            reply_prefix: Vec::new(),
        };
        let mut input = InputState {
            active_epoch: Some(3),
            until: Some(Instant::now() + Duration::from_secs(2)),
            ..Default::default()
        };
        let mut native = NativeInput {
            owner: Some("b".repeat(64)),
            epoch: 3,
            sequence: 0,
            display: "d".repeat(32),
            acquisition: mode,
        };
        let mut args = json!({"owner":"b".repeat(64),"epoch":3,"sequence":1,"frame":{"display_instance":"d".repeat(32),"presentation_serial":1,"window_epoch":1,"invalidation_epoch":0,"source_generation":7,"source_sequence":42,"width":1280,"height":800},"event":{"kind":"key","qcode":"a","down":true}});
        if let Some(receipt) = receipt {
            args["acquisition_receipt"] = receipt;
        }
        let result = native.handle("native_event", &args, &mut input, &path, &mut qmp);
        if cleanup {
            assert!(result.is_err());
            assert_eq!(input.held.len(), 1);
            input.cleanup(&mut qmp).unwrap();
            assert!(input.held.is_empty());
        }
        stop.store(true, Ordering::Release);
        let count = server.join().unwrap();
        drop(qmp);
        let written = qmp_peer.join().unwrap();
        (result, input, written, count)
    }
    #[test]
    fn real_unix_receipt_source_and_qmp_no_budget_restart() {
        let (r, input, written, count) = run(true, Some(json!("a".repeat(32))), 200000, 0, 0);
        assert_eq!(r.unwrap()["guest_delivery_confirmed"], false);
        assert_eq!(input.held.len(), 1);
        assert!(written);
        assert_eq!(count, 2);
        // Validation takes longer than the helper remaining amount; start, not reply, anchors it.
        let (r, input, written, count) = run(true, Some(json!("a".repeat(32))), 1000, 20, 0);
        assert!(r.is_err());
        assert!(input.held.is_empty());
        assert!(!written);
        assert_eq!(count, 1);
        let (r, input, written, count) = run(true, Some(json!("a".repeat(32))), 20000, 0, 40);
        assert!(r.is_err());
        assert!(input.held.is_empty());
        assert!(!written);
        assert_eq!(count, 2);
    }
    #[test]
    fn actual_worker_late_ack_rejects_without_renewal_and_releases_on_framed_qmp() {
        let (r, input, written, count) =
            run_ack(true, Some(json!("a".repeat(32))), 100_000, 0, 0, 120, true);
        assert_eq!(
            r.unwrap_err()
                .downcast_ref::<RejectionDiagnostic>()
                .unwrap()
                .stage,
            RejectionStage::ValidationExpired
        );
        assert!(input.held.is_empty());
        assert!(input.until.is_none());
        assert!(written);
        assert_eq!(count, 2);
    }
    #[test]
    fn receipt_mode_cannot_fallback_or_upgrade_legacy() {
        for (mode, receipt) in [
            (true, None),
            (true, Some(Value::Null)),
            (true, Some(json!("A".repeat(32)))),
            (false, Some(json!("a".repeat(32)))),
        ] {
            let (r, input, written, count) = run(mode, receipt, 200000, 0, 0);
            assert!(r.is_err());
            assert!(input.held.is_empty());
            assert!(!written);
            assert_eq!(count, 0);
        }
        let (r, _, written, count) = run(false, None, 200000, 0, 0);
        assert!(r.is_err(), "legacy rejects unexpected acquisition reply");
        assert!(!written);
        assert_eq!(count, 1);
    }
}
