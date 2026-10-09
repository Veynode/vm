//! Opt-in local actuator ownership. Targets/readback are not measured bitrate.
use crate::receiver_loss::{LossFeedback, INITIAL_BPS};
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use std::os::{fd::AsRawFd, unix::net::UnixStream};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vm_media_local::rate_control::{Receipt, Request, MAX_BPS, MIN_BPS};

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub trial: Option<[u64; 3]>,
    pub loss_response: bool,
    pub fault_rtp_loss: bool,
}
impl Options {
    pub fn check_capabilities(&self, caps: &serde_json::Value) -> Result<()> {
        ensure!(
            caps["rate_control_version"].as_u64() == Some(1)
                && caps["rate_min_bps"].as_u64() == Some(MIN_BPS)
                && caps["rate_max_bps"].as_u64() == Some(MAX_BPS),
            "encoder rate capability unavailable"
        );
        Ok(())
    }
}
pub fn parse_options(args: &[String]) -> Result<(Option<Options>, Vec<String>)> {
    let mut options = None;
    let mut rest = Vec::new();
    let mut fault_rtp_loss = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--encoder-rate-control" => {
                ensure!(options.is_none(), "duplicate encoder rate option");
                options = Some(Options {
                    trial: None,
                    loss_response: false,
                    fault_rtp_loss: false,
                });
            }
            "--encoder-rate-trial" => {
                ensure!(options.is_none(), "duplicate encoder rate option");
                i += 1;
                let raw = args.get(i).context("missing encoder rate trial")?;
                let values: Vec<u64> = raw
                    .split(',')
                    .map(|v| {
                        ensure!(
                            !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()),
                            "invalid encoder rate trial"
                        );
                        Ok(v.parse::<u64>()?)
                    })
                    .collect::<Result<_>>()?;
                let targets: [u64; 3] = values
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("rate trial requires three targets"))?;
                ensure!(
                    targets.iter().all(|v| (MIN_BPS..=MAX_BPS).contains(v))
                        && targets[0] == targets[2]
                        && targets[1] < targets[0],
                    "rate trial requires bounded high-low-high targets"
                );
                options = Some(Options {
                    trial: Some(targets),
                    loss_response: false,
                    fault_rtp_loss: false,
                });
            }
            "--encoder-loss-response" => {
                ensure!(
                    options.is_none(),
                    "duplicate or conflicting encoder rate option"
                );
                options = Some(Options {
                    trial: None,
                    loss_response: true,
                    fault_rtp_loss: false,
                });
            }
            "--fault-rtp-loss" => {
                ensure!(!fault_rtp_loss, "duplicate RTP loss fault option");
                fault_rtp_loss = true;
            }
            _ => rest.push(args[i].clone()),
        }
        i += 1;
    }
    if fault_rtp_loss {
        let option = options
            .as_mut()
            .context("RTP loss fault requires encoder loss response")?;
        ensure!(
            option.loss_response,
            "RTP loss fault requires encoder loss response"
        );
        option.fault_rtp_loss = true;
    }
    Ok((options, rest))
}

/// Child endpoint retained until spawn returns, then explicitly dropped in parent.
/// CLOEXEC is cleared only in that child's pre_exec; unrelated spawns cannot inherit it.
pub struct SpawnChannel {
    pub client: Client,
    child: UnixStream,
}
impl SpawnChannel {
    pub fn configure(command: &mut tokio::process::Command) -> Result<Self> {
        let (parent, child) = UnixStream::pair()?;
        parent.set_nonblocking(true)?;
        let fd = child.as_raw_fd();
        command.arg("--rate-control-fd").arg(fd.to_string());
        unsafe {
            command.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(Self {
            client: Client {
                socket: tokio::net::UnixStream::from_std(parent)?,
                current: None,
                fenced: false,
            },
            child,
        })
    }
    pub fn spawned(self) -> Client {
        drop(self.child);
        self.client
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u64,
    request_id: String,
    result: Receipt,
}
/// Mutable access enforces one in-flight request. Cancellation leaves fenced=true;
/// no caller may retry a partially written request or an unconsumed acknowledgement.
pub struct Client {
    socket: tokio::net::UnixStream,
    current: Option<Receipt>,
    fenced: bool,
}
impl Client {
    async fn exchange(&mut self, request: Request) -> Result<Receipt> {
        ensure!(!self.fenced, "encoder rate channel fenced");
        request.validate()?;
        let mut wire = serde_json::to_vec(&request)?;
        wire.push(b'\n');
        ensure!(wire.len() <= 1024, "encoder rate request oversized");
        self.fenced = true;
        let answer = tokio::time::timeout(Duration::from_secs(3), async {
            self.socket.write_all(&wire).await?;
            let mut bytes = Vec::with_capacity(1024);
            loop {
                ensure!(bytes.len() < 1024, "encoder rate reply oversized");
                let b = self.socket.read_u8().await?;
                bytes.push(b);
                if b == b'\n' {
                    break;
                }
            }
            let response: Response =
                serde_json::from_slice(&bytes).context("invalid encoder rate reply")?;
            ensure!(
                response.version == 1 && response.request_id == request.request_id,
                "encoder rate reply correlation failed"
            );
            let epoch = self
                .current
                .as_ref()
                .map_or(response.result.epoch, |r| r.epoch);
            response.result.validate(&request, epoch)?;
            if let Some(current) = &self.current {
                ensure!(
                    response.result.encoder_instance == current.encoder_instance
                        && response.result.submitted_frames >= current.submitted_frames,
                    "encoder rate incarnation changed"
                );
                if request.method == "rate_status" {
                    ensure!(
                        response.result.sequence == current.sequence
                            && response.result.target_bps == current.target_bps,
                        "encoder rate status changed unexpectedly"
                    );
                }
            } else {
                ensure!(
                    response.result.sequence == 0,
                    "encoder rate initial sequence invalid"
                );
            }
            Ok::<_, anyhow::Error>(response.result)
        })
        .await
        .context("encoder rate deadline")??;
        self.fenced = false;
        Ok(answer)
    }
    pub async fn status(&mut self) -> Result<&Receipt> {
        let receipt = self
            .exchange(Request {
                version: 1,
                request_id: uuid::Uuid::new_v4().simple().to_string(),
                method: "rate_status".into(),
                encoder_instance: None,
                epoch: None,
                sequence: None,
                target_bps: None,
            })
            .await?;
        self.current = Some(receipt);
        Ok(self.current.as_ref().unwrap())
    }
    pub async fn update(&mut self, target: u64) -> Result<&Receipt> {
        let current = self
            .current
            .as_ref()
            .context("encoder rate status required")?;
        let request = Request {
            version: 1,
            request_id: uuid::Uuid::new_v4().simple().to_string(),
            method: "set_bitrate".into(),
            encoder_instance: Some(current.encoder_instance.clone()),
            epoch: Some(current.epoch),
            sequence: Some(
                current
                    .sequence
                    .checked_add(1)
                    .context("encoder rate sequence exhausted")?,
            ),
            target_bps: Some(target),
        };
        let receipt = self.exchange(request).await?;
        self.current = Some(receipt);
        Ok(self.current.as_ref().unwrap())
    }
}
fn diagnostic(receipt: &Receipt, operation: &str, started: Instant) {
    eprintln!(
        "{}",
        serde_json::json!({"version":1,"event":"encoder_rate_ack","elapsed_ms":started.elapsed().as_millis(),"operation":operation,"receipt":receipt,"measured_bitrate":false})
    );
}

/// Runs alongside the AU reader. End of the finite trial does not end the media
/// session; retain endpoint ownership until the actual child stop/reap completes.
pub async fn drive(
    client: &mut Option<Client>,
    options: Option<&Options>,
    mut first: tokio::sync::watch::Receiver<bool>,
    loss: Option<&LossFeedback>,
    started: Instant,
) -> Result<()> {
    let (Some(client), Some(options)) = (client.as_mut(), options) else {
        return std::future::pending().await;
    };
    while !*first.borrow_and_update() {
        first
            .changed()
            .await
            .context("encoder first frame channel closed")?;
    }
    diagnostic(client.status().await?, "status", started);
    if options.loss_response {
        return drive_loss(client, loss.context("loss observer missing")?, started).await;
    }
    if let Some(targets) = options.trial {
        for target in targets {
            diagnostic(client.update(target).await?, "update", started);
            tokio::time::sleep(Duration::from_secs(6)).await;
        }
        diagnostic(client.status().await?, "trial_complete", started);
    }
    // Detect a dead/private protocol endpoint even after the finite trial.
    let mut unexpected = [0u8; 1];
    client.fenced = true;
    let n = client.socket.read(&mut unexpected).await?;
    if n == 0 {
        bail!("encoder rate channel EOF");
    }
    bail!("unsolicited encoder rate reply")
}

/// Fixed loss response, not bandwidth estimation. Only successfully acknowledged
/// native targets feed the next decision; an exchange failure exits this session.
async fn drive_loss(client: &mut Client, feedback: &LossFeedback, started: Instant) -> Result<()> {
    apply_loss_update(
        client,
        INITIAL_BPS,
        "loss_response_initial",
        None,
        None,
        started,
    )
    .await?;
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_log = None;
    loop {
        let mut unexpected = [0u8; 1];
        tokio::select! {
            reply = client.socket.read(&mut unexpected) => {
                client.fenced = true;
                eprintln!("{}", serde_json::json!({"version":1,"event":"receiver_loss_failure","elapsed_ms":started.elapsed().as_millis(),"reason":"private_channel_closed_or_unsolicited"}));
                reply?;
                bail!("encoder loss response channel closed or unsolicited");
            }
            _ = ticker.tick() => {}
        }
        let now = Instant::now();
        let target = client
            .current
            .as_ref()
            .context("encoder rate status missing")?
            .target_bps;
        let poll = feedback.poll(now, target);
        if last_log.is_none_or(|last: Instant| now.duration_since(last) >= Duration::from_secs(1))
            || poll.update.is_some()
            || poll.failure.is_some()
        {
            eprintln!(
                "{}",
                serde_json::json!({"version":1,"event":"receiver_loss_feedback","elapsed_ms":started.elapsed().as_millis(),"acknowledged_target_bps":target,"diagnostics":poll.diagnostics})
            );
            last_log = Some(now);
        }
        if poll.failure.is_some() {
            eprintln!(
                "{}",
                serde_json::json!({"version":1,"event":"receiver_loss_failure","elapsed_ms":started.elapsed().as_millis(),"reason":"observer_or_fault_budget_failed"})
            );
            bail!("receiver loss diagnostic failed");
        }
        if let Some(update) = poll.update {
            ensure!(
                update.observation == update.cause.observation,
                "loss decision evidence mismatch"
            );
            apply_loss_update(
                client,
                update.target_bps,
                update.reason,
                Some(&update.cause),
                Some(&poll.diagnostics),
                started,
            )
            .await?;
        }
    }
}
async fn apply_loss_update(
    client: &mut Client,
    target_bps: u64,
    reason: &'static str,
    cause: Option<&crate::receiver_loss::DecisionEvidence>,
    diagnostics: Option<&crate::receiver_loss::Diagnostics>,
    started: Instant,
) -> Result<()> {
    let current = client
        .current
        .as_ref()
        .context("encoder rate status missing")?;
    let observation = cause.map_or(0, |value| value.observation);
    let request = serde_json::json!({"encoder_instance":current.encoder_instance,"epoch":current.epoch,"sequence":current.sequence.checked_add(1).context("rate sequence exhausted")?,"target_bps":target_bps});
    eprintln!(
        "{}",
        serde_json::json!({"version":1,"event":"receiver_loss_rate_request","elapsed_ms":started.elapsed().as_millis(),"observation":observation,"reason":reason,"request":request,"diagnostics":diagnostics,"cause":cause})
    );
    let receipt = match client.update(target_bps).await {
        Ok(receipt) => receipt,
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({"version":1,"event":"receiver_loss_rate_failure","elapsed_ms":started.elapsed().as_millis(),"observation":observation,"reason":"actuator_exchange_failed","request":request})
            );
            return Err(error);
        }
    };
    eprintln!(
        "{}",
        serde_json::json!({"version":1,"event":"receiver_loss_rate_ack","elapsed_ms":started.elapsed().as_millis(),"observation":observation,"reason":reason,"receipt":receipt,"diagnostics":diagnostics,"cause":cause,"measured_bitrate":false})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    fn pair() -> (Client, tokio::net::UnixStream) {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        (
            Client {
                socket: a,
                current: None,
                fenced: false,
            },
            b,
        )
    }
    async fn request(peer: &mut tokio::net::UnixStream) -> Value {
        let mut bytes = Vec::new();
        loop {
            let b = peer.read_u8().await.unwrap();
            bytes.push(b);
            if b == b'\n' {
                break;
            }
        }
        serde_json::from_slice(&bytes).unwrap()
    }
    fn reply(q: &Value, sequence: u64) -> Value {
        json!({"version":1,"request_id":q["request_id"],"result":{"encoder_instance":"0123456789abcdef0123456789abcdef","epoch":7,"sequence":sequence,"target_bps":q["target_bps"].as_u64().unwrap_or(8_000_000),"readback_bps":q["target_bps"].as_u64().unwrap_or(8_000_000),"submitted_frames":sequence+1,"hardware_verified":true}})
    }
    async fn send(peer: &mut tokio::net::UnixStream, v: Value) {
        peer.write_all(format!("{v}\n").as_bytes()).await.unwrap();
    }
    #[test]
    fn options_are_explicit_bounded_and_do_not_change_default() {
        let args = vec!["--guest-stream".into()];
        assert_eq!(parse_options(&args).unwrap(), (None, args));
        for value in ["2000000,250000,2000000", "40000000,250000,40000000"] {
            assert!(
                parse_options(&["--encoder-rate-trial".into(), value.into()])
                    .unwrap()
                    .0
                    .is_some()
            );
        }
        for value in [
            "1,2,1",
            "2000000,250000,3000000",
            "8000000,8000000,8000000",
            "2e6,250000,2e6",
            "2000000,250000",
        ] {
            assert!(parse_options(&["--encoder-rate-trial".into(), value.into()]).is_err());
        }
        assert!(parse_options(&[
            "--encoder-rate-control".into(),
            "--encoder-rate-control".into()
        ])
        .is_err());
        let option = Options {
            trial: None,
            loss_response: false,
            fault_rtp_loss: false,
        };
        assert!(option
            .check_capabilities(
                &json!({"rate_control_version":true,"rate_min_bps":MIN_BPS,"rate_max_bps":MAX_BPS})
            )
            .is_err());
    }
    #[test]
    fn loss_mode_is_independent_and_fault_never_silently_enabled() {
        let (options, rest) = parse_options(&[
            "--auth-state".into(),
            "/private/auth".into(),
            "--fault-rtp-loss".into(),
            "--encoder-loss-response".into(),
        ])
        .unwrap();
        let options = options.unwrap();
        assert!(options.loss_response && options.fault_rtp_loss);
        assert!(options.trial.is_none());
        assert_eq!(rest, vec!["--auth-state", "/private/auth"]);
        for flags in [
            vec!["--fault-rtp-loss"],
            vec!["--encoder-rate-control", "--fault-rtp-loss"],
            vec!["--encoder-loss-response", "--encoder-rate-control"],
            vec![
                "--encoder-rate-trial",
                "2000000,250000,2000000",
                "--encoder-loss-response",
            ],
            vec![
                "--encoder-loss-response",
                "--fault-rtp-loss",
                "--fault-rtp-loss",
            ],
        ] {
            assert!(
                parse_options(&flags.into_iter().map(String::from).collect::<Vec<_>>()).is_err()
            );
        }
        let (options, _) = parse_options(&["--encoder-loss-response".into()]).unwrap();
        assert!(!options.unwrap().fault_rtp_loss);
    }
    #[tokio::test]
    async fn unavailable_feedback_keeps_acknowledged_initial_target_without_stopping() {
        use rtc::interceptor::Interceptor;
        let (mut observer, feedback) = crate::receiver_loss::ReceiverLoss::new(12345678, false);
        observer.bind_local_stream(&rtc::interceptor::StreamInfo {
            ssrc: 12345678,
            ..Default::default()
        });
        let (mut client, mut peer) = pair();
        let server = tokio::spawn(async move {
            let q = request(&mut peer).await;
            send(&mut peer, reply(&q, 0)).await;
            let q = request(&mut peer).await;
            assert_eq!(q["target_bps"], INITIAL_BPS);
            send(&mut peer, reply(&q, 1)).await;
            assert!(
                tokio::time::timeout(Duration::from_millis(2300), peer.read_u8())
                    .await
                    .is_err()
            );
        });
        client.status().await.unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(2100),
            drive_loss(&mut client, &feedback, Instant::now())
        )
        .await
        .is_err());
        assert_eq!(client.current.as_ref().unwrap().target_bps, INITIAL_BPS);
        assert_eq!(client.current.as_ref().unwrap().sequence, 1);
        assert!(!client.fenced);
        server.await.unwrap();
    }
    fn high_loss_feedback() -> (crate::receiver_loss::ReceiverLoss, LossFeedback) {
        use rtc::interceptor::{AttributedPacket, Interceptor, Packet, StreamInfo, TaggedPacket};
        use rtc::rtcp::{
            receiver_report::ReceiverReport, reception_report::ReceptionReport,
            sender_report::SenderReport,
        };
        use rtc::sansio::Protocol;
        let (mut observer, feedback) = crate::receiver_loss::ReceiverLoss::new(12345678, false);
        observer.bind_local_stream(&StreamInfo {
            ssrc: 12345678,
            ..Default::default()
        });
        let now = Instant::now();
        let msg = |packet| TaggedPacket {
            now,
            transport: Default::default(),
            message: AttributedPacket::new(packet),
        };
        for seq in 0..256u16 {
            let packet = rtc::rtp::Packet {
                header: rtc::rtp::header::Header {
                    ssrc: 12345678,
                    sequence_number: seq,
                    ..Default::default()
                },
                ..Default::default()
            };
            observer.handle_write(msg(Packet::Rtp(packet))).unwrap();
            observer.poll_write();
            if seq == 0 {
                observer
                    .handle_write(msg(Packet::Rtcp(vec![Box::new(SenderReport {
                        ssrc: 12345678,
                        ntp_time: 1u64 << 32,
                        ..Default::default()
                    })])))
                    .unwrap();
                observer.poll_write();
            }
            if seq == 127 || seq == 255 {
                observer
                    .handle_read(msg(Packet::Rtcp(vec![Box::new(ReceiverReport {
                        ssrc: 777,
                        reports: vec![ReceptionReport {
                            ssrc: 12345678,
                            last_sequence_number: seq as u32,
                            total_lost: if seq == 255 { 16 } else { 0 },
                            last_sender_report: 65536,
                            ..Default::default()
                        }],
                        ..Default::default()
                    })])))
                    .unwrap();
                observer.poll_read();
            }
        }
        (observer, feedback)
    }
    #[tokio::test]
    async fn real_observer_rr_drives_same_private_encoder_and_invalid_ack_fences() {
        for invalid_ack in [false, true] {
            let (_observer, feedback) = high_loss_feedback();
            let (mut client, mut peer) = pair();
            let server = tokio::spawn(async move {
                let q = request(&mut peer).await;
                send(&mut peer, reply(&q, 0)).await;
                let q = request(&mut peer).await;
                assert_eq!(q["target_bps"], INITIAL_BPS);
                send(&mut peer, reply(&q, 1)).await;
                let q = request(&mut peer).await;
                assert_eq!(q["target_bps"], 1_000_000);
                assert_eq!(q["sequence"], 2);
                let mut value = reply(&q, 2);
                if invalid_ack {
                    value["result"]["sequence"] = serde_json::json!(1);
                }
                send(&mut peer, value).await;
            });
            client.status().await.unwrap();
            assert!(tokio::time::timeout(
                Duration::from_secs(2),
                drive_loss(&mut client, &feedback, Instant::now())
            )
            .await
            .unwrap()
            .is_err());
            assert_eq!(
                client.current.as_ref().unwrap().target_bps,
                if invalid_ack { INITIAL_BPS } else { 1_000_000 }
            );
            assert!(client.fenced);
            server.await.unwrap();
        }
    }
    #[tokio::test]
    async fn exact_incarnation_sequence_and_submission_boundary() {
        let (mut client, mut peer) = pair();
        let server = tokio::spawn(async move {
            let q = request(&mut peer).await;
            send(&mut peer, reply(&q, 0)).await;
            let q = request(&mut peer).await;
            assert_eq!(q["epoch"], 7);
            assert_eq!(q["sequence"], 1);
            send(&mut peer, reply(&q, 1)).await;
            let q = request(&mut peer).await;
            let mut v = reply(&q, 2);
            v["result"]["encoder_instance"] = json!("ffffffffffffffffffffffffffffffff");
            send(&mut peer, v).await;
        });
        assert_eq!(client.status().await.unwrap().sequence, 0);
        assert_eq!(client.update(250000).await.unwrap().target_bps, 250000);
        assert!(client.update(2_000_000).await.is_err());
        assert!(client.fenced);
        assert!(client.status().await.is_err());
        server.await.unwrap();
    }
    #[tokio::test]
    async fn stale_sequence_readback_and_submission_regression_are_terminal() {
        for field in ["sequence", "readback_bps", "submitted_frames", "epoch"] {
            let (mut client, mut peer) = pair();
            let server = tokio::spawn(async move {
                let q = request(&mut peer).await;
                send(&mut peer, reply(&q, 0)).await;
                let q = request(&mut peer).await;
                let mut v = reply(&q, 1);
                v["result"][field] = json!(0);
                send(&mut peer, v).await;
            });
            client.status().await.unwrap();
            assert!(client.update(250000).await.is_err());
            assert!(client.fenced);
            server.await.unwrap();
        }
    }
    #[tokio::test]
    async fn cancelled_partial_exchange_cannot_retry_and_does_not_close_until_owner_drop() {
        let (mut client, mut peer) = pair();
        let result = tokio::time::timeout(Duration::from_millis(20), client.status()).await;
        assert!(result.is_err());
        assert!(client.fenced);
        let _ = request(&mut peer).await;
        assert!(client.status().await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), peer.read_u8())
                .await
                .is_err()
        );
        drop(client);
        assert!(peer.read_u8().await.is_err());
    }
    #[tokio::test]
    async fn malformed_duplicate_oversized_partial_and_eof_fence() {
        for mode in 0..5 {
            let (mut client, mut peer) = pair();
            let server = tokio::spawn(async move {
                let q = request(&mut peer).await;
                let bytes = match mode {
                    0 => b"{\"version\":1,\"version\":1}\n".to_vec(),
                    1 => vec![b'x'; 1024],
                    2 => b"{\"version\":1".to_vec(),
                    3 => Vec::new(),
                    _ => {
                        let mut v = reply(&q, 0);
                        v["request_id"] = json!("wrong");
                        format!("{v}\n").into_bytes()
                    }
                };
                peer.write_all(&bytes).await.unwrap();
            });
            assert!(client.status().await.is_err());
            assert!(client.fenced);
            server.await.unwrap();
        }
    }
    #[tokio::test]
    async fn private_eof_after_status_terminates_driver() {
        let (client, mut peer) = pair();
        let mut client = Some(client);
        let (_tx, rx) = tokio::sync::watch::channel(true);
        let server = tokio::spawn(async move {
            let q = request(&mut peer).await;
            send(&mut peer, reply(&q, 0)).await;
        });
        assert!(drive(
            &mut client,
            Some(&Options {
                trial: None,
                loss_response: false,
                fault_rtp_loss: false
            }),
            rx,
            None,
            Instant::now()
        )
        .await
        .is_err());
        assert!(client.unwrap().fenced);
        server.await.unwrap();
    }
    #[tokio::test]
    async fn actual_owned_child_inherits_fd_and_stop_cleanup_precedes_channel_drop() {
        use std::process::Stdio;
        let mut command = tokio::process::Command::new("python3");
        command.args(["-c", r#"
import json,socket,sys,select
s=socket.socket(fileno=int(sys.argv[sys.argv.index('--rate-control-fd')+1]))
data=b''
while not data.endswith(b'\n'): data+=s.recv(1)
q=json.loads(data)
s.sendall((json.dumps({'version':1,'request_id':q['request_id'],'result':{'encoder_instance':'0123456789abcdef0123456789abcdef','epoch':1,'sequence':0,'target_bps':8000000,'readback_bps':8000000,'submitted_frames':1,'hardware_verified':True}})+'\n').encode())
assert sys.stdin.buffer.readline()==b'stop\n'
assert not select.select([s],[],[],0)[0], 'rate endpoint closed before stop'
sys.stdout.buffer.write(b'x'*1048576)
sys.stdout.buffer.flush()
"#]);
        let channel = SpawnChannel::configure(&mut command).unwrap();
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut client = channel.spawned();
        client.status().await.unwrap();
        let input = child.stdin.take().unwrap();
        let mut output = child.stdout.take().unwrap();
        let outcome =
            super::super::encoder_cleanup::stop_and_wait(&mut child, input, &mut output).await;
        assert!(outcome.child_reaped);
        assert!(outcome.graceful_status.unwrap().success());
        drop(client);
    }
}
