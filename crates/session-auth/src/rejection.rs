//! A signed refusal of one exact, authenticated offer before resource admission.
//! It never proves cleanup of an admitted session and never grants authority.
//! The media caller must establish that no reservation, peer or encoder was made.
use super::*;

pub const CODES: [&str; 3] = [
    "guest_capture_unavailable",
    "guest_display_inactive",
    "guest_display_asleep",
];
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OfferRejection {
    pub intent: OfferIntent,
    pub code: String,
    pub outcome: String,
    pub version: u64,
}
impl OfferRejection {
    pub fn canonical(&self) -> Result<Vec<u8>> {
        self.intent.canonical()?;
        ensure!(
            self.version == 1
                && self.outcome == "not_admitted"
                && CODES.contains(&self.code.as_str()),
            "invalid offer rejection"
        );
        let mut c = Canon::new("offer-rejection");
        c.int(self.version);
        self.intent.fields(&mut c)?;
        c.text(&self.code);
        c.text(&self.outcome);
        Ok(c.0)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedOfferRejection {
    pub rejection: OfferRejection,
    pub signature: String,
}
/// Only call after authenticating the device's offer and establishing that it
/// was not admitted. This function signs that statement, not a runtime proof.
pub fn sign(intent: OfferIntent, code: &str, host: &impl Identity) -> Result<SignedOfferRejection> {
    ensure!(
        identity_id(&host.public_key()?)? == intent.challenge.host_id,
        "offer rejection host mismatch"
    );
    let rejection = OfferRejection {
        intent,
        code: code.into(),
        outcome: "not_admitted".into(),
        version: 1,
    };
    let signature = host.sign(&rejection.canonical()?)?;
    // Enforce the same strict fixed P256 r||s signature representation on output.
    unhex(&signature, 64)?;
    Ok(SignedOfferRejection {
        rejection,
        signature,
    })
}
impl SignedOfferRejection {
    /// Match only the receiver's exact outstanding intent. No time-dependent
    /// authority is granted; like SignedAnswer, this verifies a bound statement.
    /// A rejection for an old request must not be applied to a new offer or used
    /// to replace a signed-answer/cleanup lifecycle already established.
    pub fn verify(&self, host_public: &str, expected_intent: &OfferIntent) -> Result<()> {
        ensure!(
            &self.rejection.intent == expected_intent,
            "offer rejection intent substituted"
        );
        ensure!(
            self.rejection.intent.challenge.host_id == identity_id(host_public)?,
            "offer rejection host substituted"
        );
        verify(host_public, &self.rejection.canonical()?, &self.signature)
    }
}
