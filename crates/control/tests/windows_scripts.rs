//! The PowerShell scripts the daemon runs in Windows guests over SSH (`guest/windows`). The daemon writes requests and
//! reads replies as UTF-8, while an SSH session's console uses the OEM code page (437 on an English Windows), which
//! turns "ä" into byte 0x84 and CJK text into "?". So every script that reads stdin or writes stdout sets UTF-8 console
//! encoding before its first console read or write. Static checks of the script text; the live check is in
//! crates/control/README.md (Windows machines over SSH).
use std::path::Path;

const UTF8: &str =
    "[Console]::InputEncoding = [Console]::OutputEncoding = New-Object Text.UTF8Encoding $false";
/// Console reads and writes, including PowerShell's own pipeline output.
const CONSOLE_IO: [&str; 6] = [
    "[Console]::In",
    "[Console]::Out",
    "OpenStandardInput",
    "OpenStandardOutput",
    "ConvertTo-Json",
    "Write-Output",
];

fn script(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../guest/windows")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Code lines, without comments.
fn code(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect()
}

#[test]
fn ssh_scripts_set_utf8_console_encoding_before_console_io() {
    for name in ["session.ps1", "exec.ps1", "files.ps1"] {
        let text = script(name);
        let lines = code(&text);
        let set = lines
            .iter()
            .position(|line| line.trim() == UTF8)
            .unwrap_or_else(|| panic!("{name} does not set UTF-8 console encoding"));
        let first_io = lines
            .iter()
            .position(|line| line.trim() != UTF8 && CONSOLE_IO.iter().any(|io| line.contains(io)))
            .unwrap_or_else(|| panic!("{name} has no console I/O"));
        assert!(
            set < first_io,
            "{name}: console I/O on line {} before UTF-8 encoding on line {}",
            first_io + 1,
            set + 1
        );
    }
}

#[test]
fn desktop_script_uses_files_not_the_console() {
    // desktop.ps1 runs in a hidden task without a usable console; it exchanges UTF-8 files with session.ps1.
    let text = script("desktop.ps1");
    for line in code(&text) {
        for io in [
            "[Console]::In",
            "[Console]::Out",
            "OpenStandardInput",
            "OpenStandardOutput",
        ] {
            assert!(!line.contains(io), "desktop.ps1 uses {io}: {line}");
        }
    }
    assert!(text.contains(r#"[IO.File]::ReadAllText("$Dir\request.json")"#));
    assert!(text.contains(r#"[IO.File]::WriteAllText("$Dir\out.json", $json, $utf8)"#));
    let session = script("session.ps1");
    assert!(session.contains(
        r#"[IO.File]::WriteAllText("$dir\request.json", $stdin.Substring(0, $newline), $utf8)"#
    ));
    assert!(session.contains(r#"[Console]::Out.Write([IO.File]::ReadAllText("$dir\out.json"))"#));
}
