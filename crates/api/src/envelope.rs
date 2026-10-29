//! Answers and refusals: `{"result":…}` or `{"error":"…","code":"…"}`.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt;

/// Why the daemon refused a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The arguments are wrong.
    Invalid,
    /// What the call names does not exist.
    NotFound,
    /// It exists but cannot do this now (running, stopped, busy, name in use).
    State,
    /// This daemon, backend or guest cannot do it.
    Unsupported,
    /// Client and daemon share no API version.
    Version,
    /// Anything else.
    Failed,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::NotFound => "not_found",
            Self::State => "state",
            Self::Unsupported => "unsupported",
            Self::Version => "version",
            Self::Failed => "failed",
        }
    }
    /// The code a string names; an unknown one is `Failed`, as clients read it.
    pub fn parse(code: &str) -> Self {
        serde_json::from_value(Value::String(code.into())).unwrap_or(Self::Failed)
    }
}

/// A refusal the daemon answered, or one a handler raises to choose its code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ApiError {}

/// An error carrying `code`, for a handler to return through `anyhow`.
pub fn refuse(code: ErrorCode, message: impl Into<String>) -> anyhow::Error {
    ApiError {
        code,
        message: message.into(),
    }
    .into()
}

/// The code of an error a handler returned: the one it chose with [`refuse`]
/// anywhere in its chain, else one read from the message of an error raised
/// before codes existed.
pub fn code_of(error: &anyhow::Error) -> ErrorCode {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ApiError>().map(|e| e.code))
        .unwrap_or_else(|| classify(&error.to_string()))
}

/// A code for a message without one. The VM layer's errors predate codes;
/// their wording is stable enough to sort them, and anything unrecognised
/// stays `Failed`.
pub fn classify(message: &str) -> ErrorCode {
    let m = message.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| m.contains(w));
    if has(&["unsupported", "not supported", "is not available on"]) {
        ErrorCode::Unsupported
    } else if has(&[
        "unknown vm",
        "not found",
        "no such",
        "no container",
        "no machine",
        "does not exist",
    ]) {
        ErrorCode::NotFound
    } else if has(&[
        "already",
        "in use",
        "is running",
        "not running",
        "is paused",
        "not paused",
        "is hibernating",
        "is stopped",
        "busy",
        "must be stopped",
        "while running",
    ]) {
        ErrorCode::State
    } else if has(&[
        "invalid",
        "missing",
        "must be",
        "required",
        "expected",
        "out of range",
    ]) {
        ErrorCode::Invalid
    } else {
        ErrorCode::Failed
    }
}

/// The answer line for `result`.
pub fn answer(result: &anyhow::Result<Value>) -> Value {
    match result {
        Ok(value) => json!({ "result": value }),
        Err(error) => refusal(error),
    }
}

/// The answer line for a refusal.
pub fn refusal(error: &anyhow::Error) -> Value {
    json!({ "error": error.to_string(), "code": code_of(error).as_str() })
}

/// `{"result"}` as `Ok`, `{"error","code"}` as an [`ApiError`].
pub fn open(mut reply: Value) -> Result<Value, ApiError> {
    if let Some(error) = reply.get("error") {
        let message = match error.as_str() {
            Some(text) => text.to_string(),
            None => error.to_string(),
        };
        let code = match reply.get("code").and_then(Value::as_str) {
            Some(code) => ErrorCode::parse(code),
            None => classify(&message),
        };
        return Err(ApiError { code, message });
    }
    match reply.get_mut("result") {
        Some(value) => Ok(value.take()),
        None => Err(ApiError {
            code: ErrorCode::Failed,
            message: "the daemon gave no answer".into(),
        }),
    }
}

/// What the daemon runs on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Runtime {
    pub apple_virtualization: bool,
    pub qemu: bool,
    /// The GPU accelerations the QEMU runtime can run; absent without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<Gpu>,
}

/// What `gpu` a QEMU machine may ask for on this host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gpu {
    pub virgl: bool,
    pub venus: bool,
    pub neptune: bool,
    /// Why an acceleration is missing, by name.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub reason: std::collections::BTreeMap<String, String>,
}

/// `hello`'s answer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub api: u64,
    /// The daemon's version.
    pub daemon: String,
    pub methods: Vec<String>,
    pub home: Option<String>,
    pub runtime: Runtime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chosen_code_survives_context() {
        let error = refuse(ErrorCode::NotFound, "no machine dev").context("machine.inspect");
        assert_eq!(code_of(&error), ErrorCode::NotFound);
        assert_eq!(refusal(&error)["code"], "not_found");
    }

    #[test]
    fn legacy_messages_are_sorted() {
        assert_eq!(classify("unknown VM dev"), ErrorCode::NotFound);
        assert_eq!(classify("VM dev is running"), ErrorCode::State);
        assert_eq!(
            classify("unsupported operation: frobnicate"),
            ErrorCode::Unsupported
        );
        assert_eq!(
            classify("id must be 1–64 ASCII letters"),
            ErrorCode::Invalid
        );
        assert_eq!(classify("disk read failed"), ErrorCode::Failed);
    }

    #[test]
    fn open_reads_both_forms() {
        assert_eq!(open(json!({"result": 3})), Ok(json!(3)));
        let refused = open(json!({"error":"nope","code":"state"})).unwrap_err();
        assert_eq!(refused.code, ErrorCode::State);
        assert_eq!(
            open(json!({"error":"x","code":"martian"}))
                .unwrap_err()
                .code,
            ErrorCode::Failed
        );
        assert_eq!(
            open(json!({"error":"unknown VM a"})).unwrap_err().code,
            ErrorCode::NotFound
        );
    }
}
