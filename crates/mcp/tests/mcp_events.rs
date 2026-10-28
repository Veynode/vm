//! Pushed MCP events through the real TLS endpoint and `vm-control`'s stdio
//! server, against a synthetic daemon socket (crates/control/tests/mcp_events.py).
//! No VM or Guest.
mod support;

#[test]
fn https_events_are_streamed_filtered_and_bounded() {
    let output = std::process::Command::new(support::python())
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../control/tests/mcp_events.py"
        ))
        .arg(support::control())
        .arg("https")
        .arg(env!("CARGO_BIN_EXE_vm-mcp-http"))
        .output()
        .expect("Python 3 with OpenSSL for TLS 1.3 required");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let evidence: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(evidence["passed"], true);
    assert!(evidence["checks"].as_array().unwrap().len() >= 30);
}
