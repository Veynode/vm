//! Remote desktop handshake over the gateway (WebTransport or WebSocket).
//!
//! Same pairing grants, challenges and atomic challenge consumption as the
//! WebRTC handshake, with SDP replaced by ephemeral P-256 ECDH keys: the
//! browser signs its key inside the offer, the host signs its key inside the
//! answer, and both derive the end-to-end record keys from the ECDH secret
//! with the answer digest as salt (`vm_remote_proto::secure`). A relay that
//! substitutes either key breaks a signature. The offer also binds the outer
//! transport and the page origin, so a session negotiated for one origin or
//! transport cannot be replayed through another. Sessions carry their own
//! deadline (at most 8 h, as `vm-mcp-http`), enforced by `authorize_remote`.
use super::*;

pub const TRANSPORTS: [&str; 2] = ["wss", "wt"];
pub const CODECS: [&str; 2] = ["h264", "h265"];
pub const MAX_SESSION_SECONDS: u64 = 8 * 3600;
pub const MAX_ORIGIN: usize = 256;

fn ecdh_key(s: &str) -> Result<()> {
    let bytes = unhex(s, 65)?;
    ensure!(
        bytes[0] == 4,
        "ECDH key must be an uncompressed P-256 point"
    );
    p256::PublicKey::from_sec1_bytes(&bytes).context("invalid ECDH P-256 point")?;
    Ok(())
}

fn https_origin(o: &str) -> Result<()> {
    let rest = o.strip_prefix("https://").context("origin must be https")?;
    ensure!(
        !rest.is_empty()
            && o.len() <= MAX_ORIGIN
            && rest
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-.:[]".contains(&c)),
        "origin must be https://host[:port]"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteOfferIntent {
    pub challenge: Challenge,
    pub request_id: String,
    /// Codecs the browser decodes, sorted and unique.
    pub codecs: Vec<String>,
    /// Browser ephemeral ECDH key, uncompressed SEC1 hex.
    pub browser_ecdh: String,
    /// Outer transport: `wt` or `wss`.
    pub transport: String,
    /// The page origin the browser runs on.
    pub origin: String,
}

impl RemoteOfferIntent {
    pub fn canonical(&self) -> Result<Vec<u8>> {
        self.challenge.validate()?;
        identifier(&self.request_id)?;
        ensure!(
            !self.codecs.is_empty()
                && self.codecs.iter().all(|c| CODECS.contains(&c.as_str()))
                && self.codecs.windows(2).all(|w| w[0] < w[1]),
            "codecs must be known, sorted and unique"
        );
        ecdh_key(&self.browser_ecdh)?;
        ensure!(
            TRANSPORTS.contains(&self.transport.as_str()),
            "invalid transport"
        );
        https_origin(&self.origin)?;
        let mut c = Canon::new("remote-offer");
        self.fields(&mut c);
        Ok(c.0)
    }
    fn fields(&self, c: &mut Canon) {
        self.challenge.fields(c);
        c.text(&self.request_id);
        c.strings(&self.codecs);
        c.text(&self.browser_ecdh);
        c.text(&self.transport);
        c.text(&self.origin);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRemoteOffer {
    pub intent: RemoteOfferIntent,
    pub signature: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteTranscript {
    pub intent: RemoteOfferIntent,
    pub session_id: String,
    /// Host ephemeral ECDH key, uncompressed SEC1 hex.
    pub host_ecdh: String,
    /// The codec the host will send, one of the offered.
    pub codec: String,
    /// Unix seconds after which the session must end.
    pub expires_at: u64,
}

impl RemoteTranscript {
    pub fn canonical(&self) -> Result<Vec<u8>> {
        self.intent.canonical()?;
        identifier(&self.session_id)?;
        ecdh_key(&self.host_ecdh)?;
        ensure!(
            self.intent.codecs.contains(&self.codec),
            "codec was not offered"
        );
        let issued = self.intent.challenge.issued_at;
        ensure!(
            self.expires_at > issued
                && self.expires_at - issued <= MAX_SESSION_SECONDS
                && self.expires_at <= MAX_SAFE_INTEGER,
            "invalid session lifetime"
        );
        let mut c = Canon::new("remote-answer");
        self.intent.fields(&mut c);
        c.text(&self.session_id);
        c.text(&self.host_ecdh);
        c.text(&self.codec);
        c.int(self.expires_at);
        Ok(c.0)
    }
    /// SHA-256 of the canonical answer: the confirm digest and the HKDF salt.
    pub fn digest(&self) -> Result<String> {
        Ok(hash(&self.canonical()?))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRemoteAnswer {
    pub transcript: RemoteTranscript,
    pub signature: String,
}

impl SignedRemoteAnswer {
    /// The browser's check before deriving keys.
    pub fn verify(&self, host_public: &str, expected: &RemoteOfferIntent) -> Result<()> {
        ensure!(
            &self.transcript.intent == expected,
            "offer intent substituted"
        );
        ensure!(
            self.transcript.intent.challenge.host_id == identity_id(host_public)?,
            "host substituted"
        );
        verify(host_public, &self.transcript.canonical()?, &self.signature)
    }
}

pub fn remote_confirmation_bytes(digest: &str) -> Result<Vec<u8>> {
    unhex(digest, 32)?;
    let mut c = Canon::new("remote-confirm");
    c.text(digest);
    Ok(c.0)
}

/// What the host chooses for a session it answers.
pub struct RemoteAnswerParams {
    pub session_id: String,
    /// Host ephemeral ECDH key, uncompressed SEC1 hex.
    pub host_ecdh: String,
    pub codec: String,
    pub lifetime_seconds: u64,
}

pub struct AcceptedRemoteOffer {
    intent: RemoteOfferIntent,
}

impl AcceptedRemoteOffer {
    pub fn intent(&self) -> &RemoteOfferIntent {
        &self.intent
    }
}

impl SessionGate {
    /// Verifies the signed offer and consumes its challenge atomically.
    /// `transport` and `origin` are what the host actually saw (forwarded
    /// by the gateway); they must match what the browser signed.
    pub fn accept_remote_offer(
        &self,
        signed: &SignedRemoteOffer,
        transport: &str,
        origin: &str,
        now: u64,
    ) -> Result<AcceptedRemoteOffer> {
        let intent = &signed.intent;
        let bytes = intent.canonical()?;
        ensure!(
            intent.transport == transport && intent.origin == origin,
            "transport or origin substituted"
        );
        ensure!(
            intent.challenge.instance_nonce == self.instance,
            "previous host instance"
        );
        let mut state = self.state.lock().unwrap();
        prune(&mut state, now);
        ensure!(sessions(&state) < 256, "session capacity");
        ensure!(
            state.challenges.get(&intent.challenge.nonce) == Some(&intent.challenge),
            "challenge expired, replaced or consumed"
        );
        ensure!(
            now >= intent.challenge.issued_at && now < intent.challenge.expires_at,
            "challenge expired"
        );
        let grant = authorized(&state.grants, &intent.challenge)?;
        verify(&grant.device_public_key, &bytes, &signed.signature)?;
        state.challenges.remove(&intent.challenge.nonce);
        Ok(AcceptedRemoteOffer {
            intent: intent.clone(),
        })
    }

    pub fn remote_answer(
        &self,
        accepted: AcceptedRemoteOffer,
        answer: RemoteAnswerParams,
        host: &impl Identity,
        now: u64,
    ) -> Result<SignedRemoteAnswer> {
        let RemoteAnswerParams {
            session_id,
            host_ecdh,
            codec,
            lifetime_seconds,
        } = answer;
        ensure!(
            identity_id(&host.public_key()?)? == self.host_id
                && accepted.intent.challenge.instance_nonce == self.instance,
            "host/instance mismatch"
        );
        ensure!(
            (1..=MAX_SESSION_SECONDS).contains(&lifetime_seconds),
            "invalid session lifetime"
        );
        let transcript = RemoteTranscript {
            intent: accepted.intent,
            session_id,
            host_ecdh,
            codec,
            expires_at: now.checked_add(lifetime_seconds).context("time overflow")?,
        };
        let bytes = transcript.canonical()?;
        let mut state = self.state.lock().unwrap();
        prune(&mut state, now);
        authorized(&state.grants, &transcript.intent.challenge)?;
        ensure!(
            now < transcript.intent.challenge.expires_at,
            "challenge expired"
        );
        ensure!(
            sessions(&state) < 256 && !session_id_taken(&state, &transcript.session_id),
            "session capacity or duplicate"
        );
        let signature = host.sign(&bytes)?;
        state
            .remote_pending
            .insert(transcript.session_id.clone(), transcript.clone());
        Ok(SignedRemoteAnswer {
            transcript,
            signature,
        })
    }

    pub fn remote_confirm(
        &self,
        session_id: &str,
        confirmation: &Confirmation,
        now: u64,
    ) -> Result<RemoteTranscript> {
        let mut state = self.state.lock().unwrap();
        prune(&mut state, now);
        let t = state
            .remote_pending
            .get(session_id)
            .context("pending session unavailable")?;
        ensure!(
            now >= t.intent.challenge.issued_at && now < t.intent.challenge.expires_at,
            "confirmation expired"
        );
        let grant = authorized(&state.grants, &t.intent.challenge)?;
        ensure!(
            t.digest()? == confirmation.transcript_sha256,
            "transcript substituted"
        );
        verify(
            &grant.device_public_key,
            &remote_confirmation_bytes(&confirmation.transcript_sha256)?,
            &confirmation.signature,
        )?;
        let t = state.remote_pending.remove(session_id).unwrap();
        state.remote_active.insert(session_id.into(), t.clone());
        Ok(t)
    }

    /// Call before every sensitive operation of a remote session (input,
    /// lease changes) and periodically for observation. Failure means the
    /// transport owner must close the session (revoked or expired).
    pub fn authorize_remote(&self, session_id: &str, scope: &str, now: u64) -> Result<()> {
        let state = self.state.lock().unwrap();
        let t = state
            .remote_active
            .get(session_id)
            .context("inactive session")?;
        ensure!(now < t.expires_at, "session expired");
        authorized(&state.grants, &t.intent.challenge)?;
        ensure!(
            t.intent.challenge.scopes.iter().any(|s| s == scope),
            "scope denied"
        );
        Ok(())
    }

    /// Active remote sessions past their deadline; the caller closes them.
    pub fn expired_remote(&self, now: u64) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state
            .remote_active
            .iter()
            .filter(|(_, t)| now >= t.expires_at)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

pub(crate) fn sessions(state: &GateState) -> usize {
    state.pending.len()
        + state.active.len()
        + state.remote_pending.len()
        + state.remote_active.len()
}

fn session_id_taken(state: &GateState, id: &str) -> bool {
    state.pending.contains_key(id)
        || state.active.contains_key(id)
        || state.remote_pending.contains_key(id)
        || state.remote_active.contains_key(id)
}
