//! Public reproducibility fixture only. Scalars 1/2 are intentionally non-secret test keys.
use anyhow::Result;
use p256::ecdsa::{signature::Signer, SigningKey};
use serde_json::json;
use vm_session_auth::*;
struct TestIdentity(SigningKey);
impl Identity for TestIdentity {
    fn public_key(&self) -> Result<String> {
        Ok(hex(self
            .0
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()))
    }
    fn sign(&self, m: &[u8]) -> Result<String> {
        let s: p256::ecdsa::Signature = self.0.sign(m);
        Ok(hex(&s.to_bytes()))
    }
}
fn main() -> Result<()> {
    let mut scalar = [0; 32];
    scalar[31] = 1;
    let host = TestIdentity(SigningKey::from_bytes((&scalar).into())?);
    scalar[31] = 2;
    let device = TestIdentity(SigningKey::from_bytes((&scalar).into())?);
    let host_public = host.public_key()?;
    let device_public = device.public_key()?;
    let host_id = identity_id(&host_public)?;
    let device_id = identity_id(&device_public)?;
    let scopes = vec!["input".into(), "observe".into()];
    let pairing = Pairing {
        host_public_key: host_public.clone(),
        device_public_key: device_public.clone(),
        nonce: "a1".repeat(32),
        vm_id: "linux-fixture".into(),
        scopes: scopes.clone(),
    };
    let grant = SignedGrant::new(
        Grant {
            host_id: host_id.clone(),
            device_id: device_id.clone(),
            device_public_key: device_public.clone(),
            vm_id: pairing.vm_id.clone(),
            scopes: scopes.clone(),
            version: 3,
        },
        &host,
    )?;
    let offer_sdp = format!(
        "v=0\r\na=fingerprint:sha-256 {}\r\nm=video 9 UDP/TLS/RTP/SAVPF 98\r\na=recvonly\r\n",
        vec!["11"; 32].join(":")
    );
    let answer_sdp = format!(
        "v=0\r\na=fingerprint:sha-256 {}\r\nm=video 9 UDP/TLS/RTP/SAVPF 98\r\na=sendonly\r\n",
        vec!["22"; 32].join(":")
    );
    let intent = OfferIntent {
        challenge: Challenge {
            host_id: host_id.clone(),
            device_id: device_id.clone(),
            vm_id: pairing.vm_id.clone(),
            scopes,
            grant_version: 3,
            instance_nonce: "b2".repeat(32),
            nonce: "c3".repeat(32),
            issued_at: 1_700_000_000,
            expires_at: 1_700_000_060,
        },
        request_id: "12345678-1234-4234-8234-123456789abc".into(),
        codec: "h265".into(),
        offer: SdpBinding::from_sdp(&offer_sdp)?,
    };
    let offer = SignedOffer {
        signature: device.sign(&intent.canonical()?)?,
        intent: intent.clone(),
    };
    let transcript = AnswerTranscript {
        intent,
        session_id: "abcdefgh-session".into(),
        answer: SdpBinding::from_sdp(&answer_sdp)?,
    };
    let answer = SignedAnswer {
        signature: host.sign(&transcript.canonical()?)?,
        transcript: transcript.clone(),
    };
    let confirm = Confirmation {
        transcript_sha256: transcript.digest()?,
        signature: device.sign(&confirmation_bytes(&transcript.digest()?)?)?,
    };
    let close = SignedClose {
        session_id: transcript.session_id.clone(),
        transcript_sha256: transcript.digest()?,
        signature: device.sign(&close_bytes(&transcript.session_id, &transcript.digest()?)?)?,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"fixture_only":true,"now":1_700_000_010u64,"host_public_key":host_public,"device_public_key":device_public,"host_id":host_id,"device_id":device_id,"pairing":pairing,"pairing_digest":pairing.digest()?,"signed_grant":grant,"signed_offer":offer,"signed_answer":answer,"confirmation":confirm,"signed_close":close,"offer_sdp":offer_sdp,"answer_sdp":answer_sdp,"canonical_hex":{"pairing":hex(&pairing.canonical()?),"grant":hex(&grant.grant.canonical()?),"offer":hex(&offer.intent.canonical()?),"answer":hex(&transcript.canonical()?),"confirm":hex(&confirmation_bytes(&confirm.transcript_sha256)?),"close":hex(&close_bytes(&close.session_id,&close.transcript_sha256)?)}})
        )?
    );
    Ok(())
}
