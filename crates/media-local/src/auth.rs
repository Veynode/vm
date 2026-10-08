//! Authenticated signaling for the loopback integration path. No internet listener.
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};
use vm_session_auth::{self as protocol, Identity};

pub struct Auth {
    pub host_public_key: String,
    pub vm_id: String,
    pub gate: protocol::SessionGate,
    state: PathBuf,
    namespace: String,
    close_proofs: std::sync::Mutex<BTreeMap<String, CloseProof>>,
}
struct CloseProof {
    transcript: protocol::AnswerTranscript,
    device_public_key: String,
    issued: Instant,
}
pub struct SessionSetup {
    pub id: String,
    pub accepted: Option<protocol::AcceptedOffer>,
    pub confirmed: watch::Receiver<bool>,
}
fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
impl Auth {
    pub fn reload(&self) -> Result<Vec<String>> {
        let registry = protocol::store::Store::open(&self.state)?.load()?;
        anyhow::ensure!(
            registry.host_public_key == self.host_public_key,
            "host identity changed"
        );
        self.gate.replace_grants(
            &self.host_public_key,
            &registry.grants.into_values().collect::<Vec<_>>(),
        )
    }
    #[cfg(target_os = "macos")]
    pub fn sign_answer(
        &self,
        accepted: protocol::AcceptedOffer,
        id: &str,
        sdp: &str,
    ) -> Result<protocol::SignedAnswer> {
        let identity = protocol::keychain::KeychainIdentity::open(&self.namespace)?;
        anyhow::ensure!(
            identity.public_key()? == self.host_public_key,
            "host identity changed"
        );
        let signed = self
            .gate
            .answer(accepted, id.to_owned(), sdp, &identity, now()?)?;
        let registry = protocol::store::Store::open(&self.state)?.load()?;
        let c = &signed.transcript.intent.challenge;
        let grant = registry
            .grants
            .get(&format!("{}:{}", c.device_id, c.vm_id))
            .context("grant revoked while signing")?;
        anyhow::ensure!(
            grant.grant.version == c.grant_version,
            "grant replaced while signing"
        );
        let mut proofs = self
            .close_proofs
            .lock()
            .map_err(|_| anyhow!("close proof state unavailable"))?;
        proofs.retain(|_, p| p.issued.elapsed() < Duration::from_secs(240));
        anyhow::ensure!(proofs.len() < 256, "close proof capacity reached");
        proofs.insert(
            id.to_owned(),
            CloseProof {
                transcript: signed.transcript.clone(),
                device_public_key: grant.grant.device_public_key.clone(),
                issued: Instant::now(),
            },
        );
        Ok(signed)
    }
    #[cfg(not(target_os = "macos"))]
    pub fn sign_answer(
        &self,
        _: protocol::AcceptedOffer,
        _: &str,
        _: &str,
    ) -> Result<protocol::SignedAnswer> {
        bail!("host key adapter unavailable on this platform")
    }
}
pub fn from_args(args: &[String]) -> Result<Option<Arc<Auth>>> {
    let mut state = None;
    let mut namespace = None;
    let mut vm = None;
    let mut i = 0;
    while i < args.len() {
        let slot = match args[i].as_str() {
            "--fault-drop-first-keyframe" | "--fault-suppress-hevc-frames" | "--guest-stream" => {
                i += 1;
                continue;
            }
            "--auth-state" => &mut state,
            "--auth-namespace" => &mut namespace,
            "--vm" => &mut vm,
            _ => bail!("unknown option"),
        };
        anyhow::ensure!(slot.is_none(), "duplicate option");
        *slot = Some(args.get(i + 1).context("missing option value")?.clone());
        i += 2;
    }
    if state.is_none() && namespace.is_none() && vm.is_none() {
        return Ok(None);
    }
    let state = PathBuf::from(state.context("--auth-state required")?);
    let namespace = namespace.context("--auth-namespace required")?;
    let vm_id = vm.context("--vm required")?;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (state, namespace, vm_id);
        bail!("host key adapter unavailable on this platform")
    }
    #[cfg(target_os = "macos")]
    {
        protocol::keychain::disable_interaction_for_process()?;
        let identity = protocol::keychain::KeychainIdentity::open(&namespace)?;
        let host_public_key = identity.public_key()?;
        let registry = protocol::store::Store::open(&state)?.load()?;
        anyhow::ensure!(
            registry.host_public_key == host_public_key,
            "registry host identity mismatch"
        );
        let gate = protocol::SessionGate::new(
            &host_public_key,
            &registry.grants.into_values().collect::<Vec<_>>(),
        )?;
        Ok(Some(Arc::new(Auth {
            host_public_key,
            vm_id,
            gate,
            state,
            namespace,
            close_proofs: std::sync::Mutex::new(BTreeMap::new()),
        })))
    }
}
fn permitted(app: &App, headers: &HeaderMap) -> Result<Arc<Auth>> {
    check(app, headers, true).map_err(|_| anyhow!("invalid request origin"))?;
    app.auth.clone().context("authentication not configured")
}
fn denied() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({"error":"authorization rejected"})),
    )
        .into_response()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChallengeRequest {
    device_id: String,
    vm_id: String,
    scopes: Vec<String>,
}
pub async fn challenge(
    State(app): State<App>,
    headers: HeaderMap,
    Json(req): Json<ChallengeRequest>,
) -> Response {
    let result = (|| {
        let a = permitted(&app, &headers)?;
        anyhow::ensure!(
            req.vm_id == a.vm_id && req.scopes == ["observe"],
            "wrong VM/scopes"
        );
        a.reload()?;
        a.gate
            .challenge(&req.device_id, &req.vm_id, req.scopes, now()?)
    })();
    match result {
        Ok(v) => Json(v).into_response(),
        Err(_) => denied(),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedOfferRequest {
    signed_offer: protocol::SignedOffer,
    sdp: String,
}
pub async fn offer(
    State(app): State<App>,
    headers: HeaderMap,
    Json(req): Json<SignedOfferRequest>,
) -> Response {
    let result = (|| {
        let a = permitted(&app, &headers)?;
        anyhow::ensure!(
            req.signed_offer.intent.challenge.vm_id == a.vm_id
                && req.signed_offer.intent.challenge.scopes == ["observe"],
            "wrong VM/scopes"
        );
        a.reload()?;
        a.gate.accept_offer(&req.signed_offer, &req.sdp, now()?)
    })();
    match result {
        Ok(accepted) => {
            offer_inner(
                app,
                Offer {
                    request_id: req.signed_offer.intent.request_id,
                    sdp: req.sdp,
                    r#type: "offer".into(),
                    codec: req.signed_offer.intent.codec,
                },
                Some(accepted),
            )
            .await
        }
        Err(_) => denied(),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmRequest {
    session_id: String,
    confirmation: protocol::Confirmation,
}
pub async fn confirm(
    State(app): State<App>,
    headers: HeaderMap,
    Json(req): Json<ConfirmRequest>,
) -> Response {
    let a = match permitted(&app, &headers) {
        Ok(a) => a,
        Err(_) => return denied(),
    };
    let book = app.active.lock().await;
    let Some(session) = book.active.as_ref().filter(|s| s.id == req.session_id) else {
        return denied();
    };
    let result = (|| {
        a.reload()?;
        a.gate.confirm(&req.session_id, &req.confirmation, now()?)?;
        Ok::<_, anyhow::Error>(())
    })();
    if result.is_err() {
        return denied();
    }
    if session.confirmed.send(true).is_err() {
        return denied();
    }
    Json(json!({"confirmed":true})).into_response()
}
pub async fn close(
    State(app): State<App>,
    headers: HeaderMap,
    Json(req): Json<protocol::SignedClose>,
) -> Response {
    let a = match permitted(&app, &headers) {
        Ok(a) => a,
        Err(_) => return denied(),
    };
    // Closing only reduces authority. Keep a bounded signed-transcript proof so
    // peer-disconnect/revocation cleanup racing this request remains attestable.
    let request_id = (|| {
        let proofs = a
            .close_proofs
            .lock()
            .map_err(|_| anyhow!("close proof state unavailable"))?;
        let p = proofs.get(&req.session_id).context("unknown session")?;
        anyhow::ensure!(
            p.issued.elapsed() < Duration::from_secs(240)
                && p.transcript.digest()? == req.transcript_sha256,
            "close transcript mismatch"
        );
        protocol::verify(
            &p.device_public_key,
            &protocol::close_bytes(&req.session_id, &req.transcript_sha256)?,
            &req.signature,
        )?;
        Ok::<_, anyhow::Error>(p.transcript.intent.request_id.clone())
    })();
    let request_id = match request_id {
        Ok(id) => id,
        Err(_) => return denied(),
    };
    {
        let book = app.active.lock().await;
        if let Some((_, clean, id)) = book.completed.get(&request_id) {
            if id != &req.session_id {
                return denied();
            }
            return if *clean {
                Json(json!({"closed":true})).into_response()
            } else {
                (StatusCode::GATEWAY_TIMEOUT, "cleanup unconfirmed").into_response()
            };
        }
        if !book
            .active
            .as_ref()
            .is_some_and(|s| s.id == req.session_id && s.request == request_id)
        {
            return denied();
        }
    }
    close_inner(
        &app,
        Close {
            request_id,
            session_id: Some(req.session_id),
        },
    )
    .await
}

pub async fn watch_authorization(auth: Option<Arc<Auth>>, id: &str, active: bool) -> Result<()> {
    let Some(auth) = auth else {
        return std::future::pending().await;
    };
    loop {
        // Fail closed on malformed/unreadable/busy registry too. Transport owns actual teardown.
        auth.reload()?;
        if active {
            auth.gate.authorize_active(id, "observe")?;
        } else {
            auth.gate.authorize_session(id, "observe")?;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
