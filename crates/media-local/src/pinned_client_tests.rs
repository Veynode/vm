//! Synthetic client files in a private temporary directory; no listener, no
//! Keychain, no encoder. The router test uses the real `check` and `mount`.
use super::*;
use axum::body::to_bytes;
use std::fs;
use tower::ServiceExt;

/// Distinct, non-trivial bytes per file, including non-UTF-8 and a NUL, so a
/// byte-identical comparison cannot pass through any text conversion.
fn content(file: &str) -> Vec<u8> {
    let mut bytes = format!("synthetic client file {file}\n").into_bytes();
    bytes.extend_from_slice(&[0x00, 0xff, 0xfe, b'\r', b'\n']);
    bytes.extend(file.bytes().rev());
    bytes
}

struct Fixture {
    directory: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("web")).unwrap();
        fs::create_dir(directory.path().join("web/dist")).unwrap();
        for (_, file, _) in FILES {
            fs::write(directory.path().join("web").join(file), content(file)).unwrap();
        }
        Self { directory }
    }
    fn web(&self) -> PathBuf {
        self.directory.path().join("web")
    }
    /// Writes `manifest` and returns options pinning exactly those bytes.
    fn pin(&self, manifest: &str) -> Options {
        let path = self.directory.path().join("client.sha256");
        fs::write(&path, manifest).unwrap();
        Options {
            manifest: path,
            manifest_sha256: vm_session_auth::hash(manifest.as_bytes()),
        }
    }
    fn options(&self) -> Options {
        self.pin(&manifest_for(&self.web()).unwrap())
    }
    fn bundle(&self) -> Bundle {
        Bundle::load(&self.web(), &self.options()).unwrap()
    }
}

fn refusal(result: Result<Bundle>) -> String {
    match result {
        Ok(_) => panic!("a mismatching client was accepted"),
        Err(error) => format!("{error:#}"),
    }
}

fn request(bundle: &Bundle, method: Method, target: &str) -> Response {
    respond(
        bundle,
        &method,
        &target.parse::<Uri>().unwrap(),
        &HeaderMap::new(),
    )
}

async fn body(response: Response) -> Vec<u8> {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

fn assert_security_headers(response: &Response) {
    for (name, value) in SECURITY_HEADERS {
        let found: Vec<_> = response.headers().get_all(name).iter().collect();
        assert_eq!(found.len(), 1, "{name} must appear exactly once");
        assert_eq!(found[0], value, "{name}");
    }
}

#[tokio::test]
async fn every_manifest_file_is_served_byte_identical_with_its_type_and_headers() {
    let fixture = Fixture::new();
    let bundle = fixture.bundle();
    assert_eq!(bundle.entries.len(), FILES.len());
    for (route, file, content_type) in FILES {
        let expected = fs::read(fixture.web().join(file)).unwrap();
        let response = request(&bundle, Method::GET, route);
        assert_eq!(response.status(), StatusCode::OK, "{route}");
        assert_security_headers(&response);
        assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            expected.len().to_string().as_str()
        );
        assert_eq!(body(response).await, expected, "{route}");

        let head = request(&bundle, Method::HEAD, route);
        assert_eq!(head.status(), StatusCode::OK, "{route}");
        assert_security_headers(&head);
        assert_eq!(head.headers()[header::CONTENT_TYPE], content_type);
        assert_eq!(
            head.headers()[header::CONTENT_LENGTH],
            expected.len().to_string().as_str()
        );
        assert!(body(head).await.is_empty(), "HEAD carries no body");
    }
}

#[test]
fn fixed_client_matches_the_page_and_its_content_types() {
    let routes: BTreeSet<_> = FILES.iter().map(|(route, _, _)| *route).collect();
    let files: BTreeSet<_> = FILES.iter().map(|(_, file, _)| *file).collect();
    assert_eq!(routes.len(), FILES.len(), "duplicate route");
    assert_eq!(files.len(), FILES.len(), "duplicate file");
    for (route, file, content_type) in FILES {
        if route == "/" {
            assert_eq!((file, content_type), ("native.html", HTML));
            continue;
        }
        assert_eq!(route, format!("/{file}"));
        let expected = match file.rsplit_once('.').unwrap().1 {
            "js" => SCRIPT,
            "css" => STYLE,
            other => panic!("unexpected extension {other}"),
        };
        assert_eq!(content_type, expected, "{file}");
    }
}

#[test]
fn served_bytes_do_not_follow_later_disk_changes() {
    let fixture = Fixture::new();
    let bundle = fixture.bundle();
    fs::write(
        fixture.web().join("dist/native.js"),
        b"replaced after startup",
    )
    .unwrap();
    assert_eq!(
        bundle.entries["/dist/native.js"].body.as_ref(),
        content("dist/native.js").as_slice()
    );
}

#[test]
fn any_file_differing_from_its_pin_refuses_startup() {
    for (_, file, _) in FILES {
        let fixture = Fixture::new();
        let options = fixture.options();
        let mut changed = content(file);
        *changed.last_mut().unwrap() ^= 1;
        fs::write(fixture.web().join(file), changed).unwrap();
        let error = refusal(Bundle::load(&fixture.web(), &options));
        assert!(
            error.contains("does not match its manifest digest") && error.contains(file),
            "{file}: {error}"
        );
    }
}

#[test]
fn appended_truncated_or_missing_file_refuses_startup() {
    let fixture = Fixture::new();
    let options = fixture.options();
    let path = fixture.web().join("dist/session-auth.js");
    let original = content("dist/session-auth.js");
    let mut appended = original.clone();
    appended.push(b'\n');
    fs::write(&path, appended).unwrap();
    assert!(refusal(Bundle::load(&fixture.web(), &options)).contains("does not match"));
    fs::write(&path, &original[..original.len() - 1]).unwrap();
    assert!(refusal(Bundle::load(&fixture.web(), &options)).contains("does not match"));
    fs::write(&path, b"").unwrap();
    assert!(refusal(Bundle::load(&fixture.web(), &options)).contains("does not match"));
    fs::remove_file(&path).unwrap();
    assert!(refusal(Bundle::load(&fixture.web(), &options)).contains("unavailable"));
    fs::write(&path, original).unwrap();
    Bundle::load(&fixture.web(), &options).unwrap();
}

#[test]
fn manifest_digest_mismatch_refuses_startup() {
    let fixture = Fixture::new();
    let mut options = fixture.options();
    let pinned = options.manifest_sha256.clone();

    // The manifest was rewritten to match a changed file; the digest pin was not.
    fs::write(fixture.web().join("native.html"), b"<!doctype html>changed").unwrap();
    fs::write(&options.manifest, manifest_for(&fixture.web()).unwrap()).unwrap();
    let error = refusal(Bundle::load(&fixture.web(), &options));
    assert!(error.contains("manifest digest mismatch"), "{error}");

    // A different, well-formed digest.
    options.manifest_sha256 = vm_session_auth::hash(b"some other manifest");
    assert!(refusal(Bundle::load(&fixture.web(), &options)).contains("manifest digest mismatch"));

    // Not a digest at all: uppercase, short, empty.
    for malformed in [
        pinned.to_uppercase(),
        pinned[..63].to_owned(),
        String::new(),
    ] {
        options.manifest_sha256 = malformed;
        assert!(
            refusal(Bundle::load(&fixture.web(), &options)).contains("64 lowercase hex"),
            "malformed digest accepted"
        );
    }
}

#[test]
fn manifest_must_name_exactly_the_fixed_client() {
    let fixture = Fixture::new();
    let good = manifest_for(&fixture.web()).unwrap();
    let extra_digest = vm_session_auth::hash(b"extra");
    fs::write(fixture.web().join("dist/extra.js"), b"extra").unwrap();

    let missing: String = good
        .lines()
        .filter(|line| !line.ends_with("  dist/ice-config.js"))
        .map(|line| format!("{line}\n"))
        .collect();
    let error = refusal(Bundle::load(&fixture.web(), &fixture.pin(&missing)));
    assert!(error.contains("missing dist/ice-config.js"), "{error}");

    for outside in [
        "dist/extra.js",
        "../web/native.html",
        "/etc/hosts",
        "dist/../native.html",
        "./native.html",
    ] {
        let manifest = format!("{good}{extra_digest}  {outside}\n");
        let error = refusal(Bundle::load(&fixture.web(), &fixture.pin(&manifest)));
        assert!(
            error.contains("outside the fixed client"),
            "{outside}: {error}"
        );
    }

    let first = good.lines().next().unwrap();
    let duplicate = format!("{good}{first}\n");
    assert!(refusal(Bundle::load(&fixture.web(), &fixture.pin(&duplicate))).contains("duplicate"));
}

#[test]
fn malformed_manifest_refuses_startup() {
    let fixture = Fixture::new();
    let good = manifest_for(&fixture.web()).unwrap();
    let digest = &good[..64];
    let malformed = [
        String::new(),
        "\n".to_owned(),
        good.trim_end().to_owned(),
        good.replace('\n', "\r\n"),
        format!("{good}\n"),
        good.replacen("  ", " ", 1),
        good.replacen("  ", " *", 1),
        good.replacen(digest, &digest.to_uppercase(), 1),
        good.replacen(digest, &digest[..63], 1),
        good.replacen(digest, &format!("{}é", &digest[..62]), 1),
        format!("{digest}  \n"),
    ];
    for manifest in malformed {
        let result = Bundle::load(&fixture.web(), &fixture.pin(&manifest));
        assert!(result.is_err(), "accepted malformed manifest {manifest:?}");
    }
    let options = fixture.pin(&good);
    fs::write(
        &options.manifest,
        [good.as_bytes(), &[0xff, b'\n']].concat(),
    )
    .unwrap();
    assert!(Bundle::load(&fixture.web(), &options).is_err());
}

#[cfg(unix)]
#[test]
fn symlinked_directory_or_oversized_file_refuses_startup() {
    let fixture = Fixture::new();
    let options = fixture.options();
    let path = fixture.web().join("probe.css");

    // Same bytes, so only the file kind differs from the accepted case.
    let elsewhere = fixture.directory.path().join("elsewhere.css");
    fs::rename(&path, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
    let error = refusal(Bundle::load(&fixture.web(), &options));
    assert!(error.contains("not a regular file"), "{error}");
    fs::remove_file(&path).unwrap();

    fs::create_dir(&path).unwrap();
    let error = refusal(Bundle::load(&fixture.web(), &options));
    assert!(error.contains("not a regular file"), "{error}");
    fs::remove_dir(&path).unwrap();

    let oversized = vec![b' '; MAX_FILE_BYTES as usize + 1];
    fs::write(&path, &oversized).unwrap();
    let manifest = manifest_for(&fixture.web());
    assert!(manifest.is_err(), "oversized file entered a manifest");
    let error = refusal(Bundle::load(&fixture.web(), &options));
    assert!(error.contains("exceeds"), "{error}");
}

#[tokio::test]
async fn traversal_and_unlisted_paths_are_not_found() {
    let fixture = Fixture::new();
    // Real files next to the client that are not part of it.
    fs::write(fixture.web().join("dist/probe.js"), b"unlisted sibling").unwrap();
    fs::write(fixture.web().join("index.html"), b"unlisted page").unwrap();
    fs::write(fixture.directory.path().join("secret"), b"outside root").unwrap();
    let bundle = fixture.bundle();
    let targets = [
        "/native.html",
        "/index.html",
        "/dist",
        "/dist/",
        "/dist/probe.js",
        "/dist/native.js/",
        "/dist/native.js/.",
        "/dist/native.js%00",
        "/dist/native.js.map",
        "/dist/native.d.ts",
        "/dist/NATIVE.JS",
        "/dist//native.js",
        "//dist/native.js",
        "/./dist/native.js",
        "/dist/./native.js",
        "/dist/../dist/native.js",
        "/dist/../native.html",
        "/dist/../../secret",
        "/../secret",
        "/..%2fsecret",
        "/%2e%2e/secret",
        "/%2e%2e%2fsecret",
        "/dist/%2e%2e/%2e%2e/secret",
        "/dist%2fnative.js",
        "/dist/native%2ejs",
        "/dist\\native.js",
        "/..\\secret",
        "/client.sha256",
        "/probe.css/../probe.css",
        "/favicon.ico",
        "/?",
        "/?a=1",
        "/dist/native.js?v=1",
        "/probe.css?",
        "*",
    ];
    for target in targets {
        for method in [Method::GET, Method::HEAD] {
            let response = request(&bundle, method, target);
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{target}");
            assert_security_headers(&response);
            let bytes = body(response).await;
            assert_eq!(bytes, b"not found", "{target}");
        }
    }
}

#[tokio::test]
async fn only_get_and_head_are_served() {
    let bundle = Fixture::new().bundle();
    for name in [
        "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "TRACE", "CONNECT", "PROPFIND", "get",
    ] {
        let method = Method::from_bytes(name.as_bytes()).unwrap();
        for target in ["/", "/dist/native.js", "/missing", "/dist/../native.html"] {
            let response = request(&bundle, method.clone(), target);
            assert_eq!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{name} {target}"
            );
            assert_eq!(response.headers()[header::ALLOW], "GET, HEAD");
            assert_security_headers(&response);
            assert_eq!(body(response).await, b"GET or HEAD only");
        }
    }
}

#[tokio::test]
async fn request_bodies_are_refused() {
    let bundle = Fixture::new().bundle();
    let uri: Uri = "/".parse().unwrap();
    for (name, value) in [
        (header::CONTENT_LENGTH, "1"),
        (header::CONTENT_LENGTH, "262145"),
        (header::CONTENT_LENGTH, "00"),
        (header::TRANSFER_ENCODING, "chunked"),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(name, HeaderValue::from_static(value));
        let response = respond(&bundle, &Method::GET, &uri, &headers);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{value}");
        assert_security_headers(&response);
        assert_eq!(body(response).await, b"request body not accepted");
    }
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    let response = respond(&bundle, &Method::GET, &uri, &headers);
    assert_eq!(response.status(), StatusCode::OK);
}

#[test]
fn header_set_is_exact() {
    let policy: BTreeMap<&str, Vec<&str>> = CONTENT_SECURITY_POLICY
        .split("; ")
        .map(|directive| {
            let mut words = directive.split(' ');
            (words.next().unwrap(), words.collect())
        })
        .collect();
    let expected: BTreeMap<&str, Vec<&str>> = [
        ("default-src", vec!["'none'"]),
        ("script-src", vec!["'self'"]),
        ("style-src", vec!["'self'"]),
        ("connect-src", vec!["'self'"]),
        ("img-src", vec!["data:"]),
        ("base-uri", vec!["'none'"]),
        ("form-action", vec!["'none'"]),
        ("frame-ancestors", vec!["'none'"]),
    ]
    .into();
    assert_eq!(policy, expected);
    assert_eq!(
        CONTENT_SECURITY_POLICY.split("; ").count(),
        expected.len(),
        "duplicate directive"
    );
    for forbidden in ["unsafe-", "*", "http:", "https:", "ws:", "wss:", "blob:"] {
        assert!(!CONTENT_SECURITY_POLICY.contains(forbidden), "{forbidden}");
    }
    let headers: BTreeMap<_, _> = SECURITY_HEADERS.into();
    assert_eq!(headers.len(), SECURITY_HEADERS.len(), "duplicate header");
    assert_eq!(headers["content-security-policy"], CONTENT_SECURITY_POLICY);
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["referrer-policy"], "no-referrer");
    assert_eq!(headers["cross-origin-opener-policy"], "same-origin");
    assert_eq!(headers["cross-origin-resource-policy"], "same-origin");
    assert_eq!(headers["cache-control"], "no-store");
}

#[test]
fn options_are_off_by_default_and_required_together() {
    let args = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
    let other = args(&["--guest-stream", "--auth-state", "private"]);
    assert_eq!(parse_options(&other).unwrap(), (None, other.clone()));
    assert_eq!(parse_options(&[]).unwrap(), (None, Vec::new()));

    let digest = vm_session_auth::hash(b"manifest");
    let both = args(&[
        "--guest-stream",
        "--pinned-client",
        "client.sha256",
        "--auth-state",
        "private",
        "--pinned-client-sha256",
        &digest,
    ]);
    let (options, remaining) = parse_options(&both).unwrap();
    assert_eq!(
        options,
        Some(Options {
            manifest: PathBuf::from("client.sha256"),
            manifest_sha256: digest.clone(),
        })
    );
    assert_eq!(remaining, other);

    for refused in [
        args(&["--pinned-client", "client.sha256"]),
        args(&["--pinned-client-sha256", &digest]),
        args(&["--pinned-client"]),
        args(&["--pinned-client", "a", "--pinned-client-sha256"]),
        args(&[
            "--pinned-client",
            "a",
            "--pinned-client",
            "b",
            "--pinned-client-sha256",
            &digest,
        ]),
        args(&[
            "--pinned-client",
            "a",
            "--pinned-client-sha256",
            &digest,
            "--pinned-client-sha256",
            &digest,
        ]),
    ] {
        assert!(parse_options(&refused).is_err(), "{refused:?}");
    }
}

fn app() -> crate::App {
    crate::App {
        origin: "http://127.0.0.1:8787".into(),
        host: "127.0.0.1:8787".into(),
        encoder: PathBuf::new(),
        socket: PathBuf::new(),
        web: PathBuf::new(),
        capabilities: serde_json::json!({}),
        fault_au_egress_hold: false,
        fault_drop_first_keyframe: false,
        fault_suppress_hevc_frames: false,
        active: Arc::new(tokio::sync::Mutex::new(crate::SessionBook::default())),
        auth: None,
        cleanup_failure_exit: None,
        guest_binding: None,
        encoder_rate: None,
    }
}

#[tokio::test]
async fn mounted_fallback_keeps_host_and_origin_checks_and_api_routes() {
    let fixture = Fixture::new();
    let router = mount(
        Router::new().route(
            "/capabilities",
            axum::routing::get(|| async { "api route" }),
        ),
        Arc::new(fixture.bundle()),
    )
    .with_state(app());
    let send = |method: &str, target: &str, host: Option<&str>, origin: Option<&str>| {
        let mut request = axum::http::Request::builder().method(method).uri(target);
        if let Some(host) = host {
            request = request.header(header::HOST, host);
        }
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        router.clone().oneshot(request.body(Body::empty()).unwrap())
    };
    let good = Some("127.0.0.1:8787");

    let page = send("GET", "/", good, None).await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert_security_headers(&page);
    assert_eq!(body(page).await, content("native.html"));

    let same_origin = send(
        "GET",
        "/dist/native.js",
        good,
        Some("http://127.0.0.1:8787"),
    );
    assert_eq!(same_origin.await.unwrap().status(), StatusCode::OK);

    for (host, origin) in [
        (None, None),
        (Some("localhost:8787"), None),
        (Some("127.0.0.1"), None),
        (Some("attacker.example"), None),
        (good, Some("https://attacker.example")),
        (good, Some("null")),
    ] {
        for target in ["/", "/dist/native.js", "/missing"] {
            let response = send("GET", target, host, origin).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{host:?} {origin:?}"
            );
            assert_security_headers(&response);
            let bytes = body(response).await;
            assert!(
                !bytes.starts_with(b"synthetic"),
                "content served cross-origin"
            );
        }
    }

    // Paths reach the fallback exactly as sent: the router does not normalize.
    for target in [
        "/dist/../native.html",
        "/dist//native.js",
        "/%2e%2e/secret",
        "/nope",
    ] {
        let response = send("GET", target, good, None).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{target}");
        assert_security_headers(&response);
    }
    let post = send("POST", "/", good, Some("http://127.0.0.1:8787"))
        .await
        .unwrap();
    assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_security_headers(&post);

    let api = send("GET", "/capabilities", good, None).await.unwrap();
    assert_eq!(api.status(), StatusCode::OK);
    assert_eq!(body(api).await, b"api route");
}
