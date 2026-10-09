//! Signed, one-use pairing-authorized ICE bootstrap; not VM authorization itself.
use super::*;
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub username: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub credential: String,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IceConfig {
    pub version: u64,
    pub transport_policy: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub ice_servers: Vec<IceServer>,
}
impl IceConfig {
    pub fn canonical(&self) -> Result<Vec<u8>> {
        ensure!(
            self.version == 1 && ["all", "relay"].contains(&self.transport_policy.as_str()),
            "invalid ICE policy/version"
        );
        ensure!(
            self.issued_at <= MAX_SAFE_INTEGER
                && self.expires_at <= MAX_SAFE_INTEGER
                && self.expires_at > self.issued_at
                && self.expires_at - self.issued_at <= 600,
            "invalid ICE lifetime"
        );
        ensure!(self.ice_servers.len() <= 4, "ICE server count exceeded");
        let mut urls = std::collections::BTreeSet::new();
        let mut turn = false;
        let mut c = Canon::new("ice-config");
        c.int(self.version);
        c.text(&self.transport_policy);
        c.int(self.issued_at);
        c.int(self.expires_at);
        c.0.extend((self.ice_servers.len() as u32).to_be_bytes());
        for server in &self.ice_servers {
            ensure!(
                !server.urls.is_empty() && server.urls.len() <= 8,
                "invalid ICE URL count"
            );
            let mut scheme = None;
            for url in &server.urls {
                let is_turn = validate_local_url(url)?;
                ensure!(
                    scheme.is_none_or(|old| old == is_turn),
                    "mixed STUN/TURN server entry"
                );
                scheme = Some(is_turn);
                turn |= is_turn;
                ensure!(
                    urls.insert(url.clone()) && urls.len() <= 8,
                    "duplicate or excessive ICE URLs"
                );
            }
            if scheme == Some(true) {
                for (value, max) in [(&server.username, 256), (&server.credential, 1024)] {
                    ensure!(
                        !value.is_empty()
                            && value.len() <= max
                            && value.bytes().all(|b| (0x21..=0x7e).contains(&b)),
                        "invalid short-lived TURN credential"
                    );
                }
            } else {
                ensure!(
                    server.username.is_empty() && server.credential.is_empty(),
                    "STUN credentials forbidden"
                );
            }
            c.strings(&server.urls);
            c.text(&server.username);
            c.text(&server.credential);
        }
        ensure!(
            self.transport_policy != "relay" || turn,
            "relay policy requires TURN"
        );
        Ok(c.0)
    }
    pub fn check_time(&self, now: u64) -> Result<()> {
        self.canonical()?;
        ensure!(
            now >= self.issued_at && self.expires_at.checked_sub(now).is_some_and(|v| v >= 210),
            "ICE credentials expired or insufficient lifetime"
        );
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        Ok(hash(&self.canonical()?))
    }
}
/// Current transport diagnostic accepts literal IPv4 loopback UDP only. External
/// hosts/TLS/TCP/IPv6 require a separately validated future deployment policy.
pub fn validate_local_url(url: &str) -> Result<bool> {
    ensure!(url.len() <= 128, "ICE URL too long");
    let (turn, port) = if let Some(v) = url.strip_prefix("turn:127.0.0.1:") {
        (
            true,
            v.strip_suffix("?transport=udp")
                .context("TURN UDP transport required")?,
        )
    } else {
        (
            false,
            url.strip_prefix("stun:127.0.0.1:")
                .context("only explicit loopback ICE URLs supported")?,
        )
    };
    ensure!(
        !port.is_empty() && port.len() <= 5 && port.bytes().all(|v| v.is_ascii_digit()),
        "invalid ICE port"
    );
    let parsed = port
        .parse::<u16>()
        .ok()
        .filter(|v| *v > 0)
        .context("invalid ICE port")?;
    ensure!(parsed.to_string() == port, "noncanonical ICE port");
    Ok(turn)
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IceRequest {
    pub challenge: Challenge,
    pub request_nonce: String,
}
impl IceRequest {
    pub fn canonical(&self) -> Result<Vec<u8>> {
        self.challenge.validate()?;
        unhex(&self.request_nonce, 32)?;
        let mut c = Canon::new("ice-config-request");
        self.challenge.fields(&mut c);
        c.text(&self.request_nonce);
        Ok(c.0)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedIceRequest {
    pub request: IceRequest,
    pub signature: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedIceConfig {
    pub request: IceRequest,
    pub config: IceConfig,
    pub signature: String,
}
impl SignedIceConfig {
    pub fn signing_bytes(request: &IceRequest, config: &IceConfig) -> Result<Vec<u8>> {
        let mut c = Canon::new("ice-config-response");
        c.text(&hash(&request.canonical()?));
        c.text(&config.digest()?);
        Ok(c.0)
    }
    pub fn new(request: IceRequest, config: IceConfig, identity: &impl Identity) -> Result<Self> {
        ensure!(
            identity_id(&identity.public_key()?)? == request.challenge.host_id,
            "ICE host identity mismatch"
        );
        let signature = identity.sign(&Self::signing_bytes(&request, &config)?)?;
        Ok(Self {
            request,
            config,
            signature,
        })
    }
    pub fn verify(&self, host: &str, expected: &IceRequest, now: u64) -> Result<()> {
        ensure!(
            &self.request == expected && identity_id(host)? == self.request.challenge.host_id,
            "ICE response request/host mismatch"
        );
        ensure!(
            now >= self.request.challenge.issued_at && now < self.request.challenge.expires_at,
            "ICE challenge expired"
        );
        self.config.check_time(now)?;
        verify(
            host,
            &Self::signing_bytes(&self.request, &self.config)?,
            &self.signature,
        )
    }
}
impl SessionGate {
    pub fn authorize_ice_grant(&self, challenge: &Challenge) -> Result<()> {
        let state = self.state.lock().unwrap();
        authorized(&state.grants, challenge)?;
        Ok(())
    }

    pub fn accept_ice_request(&self, signed: &SignedIceRequest, now: u64) -> Result<()> {
        let request = &signed.request;
        let bytes = request.canonical()?;
        let challenge = &request.challenge;
        ensure!(
            challenge.instance_nonce == self.instance && challenge.host_id == self.host_id,
            "ICE host/instance mismatch"
        );
        let mut state = self.state.lock().unwrap();
        prune(&mut state, now);
        ensure!(
            state.challenges.get(&challenge.nonce) == Some(challenge)
                && now >= challenge.issued_at
                && now < challenge.expires_at,
            "ICE challenge unavailable"
        );
        let grant = authorized(&state.grants, challenge)?;
        ensure!(
            challenge.scopes.iter().any(|s| s == "observe"),
            "ICE bootstrap requires observe"
        );
        verify(&grant.device_public_key, &bytes, &signed.signature)?;
        state.challenges.remove(&challenge.nonce);
        Ok(())
    }
}
