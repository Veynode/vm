//! Machine base disks from upstream cloud images: a qcow2 (v2 or v3, zlib
//! compressed clusters allowed) or raw file becomes a sparse raw disk, once
//! per SHA-256 of the source file, which each machine then APFS-clones.
//!
//! Backing files, encryption, external data files and zstd compression are
//! refused: upstream arm64 cloud images use none of them.
use anyhow::{bail, Context, Result};
use flate2::read::DeflateDecoder;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const QCOW_MAGIC: &[u8; 4] = b"QFI\xfb";
/// Largest virtual disk a source image may declare (the machine disk limit).
const MAX_VIRTUAL: u64 = 4 << 40;

/// `sha256:<hex>` of a file's bytes.
pub fn file_digest(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Format {
    Qcow2,
    Raw,
}

pub fn detect(path: &Path) -> Result<Format> {
    let mut magic = [0u8; 4];
    let mut file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let n = file.read(&mut magic)?;
    Ok(if n == 4 && &magic == QCOW_MAGIC {
        Format::Qcow2
    } else {
        Format::Raw
    })
}

/// The raw base disk for `source` under `cache` (`<cache>/<hex>/disk.raw`,
/// read-only), converting it the first time this digest is seen. Returns the
/// disk and the source digest.
pub fn base_disk(
    cache: &Path,
    source: &Path,
    progress: &mut dyn FnMut(&str),
) -> Result<(PathBuf, String)> {
    progress(&format!("hashing {}", source.display()));
    let digest = file_digest(source)?;
    let dir = cache.join(digest.trim_start_matches("sha256:"));
    let disk = dir.join("disk.raw");
    if disk.is_file() {
        return Ok((disk, digest));
    }
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let partial = dir.join("disk.raw.partial");
    let _ = fs::remove_file(&partial);
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)?;
    let result = match detect(source)? {
        Format::Qcow2 => {
            progress("converting qcow2 to a raw disk");
            qcow2_to_raw(&mut File::open(source)?, &out)
        }
        Format::Raw => {
            progress("copying the raw disk (sparse)");
            sparse_copy(&mut File::open(source)?, &mut out)
        }
    };
    if let Err(error) = result.and_then(|()| Ok(out.sync_all()?)) {
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    fs::set_permissions(&partial, fs::Permissions::from_mode(0o400))?;
    fs::rename(&partial, &disk)?;
    Ok((disk, digest))
}

/// Copies `from` into `to`, leaving runs of zero blocks as holes.
fn sparse_copy(from: &mut File, to: &mut File) -> Result<()> {
    let length = from.metadata()?.len();
    let mut buffer = vec![0u8; 1 << 20];
    let mut at = 0u64;
    loop {
        let n = from.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        for (i, block) in buffer[..n].chunks(4096).enumerate() {
            if block.iter().any(|b| *b != 0) {
                to.write_all_at(block, at + (i * 4096) as u64)?;
            }
        }
        at += n as u64;
    }
    to.set_len(length)?;
    Ok(())
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

/// Writes the guest-visible contents of a qcow2 image into `out` (holes
/// where the image has no data or zero clusters).
pub fn qcow2_to_raw(source: &mut File, out: &File) -> Result<()> {
    let mut header = [0u8; 104];
    source.read_exact(&mut header[..72])?;
    if &header[..4] != QCOW_MAGIC {
        bail!("not a qcow2 image")
    }
    let version = be32(&header, 4);
    if !(2..=3).contains(&version) {
        bail!("unsupported qcow2 version {version}")
    }
    if be64(&header, 8) != 0 {
        bail!("qcow2 images with a backing file are not supported")
    }
    let cluster_bits = be32(&header, 20);
    if !(9..=21).contains(&cluster_bits) {
        bail!("invalid qcow2 cluster size")
    }
    let size = be64(&header, 24);
    if size > MAX_VIRTUAL {
        bail!("qcow2 virtual size {size} exceeds 4 TiB")
    }
    if be32(&header, 32) != 0 {
        bail!("encrypted qcow2 images are not supported")
    }
    let l1_size = be32(&header, 36) as u64;
    let l1_offset = be64(&header, 40);
    if version == 3 {
        source.read_exact(&mut header[72..104])?;
        let incompatible = be64(&header, 72);
        // Bit 0 dirty and bit 1 corrupt are refused too: such an image needs a repair first.
        if incompatible != 0 {
            bail!("qcow2 incompatible features {incompatible:#x} are not supported (external data, zstd, dirty or corrupt)")
        }
    }
    let cluster = 1u64 << cluster_bits;
    let l2_entries = cluster / 8;
    let needed_l1 = size.div_ceil(cluster * l2_entries);
    if l1_size < needed_l1 || l1_size > 1 << 24 {
        bail!("invalid qcow2 L1 table size")
    }
    out.set_len(size)?;
    let mut l1 = vec![0u8; (l1_size * 8) as usize];
    source.read_exact_at(&mut l1, l1_offset)?;
    let mut l2 = vec![0u8; cluster as usize];
    let mut data = vec![0u8; cluster as usize];
    let mut compressed = Vec::new();
    // Compressed descriptors: host offset in the low (62 - (cluster_bits - 8)) bits,
    // then the number of additional 512-byte sectors.
    let offset_bits = 62 - (cluster_bits - 8);
    for (i, entry) in l1.chunks(8).enumerate().take(needed_l1 as usize) {
        let l2_offset = u64::from_be_bytes(entry.try_into().unwrap()) & 0x00ff_ffff_ffff_fe00;
        if l2_offset == 0 {
            continue;
        }
        source.read_exact_at(&mut l2, l2_offset)?;
        for (j, entry) in l2.chunks(8).enumerate() {
            let guest = (i as u64 * l2_entries + j as u64) * cluster;
            if guest >= size {
                break;
            }
            let entry = u64::from_be_bytes(entry.try_into().unwrap());
            let length = cluster.min(size - guest) as usize;
            if entry & (1 << 62) != 0 {
                let descriptor = entry & ((1 << 62) - 1);
                let host = descriptor & ((1 << offset_bits) - 1);
                let sectors = (descriptor >> offset_bits) + 1;
                let span = (sectors * 512 - (host & 511)) as usize;
                compressed.resize(span, 0);
                // The last compressed cluster may end before its sectors do.
                let got = read_at_most(source, &mut compressed, host)?;
                let mut decoder = DeflateDecoder::new(&compressed[..got]);
                decoder.read_exact(&mut data[..length]).with_context(|| {
                    format!("corrupt compressed cluster at guest offset {guest}")
                })?;
            } else {
                let host = entry & 0x00ff_ffff_ffff_fe00;
                // v3 bit 0: reads as zeros. Unallocated: zeros (no backing file).
                if host == 0 || (version == 3 && entry & 1 != 0) {
                    continue;
                }
                source.read_exact_at(&mut data[..length], host)?;
            }
            if data[..length].iter().any(|b| *b != 0) {
                out.write_all_at(&data[..length], guest)?;
            }
        }
    }
    Ok(())
}

fn read_at_most(file: &File, buffer: &mut [u8], offset: u64) -> Result<usize> {
    let mut got = 0;
    while got < buffer.len() {
        let n = file.read_at(&mut buffer[got..], offset + got as u64)?;
        if n == 0 {
            break;
        }
        got += n;
    }
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::DeflateEncoder, Compression};
    use std::io::Write;

    /// A v3 qcow2 with 64 KiB clusters: cluster 0 plain, cluster 1 compressed,
    /// cluster 2 a v3 zero cluster, the rest unallocated.
    fn image(dir: &Path) -> (PathBuf, Vec<u8>) {
        let cluster = 65536u64;
        let size = 5 * cluster + 1000;
        let mut expected = vec![0u8; size as usize];
        let plain: Vec<u8> = (0..cluster).map(|i| (i % 251) as u8 + 1).collect();
        let packed: Vec<u8> = (0..cluster).map(|i| (i / 7 % 13) as u8).collect();
        expected[..cluster as usize].copy_from_slice(&plain);
        expected[cluster as usize..2 * cluster as usize].copy_from_slice(&packed);
        let mut header = vec![0u8; 104];
        header[..4].copy_from_slice(QCOW_MAGIC);
        header[4..8].copy_from_slice(&3u32.to_be_bytes());
        header[20..24].copy_from_slice(&16u32.to_be_bytes());
        header[24..32].copy_from_slice(&size.to_be_bytes());
        header[36..40].copy_from_slice(&1u32.to_be_bytes());
        header[40..48].copy_from_slice(&cluster.to_be_bytes()); // L1 at cluster 1
        header[100..104].copy_from_slice(&104u32.to_be_bytes());
        let l2_at = 2 * cluster;
        let plain_at = 3 * cluster;
        let packed_at = 4 * cluster;
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&packed).unwrap();
        let deflated = encoder.finish().unwrap();
        let path = dir.join("i.qcow2");
        let mut file = File::create(&path).unwrap();
        file.write_all(&header).unwrap();
        file.write_all_at(&(l2_at | 1 << 63).to_be_bytes(), cluster)
            .unwrap();
        let offset_bits = 62 - (16 - 8);
        let sectors = (deflated.len() as u64).div_ceil(512);
        let descriptor = 1u64 << 62 | (sectors - 1) << offset_bits | packed_at;
        for (index, entry) in [(0u64, plain_at | 1 << 63), (1, descriptor), (2, 1)] {
            file.write_all_at(&entry.to_be_bytes(), l2_at + index * 8)
                .unwrap();
        }
        file.write_all_at(&plain, plain_at).unwrap();
        file.write_all_at(&deflated, packed_at).unwrap();
        (path, expected)
    }

    #[test]
    fn qcow2_clusters_become_a_sparse_raw_disk() {
        let dir = tempfile::tempdir().unwrap();
        let (path, expected) = image(dir.path());
        assert_eq!(detect(&path).unwrap(), Format::Qcow2);
        let cache = dir.path().join("cache");
        let mut lines = Vec::new();
        let (disk, digest) = base_disk(&cache, &path, &mut |l| lines.push(l.to_owned())).unwrap();
        assert_eq!(fs::read(&disk).unwrap(), expected);
        assert!(digest.starts_with("sha256:") && disk.starts_with(cache.join(&digest[7..])));
        // Same digest: no second conversion.
        let mut again = Vec::new();
        assert_eq!(
            base_disk(&cache, &path, &mut |l| again.push(l.to_owned()))
                .unwrap()
                .0,
            disk
        );
        assert!(!again.iter().any(|l| l.contains("converting")));
        assert_eq!(
            fs::metadata(&disk).unwrap().permissions().mode() & 0o777,
            0o400
        );
    }

    #[test]
    fn raw_sources_copy_and_unsupported_qcow2_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("r.raw");
        let mut bytes = vec![0u8; 3 * 4096 + 17];
        bytes[5000] = 7;
        fs::write(&raw, &bytes).unwrap();
        let (disk, _) = base_disk(&dir.path().join("c"), &raw, &mut |_| {}).unwrap();
        assert_eq!(fs::read(disk).unwrap(), bytes);

        let (path, _) = image(dir.path());
        let mut header = fs::read(&path).unwrap();
        header[8..16].copy_from_slice(&512u64.to_be_bytes());
        let backed = dir.path().join("b.qcow2");
        fs::write(&backed, &header).unwrap();
        let out = File::create(dir.path().join("o")).unwrap();
        let error = qcow2_to_raw(&mut File::open(&backed).unwrap(), &out).unwrap_err();
        assert!(error.to_string().contains("backing file"));
    }
}
