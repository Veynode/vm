//! What the end-to-end tests run besides `vm-mcp-http`.
use std::path::PathBuf;

/// The core's `vm-control`, from the same target directory: its stdio MCP
/// server is what `vm-mcp-http` runs per session. `scripts/check-changed.py`
/// and `scripts/check.sh` build it before these tests.
pub fn control() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_vm-mcp-http")).with_file_name("vm-control");
    assert!(
        path.is_file(),
        "{} is missing: cargo build -p vm-control --bin vm-control first",
        path.display()
    );
    path
}

/// `VEYNODE_PYTHON`, else the first of python3.13, python3.12 and python3 on
/// PATH, as `scripts/check.sh` picks: TLS 1.3 needs a modern OpenSSL, and the
/// macOS system python3 links LibreSSL 2.8.
pub fn python() -> String {
    if let Ok(python) = std::env::var("VEYNODE_PYTHON") {
        return python;
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    ["python3.13", "python3.12"]
        .into_iter()
        .find(|name| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
        .unwrap_or("python3")
        .into()
}
