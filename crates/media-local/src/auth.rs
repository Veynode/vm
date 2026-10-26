//! Authenticated signaling for the loopback integration path. No internet listener.
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};
use vm_session_auth::{self as protocol, Identity};
#[path = "browser_files.rs"]
pub mod browser_files;
#[path = "browser_input.rs"]
pub mod browser_input;

pub struct Auth {
    pub host_public_key: String,
    pub vm_id: String,
    pub control_state: Option<PathBuf>,
    pub files_enabled: bool,
    pub ice_config: Option<Arc<crate::ice_config::IceSource>>,
    pub gate: protocol::SessionGate,
    state: PathBuf,
    namespace: String,
    close_proofs: std::sync::Mutex<BTreeMap<String, CloseProof>>,
    registry_read: std::sync::Mutex<()>,
    #[cfg(test)]
    test_identity: Option<Arc<dyn Identity + Send + Sync>>,
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
pub fn unix_now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
fn now() -> Result<u64> {
    unix_now()
}
impl Auth {
    fn load_registry(&self) -> Result<protocol::store::Registry> {
        // Store uses a nonblocking exclusive flock, including for validation reads.
        // Serialize this process's readers; external admin writers still fail closed.
        // Never hold this guard across Keychain operations or an await point.
        let _reader = self
            .registry_read
            .lock()
            .map_err(|_| anyhow!("registry reader unavailable"))?;
        let store = protocol::store::Store::open(&self.state)?;
        let registry = store.load();
        // In edition2021 a tail-expression temporary can outlive local guards.
        // Release flock before releasing registry_read, including a load error.
        drop(store);
        registry
    }
    fn sign_ice(
        &self,
        request: protocol::ice::IceRequest,
        config: protocol::ice::IceConfig,
    ) -> Result<protocol::ice::SignedIceConfig> {
        #[cfg(test)]
        if let Some(identity) = &self.test_identity {
            return protocol::ice::SignedIceConfig::new(
                request,
                config,
                &TestSigner(identity.as_ref()),
            );
        }
        #[cfg(target_os = "macos")]
        {
            let identity = protocol::keychain::KeychainIdentity::open(&self.namespace)?;
            protocol::ice::SignedIceConfig::new(request, config, &identity)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (request, config);
            bail!("host key adapter unavailable on this platform")
        }
    }
    fn sign_rejection(
        &self,
        intent: protocol::OfferIntent,
        code: &str,
    ) -> Result<protocol::rejection::SignedOfferRejection> {
        #[cfg(test)]
        if let Some(identity) = &self.test_identity {
            return protocol::rejection::sign(intent, code, &TestSigner(identity.as_ref()));
        }
        #[cfg(target_os = "macos")]
        {
            let identity = protocol::keychain::KeychainIdentity::open(&self.namespace)?;
            protocol::rejection::sign(intent, code, &identity)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (intent, code);
            bail!("host key adapter unavailable on this platform")
        }
    }
    pub fn reload(&self) -> Result<Vec<String>> {
        let registry = self.load_registry()?;
        anyhow::ensure!(
            registry.host_public_key == self.host_public_key,
            "host identity changed"
        );
        self.gate.replace_grants(
            &self.host_public_key,
            &registry.grants.into_values().collect::<Vec<_>>(),
        )
    }
    pub fn sign_answer(
        &self,
        accepted: protocol::AcceptedOffer,
        id: &str,
        sdp: &str,
    ) -> Result<protocol::SignedAnswer> {
        #[cfg(test)]
        if let Some(identity) = &self.test_identity {
            return self.sign_answer_with(accepted, id, sdp, &TestSigner(identity.as_ref()));
        }
        #[cfg(target_os = "macos")]
        {
            let identity = protocol::keychain::KeychainIdentity::open(&self.namespace)?;
            self.sign_answer_with(accepted, id, sdp, &identity)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (accepted, id, sdp);
            bail!("host key adapter unavailable on this platform")
        }
    }
    #[cfg(any(test, target_os = "macos"))]
    fn sign_answer_with(
        &self,
        accepted: protocol::AcceptedOffer,
        id: &str,
        sdp: &str,
        identity: &impl Identity,
    ) -> Result<protocol::SignedAnswer> {
        anyhow::ensure!(
            identity.public_key()? == self.host_public_key,
            "host identity changed"
        );
        let signed = self
            .gate
            .answer(accepted, id.to_owned(), sdp, identity, now()?)?;
        // The session id takes over the deadline of the credential that admitted the offer.
        if let Some(source) = &self.ice_config {
            source.bind_session(&signed.transcript.intent.request_id, id)?;
        }
        let registry = self.load_registry()?;
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
    /// The single teardown for an offer's session. Its signed authority ends first, then its
    /// admitted credential is released, so the admission slot is free once the session ends.
    pub fn teardown_session(&self, request_id: &str, session_id: &str) {
        self.gate.close(session_id);
        if let Some(source) = &self.ice_config {
            // A poisoned entry table already refuses every admission and deadline check.
            let _ = source.release(session_id);
            let _ = source.release(request_id);
        }
    }
}
/// Startup and restart construction. Tests call this same helper with an in-memory registry.
#[cfg(any(test, target_os = "macos"))]
fn gate_for_registry(
    host_public_key: &str,
    registry: &protocol::store::Registry,
) -> Result<protocol::SessionGate> {
    protocol::SessionGate::with_versions(
        host_public_key,
        &registry.grants.values().cloned().collect::<Vec<_>>(),
        &registry.versions,
    )
}
pub fn from_args(args: &[String]) -> Result<Option<Arc<Auth>>> {
    let mut state = None;
    let mut namespace = None;
    let mut vm = None;
    let mut control_state = None;
    let mut ice_path = None;
    let mut files_enabled = false;
    let mut i = 0;
    while i < args.len() {
        let slot = match args[i].as_str() {
            "--allow-files" => {
                anyhow::ensure!(!files_enabled, "duplicate option");
                files_enabled = true;
                i += 1;
                continue;
            }
            "--fault-drop-first-keyframe" | "--fault-suppress-hevc-frames" | "--guest-stream" => {
                i += 1;
                continue;
            }
            "--auth-state" => &mut state,
            "--auth-namespace" => &mut namespace,
            "--vm" => &mut vm,
            "--control-state" => &mut control_state,
            "--ice-config" => &mut ice_path,
            _ => bail!("unknown option"),
        };
        anyhow::ensure!(slot.is_none(), "duplicate option");
        *slot = Some(args.get(i + 1).context("missing option value")?.clone());
        i += 2;
    }
    if state.is_none()
        && namespace.is_none()
        && vm.is_none()
        && control_state.is_none()
        && ice_path.is_none()
        && !files_enabled
    {
        return Ok(None);
    }
    let state = PathBuf::from(state.context("--auth-state required")?);
    let namespace = namespace.context("--auth-namespace required")?;
    let vm_id = vm.context("--vm required")?;
    let control_state = control_state
        .map(PathBuf::from)
        .map(|p| p.canonicalize())
        .transpose()?;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (state, namespace, vm_id, control_state, ice_path);
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
        let gate = gate_for_registry(&host_public_key, &registry)?;
        Ok(Some(Arc::new(Auth {
            host_public_key,
            vm_id,
            control_state,
            files_enabled,
            ice_config: ice_path
                .map(|path| crate::ice_config::IceSource::open(path.into(), now()?))
                .transpose()?
                .map(Arc::new),
            gate,
            state,
            namespace,
            close_proofs: std::sync::Mutex::new(BTreeMap::new()),
            registry_read: std::sync::Mutex::new(()),
            #[cfg(test)]
            test_identity: None,
        })))
    }
}
fn permitted(app: &App, headers: &HeaderMap) -> Result<Arc<Auth>> {
    check(app, headers, true).map_err(|_| anyhow!("invalid request origin"))?;
    app.auth.clone().context("authentication not configured")
}
fn scopes_permitted(auth: &Auth, scopes: &[String]) -> bool {
    scopes == ["observe"]
        || (auth.control_state.is_some() && scopes == ["input", "observe"])
        || (auth.files_enabled && scopes == ["files", "observe"])
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
            req.vm_id == a.vm_id && scopes_permitted(&a, &req.scopes),
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
/// Bootstrap needs its own signed, single-use challenge before creating SDP.
/// No long-term minting key is accepted or returned here.
pub async fn ice_configuration(
    State(app): State<App>,
    headers: HeaderMap,
    Json(signed): Json<protocol::ice::SignedIceRequest>,
) -> Response {
    let result = (|| {
        let a = permitted(&app, &headers)?;
        let source = a
            .ice_config
            .as_ref()
            .context("ICE configuration unavailable")?;
        let c = &signed.request.challenge;
        anyhow::ensure!(
            c.vm_id == a.vm_id && scopes_permitted(&a, &c.scopes),
            "wrong VM/scopes"
        );
        a.reload()?;
        let config = source.read(now()?)?;
        a.gate.accept_ice_request(&signed, now()?)?;
        source.admit(&c.device_id)?;
        let response = a.sign_ice(signed.request, config)?;
        a.reload()?;
        a.gate.authorize_ice_grant(&response.request.challenge)?;
        Ok::<_, anyhow::Error>(response)
    })();
    match result {
        Ok(response) => {
            let mut response = Json(response).into_response();
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-store"),
            );
            response
        }
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
                && scopes_permitted(&a, &req.signed_offer.intent.challenge.scopes),
            "wrong VM/scopes"
        );
        a.reload()?;
        let accepted = a.gate.accept_offer(&req.signed_offer, &req.sdp, now()?)?;
        // Admission records the deadline of the ICE credential this session will use.
        // An expired or short-lived credential is refused here, before any session slot.
        if let Some(source) = &a.ice_config {
            source.admit_offer(&req.signed_offer.intent.request_id, now()?)?;
        }
        Ok::<_, anyhow::Error>(accepted)
    })();
    match result {
        Ok(accepted) => {
            let request = req.signed_offer.intent.request_id.clone();
            let response = admitted_offer(&app, req, accepted).await;
            release_unstarted(&app, &request).await;
            response
        }
        Err(_) => denied(),
    }
}
/// Everything after admission. Each refusal here returns before any session slot exists.
async fn admitted_offer(
    app: &App,
    req: SignedOfferRequest,
    accepted: protocol::AcceptedOffer,
) -> Response {
    if let Some(source) = &app.guest_binding {
        // This guard bounds authenticated preflight RPCs/signatures before
        // the existing session reservation. Busy is not a source failure.
        let Ok(_preflight) = source.admit_preflight() else {
            return denied();
        };
        if !rejection_admission(app, &req.signed_offer.intent.request_id, false).await {
            return denied();
        }
        if let Err(error) = source.validate().await {
            if !rejection_admission(app, &req.signed_offer.intent.request_id, true).await {
                return denied();
            }
            let code = guest_binding::source_failure(&error).code;
            let Some(auth) = &app.auth else {
                return denied();
            };
            let signed = match auth.sign_rejection(req.signed_offer.intent, code) {
                Ok(signed) => signed,
                Err(_) => return denied(),
            };
            // The accepted token is dropped here. No reservation, peer,
            // input adapter or encoder has been created for this intent.
            let mut response = Json(json!({"signed_rejection":signed})).into_response();
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-store"),
            );
            return response;
        }
    }
    offer_inner(
        app.clone(),
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
/// An admitted offer that did not start a session keeps no admission entry. A running
/// session keeps its entry, and its teardown releases it.
async fn release_unstarted(app: &App, request: &str) {
    let Some(source) = app.auth.as_ref().and_then(|a| a.ice_config.as_ref()) else {
        return;
    };
    let book = app.active.lock().await;
    if !book.active.as_ref().is_some_and(|s| s.request == request) {
        let _ = source.release(request);
    }
}
// A rejection for a new intent must not clear or borrow an existing session's
// unresolved cleanup. Reserve a cancellation tombstone before publishing it.
async fn rejection_admission(app: &App, request: &str, retire: bool) -> bool {
    let mut book = app.active.lock().await;
    book.prune();
    if uuid::Uuid::parse_str(request).is_err()
        || book.stopping
        || book.active.is_some()
        || book.completed.values().any(|(_, clean, _)| !*clean)
        || book.completed.contains_key(request)
        || book.cancelled.contains_key(request)
    {
        return false;
    }
    !retire || book.cancel_id(request)
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
    let mut book = app.active.lock().await;
    if book.stopping {
        return denied();
    }
    let Some(session) = book.active.as_mut().filter(|s| s.id == req.session_id) else {
        return denied();
    };
    let result = (|| {
        a.reload()?;
        if *session.confirmed.borrow() {
            // SessionGate.confirm deliberately consumes only pending sessions.
            // A retry of the HTTP acknowledgement validates the active binding
            // and signature without creating a fresh adapter owner or lease.
            a.gate.authorize_active(&req.session_id, "observe")?;
            let proofs = a
                .close_proofs
                .lock()
                .map_err(|_| anyhow!("session proof unavailable"))?;
            let proof = proofs
                .get(&req.session_id)
                .context("session transcript unavailable")?;
            anyhow::ensure!(
                proof.transcript.digest()? == req.confirmation.transcript_sha256,
                "confirmation transcript mismatch"
            );
            protocol::verify(
                &proof.device_public_key,
                &protocol::confirmation_bytes(&req.confirmation.transcript_sha256)?,
                &req.confirmation.signature,
            )?;
        } else {
            a.gate.confirm(&req.session_id, &req.confirmation, now()?)?;
        }
        Ok::<_, anyhow::Error>(())
    })();
    if result.is_err() {
        return denied();
    }
    if *session.confirmed.borrow() {
        return Json(json!({"confirmed":true})).into_response();
    }
    if let Some(state) = a.control_state.clone() {
        match browser_input::SessionInput::attach(
            a.clone(),
            &req.session_id,
            state,
            app.socket.clone(),
        )
        .await
        {
            Ok(input) => session.input = Some(input),
            Err(_) => {
                a.gate.close(&req.session_id);
                let _ = session.cancel.send(true);
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"error":"console input backend unavailable or incompatible"})),
                )
                    .into_response();
            }
        }
    }
    if a.gate.authorize_active(&req.session_id, "files").is_ok() {
        let attached = match &app.guest_binding {
            Some(source) => {
                browser_files::SessionFiles::attach(a.clone(), &req.session_id, source.clone())
                    .await
            }
            None => Err(anyhow!("strict Guest source required for files")),
        };
        match attached {
            Ok(files) => session.files = Some(files),
            Err(_) => {
                a.gate.close(&req.session_id);
                let _ = session.cancel.send(true);
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"error":"Guest files unavailable"})),
                )
                    .into_response();
            }
        }
    }
    if session.confirmed.send(true).is_err() {
        a.gate.close(&req.session_id);
        let _ = session.cancel.send(true);
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
    // An unresolved cleanup outlives the normal close-receipt window. Snapshot
    // its immutable request/session binding before validating the proof; this
    // exception authorizes only another cleanup attempt, never a new session.
    let unresolved_request = {
        let book = app.active.lock().await;
        book.completed
            .iter()
            .find_map(|(request, (_, clean, session))| {
                (!clean && session == &req.session_id).then(|| request.clone())
            })
    };
    let request_id = (|| {
        let mut proofs = a
            .close_proofs
            .lock()
            .map_err(|_| anyhow!("close proof state unavailable"))?;
        let p = proofs.get_mut(&req.session_id).context("unknown session")?;
        let unresolved = unresolved_request.as_ref() == Some(&p.transcript.intent.request_id);
        anyhow::ensure!(
            (p.issued.elapsed() < Duration::from_secs(240) || unresolved)
                && p.transcript.session_id == req.session_id
                && p.transcript.digest()? == req.transcript_sha256,
            "close transcript mismatch"
        );
        protocol::verify(
            &p.device_public_key,
            &protocol::close_bytes(&req.session_id, &req.transcript_sha256)?,
            &req.signature,
        )?;
        // Recovery's own HTTP acknowledgement can also be lost. Renew only
        // this verified close-only proof so the eventual clean receipt remains
        // retryable; no grant, confirmation or input authority is revived.
        if unresolved {
            p.issued = Instant::now();
        }
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
            if *clean {
                return Json(book.close_receipt(&request_id)).into_response();
            }
            // The bounded recovery path in close_inner must independently
            // prove cleanup; a valid signature is not a cleanup receipt.
        } else if !book
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
        authorization_check(&auth, id, active, now()?)?;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
/// One watch step with an injected clock: registry, grant and credential deadline.
fn authorization_check(auth: &Auth, id: &str, active: bool, now: u64) -> Result<()> {
    // Fail closed on malformed/unreadable/busy registry too. Transport owns actual teardown.
    auth.reload()?;
    if active {
        auth.gate.authorize_active(id, "observe")?;
    } else {
        auth.gate.authorize_session(id, "observe")?;
    }
    // A session may not outlive the ICE credential that admitted it. Failing here takes
    // the same watch arm as a revoked grant, so the existing teardown path ends the session.
    if let Some(source) = &auth.ice_config {
        source.check_session(id, now)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;

#[cfg(test)]
struct TestSigner<'a>(&'a (dyn Identity + Send + Sync));
#[cfg(test)]
impl Identity for TestSigner<'_> {
    fn public_key(&self) -> Result<String> {
        self.0.public_key()
    }
    fn sign(&self, message: &[u8]) -> Result<String> {
        self.0.sign(message)
    }
}
#[cfg(test)]
#[path = "auth_hostile_peer_tests.rs"]
mod hostile_peer_tests;
