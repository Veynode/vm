//! WebTransport listener (HTTP/3 over QUIC).
//!
//! The browser opens one bidirectional stream for reliable messages, then
//! sends pointer moves, pings and (in datagram mode) nothing else as
//! datagrams. Video comes from the host as one tunnel stream per access
//! unit and is copied into one browser stream per access unit as bytes
//! arrive, so the gateway adds no store-and-forward delay; a host reset is
//! passed on as a browser reset.
use crate::certs::CertStore;
use crate::routes::{browser_message_ok, read_frame, Downlink, Route};
use crate::state::{host_from_path, Residence, State};
use anyhow::{bail, Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use vm_remote_proto::tunnel::{self, MAX_MESSAGE};
use wtransport::endpoint::IncomingSession;
use wtransport::{Connection, Endpoint, ServerConfig};

/// A video unit the browser has not taken within this is dropped (the host
/// resets it sooner, at its own deadline).
pub const VIDEO_COPY_LIMIT: Duration = Duration::from_millis(300);
/// Video units copied at once per session.
pub const VIDEO_IN_FLIGHT: usize = 3;
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

pub type Server = Endpoint<wtransport::endpoint::endpoint_side::Server>;

pub fn endpoint(addr: SocketAddr, certs: Arc<CertStore>) -> Result<Server> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_cert_resolver(certs);
    tls.alpn_protocols = vec![wtransport::tls::WEBTRANSPORT_ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let config = ServerConfig::builder()
        .with_bind_address(addr)
        .with_custom_tls(tls)
        .keep_alive_interval(Some(Duration::from_secs(2)))
        .max_idle_timeout(Some(Duration::from_secs(10)))?
        .build();
    Ok(Endpoint::server(config)?)
}

pub async fn serve(endpoint: Server, state: Arc<State>) {
    loop {
        let incoming = endpoint.accept().await;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = session(incoming, state).await {
                eprintln!("vm-gateway: webtransport: {e:#}");
            }
        });
    }
}

async fn session(incoming: IncomingSession, state: Arc<State>) -> Result<()> {
    let request = incoming.await?;
    let remote = request.remote_address();
    if !state.admit(remote.ip()) {
        request.too_many_requests().await;
        bail!("{remote}: connection rate exceeded");
    }
    let origin = request.origin().map(str::to_string);
    if !state.origin_allowed(origin.as_deref()) {
        request.forbidden().await;
        bail!("{remote}: origin {origin:?} not allowed");
    }
    let path = request.path().to_string();
    let Some(host) = host_from_path(&path, "/v1/desktop").map(|h| h.map(str::to_string)) else {
        request.not_found().await;
        bail!("{remote}: path {path}");
    };
    let route = match state
        .routes
        .open(
            &state.hosts,
            host.as_deref(),
            "wt",
            origin.as_deref().unwrap_or_default(),
            &remote.to_string(),
            state.sessions_max(),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            request.not_found().await;
            return Err(e.context("open route"));
        }
    };
    let conn = request.accept().await?;
    let (send, recv) = tokio::time::timeout(OPEN_TIMEOUT, conn.accept_bi())
        .await
        .context("browser opened no stream")??;
    relay(conn, send, recv, route).await
}

async fn relay(
    conn: Connection,
    mut bsend: wtransport::SendStream,
    mut brecv: wtransport::RecvStream,
    route: Route,
) -> Result<()> {
    let Route {
        id: route_id,
        host,
        send: mut tsend,
        recv: mut trecv,
        mut downlink,
        guard: _guard,
    } = route;
    let video = Arc::new(Semaphore::new(VIDEO_IN_FLIGHT));
    let mut residence = Residence::default();
    let (residence_tx, mut residence_rx) = tokio::sync::mpsc::unbounded_channel::<Duration>();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    // Browser to host reliable messages run in their own task so a slow
    // read on one side never blocks the other.
    let mut up = tokio::spawn(async move {
        while let Some(message) = read_frame(&mut brecv).await? {
            anyhow::ensure!(browser_message_ok(&message), "invalid browser message");
            tsend.write_all(&tunnel::frame(&message)?).await?;
        }
        let _ = tsend.finish();
        anyhow::Ok(())
    });
    let result: Result<()> = async {
        loop {
            tokio::select! {
                // The browser closed its stream or sent something invalid.
                done = &mut up => return done?,
                frame = read_frame(&mut trecv) => {
                    let Some(message) = frame? else { return Ok(()) };
                    bsend.write_all(&tunnel::frame(&message)?).await?;
                }
                dgram = conn.receive_datagram() => {
                    let dgram = dgram?;
                    if browser_message_ok(&dgram) {
                        let _ = host.send_datagram(tunnel::routed(route_id, &dgram).into());
                    }
                }
                item = downlink.recv() => match item {
                    None => return Ok(()),
                    Some(Downlink::Datagram(bytes)) => {
                        let _ = conn.send_datagram(bytes);
                    }
                    Some(Downlink::Video(recv)) => {
                        let Ok(permit) = video.clone().try_acquire_owned() else {
                            let mut recv = recv;
                            let _ = recv.stop(1u32.into());
                            continue;
                        };
                        let (conn, tx) = (conn.clone(), residence_tx.clone());
                        tokio::spawn(async move {
                            let started = Instant::now();
                            if copy_video(recv, &conn).await.is_ok() {
                                let _ = tx.send(started.elapsed());
                            }
                            drop(permit);
                        });
                    }
                },
                Some(d) = residence_rx.recv() => residence.push(d),
                _ = tick.tick() => {
                    if let Some(n) = residence.due(Instant::now()) {
                        bsend.write_all(&tunnel::frame(&n)?).await?;
                    }
                }
            }
        }
    }
    .await;
    up.abort();
    conn.close(0u32.into(), b"");
    result
}

async fn copy_video(mut from: quinn::RecvStream, conn: &Connection) -> Result<()> {
    let mut to = match conn.open_uni().await {
        Ok(opening) => opening.await?,
        Err(e) => {
            let _ = from.stop(1u32.into());
            return Err(e.into());
        }
    };
    let copied = tokio::time::timeout(VIDEO_COPY_LIMIT, async {
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0usize;
        while let Some(n) = from.read(&mut buf).await? {
            total += n;
            anyhow::ensure!(total <= MAX_MESSAGE, "oversized video unit");
            to.write_all(&buf[..n]).await?;
        }
        to.finish().await?;
        anyhow::Ok(())
    })
    .await;
    match copied {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            let _ = to.reset(1u32.into());
            let _ = from.stop(1u32.into());
            Err(e)
        }
        Err(_) => {
            let _ = to.reset(1u32.into());
            let _ = from.stop(1u32.into());
            bail!("video unit past the gateway limit")
        }
    }
}
