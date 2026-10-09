//! File-boundary checks only; no Guest, capture or running media service.
use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};

struct PrivateFile {
    _dir: tempfile::TempDir,
    directory: std::path::PathBuf,
    path: std::path::PathBuf,
}
fn valid() -> Value {
    json!({"version":1,"service_instance":"a".repeat(32),"capture_instance":"b".repeat(32),"cleanup_retention_ms":120000})
}
impl PrivateFile {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let directory = dir.path().canonicalize().unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("receipt.json");
        let fixture = Self {
            _dir: dir,
            directory,
            path,
        };
        fixture.write(&serde_json::to_vec(&valid()).unwrap());
        fixture
    }
    fn write(&self, bytes: &[u8]) {
        fs::write(&self.path, bytes).unwrap();
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}
#[test]
fn exact_private_receipt_is_accepted_and_nonprivate_modes_are_rejected() {
    let fixture = PrivateFile::new();
    let ticket = Ticket::read(&fixture.path).unwrap();
    assert_eq!(ticket.service_instance, "a".repeat(32));
    assert_eq!(ticket.capture_instance, "b".repeat(32));
    for mode in [0o644, 0o640, 0o604, 0o660, 0o400] {
        fs::set_permissions(&fixture.path, fs::Permissions::from_mode(mode)).unwrap();
        assert!(Ticket::read(&fixture.path).is_err(), "file mode {mode:o}");
    }
    fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o600)).unwrap();
    for mode in [0o755, 0o750, 0o707, 0o770] {
        fs::set_permissions(&fixture.directory, fs::Permissions::from_mode(mode)).unwrap();
        assert!(Ticket::read(&fixture.path).is_err(), "parent mode {mode:o}");
    }
}
#[test]
fn direct_parent_and_ancestor_symlinks_do_not_select_a_receipt() {
    let fixture = PrivateFile::new();
    let direct = fixture.directory.join("link.json");
    symlink(&fixture.path, &direct).unwrap();
    assert!(Ticket::read(&direct).is_err());
    let child = fixture.directory.join("real");
    fs::create_dir(&child).unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();
    fs::rename(&fixture.path, child.join("receipt.json")).unwrap();
    let alias = fixture.directory.join("alias");
    symlink(&child, &alias).unwrap();
    assert!(Ticket::read(&alias.join("receipt.json")).is_err());
    let deeper = child.join("deeper");
    fs::create_dir(&deeper).unwrap();
    fs::set_permissions(&deeper, fs::Permissions::from_mode(0o700)).unwrap();
    fs::rename(child.join("receipt.json"), deeper.join("receipt.json")).unwrap();
    assert!(Ticket::read(&alias.join("deeper/receipt.json")).is_err());
    assert!(Ticket::read(&deeper.join("receipt.json")).is_ok());
}
#[test]
fn hardlink_fifo_directory_and_relative_path_are_refused_without_blocking() {
    let fixture = PrivateFile::new();
    let link = fixture.directory.join("second-name");
    fs::hard_link(&fixture.path, &link).unwrap();
    assert!(Ticket::read(&fixture.path).is_err());
    assert!(Ticket::read(&link).is_err());
    fs::remove_file(&link).unwrap();
    fs::remove_file(&fixture.path).unwrap();
    let c = std::ffi::CString::new(fixture.path.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let before = std::time::Instant::now();
    assert!(Ticket::read(&fixture.path).is_err());
    assert!(before.elapsed() < std::time::Duration::from_millis(500));
    fs::remove_file(&fixture.path).unwrap();
    fs::create_dir(&fixture.path).unwrap();
    assert!(Ticket::read(&fixture.path).is_err());
    assert!(Ticket::read(Path::new("relative-receipt.json")).is_err());
}
#[test]
fn partial_empty_oversized_and_multiple_json_values_are_not_cleanup_tickets() {
    let fixture = PrivateFile::new();
    let bytes = serde_json::to_vec(&valid()).unwrap();
    for body in [
        vec![],
        bytes[..bytes.len() - 1].to_vec(),
        b"{\"version\":1,".to_vec(),
        [bytes.clone(), bytes.clone()].concat(),
        vec![b'x'; 4097],
    ] {
        fixture.write(&body);
        assert!(Ticket::read(&fixture.path).is_err());
    }
    fixture.write(&bytes);
    assert!(Ticket::read(&fixture.path).is_ok());
}
#[test]
fn receipt_schema_requires_exact_ids_version_retention_and_rejects_duplicate_fields() {
    let fixture = PrivateFile::new();
    for (key, bad) in [
        ("version", json!(true)),
        ("version", json!(0)),
        ("version", json!(1.5)),
        ("service_instance", json!("A".repeat(32))),
        ("service_instance", json!("a".repeat(31))),
        ("capture_instance", json!("b".repeat(33))),
        ("capture_instance", Value::Null),
        ("cleanup_retention_ms", json!(120001)),
        ("cleanup_retention_ms", json!(false)),
        ("cleanup_retention_ms", json!(-1)),
        ("extra", json!("untrusted")),
    ] {
        let mut value = valid();
        value[key] = bad;
        fixture.write(&serde_json::to_vec(&value).unwrap());
        assert!(Ticket::read(&fixture.path).is_err(), "{key}");
    }
    for key in [
        "version",
        "service_instance",
        "capture_instance",
        "cleanup_retention_ms",
    ] {
        let mut value = valid();
        value.as_object_mut().unwrap().remove(key);
        fixture.write(&serde_json::to_vec(&value).unwrap());
        assert!(Ticket::read(&fixture.path).is_err(), "missing {key}");
    }
    let original = serde_json::to_string(&valid()).unwrap();
    for field in [
        "\"version\":1",
        "\"capture_instance\":\"cccccccccccccccccccccccccccccccc\"",
    ] {
        let duplicate = format!("{{{field},{}", &original[1..]);
        fixture.write(duplicate.as_bytes());
        assert!(Ticket::read(&fixture.path).is_err(), "duplicate {field}");
    }
}
