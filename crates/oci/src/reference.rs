//! Docker-style image references: `alpine`, `alpine:3.20`, `ghcr.io/o/r:tag`,
//! `repo@sha256:...`.
use anyhow::{bail, Result};
use std::fmt;

pub const DOCKER_HUB: &str = "docker.io";
const DOCKER_HUB_API: &str = "registry-1.docker.io";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl Reference {
    pub fn parse(input: &str) -> Result<Self> {
        if input.is_empty() || input.len() > 512 || input.chars().any(|c| c.is_whitespace()) {
            bail!("invalid image reference")
        }
        let (rest, digest) = match input.split_once('@') {
            Some((rest, digest)) => {
                validate_digest(digest)?;
                (rest, Some(digest.to_owned()))
            }
            None => (input, None),
        };
        // A tag is after the last ':' that follows the last '/'.
        let slash = rest.rfind('/').map_or(0, |i| i + 1);
        let (name, tag) = match rest[slash..].rfind(':') {
            Some(i) => (&rest[..slash + i], Some(rest[slash + i + 1..].to_owned())),
            None => (rest, None),
        };
        let mut parts = name.splitn(2, '/');
        let first = parts.next().unwrap_or("");
        let (registry, repository) = match parts.next() {
            Some(remainder)
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_owned(), remainder.to_owned())
            }
            _ => (DOCKER_HUB.to_owned(), name.to_owned()),
        };
        let (host, port) = registry.split_once(':').unwrap_or((&registry, ""));
        if host.is_empty()
            || host.contains("..")
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
            || (registry.contains(':')
                && (port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit())))
        {
            bail!("invalid registry host")
        }
        let repository = if registry == DOCKER_HUB && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };
        if repository.is_empty()
            || !repository
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-/".contains(&b))
        {
            bail!("invalid repository name")
        }
        if let Some(tag) = &tag {
            if tag.is_empty()
                || tag.len() > 128
                || !tag
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            {
                bail!("invalid tag")
            }
        }
        let tag = if tag.is_none() && digest.is_none() {
            Some("latest".to_owned())
        } else {
            tag
        };
        Ok(Self {
            registry,
            repository,
            tag,
            digest,
        })
    }
    /// Host used for the registry API.
    pub fn api_host(&self) -> &str {
        if self.registry == DOCKER_HUB {
            DOCKER_HUB_API
        } else {
            &self.registry
        }
    }
    /// The manifest reference to request: digest when pinned, otherwise tag.
    pub fn manifest_reference(&self) -> &str {
        self.digest
            .as_deref()
            .or(self.tag.as_deref())
            .unwrap_or("latest")
    }
    /// Short display form: `alpine:3.20`, `ghcr.io/o/r:tag`.
    pub fn familiar(&self) -> String {
        let name = if self.registry == DOCKER_HUB {
            self.repository
                .strip_prefix("library/")
                .unwrap_or(&self.repository)
                .to_owned()
        } else {
            format!("{}/{}", self.registry, self.repository)
        };
        match (&self.tag, &self.digest) {
            (_, Some(d)) => format!("{name}@{d}"),
            (Some(t), None) => format!("{name}:{t}"),
            (None, None) => name,
        }
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.familiar())
    }
}

pub fn validate_digest(digest: &str) -> Result<&str> {
    match digest.strip_prefix("sha256:") {
        Some(hex)
            if hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) =>
        {
            Ok(hex)
        }
        _ => bail!("only lowercase sha256 digests are supported: {digest}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn docker_hub_shorthand() {
        let r = Reference::parse("alpine").unwrap();
        assert_eq!(
            (r.registry.as_str(), r.repository.as_str()),
            ("docker.io", "library/alpine")
        );
        assert_eq!(r.manifest_reference(), "latest");
        assert_eq!(r.familiar(), "alpine:latest");
        assert_eq!(r.api_host(), "registry-1.docker.io");
        let r = Reference::parse("docker:28-dind").unwrap();
        assert_eq!(r.tag.as_deref(), Some("28-dind"));
        let r = Reference::parse("grafana/grafana:11.0.0").unwrap();
        assert_eq!(r.repository, "grafana/grafana");
    }
    #[test]
    fn explicit_registry_port_and_digest() {
        let d = format!("sha256:{}", "a".repeat(64));
        let r = Reference::parse(&format!("localhost:5000/team/app:v1@{d}")).unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "team/app");
        assert_eq!(r.manifest_reference(), d);
        let r = Reference::parse("ghcr.io/o/r").unwrap();
        assert_eq!(
            (r.registry.as_str(), r.repository.as_str()),
            ("ghcr.io", "o/r")
        );
    }
    #[test]
    fn invalid_references() {
        for bad in [
            "",
            "Alpine",
            "a b",
            "alpine:",
            "x@sha256:abc",
            "alpine:../x",
        ] {
            assert!(Reference::parse(bad).is_err(), "{bad}");
        }
    }
}
