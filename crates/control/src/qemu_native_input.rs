//! Worker-side native input fence; the daemon cannot substitute a screenshot anchor.
use super::*;
use vm_control::native_input::{DisplayedFrame, Event};
#[derive(Default)]
pub(super) struct NativeInput {
    owner: Option<String>,
    epoch: u64,
    sequence: u64,
    display: String,
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
            let status = console_window(console, "window_status")?;
            if status["display_instance"] != display {
                bail!("native display instance changed")
            }
            self.owner = Some(owner.into());
            self.epoch = epoch;
            self.sequence = sequence;
            self.display = display.into();
            input.operation =
                Some(vm_control::bridge::State::new()?.worker_instance[..32].to_owned());
            input.active_epoch = Some(epoch);
            input.until = Some(Instant::now() + Duration::from_secs(2));
            return Ok(json!({"ready":true,"epoch":epoch}));
        }
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
        let frame: DisplayedFrame = serde_json::from_value(args["frame"].clone())?;
        frame.validate()?;
        if frame.display_instance != self.display {
            bail!("native displayed frame belongs to another instance")
        }
        let event: Event = serde_json::from_value(args["event"].clone())?;
        let events = event.qmp(&frame)?;
        let mut socket = UnixStream::connect(console)?;
        socket.set_write_timeout(Some(Duration::from_millis(500)))?;
        serde_json::to_writer(
            &mut socket,
            &json!({"request_id":"native-frame","method":"validate_displayed_frame","frame":frame}),
        )?;
        socket.write_all(b"\n")?;
        let validation = read_console_reply(
            &mut socket,
            "native-frame",
            Instant::now() + Duration::from_millis(750),
        )?;
        if validation["valid"] != true
            || !frame.matches_record(&validation["frame"])
            || !validation["current"].is_boolean()
        {
            bail!("native displayed frame is stale")
        }
        let current = console_frame_status(console)?;
        if current["capture_connected"] != true
            || current["frame_available"] != true
            || current["source_generation"] != frame.source_generation
            || current["width"] != frame.width
            || current["height"] != frame.height
        {
            bail!("native source changed")
        }
        // Validation RPC and QMP are separate processes: no atomic display/input claim.
        if input.until.is_none_or(|until| Instant::now() >= until) {
            bail!("native input expired while validating frame")
        }
        input.send_events(&events, qmp)?;
        input.until = Some(Instant::now() + Duration::from_secs(2));
        Ok(json!({"ready":true,"epoch":epoch,"dispatched":true,"guest_delivery_confirmed":false}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_unix_display_proof_and_qmp_epoch_release() {
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
                        let mut f = r["frame"].clone();
                        f["source_width"] = f["width"].take();
                        f["source_height"] = f["height"].take();
                        json!({"valid":r["frame"]["presentation_serial"]!=2,"frame":f,"current":if r["frame"]["presentation_serial"]==3 {json!(1)}else{json!(true)}})
                    }
                    _ => panic!(),
                };
                let body =
                    serde_json::to_vec(&json!({"request_id":r["request_id"],"result":value}))
                        .unwrap();
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
        assert!(native
            .handle("native_event", &args, &mut input, &path, &mut qmp)
            .is_err());
        assert!(input.held.is_empty());
        args["sequence"] = json!(3);
        args["frame"]["presentation_serial"] = json!(1);
        args["frame"]["display_instance"] = json!("e".repeat(32));
        assert!(native
            .handle("native_event", &args, &mut input, &path, &mut qmp)
            .is_err());
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
        args["sequence"] = json!(5);
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
        args["sequence"] = json!(6);
        assert!(native
            .handle("native_event", &args, &mut input, &path, &mut qmp)
            .is_err());
        assert!(input.held.is_empty());
        qmp_thread.join().unwrap();
        stop.store(true, Ordering::Release);
        broker.join().unwrap();
    }
}
