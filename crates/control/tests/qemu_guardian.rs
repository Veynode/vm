#[test]
fn actual_guardian_reaps_owned_child_after_eof_or_stopped_owner() {
    let result = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/qemu_guardian.py"
        ))
        .arg(env!("CARGO_BIN_EXE_vm-qemu"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    println!("{}", String::from_utf8_lossy(&result.stdout));
}
