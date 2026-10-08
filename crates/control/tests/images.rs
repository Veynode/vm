#![cfg(target_os = "macos")]
use serde_json::Value;
use std::{
    fs,
    io::Write,
    os::unix::fs::{symlink, PermissionsExt},
    path::Path,
    process::Command,
};
fn image(store: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_vm-image"))
        .args(args)
        .arg("--store")
        .arg(store)
        .output()
        .unwrap()
}
fn fixture(path: &Path) {
    let mut file = fs::File::create(path).unwrap();
    file.write_all(&vec![0x23; 8192]).unwrap();
}
#[test]
fn actual_apfs_clone_is_independent_and_publication_refuses_overwrite() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source.raw");
    fixture(&source);
    let store = tmp.path().join("store");
    let out = image(
        &store,
        &[
            "import",
            "--id",
            "base",
            "--source",
            source.to_str().unwrap(),
            "--source-stopped",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let base: Value = serde_json::from_slice(&out.stdout).unwrap();
    let base_path = std::path::PathBuf::from(base["disk_path"].as_str().unwrap());
    assert_eq!(
        fs::metadata(&base_path).unwrap().permissions().mode() & 0o777,
        0o400
    );
    let before = fs::read(&base_path).unwrap();
    let out = image(&store, &["clone", "--base", "base", "--id", "clone"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let clone: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        base["manifest"]["initial_sha256"],
        clone["manifest"]["initial_sha256"]
    );
    let clone_path = Path::new(clone["disk_path"].as_str().unwrap());
    assert_eq!(
        fs::metadata(clone_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::OpenOptions::new()
        .write(true)
        .open(clone_path)
        .unwrap()
        .write_all(b"CLONE-ONLY-WRITE")
        .unwrap();
    assert_eq!(fs::read(&base_path).unwrap(), before);
    assert_eq!(fs::read(&source).unwrap(), before);
    assert_ne!(fs::read(clone_path).unwrap(), before);
    assert!(
        !image(&store, &["clone", "--base", "base", "--id", "clone"])
            .status
            .success()
    );
    assert_eq!(fs::read(&base_path).unwrap(), before);
    let manifest: Value =
        serde_json::from_slice(&fs::read(store.join("bases/base/manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["virtual_size_bytes"], 8192);
}
#[test]
fn locks_symlinks_space_and_corrupt_base_fail_without_publication() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source.raw");
    fixture(&source);
    let store = tmp.path().join("store");
    let symlinked = tmp.path().join("symlink.raw");
    symlink(&source, &symlinked).unwrap();
    assert!(!image(
        &store,
        &[
            "import",
            "--id",
            "symlink",
            "--source",
            symlinked.to_str().unwrap(),
            "--source-stopped"
        ]
    )
    .status
    .success());
    assert!(!image(
        &store,
        &[
            "import",
            "--id",
            "../escape",
            "--source",
            source.to_str().unwrap(),
            "--source-stopped"
        ]
    )
    .status
    .success());
    assert!(!image(
        &store,
        &[
            "import",
            "--id",
            "directory",
            "--source",
            tmp.path().to_str().unwrap(),
            "--source-stopped"
        ]
    )
    .status
    .success());
    assert!(!image(
        &store,
        &[
            "import",
            "--id",
            "unasserted",
            "--source",
            source.to_str().unwrap()
        ]
    )
    .status
    .success());
    let handle = fs::File::open(&source).unwrap();
    use std::os::unix::io::AsRawFd;
    assert_eq!(
        unsafe { libc::flock(handle.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert!(!image(
        &store,
        &[
            "import",
            "--id",
            "locked",
            "--source",
            source.to_str().unwrap(),
            "--source-stopped"
        ]
    )
    .status
    .success());
    drop(handle);
    assert!(!store.join("bases/locked").exists());
    let out = image(
        &store,
        &[
            "import",
            "--id",
            "base",
            "--source",
            source.to_str().unwrap(),
            "--source-stopped",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let before = fs::read(store.join("bases/base/disk.raw")).unwrap();
    let space = image(
        &store,
        &[
            "clone",
            "--base",
            "base",
            "--id",
            "no-space",
            "--min-free-mib",
            "17592186044415",
        ],
    );
    assert!(!space.status.success());
    assert!(String::from_utf8_lossy(&space.stderr).contains("insufficient free space"));
    assert!(!store.join("clones/no-space").exists());
    assert_eq!(fs::read(store.join("bases/base/disk.raw")).unwrap(), before);
    let disk = store.join("bases/base/disk.raw");
    fs::set_permissions(&disk, fs::Permissions::from_mode(0o600)).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&disk)
        .unwrap()
        .write_all(b"corrupted")
        .unwrap();
    fs::set_permissions(&disk, fs::Permissions::from_mode(0o400)).unwrap();
    let out = image(&store, &["clone", "--base", "base", "--id", "corrupt"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("integrity"));
    assert!(!store.join("clones/corrupt").exists());
}
