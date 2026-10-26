//! Typed pairing/session authentication. No network listener and no media transport.
//! Callers must stop active transports returned by `replace_grants` and enforce expiry.
pub mod ice;
pub mod rejection;
use anyhow::{ensure, Context, Result};
use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Mutex;

pub mod cli;
pub mod control;
pub mod files;
#[cfg(target_os = "macos")]
pub mod keychain;
pub mod store;

pub const MAX_SDP: usize = 128 * 1024;
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
pub const SCOPES: [&str; 5] = ["execute", "files", "input", "lifecycle", "observe"];

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn unhex(s: &str, n: usize) -> Result<Vec<u8>> {
    ensure!(
        s.len() == n * 2
            && s.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "invalid lowercase hex"
    );
    (0..n)
        .map(|i| Ok(u8::from_str_radix(&s[2 * i..2 * i + 2], 16)?))
        .collect()
}
pub fn hash(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
pub fn random32() -> String {
    let mut n = [0; 32];
    OsRng.fill_bytes(&mut n);
    hex(&n)
}
fn identifier(s: &str) -> Result<()> {
    ensure!(
        !s.is_empty()
            && s.len() <= 128
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
        "invalid identifier"
    );
    Ok(())
}
pub fn validate_scopes(scopes: &[String]) -> Result<()> {
    ensure!(
        !scopes.is_empty() && scopes.len() <= SCOPES.len(),
        "invalid scopes"
    );
    ensure!(
        scopes.iter().all(|s| SCOPES.contains(&s.as_str()))
            && scopes.windows(2).all(|w| w[0] < w[1]),
        "scopes must be known, sorted and unique"
    );
    Ok(())
}
fn public_key(s: &str) -> Result<VerifyingKey> {
    let bytes = unhex(s, 65)?;
    ensure!(bytes[0] == 4, "only uncompressed SEC1 P256 keys accepted");
    VerifyingKey::from_sec1_bytes(&bytes).context("invalid P256 point")
}
pub fn identity_id(public: &str) -> Result<String> {
    public_key(public)?;
    let mut bytes = b"Veynode/identity/v1\0".to_vec();
    bytes.extend(unhex(public, 65)?);
    Ok(hash(&bytes))
}
pub trait Identity {
    fn public_key(&self) -> Result<String>;
    fn sign(&self, message: &[u8]) -> Result<String>;
}
pub fn verify(public: &str, bytes: &[u8], signature: &str) -> Result<()> {
    public_key(public)?
        .verify(bytes, &Signature::from_slice(&unhex(signature, 64)?)?)
        .context("invalid signature")
}
struct Canon(Vec<u8>);
impl Canon {
    fn new(domain: &str) -> Self {
        Self(format!("Veynode/{domain}/v1\0").into_bytes())
    }
    fn text(&mut self, s: &str) {
        self.0.extend((s.len() as u32).to_be_bytes());
        self.0.extend(s.as_bytes());
    }
    fn int(&mut self, n: u64) {
        self.0.extend(n.to_be_bytes());
    }
    fn strings(&mut self, values: &[String]) {
        self.0.extend((values.len() as u32).to_be_bytes());
        for v in values {
            self.text(v)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Pairing {
    pub host_public_key: String,
    pub device_public_key: String,
    pub nonce: String,
    pub vm_id: String,
    pub scopes: Vec<String>,
}
impl Pairing {
    pub fn canonical(&self) -> Result<Vec<u8>> {
        public_key(&self.host_public_key)?;
        public_key(&self.device_public_key)?;
        unhex(&self.nonce, 32)?;
        identifier(&self.vm_id)?;
        validate_scopes(&self.scopes)?;
        let mut c = Canon::new("pairing");
        c.text(&self.host_public_key);
        c.text(&self.device_public_key);
        c.text(&self.nonce);
        c.text(&self.vm_id);
        c.strings(&self.scopes);
        Ok(c.0)
    }
    pub fn digest(&self) -> Result<String> {
        Ok(hash(&self.canonical()?))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub host_id: String,
    pub device_id: String,
    pub device_public_key: String,
    pub vm_id: String,
    pub scopes: Vec<String>,
    pub version: u64,
}
impl Grant {
    pub fn canonical(&self) -> Result<Vec<u8>> {
        unhex(&self.host_id, 32)?;
        ensure!(
            identity_id(&self.device_public_key)? == self.device_id,
            "device identity mismatch"
        );
        identifier(&self.vm_id)?;
        validate_scopes(&self.scopes)?;
        ensure!(
            (1..=MAX_SAFE_INTEGER).contains(&self.version),
            "invalid grant version"
        );
        let mut c = Canon::new("grant");
        self.fields(&mut c);
        Ok(c.0)
    }
    fn fields(&self, c: &mut Canon) {
        c.text(&self.host_id);
        c.text(&self.device_id);
        c.text(&self.device_public_key);
        c.text(&self.vm_id);
        c.strings(&self.scopes);
        c.int(self.version)
    }
    pub fn key(&self) -> String {
        format!("{}:{}", self.device_id, self.vm_id)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedGrant {
    pub grant: Grant,
    pub signature: String,
}
impl SignedGrant {
    pub fn new(grant: Grant, host: &impl Identity) -> Result<Self> {
        ensure!(
            grant.host_id == identity_id(&host.public_key()?)?,
            "host mismatch"
        );
        let signature = host.sign(&grant.canonical()?)?;
        Ok(Self { grant, signature })
    }
    pub fn verify(&self, host_public: &str) -> Result<()> {
        ensure!(
            self.grant.host_id == identity_id(host_public)?,
            "host mismatch"
        );
        verify(host_public, &self.grant.canonical()?, &self.signature)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Challenge {
    pub host_id: String,
    pub device_id: String,
    pub vm_id: String,
    pub scopes: Vec<String>,
    pub grant_version: u64,
    pub instance_nonce: String,
    pub nonce: String,
    pub issued_at: u64,
    pub expires_at: u64,
}
impl Challenge {
    fn validate(&self) -> Result<()> {
        unhex(&self.host_id, 32)?;
        unhex(&self.device_id, 32)?;
        identifier(&self.vm_id)?;
        validate_scopes(&self.scopes)?;
        unhex(&self.instance_nonce, 32)?;
        unhex(&self.nonce, 32)?;
        ensure!(
            (1..=MAX_SAFE_INTEGER).contains(&self.grant_version)
                && self.issued_at <= MAX_SAFE_INTEGER
                && self.expires_at <= MAX_SAFE_INTEGER,
            "integer out of range"
        );
        ensure!(
            self.expires_at > self.issued_at && self.expires_at - self.issued_at <= 60,
            "invalid challenge lifetime"
        );
        Ok(())
    }
    fn fields(&self, c: &mut Canon) {
        c.text(&self.host_id);
        c.text(&self.device_id);
        c.text(&self.vm_id);
        c.strings(&self.scopes);
        c.int(self.grant_version);
        c.text(&self.instance_nonce);
        c.text(&self.nonce);
        c.int(self.issued_at);
        c.int(self.expires_at)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SdpBinding {
    pub sha256: String,
    pub fingerprint_sha256: String,
}
impl SdpBinding {
    pub fn from_sdp(sdp: &str) -> Result<Self> {
        ensure!(
            !sdp.is_empty() && sdp.len() <= MAX_SDP && !sdp.contains('\0'),
            "invalid SDP size/content"
        );
        let mut fp = None;
        let mut session_fp = false;
        let mut media_count = 0;
        let mut media_with_fp = 0;
        let mut media_fp = false;
        for line in sdp.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            ensure!(!line.contains('\r'), "invalid SDP line ending");
            if line.starts_with("m=") {
                if media_count > 0 && media_fp {
                    media_with_fp += 1
                };
                media_count += 1;
                media_fp = false;
            }
            if let Some(value) = line.strip_prefix("a=fingerprint:") {
                let parts: Vec<_> = value.split(' ').collect();
                ensure!(
                    parts.len() == 2 && parts[0] == "sha-256",
                    "unsupported or malformed DTLS fingerprint"
                );
                let octets: Vec<_> = parts[1].split(':').collect();
                ensure!(
                    octets.len() == 32
                        && octets
                            .iter()
                            .all(|x| x.len() == 2 && x.bytes().all(|c| c.is_ascii_hexdigit())),
                    "malformed SHA256 fingerprint"
                );
                let normalized = octets.join("").to_ascii_lowercase();
                if let Some(previous) = &fp {
                    ensure!(previous == &normalized, "conflicting DTLS fingerprints")
                } else {
                    fp = Some(normalized)
                }
                if media_count == 0 {
                    session_fp = true
                } else {
                    media_fp = true
                }
            }
        }
        if media_count > 0 && media_fp {
            media_with_fp += 1
        };
        ensure!(
            media_count > 0 && (session_fp || media_count == media_with_fp),
            "unbound media fingerprint"
        );
        Ok(Self {
            sha256: hash(sdp.as_bytes()),
            fingerprint_sha256: fp.context("missing DTLS fingerprint")?,
        })
    }
    fn fields(&self, c: &mut Canon) -> Result<()> {
        unhex(&self.sha256, 32)?;
        unhex(&self.fingerprint_sha256, 32)?;
        c.text(&self.sha256);
        c.text(&self.fingerprint_sha256);
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OfferIntent {
    pub challenge: Challenge,
    pub request_id: String,
    pub codec: String,
    pub offer: SdpBinding,
}
impl OfferIntent {
    pub fn canonical(&self) -> Result<Vec<u8>> {
        self.challenge.validate()?;
        identifier(&self.request_id)?;
        ensure!(
            matches!(self.codec.as_str(), "h264" | "h265"),
            "invalid codec"
        );
        let mut c = Canon::new("offer");
        self.fields(&mut c)?;
        Ok(c.0)
    }
    fn fields(&self, c: &mut Canon) -> Result<()> {
        self.challenge.fields(c);
        c.text(&self.request_id);
        c.text(&self.codec);
        self.offer.fields(c)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedOffer {
    pub intent: OfferIntent,
    pub signature: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AnswerTranscript {
    pub intent: OfferIntent,
    pub session_id: String,
    pub answer: SdpBinding,
}
impl AnswerTranscript {
    pub fn canonical(&self) -> Result<Vec<u8>> {
        self.intent.canonical()?;
        identifier(&self.session_id)?;
        let mut c = Canon::new("answer");
        self.intent.fields(&mut c)?;
        c.text(&self.session_id);
        self.answer.fields(&mut c)?;
        Ok(c.0)
    }
    pub fn digest(&self) -> Result<String> {
        Ok(hash(&self.canonical()?))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedAnswer {
    pub transcript: AnswerTranscript,
    pub signature: String,
}
impl SignedAnswer {
    pub fn verify(
        &self,
        host_public: &str,
        expected_intent: &OfferIntent,
        exact_answer_sdp: &str,
    ) -> Result<()> {
        ensure!(
            &self.transcript.intent == expected_intent,
            "offer intent substituted"
        );
        ensure!(
            self.transcript.intent.challenge.host_id == identity_id(host_public)?,
            "host substituted"
        );
        ensure!(
            self.transcript.answer == SdpBinding::from_sdp(exact_answer_sdp)?,
            "answer SDP substituted"
        );
        verify(host_public, &self.transcript.canonical()?, &self.signature)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Confirmation {
    pub transcript_sha256: String,
    pub signature: String,
}
pub fn confirmation_bytes(digest: &str) -> Result<Vec<u8>> {
    unhex(digest, 32)?;
    let mut c = Canon::new("confirm");
    c.text(digest);
    Ok(c.0)
}

struct GateState {
    /// Per-key replay floor: the active version, or last revoked version + 1 (the durable tombstone).
    highest: BTreeMap<String, u64>,
    grants: BTreeMap<String, Grant>,
    challenges: BTreeMap<String, Challenge>,
    pending: BTreeMap<String, AnswerTranscript>,
    active: BTreeMap<String, AnswerTranscript>,
}
pub struct SessionGate {
    host_id: String,
    instance: String,
    state: Mutex<GateState>,
}
impl SessionGate {
    /// Fresh host with no durable registry history. Any start from an existing
    /// registry must use `with_versions`, which keeps tombstones across restarts.
    pub fn new(host_public: &str, grants: &[SignedGrant]) -> Result<Self> {
        Self::build(host_public, BTreeMap::new(), grants)
    }
    /// Restart path: seeds the replay floor from the durable per-key version map.
    /// A revoked key keeps its tombstone (last version + 1), so superseded versions
    /// stay refused. Every active grant must match its durable version, and malformed
    /// or oversized maps fail closed.
    pub fn with_versions(
        host_public: &str,
        grants: &[SignedGrant],
        versions: &BTreeMap<String, u64>,
    ) -> Result<Self> {
        ensure!(versions.len() <= 1024, "grant history capacity");
        for (key, version) in versions {
            let (device, vm) = key.split_once(':').context("invalid registry key")?;
            unhex(device, 32)?;
            identifier(vm)?;
            ensure!(
                (1..=MAX_SAFE_INTEGER).contains(version),
                "invalid persisted version"
            );
        }
        for signed in grants {
            ensure!(
                versions.get(&signed.grant.key()) == Some(&signed.grant.version),
                "registry grant/version mismatch"
            );
        }
        Self::build(host_public, versions.clone(), grants)
    }
    fn build(
        host_public: &str,
        highest: BTreeMap<String, u64>,
        grants: &[SignedGrant],
    ) -> Result<Self> {
        let gate = Self {
            host_id: identity_id(host_public)?,
            instance: random32(),
            state: Mutex::new(GateState {
                highest,
                grants: BTreeMap::new(),
                challenges: BTreeMap::new(),
                pending: BTreeMap::new(),
                active: BTreeMap::new(),
            }),
        };
        gate.replace_grants(host_public, grants)?;
        Ok(gate)
    }
    /// Atomically replace the trusted current registry snapshot. Returned session IDs MUST be stopped by the transport owner.
    pub fn replace_grants(&self, host_public: &str, grants: &[SignedGrant]) -> Result<Vec<String>> {
        ensure!(self.host_id == identity_id(host_public)?, "host mismatch");
        ensure!(grants.len() <= 1024, "grant capacity");
        let mut current = BTreeMap::new();
        for signed in grants {
            signed.verify(host_public)?;
            ensure!(
                current
                    .insert(signed.grant.key(), signed.grant.clone())
                    .is_none(),
                "duplicate grant"
            )
        }
        let mut state = self.state.lock().unwrap();
        for (key, grant) in &current {
            if let Some(old) = state.grants.get(key) {
                ensure!(
                    grant.version >= old.version && (grant.version != old.version || grant == old),
                    "grant rollback/replacement"
                )
            } else if let Some(floor) = state.highest.get(key) {
                // A non-active key's floor is its durable tombstone (last revoked version + 1),
                // so only a newer approval can pass here.
                ensure!(grant.version >= *floor, "revoked grant replay")
            }
        }
        // A grant absent from this snapshot is revoked. Advance its floor to the
        // tombstone the durable registry records (last version + 1).
        let mut tombstones = Vec::new();
        for (key, old) in &state.grants {
            if !current.contains_key(key) {
                tombstones.push((
                    key.clone(),
                    old.version.checked_add(1).context("version overflow")?,
                ));
            }
        }
        ensure!(
            state.highest.len()
                + current
                    .keys()
                    .filter(|k| !state.highest.contains_key(*k))
                    .count()
                <= 1024,
            "grant history capacity"
        );
        for (key, floor) in tombstones {
            let entry = state.highest.entry(key).or_insert(0);
            *entry = (*entry).max(floor);
        }
        for (key, grant) in &current {
            state.highest.insert(key.clone(), grant.version);
        }
        let mut revoked = Vec::new();
        state.active.retain(|id, t| {
            let keep = authorized(&current, &t.intent.challenge).is_ok();
            if !keep {
                revoked.push(id.clone())
            }
            keep
        });
        state.pending.retain(|id, t| {
            let keep = authorized(&current, &t.intent.challenge).is_ok();
            if !keep {
                revoked.push(id.clone())
            }
            keep
        });
        state
            .challenges
            .retain(|_, c| authorized(&current, c).is_ok());
        state.grants = current;
        Ok(revoked)
    }
    pub fn challenge(
        &self,
        device_id: &str,
        vm_id: &str,
        scopes: Vec<String>,
        now: u64,
    ) -> Result<Challenge> {
        unhex(device_id, 32)?;
        identifier(vm_id)?;
        validate_scopes(&scopes)?;
        let mut state = self.state.lock().unwrap();
        prune(&mut state, now);
        ensure!(state.challenges.len() < 256, "challenge capacity");
        let g = state
            .grants
            .get(&format!("{device_id}:{vm_id}"))
            .context("grant unavailable")?;
        ensure!(scopes.iter().all(|s| g.scopes.contains(s)), "scope denied");
        let c = Challenge {
            host_id: self.host_id.clone(),
            device_id: device_id.into(),
            vm_id: vm_id.into(),
            scopes,
            grant_version: g.version,
            instance_nonce: self.instance.clone(),
            nonce: random32(),
            issued_at: now,
            expires_at: now.checked_add(60).context("time overflow")?,
        };
        c.validate()?;
        state.challenges.insert(c.nonce.clone(), c.clone());
        Ok(c)
    }
    /// Verification and challenge consumption are atomic. No peer/session reservation before this succeeds.
    pub fn accept_offer(
        &self,
        signed: &SignedOffer,
        exact_sdp: &str,
        now: u64,
    ) -> Result<AcceptedOffer> {
        let intent = &signed.intent;
        let bytes = intent.canonical()?;
        ensure!(
            intent.offer == SdpBinding::from_sdp(exact_sdp)?,
            "offer SDP substituted"
        );
        ensure!(
            intent.challenge.instance_nonce == self.instance,
            "previous host instance"
        );
        let mut state = self.state.lock().unwrap();
        prune(&mut state, now);
        ensure!(
            state.pending.len() + state.active.len() < 256,
            "session capacity"
        );
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
        Ok(AcceptedOffer {
            intent: intent.clone(),
        })
    }
    pub fn answer(
        &self,
        accepted: AcceptedOffer,
        session_id: String,
        sdp: &str,
        host: &impl Identity,
        now: u64,
    ) -> Result<SignedAnswer> {
        ensure!(
            identity_id(&host.public_key()?)? == self.host_id
                && accepted.intent.challenge.instance_nonce == self.instance,
            "host/instance mismatch"
        );
        let transcript = AnswerTranscript {
            intent: accepted.intent,
            session_id,
            answer: SdpBinding::from_sdp(sdp)?,
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
            state.pending.len() + state.active.len() < 256
                && !state.pending.contains_key(&transcript.session_id)
                && !state.active.contains_key(&transcript.session_id),
            "session capacity or duplicate"
        );
        let signature = host.sign(&bytes)?;
        state
            .pending
            .insert(transcript.session_id.clone(), transcript.clone());
        Ok(SignedAnswer {
            transcript,
            signature,
        })
    }
    pub fn confirm(
        &self,
        session_id: &str,
        confirmation: &Confirmation,
        now: u64,
    ) -> Result<AnswerTranscript> {
        let mut state = self.state.lock().unwrap();
        prune(&mut state, now);
        let t = state
            .pending
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
            &confirmation_bytes(&confirmation.transcript_sha256)?,
            &confirmation.signature,
        )?;
        let t = state.pending.remove(session_id).unwrap();
        state.active.insert(session_id.into(), t.clone());
        Ok(t)
    }
    /// Check current authorization for either negotiated-pending or active sessions.
    /// Failure requires the transport owner to stop its corresponding peer/encoder.
    pub fn authorize_session(&self, session_id: &str, scope: &str) -> Result<()> {
        let state = self.state.lock().unwrap();
        let t = state
            .active
            .get(session_id)
            .or_else(|| state.pending.get(session_id))
            .context("session unavailable")?;
        authorized(&state.grants, &t.intent.challenge)?;
        ensure!(
            t.intent.challenge.scopes.iter().any(|s| s == scope),
            "scope denied"
        );
        Ok(())
    }
    pub fn close(&self, session_id: &str) {
        let mut state = self.state.lock().unwrap();
        state.pending.remove(session_id);
        state.active.remove(session_id);
    }
    /// Call before accepting each sensitive operation. Transport owns media termination and lifetime.
    pub fn authorize_active(&self, session_id: &str, scope: &str) -> Result<()> {
        let state = self.state.lock().unwrap();
        let t = state.active.get(session_id).context("inactive session")?;
        authorized(&state.grants, &t.intent.challenge)?;
        ensure!(
            t.intent.challenge.scopes.iter().any(|s| s == scope),
            "scope denied"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedClose {
    pub session_id: String,
    pub transcript_sha256: String,
    pub signature: String,
}
pub fn close_bytes(session_id: &str, digest: &str) -> Result<Vec<u8>> {
    identifier(session_id)?;
    unhex(digest, 32)?;
    let mut c = Canon::new("close");
    c.text(session_id);
    c.text(digest);
    Ok(c.0)
}
impl SessionGate {
    pub fn verify_close(&self, request: &SignedClose) -> Result<()> {
        let state = self.state.lock().unwrap();
        let t = state
            .active
            .get(&request.session_id)
            .or_else(|| state.pending.get(&request.session_id))
            .context("session unavailable")?;
        ensure!(
            t.digest()? == request.transcript_sha256,
            "close transcript mismatch"
        );
        let grant = authorized(&state.grants, &t.intent.challenge)?;
        verify(
            &grant.device_public_key,
            &close_bytes(&request.session_id, &request.transcript_sha256)?,
            &request.signature,
        )
    }
}
pub struct AcceptedOffer {
    intent: OfferIntent,
}
fn authorized<'a>(grants: &'a BTreeMap<String, Grant>, c: &Challenge) -> Result<&'a Grant> {
    let g = grants
        .get(&format!("{}:{}", c.device_id, c.vm_id))
        .context("grant revoked")?;
    ensure!(
        g.host_id == c.host_id
            && g.version == c.grant_version
            && c.scopes.iter().all(|s| g.scopes.contains(s)),
        "grant changed or scope denied"
    );
    Ok(g)
}
fn prune(state: &mut GateState, now: u64) {
    state
        .challenges
        .retain(|_, c| now < c.expires_at && now >= c.issued_at);
    state
        .pending
        .retain(|_, t| now < t.intent.challenge.expires_at && now >= t.intent.challenge.issued_at);
}
#[cfg(test)]
mod bounds_tests;
