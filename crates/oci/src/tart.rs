//! macOS VM images in Tart's OCI layout (ghcr.io/cirruslabs/macos-*).
//!
//! A Tart manifest carries one `tart.config.v1` layer (VM JSON with the
//! platform hardware model and ECID), the disk as ordered `tart.disk.v2`
//! layers (each an Apple-LZ4 chunk of a raw disk, annotated with its
//! uncompressed size and digest) and one `tart.nvram.v1` layer (the
//! `VZMacAuxiliaryStorage`). Pulling materializes a read-only machine:
//!
//! ```text
//! <root>/macos/<manifest>/disk.img            sparse raw disk, 0444
//! <root>/macos/<manifest>/nvram.bin           auxiliary storage, 0444
//! <root>/macos/<manifest>/hardware-model.bin  VZMacHardwareModel data
//! <root>/macos/<manifest>/machine-id.bin      VZMacMachineIdentifier data
//! <root>/macos/<manifest>/vm.json             Tart VM config
//! ```
//!
//! Containers clone `disk.img` and `nvram.bin` (APFS clones). Disk chunks are
//! decompressed straight into the sparse disk, so no compressed copy is kept;
//! an interrupted pull resumes from `.partial-<manifest>/done.json`.
use crate::{reference::Reference, registry::Client};
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    os::unix::{
        fs::{FileExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
};

pub const CONFIG: &str = "application/vnd.cirruslabs.tart.config.v1";
pub const DISK_V1: &str = "application/vnd.cirruslabs.tart.disk.v1";
pub const DISK_V2: &str = "application/vnd.cirruslabs.tart.disk.v2";
pub const NVRAM: &str = "application/vnd.cirruslabs.tart.nvram.v1";
const UNCOMPRESSED_SIZE: &str = "org.cirruslabs.tart.uncompressed-size";
const UNCOMPRESSED_DIGEST: &str = "org.cirruslabs.tart.uncompressed-content-digest";
const DISK_SIZE: &str = "org.cirruslabs.tart.uncompressed-disk-size";
/// Larger than any chunk Tart produces (512 MiB); bounds memory per worker.
const MAX_CHUNK: u64 = 1 << 30;
const WORKERS: usize = 4;
/// Zero blocks of this size are skipped, keeping the disk sparse.
const SPARSE_BLOCK: usize = 1 << 20;
/// Free space kept for the rest of the system while writing the disk.
const RESERVE: u64 = 8 << 30;

pub fn is_tart(manifest: &Value) -> bool {
    manifest["layers"].as_array().is_some_and(|layers| {
        layers
            .iter()
            .any(|l| l["mediaType"].as_str() == Some(CONFIG))
    })
}

struct Chunk {
    digest: String,
    size: u64,
    uncompressed: u64,
    uncompressed_digest: String,
    offset: u64,
}

struct Plan {
    config: (String, u64),
    nvram: (String, u64),
    chunks: Vec<Chunk>,
    disk_size: u64,
    compressed: u64,
}

fn plan(manifest: &Value) -> Result<Plan> {
    let mut config = None;
    let mut nvram = None;
    let mut chunks = Vec::new();
    let mut offset = 0u64;
    let mut compressed = 0u64;
    for layer in manifest["layers"]
        .as_array()
        .context("manifest without layers")?
    {
        let digest = layer["digest"].as_str().context("layer without digest")?;
        crate::reference::validate_digest(digest)?;
        let size = layer["size"].as_u64().context("layer without size")?;
        match layer["mediaType"].as_str().unwrap_or_default() {
            CONFIG => config = Some((digest.to_owned(), size)),
            NVRAM => nvram = Some((digest.to_owned(), size)),
            DISK_V2 => {
                let note = |key: &str| layer["annotations"][key].as_str();
                let uncompressed: u64 = note(UNCOMPRESSED_SIZE)
                    .context("disk layer without uncompressed size")?
                    .parse()?;
                let uncompressed_digest =
                    note(UNCOMPRESSED_DIGEST).context("disk layer without uncompressed digest")?;
                crate::reference::validate_digest(uncompressed_digest)?;
                if uncompressed > MAX_CHUNK || size > MAX_CHUNK {
                    bail!("disk chunk larger than 1 GiB")
                }
                chunks.push(Chunk {
                    digest: digest.to_owned(),
                    size,
                    uncompressed,
                    uncompressed_digest: uncompressed_digest.to_owned(),
                    offset,
                });
                offset += uncompressed;
                compressed += size;
            }
            DISK_V1 => bail!("Tart disk format v1 is unsupported; pull a current (disk v2) image"),
            other if other.contains("cirruslabs.tart") => {
                bail!("unsupported Tart layer type {other}")
            }
            _ => {}
        }
    }
    let disk_size = manifest["annotations"][DISK_SIZE]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(offset)
        .max(offset);
    Ok(Plan {
        config: config.context("Tart manifest without VM config")?,
        nvram: nvram.context("Tart manifest without NVRAM")?,
        chunks,
        disk_size,
        compressed,
    })
}

#[cfg(target_os = "macos")]
#[link(name = "compression")]
extern "C" {
    fn compression_decode_buffer(
        dst: *mut u8,
        dst_size: usize,
        src: *const u8,
        src_size: usize,
        scratch: *mut std::ffi::c_void,
        algorithm: i32,
    ) -> usize;
}
#[cfg(target_os = "macos")]
const COMPRESSION_LZ4: i32 = 0x100;

/// Decode one Apple-LZ4 chunk of exactly `size` bytes.
#[cfg(target_os = "macos")]
pub fn decode_lz4(src: &[u8], size: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; size];
    let n = unsafe {
        compression_decode_buffer(
            out.as_mut_ptr(),
            size,
            src.as_ptr(),
            src.len(),
            std::ptr::null_mut(),
            COMPRESSION_LZ4,
        )
    };
    if n != size {
        bail!("LZ4 chunk decoded to {n} bytes, expected {size}")
    }
    Ok(out)
}
#[cfg(not(target_os = "macos"))]
pub fn decode_lz4(_: &[u8], _: usize) -> Result<Vec<u8>> {
    bail!("Tart disk chunks need Apple's Compression library")
}

/// Write `data` at `offset`, skipping all-zero blocks of a freshly sized
/// sparse file so they stay unallocated.
pub fn write_sparse(file: &File, offset: u64, data: &[u8]) -> Result<u64> {
    let mut written = 0;
    for (i, block) in data.chunks(SPARSE_BLOCK).enumerate() {
        if block.iter().all(|&b| b == 0) {
            continue;
        }
        file.write_all_at(block, offset + (i * SPARSE_BLOCK) as u64)?;
        written += block.len() as u64;
    }
    Ok(written)
}

fn fetch(client: &mut Client, image: &Reference, digest: &str, size: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(size as usize);
    client.blob(image, digest, Some(size), &mut bytes)?;
    Ok(bytes)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    std::io::Write::write_all(&mut file, bytes)?;
    file.sync_all()?;
    Ok(())
}

fn available_bytes(path: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0)
        .then(|| s.f_bavail as u64 * s.f_frsize as u64)
}

/// Allocated (not apparent) size of a machine directory.
pub fn allocated(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok()?.metadata().ok())
                .map(|m| m.blocks() * 512)
                .sum()
        })
        .unwrap_or(0)
}

pub struct Pulled {
    pub dir: PathBuf,
    pub vm: Value,
}

/// Materialize the machine for `manifest` under `root/macos/<hex>`.
pub fn pull(
    root: &Path,
    image: &Reference,
    manifest_digest: &str,
    manifest: &Value,
    progress: &mut dyn FnMut(&str),
) -> Result<Pulled> {
    let hex = crate::reference::validate_digest(manifest_digest)?;
    let machines = root.join("macos");
    fs::create_dir_all(&machines)?;
    let dir = machines.join(hex);
    if !dir.join("vm.json").exists() {
        let plan = plan(manifest)?;
        let partial = machines.join(format!(".partial-{hex}"));
        fs::create_dir_all(&partial)?;
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o700))?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(partial.join("lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("another pull of {image} is in progress")
        }
        materialize(image, &plan, &partial, progress)?;
        drop(lock);
        let _ = fs::remove_file(partial.join("lock"));
        let _ = fs::remove_file(partial.join("done.json"));
        fs::rename(&partial, &dir)?;
    }
    let vm = serde_json::from_slice(&fs::read(dir.join("vm.json"))?)?;
    Ok(Pulled { dir, vm })
}

/// Lock of a partial pull directory, or `None` while a pull holds it. A
/// directory without a lock file was never claimed and is free to remove.
pub fn try_lock_partial(partial: &Path) -> Result<Option<File>> {
    let lock = match OpenOptions::new().write(true).open(partial.join("lock")) {
        Ok(lock) => lock,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(File::open(partial)?));
        }
        Err(e) => return Err(e.into()),
    };
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(None);
    }
    Ok(Some(lock))
}

fn materialize(
    image: &Reference,
    plan: &Plan,
    partial: &Path,
    progress: &mut dyn FnMut(&str),
) -> Result<()> {
    let mut client = Client::new();
    let vm: Value =
        serde_json::from_slice(&fetch(&mut client, image, &plan.config.0, plan.config.1)?)?;
    if vm["os"] != "darwin" || vm["arch"] != "arm64" {
        bail!(
            "Tart image is {}/{}, not darwin/arm64",
            vm["os"],
            vm["arch"]
        )
    }
    if !matches!(vm["diskFormat"].as_str(), None | Some("raw")) {
        bail!(
            "Tart disk format {} is unsupported (raw only)",
            vm["diskFormat"]
        )
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let field = |key: &str| -> Result<Vec<u8>> {
        b64.decode(
            vm[key]
                .as_str()
                .with_context(|| format!("Tart config without {key}"))?,
        )
        .with_context(|| format!("invalid {key}"))
    };
    write_private(
        &partial.join("hardware-model.bin"),
        &field("hardwareModel")?,
    )?;
    write_private(&partial.join("machine-id.bin"), &field("ecid")?)?;
    let nvram = partial.join("nvram.bin");
    if !nvram.exists() {
        let bytes = fetch(&mut client, image, &plan.nvram.0, plan.nvram.1)?;
        write_private(&partial.join("nvram.tmp"), &bytes)?;
        fs::rename(partial.join("nvram.tmp"), &nvram)?;
    }
    let done_path = partial.join("done.json");
    let done: BTreeSet<String> = fs::read(&done_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let remaining: Vec<&Chunk> = plan
        .chunks
        .iter()
        .filter(|c| !done.contains(&c.digest))
        .collect();
    let left: u64 = remaining.iter().map(|c| c.size).sum();
    if let Some(free) = available_bytes(partial) {
        // Raw chunks are larger than their compressed form; this is a floor
        // (macOS disks are mostly dense: expect ~1.8x the compressed size).
        if free < left + RESERVE {
            bail!(
                "not enough free disk space: {} GiB left to download, {} GiB free",
                left >> 30,
                free >> 30
            )
        }
    }
    progress(&format!(
        "macOS image: {} disk chunks ({} GiB compressed, {} GiB disk), {} already present",
        plan.chunks.len(),
        plan.compressed >> 30,
        plan.disk_size >> 30,
        plan.chunks.len() - remaining.len()
    ));
    let disk = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(partial.join("disk.img"))?;
    if disk.metadata()?.len() != plan.disk_size {
        disk.set_len(plan.disk_size)?;
    }
    let disk = Arc::new(disk);
    let done = Arc::new(Mutex::new(done));
    let next = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicBool::new(false));
    let total = remaining.len();
    let (tx, rx) = mpsc::channel::<Result<String>>();
    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..WORKERS.min(total) {
            let (disk, done, next, failed, tx, remaining) = (
                disk.clone(),
                done.clone(),
                next.clone(),
                failed.clone(),
                tx.clone(),
                &remaining,
            );
            let done_path = &done_path;
            scope.spawn(move || {
                let mut client = Client::new();
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    if index >= remaining.len() || failed.load(Ordering::SeqCst) {
                        return;
                    }
                    let chunk = remaining[index];
                    let result = (|| -> Result<String> {
                        let compressed = fetch(&mut client, image, &chunk.digest, chunk.size)?;
                        let raw = decode_lz4(&compressed, chunk.uncompressed as usize)?;
                        drop(compressed);
                        // Stop before the disk fills; finished chunks resume.
                        if available_bytes(&done_path.with_file_name("disk.img"))
                            .is_some_and(|free| free < chunk.uncompressed + RESERVE)
                        {
                            bail!("free disk space fell below {} GiB", RESERVE >> 30)
                        }
                        if format!("sha256:{:x}", Sha256::digest(&raw)) != chunk.uncompressed_digest
                        {
                            bail!(
                                "disk chunk {} uncompressed digest mismatch",
                                &chunk.digest[7..19]
                            )
                        }
                        write_sparse(&disk, chunk.offset, &raw)?;
                        disk.sync_data()?;
                        let mut done = done.lock().unwrap();
                        done.insert(chunk.digest.clone());
                        fs::write(done_path, serde_json::to_vec(&*done)?)?;
                        Ok(chunk.digest.clone())
                    })();
                    if result.is_err() {
                        failed.store(true, Ordering::SeqCst);
                    }
                    let _ = tx.send(result);
                }
            });
        }
        drop(tx);
        let mut finished = 0;
        let mut first_error = None;
        for result in rx {
            match result {
                Ok(digest) => {
                    finished += 1;
                    progress(&format!(
                        "disk chunk {} done ({finished}/{total})",
                        &digest[7..19]
                    ));
                }
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        match first_error {
            Some(e) => Err(e.context("pull interrupted; run it again to resume")),
            None => Ok(()),
        }
    })?;
    for name in [
        "disk.img",
        "nvram.bin",
        "hardware-model.bin",
        "machine-id.bin",
    ] {
        fs::set_permissions(partial.join(name), fs::Permissions::from_mode(0o444))?;
    }
    write_private(&partial.join("vm.json"), &serde_json::to_vec_pretty(&vm)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plan_orders_chunks_and_requires_config_and_nvram() {
        let d = |c: char| format!("sha256:{}", c.to_string().repeat(64));
        let manifest = json!({
            "annotations": {DISK_SIZE: "3000"},
            "layers": [
                {"mediaType": CONFIG, "digest": d('a'), "size": 10},
                {"mediaType": DISK_V2, "digest": d('b'), "size": 5, "annotations": {UNCOMPRESSED_SIZE: "1024", UNCOMPRESSED_DIGEST: d('c')}},
                {"mediaType": DISK_V2, "digest": d('d'), "size": 6, "annotations": {UNCOMPRESSED_SIZE: "512", UNCOMPRESSED_DIGEST: d('e')}},
                {"mediaType": NVRAM, "digest": d('f'), "size": 7},
            ]
        });
        assert!(is_tart(&manifest));
        let p = plan(&manifest).unwrap();
        assert_eq!(p.chunks[1].offset, 1024);
        assert_eq!(p.disk_size, 3000);
        assert_eq!(p.compressed, 11);
        let mut missing = manifest.clone();
        missing["layers"].as_array_mut().unwrap().pop();
        assert!(plan(&missing).is_err());
        assert!(!is_tart(
            &json!({"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip"}]})
        ));
    }

    #[test]
    fn sparse_write_skips_zero_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.path().join("d"))
            .unwrap();
        file.set_len(4 * SPARSE_BLOCK as u64).unwrap();
        let mut data = vec![0u8; 3 * SPARSE_BLOCK];
        data[SPARSE_BLOCK + 5] = 7;
        assert_eq!(
            write_sparse(&file, SPARSE_BLOCK as u64, &data).unwrap(),
            SPARSE_BLOCK as u64
        );
        let mut byte = [0u8];
        file.read_exact_at(&mut byte, 2 * SPARSE_BLOCK as u64 + 5)
            .unwrap();
        assert_eq!(byte[0], 7);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn decodes_apple_lz4() {
        #[link(name = "compression")]
        extern "C" {
            fn compression_encode_buffer(
                dst: *mut u8,
                dst_size: usize,
                src: *const u8,
                src_size: usize,
                scratch: *mut std::ffi::c_void,
                algorithm: i32,
            ) -> usize;
        }
        let raw: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut out = vec![0u8; raw.len() + 4096];
        let n = unsafe {
            compression_encode_buffer(
                out.as_mut_ptr(),
                out.len(),
                raw.as_ptr(),
                raw.len(),
                std::ptr::null_mut(),
                COMPRESSION_LZ4,
            )
        };
        assert_eq!(&out[..4], b"bv41");
        assert_eq!(decode_lz4(&out[..n], raw.len()).unwrap(), raw);
        assert!(decode_lz4(&out[..n], raw.len() + 1).is_err());
    }
}
