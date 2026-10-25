//! Minimal OCI Distribution (registry v2) client: anonymous bearer-token auth,
//! manifest-list platform selection and digest-verified blob download.
use crate::reference::{validate_digest, Reference};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, io::Read, time::Duration};

pub const MANIFEST_TYPES: &[&str] = &[
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];
const MAX_MANIFEST: u64 = 4 * 1024 * 1024;

pub struct Client {
    agent: ureq::Agent,
    tokens: HashMap<String, String>,
}

pub struct Platform<'a> {
    pub os: &'a str,
    pub architecture: &'a str,
}
pub const LINUX_ARM64: Platform<'static> = Platform {
    os: "linux",
    architecture: "arm64",
};

fn parse_challenge(header: &str) -> Option<HashMap<String, String>> {
    let rest = header.strip_prefix("Bearer ")?;
    let mut out = HashMap::new();
    let mut rest = rest.trim();
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let after = after.strip_prefix('"')?;
        let end = after.find('"')?;
        out.insert(key.trim().to_owned(), after[..end].to_owned());
        rest = after[end + 1..].trim_start_matches(',').trim();
    }
    Some(out)
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(20))
                .timeout_read(Duration::from_secs(120))
                .user_agent("veynodevm/0.1")
                .build(),
            tokens: HashMap::new(),
        }
    }

    fn get(&mut self, image: &Reference, url: &str, accept: &[&str]) -> Result<ureq::Response> {
        let key = format!("{}/{}", image.api_host(), image.repository);
        for attempt in 0..2 {
            let mut request = self.agent.get(url);
            if !accept.is_empty() {
                request = request.set("Accept", &accept.join(", "));
            }
            if let Some(token) = self.tokens.get(&key) {
                request = request.set("Authorization", &format!("Bearer {token}"));
            }
            match request.call() {
                Ok(response) => return Ok(response),
                Err(ureq::Error::Status(401, response)) if attempt == 0 => {
                    let challenge = response
                        .header("WWW-Authenticate")
                        .and_then(parse_challenge)
                        .context("registry requires authentication we cannot satisfy")?;
                    let realm = challenge
                        .get("realm")
                        .context("auth challenge without realm")?;
                    if !realm.starts_with("https://") {
                        bail!("refusing non-HTTPS token realm")
                    }
                    let mut token_request = self.agent.get(realm);
                    if let Some(service) = challenge.get("service") {
                        token_request = token_request.query("service", service);
                    }
                    let scope = challenge
                        .get("scope")
                        .cloned()
                        .unwrap_or_else(|| format!("repository:{}:pull", image.repository));
                    token_request = token_request.query("scope", &scope);
                    let body: Value = serde_json::from_reader(
                        token_request
                            .call()
                            .context("anonymous registry token request failed")?
                            .into_reader()
                            .take(MAX_MANIFEST),
                    )?;
                    let token = body["token"]
                        .as_str()
                        .or(body["access_token"].as_str())
                        .context("token response without token")?;
                    self.tokens.insert(key.clone(), token.to_owned());
                }
                Err(ureq::Error::Status(code, response)) => {
                    let detail = response.into_string().unwrap_or_default();
                    bail!(
                        "registry returned HTTP {code} for {url}: {}",
                        detail.chars().take(300).collect::<String>()
                    )
                }
                Err(e) => return Err(e).with_context(|| format!("request failed: {url}")),
            }
        }
        bail!("registry authentication failed")
    }

    /// Fetch a manifest (or index) by reference or digest; returns (media type, digest, bytes).
    pub fn manifest(
        &mut self,
        image: &Reference,
        reference: &str,
    ) -> Result<(String, String, Vec<u8>)> {
        let url = format!(
            "https://{}/v2/{}/manifests/{}",
            image.api_host(),
            image.repository,
            reference
        );
        let response = self.get(image, &url, MANIFEST_TYPES)?;
        let media_type = response
            .header("Content-Type")
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned();
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(MAX_MANIFEST + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_MANIFEST {
            bail!("manifest exceeds 4 MiB")
        }
        let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
        if reference.starts_with("sha256:") && reference != digest {
            bail!("manifest digest mismatch: expected {reference}, got {digest}")
        }
        let media_type = if media_type.is_empty() || media_type == "application/json" {
            serde_json::from_slice::<Value>(&bytes)?["mediaType"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        } else {
            media_type
        };
        Ok((media_type, digest, bytes))
    }

    /// Resolve a reference to a single-platform image manifest.
    pub fn resolve(&mut self, image: &Reference, platform: &Platform) -> Result<(String, Vec<u8>)> {
        let (media_type, digest, bytes) = self.manifest(image, image.manifest_reference())?;
        let value: Value = serde_json::from_slice(&bytes)?;
        if media_type.contains("index")
            || media_type.contains("manifest.list")
            || value["manifests"].is_array()
        {
            let chosen = select_platform(&value, platform)?;
            let (_, digest, bytes) = self.manifest(image, &chosen)?;
            return Ok((digest, bytes));
        }
        Ok((digest, bytes))
    }

    /// Stream a blob into `sink`, verifying its digest and size.
    pub fn blob(
        &mut self,
        image: &Reference,
        digest: &str,
        size: Option<u64>,
        sink: &mut dyn std::io::Write,
    ) -> Result<u64> {
        let expected = validate_digest(digest)?.to_owned();
        let url = format!(
            "https://{}/v2/{}/blobs/{}",
            image.api_host(),
            image.repository,
            digest
        );
        let response = self.get(image, &url, &[])?;
        let mut reader = response.into_reader();
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 1 << 20];
        let mut total = 0u64;
        loop {
            let n = reader.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            sink.write_all(&buffer[..n])?;
            total += n as u64;
            if size.is_some_and(|s| total > s) {
                bail!("blob {digest} larger than declared")
            }
        }
        if size.is_some_and(|s| total != s) {
            bail!("blob {digest} size mismatch")
        }
        if format!("{:x}", hasher.finalize()) != expected {
            bail!("blob {digest} digest mismatch")
        }
        Ok(total)
    }
}

pub fn select_platform(index: &Value, platform: &Platform) -> Result<String> {
    let manifests = index["manifests"]
        .as_array()
        .context("index without manifests")?;
    let mut best: Option<(u8, &str)> = None;
    for m in manifests {
        let p = &m["platform"];
        if p["os"] != platform.os || p["architecture"] != platform.architecture {
            continue;
        }
        // Prefer arm64 v8 or unspecified variant.
        let rank = match p["variant"].as_str() {
            None | Some("v8") => 2,
            _ => 1,
        };
        let digest = m["digest"].as_str().context("index entry without digest")?;
        if best.is_none_or(|(r, _)| rank > r) {
            best = Some((rank, digest));
        }
    }
    let (_, digest) = best.with_context(|| {
        format!(
            "image has no {}/{} variant",
            platform.os, platform.architecture
        )
    })?;
    validate_digest(digest)?;
    Ok(digest.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bearer_challenge_parsing() {
        let c = parse_challenge(r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#).unwrap();
        assert_eq!(c["realm"], "https://auth.docker.io/token");
        assert_eq!(c["scope"], "repository:library/alpine:pull");
        assert!(parse_challenge("Basic realm=\"x\"").is_none());
    }
    #[test]
    fn platform_selection_prefers_arm64_v8() {
        let d = |c: char| format!("sha256:{}", c.to_string().repeat(64));
        let index = serde_json::json!({"manifests":[
            {"digest":d('a'),"platform":{"os":"linux","architecture":"amd64"}},
            {"digest":d('b'),"platform":{"os":"linux","architecture":"arm64","variant":"v7"}},
            {"digest":d('c'),"platform":{"os":"linux","architecture":"arm64","variant":"v8"}},
            {"digest":d('e'),"platform":{"os":"unknown","architecture":"unknown"}}
        ]});
        assert_eq!(select_platform(&index, &LINUX_ARM64).unwrap(), d('c'));
        let amd_only = serde_json::json!({"manifests":[{"digest":d('a'),"platform":{"os":"linux","architecture":"amd64"}}]});
        assert!(select_platform(&amd_only, &LINUX_ARM64).is_err());
    }
}
