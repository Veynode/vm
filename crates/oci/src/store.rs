//! Content-addressed local image store.
//!
//! ```text
//! <root>/blobs/sha256/<hex>     manifests, configs and layer blobs (verified)
//! <root>/images.json            image records (reference → manifest/config/layers)
//! <root>/rootfs/<config>.sqfs   read-only flattened root, built once per image
//! <root>/macos/<manifest>/      read-only macOS machine from a Tart image (see `tart`)
//! <root>/store.lock             flock held across mutations
//! ```
use crate::{
    flatten,
    reference::{validate_digest, Reference},
    registry::{Client, LINUX_ARM64},
    tart,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{fs::OpenOptionsExt, io::AsRawFd},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageConfig {
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub working_dir: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub volumes: Vec<String>,
    #[serde(default)]
    pub exposed_ports: Vec<String>,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub architecture: String,
}

impl ImageConfig {
    pub fn from_oci(value: &Value) -> Self {
        let c = &value["config"];
        let strings = |v: &Value| -> Vec<String> {
            v.as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let keys = |v: &Value| -> Vec<String> {
            let mut k: Vec<String> = v
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            k.sort();
            k
        };
        Self {
            env: strings(&c["Env"]),
            entrypoint: strings(&c["Entrypoint"]),
            cmd: strings(&c["Cmd"]),
            working_dir: c["WorkingDir"].as_str().unwrap_or_default().to_owned(),
            user: c["User"].as_str().unwrap_or_default().to_owned(),
            volumes: keys(&c["Volumes"]),
            exposed_ports: keys(&c["ExposedPorts"]),
            os: value["os"].as_str().unwrap_or_default().to_owned(),
            architecture: value["architecture"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImageRecord {
    pub reference: String,
    pub manifest_digest: Option<String>,
    pub config_digest: String,
    pub layers: Vec<String>,
    pub size: u64,
    pub config: ImageConfig,
    pub created_at: u64,
}

impl ImageRecord {
    /// Short image id like Docker: first 12 hex digits of the config digest.
    pub fn id(&self) -> &str {
        &self.config_digest["sha256:".len().."sha256:".len() + 12]
    }
}

pub struct Store {
    root: PathBuf,
}

struct Lock(File);
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe { libc_flock(self.0.as_raw_fd(), 8) };
    }
}
extern "C" {
    #[link_name = "flock"]
    fn libc_flock(fd: i32, operation: i32) -> i32;
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        for dir in [
            root.to_path_buf(),
            root.join("blobs/sha256"),
            root.join("rootfs"),
            root.join("tmp"),
        ] {
            fs::create_dir_all(&dir)?;
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn lock(&self) -> Result<Lock> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(self.root.join("store.lock"))?;
        if unsafe { libc_flock(file.as_raw_fd(), 2) } != 0 {
            bail!("cannot lock image store")
        }
        Ok(Lock(file))
    }

    pub fn blob_path(&self, digest: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join("blobs/sha256")
            .join(validate_digest(digest)?))
    }

    fn temp(&self) -> Result<(PathBuf, File)> {
        for attempt in 0..100u32 {
            let path =
                self.root
                    .join("tmp")
                    .join(format!("{}-{}-{attempt}", std::process::id(), now()));
            match OpenOptions::new()
                .write(true)
                .read(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => return Ok((path, file)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        bail!("cannot create temporary file")
    }

    /// Write bytes as a verified blob and return its digest.
    fn put_bytes(&self, bytes: &[u8]) -> Result<String> {
        let digest = format!("sha256:{:x}", Sha256::digest(bytes));
        let path = self.blob_path(&digest)?;
        if !path.exists() {
            let (tmp, mut file) = self.temp()?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(tmp, &path)?;
        }
        Ok(digest)
    }

    /// Copy a reader into the store, computing its digest.
    fn put_reader(&self, reader: &mut dyn Read) -> Result<(String, u64)> {
        let (tmp, mut file) = self.temp()?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 1 << 20];
        let mut total = 0;
        loop {
            let n = reader.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            file.write_all(&buffer[..n])?;
            total += n as u64;
        }
        file.sync_all()?;
        let digest = format!("sha256:{:x}", hasher.finalize());
        let path = self.blob_path(&digest)?;
        if path.exists() {
            fs::remove_file(tmp)?;
        } else {
            fs::rename(tmp, path)?;
        }
        Ok((digest, total))
    }

    pub fn list(&self) -> Result<Vec<ImageRecord>> {
        let path = self.root.join("images.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        serde_json::from_slice(&fs::read(path)?).context("invalid images.json")
    }

    fn save(&self, images: &[ImageRecord]) -> Result<()> {
        let (tmp, mut file) = self.temp()?;
        file.write_all(&serde_json::to_vec_pretty(images)?)?;
        file.sync_all()?;
        fs::rename(tmp, self.root.join("images.json"))?;
        Ok(())
    }

    fn upsert(&self, record: ImageRecord) -> Result<ImageRecord> {
        let _lock = self.lock()?;
        let mut images = self.list()?;
        images.retain(|i| i.reference != record.reference);
        images.push(record.clone());
        images.sort_by(|a, b| a.reference.cmp(&b.reference));
        self.save(&images)?;
        Ok(record)
    }

    /// Find by reference (`alpine`, `alpine:3.20`), full or short image id.
    pub fn find(&self, name: &str) -> Result<Option<ImageRecord>> {
        let images = self.list()?;
        let familiar = Reference::parse(name).ok().map(|r| r.familiar());
        Ok(images.into_iter().find(|i| {
            Some(&i.reference) == familiar.as_ref()
                || i.reference == name
                || i.config_digest == name
                || (name.len() >= 12
                    && name.bytes().all(|b| b.is_ascii_hexdigit())
                    && i.config_digest[7..].starts_with(name))
        }))
    }

    pub fn pull(&self, name: &str, progress: &mut dyn FnMut(&str)) -> Result<ImageRecord> {
        let reference = Reference::parse(name)?;
        let mut client = Client::new();
        progress(&format!("resolving {reference}"));
        let (manifest_digest, manifest) = client.resolve(&reference, &LINUX_ARM64)?;
        let value: Value = serde_json::from_slice(&manifest)?;
        self.put_bytes(&manifest)?;
        if tart::is_tart(&value) {
            return self.pull_macos(&reference, manifest_digest, &value, progress);
        }
        let config = &value["config"];
        let config_digest = config["digest"]
            .as_str()
            .context("manifest without config")?
            .to_owned();
        let mut wanted = vec![(config_digest.clone(), config["size"].as_u64())];
        let mut layers = Vec::new();
        for layer in value["layers"]
            .as_array()
            .context("manifest without layers")?
        {
            let media = layer["mediaType"].as_str().unwrap_or_default();
            if media.contains("foreign") || media.contains("nondistributable") {
                bail!("foreign or non-distributable layers are unsupported")
            }
            let digest = layer["digest"]
                .as_str()
                .context("layer without digest")?
                .to_owned();
            wanted.push((digest.clone(), layer["size"].as_u64()));
            layers.push(digest);
        }
        let mut total = 0;
        for (index, (digest, size)) in wanted.iter().enumerate() {
            let path = self.blob_path(digest)?;
            if !path.exists() {
                progress(&format!(
                    "downloading {} ({}/{}, {} MiB)",
                    &digest[7..19],
                    index + 1,
                    wanted.len(),
                    size.unwrap_or(0) / (1 << 20)
                ));
                let (tmp, mut file) = self.temp()?;
                let result = client.blob(&reference, digest, *size, &mut file);
                if let Err(e) = result.and_then(|_| Ok(file.sync_all()?)) {
                    let _ = fs::remove_file(&tmp);
                    return Err(e);
                }
                fs::rename(tmp, &path)?;
            }
            total += fs::metadata(&path)?.len();
        }
        let config_value: Value =
            serde_json::from_slice(&fs::read(self.blob_path(&config_digest)?)?)?;
        let image_config = ImageConfig::from_oci(&config_value);
        if image_config.architecture != "arm64" || image_config.os != "linux" {
            bail!(
                "image is {}/{}, not linux/arm64",
                image_config.os,
                image_config.architecture
            )
        }
        progress(&format!("pulled {reference} ({})", &config_digest[7..19]));
        self.upsert(ImageRecord {
            reference: reference.familiar(),
            manifest_digest: Some(manifest_digest),
            config_digest,
            layers,
            size: total,
            config: image_config,
            created_at: now(),
        })
    }

    /// Tart macOS image. Tart configs are not unique per image, so the manifest
    /// digest identifies it (as `config_digest`) and names its machine directory.
    fn pull_macos(
        &self,
        reference: &Reference,
        manifest_digest: String,
        manifest: &Value,
        progress: &mut dyn FnMut(&str),
    ) -> Result<ImageRecord> {
        let pulled = tart::pull(&self.root, reference, &manifest_digest, manifest, progress)?;
        progress(&format!("pulled {reference} ({})", &manifest_digest[7..19]));
        self.upsert(ImageRecord {
            reference: reference.familiar(),
            manifest_digest: Some(manifest_digest.clone()),
            config_digest: manifest_digest,
            layers: Vec::new(),
            size: tart::allocated(&pulled.dir),
            config: ImageConfig {
                os: "darwin".into(),
                architecture: "arm64".into(),
                ..Default::default()
            },
            created_at: now(),
        })
    }

    /// Machine directory of a macOS image (see `tart`).
    pub fn macos_dir(&self, image: &ImageRecord) -> Result<PathBuf> {
        if image.config.os != "darwin" {
            bail!("{} is not a macOS image", image.reference)
        }
        Ok(self
            .root
            .join("macos")
            .join(validate_digest(&image.config_digest)?))
    }

    /// Import a `docker save` archive (manifest.json + config + layers).
    pub fn load(&self, archive: &Path) -> Result<Vec<ImageRecord>> {
        // Two passes over the archive keep memory bounded to manifest + config.
        let mut members: BTreeMap<String, (String, u64)> = BTreeMap::new();
        let mut manifest: Option<Value> = None;
        let mut small: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut tar = tar::Archive::new(File::open(archive)?);
        for entry in tar.entries()? {
            let mut entry = entry?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let name = flatten::normalize(entry.path()?.to_str().context("non-UTF-8 member")?)?;
            if name == "manifest.json" {
                let mut bytes = Vec::new();
                entry.by_ref().take(4 << 20).read_to_end(&mut bytes)?;
                manifest = Some(serde_json::from_slice(&bytes)?);
                continue;
            }
            if entry.size() <= 4 << 20 {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                let digest = self.put_bytes(&bytes)?;
                members.insert(name.clone(), (digest, bytes.len() as u64));
                small.insert(name, bytes);
            } else {
                let (digest, size) = self.put_reader(&mut entry)?;
                members.insert(name, (digest, size));
            }
        }
        let manifest = manifest.context("not a docker save archive: missing manifest.json")?;
        let mut records = Vec::new();
        for image in manifest
            .as_array()
            .context("manifest.json must be an array")?
        {
            let config_name = image["Config"].as_str().context("image without Config")?;
            let config_bytes = small.get(config_name).context("missing config member")?;
            let config_digest = members[config_name].0.clone();
            let image_config = ImageConfig::from_oci(&serde_json::from_slice(config_bytes)?);
            if image_config.architecture != "arm64" {
                bail!("archive image is {}, not arm64", image_config.architecture)
            }
            let mut layers = Vec::new();
            let mut size = 0;
            for layer in image["Layers"].as_array().context("image without Layers")? {
                let (digest, layer_size) = members
                    .get(layer.as_str().context("invalid layer name")?)
                    .context("missing layer member")?
                    .clone();
                size += layer_size;
                layers.push(digest);
            }
            let tags: Vec<String> = image["RepoTags"]
                .as_array()
                .map(|t| {
                    t.iter()
                        .filter_map(|s| s.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let names = if tags.is_empty() {
                vec![config_digest.clone()]
            } else {
                tags
            };
            for name in names {
                let reference = Reference::parse(&name)
                    .map(|r| r.familiar())
                    .unwrap_or(name);
                records.push(self.upsert(ImageRecord {
                    reference,
                    manifest_digest: None,
                    config_digest: config_digest.clone(),
                    layers: layers.clone(),
                    size,
                    config: image_config.clone(),
                    created_at: now(),
                })?);
            }
        }
        Ok(records)
    }

    pub fn rootfs_path(&self, image: &ImageRecord) -> Result<PathBuf> {
        Ok(self
            .root
            .join("rootfs")
            .join(format!("{}.sqfs", validate_digest(&image.config_digest)?)))
    }

    /// Build (once) the read-only squashfs root for an image with `sqfstar`.
    pub fn rootfs(&self, image: &ImageRecord, progress: &mut dyn FnMut(&str)) -> Result<PathBuf> {
        if image.config.os == "darwin" {
            bail!(
                "{} is a macOS image; it runs as a macOS VM, not on a Linux root",
                image.reference
            )
        }
        let target = self.rootfs_path(image)?;
        if target.exists() {
            return Ok(target);
        }
        let _lock = self.lock()?;
        if target.exists() {
            return Ok(target);
        }
        progress(&format!(
            "converting {} layers to a squashfs root",
            image.layers.len()
        ));
        let mut decompressed = Vec::new();
        let mut temporaries = Vec::new();
        for digest in &image.layers {
            let (path, mut file) = self.temp()?;
            flatten::decompress(&self.blob_path(digest)?, &mut file)?;
            temporaries.push(path);
            decompressed.push(file);
        }
        let output = self
            .root
            .join("tmp")
            .join(format!("{}.sqfs", &image.config_digest[7..]));
        let _ = fs::remove_file(&output);
        let sqfstar = which("sqfstar").context("sqfstar not found (brew install squashfs)")?;
        let mut child = Command::new(sqfstar)
            .args(["-quiet", "-no-progress", "-no-xattrs", "-root-mode", "0755"])
            .arg(&output)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().unwrap();
        let flattened = flatten::flatten(&decompressed, std::io::BufWriter::new(stdin));
        let result = child.wait_with_output()?;
        for path in temporaries {
            let _ = fs::remove_file(path);
        }
        let stats = flattened?;
        if !result.status.success() {
            let _ = fs::remove_file(&output);
            bail!(
                "sqfstar failed: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            )
        }
        if stats.skipped_hardlinks > 0 {
            progress(&format!(
                "skipped {} hard links to hidden targets",
                stats.skipped_hardlinks
            ));
        }
        fs::set_permissions(&output, std::os::unix::fs::PermissionsExt::from_mode(0o444))?;
        fs::rename(&output, &target)?;
        Ok(target)
    }

    /// Remove an image record; blobs and roots are reclaimed by `gc`.
    pub fn remove(&self, name: &str) -> Result<ImageRecord> {
        let _lock = self.lock()?;
        let found = self
            .find(name)?
            .with_context(|| format!("image not found: {name}"))?;
        let mut images = self.list()?;
        images.retain(|i| i.reference != found.reference);
        self.save(&images)?;
        Ok(found)
    }

    /// Delete blobs and roots not referenced by any image record or by `in_use`
    /// (config digests of images that existing containers still need).
    pub fn gc(&self, in_use: &HashSet<String>) -> Result<u64> {
        let _lock = self.lock()?;
        let images = self.list()?;
        let mut keep: HashSet<String> = HashSet::new();
        let mut keep_roots: HashSet<String> = in_use
            .iter()
            .filter_map(|d| d.get(7..).map(str::to_owned))
            .collect();
        for image in &images {
            keep.insert(image.config_digest[7..].to_owned());
            keep_roots.insert(image.config_digest[7..].to_owned());
            if let Some(m) = &image.manifest_digest {
                keep.insert(m[7..].to_owned());
            }
            keep.extend(image.layers.iter().map(|l| l[7..].to_owned()));
        }
        let mut freed = 0;
        for entry in fs::read_dir(self.root.join("blobs/sha256"))? {
            let entry = entry?;
            if !keep.contains(entry.file_name().to_str().unwrap_or_default()) {
                freed += entry.metadata()?.len();
                fs::remove_file(entry.path())?;
            }
        }
        for entry in fs::read_dir(self.root.join("rootfs"))? {
            let entry = entry?;
            let name = entry
                .file_name()
                .to_string_lossy()
                .trim_end_matches(".sqfs")
                .to_owned();
            if !keep_roots.contains(&name) {
                freed += entry.metadata()?.len();
                fs::remove_file(entry.path())?;
            }
        }
        let machines = self.root.join("macos");
        for entry in fs::read_dir(&machines).into_iter().flatten() {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if keep_roots.contains(&name) {
                continue;
            }
            // An interrupted pull resumes from its partial directory until gc
            // drops it; one that a pull still holds locked stays.
            let _pulling = if name.starts_with(".partial-") {
                match tart::try_lock_partial(&entry.path())? {
                    Some(lock) => Some(lock),
                    None => continue,
                }
            } else {
                None
            };
            freed += tart::allocated(&entry.path());
            for file in fs::read_dir(entry.path())? {
                let file = file?.path();
                fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
                fs::remove_file(file)?;
            }
            fs::remove_dir(entry.path())?;
        }
        Ok(freed)
    }
}

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ])
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oci_config_mapping() {
        let c = ImageConfig::from_oci(&serde_json::json!({
            "architecture":"arm64","os":"linux",
            "config":{"Env":["PATH=/bin"],"Entrypoint":["/e"],"Cmd":["a","b"],"WorkingDir":"/w","User":"1000",
                      "Volumes":{"/var/lib/docker":{}},"ExposedPorts":{"80/tcp":{}}}
        }));
        assert_eq!(c.entrypoint, ["/e"]);
        assert_eq!(c.cmd, ["a", "b"]);
        assert_eq!(c.volumes, ["/var/lib/docker"]);
        assert_eq!(c.exposed_ports, ["80/tcp"]);
        assert_eq!((c.os.as_str(), c.architecture.as_str()), ("linux", "arm64"));
    }
    #[test]
    fn load_docker_save_archive_and_gc() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("images")).unwrap();
        let mut layer = Vec::new();
        {
            let mut b = tar::Builder::new(&mut layer);
            let mut h = tar::Header::new_gnu();
            h.set_size(2);
            h.set_mode(0o644);
            b.append_data(&mut h, "hello", &b"hi"[..]).unwrap();
            b.finish().unwrap();
        }
        let config = serde_json::to_vec(
            &serde_json::json!({"architecture":"arm64","os":"linux","config":{"Cmd":["/hello"]}}),
        )
        .unwrap();
        let config_hex = format!("{:x}", Sha256::digest(&config));
        let mut archive = Vec::new();
        {
            let mut b = tar::Builder::new(&mut archive);
            let mut add = |name: &str, data: &[u8]| {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                b.append_data(&mut h, name, data).unwrap();
            };
            add(&format!("blobs/sha256/{config_hex}"), &config);
            add("layer.tar", &layer);
            add("manifest.json", serde_json::to_string(&serde_json::json!([{"Config":format!("blobs/sha256/{config_hex}"),"RepoTags":["example/app:v1"],"Layers":["layer.tar"]}])).unwrap().as_bytes());
            b.finish().unwrap();
        }
        let path = dir.path().join("save.tar");
        fs::write(&path, archive).unwrap();
        let records = store.load(&path).unwrap();
        assert_eq!(records[0].reference, "example/app:v1");
        assert_eq!(records[0].config.cmd, ["/hello"]);
        assert_eq!(records[0].config_digest, format!("sha256:{config_hex}"));
        assert!(store.find("example/app:v1").unwrap().is_some());
        assert!(store.find(&config_hex[..12]).unwrap().is_some());
        store.remove("example/app:v1").unwrap();
        assert!(store.gc(&HashSet::new()).unwrap() > 0);
        assert_eq!(
            fs::read_dir(dir.path().join("images/blobs/sha256"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn gc_drops_unused_and_abandoned_macos_machines() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("images")).unwrap();
        let machines = dir.path().join("images/macos");
        let hex = |c: char| c.to_string().repeat(64);
        let machine = |name: &str| {
            let path = machines.join(name);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("disk.img"), vec![1u8; 8192]).unwrap();
            fs::set_permissions(path.join("disk.img"), fs::Permissions::from_mode(0o444)).unwrap();
            path
        };
        let record = |c: char| ImageRecord {
            reference: format!("example/mac-{c}:latest"),
            manifest_digest: Some(format!("sha256:{}", hex(c))),
            config_digest: format!("sha256:{}", hex(c)),
            layers: Vec::new(),
            size: 0,
            config: ImageConfig {
                os: "darwin".into(),
                ..Default::default()
            },
            created_at: 0,
        };
        machine(&hex('a'));
        machine(&hex('b'));
        machine(&hex('c'));
        store.upsert(record('a')).unwrap();
        store.upsert(record('b')).unwrap();
        // b's image is removed but a container still uses it; c has no record.
        store.remove("example/mac-b:latest").unwrap();
        let abandoned = machine(&format!(".partial-{}", hex('d')));
        fs::write(abandoned.join("lock"), b"").unwrap();
        let pulling = machine(&format!(".partial-{}", hex('e')));
        fs::write(pulling.join("lock"), b"").unwrap();
        let held = tart::try_lock_partial(&pulling).unwrap();
        assert!(held.is_some());
        let in_use = HashSet::from([format!("sha256:{}", hex('b'))]);
        assert!(store.gc(&in_use).unwrap() > 0);
        let mut left: Vec<String> = fs::read_dir(&machines)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, [format!(".partial-{}", hex('e')), hex('a'), hex('b')]);
    }
}
