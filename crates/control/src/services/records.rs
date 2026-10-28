//! The versions of the records under the home, `container.json` and
//! `machine.json`, and the steps between them.
//!
//! A record carries `schema`; one without it is version 1 (every record
//! written before the field). Reading an older record upgrades it in place,
//! after keeping the original beside it as `<file>.v<N>`; a record newer than
//! this daemon is refused and left as it is, so a downgrade never drops the
//! fields it does not know. `registry.json` has its own sidecar version
//! (`crate::REGISTRY_SCHEMA`).
use super::util;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use veynodevm_api::{refuse, ErrorCode};

/// The records this module versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Container,
    Machine,
}

/// `container.json`: version 1 is every record so far.
pub const CONTAINER_SCHEMA: u64 = 1;
/// `machine.json`: version 2 always has an `id` (version 1 records from
/// before ids got one when read).
pub const MACHINE_SCHEMA: u64 = 2;

impl Kind {
    pub fn current(self) -> u64 {
        match self {
            Kind::Container => CONTAINER_SCHEMA,
            Kind::Machine => MACHINE_SCHEMA,
        }
    }
    pub fn file(self) -> &'static str {
        match self {
            Kind::Container => "container.json",
            Kind::Machine => "machine.json",
        }
    }
}

/// The version `record` says it is.
pub fn version(kind: Kind, record: &Value) -> Result<u64> {
    match record.get("schema") {
        None => Ok(1),
        Some(value) => value
            .as_u64()
            .filter(|&v| v >= 1)
            .with_context(|| format!("{}: schema must be a version number", kind.file())),
    }
}

/// `record` brought to this daemon's version, and the version it was.
pub fn upgrade(kind: Kind, mut record: Value) -> Result<(Value, u64)> {
    let found = version(kind, &record)?;
    let current = kind.current();
    if found > current {
        return Err(refuse(
            ErrorCode::State,
            format!(
                "{} is schema {found}, newer than this daemon reads ({current}); run the VeynodeVM that wrote it",
                kind.file()
            ),
        ));
    }
    for from in found..current {
        step(kind, from, &mut record)?;
    }
    record["schema"] = json!(current);
    Ok((record, found))
}

/// One version forward.
fn step(kind: Kind, from: u64, record: &mut Value) -> Result<()> {
    match (kind, from) {
        (Kind::Machine, 1) => {
            if record["id"].as_str().is_none_or(str::is_empty) {
                record["id"] = json!(util::random_hex(6));
            }
        }
        _ => anyhow::bail!("{}: no step from schema {from}", kind.file()),
    }
    Ok(())
}

/// The record at `path`, upgraded in place when it is older.
pub fn read(kind: Kind, path: &Path) -> Result<Value> {
    let bytes = fs::read(path)?;
    let (record, found) = upgrade(kind, serde_json::from_slice(&bytes)?)?;
    if found < kind.current() {
        let backup = backup_path(path, found);
        if !backup.exists() {
            write_private(&backup, &bytes)?;
        }
        write(path, &record)?;
    }
    Ok(record)
}

fn backup_path(path: &Path, version: u64) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".v{version}"));
    path.with_file_name(name)
}

/// `record` at `path` with this daemon's version, through a temporary file.
pub fn write(path: &Path, record: &Value) -> Result<()> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    write_private(&tmp, &serde_json::to_vec_pretty(record)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// `value` serialized with this daemon's version for `kind`.
pub fn stamped(kind: Kind, value: impl serde::Serialize) -> Result<Value> {
    let mut record = serde_json::to_value(value)?;
    record["schema"] = json!(kind.current());
    Ok(record)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Every record under `containers` and `machines` brought to this daemon's
/// version, as the daemon starts. A record that cannot be is left as it is
/// and named in the answer; its methods refuse it until then.
pub fn migrate(containers: &Path, machines: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    for (kind, dir) in [(Kind::Container, containers), (Kind::Machine, machines)] {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path().join(kind.file());
            if path.is_file() {
                if let Err(error) = read(kind, &path) {
                    problems.push(format!("{}: {error:#}", path.display()));
                }
            }
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &Path, kind: Kind, name: &str, record: Value) -> PathBuf {
        let parent = dir.join(name);
        fs::create_dir_all(&parent).unwrap();
        let path = parent.join(kind.file());
        fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        path
    }

    fn json_at(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn a_machine_from_before_ids_gets_one_and_keeps_its_original() {
        let temp = tempfile::tempdir().unwrap();
        let old = json!({"name":"u","backend":"qemu-hvf","disk_gib":32,"created_at":0});
        let path = fixture(temp.path(), Kind::Machine, "u", old.clone());
        let record = read(Kind::Machine, &path).unwrap();
        assert_eq!(record["schema"], 2);
        assert_eq!(record["id"].as_str().unwrap().len(), 12);
        assert_eq!(json_at(&path), record);
        assert_eq!(json_at(&path.with_file_name("machine.json.v1")), old);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Read again: already current, nothing rewritten.
        assert_eq!(read(Kind::Machine, &path).unwrap(), record);
    }

    #[test]
    fn a_version_1_machine_with_an_id_keeps_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = fixture(
            temp.path(),
            Kind::Machine,
            "w",
            json!({"name":"w","id":"abc123abc123","backend":"qemu-hvf","os":"windows","disk_gib":64,"created_at":5}),
        );
        let record = read(Kind::Machine, &path).unwrap();
        assert_eq!(record["id"], "abc123abc123");
        assert_eq!(record["os"], "windows");
    }

    #[test]
    fn a_current_container_is_not_rewritten() {
        let temp = tempfile::tempdir().unwrap();
        let old = json!({"id":"c1","name":"web","image":"alpine"});
        let path = fixture(temp.path(), Kind::Container, "c1", old.clone());
        let record = read(Kind::Container, &path).unwrap();
        assert_eq!(record["schema"], 1);
        // Version 1 is current: the file stays as it was, with no backup.
        assert_eq!(json_at(&path), old);
        assert!(!path.with_file_name("container.json.v1").exists());
    }

    #[test]
    fn a_newer_record_is_refused_and_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        let newer = json!({"schema":9,"name":"u","future":true});
        let path = fixture(temp.path(), Kind::Machine, "u", newer.clone());
        let error = read(Kind::Machine, &path).unwrap_err();
        assert_eq!(veynodevm_api::code_of(&error), ErrorCode::State);
        assert!(error
            .to_string()
            .contains("schema 9, newer than this daemon reads (2)"));
        assert_eq!(json_at(&path), newer);
        for bad in [json!(0), json!("2"), json!(-1)] {
            assert!(upgrade(Kind::Container, json!({"schema":bad})).is_err());
        }
    }

    #[test]
    fn migrate_upgrades_what_it_can_and_names_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let (containers, machines) = (temp.path().join("containers"), temp.path().join("machines"));
        let old = fixture(
            &machines,
            Kind::Machine,
            "old",
            json!({"name":"old","backend":"qemu-hvf","disk_gib":8,"created_at":0}),
        );
        fixture(
            &machines,
            Kind::Machine,
            "new",
            json!({"schema":3,"name":"new"}),
        );
        fixture(&containers, Kind::Container, "c", json!({"id":"c"}));
        fs::create_dir_all(containers.join("empty")).unwrap();
        let problems = migrate(&containers, &machines);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("machines/new/machine.json"),
            "{problems:?}"
        );
        assert_eq!(json_at(&old)["schema"], 2);
        // Missing directories are no records.
        assert!(migrate(&temp.path().join("none"), &temp.path().join("none")).is_empty());
    }

    #[test]
    fn stamped_records_carry_the_current_version() {
        assert_eq!(
            stamped(Kind::Container, json!({"id":"c"})).unwrap(),
            json!({"id":"c","schema":CONTAINER_SCHEMA})
        );
    }
}
