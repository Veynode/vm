//! The registry (`registry.json`, `REGISTRY_SCHEMA`), VM ids and private state files.
use super::*;

/// The registry format this daemon reads and writes. Version 1 is `registry.json` as a map of VM id to `Config`,
/// with no version inside it: every earlier build parses exactly that, and `schema_version` is itself a valid VM id.
pub const REGISTRY_SCHEMA: u64 = 1;
/// The version lives in a sidecar, `registry.schema` beside `registry.json`: a decimal number, optionally followed by
/// one newline. Absent means version 1, and at version 1 this daemon never writes the file. A higher version, or a
/// file that is not exactly that, refuses the start before the registry is read; nothing is rewritten.
pub(crate) fn check_registry_schema(dir: &Path) -> Result<()> {
    const REFUSED: &str = "daemon not started, nothing rewritten";
    let path = dir.join("registry.schema");
    let malformed = || {
        anyhow::anyhow!("registry.schema is malformed (expected a decimal registry schema version; this daemon supports {REGISTRY_SCHEMA}); {REFUSED}")
    };
    let unreadable = || {
        format!("registry.schema unreadable (this daemon supports registry schema version {REGISTRY_SCHEMA}); {REFUSED}")
    };
    let file = match open_state_file(&path) {
        Ok(None) => return Ok(()),
        Ok(Some(file)) => file,
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Err(malformed()),
        Err(error) => return Err(error).context(unreadable()),
    };
    let bytes = match read_state_file(file, &path, 21) {
        Ok(bytes) => bytes,
        Err(error) if error.is::<StateFileError>() => return Err(malformed()),
        Err(error) => return Err(error).context(unreadable()),
    };
    let digits = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    // Canonical decimal only: no sign, space or leading zero, and versions start at 1.
    let found = std::str::from_utf8(digits)
        .ok()
        .filter(|text| text.bytes().all(|b| b.is_ascii_digit()) && !text.starts_with('0'))
        .and_then(|text| text.parse::<u64>().ok())
        .ok_or_else(malformed)?;
    if found > REGISTRY_SCHEMA {
        bail!("registry schema version {found} is newer than this daemon supports ({REGISTRY_SCHEMA}); {REFUSED}")
    }
    Ok(())
}
#[derive(Debug)]
pub(crate) enum RegistryError {
    ReservedId(String),
    DuplicateKey(String),
    ProtectedConfigPath(PathBuf),
}
impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReservedId(id) => write!(f, "VM id {id:?} is reserved for daemon state"),
            Self::DuplicateKey(key) => {
                write!(f, "duplicate registry key {key:?}; preserving original")
            }
            Self::ProtectedConfigPath(path) => write!(
                f,
                "VM configuration path collides with daemon state: {}",
                path.display()
            ),
        }
    }
}
impl std::error::Error for RegistryError {}

#[derive(Debug)]
pub(crate) enum StateFileError {
    NotRegular(PathBuf),
    TooLarge(PathBuf),
}
impl std::fmt::Display for StateFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRegular(path) => write!(f, "{} is not a regular file", path.display()),
            Self::TooLarge(path) => write!(f, "{} exceeds size limit", path.display()),
        }
    }
}
impl std::error::Error for StateFileError {}

pub(crate) fn open_state_file(path: &Path) -> std::io::Result<Option<File>> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
pub(crate) fn read_state_file(file: File, path: &Path, limit: usize) -> Result<Vec<u8>> {
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(StateFileError::NotRegular(path.into()).into());
    }
    if meta.len() > limit as u64 {
        return Err(StateFileError::TooLarge(path.into()).into());
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    // The opened file can grow after fstat; the descriptor read has the same bound.
    if bytes.len() > limit {
        return Err(StateFileError::TooLarge(path.into()).into());
    }
    Ok(bytes)
}
/// The registry, refused whole when it is not valid (`load_registry` is the daemon's tolerant reading).
#[cfg(test)]
pub(crate) fn read_registry(path: &Path) -> Result<BTreeMap<String, Config>> {
    let Some(file) = open_state_file(path)? else {
        return Ok(BTreeMap::new());
    };
    parse_registry(&read_state_file(file, path, 16 * MAX_MESSAGE)?)
}
fn parse_registry(bytes: &[u8]) -> Result<BTreeMap<String, Config>> {
    struct RegistryVisitor<'a> {
        duplicate: &'a mut Option<String>,
    }
    impl<'de> serde::de::Visitor<'de> for RegistryVisitor<'_> {
        type Value = BTreeMap<String, Config>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a map of VM ids to configurations")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            let mut configs = BTreeMap::new();
            while let Some(key) = map.next_key::<String>()? {
                if configs.contains_key(&key) {
                    *self.duplicate = Some(key);
                    return Err(serde::de::Error::custom("duplicate registry key"));
                }
                configs.insert(key, map.next_value::<Config>()?);
            }
            Ok(configs)
        }
    }
    let mut duplicate = None;
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let result = serde::Deserializer::deserialize_map(
        &mut deserializer,
        RegistryVisitor {
            duplicate: &mut duplicate,
        },
    );
    if let Some(key) = duplicate {
        return Err(RegistryError::DuplicateKey(key).into());
    }
    let configs = result.context("invalid registry; preserving original")?;
    deserializer
        .end()
        .context("invalid registry; preserving original")?;
    for (id, config) in &configs {
        if *id != config.id {
            bail!(
                "registry entry {id:?} names another VM id ({:?})",
                config.id
            )
        }
    }
    Ok(configs)
}

/// The registry as the daemon starts with it. A file that is not a valid registry does not stop the daemon: it is
/// copied aside once per content (`registry.json.<hash>.bad`, beside it), the entries in it that are valid on their own
/// are served, and the note says what was left out. `registry.json` itself is not rewritten here; the next change to
/// the registry writes the served entries. An unreadable file (not a regular file, over the size limit) still refuses
/// the start.
pub(crate) fn load_registry(dir: &Path) -> Result<(BTreeMap<String, Config>, Option<String>)> {
    let path = dir.join("registry.json");
    let Some(file) = open_state_file(&path)? else {
        return Ok((BTreeMap::new(), None));
    };
    let bytes = read_state_file(file, &path, 16 * MAX_MESSAGE)?;
    let error = match parse_registry(&bytes) {
        Ok(configs) => return Ok((configs, None)),
        Err(error) => error,
    };
    let (configs, left_out) = salvage_registry(&bytes);
    let hash = bytes.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    });
    let backup = dir.join(format!("registry.json.{hash:016x}.bad"));
    let saved = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&backup)
    {
        Ok(mut file) => file.write_all(&bytes).and_then(|()| file.sync_all()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    };
    let copy = match saved {
        Ok(()) => format!("a copy of the file is {}", backup.display()),
        Err(e) => format!("copying it to {} failed ({e})", backup.display()),
    };
    let left_out = if left_out.is_empty() {
        "nothing was left out".to_owned()
    } else {
        format!("left out: {}", left_out.join(", "))
    };
    let reason = format!("{error:#}");
    let reason = reason.trim_end_matches("; preserving original");
    let note = format!(
        "the VM registry was damaged ({reason}); {} VMs kept, {left_out}; {copy}",
        configs.len()
    );
    Ok((configs, Some(note)))
}

/// The entries of a damaged registry that are valid on their own: a key given more than once, an entry that is not a
/// VM configuration or names another id is left out (its id in the second list). Nothing, when the file is not a
/// JSON object.
fn salvage_registry(bytes: &[u8]) -> (BTreeMap<String, Config>, Vec<String>) {
    let Ok(entries) = serde_json::from_slice::<Entries>(bytes) else {
        return (BTreeMap::new(), vec!["the whole file".to_owned()]);
    };
    let mut counts = BTreeMap::<&str, usize>::new();
    for (id, _) in &entries.0 {
        *counts.entry(id).or_default() += 1;
    }
    let mut configs = BTreeMap::new();
    let mut left_out = Vec::new();
    for (id, value) in &entries.0 {
        let config = (counts[id.as_str()] == 1)
            .then(|| serde_json::from_value::<Config>(value.clone()).ok())
            .flatten()
            .filter(|config| config.id == *id);
        match config {
            Some(config) => {
                configs.insert(id.clone(), config);
            }
            None if !left_out.contains(&format!("{id:?}")) => left_out.push(format!("{id:?}")),
            None => {}
        }
    }
    (configs, left_out)
}

/// A JSON object's members in order, repeated keys included.
struct Entries(Vec<(String, Value)>);
impl<'de> Deserialize<'de> for Entries {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Members;
        impl<'de> serde::de::Visitor<'de> for Members {
            type Value = Entries;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a JSON object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Entries, M::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry::<String, Value>()? {
                    entries.push(entry);
                }
                Ok(Entries(entries))
            }
        }
        deserializer.deserialize_map(Members)
    }
}

pub use veynodevm_api::validate_id;
pub(crate) const PROTECTED_STATE_FILES: &[&str] = &["registry.json"];
pub(crate) fn validate_new_vm_id(id: &str) -> Result<()> {
    validate_id(id)?;
    // Only direct JSON state files can collide with <id>.json, including case aliases on macOS.
    if PROTECTED_STATE_FILES.iter().any(|name| {
        !name.contains('/')
            && name
                .strip_suffix(".json")
                .is_some_and(|stem| id.eq_ignore_ascii_case(stem))
    }) {
        return Err(RegistryError::ReservedId(id.into()).into());
    }
    Ok(())
}

pub(crate) fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        bail!("state directory must be a real directory")
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let tmp = path.with_extension(format!("{nonce}.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let result = (|| {
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(path.parent().unwrap())?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
pub(crate) fn write_vm_config(dir: &Path, path: &Path, config: &Config) -> Result<()> {
    if PROTECTED_STATE_FILES
        .iter()
        .any(|name| path == dir.join(name))
        || (path.parent() == Some(dir)
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("registry.json")))
    {
        return Err(RegistryError::ProtectedConfigPath(path.into()).into());
    }
    atomic_json(path, config)
}

/// Worker runs whose logs a VM keeps in the state directory: `<id>-<pid>.stderr.log` and `<id>-<pid>.console.log`,
/// one pair per worker start. Older runs' logs go when the VM starts again.
pub(crate) const KEPT_RUN_LOGS: usize = 5;

/// Removes the logs of all but the newest `keep` worker runs of VM `id` (by modification time).
pub(crate) fn prune_run_logs(dir: &Path, id: &str, keep: usize) {
    let mut runs: BTreeMap<String, (std::time::SystemTime, Vec<PathBuf>)> = BTreeMap::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some((pid, log)) = name
            .strip_prefix(id)
            .and_then(|rest| rest.strip_prefix('-'))
            .and_then(|run| run.split_once('.'))
        else {
            continue;
        };
        if pid.is_empty()
            || !pid.bytes().all(|b| b.is_ascii_digit())
            || !matches!(log, "stderr.log" | "console.log")
        {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        let run = runs
            .entry(pid.to_owned())
            .or_insert((std::time::SystemTime::UNIX_EPOCH, Vec::new()));
        run.0 = run.0.max(modified);
        run.1.push(entry.path());
    }
    let mut runs: Vec<_> = runs.into_values().collect();
    runs.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, files) in runs.into_iter().skip(keep) {
        for file in files {
            let _ = fs::remove_file(file);
        }
    }
}
