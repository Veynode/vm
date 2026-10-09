use crate::{
    store::{Registry, Store},
    *,
};
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::{
    io::{self, Read},
    path::Path,
};
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Verification {
    IceRequest {
        device_public_key: String,
        signed: ice::SignedIceRequest,
    },
    IceConfig {
        host_public_key: String,
        expected_request: ice::IceRequest,
        signed: ice::SignedIceConfig,
        now: u64,
    },
    Control {
        device_public_key: String,
        transcript: AnswerTranscript,
        signed: control::SignedControl,
    },
    Grant {
        host_public_key: String,
        signed: SignedGrant,
    },
    Offer {
        device_public_key: String,
        signed: SignedOffer,
        exact_sdp: String,
    },
    OfferRejection {
        host_public_key: String,
        expected_intent: OfferIntent,
        signed: rejection::SignedOfferRejection,
    },
    Answer {
        host_public_key: String,
        expected_intent: OfferIntent,
        exact_sdp: String,
        signed: Box<SignedAnswer>,
    },
    Close {
        device_public_key: String,
        transcript: AnswerTranscript,
        signed: SignedClose,
    },
    Confirm {
        device_public_key: String,
        transcript: AnswerTranscript,
        signed: Confirmation,
    },
}
fn read<T: serde::de::DeserializeOwned>() -> Result<T> {
    let mut bytes = Vec::new();
    io::stdin().take(512 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 512 * 1024, "input too large");
    Ok(serde_json::from_slice(&bytes)?)
}
pub fn run_cli(a: &[String]) -> Result<()> {
    let command=a.first().context("usage: vm-pair verify | pairing-digest | init-host|public|approve|revoke|list STATE NAMESPACE [DIGEST|DEVICE VM] | keychain-self-test test-NAMESPACE")?;
    if command == "verify" {
        ensure!(a.len() == 1, "unexpected arguments");
        match read::<Verification>()? {
            Verification::IceRequest {
                device_public_key,
                signed,
            } => {
                ensure!(
                    identity_id(&device_public_key)? == signed.request.challenge.device_id,
                    "device mismatch"
                );
                verify(
                    &device_public_key,
                    &signed.request.canonical()?,
                    &signed.signature,
                )?;
            }
            Verification::IceConfig {
                host_public_key,
                expected_request,
                signed,
                now,
            } => signed.verify(&host_public_key, &expected_request, now)?,
            Verification::Control {
                device_public_key,
                transcript,
                signed,
            } => {
                ensure!(
                    identity_id(&device_public_key)? == transcript.intent.challenge.device_id,
                    "device mismatch"
                );
                ensure!(
                    signed.request.session_id == transcript.session_id
                        && signed.request.transcript_sha256 == transcript.digest()?,
                    "control transcript mismatch"
                );
                ensure!(
                    transcript
                        .intent
                        .challenge
                        .scopes
                        .iter()
                        .any(|s| s == signed.request.action.required_scope()),
                    "control scope absent from transcript"
                );
                verify(
                    &device_public_key,
                    &signed.request.canonical()?,
                    &signed.signature,
                )?;
            }
            Verification::Grant {
                host_public_key,
                signed,
            } => signed.verify(&host_public_key)?,
            Verification::Offer {
                device_public_key,
                signed,
                exact_sdp,
            } => {
                ensure!(
                    identity_id(&device_public_key)? == signed.intent.challenge.device_id,
                    "device mismatch"
                );
                ensure!(
                    signed.intent.offer == SdpBinding::from_sdp(&exact_sdp)?,
                    "offer mismatch"
                );
                verify(
                    &device_public_key,
                    &signed.intent.canonical()?,
                    &signed.signature,
                )?
            }
            Verification::OfferRejection {
                host_public_key,
                expected_intent,
                signed,
            } => signed.verify(&host_public_key, &expected_intent)?,
            Verification::Answer {
                host_public_key,
                expected_intent,
                exact_sdp,
                signed,
            } => signed.verify(&host_public_key, &expected_intent, &exact_sdp)?,
            Verification::Close {
                device_public_key,
                transcript,
                signed,
            } => {
                ensure!(
                    identity_id(&device_public_key)? == transcript.intent.challenge.device_id,
                    "device mismatch"
                );
                ensure!(
                    transcript.digest()? == signed.transcript_sha256
                        && transcript.session_id == signed.session_id,
                    "close transcript mismatch"
                );
                verify(
                    &device_public_key,
                    &close_bytes(&signed.session_id, &signed.transcript_sha256)?,
                    &signed.signature,
                )?;
            }
            Verification::Confirm {
                device_public_key,
                transcript,
                signed,
            } => {
                ensure!(
                    identity_id(&device_public_key)? == transcript.intent.challenge.device_id,
                    "device mismatch"
                );
                ensure!(
                    transcript.digest()? == signed.transcript_sha256,
                    "transcript mismatch"
                );
                verify(
                    &device_public_key,
                    &confirmation_bytes(&signed.transcript_sha256)?,
                    &signed.signature,
                )?
            }
        };
        println!("{}", json!({"verified":true,"authorization_checked":false}));
        return Ok(());
    }
    if command == "pairing-digest" {
        ensure!(a.len() == 1, "unexpected arguments");
        let p: Pairing = read()?;
        println!(
            "{}",
            json!({"pairing_digest":p.digest()?,"host_id":identity_id(&p.host_public_key)?,"device_id":identity_id(&p.device_public_key)?})
        );
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        use keychain::KeychainIdentity;
        keychain::disable_interaction_for_process()?;
        if command == "keychain-self-test" {
            ensure!(
                a.len() == 2 && a[1].starts_with("test-"),
                "unique test- namespace required"
            );
            let identity = KeychainIdentity::create(&a[1])?;
            let result = (|| {
                let public = identity.public_key()?;
                let signature = identity.sign(b"Veynode/keychain-self-test/v1\0")?;
                verify(&public, b"Veynode/keychain-self-test/v1\0", &signature)?;
                let reopened = KeychainIdentity::open(&a[1])?;
                ensure!(reopened.public_key()? == public, "reopened key changed");
                println!(
                    "{}",
                    json!({"verified":true,"private_extractable":false,"public_key":public,"reopened_same_identity":true})
                );
                Ok::<_, anyhow::Error>(())
            })();
            identity
                .delete_test_identity(&a[1])
                .context("test identity cleanup failed")?;
            return result;
        }
        ensure!(a.len() >= 3, "state directory and namespace required");
        let store = Store::open(Path::new(&a[1]))?;
        if command == "init-host" {
            ensure!(a.len() == 3, "unexpected arguments");
            ensure!(store.load()?.host_public_key.is_empty(), "registry exists");
            let identity = KeychainIdentity::create(&a[2])?;
            let public = identity.public_key()?;
            store.save(&Registry {
                host_public_key: public.clone(),
                ..Registry::default()
            })?;
            println!(
                "{}",
                json!({"host_id":identity_id(&public)?,"host_public_key":public})
            );
            return Ok(());
        }
        let identity = KeychainIdentity::open(&a[2])?;
        let mut registry = store.load()?;
        ensure!(
            registry.host_public_key == identity.public_key()?,
            "registry identity mismatch"
        );
        match command.as_str() {
            "public" => {
                ensure!(a.len() == 3, "unexpected arguments");
                println!(
                    "{}",
                    json!({"host_id":identity_id(&registry.host_public_key)?,"host_public_key":registry.host_public_key})
                )
            }
            "list" => {
                ensure!(a.len() == 3, "unexpected arguments");
                println!("{}", serde_json::to_string(&registry)?)
            }
            "approve" => {
                ensure!(
                    a.len() == 4,
                    "independently verified pairing digest required as final argument"
                );
                let pairing: Pairing = read()?;
                let grant = registry.approve(&pairing, &a[3], &identity)?;
                store.save(&registry)?;
                println!("{}", serde_json::to_string(&grant)?)
            }
            "revoke" => {
                ensure!(a.len() == 5, "device id and VM id required");
                registry.revoke(&a[3], &a[4])?;
                store.save(&registry)?;
                println!(
                    "{}",
                    json!({"revoked":true,"active_transport_termination":"integration must reload registry and stop returned session IDs"})
                )
            }
            _ => bail!("unknown command"),
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    bail!("host Keychain identity requires macOS")
}
