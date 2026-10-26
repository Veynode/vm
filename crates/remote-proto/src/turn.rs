//! TURN relay configuration and short-lived credentials for the WebRTC
//! fallback. Same three relay kinds, variable suffixes and precedence as
//! veynode/core's account service (`packages/cloud-core/src/ice.ts`), with
//! the `VEYNODE_GATEWAY_` prefix: a Cloudflare key id and token win over
//! URLs, a shared secret wins over a fixed credential, a half configuration
//! is never guessed into a relay. The long-term secret or Cloudflare key
//! stays on the Mac host; only minted credentials reach a browser, inside
//! the end-to-end channel. No HTTP here: the host calls Cloudflare.
use anyhow::{ensure, Result};
use aws_lc_rs::hmac;
use base64::Engine;
use serde::{Deserialize, Serialize};

pub const ENV_PREFIX: &str = "VEYNODE_GATEWAY_";
pub const KEY_ID: &str = "TURN_KEY_ID";
pub const API_TOKEN: &str = "TURN_KEY_API_TOKEN";
pub const URLS: &str = "TURN_URLS";
pub const SECRET: &str = "TURN_SECRET";
pub const USERNAME: &str = "TURN_USERNAME";
pub const CREDENTIAL: &str = "TURN_CREDENTIAL";

pub const CLOUDFLARE_ORIGIN: &str = "https://rtc.live.cloudflare.com";
/// Remote sessions are capped at 8 h; credentials never outlive the session.
pub const MAX_TTL_SECONDS: u64 = 8 * 3600;

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Relay {
    Cloudflare {
        key_id: String,
        api_token: String,
    },
    SharedSecret {
        urls: Vec<String>,
        secret: String,
    },
    Static {
        urls: Vec<String>,
        username: String,
        credential: String,
    },
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cloudflare { key_id, .. } => write!(f, "Cloudflare({key_id})"),
            Self::SharedSecret { urls, .. } => write!(f, "SharedSecret({urls:?})"),
            Self::Static { urls, .. } => write!(f, "Static({urls:?})"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

fn valid_key_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// A `turn:`/`turns:` URL a browser will use: no whitespace, a host, not port 53.
pub fn relay_url(url: &str) -> Option<&str> {
    let url = url.trim();
    let rest = url
        .strip_prefix("turns:")
        .or_else(|| url.strip_prefix("turn:"))?;
    let host_port = rest.split('?').next()?;
    if host_port.is_empty() || url.chars().any(char::is_whitespace) || url.len() > 512 {
        return None;
    }
    if host_port.rsplit_once(':').is_some_and(|(_, p)| p == "53") {
        return None;
    }
    Some(url)
}

/// The relay `read(suffix)` configures, or `None`.
pub fn from_env(read: impl Fn(&str) -> Option<String>) -> Option<Relay> {
    let get = |name: &str| {
        read(&format!("{ENV_PREFIX}{name}"))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    if let (Some(key_id), Some(api_token)) = (get(KEY_ID), get(API_TOKEN)) {
        if valid_key_id(&key_id) {
            return Some(Relay::Cloudflare { key_id, api_token });
        }
    }
    let urls: Vec<String> = get(URLS)
        .unwrap_or_default()
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter_map(relay_url)
        .map(str::to_string)
        .collect();
    if urls.is_empty() {
        return None;
    }
    if let Some(secret) = get(SECRET) {
        return Some(Relay::SharedSecret { urls, secret });
    }
    match (get(USERNAME), get(CREDENTIAL)) {
        (Some(username), Some(credential)) => Some(Relay::Static {
            urls,
            username,
            credential,
        }),
        _ => None,
    }
}

impl Relay {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Cloudflare { key_id, api_token } => {
                ensure!(valid_key_id(key_id), "invalid Cloudflare TURN key id");
                ensure!(!api_token.is_empty(), "empty Cloudflare TURN API token");
            }
            Self::SharedSecret { urls, secret } => {
                ensure!(
                    !urls.is_empty() && urls.iter().all(|u| relay_url(u).is_some()),
                    "invalid TURN URLs"
                );
                ensure!(
                    secret.len() >= 16,
                    "TURN shared secret shorter than 16 bytes"
                );
            }
            Self::Static {
                urls,
                username,
                credential,
            } => {
                ensure!(
                    !urls.is_empty() && urls.iter().all(|u| relay_url(u).is_some()),
                    "invalid TURN URLs"
                );
                ensure!(
                    !username.is_empty() && !credential.is_empty(),
                    "empty TURN credential"
                );
            }
        }
        Ok(())
    }

    /// Credentials for a relay this host can mint locally; `None` for
    /// Cloudflare, which needs [`cloudflare_request`].
    pub fn local_servers(&self, id: &str, expires_at_unix: u64) -> Option<Vec<IceServer>> {
        match self {
            Self::Cloudflare { .. } => None,
            Self::SharedSecret { urls, secret } => {
                let (username, credential) = shared_secret_credentials(secret, id, expires_at_unix);
                Some(vec![IceServer {
                    urls: urls.clone(),
                    username: Some(username),
                    credential: Some(credential),
                }])
            }
            Self::Static {
                urls,
                username,
                credential,
            } => Some(vec![IceServer {
                urls: urls.clone(),
                username: Some(username.clone()),
                credential: Some(credential.clone()),
            }]),
        }
    }
}

/// coturn `use-auth-secret`: username `<expiry>:<id>`, credential
/// base64(HMAC-SHA1(secret, username)).
pub fn shared_secret_credentials(secret: &str, id: &str, expires_at_unix: u64) -> (String, String) {
    let username = format!("{expires_at_unix}:{id}");
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret.as_bytes());
    let tag = hmac::sign(&key, username.as_bytes());
    (
        username,
        base64::engine::general_purpose::STANDARD.encode(tag.as_ref()),
    )
}

/// The Cloudflare Realtime TURN call: `(url, bearer token, JSON body)`.
pub fn cloudflare_request(
    key_id: &str,
    api_token: &str,
    ttl_seconds: u64,
) -> Result<(String, String, String)> {
    ensure!(valid_key_id(key_id), "invalid Cloudflare TURN key id");
    let ttl = ttl_seconds.clamp(60, MAX_TTL_SECONDS);
    Ok((
        format!("{CLOUDFLARE_ORIGIN}/v1/turn/keys/{key_id}/credentials/generate-ice-servers"),
        api_token.to_string(),
        serde_json::json!({ "ttl": ttl }).to_string(),
    ))
}

/// Servers from Cloudflare's answer, keeping only URLs a browser will use.
pub fn parse_cloudflare(body: &[u8]) -> Result<Vec<IceServer>> {
    #[derive(Deserialize)]
    struct Answer {
        #[serde(rename = "iceServers")]
        ice_servers: Vec<Raw>,
    }
    #[derive(Deserialize)]
    struct Raw {
        urls: serde_json::Value,
        username: Option<String>,
        credential: Option<String>,
    }
    let answer: Answer = serde_json::from_slice(body)?;
    let mut out = vec![];
    for raw in answer.ice_servers {
        let urls: Vec<String> = match raw.urls {
            serde_json::Value::String(s) => vec![s],
            serde_json::Value::Array(a) => a
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => vec![],
        };
        let urls: Vec<String> = urls
            .into_iter()
            .filter(|u| relay_url(u).is_some() || (u.starts_with("stun:") && !u.contains(":53")))
            .collect();
        if !urls.is_empty() {
            out.push(IceServer {
                urls,
                username: raw.username,
                credential: raw.credential,
            });
        }
    }
    ensure!(!out.is_empty(), "Cloudflare returned no usable ICE servers");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (format!("{ENV_PREFIX}{k}"), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn precedence_matches_core() {
        let all = env(&[
            (KEY_ID, "abc"),
            (API_TOKEN, "tok"),
            (URLS, "turn:a.example:3478, turns:a.example:5349"),
            (SECRET, "s"),
        ]);
        assert!(matches!(from_env(all), Some(Relay::Cloudflare { .. })));
        let secret = env(&[
            (URLS, "turn:a.example:3478 stun:x"),
            (SECRET, "s"),
            (USERNAME, "u"),
            (CREDENTIAL, "c"),
        ]);
        assert_eq!(
            from_env(secret),
            Some(Relay::SharedSecret {
                urls: vec!["turn:a.example:3478".into()],
                secret: "s".into()
            })
        );
        assert!(from_env(env(&[(URLS, "turn:a.example:3478"), (USERNAME, "u")])).is_none());
        assert!(from_env(env(&[(KEY_ID, "bad id"), (API_TOKEN, "t")])).is_none());
    }

    #[test]
    fn urls_on_port_53_or_wrong_scheme_are_dropped() {
        assert_eq!(relay_url("turn:x:53"), None);
        assert_eq!(relay_url("turn:x:53?transport=udp"), None);
        assert_eq!(relay_url("http://x"), None);
        assert_eq!(
            relay_url(" turns:x:443?transport=tcp "),
            Some("turns:x:443?transport=tcp")
        );
    }

    #[test]
    fn shared_secret_credential_is_coturn_hmac_sha1() {
        // RFC 2202 test case 2 key/data shape, via the coturn username form.
        let (u, c) = shared_secret_credentials("Jefe", "acct", 1_700_000_000);
        assert_eq!(u, "1700000000:acct");
        let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, b"Jefe");
        let expect = base64::engine::general_purpose::STANDARD
            .encode(hmac::sign(&key, u.as_bytes()).as_ref());
        assert_eq!(c, expect);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&c)
                .unwrap()
                .len(),
            20
        );
        // RFC 2202 test case 2 itself.
        let tag = hmac::sign(&key, b"what do ya want for nothing?");
        assert_eq!(
            tag.as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
    }

    #[test]
    fn cloudflare_request_and_answer() {
        let (url, token, body) = cloudflare_request("k1", "t", 999_999).unwrap();
        assert_eq!(
            url,
            "https://rtc.live.cloudflare.com/v1/turn/keys/k1/credentials/generate-ice-servers"
        );
        assert_eq!(token, "t");
        assert_eq!(body, r#"{"ttl":28800}"#);
        let answer = br#"{"iceServers":[{"urls":["stun:stun.cloudflare.com:3478","turn:turn.cloudflare.com:53?transport=udp","turn:turn.cloudflare.com:3478?transport=udp"],"username":"u","credential":"c"}]}"#;
        let servers = parse_cloudflare(answer).unwrap();
        assert_eq!(servers[0].urls.len(), 2);
        assert!(parse_cloudflare(br#"{"iceServers":[]}"#).is_err());
    }

    #[test]
    fn debug_never_prints_secrets() {
        let r = Relay::SharedSecret {
            urls: vec!["turn:x:3478".into()],
            secret: "hunter2hunter2xx".into(),
        };
        assert!(!format!("{r:?}").contains("hunter2"));
        r.validate().unwrap();
    }
}
