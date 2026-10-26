//! `gateway.toml`: one file for every deployment. `vm-gateway` reads
//! `[listen]`, `[tls]`, `[tunnel]`, `[limits]`; the Mac host
//! (`vm-media-local remote`) reads `[host]`, `[turn]`, `[limits]`, `[video]`.
//! TURN settings may instead come from `VEYNODE_GATEWAY_TURN_*` variables
//! (see [`crate::turn`]); a relay in the file wins over the environment.
use crate::turn::{self, Relay};
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;

pub const MAX_FILE: u64 = 64 * 1024;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub listen: Option<Listen>,
    #[serde(default)]
    pub tls: Option<Tls>,
    #[serde(default)]
    pub tunnel: Option<Tunnel>,
    #[serde(default)]
    pub host: Option<Host>,
    #[serde(default)]
    pub turn: Option<Relay>,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub video: Video,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listen {
    /// wss, static UI and `/v1/capabilities`.
    pub tcp: SocketAddr,
    /// HTTP/3 and WebTransport; absent disables WebTransport (Cloudflare).
    pub udp: Option<SocketAddr>,
    /// Plain HTTP: ACME HTTP-01 challenges and a redirect to HTTPS.
    pub http: Option<SocketAddr>,
    pub allowed_origins: Vec<String>,
    /// Directory of the static viewer bundle, served at `/`.
    pub ui: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Tls {
    Acme {
        domains: Vec<String>,
        email: String,
        /// Account key, certificate and order state.
        state_dir: PathBuf,
        #[serde(default)]
        staging: bool,
    },
    Files {
        cert: PathBuf,
        key: PathBuf,
    },
    /// A P-256 certificate made at start and rotated before its 13 days
    /// end, for LAN use: browsers trust it through the pinned hash
    /// (`serverCertificateHashes` for WebTransport) published in
    /// `/v1/capabilities` and the pairing code, not through a CA.
    SelfSigned {
        names: Vec<String>,
    },
    /// TLS ends in front (cloudflared); only allowed on loopback.
    External,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tunnel {
    /// Where hosts dial in (QUIC).
    pub listen: SocketAddr,
    /// Persistent P-256 key (PKCS#8 PEM, created when absent) of the
    /// tunnel's own self-signed certificate. Hosts pin its key, so the
    /// tunnel works the same whether the public side uses ACME, files or
    /// Cloudflare. `vm-gateway pin` prints the pin.
    pub key: PathBuf,
    pub hosts: Vec<HostEntry>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostEntry {
    /// `sha256:<hex>` of the host tunnel certificate's SPKI.
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    /// `host:port` of the gateway tunnel listener.
    pub connect: String,
    /// TLS server name; defaults to the host part of `connect`.
    pub server_name: Option<String>,
    /// `sha256:<hex>` pin of the gateway tunnel certificate's key.
    pub gateway_spki_sha256: String,
    /// Persistent P-256 key of this host's tunnel client certificate
    /// (created when absent); the gateway pins it in `[tunnel] hosts`.
    pub key: PathBuf,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub conn_per_ip_per_min: u32,
    pub handshakes_per_device_per_min: u32,
    pub sessions_max: u32,
    pub viewers_per_vm: u32,
    /// Encoder tracks per VM, the full screen included (crop views).
    pub tracks_per_vm: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            conn_per_ip_per_min: 30,
            handshakes_per_device_per_min: 8,
            sessions_max: 32,
            viewers_per_vm: 8,
            tracks_per_vm: 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VideoMode {
    /// One short-lived WebTransport stream per access unit.
    Stream,
    /// Fragments in datagrams.
    Datagram,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Video {
    pub mode: VideoMode,
    pub start_bps: u64,
}

impl Default for Video {
    fn default() -> Self {
        Self {
            mode: VideoMode::Stream,
            start_bps: crate::abr::START_REMOTE_BPS,
        }
    }
}

pub fn spki_pin(s: &str) -> Result<()> {
    let hex = s
        .strip_prefix("sha256:")
        .context("pin must start with sha256:")?;
    ensure!(
        hex.len() == 64
            && hex
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "pin must be 64 lowercase hex digits"
    );
    Ok(())
}

fn origin(o: &str) -> Result<()> {
    let rest = o
        .strip_prefix("https://")
        .with_context(|| format!("origin {o} must be https"))?;
    ensure!(
        !rest.is_empty()
            && !rest.contains('/')
            && !rest.contains('?')
            && !rest.contains('#')
            && !rest.contains('@'),
        "origin {o} must be scheme://host[:port] only"
    );
    Ok(())
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        ensure!(text.len() as u64 <= MAX_FILE, "config larger than 64 KiB");
        let config: Self = toml::from_str(text).context("invalid gateway.toml")?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &std::path::Path) -> Result<Self> {
        let meta = std::fs::metadata(path).with_context(|| format!("read {}", path.display()))?;
        ensure!(meta.len() <= MAX_FILE, "config larger than 64 KiB");
        Self::parse(&std::fs::read_to_string(path)?)
    }

    /// The TURN relay: the file's, else the environment's, else none (STUN only).
    pub fn relay(&self, env: impl Fn(&str) -> Option<String>) -> Option<Relay> {
        self.turn.clone().or_else(|| turn::from_env(env))
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(l) = &self.listen {
            ensure!(
                !l.allowed_origins.is_empty(),
                "listen.allowed_origins is required"
            );
            for o in &l.allowed_origins {
                origin(o)?;
            }
            match &self.tls {
                None => bail!("[listen] needs [tls]"),
                Some(Tls::External) => {
                    ensure!(
                        l.tcp.ip().is_loopback(),
                        "tls mode external only listens on loopback"
                    );
                    ensure!(
                        l.udp.is_none(),
                        "WebTransport needs TLS here; remove listen.udp with tls external"
                    );
                }
                Some(Tls::Acme { domains, email, .. }) => {
                    ensure!(!domains.is_empty(), "tls.domains is required for acme");
                    ensure!(email.contains('@'), "tls.email is required for acme");
                    ensure!(
                        l.http.is_some(),
                        "acme answers HTTP-01 challenges: set listen.http"
                    );
                }
                Some(Tls::Files { .. }) => {}
                Some(Tls::SelfSigned { names }) => {
                    ensure!(!names.is_empty(), "tls.names is required for self-signed")
                }
            }
        }
        if let Some(t) = &self.tunnel {
            ensure!(!t.hosts.is_empty(), "tunnel.hosts is empty");
            for h in &t.hosts {
                spki_pin(&h.id)?;
                ensure!(!h.name.is_empty() && h.name.len() <= 64, "tunnel host name");
            }
        }
        if let Some(h) = &self.host {
            ensure!(
                h.connect
                    .rsplit_once(':')
                    .is_some_and(|(_, p)| p.parse::<u16>().is_ok()),
                "host.connect must be host:port"
            );
            spki_pin(&h.gateway_spki_sha256)?;
        }
        if let Some(r) = &self.turn {
            r.validate()?;
        }
        let l = &self.limits;
        ensure!(
            l.viewers_per_vm >= 1 && l.viewers_per_vm <= 64,
            "limits.viewers_per_vm 1..=64"
        );
        ensure!(
            l.tracks_per_vm >= 1 && l.tracks_per_vm <= 8,
            "limits.tracks_per_vm 1..=8"
        );
        ensure!(l.sessions_max >= 1, "limits.sessions_max");
        ensure!(
            (crate::abr::MIN_BPS..=crate::abr::MAX_BPS).contains(&self.video.start_bps),
            "video.start_bps outside 250 kbps..40 Mbps"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_example_parses() {
        let text = include_str!("../../../deploy/gateway/gateway.example.toml");
        let c = Config::parse(text).unwrap();
        assert!(c.listen.unwrap().udp.is_some());
        assert!(matches!(c.tls, Some(Tls::Acme { staging: true, .. })));
        assert!(matches!(c.turn, Some(Relay::SharedSecret { .. })));
    }

    const EC2: &str = r#"
[listen]
tcp = "0.0.0.0:443"
udp = "0.0.0.0:443"
http = "0.0.0.0:80"
allowed_origins = ["https://desk.example.com"]
[tls]
mode = "acme"
domains = ["desk.example.com"]
email = "ops@example.com"
state_dir = "/data/acme"
[tunnel]
listen = "0.0.0.0:7443"
key = "/data/tunnel.key"
hosts = [{ id = "sha256:0000000000000000000000000000000000000000000000000000000000000000", name = "studio" }]
[turn]
kind = "shared-secret"
urls = ["turn:desk.example.com:3478?transport=udp"]
secret = "0123456789abcdef"
"#;

    #[test]
    fn ec2_example_parses() {
        let c = Config::parse(EC2).unwrap();
        assert!(matches!(c.tls, Some(Tls::Acme { .. })));
        assert_eq!(c.limits.viewers_per_vm, 8);
        assert_eq!(c.video.mode, VideoMode::Stream);
        assert!(matches!(
            c.relay(|_| None),
            Some(Relay::SharedSecret { .. })
        ));
    }

    #[test]
    fn external_tls_only_on_loopback_without_webtransport() {
        let base = |tcp: &str, udp: &str| {
            format!("[listen]\ntcp = \"{tcp}\"\n{udp}allowed_origins = [\"https://d.example.com\"]\n[tls]\nmode = \"external\"\n")
        };
        Config::parse(&base("127.0.0.1:8790", "")).unwrap();
        assert!(Config::parse(&base("0.0.0.0:8790", "")).is_err());
        assert!(Config::parse(&base("127.0.0.1:8790", "udp = \"127.0.0.1:8790\"\n")).is_err());
    }

    #[test]
    fn rejects_bad_origins_pins_and_unknown_keys() {
        let bad_origin = EC2.replace("https://desk.example.com\"]", "http://desk.example.com\"]");
        assert!(Config::parse(&bad_origin).is_err());
        assert!(Config::parse(&EC2.replace("sha256:0000", "sha1:0000")).is_err());
        assert!(Config::parse(&format!("{EC2}\nsurprise = 1\n")).is_err());
    }

    #[test]
    fn environment_relay_used_when_file_has_none() {
        let c = Config::parse("[host]\nconnect = \"g.example.com:7443\"\nname = \"m\"\nkey = \"/k\"\ngateway_spki_sha256 = \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"\n").unwrap();
        let env = |k: &str| match k {
            "VEYNODE_GATEWAY_TURN_KEY_ID" => Some("k".to_string()),
            "VEYNODE_GATEWAY_TURN_KEY_API_TOKEN" => Some("t".to_string()),
            _ => None,
        };
        assert!(matches!(c.relay(env), Some(Relay::Cloudflare { .. })));
    }
}
