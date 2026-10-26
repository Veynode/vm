//! Bounded file operations in the independently granted Guest files directory.
use super::{unhex, Canon};
use anyhow::{ensure, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const CHUNK_BYTES: usize = 32768;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileAction {
    Info,
    UploadBegin {
        path: String,
        size: u64,
        sha256: String,
    },
    UploadWrite {
        token: String,
        offset: u64,
        data: String,
    },
    UploadCommit {
        token: String,
    },
    DownloadBegin {
        path: String,
    },
    DownloadRead {
        token: String,
        offset: u64,
    },
    Cancel {
        token: String,
    },
}
fn filename(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 255
            && ![".", ".."].contains(&path)
            && !path.starts_with(".veynode-")
            && !path.bytes().any(|b| [0, 47, 92].contains(&b)),
        "invalid file name"
    );
    Ok(())
}
impl FileAction {
    pub(crate) fn fields(&self, c: &mut Canon) -> Result<()> {
        match self {
            Self::Info => c.text("info"),
            Self::UploadBegin { path, size, sha256 } => {
                filename(path)?;
                ensure!(*size <= MAX_FILE_BYTES, "file too large");
                unhex(sha256, 32)?;
                c.text("upload_begin");
                c.text(path);
                c.int(*size);
                c.text(sha256);
            }
            Self::UploadWrite {
                token,
                offset,
                data,
            } => {
                unhex(token, 16)?;
                ensure!(
                    *offset <= MAX_FILE_BYTES && data.len() <= 43692,
                    "invalid file chunk"
                );
                let decoded = STANDARD.decode(data)?;
                ensure!(
                    !decoded.is_empty()
                        && decoded.len() <= CHUNK_BYTES
                        && STANDARD.encode(&decoded) == *data
                        && *offset + decoded.len() as u64 <= MAX_FILE_BYTES,
                    "invalid file chunk"
                );
                c.text("upload_write");
                c.text(token);
                c.int(*offset);
                c.text(data);
            }
            Self::UploadCommit { token } => {
                unhex(token, 16)?;
                c.text("upload_commit");
                c.text(token);
            }
            Self::DownloadBegin { path } => {
                filename(path)?;
                c.text("download_begin");
                c.text(path);
            }
            Self::DownloadRead { token, offset } => {
                unhex(token, 16)?;
                ensure!(*offset <= MAX_FILE_BYTES, "invalid file offset");
                c.text("download_read");
                c.text(token);
                c.int(*offset);
            }
            Self::Cancel { token } => {
                unhex(token, 16)?;
                c.text("cancel");
                c.text(token);
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn valid(action: FileAction) -> bool {
        action.fields(&mut Canon::new("control")).is_ok()
    }
    #[test]
    fn names_and_chunks_are_bounded_by_bytes() {
        for path in [
            "../a",
            "/\u{301}",
            "\\\u{301}",
            ".",
            "..",
            ".veynode-secret",
            "x\0y",
        ] {
            assert!(!valid(FileAction::DownloadBegin { path: path.into() }));
        }
        assert!(valid(FileAction::DownloadBegin {
            path: "日本語.txt".into()
        }));
        let token = "a".repeat(32);
        assert!(valid(FileAction::UploadWrite {
            token: token.clone(),
            offset: 0,
            data: STANDARD.encode(vec![7; CHUNK_BYTES])
        }));
        for data in [
            "".into(),
            "YQ".into(),
            STANDARD.encode(vec![7; CHUNK_BYTES + 1]),
        ] {
            assert!(!valid(FileAction::UploadWrite {
                token: token.clone(),
                offset: 0,
                data
            }));
        }
        assert!(serde_json::from_value::<FileAction>(
            serde_json::json!({"kind":"cancel","token":token,"owner":"other"})
        )
        .is_err());
    }
}
#[cfg(test)]
mod golden_tests {
    #[test]
    fn golden_bytes_signatures_scope_and_payload_binding() {
        let value: serde_json::Value =
            serde_json::from_str(include_str!("../tests/files-golden.json")).unwrap();
        for case in value["cases"].as_array().unwrap() {
            let signed: crate::control::SignedControl =
                serde_json::from_value(case["signed"].clone()).unwrap();
            assert_eq!(
                crate::hex(&signed.request.canonical().unwrap()),
                case["canonical_hex"]
            );
            assert_eq!(signed.request.action.required_scope(), "files");
            crate::verify(
                value["device_public_key"].as_str().unwrap(),
                &signed.request.canonical().unwrap(),
                &signed.signature,
            )
            .unwrap();
            let mut changed = signed.request.clone();
            changed.sequence += 1;
            assert!(crate::verify(
                value["device_public_key"].as_str().unwrap(),
                &changed.canonical().unwrap(),
                &signed.signature
            )
            .is_err());
        }
    }
}
