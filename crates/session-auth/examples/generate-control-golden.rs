//! Public scalar-2 test identity only; never a product identity.
use anyhow::Result;
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use serde_json::json;
use vm_session_auth::{control::*, *};
fn main() -> Result<()> {
    let original: serde_json::Value = serde_json::from_str(include_str!("../tests/golden.json"))?;
    let answer: SignedAnswer = serde_json::from_value(original["signed_answer"].clone())?;
    let mut bytes = [0; 32];
    bytes[31] = 2;
    let key = SigningKey::from_bytes((&bytes).into())?;
    let frame = FrameAnchor {
        frame_id: "01234567-89ab-4def-8123-456789abcdef".into(),
        source_generation: 4,
        width: 1280,
        height: 800,
    };
    let actions = vec![
        ControlAction::Acquire,
        ControlAction::Release { epoch: 7 },
        ControlAction::ObserveFrame,
        ControlAction::Click {
            epoch: 7,
            frame: frame.clone(),
            x: 200,
            y: 200,
            button: "left".into(),
        },
        ControlAction::Drag {
            epoch: 7,
            frame: frame.clone(),
            x: 0,
            y: 0,
            end_x: 1279,
            end_y: 799,
            button: "right".into(),
        },
        ControlAction::Scroll {
            epoch: 7,
            frame: frame.clone(),
            x: 640,
            y: 400,
            direction: "down".into(),
            ticks: 3,
        },
        ControlAction::Key {
            epoch: 7,
            frame,
            keys: vec!["ctrl".into(), "a".into()],
        },
    ];
    let cases=actions.into_iter().enumerate().map(|(i,action)|{let request=ControlRequest{session_id:answer.transcript.session_id.clone(),transcript_sha256:answer.transcript.digest()?,sequence:i as u64+1,expires_at:1_700_000_015,action};let canonical=request.canonical()?;let signature:Signature=key.sign(&canonical);Ok::<_,anyhow::Error>(json!({"signed":SignedControl{request,signature:hex(&signature.to_bytes())},"canonical_hex":hex(&canonical)}))}).collect::<Result<Vec<_>>>()?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"fixture_only":true,"now":1_700_000_010u64,"device_public_key":original["device_public_key"],"transcript":answer.transcript,"cases":cases})
        )?
    );
    Ok(())
}
