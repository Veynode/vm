//! Host-served pinned browser client (default off; `--pinned-client`).
//!
//! The legacy asset routes read `BROWSER_DIR` from disk on every request, so
//! whatever is in that directory at request time is what the browser runs. With
//! this module enabled the service instead:
//!
//! * reads a fixed, compiled-in set of files exactly once at startup,
//! * requires each to match a SHA-256 manifest, and the manifest itself to match
//!   a digest given on the command line, refusing to start otherwise,
//! * serves only those bytes from memory, on exact paths, for GET and HEAD,
//! * attaches one fixed set of response headers to every reply it produces.
//!
//! This does not protect against a compromised host or a compromised build of
//! the client: it pins what this process serves, not who wrote it. Transport to a
//! remote browser is out of scope here; the listener remains loopback HTTP.
use anyhow::{bail, ensure, Context, Result};
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::Response,
    Router,
};
use bytes::Bytes;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

const HTML: &str = "text/html; charset=utf-8";
const SCRIPT: &str = "text/javascript; charset=utf-8";
const STYLE: &str = "text/css; charset=utf-8";

/// The whole client: request path, file relative to `BROWSER_DIR`, content type.
/// This is the module closure of `native.html`; a file that is not listed here
/// is never read and never served. The manifest must name exactly these files.
pub const FILES: [(&str, &str, &str); 11] = [
    ("/", "native.html", HTML),
    ("/probe.css", "probe.css", STYLE),
    ("/dist/native.js", "dist/native.js", SCRIPT),
    ("/dist/codec-policy.js", "dist/codec-policy.js", SCRIPT),
    ("/dist/native-auth.js", "dist/native-auth.js", SCRIPT),
    ("/dist/session-auth.js", "dist/session-auth.js", SCRIPT),
    (
        "/dist/control-channel.js",
        "dist/control-channel.js",
        SCRIPT,
    ),
    ("/dist/ice-config.js", "dist/ice-config.js", SCRIPT),
    (
        "/dist/feedback-evidence.js",
        "dist/feedback-evidence.js",
        SCRIPT,
    ),
    ("/dist/session-files.js", "dist/session-files.js", SCRIPT),
    (
        "/dist/session-control.js",
        "dist/session-control.js",
        SCRIPT,
    ),
];

/// No inline script or style, no `eval`, no third-party origin. `img-src data:`
/// is the verified control screenshot; `connect-src 'self'` is the same-origin
/// signalling `fetch`.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Applied to every response this module produces, including refusals.
pub const SECURITY_HEADERS: [(&str, &str); 6] = [
    ("content-security-policy", CONTENT_SECURITY_POLICY),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-resource-policy", "same-origin"),
    ("cache-control", "no-store"),
];

/// Largest single client file accepted at startup (the largest today is ~72 KiB).
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub manifest: PathBuf,
    pub manifest_sha256: String,
}

/// Removes this module's options from `args`. Both are required together: a
/// manifest without an independent digest would be pinned only by itself.
pub fn parse_options(args: &[String]) -> Result<(Option<Options>, Vec<String>)> {
    let mut manifest = None;
    let mut digest = None;
    let mut remaining = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let slot = match args[i].as_str() {
            "--pinned-client" => &mut manifest,
            "--pinned-client-sha256" => &mut digest,
            _ => {
                remaining.push(args[i].clone());
                i += 1;
                continue;
            }
        };
        ensure!(slot.is_none(), "duplicate pinned client option");
        *slot = Some(
            args.get(i + 1)
                .context("missing pinned client option value")?
                .clone(),
        );
        i += 2;
    }
    match (manifest, digest) {
        (None, None) => Ok((None, remaining)),
        (Some(manifest), Some(manifest_sha256)) => Ok((
            Some(Options {
                manifest: PathBuf::from(manifest),
                manifest_sha256,
            }),
            remaining,
        )),
        _ => bail!("--pinned-client MANIFEST and --pinned-client-sha256 HEX are required together"),
    }
}

struct Entry {
    content_type: &'static str,
    body: Bytes,
}

/// Verified client bytes held in memory. The disk is not read again after
/// `load`, so a later change to `BROWSER_DIR` cannot change what is served.
pub struct Bundle {
    manifest_sha256: String,
    entries: BTreeMap<&'static str, Entry>,
}

fn read_regular_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    // `symlink_metadata` does not follow a final symlink, and it keeps a FIFO
    // or device from being opened at all.
    let kind = std::fs::symlink_metadata(path)
        .with_context(|| format!("pinned client file unavailable: {}", path.display()))?
        .file_type();
    ensure!(
        kind.is_file(),
        "pinned client path is not a regular file: {}",
        path.display()
    );
    let file = std::fs::File::open(path)
        .with_context(|| format!("pinned client file unavailable: {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "pinned client path is not a regular file: {}",
        path.display()
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "pinned client file exceeds {limit} bytes: {}",
        path.display()
    );
    Ok(bytes)
}

/// `shasum -a 256` text form: 64 lowercase hex, two spaces, relative path, LF.
fn parse_manifest(text: &str) -> Result<BTreeMap<&str, &str>> {
    ensure!(
        text.ends_with('\n') && !text.contains('\r'),
        "pinned client manifest must be LF-terminated lines"
    );
    let mut pins = BTreeMap::new();
    for line in text[..text.len() - 1].split('\n') {
        ensure!(
            line.len() > 66 && line.is_char_boundary(64) && line.is_char_boundary(66),
            "malformed pinned client manifest line"
        );
        let (digest, rest) = line.split_at(64);
        vm_session_auth::unhex(digest, 32).context("malformed pinned client manifest digest")?;
        let path = rest
            .strip_prefix("  ")
            .context("malformed pinned client manifest line")?;
        ensure!(
            pins.insert(path, digest).is_none(),
            "duplicate pinned client manifest entry: {path}"
        );
    }
    Ok(pins)
}

impl Bundle {
    /// Fails unless the manifest has exactly the expected digest, names exactly
    /// `FILES`, and every file's content matches its pin.
    pub fn load(root: &Path, options: &Options) -> Result<Self> {
        vm_session_auth::unhex(&options.manifest_sha256, 32)
            .context("--pinned-client-sha256 must be 64 lowercase hex characters")?;
        let manifest = read_regular_bounded(&options.manifest, MAX_MANIFEST_BYTES)?;
        let actual = vm_session_auth::hash(&manifest);
        ensure!(
            actual == options.manifest_sha256,
            "pinned client manifest digest mismatch: expected {}, found {actual}",
            options.manifest_sha256
        );
        let text = std::str::from_utf8(&manifest).context("pinned client manifest is not UTF-8")?;
        let pins = parse_manifest(text)?;
        let expected: BTreeSet<&str> = FILES.iter().map(|(_, file, _)| *file).collect();
        for file in pins.keys() {
            ensure!(
                expected.contains(file),
                "pinned client manifest names a file outside the fixed client: {file}"
            );
        }
        let mut entries = BTreeMap::new();
        for (route, file, content_type) in FILES {
            let pin = pins
                .get(file)
                .with_context(|| format!("pinned client manifest is missing {file}"))?;
            let body = read_regular_bounded(&root.join(file), MAX_FILE_BYTES)?;
            let found = vm_session_auth::hash(&body);
            ensure!(
                found == *pin,
                "pinned client file does not match its manifest digest: {file} (expected {pin}, found {found})"
            );
            entries.insert(
                route,
                Entry {
                    content_type,
                    body: Bytes::from(body),
                },
            );
        }
        Ok(Self {
            manifest_sha256: actual,
            entries,
        })
    }

    /// SHA-256 of the manifest bytes: one value that identifies the whole client.
    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }
}

/// Manifest text for the fixed client as it currently is under `root`. An
/// operator reviews and stores this, and passes its SHA-256 at startup.
pub fn manifest_for(root: &Path) -> Result<String> {
    let mut text = String::new();
    for (_, file, _) in FILES {
        let body = read_regular_bounded(&root.join(file), MAX_FILE_BYTES)?;
        text.push_str(&format!("{}  {file}\n", vm_session_auth::hash(&body)));
    }
    Ok(text)
}

fn reply(status: StatusCode, content_type: &'static str, length: usize, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    for (name, value) in SECURITY_HEADERS {
        headers.insert(name, HeaderValue::from_static(value));
    }
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    response
}

/// A refusal with the same headers as a served file and a fixed short body.
pub fn refuse(status: StatusCode, reason: &'static str) -> Response {
    reply(
        status,
        "text/plain; charset=utf-8",
        reason.len(),
        Body::from(reason),
    )
}

/// Pure request-to-response mapping; no disk access and no path arithmetic.
/// The raw request path is compared byte-for-byte with the fixed routes, so an
/// encoded, dotted, doubled or suffixed path is simply an unknown path.
pub fn respond(bundle: &Bundle, method: &Method, uri: &Uri, headers: &HeaderMap) -> Response {
    if method != Method::GET && method != Method::HEAD {
        let mut response = refuse(StatusCode::METHOD_NOT_ALLOWED, "GET or HEAD only");
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return response;
    }
    // A static read has no use for a request body; refuse rather than drain one.
    let declares_body = headers.contains_key(header::TRANSFER_ENCODING)
        || headers
            .get(header::CONTENT_LENGTH)
            .is_some_and(|length| length.as_bytes() != b"0");
    if declares_body {
        return refuse(StatusCode::BAD_REQUEST, "request body not accepted");
    }
    let entry = match (uri.query(), bundle.entries.get(uri.path())) {
        (None, Some(entry)) => entry,
        _ => return refuse(StatusCode::NOT_FOUND, "not found"),
    };
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(entry.body.clone())
    };
    reply(StatusCode::OK, entry.content_type, entry.body.len(), body)
}

/// Every path that is not one of the service's own API routes goes through the
/// same exact-Host (and Origin, when sent) check as the rest of the listener,
/// then through `respond`.
pub fn mount(router: Router<crate::App>, bundle: Arc<Bundle>) -> Router<crate::App> {
    router.fallback(
        move |State(app): State<crate::App>, method: Method, uri: Uri, headers: HeaderMap| {
            let bundle = bundle.clone();
            async move {
                if let Err(reason) = crate::check(&app, &headers, false) {
                    return refuse(StatusCode::FORBIDDEN, reason);
                }
                respond(&bundle, &method, &uri, &headers)
            }
        },
    )
}

#[cfg(test)]
#[path = "pinned_client_tests.rs"]
mod tests;
