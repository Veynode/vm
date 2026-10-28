mod support;

#[test]
fn actual_https_subprocess_signals_and_cleanup_outcomes() {
    let output = std::process::Command::new(support::python())
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/http_shutdown.py"
        ))
        .arg(env!("CARGO_BIN_EXE_vm-mcp-http"))
        .arg(support::control())
        .output()
        .expect("Python 3 plus OpenSSL required for real TLS signal tests");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let evidence: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(evidence["passed"], true);
    assert_eq!(evidence["checks"].as_array().unwrap().len(), 14);
    println!("{}", String::from_utf8_lossy(&output.stdout));
}

#[test]
fn actual_https_graphics_creation_remains_denied() {
    let output = std::process::Command::new(support::python())
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/http_shutdown.py"
        ))
        .arg(env!("CARGO_BIN_EXE_vm-mcp-http"))
        .arg(support::control())
        .arg("--graphics-only")
        .output()
        .expect("Python/OpenSSL required for actual TLS negative test");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let evidence: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(evidence["passed"], true);
    assert_eq!(evidence["checks"].as_array().unwrap().len(), 1);
    println!("{}", String::from_utf8_lossy(&output.stdout));
}
