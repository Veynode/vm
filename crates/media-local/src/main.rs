//! Loopback-only integration diagnostic. Not an authenticated internet endpoint.
mod auth;
mod rtcp_forwarder;
use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rtc::{
    interceptor::Registry,
    media_stream::MediaStreamTrack,
    peer_connection::{
        configuration::{
            interceptor_registry::register_default_interceptors, media_engine::MediaEngine,
            RTCConfigurationBuilder,
        },
        sdp::RTCSessionDescription,
    },
    rtp_transceiver::rtp_sender::{
        RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
        RtpCodecKind,
    },
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{watch, Mutex},
};
use webrtc::{
    media_stream::{
        track_local::{static_rtp::TrackLocalStaticRTP, TrackLocal},
        Track,
    },
    peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
        RTCPeerConnectionState,
    },
};

#[derive(Clone)]
struct App {
    origin: String,
    host: String,
    encoder: PathBuf,
    socket: PathBuf,
    web: PathBuf,
    capabilities: Value,
    fault_drop_first_keyframe: bool,
    fault_suppress_hevc_frames: bool,
    active: Arc<Mutex<SessionBook>>,
    auth: Option<Arc<auth::Auth>>,
    cleanup_failure_exit: Option<i32>,
}
#[derive(Default)]
struct SessionBook {
    stopping: bool,
    active: Option<Session>,
    cancelled: BTreeMap<String, Instant>,
    completed: BTreeMap<String, (Instant, bool, String)>,
}
impl SessionBook {
    fn prune(&mut self) {
        self.cancelled
            .retain(|_, at| at.elapsed() < Duration::from_secs(60));
        self.completed
            .retain(|_, (at, _, _)| at.elapsed() < Duration::from_secs(60));
    }
    fn cancel_id(&mut self, id: &str) -> bool {
        self.prune();
        if self.cancelled.len() >= 256 && !self.cancelled.contains_key(id) {
            return false;
        }
        self.cancelled.insert(id.into(), Instant::now());
        true
    }
}
struct Session {
    request: String,
    id: String,
    cancel: watch::Sender<bool>,
    finished: watch::Receiver<Option<bool>>,
    confirmed: watch::Sender<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Offer {
    request_id: String,
    sdp: String,
    r#type: String,
    codec: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Close {
    request_id: String,
    session_id: Option<String>,
}
struct Handler {
    gathered: watch::Sender<bool>,
    connected: watch::Sender<bool>,
    lost: watch::Sender<bool>,
}
#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.send(true);
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if state == RTCPeerConnectionState::Connected {
            let _ = self.connected.send(true);
        }
        if matches!(
            state,
            RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Disconnected
                | RTCPeerConnectionState::Closed
        ) {
            let _ = self.lost.send(true);
        }
    }
}
async fn flag(mut value: watch::Receiver<bool>) {
    loop {
        if *value.borrow_and_update() {
            return;
        }
        if value.changed().await.is_err() {
            return;
        }
    }
}
async fn completion(mut value: watch::Receiver<Option<bool>>) -> bool {
    loop {
        if let Some(clean) = *value.borrow_and_update() {
            return clean;
        }
        if value.changed().await.is_err() {
            return false;
        }
    }
}
fn check(app: &App, headers: &HeaderMap, mutation: bool) -> std::result::Result<(), &'static str> {
    if headers.get(header::HOST).and_then(|v| v.to_str().ok()) != Some(&app.host) {
        return Err("invalid Host");
    }
    if (mutation || headers.contains_key(header::ORIGIN))
        && headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(&app.origin)
    {
        return Err("same-origin required");
    }
    Ok(())
}
async fn get_capabilities(State(app): State<App>, headers: HeaderMap) -> Response {
    if let Err(e) = check(&app, &headers, false) {
        return (StatusCode::FORBIDDEN, e).into_response();
    }
    let mut value = app.capabilities.clone();
    value["authentication"] = json!(app
        .auth
        .as_ref()
        .map(|a| json!({"required":true,"host_public_key":a.host_public_key,"vm_id":a.vm_id}))
        .unwrap_or(json!({"required":false})));
    Json(value).into_response()
}
async fn asset(app: App, headers: HeaderMap, name: &str, mime: &str) -> Response {
    if let Err(e) = check(&app, &headers, false) {
        return (StatusCode::FORBIDDEN, e).into_response();
    }
    match tokio::fs::read(app.web.join(name)).await { Ok(body)=>([(header::CONTENT_TYPE,mime),(header::CACHE_CONTROL,"no-store"),(header::CONTENT_SECURITY_POLICY,"default-src 'self'; connect-src 'self'; media-src 'self' blob:; img-src 'self' data:; object-src 'none'; frame-ancestors 'none'")],body).into_response(), Err(_)=>(StatusCode::NOT_FOUND,"asset unavailable").into_response() }
}
async fn close(State(app): State<App>, headers: HeaderMap, Json(req): Json<Close>) -> Response {
    if let Err(e) = check(&app, &headers, true) {
        return (StatusCode::FORBIDDEN, e).into_response();
    }
    if app.auth.is_some() {
        return (StatusCode::FORBIDDEN, "signed close required").into_response();
    }
    close_inner(&app, req).await
}
async fn close_inner(app: &App, req: Close) -> Response {
    let finished = {
        let mut book = app.active.lock().await;
        if uuid::Uuid::parse_str(&req.request_id).is_err() {
            return (StatusCode::BAD_REQUEST, "invalid request id").into_response();
        }
        if !book.cancel_id(&req.request_id) {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "cancellation capacity reached",
            )
                .into_response();
        }
        match book.active.as_ref() {
            Some(session)
                if session.request == req.request_id
                    && req.session_id.as_ref().is_none_or(|id| id == &session.id) =>
            {
                let _ = session.cancel.send(true);
                Some(session.finished.clone())
            }
            Some(_) => return (StatusCode::FORBIDDEN, "session mismatch").into_response(),
            None => {
                if book
                    .completed
                    .get(&req.request_id)
                    .is_some_and(|(_, _, id)| {
                        req.session_id
                            .as_ref()
                            .is_some_and(|requested| requested != id)
                    })
                {
                    return (StatusCode::FORBIDDEN, "session mismatch").into_response();
                }
                if book
                    .completed
                    .get(&req.request_id)
                    .is_some_and(|(_, clean, _)| !*clean)
                {
                    return (StatusCode::GATEWAY_TIMEOUT, "cleanup unconfirmed").into_response();
                }
                None
            }
        }
    };
    if let Some(finished) = finished {
        if !matches!(
            tokio::time::timeout(Duration::from_secs(4), completion(finished)).await,
            Ok(true)
        ) {
            return (StatusCode::GATEWAY_TIMEOUT, "cleanup unconfirmed").into_response();
        }
    }
    Json(json!({"closed":true})).into_response()
}
async fn offer(State(app): State<App>, headers: HeaderMap, Json(req): Json<Offer>) -> Response {
    if let Err(e) = check(&app, &headers, true) {
        return (StatusCode::FORBIDDEN, e).into_response();
    }
    if app.auth.is_some() {
        return (StatusCode::FORBIDDEN, "signed offer required").into_response();
    }
    offer_inner(app, req, None).await
}
async fn offer_inner(
    app: App,
    req: Offer,
    accepted: Option<vm_session_auth::AcceptedOffer>,
) -> Response {
    if uuid::Uuid::parse_str(&req.request_id).is_err()
        || req.r#type != "offer"
        || req.sdp.len() > 200_000
        || !matches!(req.codec.as_str(), "h264" | "h265")
    {
        return (StatusCode::BAD_REQUEST, "invalid offer").into_response();
    }
    if app.capabilities[&req.codec] != "verified-hardware" {
        return (
            StatusCode::CONFLICT,
            "required hardware encoder unavailable",
        )
            .into_response();
    }
    let (cancel, cancel_rx) = watch::channel(false);
    let (confirmed, confirmed_rx) = watch::channel(accepted.is_none());
    let cancel_on_timeout = cancel.clone();
    let (finished_tx, finished) = watch::channel(None);
    let cleanup_confirmed = Arc::new(AtomicBool::new(true));
    let id = uuid::Uuid::new_v4().to_string();
    let request = req.request_id.clone();
    {
        let mut book = app.active.lock().await;
        if book.stopping {
            return (StatusCode::SERVICE_UNAVAILABLE, "service stopping").into_response();
        }
        book.prune();
        if book.cancelled.contains_key(&request) || book.completed.contains_key(&request) {
            return (StatusCode::CONFLICT, "request already cancelled").into_response();
        }
        if book.cancelled.len() >= 256 || book.completed.len() >= 256 {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "cancellation capacity reached",
            )
                .into_response();
        }
        if book.active.is_some() {
            return (
                StatusCode::CONFLICT,
                "one diagnostic session already active",
            )
                .into_response();
        }
        book.active = Some(Session {
            request: request.clone(),
            id: id.clone(),
            cancel,
            finished,
            confirmed,
        });
    }
    let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();
    let session_id = id.clone();
    tokio::spawn(async move {
        let outcome = run_session(
            &app,
            &req,
            cancel_rx,
            answer_tx,
            cleanup_confirmed.clone(),
            auth::SessionSetup {
                id: session_id.clone(),
                accepted,
                confirmed: confirmed_rx,
            },
        )
        .await;
        if let Some(auth) = &app.auth {
            auth.gate.close(&session_id);
        }
        let clean = cleanup_confirmed.load(Ordering::Acquire);
        // No SDP, ICE credentials or frame data in logs.
        if let Err(error) = outcome {
            eprintln!("media session failed: {error:#}");
        }
        {
            let mut active = app.active.lock().await;
            if active.active.as_ref().is_some_and(|s| s.request == request) {
                active.active = None;
            }
            active
                .completed
                .insert(request, (Instant::now(), clean, session_id));
        }
        let _ = finished_tx.send(Some(clean));
    });
    match tokio::time::timeout(Duration::from_secs(8), answer_rx).await {
        Ok(Ok(Ok(mut answer))) => {
            answer["session_id"] = json!(id);
            Json(answer).into_response()
        }
        Ok(Ok(Err(error))) => {
            (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response()
        }
        _ => {
            let _ = cancel_on_timeout.send(true);
            (StatusCode::GATEWAY_TIMEOUT, "offer failed or timed out").into_response()
        }
    }
}
async fn run_session(
    app: &App,
    req: &Offer,
    cancel: watch::Receiver<bool>,
    answer: tokio::sync::oneshot::Sender<Result<Value, String>>,
    cleanup_confirmed: Arc<AtomicBool>,
    authorization: auth::SessionSetup,
) -> Result<()> {
    let (gathered_tx, gathered) = watch::channel(false);
    let (connected_tx, connected) = watch::channel(false);
    let (lost_tx, lost) = watch::channel(false);
    let codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: if req.codec == "h265" {
                "video/H265"
            } else {
                "video/H264"
            }
            .into(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: if req.codec == "h265" {
                "profile-id=1;tier-flag=0;level-id=120;tx-mode=SRST"
            } else {
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
            }
            .into(),
            rtcp_feedback: vec![],
        },
        payload_type: if req.codec == "h265" { 98 } else { 102 },
    };
    let mut engine = MediaEngine::default();
    engine.register_codec(codec.clone(), RtpCodecKind::Video)?;
    let registry = register_default_interceptors(Registry::new(), &mut engine)?.with(
        rtc::interceptor::Slot::from(14_000),
        rtcp_forwarder::RtcpForwarder::new(12345678),
    );
    let peer = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(engine)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Handler {
            gathered: gathered_tx,
            connected: connected_tx,
            lost: lost_tx,
        }))
        .with_runtime(webrtc::runtime::default_runtime().context("Tokio runtime unavailable")?)
        .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
        .build()
        .await?;
    let result=async {
        let track=Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new("vm-console".into(),"video".into(),"VM desktop".into(),RtpCodecKind::Video,vec![RTCRtpEncodingParameters{rtp_coding_parameters:RTCRtpCodingParameters{ssrc:Some(12345678),..Default::default()},codec:codec.rtp_codec.clone(),..Default::default()}])));
        let sender=peer.add_track(track.clone() as Arc<dyn TrackLocal>).await?;
        let setup=async {
            peer.set_remote_description(serde_json::from_value::<RTCSessionDescription>(json!({"type":"offer","sdp":req.sdp}))?).await?;
            let answer=peer.create_answer(None).await?;peer.set_local_description(answer).await?;
            tokio::time::timeout(Duration::from_secs(4),flag(gathered)).await?;
            let answer=peer.local_description().await.context("missing answer")?;
            let mut value = serde_json::to_value(answer)?;
            if let Some(accepted) = authorization.accepted {
                let auth = app.auth.as_ref().context("authentication unavailable")?;
                auth.reload()?;
                let signed = auth.sign_answer(accepted, &authorization.id, value["sdp"].as_str().context("answer SDP missing")?)?;
                value["signed_answer"] = serde_json::to_value(signed)?;
            }
            Ok::<Value,anyhow::Error>(value)
        };
        let setup=tokio::select!{result=setup=>result,_=flag(cancel.clone())=>Err(anyhow!("cancelled during negotiation"))};
        match setup {Ok(description)=>{let _=answer.send(Ok(description));},Err(error)=>{let _=answer.send(Err("WebRTC negotiation failed".into()));return Err(error);}}
        let ready=async { tokio::try_join!(tokio::time::timeout(Duration::from_secs(12),flag(connected)),tokio::time::timeout(Duration::from_secs(12),flag(authorization.confirmed)))?; Ok::<(),anyhow::Error>(()) };
        tokio::select!{result=ready=>result?,_=flag(cancel.clone())=>return Ok(()),_=flag(lost.clone())=>bail!("connection failed before streaming"),result=auth::watch_authorization(app.auth.clone(),&authorization.id,false)=>return result};
        if let Some(auth) = &app.auth { auth.reload()?; auth.gate.authorize_active(&authorization.id,"observe")?; }
        let parameters=sender.get_parameters().await?;
        let selected=parameters.rtp_parameters.codecs.iter().find(|p|native_codec_matches(&req.codec,&p.rtp_codec.mime_type,&p.rtp_codec.sdp_fmtp_line)).context("negotiated codec profile does not match native encoder; refusing MIME-only fallback")?;
        let payload=selected.payload_type;
        eprintln!("media negotiated codec={} payload={} fmtp={}",req.codec,payload,selected.rtp_codec.sdp_fmtp_line);
        let ssrc=*track.ssrcs().await.first().context("missing SSRC")?;
        let mut child=tokio::process::Command::new(&app.encoder).arg(&app.socket).args(["--codec",if req.codec=="h265"{"hevc"}else{"h264"}]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true).spawn()?;
        cleanup_confirmed.store(false,Ordering::Release);
        let mut input=child.stdin.take().context("native stdin")?;let mut output=child.stdout.take().context("native stdout")?;
        let mut packetizer=rtc::rtp::packetizer::new_packetizer(Instant::now(),1200,payload,ssrc,codec.rtp_codec.payloader()?,Box::new(rtc::rtp::sequence::new_random_sequencer()),90_000);
        let timestamp_base=uuid::Uuid::new_v4().as_u128() as u32;
        let streaming=async {
            let mut previous=None; let mut frames=0_u64;
            loop {
                let mut header=[0_u8;13]; output.read_exact(&mut header).await.context("native frame header")?;
                let (len,pts,key)=frame_header(header,previous)?; let mut data=vec![0_u8;len];output.read_exact(&mut data).await?;
                if frames==0 && !key {bail!("first native frame must be keyframe")};
                previous=Some(pts);
                if req.codec=="h265" && app.fault_suppress_hevc_frames {
                    if frames==0 {eprintln!("FAULT: suppressing actual hardware HEVC access units before RTP to exercise first-frame fallback");}
                    frames+=1;continue;
                }
                if frames==0 && app.fault_drop_first_keyframe { frames+=1;eprintln!("FAULT: deliberately dropped first complete keyframe before RTP; awaiting actual receiver recovery feedback");continue; }
                if key {eprintln!("media native keyframe index={frames}");}
                use rtc::rtp::packetizer::Packetizer;
                let mut packets=packetizer.packetize(Instant::now(),&data.into(),0)?;
                let timestamp=timestamp_base.wrapping_add(((pts as u128*90_000+500_000)/1_000_000) as u32);
                for packet in &mut packets { packet.header.timestamp=timestamp; }
                tokio::time::timeout(Duration::from_millis(500),async { for packet in packets { track.write_rtp(packet).await?; } Ok::<(),anyhow::Error>(()) }).await??;
                frames+=1;if frames==1 || frames.is_multiple_of(300) {eprintln!("media encoded access units sent: {frames}");}
            }
            #[allow(unreachable_code)] Ok::<(),anyhow::Error>(())
        };
        let feedback=async {
            while let Some(event)=track.poll().await {
                if let webrtc::media_stream::track_local::TrackLocalEvent::OnRtcpPacket(packets)=event {
                    if rtcp_forwarder::requests_keyframe(&packets,ssrc) {
                        tokio::time::timeout(Duration::from_millis(100),input.write_all(b"keyframe\n")).await??;
                        eprintln!("media receiver PLI/FIR requested native keyframe");
                    }
                }
            }
            Ok::<(),anyhow::Error>(())
        };
        let result=tokio::select!{result=streaming=>result,result=feedback=>result,result=auth::watch_authorization(app.auth.clone(),&authorization.id,true)=>result,_=flag(cancel.clone())=>Ok(()),_=flag(lost)=>Ok(()),_=tokio::time::sleep(Duration::from_secs(180))=>Ok(())};
        let _=tokio::time::timeout(Duration::from_millis(100),input.write_all(b"stop\n")).await; drop(input);
        match tokio::time::timeout(Duration::from_secs(2),child.wait()).await {
            Ok(Ok(status)) if cleanup_attested_exit(status.code(), app.cleanup_failure_exit)=>cleanup_confirmed.store(true,Ordering::Release),
            _=>{let _=child.kill().await;let _=child.wait().await;}
        }
        result
    }.await;
    if !matches!(
        tokio::time::timeout(Duration::from_secs(2), peer.close()).await,
        Ok(Ok(()))
    ) {
        cleanup_confirmed.store(false, Ordering::Release);
    }
    result
}
fn cleanup_attested_exit(code: Option<i32>, failure_exit: Option<i32>) -> bool {
    code == Some(0) || (code.is_some() && code == failure_exit)
}

// RFC6184 Table5 subprofile matching; never accept a library's MIME-only fallback.
fn native_codec_matches(codec: &str, mime: &str, fmtp: &str) -> bool {
    let mut fields = BTreeMap::new();
    for field in fmtp.split(';').map(str::trim).filter(|v| !v.is_empty()) {
        let Some((k, v)) = field.split_once('=') else {
            return false;
        };
        let key = k.trim().to_ascii_lowercase();
        if key.is_empty() || v.trim().is_empty() || fields.insert(key, v.trim()).is_some() {
            return false;
        }
    }
    if codec == "h265" {
        return mime.eq_ignore_ascii_case("video/H265")
            && fields.get("profile-space").copied().unwrap_or("0") == "0"
            && fields.get("profile-id").copied().unwrap_or("1") == "1"
            && fields.get("tier-flag").copied().unwrap_or("0") == "0"
            && fields.get("tx-mode").copied().unwrap_or("SRST") == "SRST"
            && fields
                .get("level-id")
                .filter(|v| v.len() <= 3 && v.bytes().all(|c| c.is_ascii_digit()))
                .and_then(|v| v.parse::<u16>().ok())
                .is_some_and(|v| [120, 123, 150, 153, 156, 180, 183, 186].contains(&v));
    }
    if !mime.eq_ignore_ascii_case("video/H264") || fields.get("packetization-mode") != Some(&"1") {
        return false;
    }
    let Some(profile) = fields.get("profile-level-id") else {
        return false;
    };
    if profile.len() != 6 {
        return false;
    }
    let Ok(n) = u32::from_str_radix(profile, 16) else {
        return false;
    };
    let id = (n >> 16) as u8;
    let flags = (n >> 8) as u8;
    let level = n as u8;
    let cb = match id {
        0x42 => flags & 0x4f == 0x40,
        0x4d => flags & 0x8f == 0x80,
        0x58 => flags & 0xcf == 0xc0,
        _ => false,
    };
    cb && [31, 32, 40, 41, 42, 50, 51, 52, 60, 61, 62].contains(&level)
}
fn frame_header(header: [u8; 13], previous: Option<u64>) -> Result<(usize, u64, bool)> {
    let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
    let pts = u64::from_be_bytes(header[4..12].try_into().unwrap());
    if previous.is_none() && pts != 0 {
        bail!("native timeline must start at zero")
    }
    if length == 0
        || length > 4 * 1024 * 1024
        || header[12] > 1
        || previous.is_some_and(|last| pts <= last || pts - last > 10_000_000)
    {
        bail!("invalid or discontinuous native frame header")
    }
    Ok((length, pts, header[12] == 1))
}
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|v| v == "--pair") {
        return vm_session_auth::cli::run_cli(&args[2..]);
    }
    if args.len() < 5 {
        bail!("usage: vm-media-local ENCODER SOCKET BROWSER_DIR PORT [--guest-stream] [--auth-state DIR --auth-namespace NAME --vm ID] [--fault-drop-first-keyframe] [--fault-suppress-hevc-frames]")
    }
    let authentication = auth::from_args(&args[5..])?;
    let broker_mode = args[1] == "--console";
    let encoder = if broker_mode {
        std::env::current_exe()?
            .with_file_name("vm-console-stream")
            .canonicalize()?
    } else {
        PathBuf::from(&args[1]).canonicalize()?
    };
    let socket = PathBuf::from(&args[2]).canonicalize()?;
    let web = PathBuf::from(&args[3]).canonicalize()?;
    let mut probe = tokio::process::Command::new(&encoder);
    probe.arg("--probe-codecs").kill_on_drop(true);
    if broker_mode {
        probe.arg(&socket);
    }
    let output = tokio::time::timeout(Duration::from_secs(8), probe.output()).await??;
    if !output.status.success() || output.stdout.len() > 8192 {
        bail!("native hardware capability probe failed")
    }
    let capabilities: Value = serde_json::from_slice(&output.stdout)?;
    let cleanup_failure_exit = if args[5..].iter().any(|a| a == "--guest-stream") {
        anyhow::ensure!(
            capabilities["protocol"] == "veynode-guest-video-stream-v1"
                && capabilities["cleanup_contract_version"] == 1
                && capabilities["cleanup_confirmed_failure_exit"] == 78,
            "Guest encoder cleanup contract missing or incompatible"
        );
        Some(78)
    } else {
        None
    };
    let listener =
        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, args[4].parse::<u16>()?))
            .await?;
    let host = listener.local_addr()?.to_string();
    let origin = format!("http://{host}");
    let app = App {
        host,
        origin: origin.clone(),
        encoder,
        socket,
        web,
        capabilities,
        fault_drop_first_keyframe: args[5..].iter().any(|a| a == "--fault-drop-first-keyframe"),
        fault_suppress_hevc_frames: args[5..]
            .iter()
            .any(|a| a == "--fault-suppress-hevc-frames"),
        active: Arc::new(Mutex::new(SessionBook::default())),
        auth: authentication,
        cleanup_failure_exit,
    };
    let shutdown_app = app.clone();
    let router = Router::new()
        .route("/capabilities", get(get_capabilities))
        .route("/auth/challenge", post(auth::challenge))
        .route("/auth/offer", post(auth::offer))
        .route("/auth/confirm", post(auth::confirm))
        .route("/auth/close", post(auth::close))
        .route("/offer", post(offer))
        .route("/close", post(close))
        .route(
            "/",
            get(|State(a), h| async {
                asset(a, h, "native.html", "text/html; charset=utf-8").await
            }),
        )
        .route(
            "/dist/native.js",
            get(|State(a), h| async { asset(a, h, "dist/native.js", "text/javascript").await }),
        )
        .route(
            "/dist/codec-policy.js",
            get(|State(a), h| async {
                asset(a, h, "dist/codec-policy.js", "text/javascript").await
            }),
        )
        .route(
            "/dist/native-auth.js",
            get(|State(a), h| async {
                asset(a, h, "dist/native-auth.js", "text/javascript").await
            }),
        )
        .route(
            "/dist/session-auth.js",
            get(|State(a), h| async {
                asset(a, h, "dist/session-auth.js", "text/javascript").await
            }),
        )
        .route(
            "/probe.css",
            get(|State(a), h| async { asset(a, h, "probe.css", "text/css").await }),
        )
        .layer(DefaultBodyLimit::max(256 * 1024))
        .with_state(app);
    println!(
        "{}",
        json!({"url":origin,"scope":"localhost-native-VM-WebRTC-diagnostic","internet_authentication":false,"fault_drop_first_keyframe":args[5..].iter().any(|a| a == "--fault-drop-first-keyframe"),"fault_suppress_hevc_frames":args[5..].iter().any(|a| a == "--fault-suppress-hevc-frames")})
    );
    serve_with_shutdown(listener, router, &shutdown_app, tokio::signal::ctrl_c(), Duration::from_secs(5)).await?;
    Ok(())
}

// Mark stopping and capture the only session under the same lock as admission.
// Nothing can occupy the just-vacated slot after this transition.
async fn begin_shutdown(app: &App) -> Option<watch::Receiver<Option<bool>>> {
    let mut book = app.active.lock().await;
    book.stopping = true;
    book.active.as_ref().map(|session| {
        let _ = session.cancel.send(true);
        session.finished.clone()
    })
}
async fn await_shutdown_cleanup(finished: Option<watch::Receiver<Option<bool>>>, deadline: Duration) -> Result<()> {
    if let Some(finished) = finished {
        let clean = tokio::time::timeout(deadline, completion(finished)).await.context("shutdown cleanup deadline exceeded")?;
        anyhow::ensure!(clean, "shutdown cleanup unconfirmed");
    }
    Ok(())
}
async fn serve_with_shutdown<F>(listener: tokio::net::TcpListener, router: Router, app: &App, signal: F, deadline: Duration) -> Result<()>
where F: std::future::Future<Output = std::io::Result<()>> {
    use std::future::IntoFuture;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(async { let _ = stop_rx.await; }).into_future();
    tokio::pin!(server);
    let (early_result, signal_result) = tokio::select! {
        result = &mut server => (Some(result), Ok(())),
        result = signal => (None, result),
    };
    let finished = begin_shutdown(app).await;
    // Resolve Axum's shutdown future immediately. Never wait for Guest cleanup
    // while leaving the listening socket open to new requests.
    let _ = stop_tx.send(());
    let drain = async {
        if let Some(result) = early_result { result?; bail!("HTTP server ended before shutdown signal"); }
        tokio::time::timeout(deadline, &mut server).await.context("HTTP shutdown drain deadline exceeded")??;
        Ok::<(), anyhow::Error>(())
    };
    // Existing partial-body/keepalive handlers cannot hold process shutdown open.
    // Both paths get time to complete even if the other fails immediately.
    let (cleanup, drained) = tokio::join!(await_shutdown_cleanup(finished, deadline), drain);
    signal_result.context("shutdown signal failed")?;
    cleanup?;
    drained?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn header(n: u32, pts: u64, key: u8) -> [u8; 13] {
        let mut h = [0; 13];
        h[..4].copy_from_slice(&n.to_be_bytes());
        h[4..12].copy_from_slice(&pts.to_be_bytes());
        h[12] = key;
        h
    }
    #[tokio::test]
    async fn close_requires_cleanup_attestation_even_after_session_exit() {
        let request = uuid::Uuid::new_v4().to_string();
        let app = App {
            origin: "http://127.0.0.1:8787".into(),
            host: "127.0.0.1:8787".into(),
            encoder: PathBuf::new(),
            socket: PathBuf::new(),
            web: PathBuf::new(),
            capabilities: json!({}),
            fault_drop_first_keyframe: false,
            fault_suppress_hevc_frames: false,
            active: Arc::new(Mutex::new(SessionBook::default())),
            auth: None,
            cleanup_failure_exit: None,
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, app.host.parse().unwrap());
        headers.insert(header::ORIGIN, app.origin.parse().unwrap());
        let (cancel, _) = watch::channel(false);
        let (done, finished) = watch::channel(None);
        app.active.lock().await.active = Some(Session {
            request: request.clone(),
            id: "owned".into(),
            cancel,
            finished,
            confirmed: watch::channel(false).0,
        });
        // A task disappearing without a final value is not proof of cleanup.
        drop(done);
        let reply = close(
            State(app.clone()),
            headers.clone(),
            Json(Close {
                request_id: request.clone(),
                session_id: Some("owned".into()),
            }),
        )
        .await;
        assert_eq!(reply.status(), StatusCode::GATEWAY_TIMEOUT);
        {
            let mut book = app.active.lock().await;
            book.active = None;
            book.completed
                .insert(request.clone(), (Instant::now(), false, "owned".into()));
        }
        let reply = close(
            State(app.clone()),
            headers.clone(),
            Json(Close {
                request_id: request.clone(),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(reply.status(), StatusCode::GATEWAY_TIMEOUT);
        app.active
            .lock()
            .await
            .completed
            .insert(request.clone(), (Instant::now(), true, "owned".into()));
        let reply = close(
            State(app),
            headers,
            Json(Close {
                request_id: request,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(reply.status(), StatusCode::OK);
    }
    #[test]
    fn cleanup_failure_exit_is_opt_in_and_not_a_media_success() {
        assert!(cleanup_attested_exit(Some(0), None));
        assert!(cleanup_attested_exit(Some(78), Some(78)));
        for (code, contract) in [
            (None, None),
            (None, Some(78)),
            (Some(1), Some(78)),
            (Some(78), None),
        ] {
            assert!(!cleanup_attested_exit(code, contract));
        }
    }
    #[test]
    fn cancellation_before_offer_is_retained_and_bounded() {
        let mut book = SessionBook::default();
        assert!(book.cancel_id("cancelled-before-offer"));
        assert!(book.cancelled.contains_key("cancelled-before-offer"));
        for i in 0..255 {
            assert!(book.cancel_id(&i.to_string()));
        }
        assert!(!book.cancel_id("overflow"));
        assert!(book.cancel_id("cancelled-before-offer"));
        book.cancelled.insert(
            "cancelled-before-offer".into(),
            Instant::now() - Duration::from_secs(61),
        );
        book.prune();
        assert!(!book.cancelled.contains_key("cancelled-before-offer"));
    }
    #[test]
    fn negotiated_profile_requires_encoder_compatible_subprofile_and_level() {
        for id in ["42e01f", "42c01f", "4d801f", "58c01f"] {
            assert!(native_codec_matches(
                "h264",
                "video/H264",
                &format!("packetization-mode=1;profile-level-id={id}")
            ));
        }
        for id in ["4d001f", "640c1f", "42e01e", "42e0ff", "zzzzzz"] {
            assert!(!native_codec_matches(
                "h264",
                "video/H264",
                &format!("packetization-mode=1;profile-level-id={id}")
            ));
        }
        assert!(!native_codec_matches(
            "h264",
            "video/H264",
            "packetization-mode=0;profile-level-id=42e01f"
        ));
        assert!(!native_codec_matches(
            "h264",
            "video/H264",
            "packetization-mode=1;profile-level-id=42e01f;profile-level-id=640c1f"
        ));
        assert!(native_codec_matches(
            "h265",
            "video/H265",
            "profile-id=1;tier-flag=0;level-id=120;tx-mode=SRST"
        ));
        assert!(!native_codec_matches(
            "h265",
            "video/H265",
            "profile-id=2;tier-flag=0;level-id=120;tx-mode=SRST"
        ));
        assert!(native_codec_matches("h265", "video/H265", "level-id=120"));
        for fmtp in [
            "level-id=93",
            "level-id=121",
            "level-id=+120",
            "level-id=120;profile-space=1",
            "level-id=120;tier-flag=1",
            "level-id=120;tx-mode=MRST",
            "level-id=120;PROFILE-ID=1;profile-id=1",
            "level-id=120;broken",
        ] {
            assert!(!native_codec_matches("h265", "video/H265", fmtp));
        }
    }
    #[test]
    fn malformed_frame_is_rejected_before_allocation() {
        for h in [
            header(0, 0, 1),
            header(4 * 1024 * 1024 + 1, 0, 1),
            header(99, 0, 2),
            header(99, 1, 1),
        ] {
            assert!(frame_header(h, None).is_err());
        }
        assert_eq!(
            frame_header(header(123, 0, 1), None).unwrap(),
            (123, 0, true)
        );
        assert!(frame_header(header(123, 42, 0), Some(42)).is_err());
        assert!(frame_header(header(123, 41, 0), Some(42)).is_err());
        assert!(frame_header(header(123, 10_000_043, 0), Some(42)).is_err());
        assert_eq!(
            frame_header(header(123, 33_333, 0), Some(0)).unwrap().1,
            33_333
        );
    }
    #[test]
    fn loopback_mutations_require_exact_host_and_origin() {
        let app = App {
            origin: "http://127.0.0.1:1234".into(),
            host: "127.0.0.1:1234".into(),
            encoder: PathBuf::new(),
            socket: PathBuf::new(),
            web: PathBuf::new(),
            capabilities: Value::Null,
            fault_drop_first_keyframe: false,
            fault_suppress_hevc_frames: false,
            active: Arc::new(Mutex::new(SessionBook::default())),
            auth: None,
            cleanup_failure_exit: None,
        };
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "127.0.0.1:1234".parse().unwrap());
        assert!(check(&app, &h, false).is_ok());
        assert!(check(&app, &h, true).is_err());
        h.insert(header::ORIGIN, "https://attacker.example".parse().unwrap());
        assert!(check(&app, &h, true).is_err());
        h.insert(header::ORIGIN, app.origin.parse().unwrap());
        assert!(check(&app, &h, true).is_ok());
        h.insert(header::HOST, "attacker.example".parse().unwrap());
        assert!(check(&app, &h, false).is_err());
    }
    fn shutdown_fixture() -> App {
        App {
            origin: "http://127.0.0.1:1".into(), host: "127.0.0.1:1".into(),
            encoder: PathBuf::from("must-not-start"), socket: PathBuf::new(), web: PathBuf::new(),
            capabilities: json!({"h264":"verified-hardware"}), fault_drop_first_keyframe: false,
            fault_suppress_hevc_frames: false, active: Arc::new(Mutex::new(SessionBook::default())),
            auth: None, cleanup_failure_exit: None,
        }
    }
    #[tokio::test]
    async fn shutdown_requires_actual_cleanup_and_bounds_missing_completion() {
        let deadline = Duration::from_millis(20);
        await_shutdown_cleanup(None, deadline).await.unwrap();
        let (done, finished) = watch::channel(Some(true));
        await_shutdown_cleanup(Some(finished), deadline).await.unwrap(); drop(done);
        let (done, finished) = watch::channel(Some(false));
        assert!(await_shutdown_cleanup(Some(finished), deadline).await.unwrap_err().to_string().contains("unconfirmed")); drop(done);
        let (done, finished) = watch::channel(None); drop(done);
        assert!(await_shutdown_cleanup(Some(finished), deadline).await.unwrap_err().to_string().contains("unconfirmed"));
        let (_done, finished) = watch::channel(None);
        assert!(await_shutdown_cleanup(Some(finished), deadline).await.unwrap_err().to_string().contains("deadline"));
    }
    #[tokio::test]
    async fn shutdown_stops_real_listener_and_bounds_partial_body_handler() {
        let app = shutdown_fixture();
        let entered = Arc::new(tokio::sync::Notify::new());
        let notification = entered.clone();
        let router = Router::new().route("/slow", post(move |request: axum::http::Request<axum::body::Body>| {
            let notification = notification.clone();
            async move {
                notification.notify_one();
                let _ = axum::body::to_bytes(request.into_body(), 1024).await;
                StatusCode::OK
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, signal) = tokio::sync::oneshot::channel::<()>();
        let running_app = app.clone();
        let task = tokio::spawn(async move {
            serve_with_shutdown(listener, router, &running_app, async { let _ = signal.await; Ok(()) }, Duration::from_millis(50)).await
        });
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client.write_all(b"POST /slow HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\nx").await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), entered.notified()).await.unwrap();
        shutdown.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), task).await.unwrap().unwrap();
        assert!(result.unwrap_err().to_string().contains("HTTP shutdown drain deadline"));
        assert!(app.active.lock().await.stopping);
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
        drop(client);
    }
    #[tokio::test]
    async fn shutdown_http_returns_error_for_failed_guest_cleanup() {
        let app = shutdown_fixture();
        let (cancel, cancelled) = watch::channel(false);
        let (_done, finished) = watch::channel(Some(false));
        app.active.lock().await.active = Some(Session {
            request: uuid::Uuid::new_v4().to_string(), id: uuid::Uuid::new_v4().to_string(),
            cancel, finished, confirmed: watch::channel(false).0,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let result = serve_with_shutdown(listener, Router::new(), &app, async { Ok(()) }, Duration::from_millis(50)).await;
        assert!(result.unwrap_err().to_string().contains("cleanup unconfirmed"));
        assert!(*cancelled.borrow());
    }

    #[tokio::test]
    async fn shutdown_http_clean_idle_service_exits_successfully() {
        let app = shutdown_fixture();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        serve_with_shutdown(listener, Router::new(), &app, async { Ok(()) }, Duration::from_millis(100)).await.unwrap();
        assert!(app.active.lock().await.stopping);
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }

}
