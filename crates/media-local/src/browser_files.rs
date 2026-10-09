//! Independently authorized files on a pinned Guest source. No desktop lease or input authority.
use super::{now, Auth};
use crate::control_channel::FailureCode;
use crate::guest_binding::Source;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{watch, Mutex};
use vm_session_auth::{
    self as protocol,
    control::{ControlAction, SignedControl},
    files::FileAction,
};
const OP_DEADLINE: Duration = Duration::from_secs(10);
const END_DEADLINE: Duration = Duration::from_secs(10);
pub struct SessionFiles {
    auth: Arc<Auth>,
    source: Arc<Source>,
    session_id: String,
    transcript: String,
    device: String,
    owner: String,
    sequence: Mutex<u64>,
    closing: AtomicBool,
    file_request_started: AtomicBool,
    business_failure_logged: AtomicBool,
    cancel: watch::Sender<bool>,
    cleanup: std::sync::Mutex<Option<watch::Receiver<Option<bool>>>>,
}
impl SessionFiles {
    pub async fn attach(
        auth: Arc<Auth>,
        session_id: &str,
        source: Arc<Source>,
    ) -> Result<Arc<Self>> {
        auth.reload()?;
        auth.gate.authorize_active(session_id, "files")?;
        ensure!(
            auth.files_enabled && source.vm_id() == auth.vm_id,
            "file source mismatch"
        );
        source.validate().await?;
        let (transcript, device) = {
            let proofs = auth
                .close_proofs
                .lock()
                .map_err(|_| anyhow::anyhow!("proof unavailable"))?;
            let proof = proofs.get(session_id).context("proof unavailable")?;
            (proof.transcript.digest()?, proof.device_public_key.clone())
        };
        let caps = source
            .file_rpc("capabilities", json!({}), Duration::from_secs(3))
            .await?;
        ensure!(
            caps["result"]["control_session_fencing"]["retired_owner_ttl_seconds"] == 120
                && caps["result"]["control_session_fencing"]["request_dispatch_deadline_seconds"]
                    == 30,
            "controller retirement unsupported"
        );
        auth.reload()?;
        auth.gate.authorize_active(session_id, "files")?;
        let (cancel, _) = watch::channel(false);
        Ok(Arc::new(Self {
            auth,
            source,
            session_id: session_id.into(),
            transcript,
            device,
            owner: protocol::random32(),
            sequence: Mutex::new(0),
            closing: AtomicBool::new(false),
            file_request_started: AtomicBool::new(false),
            business_failure_logged: AtomicBool::new(false),
            cancel,
            cleanup: std::sync::Mutex::new(None),
        }))
    }
    pub async fn handle(&self, signed: SignedControl) -> Result<Value> {
        ensure!(
            !self.closing.load(Ordering::Acquire),
            FailureCode::SessionEnded
        );
        let mut sequence = self.sequence.try_lock().context(FailureCode::Busy)?;
        let request = &signed.request;
        let ControlAction::File { operation } = &request.action else {
            anyhow::bail!("not a file operation")
        };
        request
            .check_time(now()?)
            .context(FailureCode::RequestExpired)?;
        ensure!(
            request.session_id == self.session_id && request.transcript_sha256 == self.transcript,
            FailureCode::RequestBinding
        );
        protocol::verify(
            &self.device,
            &request.canonical().context(FailureCode::Malformed)?,
            &signed.signature,
        )
        .context(FailureCode::Signature)?;
        self.authorize().context(FailureCode::FileAuthorization)?;
        ensure!(request.sequence > *sequence, FailureCode::Replay);
        *sequence = request.sequence;
        let mut cancel = self.cancel.subscribe();
        let work = tokio::time::timeout(OP_DEADLINE, async {
            self.source
                .validate()
                .await
                .context(FailureCode::FileSource)?;
            self.authorize().context(FailureCode::FileAuthorization)?;
            ensure!(
                request.expires_at > now()? && !self.closing.load(Ordering::Acquire),
                FailureCode::RequestExpired
            );
            let result = self
                .dispatch(operation)
                .await
                .context(FailureCode::FileBackend)?;
            self.source
                .validate()
                .await
                .context(FailureCode::FileSource)?;
            self.authorize().context(FailureCode::FileAuthorization)?;
            ensure!(
                !self.closing.load(Ordering::Acquire),
                FailureCode::SessionEnded
            );
            Ok::<_, anyhow::Error>(result)
        });
        let result = tokio::select! {result=work=>result.context(FailureCode::DispatchTimeout)??,_=async{if *cancel.borrow(){return}let _=cancel.changed().await;}=>anyhow::bail!("file operation cancelled")};
        Ok(json!({"sequence":request.sequence,"result":result}))
    }
    fn authorize(&self) -> Result<()> {
        self.auth.reload()?;
        self.auth.gate.authorize_active(&self.session_id, "files")?;
        Ok(())
    }
    async fn dispatch(&self, action: &FileAction) -> Result<Value> {
        let mut args = json!({"id":self.auth.vm_id,"owner":self.owner});
        let method = match action {
            FileAction::Info => "guest_info",
            FileAction::UploadBegin { path, size, sha256 } => {
                args["path"] = json!(path);
                args["size"] = json!(size);
                args["sha256"] = json!(sha256);
                "file_upload_begin"
            }
            FileAction::UploadWrite {
                token,
                offset,
                data,
            } => {
                args["token"] = json!(token);
                args["offset"] = json!(offset);
                args["data"] = json!(data);
                "file_upload_write"
            }
            FileAction::UploadCommit { token } => {
                args["token"] = json!(token);
                "file_upload_commit"
            }
            FileAction::DownloadBegin { path } => {
                args["path"] = json!(path);
                "file_download_begin"
            }
            FileAction::DownloadRead { token, offset } => {
                args["token"] = json!(token);
                args["offset"] = json!(offset);
                "file_download_read"
            }
            FileAction::Cancel { token } => {
                args["token"] = json!(token);
                "file_cancel"
            }
        };
        if matches!(action, FileAction::Info) {
            args = json!({"id":self.auth.vm_id});
        }
        if !matches!(action, FileAction::Info) {
            self.file_request_started.store(true, Ordering::Release);
        }
        let reply = self
            .source
            .file_rpc(method, args, Duration::from_secs(7))
            .await?;
        let result = normalize(action, reply)?;
        if result["ok"] == false && !self.business_failure_logged.swap(true, Ordering::AcqRel) {
            // normalize emits only fixed allowlisted codes, never Guest text.
            eprintln!(
                "file operation business_failure code={}",
                result["error"]["code"].as_str().unwrap_or("io_error")
            );
        }
        Ok(result)
    }
    /// A spawned owned cleanup survives cancellation of a caller waiting for close.
    /// Its first outcome is retained, including failure; a second empty cleanup cannot erase it.
    pub async fn end(self: &Arc<Self>) -> Result<()> {
        self.closing.store(true, Ordering::Release);
        let _ = self.cancel.send(true);
        let mut receipt = {
            let mut retained = self
                .cleanup
                .lock()
                .map_err(|_| anyhow::anyhow!("cleanup state unavailable"))?;
            if retained.is_none() {
                let (sender, receiver) = watch::channel(None);
                let this = self.clone();
                tokio::spawn(async move {
                    let clean = tokio::time::timeout(END_DEADLINE, async {
                        let result = this
                            .source
                            .file_rpc("session_end", json!({"owner":this.owner}), END_DEADLINE)
                            .await?;
                        validate_cleanup(&result["result"])?;
                        ensure!(
                            !this.file_request_started.load(Ordering::Acquire)
                                || result["result"]["file_cancellation"]["attempted"] == 1,
                            "file owner route cleanup missing"
                        );
                        Ok::<_, anyhow::Error>(())
                    })
                    .await
                    .is_ok_and(|v| v.is_ok());
                    let _ = sender.send(Some(clean));
                });
                *retained = Some(receiver);
            }
            retained.as_ref().unwrap().clone()
        };
        let clean = loop {
            if let Some(clean) = *receipt.borrow() {
                break clean;
            }
            if receipt.changed().await.is_err() {
                break false;
            }
        };
        ensure!(clean, "file session cleanup unconfirmed");
        Ok(())
    }
}
fn normalize(action: &FileAction, reply: Value) -> Result<Value> {
    if let Some(error) = reply.get("error") {
        let text = error.as_str().context("invalid controller error")?;
        let guest: Value = serde_json::from_str(
            text.strip_prefix("guest: ")
                .context("controller file request failed")?,
        )
        .context("invalid Guest error")?;
        let code = guest["code"].as_str().context("invalid Guest error code")?;
        const CODES: &[&str] = &[
            "already_exists",
            "not_found",
            "invalid_request",
            "integrity_error",
            "changed",
            "busy",
            "unavailable",
            "timeout",
            "io_error",
            "cleanup_failed",
            "durability_unknown",
            "permission_denied",
            "unsupported",
            "forbidden",
        ];
        return Ok(
            json!({"ok":false,"error":{"code":if CODES.contains(&code){code}else{"io_error"}}}),
        );
    }
    let value = reply
        .get("result")
        .and_then(|v| v.get("result"))
        .context("invalid Guest file reply")?;
    ensure!(
        reply["result"].get("error").is_none(),
        "invalid Guest success envelope"
    );
    let fields: &[&str] = match action {
        FileAction::Info => {
            ensure!(
                value["capabilities"]["files"] == true,
                "Guest files unsupported"
            );
            let policy = &value["file_policy"];
            ensure!(
                policy["max_file_bytes"] == 8388608
                    && policy["chunk_bytes"] == 32768
                    && policy["idle_timeout_seconds"] == 60
                    && policy["absolute_timeout_seconds"] == 300
                    && ["path_scope", "ownership"]
                        .iter()
                        .all(|k| policy[*k].as_str().is_some_and(|v| v.len() <= 256)),
                "Guest file limits incompatible"
            );
            return Ok(
                json!({"ok":true,"value":pick(policy,&["max_file_bytes","chunk_bytes","idle_timeout_seconds","absolute_timeout_seconds","path_scope","ownership"])?}),
            );
        }
        FileAction::UploadBegin { .. } | FileAction::DownloadBegin { .. } => &[
            "token",
            "size",
            "sha256",
            "chunk_bytes",
            "idle_timeout_seconds",
        ],
        FileAction::UploadWrite { .. } => &["offset"],
        FileAction::UploadCommit { .. } => &["published", "size", "sha256"],
        FileAction::DownloadRead { .. } => &["data", "offset", "eof", "sha256"],
        FileAction::Cancel { .. } => &["cancelled"],
    };
    validate_value(action, value)?;
    Ok(json!({"ok":true,"value":pick(value,fields)?}))
}
fn validate_value(action: &FileAction, v: &Value) -> Result<()> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let number = |key: &str| {
        v[key]
            .as_u64()
            .filter(|n| *n <= protocol::files::MAX_FILE_BYTES)
            .context("invalid file integer")
    };
    let digest = || protocol::unhex(v["sha256"].as_str().context("missing digest")?, 32);
    match action {
        FileAction::UploadBegin { .. } | FileAction::DownloadBegin { .. } => {
            // Identity checks for upload are applied separately below.
            protocol::unhex(v["token"].as_str().context("missing token")?, 16)?;
            number("size")?;
            digest()?;
            ensure!(
                v["chunk_bytes"] == 32768 && v["idle_timeout_seconds"] == 60,
                "invalid transfer policy"
            );
        }
        FileAction::UploadWrite { offset, data, .. } => ensure!(
            number("offset")? == offset + STANDARD.decode(data)?.len() as u64,
            "invalid upload offset"
        ),
        FileAction::UploadCommit { .. } => {
            ensure!(v["published"] == true, "publication unconfirmed");
            number("size")?;
            digest()?;
        }
        FileAction::DownloadRead { offset, .. } => {
            let encoded = v["data"].as_str().context("missing chunk")?;
            ensure!(encoded.len() <= 43692, "oversized chunk");
            let data = STANDARD.decode(encoded)?;
            let eof = v["eof"].as_bool().context("missing EOF")?;
            ensure!(
                data.len() <= 32768
                    && (eof || !data.is_empty())
                    && STANDARD.encode(&data) == encoded
                    && number("offset")? == offset + data.len() as u64,
                "invalid download chunk"
            );
            digest()?;
        }
        FileAction::Cancel { .. } => ensure!(v["cancelled"] == true, "cancellation unconfirmed"),
        FileAction::Info => {}
    }
    if let FileAction::UploadBegin { size, sha256, .. } = action {
        ensure!(
            v["size"] == *size && v["sha256"] == *sha256,
            "upload identity mismatch"
        );
    }
    Ok(())
}
fn pick(value: &Value, fields: &[&str]) -> Result<Value> {
    let mut out = serde_json::Map::new();
    for key in fields {
        out.insert(
            (*key).into(),
            value.get(*key).context("missing Guest file field")?.clone(),
        );
    }
    Ok(Value::Object(out))
}
fn validate_cleanup(v: &Value) -> Result<()> {
    ensure!(
        v["closed"] == true && v["retired_owner"] == true && v["retired_owner_ttl_seconds"] == 120,
        "owner retirement unconfirmed"
    );
    ensure!(
        v["job_cancellation"]["attempted"] == 0
            && v["job_cancellation"]["unconfirmed_requests"] == 0
            && v["job_cancellation"]["outcomes"]
                .as_array()
                .is_some_and(|v| v.is_empty()),
        "unexpected file-owner jobs"
    );
    let files = &v["file_cancellation"];
    let outcomes = files["outcomes"]
        .as_array()
        .context("missing file cleanup outcomes")?;
    ensure!(
        files["unconfirmed_requests"] == 0
            && files["attempted"].as_u64() == Some(outcomes.len() as u64)
            && outcomes.iter().all(|v| v.get("error").is_none()
                && v["result"].get("error").is_none()
                && v["result"]["result"]["cancelled_transfers"]
                    .as_u64()
                    .is_some()),
        "Guest file cleanup unconfirmed"
    );
    ensure!(
        v.get("input_release").is_none(),
        "unexpected file-owner input"
    );
    Ok(())
}

#[cfg(test)]
#[path = "browser_files_tests.rs"]
mod tests;
