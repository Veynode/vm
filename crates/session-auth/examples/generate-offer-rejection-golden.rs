//! Public scalar-1 fixture key only. Never create a product identity this way.
use anyhow::Result;
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use serde_json::json;
use vm_session_auth::{rejection, *};
struct FixtureKey(SigningKey);
impl Identity for FixtureKey {
    fn public_key(&self) -> Result<String> {
        Ok(hex(self
            .0
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()))
    }
    fn sign(&self, bytes: &[u8]) -> Result<String> {
        let signature: Signature = self.0.sign(bytes);
        Ok(hex(&signature.to_bytes()))
    }
}
fn main() -> Result<()> {
    let original: serde_json::Value = serde_json::from_str(include_str!("../tests/golden.json"))?;
    let offer: SignedOffer = serde_json::from_value(original["signed_offer"].clone())?;
    let mut scalar = [0; 32];
    scalar[31] = 1;
    let host = FixtureKey(SigningKey::from_bytes((&scalar).into())?);
    let cases = rejection::CODES
        .iter()
        .map(|code| {
            let signed = rejection::sign(offer.intent.clone(), code, &host)?;
            Ok::<_, anyhow::Error>(
                json!({"canonical_hex":hex(&signed.rejection.canonical()?),"signed":signed}),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"fixture_only":true,"host_public_key":host.public_key()?,"expected_intent":offer.intent,"cases":cases})
        )?
    );
    Ok(())
}
