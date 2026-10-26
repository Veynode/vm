//! What every listener of one gateway process shares.
use crate::certs::CertStore;
use crate::routes::Routes;
use crate::tunnel::Hosts;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vm_remote_proto::config::Config;
use vm_remote_proto::hops::Window;
use vm_remote_proto::ratelimit::{Keyed, Rate};

pub struct State {
    pub config: Config,
    pub public: Option<Arc<CertStore>>,
    pub hosts: Arc<Hosts>,
    pub routes: Arc<Routes>,
    pub webtransport: bool,
    pub self_signed: bool,
    connections: Mutex<Keyed<IpAddr>>,
    /// ACME HTTP-01 answers by token.
    pub acme: Arc<Mutex<HashMap<String, String>>>,
}

impl State {
    pub fn new(
        config: Config,
        public: Option<Arc<CertStore>>,
        hosts: Arc<Hosts>,
        routes: Arc<Routes>,
    ) -> Arc<Self> {
        let webtransport = config.listen.as_ref().is_some_and(|l| l.udp.is_some());
        let self_signed = matches!(
            config.tls,
            Some(vm_remote_proto::config::Tls::SelfSigned { .. })
        );
        let per_minute = config.limits.conn_per_ip_per_min;
        Arc::new(Self {
            config,
            public,
            hosts,
            routes,
            webtransport,
            self_signed,
            connections: Mutex::new(Keyed::new(Rate::per_minute(per_minute), 10_000)),
            acme: Default::default(),
        })
    }

    pub fn origin_allowed(&self, origin: Option<&str>) -> bool {
        let Some(origin) = origin else { return false };
        self.config
            .listen
            .as_ref()
            .is_some_and(|l| l.allowed_origins.iter().any(|o| o == origin))
    }

    pub fn admit(&self, ip: IpAddr) -> bool {
        self.connections.lock().unwrap().allow(&ip, Instant::now())
    }

    pub fn sessions_max(&self) -> usize {
        self.config.limits.sessions_max as usize
    }
}

/// `/v1/desktop`, `/v1/desktop/<host>` (WebTransport) and the same under
/// `/ws` (WebSocket): the host name, if any.
pub fn host_from_path<'a>(path: &'a str, prefix: &str) -> Option<Option<&'a str>> {
    let rest = path.split('?').next()?.strip_prefix(prefix)?;
    if rest.is_empty() {
        return Some(None);
    }
    let name = rest.strip_prefix('/')?;
    (!name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)))
    .then_some(Some(name))
}

/// Gateway residence of video, reported to the browser once a second.
#[derive(Default)]
pub struct Residence {
    window: Window<120>,
    last: Option<Instant>,
}

impl Residence {
    pub fn push(&mut self, d: Duration) {
        self.window.push(d.as_micros().min(u32::MAX as u128) as u32);
    }

    /// A notice when one is due.
    pub fn due(&mut self, now: Instant) -> Option<Vec<u8>> {
        if self
            .last
            .is_some_and(|l| now.duration_since(l) < Duration::from_secs(1))
        {
            return None;
        }
        self.last = Some(now);
        let (p50, p95) = (self.window.percentile(50.0)?, self.window.percentile(95.0)?);
        Some(vm_remote_proto::tunnel::notice(&serde_json::json!({
            "gateway": { "p50_us": p50, "p95_us": p95, "samples": self.window.len() }
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_paths() {
        assert_eq!(host_from_path("/v1/desktop", "/v1/desktop"), Some(None));
        assert_eq!(
            host_from_path("/v1/desktop/studio-mac", "/v1/desktop"),
            Some(Some("studio-mac"))
        );
        assert_eq!(host_from_path("/v1/desktop/../x", "/v1/desktop"), None);
        assert_eq!(host_from_path("/v1/desktopx", "/v1/desktop"), None);
        assert_eq!(host_from_path("/other", "/v1/desktop"), None);
    }

    #[test]
    fn residence_notice_once_a_second() {
        let mut r = Residence::default();
        let t = Instant::now();
        assert!(r.due(t).is_none(), "no samples yet");
        r.push(Duration::from_micros(300));
        assert!(r.due(t + Duration::from_millis(500)).is_none());
        let n = r.due(t + Duration::from_millis(1600)).unwrap();
        assert_eq!(n[0], 0xff);
    }
}
