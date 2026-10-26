//! Host side of the remote desktop handshake (docs/remote-desktop.md
//! "Handshake"). Before keys exist, the reliable path carries plaintext
//! handshake units `u8 7 | JSON`; every later unit is a secure record. The
//! browser signs an offer that binds the session-auth challenge, its
//! ephemeral ECDH key, the outer transport and the page origin; the host
//! answers with its own ephemeral key under the Keychain host key; the
//! browser confirms the transcript digest with its device key. Keys are
//! derived from the ECDH secret salted with that digest, so the gateway can
//! neither read nor splice the session.
use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use vm_remote_proto::record::{close, AUTH};
use vm_remote_proto::secure::{Ephemeral, SessionKeys};
use vm_session_auth::remote::{
    AcceptedRemoteOffer, RemoteAnswerParams, SignedRemoteAnswer, SignedRemoteOffer, CODECS,
};
use vm_session_auth::{hex, unhex, Challenge, Confirmation, SessionGate};

/// Browser to host.
#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrowserMessage {
    Hello {
        proto: u32,
        device_id: String,
        vm_id: String,
        scopes: Vec<String>,
    },
    Offer {
        offer: Box<SignedRemoteOffer>,
    },
    Confirm {
        session_id: String,
        confirmation: Confirmation,
    },
}

/// Host to browser.
#[derive(Debug, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum HostMessage {
    Challenge { challenge: Box<Challenge> },
    Answer { answer: Box<SignedRemoteAnswer> },
    Error { code: u16, message: String },
}

pub fn encode(message: &HostMessage) -> Result<Vec<u8>> {
    let mut out = vec![AUTH];
    out.extend(serde_json::to_vec(message)?);
    Ok(out)
}

pub fn decode(unit: &[u8]) -> Result<BrowserMessage> {
    ensure!(unit.first() == Some(&AUTH), "expected a handshake unit");
    ensure!(
        unit.len() <= vm_remote_proto::record::MAX_JSON + 1,
        "handshake unit too large"
    );
    Ok(serde_json::from_slice(&unit[1..])?)
}

/// Signs with the host key: the Keychain in production, a test key in tests.
pub trait HostIdentity: Send + Sync {
    fn answer(
        &self,
        gate: &SessionGate,
        accepted: AcceptedRemoteOffer,
        params: RemoteAnswerParams,
        now: u64,
    ) -> Result<SignedRemoteAnswer>;
}

/// A confirmed session.
pub struct Established {
    pub session_id: String,
    pub vm_id: String,
    pub scopes: Vec<String>,
    /// `h264` or `h265`, as offered.
    pub codec: String,
    pub keys: SessionKeys,
    pub expires_at: u64,
}

impl std::fmt::Debug for Established {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Established")
            .field("session_id", &self.session_id)
            .field("vm_id", &self.vm_id)
            .field("codec", &self.codec)
            .finish_non_exhaustive()
    }
}

pub enum Next {
    Send(HostMessage),
    Established(Established),
}

enum Stage {
    Hello,
    Challenged,
    Answered {
        session_id: String,
        established: Established,
    },
    Done,
}

/// One browser's handshake, fed one message at a time. Any error ends the
/// route; the caller sends [`HostMessage::Error`] with [`close::UNAUTHORIZED`].
pub struct Handshake<'a> {
    gate: &'a SessionGate,
    host: &'a dyn HostIdentity,
    /// What the gateway reported for this route; the browser signed its own.
    transport: String,
    origin: String,
    lifetime_seconds: u64,
    session_id: String,
    stage: Stage,
}

impl<'a> Handshake<'a> {
    pub fn new(
        gate: &'a SessionGate,
        host: &'a dyn HostIdentity,
        transport: &str,
        origin: &str,
        lifetime_seconds: u64,
        session_id: String,
    ) -> Self {
        Self {
            gate,
            host,
            transport: transport.into(),
            origin: origin.into(),
            lifetime_seconds,
            session_id,
            stage: Stage::Hello,
        }
    }

    pub fn step(&mut self, message: BrowserMessage, now: u64) -> Result<Next> {
        match (std::mem::replace(&mut self.stage, Stage::Done), message) {
            (
                Stage::Hello,
                BrowserMessage::Hello {
                    proto,
                    device_id,
                    vm_id,
                    scopes,
                },
            ) => {
                ensure!(
                    proto == vm_remote_proto::PROTOCOL,
                    "protocol {proto} unsupported"
                );
                let challenge = self.gate.challenge(&device_id, &vm_id, scopes, now)?;
                self.stage = Stage::Challenged;
                Ok(Next::Send(HostMessage::Challenge {
                    challenge: Box::new(challenge),
                }))
            }
            (Stage::Challenged, BrowserMessage::Offer { offer }) => {
                let accepted =
                    self.gate
                        .accept_remote_offer(&offer, &self.transport, &self.origin, now)?;
                let intent = accepted.intent().clone();
                let codec = choose_codec(&intent.codecs)?;
                let ephemeral = Ephemeral::generate()?;
                let host_ecdh = hex(&ephemeral.public_key()?);
                let answer = self.host.answer(
                    self.gate,
                    accepted,
                    RemoteAnswerParams {
                        session_id: self.session_id.clone(),
                        host_ecdh,
                        codec: codec.into(),
                        lifetime_seconds: self.lifetime_seconds,
                    },
                    now,
                )?;
                let digest: [u8; 32] = unhex(&answer.transcript.digest()?, 32)?
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("digest length"))?;
                let keys = ephemeral.derive(&unhex(&intent.browser_ecdh, 65)?, &digest)?;
                let challenge = &intent.challenge;
                self.stage = Stage::Answered {
                    session_id: self.session_id.clone(),
                    established: Established {
                        session_id: self.session_id.clone(),
                        vm_id: challenge.vm_id.clone(),
                        scopes: challenge.scopes.clone(),
                        codec: codec.into(),
                        keys,
                        expires_at: answer.transcript.expires_at,
                    },
                };
                Ok(Next::Send(HostMessage::Answer {
                    answer: Box::new(answer),
                }))
            }
            (
                Stage::Answered {
                    session_id,
                    established,
                },
                BrowserMessage::Confirm {
                    session_id: confirmed,
                    confirmation,
                },
            ) => {
                ensure!(confirmed == session_id, "confirmation for another session");
                self.gate.remote_confirm(&session_id, &confirmation, now)?;
                Ok(Next::Established(established))
            }
            (_, m) => bail!("unexpected handshake message {}", kind(&m)),
        }
    }
}

fn kind(m: &BrowserMessage) -> &'static str {
    match m {
        BrowserMessage::Hello { .. } => "hello",
        BrowserMessage::Offer { .. } => "offer",
        BrowserMessage::Confirm { .. } => "confirm",
    }
}

/// HEVC when the browser decodes it (less bandwidth for the same picture),
/// else H.264.
pub fn choose_codec(offered: &[String]) -> Result<&'static str> {
    for preferred in ["h265", "h264"] {
        if offered.iter().any(|c| c == preferred) {
            return Ok(CODECS.iter().find(|c| **c == preferred).unwrap());
        }
    }
    bail!("no common codec")
}

/// The encoder's name for a session codec.
pub fn encoder_codec(codec: &str) -> &'static str {
    if codec == "h265" {
        "hevc"
    } else {
        "h264"
    }
}

pub fn refusal(error: &anyhow::Error) -> HostMessage {
    HostMessage::Error {
        code: close::UNAUTHORIZED,
        // Enough for the browser to tell expired from denied, nothing more.
        message: format!("{error}").chars().take(200).collect(),
    }
}

pub fn session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
