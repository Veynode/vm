//! Worker-side native input fence; the daemon cannot substitute a screenshot anchor.
//! `epoch` here is the native session's activation number: it binds frame
//! acquisition receipts to one focus period of the window, nothing more.
use super::*;
use vm_control::native_input::{DisplayedFrame, Event, RejectionDiagnostic, RejectionStage};
#[derive(Default)]
pub(super) struct NativeInput {
    owner: Option<String>,
    operation: Option<String>,
    epoch: u64,
    sequence: u64,
    display: String,
    acquisition: bool,
}
impl NativeInput {
    pub fn active(&self, input: &InputState) -> bool {
        self.owner.is_some() && self.operation.is_some() && input.operation == self.operation
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
        if method == "native_end" {
            // Releases what this window session still holds; it may begin again.
            if self.owner.as_deref() == Some(owner) {
                if self.active(input) {
                    input.cleanup(qmp)?;
                }
                self.owner = None;
                self.operation = None;
            }
            return Ok(json!({"ready":false,"released":true}));
        }
        let epoch = args["epoch"]
            .as_u64()
            .filter(|n| *n > 0)
            .context("invalid native epoch")?;
        let sequence = args["sequence"]
            .as_u64()
            .filter(|n| *n > 0)
            .context("invalid native sequence")?;
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
            let operation = vm_control::bridge::State::new()?.worker_instance[..32].to_owned();
            self.operation = Some(operation.clone());
            input.operation = Some(operation);
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
            || !self.active(input)
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

/// Guest reply budget for one text chunk. The Guest bounds its own work to 3 s:
/// it may have to wait out the 1 s quiet period of a temporary keycode before
/// rebinding it (see guest/veynode-agent/text_input.go).
pub(super) const TEXT_CHUNK_DEADLINE: Duration = Duration::from_secs(4);

/// Worker-side fence for one chunk of Unicode text (`input_text_step`).
///
/// Text does not travel over QMP: the chunk becomes a `text_input` request to the Guest
/// service, handed to `submit` (the serial execution transport). The fences are those of
/// `InputState::step`: one operation at a time, a recent matching frame and a live operation
/// deadline. Without a transport the answer is an explicit `unsupported`; there
/// is no fallback to physical key codes. Nothing is submitted unless every check passes.
pub(super) fn text_step<P>(
    args: &Value,
    input: &mut InputState,
    frames: &vm_control::input::Frames,
    console: &Path,
    submit: Option<&dyn Fn(Value, Instant) -> Result<P>>,
) -> Result<P> {
    let submit = submit.context(
        "unsupported: Unicode text input requires the Guest execution transport (qemu_guest_exec) and a Guest service started with --allow-text-input",
    )?;
    let operation = args["operation_id"]
        .as_str()
        .context("missing input operation")?;
    if operation.len() != 32 || !operation.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("invalid input operation")
    }
    let begin = args["begin"] == true;
    if begin {
        if input.operation.is_some() {
            bail!("input busy: another operation is active")
        }
        if input
            .completed_operations
            .iter()
            .any(|known| known == operation)
        {
            bail!("input operation was already completed")
        }
    } else if input.operation.as_deref() != Some(operation) {
        bail!("input operation expired or replaced")
    }
    let index = args["index"]
        .as_u64()
        .filter(|index| {
            *index < vm_control::input::TEXT_MAX_SCALARS as u64 && (*index == 0) == begin
        })
        .context("invalid text chunk index")?;
    let request = vm_control::input::guest_text_request(
        &format!("text-{operation}-{index}"),
        args["text"].as_str().context("text chunk required")?,
    )?;
    let current = console_frame_status(console)?;
    frames.check(args, &current, Instant::now())?;
    if input.until.is_some_and(|until| Instant::now() >= until) {
        bail!("input operation deadline expired")
    }
    // A chunk the transport refuses leaves no operation behind: the caller is told "not sent".
    let pending = submit(request, Instant::now() + TEXT_CHUNK_DEADLINE)?;
    if begin {
        input.operation = Some(operation.into());
    }
    // Each admitted chunk renews the idle deadline; the daemon bounds the number of chunks.
    input.until = Some(Instant::now() + Duration::from_secs(5));
    Ok(pending)
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
            guest_reset: false,
            run_state_event: false,
            shutdown_event: false,
        };
        let mut input = InputState {
            operation: Some("o".repeat(32)),
            until: Some(Instant::now() + Duration::from_secs(2)),
            ..Default::default()
        };
        let mut native = NativeInput {
            owner: Some("a".repeat(64)),
            operation: Some("o".repeat(32)),
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
    fn real_unix_display_proof_and_qmp_release_on_end() {
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
            guest_reset: false,
            run_state_event: false,
            shutdown_event: false,
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
            native
                .handle("native_end", &args, &mut input, &path, &mut qmp)
                .unwrap()["released"],
            true
        );
        args["sequence"] = json!(8);
        let error = native
            .handle("native_event", &args, &mut input, &path, &mut qmp)
            .unwrap_err();
        let diagnostic = error.downcast_ref::<RejectionDiagnostic>().unwrap();
        assert_eq!(diagnostic.stage, RejectionStage::Authority);
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
                guest_reset: false,
                run_state_event: false,
                shutdown_event: false,
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
                assert!(input.operation.is_none());
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
            guest_reset: false,
            run_state_event: false,
            shutdown_event: false,
        };
        let mut input = InputState {
            operation: Some("o".repeat(32)),
            until: Some(Instant::now() + Duration::from_secs(2)),
            ..Default::default()
        };
        let mut native = NativeInput {
            owner: Some("b".repeat(64)),
            operation: Some("o".repeat(32)),
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

#[cfg(test)]
mod text_tests {
    use super::*;
    use std::cell::RefCell;
    const FRAME: &str = "00000000-0000-0000-0000-000000000000";
    const MIXED: &str = "你好e\u{301}😀";
    /// A console broker that answers `frame_status` and counts the requests it saw.
    fn console(
        generation: u64,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        std::sync::Arc<AtomicBool>,
        thread::JoinHandle<usize>,
    ) {
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
                assert_eq!(r["method"], "frame_status");
                count += 1;
                let b = serde_json::to_vec(&json!({"request_id":r["request_id"],"result":{"capture_connected":true,"frame_available":true,"source_generation":generation,"width":1280,"height":800}})).unwrap();
                let mut wire = Vec::new();
                wire.extend_from_slice(&(b.len() as u32).to_be_bytes());
                wire.push(1);
                wire.extend_from_slice(&[0; 8]);
                wire.extend(b);
                s.write_all(&wire).unwrap();
            }
            count
        });
        (dir, path, stop, server)
    }
    fn args(begin: bool, index: u64, text: &str) -> Value {
        json!({"operation_id":"0123456789abcdef0123456789abcdef",
            "begin":begin,"index":index,"text":text,
            "frame_id":FRAME,"width":1280,"height":800,"source_generation":7})
    }
    fn observed() -> vm_control::input::Frames {
        let mut frames = vm_control::input::Frames::default();
        frames.record(
            &json!({"frame_id":FRAME,"width":1280,"height":800,"source_generation":7}),
            Instant::now(),
        );
        frames
    }
    #[test]
    fn chunk_becomes_one_guest_request_under_the_input_fences() {
        let (_dir, path, stop, server) = console(7);
        let frames = observed();
        let mut input = InputState::default();
        let sent = RefCell::new(Vec::<(Value, Instant)>::new());
        let submit = |request: Value, deadline: Instant| -> Result<usize> {
            sent.borrow_mut().push((request, deadline));
            Ok(sent.borrow().len())
        };
        let before = Instant::now();
        assert_eq!(
            text_step(
                &args(true, 0, MIXED),
                &mut input,
                &frames,
                &path,
                Some(&submit)
            )
            .unwrap(),
            1
        );
        {
            let sent = sent.borrow();
            assert_eq!(
                sent[0].0,
                json!({"id":"text-0123456789abcdef0123456789abcdef-0","method":"text_input",
                    "text_hex":"e4bda0e5a5bd65cc81f09f9880"})
            );
            let budget = sent[0].1.duration_since(before);
            assert!(
                budget >= TEXT_CHUNK_DEADLINE
                    && budget < TEXT_CHUNK_DEADLINE + Duration::from_secs(1)
            );
        }
        assert_eq!(
            input.operation.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert!(input.held.is_empty(), "text holds no physical key");
        // The same operation continues; its idle deadline is renewed, never shortened.
        let first_deadline = input.until.unwrap();
        thread::sleep(Duration::from_millis(5));
        assert_eq!(
            text_step(
                &args(false, 1, "q"),
                &mut input,
                &frames,
                &path,
                Some(&submit)
            )
            .unwrap(),
            2
        );
        assert!(input.until.unwrap() > first_deadline);
        assert_eq!(sent.borrow()[1].0["text_hex"], "71");
        assert_eq!(
            sent.borrow()[1].0["id"],
            "text-0123456789abcdef0123456789abcdef-1"
        );

        // Every rejection below must leave the Guest untouched.
        let mut other = args(true, 0, "a");
        other["operation_id"] = json!("f".repeat(32));
        let mut replaced = args(false, 2, "a");
        replaced["operation_id"] = json!("f".repeat(32));
        let mut stale_frame = args(false, 2, "a");
        stale_frame["frame_id"] = json!("11111111-1111-1111-1111-111111111111");
        let mut resized = args(false, 2, "a");
        resized["width"] = json!(1920);
        let mut no_text = args(false, 2, "a");
        no_text["text"] = json!(["a"]);
        for (case, rejected) in [
            ("second operation while one is active", other),
            ("replaced operation", replaced),
            ("unknown frame", stale_frame),
            ("geometry change", resized),
            ("text type", no_text),
            ("control character", args(false, 2, "\u{1b}")),
            ("carriage return", args(false, 2, "a\r")),
            ("oversized chunk", args(false, 2, &"a".repeat(17))),
            ("begin with nonzero index", args(true, 1, "a")),
            ("continuation with zero index", args(false, 0, "a")),
            ("index out of range", args(false, 256, "a")),
        ] {
            assert!(
                text_step(&rejected, &mut input, &frames, &path, Some(&submit)).is_err(),
                "{case}"
            );
            assert_eq!(sent.borrow().len(), 2, "{case} reached the Guest");
        }
        // An expired operation deadline is not renewed by a late chunk.
        input.until = Some(Instant::now() - Duration::from_millis(1));
        assert!(text_step(
            &args(false, 2, "a"),
            &mut input,
            &frames,
            &path,
            Some(&submit)
        )
        .is_err());
        assert_eq!(sent.borrow().len(), 2);
        // A completed operation cannot be replayed.
        input.operation = None;
        input.until = None;
        input
            .completed_operations
            .push_back("0123456789abcdef0123456789abcdef".into());
        assert!(text_step(
            &args(true, 0, "a"),
            &mut input,
            &frames,
            &path,
            Some(&submit)
        )
        .is_err());
        assert_eq!(sent.borrow().len(), 2);
        stop.store(true, Ordering::Release);
        assert!(server.join().unwrap() >= 2);
    }
    #[test]
    fn missing_transport_is_unsupported_and_changes_nothing() {
        let (_dir, path, stop, server) = console(7);
        let frames = observed();
        let mut input = InputState::default();
        let error =
            text_step::<()>(&args(true, 0, MIXED), &mut input, &frames, &path, None).unwrap_err();
        assert!(error.to_string().starts_with("unsupported: "), "{error}");
        assert!(error.to_string().contains("--allow-text-input"));
        assert!(input.operation.is_none() && input.until.is_none());
        stop.store(true, Ordering::Release);
        assert_eq!(
            server.join().unwrap(),
            0,
            "no console request without a transport"
        );
    }
    #[test]
    fn source_generation_change_and_transport_refusal_send_or_report_nothing_false() {
        // The console now reports another generation than the observed frame.
        let (_dir, path, stop, server) = console(8);
        let frames = observed();
        let mut input = InputState::default();
        let calls = RefCell::new(0);
        let submit = |_: Value, _: Instant| -> Result<()> {
            *calls.borrow_mut() += 1;
            bail!("serial execution busy")
        };
        assert!(text_step(
            &args(true, 0, "a"),
            &mut input,
            &frames,
            &path,
            Some(&submit)
        )
        .is_err());
        assert_eq!(*calls.borrow(), 0);
        assert!(input.operation.is_none());
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        // A refusing transport surfaces its error and leaves no operation: nothing was sent.
        let (_dir, path, stop, server) = console(7);
        let error = text_step(
            &args(true, 0, "a"),
            &mut input,
            &frames,
            &path,
            Some(&submit),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "serial execution busy");
        assert_eq!(*calls.borrow(), 1);
        assert!(input.operation.is_none() && input.until.is_none());
        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }
    /// Host planner, worker fence and serial transport against the real Go Guest service, whose
    /// text backend talks to an in-process X server. Byte identity is asserted on the Go side.
    ///
    /// The Go test executable comes from VEYNODE_SERIAL_TEXT_GO_TEST_BIN, or is built here with
    /// the Go toolchain. Without either the test fails; it never passes without running. The only
    /// way not to run it is the explicit VEYNODE_SERIAL_TEXT_INTEROP=skip, which says so on stderr.
    /// Not real Guest evidence: the X server on the Go side is this repository's fake.
    #[test]
    fn actual_go_service_receives_planned_text_byte_identical() {
        let text = format!("{}\n\t", MIXED.repeat(8));
        let directory = tempfile::Builder::new()
            .prefix("vtgo-")
            .tempdir_in("/tmp")
            .unwrap();
        let binary = match std::env::var_os("VEYNODE_SERIAL_TEXT_GO_TEST_BIN") {
            Some(binary) => std::path::PathBuf::from(binary),
            None if std::env::var_os("VEYNODE_SERIAL_TEXT_INTEROP")
                .is_some_and(|v| v == "skip") =>
            {
                eprintln!("SKIPPED actual_go_service_receives_planned_text_byte_identical: VEYNODE_SERIAL_TEXT_INTEROP=skip; the Rust-to-Go text interop was NOT exercised");
                return;
            }
            None => {
                let built = directory.path().join("agent.test");
                let source =
                    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guest/veynode-agent");
                let status = ["go", "/opt/homebrew/bin/go", "/usr/local/go/bin/go"]
                    .iter()
                    .find_map(|go| {
                        std::process::Command::new(go)
                            .arg("-C")
                            .arg(&source)
                            .args(["test", "-c", "-o"])
                            .arg(&built)
                            .arg(".")
                            .stdout(std::process::Stdio::null())
                            .status()
                            .ok()
                    })
                    .expect("Go toolchain not found: set VEYNODE_SERIAL_TEXT_GO_TEST_BIN to a `go test -c` build of guest/veynode-agent, or VEYNODE_SERIAL_TEXT_INTEROP=skip to skip explicitly");
                assert!(status.success(), "go test -c of guest/veynode-agent failed");
                built
            }
        };
        let socket = directory.path().join("agent.sock");
        struct Owned(std::process::Child);
        impl Drop for Owned {
            fn drop(&mut self) {
                if self.0.try_wait().ok().flatten().is_none() {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
        }
        let mut child = Owned(
            std::process::Command::new(binary)
                .arg("-test.run=^TestSerialTextInteropProcess$")
                .env("VEYNODE_SERIAL_TEXT_SOCKET", &socket)
                .env(
                    "VEYNODE_SERIAL_TEXT_EXPECT_HEX",
                    text.bytes().map(|b| format!("{b:02x}")).collect::<String>(),
                )
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let until = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(Instant::now() < until);
            assert!(child.0.try_wait().unwrap().is_none());
            thread::sleep(Duration::from_millis(5));
        }
        let transport = guest_exec::Transport::start(&socket, "c".repeat(64));
        while transport.state() == "starting" {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(transport.state(), "ready");
        fn wait(mut pending: guest_exec::Pending) -> Result<Value> {
            let until = Instant::now() + Duration::from_secs(3);
            loop {
                if let Some(result) = pending.poll() {
                    return result;
                }
                assert!(Instant::now() < until, "Guest reply missing");
                thread::sleep(Duration::from_millis(2));
            }
        }
        let info = wait(
            transport
                .request(json!({"id":"info","method":"info"}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(info["result"]["capabilities"]["text_input"], true);
        let (_dir, console_path, stop, server) = console(7);
        let frames = observed();
        let mut input = InputState::default();
        let mut arguments = args(true, 0, "");
        arguments["text"] = json!(text);
        let plan = vm_control::input::plan_text(&arguments).unwrap();
        assert_eq!(plan.chunks.len(), 3);
        let submit =
            |request: Value, deadline: Instant| transport.request_before(request, deadline);
        let result = vm_control::input::deliver_text(&plan, &info, |index, chunk| {
            let step = args(index == 0, index as u64, chunk);
            let started = Instant::now();
            let reply = wait(text_step(
                &step,
                &mut input,
                &frames,
                &console_path,
                Some(&submit),
            )?)?;
            assert!(
                started.elapsed() < TEXT_CHUNK_DEADLINE,
                "chunk exceeded its reply deadline"
            );
            // What the daemon forwards: counts and codes, never the Guest's per-key diagnostics.
            assert!(reply["result"]["bindings"].is_array());
            let summary = vm_control::input::guest_text_summary(&reply);
            assert!(!summary.to_string().contains("keysym"));
            Ok(summary)
        })
        .unwrap();
        assert_eq!(result["scalars"], 42);
        assert_eq!(result["bytes"], text.len());
        assert_eq!(transport.state(), "ready");
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        drop(transport);
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success(), "Go side did not decode the exact text");
                break;
            }
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(5));
        }
    }
}
