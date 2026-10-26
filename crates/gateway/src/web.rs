//! HTTPS side: capabilities, the WebSocket entry, the static UI, and the
//! plain-HTTP listener that answers ACME challenges and redirects the rest.
use crate::certs::{pin, CertStore};
use crate::state::State;
use crate::wss;
use anyhow::Result;
use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, Path, State as Extract, WebSocketUpgrade};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use vm_remote_proto::config::{Tls, VideoMode};

const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const PENDING_HANDSHAKES: usize = 256;

/// The connecting address as the listener saw it.
#[derive(Clone, Copy, Debug)]
pub struct Peer(pub SocketAddr);

impl Connected<axum::serve::IncomingStream<'_, TcpListener>> for Peer {
    fn connect_info(stream: axum::serve::IncomingStream<'_, TcpListener>) -> Self {
        Peer(*stream.remote_addr())
    }
}

impl Connected<axum::serve::IncomingStream<'_, TlsListener>> for Peer {
    fn connect_info(stream: axum::serve::IncomingStream<'_, TlsListener>) -> Self {
        Peer(*stream.remote_addr())
    }
}

/// TCP plus TLS, with handshakes run concurrently off the accept path so
/// one slow client never holds up the others.
pub struct TlsListener {
    ready: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
}

impl TlsListener {
    pub fn new(tcp: TcpListener, certs: Arc<CertStore>) -> Result<Self> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_cert_resolver(certs);
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(tls));
        let (tx, ready) = mpsc::channel(PENDING_HANDSHAKES);
        tokio::spawn(async move {
            loop {
                let (stream, addr) = match tcp.accept().await {
                    Ok(pair) => pair,
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let (acceptor, tx) = (acceptor.clone(), tx.clone());
                tokio::spawn(async move {
                    let _ = stream.set_nodelay(true);
                    if let Ok(Ok(tls)) =
                        tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await
                    {
                        let _ = tx.send((tls, addr)).await;
                    }
                });
            }
        });
        Ok(Self { ready })
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(pair) => pair,
            // The accept task never ends; park if it somehow did.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Err(std::io::Error::other("TLS listener"))
    }
}

pub fn router(state: Arc<State>) -> Router {
    let mut app = Router::new()
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/desktop/ws", get(ws_any))
        .route("/v1/desktop/ws/{host}", get(ws_host));
    if let Some(ui) = state.config.listen.as_ref().and_then(|l| l.ui.clone()) {
        app = app.fallback_service(tower_http::services::ServeDir::new(ui));
    }
    app.layer(axum::middleware::map_response(security_headers))
        .with_state(state)
}

async fn security_headers(mut response: Response) -> Response {
    let h = response.headers_mut();
    h.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=31536000"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

/// The client address. Behind `tls.mode = "external"` the gateway only
/// listens on loopback (checked at load), so the tunnel daemon's
/// `CF-Connecting-IP` is the one trustworthy source.
fn client_ip(state: &State, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if matches!(state.config.tls, Some(Tls::External)) && peer.ip().is_loopback() {
        if let Some(ip) = headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok())
        {
            return ip;
        }
    }
    peer.ip()
}

async fn ws_any(
    Extract(state): Extract<Arc<State>>,
    ConnectInfo(Peer(peer)): ConnectInfo<Peer>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let ip = client_ip(&state, peer, &headers);
    wss::upgrade(state, None, ip, headers, ws).await
}

async fn ws_host(
    Extract(state): Extract<Arc<State>>,
    ConnectInfo(Peer(peer)): ConnectInfo<Peer>,
    Path(host): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if crate::state::host_from_path(&format!("/h/{host}"), "/h") != Some(Some(host.as_str())) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let ip = client_ip(&state, peer, &headers);
    wss::upgrade(state, Some(host), ip, headers, ws).await
}

/// What a browser may try, in order. No host names: who is online is not
/// shown to someone who has not paired.
pub fn capabilities_body(state: &State) -> serde_json::Value {
    let mut transports = Vec::new();
    if state.webtransport {
        transports.push("wt");
    }
    transports.push("wss");
    // "webrtc" joins once the host mints remote TURN credentials for its
    // WebRTC path (docs/remote-desktop.md, "Not yet").
    let mut body = serde_json::json!({
        "protocol": vm_remote_proto::PROTOCOL,
        "transports": transports,
        "video": match state.config.video.mode {
            VideoMode::Stream => "stream",
            VideoMode::Datagram => "datagram",
        },
        "hosts_online": state.hosts.names().len(),
    });
    if state.webtransport && state.self_signed {
        // WebTransport's serverCertificateHashes: the leaf, not the key.
        if let Some(store) = &state.public {
            body["server_certificate_sha256"] = pin(&store.leaf_sha256()).into();
        }
    }
    body
}

async fn capabilities(Extract(state): Extract<Arc<State>>) -> Response {
    let mut r = Json(capabilities_body(&state)).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// Port 80: ACME HTTP-01 answers, a redirect to HTTPS for everything else.
pub fn http_router(state: Arc<State>) -> Router {
    Router::new()
        .route("/.well-known/acme-challenge/{token}", get(acme_answer))
        .fallback(redirect)
        .with_state(state)
}

async fn acme_answer(Extract(state): Extract<Arc<State>>, Path(token): Path<String>) -> Response {
    match state.acme.lock().unwrap().get(&token) {
        Some(answer) => answer.clone().into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn redirect(Extract(state): Extract<Arc<State>>, uri: Uri) -> Response {
    let host = match &state.config.tls {
        Some(Tls::Acme { domains, .. }) => domains.first().cloned(),
        _ => None,
    };
    match host {
        Some(h) => {
            let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
            Redirect::permanent(&format!("https://{h}{path}")).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
