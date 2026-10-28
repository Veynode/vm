//! `vm-media-local remote`: the Mac end of remote desktop.
//!
//! The host dials out to `vm-gateway` over a pinned QUIC tunnel (no inbound
//! port), runs the session-auth handshake with each browser the gateway
//! routes to it, and serves end-to-end encrypted desktop sessions from one
//! shared encoder per VM. It runs in the process that owns the Keychain host
//! key; see docs/remote-desktop.md.
pub mod handshake;
pub mod hub;
pub mod session;
pub mod tunnel;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use handshake::HostIdentity;
use hub::{Hub, Source};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use vm_remote_proto::config::Config;
use vm_remote_proto::ratelimit::{Keyed, Rate};
use vm_remote_proto::record::close;
use vm_remote_proto::tunnel::{RouteOpen, MAX_MESSAGE};
use vm_session_auth::store::{Registry, Store};
use vm_session_auth::SessionGate;

const USAGE: &str = "usage: vm-media-local remote --config GATEWAY_TOML --auth-state DIR --auth-namespace NAME [--state-dir DIR]";
/// How often the pairing registry is re-read for revocations.
const RELOAD: Duration = Duration::from_millis(250);
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Datagrams queued per route before newer ones are dropped.
const ROUTE_DATAGRAMS: usize = 64;

pub struct Remote {
    pub gate: SessionGate,
    pub identity: Arc<dyn HostIdentity>,
    pub config: Config,
    source: Arc<dyn Source>,
    hubs: Mutex<HashMap<(String, String), Arc<Hub>>>,
    sessions: Mutex<HashMap<String, oneshot::Sender<u16>>>,
    handshakes: Mutex<Keyed<String>>,
}

impl Remote {
    pub fn new(
        gate: SessionGate,
        identity: Arc<dyn HostIdentity>,
        config: Config,
        source: Arc<dyn Source>,
    ) -> Arc<Self> {
        let per_minute = config.limits.handshakes_per_device_per_min;
        Arc::new(Self {
            gate,
            identity,
            config,
            source,
            hubs: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            handshakes: Mutex::new(Keyed::new(Rate::per_minute(per_minute), 4096)),
        })
    }

    pub fn handshake_allowed(&self, device_id: &str) -> bool {
        self.handshakes
            .lock()
            .unwrap()
            .allow(&device_id.to_string(), Instant::now())
    }

    /// The shared encoder for a VM and codec, started on first use.
    pub fn hub(&self, vm: &str, codec: &str) -> Result<Arc<Hub>> {
        let mut hubs = self.hubs.lock().unwrap();
        let key = (vm.to_string(), codec.to_string());
        if let Some(hub) = hubs.get(&key).filter(|h| h.ended().is_none()) {
            anyhow::ensure!(
                hub.viewers() < self.config.limits.viewers_per_vm as usize,
                "viewer limit for this VM reached"
            );
            return Ok(hub.clone());
        }
        let hub = Hub::start(
            self.source.as_ref(),
            vm,
            codec,
            self.config.limits.tracks_per_vm as usize,
        )?;
        hubs.insert(key, hub.clone());
        Ok(hub)
    }

    pub fn register(&self, session_id: &str, closer: oneshot::Sender<u16>) {
        self.sessions
            .lock()
            .unwrap()
            .insert(session_id.into(), closer);
    }

    pub fn unregister(&self, session_id: &str) {
        self.sessions.lock().unwrap().remove(session_id);
    }

    /// Ends a session with a close code (revoked, expired, replaced).
    pub fn close(&self, session_id: &str, code: u16) {
        if let Some(closer) = self.sessions.lock().unwrap().remove(session_id) {
            let _ = closer.send(code);
        }
    }

    /// Stops encoders nobody has watched for [`hub::LINGER`].
    pub fn sweep(&self, now: Instant) {
        self.hubs
            .lock()
            .unwrap()
            .retain(|_, hub| hub.viewers() > 0 || !hub.idle(now));
    }
}

#[cfg(target_os = "macos")]
struct KeychainHost {
    namespace: String,
}

#[cfg(target_os = "macos")]
impl HostIdentity for KeychainHost {
    fn answer(
        &self,
        gate: &SessionGate,
        accepted: vm_session_auth::remote::AcceptedRemoteOffer,
        params: vm_session_auth::remote::RemoteAnswerParams,
        now: u64,
    ) -> Result<vm_session_auth::remote::SignedRemoteAnswer> {
        // Opened per answer, as the WebRTC path does: the Keychain item
        // never stays open across awaits.
        let identity = vm_session_auth::keychain::KeychainIdentity::open(&self.namespace)?;
        gate.remote_answer(accepted, params, &identity, now)
    }
}

fn load_registry(state: &std::path::Path) -> Result<Registry> {
    let store = Store::open(state)?;
    let registry = store.load();
    drop(store);
    registry
}

struct Args {
    config: PathBuf,
    state: PathBuf,
    namespace: String,
    /// The daemon's state directory, for a home other than `VEYNODEVM_HOME`'s.
    state_dir: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<Args> {
    let (mut config, mut state, mut namespace, mut state_dir) = (None, None, None, None);
    let mut i = 0;
    while i < args.len() {
        let value = args.get(i + 1).context(USAGE)?.clone();
        let slot = match args[i].as_str() {
            "--config" => &mut config,
            "--auth-state" => &mut state,
            "--auth-namespace" => &mut namespace,
            "--state-dir" => &mut state_dir,
            other => bail!("unknown option {other}\n{USAGE}"),
        };
        anyhow::ensure!(slot.is_none(), "duplicate option {}", args[i]);
        *slot = Some(value);
        i += 2;
    }
    Ok(Args {
        config: config.context(USAGE)?.into(),
        state: state.context(USAGE)?.into(),
        namespace: namespace.context(USAGE)?,
        state_dir: state_dir.map(PathBuf::from),
    })
}

pub async fn run(args: &[String]) -> Result<()> {
    let args = parse_args(args)?;
    let config = Config::load(&args.config)?;
    let host = config
        .host
        .clone()
        .context("the config has no [host] section")?;
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let tunnel_key = vm_remote_proto::keys::persistent_key(&host.key)?;
    eprintln!(
        "vm-media-local remote: tunnel key {} (add to the gateway's tunnel.hosts as {:?})",
        vm_remote_proto::keys::key_pin(&tunnel_key),
        host.name
    );
    let registry = load_registry(&args.state)?;
    let identity = host_identity(&args.namespace, &registry)?;
    let gate = SessionGate::with_versions(
        &registry.host_public_key,
        &registry.grants.values().cloned().collect::<Vec<_>>(),
        &registry.versions,
    )?;
    let name = concat!("vm-media-local/", env!("CARGO_PKG_VERSION"));
    let client = match args.state_dir {
        Some(dir) => veynodevm_api::Client::at_socket(dir.join("control.sock"), name),
        None => veynodevm_api::Client::from_env(name)?,
    };
    let remote = Remote::new(
        gate,
        identity,
        config,
        Arc::new(hub::DesktopStream { client }),
    );
    tokio::spawn(reload_loop(
        remote.clone(),
        args.state.clone(),
        registry.host_public_key.clone(),
    ));

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(tunnel::client_config(&host, &tunnel_key)?);
    let mut backoff = RECONNECT_MIN;
    loop {
        match tunnel::connect(&endpoint, &host).await {
            Ok(conn) => {
                eprintln!("vm-media-local remote: connected to {}", host.connect);
                backoff = RECONNECT_MIN;
                let reason = serve_connection(conn, remote.clone()).await;
                eprintln!("vm-media-local remote: tunnel closed: {reason}");
            }
            Err(e) => eprintln!("vm-media-local remote: connect {}: {e:#}", host.connect),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

#[cfg(target_os = "macos")]
fn host_identity(namespace: &str, registry: &Registry) -> Result<Arc<dyn HostIdentity>> {
    use vm_session_auth::Identity;
    let public = vm_session_auth::keychain::KeychainIdentity::open(namespace)?.public_key()?;
    anyhow::ensure!(
        public == registry.host_public_key,
        "the Keychain host key does not match the pairing registry"
    );
    Ok(Arc::new(KeychainHost {
        namespace: namespace.into(),
    }))
}

#[cfg(not(target_os = "macos"))]
fn host_identity(_: &str, _: &Registry) -> Result<Arc<dyn HostIdentity>> {
    bail!("the host key lives in the macOS Keychain")
}

/// Re-reads grants: revoked or narrowed devices lose their sessions within
/// one period, and expired sessions end.
async fn reload_loop(remote: Arc<Remote>, state: PathBuf, host_public: String) {
    let mut tick = tokio::time::interval(RELOAD);
    loop {
        tick.tick().await;
        let now = session::unix_now();
        for id in remote.gate.expired_remote(now) {
            remote.close(&id, close::UNAUTHORIZED);
        }
        remote.sweep(Instant::now());
        let registry = match load_registry(&state) {
            Ok(r) => r,
            // A pairing admin holds the lock; try next period.
            Err(_) => continue,
        };
        if registry.host_public_key != host_public {
            eprintln!("vm-media-local remote: host identity changed; closing all sessions");
            let ids: Vec<String> = remote.sessions.lock().unwrap().keys().cloned().collect();
            for id in ids {
                remote.close(&id, close::REVOKED);
            }
            continue;
        }
        match remote.gate.replace_grants(
            &host_public,
            &registry.grants.into_values().collect::<Vec<_>>(),
        ) {
            Ok(revoked) => {
                for id in revoked {
                    remote.close(&id, close::REVOKED);
                }
            }
            Err(e) => eprintln!("vm-media-local remote: reload grants: {e:#}"),
        }
    }
}

/// Routes from one tunnel connection until it ends.
pub async fn serve_connection(conn: quinn::Connection, remote: Arc<Remote>) -> String {
    let routes: Arc<Mutex<HashMap<u64, mpsc::Sender<Bytes>>>> = Default::default();
    let datagram_routes = routes.clone();
    let datagram_conn = conn.clone();
    let datagrams = tokio::spawn(async move {
        while let Ok(bytes) = datagram_conn.read_datagram().await {
            let Ok((route_id, message)) = vm_remote_proto::tunnel::unroute(&bytes) else {
                continue;
            };
            let at = bytes.len() - message.len();
            if let Some(tx) = datagram_routes.lock().unwrap().get(&route_id) {
                let _ = tx.try_send(bytes.slice(at..));
            }
        }
    });
    let reason = loop {
        let (send, mut recv) = match conn.accept_bi().await {
            Ok(pair) => pair,
            Err(e) => break e.to_string(),
        };
        let (remote, conn, routes) = (remote.clone(), conn.clone(), routes.clone());
        tokio::spawn(async move {
            let open = match session::read_frame(&mut recv).await {
                Ok(Some(m)) if m.len() <= MAX_MESSAGE => serde_json::from_slice::<RouteOpen>(&m),
                _ => return,
            };
            let Ok(open) = open else { return };
            let route_id = open.route_id;
            let (tx, datagrams) = mpsc::channel(ROUTE_DATAGRAMS);
            routes.lock().unwrap().insert(route_id, tx);
            let route = session::Route {
                conn,
                open,
                send,
                recv,
                datagrams,
            };
            if let Err(e) = session::serve(route, remote).await {
                eprintln!("vm-media-local remote: route {route_id}: {e:#}");
            }
            routes.lock().unwrap().remove(&route_id);
        });
    };
    datagrams.abort();
    reason
}

#[cfg(test)]
#[path = "remote_tests.rs"]
mod tests;
