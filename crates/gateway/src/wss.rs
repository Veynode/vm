//! WebSocket listener: one binary message per browser message.
//!
//! Everything the host sends rides one ordered socket, so the writer always
//! drains reliable messages first and keeps at most [`VIDEO_QUEUE`] video
//! units behind them. A unit that does not fit is refused at the tunnel
//! (the host sees its stream stopped), which keeps the delay at the host
//! pacer instead of in socket buffers.
use crate::routes::{browser_message_ok, read_frame, Downlink, Route};
use crate::state::{Residence, State};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use vm_remote_proto::record::close;
use vm_remote_proto::tunnel::{self, MAX_MESSAGE};

pub const VIDEO_QUEUE: usize = 2;
/// A video unit not read off the tunnel within this is dropped.
pub const VIDEO_READ_LIMIT: Duration = Duration::from_millis(300);
const RELIABLE_QUEUE: usize = 64;

pub async fn upgrade(
    state: Arc<State>,
    host: Option<String>,
    ip: IpAddr,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !state.admit(ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if !state.origin_allowed(origin.as_deref()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let route = match state
        .routes
        .open(
            &state.hosts,
            host.as_deref(),
            "wss",
            origin.as_deref().unwrap_or_default(),
            &ip.to_string(),
            state.sessions_max(),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("vm-gateway: wss from {ip}: {e:#}");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .write_buffer_size(64 * 1024)
        .on_upgrade(move |socket| async move {
            if let Err(e) = relay(socket, route).await {
                eprintln!("vm-gateway: wss from {ip}: {e:#}");
            }
        })
}

async fn relay(socket: WebSocket, route: Route) -> anyhow::Result<()> {
    let Route {
        id: _,
        host: _,
        send: mut tsend,
        recv: mut trecv,
        mut downlink,
        guard: _guard,
    } = route;
    let (mut sink, mut stream) = socket.split();
    let (reliable_tx, mut reliable_rx) = mpsc::channel::<Bytes>(RELIABLE_QUEUE);
    let (video_tx, mut video_rx) = mpsc::channel::<(Bytes, Instant)>(VIDEO_QUEUE);
    let writer = tokio::spawn(async move {
        let mut residence = Residence::default();
        loop {
            let (bytes, video_since) = tokio::select! {
                biased;
                m = reliable_rx.recv() => match m { Some(m) => (m, None), None => break },
                v = video_rx.recv() => match v { Some((v, t)) => (v, Some(t)), None => break },
            };
            if sink.send(Message::Binary(bytes)).await.is_err() {
                return;
            }
            if let Some(t) = video_since {
                residence.push(t.elapsed());
                if let Some(n) = residence.due(Instant::now()) {
                    if sink.send(Message::Binary(n.into())).await.is_err() {
                        return;
                    }
                }
            }
        }
        let _ = sink
            .send(Message::Close(Some(CloseFrame {
                code: close::RETRY,
                reason: "host closed".into(),
            })))
            .await;
    });
    let up = async {
        while let Some(message) = stream.next().await {
            match message? {
                Message::Binary(bytes) => {
                    anyhow::ensure!(browser_message_ok(&bytes), "invalid browser message");
                    tsend.write_all(&tunnel::frame(&bytes)?).await?;
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = tsend.finish();
        anyhow::Ok(())
    };
    let down = async {
        loop {
            tokio::select! {
                frame = read_frame(&mut trecv) => {
                    let Some(message) = frame? else { return anyhow::Ok(()) };
                    if reliable_tx.send(message.into()).await.is_err() {
                        return Ok(());
                    }
                }
                item = downlink.recv() => match item {
                    None => return Ok(()),
                    // Datagram-class messages (pointer echoes, pings) ride
                    // the reliable queue: the socket is ordered anyway.
                    Some(Downlink::Datagram(bytes)) => {
                        let _ = reliable_tx.try_send(bytes);
                    }
                    Some(Downlink::Video(mut recv)) => {
                        if video_tx.capacity() == 0 {
                            let _ = recv.stop(1u32.into());
                            continue;
                        }
                        let video_tx = video_tx.clone();
                        tokio::spawn(async move {
                            let started = Instant::now();
                            let mut unit = Vec::new();
                            let read = tokio::time::timeout(
                                VIDEO_READ_LIMIT,
                                (&mut recv).take(MAX_MESSAGE as u64 + 1).read_to_end(&mut unit),
                            )
                            .await;
                            if !matches!(read, Ok(Ok(_))) || unit.len() > MAX_MESSAGE {
                                let _ = recv.stop(1u32.into());
                                return;
                            }
                            // Full after the read: the browser detects the
                            // gap by frame number and asks for a keyframe.
                            let _ = video_tx.try_send((unit.into(), started));
                        });
                    }
                },
            }
        }
    };
    let result = tokio::select! {
        r = up => r,
        r = down => r,
    };
    drop(reliable_tx);
    drop(video_tx);
    let _ = writer.await;
    result
}
