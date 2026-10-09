//! Public scalar-2 test identity only; never a product identity.
use anyhow::Result;
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use serde_json::json;
use vm_session_auth::{control::*, *};
fn main() -> Result<()> {
    let original: serde_json::Value = serde_json::from_str(include_str!("../tests/golden.json"))?;
    let mut answer: SignedAnswer = serde_json::from_value(original["signed_answer"].clone())?;
    answer.transcript.intent.challenge.scopes = vec!["files".into(), "observe".into()];
    let mut bytes = [0; 32];
    bytes[31] = 2;
    let key = SigningKey::from_bytes((&bytes).into())?;
    use vm_session_auth::files::FileAction;
    let token = "a".repeat(32);
    let actions = vec![
        FileAction::Info,
        FileAction::UploadBegin {
            path: "日本語.txt".into(),
            size: 3,
            sha256: hash(b"abc"),
        },
        FileAction::UploadWrite {
            token: token.clone(),
            offset: 0,
            data: "YWJj".into(),
        },
        FileAction::UploadCommit {
            token: token.clone(),
        },
        FileAction::DownloadBegin {
            path: "日本語.txt".into(),
        },
        FileAction::DownloadRead {
            token: token.clone(),
            offset: 0,
        },
        FileAction::Cancel { token },
    ]
    .into_iter()
    .map(|operation| ControlAction::File { operation })
    .collect::<Vec<_>>();
    let cases=actions.into_iter().enumerate().map(|(i,action)|{let request=ControlRequest{session_id:answer.transcript.session_id.clone(),transcript_sha256:answer.transcript.digest()?,sequence:i as u64+1,expires_at:1_700_000_015,action};let canonical=request.canonical()?;let signature:Signature=key.sign(&canonical);Ok::<_,anyhow::Error>(json!({"signed":SignedControl{request,signature:hex(&signature.to_bytes())},"canonical_hex":hex(&canonical)}))}).collect::<Result<Vec<_>>>()?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"fixture_only":true,"now":1_700_000_010u64,"device_public_key":original["device_public_key"],"transcript":answer.transcript,"cases":cases})
        )?
    );
    Ok(())
}
