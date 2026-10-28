//! Grow a raw GPT disk image so the macOS system container can be resized.
//!
//! macOS VM disks end with the APFS Recovery container, directly after the
//! system container, and SIP forbids deleting it inside the Guest. With the VM
//! stopped, the host extends the file and moves that last partition to the new
//! end (APFS addresses blocks relative to its container, so a byte copy is
//! valid), rewriting both GPT copies and the protective MBR. The free space
//! then follows the system container and the Guest can run
//! `diskutil apfs resizeContainer <store> 0`.
use anyhow::{bail, Context, Result};
use std::{
    fs::File,
    os::unix::{fs::FileExt, io::AsRawFd},
    path::Path,
};

const SECTOR: u64 = 512;
const CHUNK: u64 = 1 << 20;
/// Apple_APFS_Recovery, 52637672-7900-11AA-AA11-00306543ECAC (on-disk order).
const APFS_RECOVERY: [u8; 16] = [
    0x72, 0x76, 0x63, 0x52, 0x00, 0x79, 0xaa, 0x11, 0xaa, 0x11, 0x00, 0x30, 0x65, 0x43, 0xec, 0xac,
];

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn read_at(file: &File, offset: u64, len: usize) -> Result<Vec<u8>> {
    let mut buffer = vec![0; len];
    file.read_exact_at(&mut buffer, offset)?;
    Ok(buffer)
}

fn seal_header(header: &mut [u8]) {
    let size = u32_at(header, 12) as usize;
    header[16..20].fill(0);
    let crc = crc32(&header[..size]);
    header[16..20].copy_from_slice(&crc.to_le_bytes());
}

/// Extend `path` to `new_size` bytes (rounded up to 1 MiB) and move a
/// trailing APFS Recovery partition to the new end of the disk.
pub fn grow(path: &Path, new_size: u64) -> Result<()> {
    let new_size = new_size.div_ceil(CHUNK) * CHUNK;
    let file = File::options()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let old_size = file.metadata()?.len();
    if new_size <= old_size {
        bail!(
            "--disk-size must be larger than the image disk ({} GiB)",
            old_size.div_ceil(1 << 30)
        )
    }
    let mut header = read_at(&file, SECTOR, SECTOR as usize)?;
    if &header[..8] != b"EFI PART" {
        bail!("{} has no GPT with 512-byte sectors", path.display())
    }
    let header_size = u32_at(&header, 12) as usize;
    if !(92..=SECTOR as usize).contains(&header_size) {
        bail!("unsupported GPT header size {header_size}")
    }
    let mut check = header[..header_size].to_vec();
    check[16..20].fill(0);
    if crc32(&check) != u32_at(&header, 16) {
        bail!("GPT header checksum mismatch in {}", path.display())
    }
    let entries_lba = u64_at(&header, 72);
    let count = u32_at(&header, 80) as usize;
    let entry_size = u32_at(&header, 84) as usize;
    if entry_size < 128 || count == 0 || count * entry_size > 1 << 20 {
        bail!("unsupported GPT entry layout")
    }
    let mut entries = read_at(&file, entries_lba * SECTOR, count * entry_size)?;
    if crc32(&entries) != u32_at(&header, 88) {
        bail!("GPT entry checksum mismatch in {}", path.display())
    }
    let entry_sectors = ((count * entry_size) as u64).div_ceil(SECTOR);
    let total = new_size / SECTOR;
    // Backup entries end just before the backup header in the last sector.
    let last_usable = total - 2 - entry_sectors;
    let last = (0..count)
        .filter(|i| entries[i * entry_size..i * entry_size + 16] != [0; 16])
        .max_by_key(|i| u64_at(&entries, i * entry_size + 40));
    file.set_len(new_size)?;
    if let Some(index) =
        last.filter(|i| entries[i * entry_size..i * entry_size + 16] == APFS_RECOVERY)
    {
        let at = index * entry_size;
        let (first, end) = (u64_at(&entries, at + 32), u64_at(&entries, at + 40));
        let sectors = end - first + 1;
        let new_first = (last_usable + 1 - sectors) & !7;
        if new_first > first {
            move_range(
                &file,
                first * SECTOR,
                new_first * SECTOR,
                sectors * SECTOR,
                old_size,
            )?;
            entries[at + 32..at + 40].copy_from_slice(&new_first.to_le_bytes());
            entries[at + 40..at + 48].copy_from_slice(&(new_first + sectors - 1).to_le_bytes());
        }
    }
    let entries_crc = crc32(&entries);
    header[32..40].copy_from_slice(&(total - 1).to_le_bytes());
    header[48..56].copy_from_slice(&last_usable.to_le_bytes());
    header[88..92].copy_from_slice(&entries_crc.to_le_bytes());
    seal_header(&mut header);
    let mut backup = header.clone();
    backup[24..32].copy_from_slice(&(total - 1).to_le_bytes());
    backup[32..40].copy_from_slice(&1u64.to_le_bytes());
    backup[72..80].copy_from_slice(&(last_usable + 1).to_le_bytes());
    seal_header(&mut backup);
    file.write_all_at(&entries, entries_lba * SECTOR)?;
    file.write_all_at(&header, SECTOR)?;
    file.write_all_at(&entries, (last_usable + 1) * SECTOR)?;
    file.write_all_at(&backup, (total - 1) * SECTOR)?;
    // The old backup header now sits in free space; clear it.
    file.write_all_at(&[0; SECTOR as usize], old_size - SECTOR)?;
    let mut mbr = read_at(&file, 0, SECTOR as usize)?;
    if mbr[510..512] == [0x55, 0xaa] && mbr[446 + 4] == 0xee {
        let sectors = (total - 1).min(u32::MAX as u64) as u32;
        mbr[446 + 12..446 + 16].copy_from_slice(&sectors.to_le_bytes());
        file.write_all_at(&mbr, 0)?;
    }
    file.sync_all()?;
    Ok(())
}

/// Whether `[start, start + len)` of `file` is entirely a sparse hole.
fn is_hole(file: &File, start: u64, len: u64) -> bool {
    let data = unsafe { libc::lseek(file.as_raw_fd(), start as libc::off_t, libc::SEEK_DATA) };
    // ENXIO: no data at or after `start`.
    data >= 0 && data as u64 >= start + len
        || data < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENXIO)
}

/// Copy `len` bytes from `from` to a higher offset `to`, last chunk first so
/// an overlapping source is read before it is overwritten. Holes and zero
/// chunks are not written beyond the old end of file, which is already a hole.
fn move_range(file: &File, from: u64, to: u64, len: u64, old_size: u64) -> Result<()> {
    let zeros = vec![0; CHUNK as usize];
    let mut remaining = len;
    while remaining > 0 {
        let size = remaining.min(CHUNK);
        remaining -= size;
        let target = to + remaining;
        if is_hole(file, from + remaining, size) {
            if target < old_size {
                file.write_all_at(&zeros[..size as usize], target)?;
            }
            continue;
        }
        let chunk = read_at(file, from + remaining, size as usize)?;
        if target >= old_size && chunk.iter().all(|&b| b == 0) {
            continue;
        }
        file.write_all_at(&chunk, target)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSTEM: [u8; 16] = [
        0xef, 0x57, 0x34, 0x7c, 0x00, 0x00, 0xaa, 0x11, 0xaa, 0x11, 0x00, 0x30, 0x65, 0x43, 0xec,
        0xac,
    ];

    /// A 64 MiB disk laid out like a macOS VM: system container, then a
    /// recovery container ending at the last usable sector.
    fn disk(path: &Path) -> (u64, u64) {
        let total = 64 * 2048;
        let last_usable = total - 34;
        let recovery_first = last_usable + 1 - 8 * 2048;
        let file = File::create(path).unwrap();
        file.set_len(total * SECTOR).unwrap();
        let mut entries = vec![0u8; 128 * 128];
        for (i, (kind, first, last)) in [
            (SYSTEM, 2048, recovery_first - 1),
            (APFS_RECOVERY, recovery_first, last_usable),
        ]
        .into_iter()
        .enumerate()
        {
            entries[i * 128..i * 128 + 16].copy_from_slice(&kind);
            entries[i * 128 + 32..i * 128 + 40].copy_from_slice(&first.to_le_bytes());
            entries[i * 128 + 40..i * 128 + 48].copy_from_slice(&last.to_le_bytes());
        }
        let mut header = vec![0u8; 512];
        header[..8].copy_from_slice(b"EFI PART");
        header[8..12].copy_from_slice(&0x10000u32.to_le_bytes());
        header[12..16].copy_from_slice(&92u32.to_le_bytes());
        header[24..32].copy_from_slice(&1u64.to_le_bytes());
        header[32..40].copy_from_slice(&(total - 1).to_le_bytes());
        header[40..48].copy_from_slice(&34u64.to_le_bytes());
        header[48..56].copy_from_slice(&last_usable.to_le_bytes());
        header[72..80].copy_from_slice(&2u64.to_le_bytes());
        header[80..84].copy_from_slice(&128u32.to_le_bytes());
        header[84..88].copy_from_slice(&128u32.to_le_bytes());
        header[88..92].copy_from_slice(&crc32(&entries).to_le_bytes());
        seal_header(&mut header);
        let mut mbr = vec![0u8; 512];
        mbr[446 + 4] = 0xee;
        mbr[446 + 8] = 1;
        mbr[510] = 0x55;
        mbr[511] = 0xaa;
        file.write_all_at(&mbr, 0).unwrap();
        file.write_all_at(&header, 512).unwrap();
        file.write_all_at(&entries, 1024).unwrap();
        // Recognizable recovery content: first and last sector, one sparse hole.
        file.write_all_at(&[0xa5; 512], recovery_first * SECTOR)
            .unwrap();
        file.write_all_at(&[0x5a; 512], last_usable * SECTOR)
            .unwrap();
        (recovery_first, last_usable)
    }

    fn check(path: &Path, size: u64, moved_first: u64) {
        let file = File::open(path).unwrap();
        assert_eq!(file.metadata().unwrap().len(), size);
        let total = size / SECTOR;
        let header = read_at(&file, SECTOR, 512).unwrap();
        let mut sealed = header.clone();
        seal_header(&mut sealed);
        assert_eq!(header, sealed, "primary header CRC");
        assert_eq!(u64_at(&header, 32), total - 1);
        assert_eq!(u64_at(&header, 48), total - 34);
        let entries = read_at(&file, 1024, 128 * 128).unwrap();
        assert_eq!(crc32(&entries), u32_at(&header, 88));
        let backup = read_at(&file, (total - 1) * SECTOR, 512).unwrap();
        assert_eq!(u64_at(&backup, 24), total - 1);
        assert_eq!(u64_at(&backup, 72), total - 33);
        assert_eq!(
            read_at(&file, (total - 33) * SECTOR, 128 * 128).unwrap(),
            entries
        );
        let (first, last) = (u64_at(&entries, 128 + 32), u64_at(&entries, 128 + 40));
        assert_eq!(first, moved_first);
        assert_eq!(first % 8, 0);
        assert!(last <= total - 34);
        assert_eq!(
            read_at(&file, first * SECTOR, 512).unwrap(),
            vec![0xa5; 512]
        );
        assert_eq!(read_at(&file, last * SECTOR, 512).unwrap(), vec![0x5a; 512]);
        let mbr = read_at(&file, 0, 512).unwrap();
        assert_eq!(u32_at(&mbr, 446 + 12) as u64, total - 1);
    }

    #[test]
    fn grows_and_moves_recovery_to_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        let (first, _) = disk(&path);
        grow(&path, 128 << 20).unwrap();
        let total = (128u64 << 20) / SECTOR;
        let moved = (total - 34 + 1 - 8 * 2048) & !7;
        assert!(moved > first);
        check(&path, 128 << 20, moved);
    }

    #[test]
    fn overlapping_move_keeps_recovery_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        disk(&path);
        // Growth (2 MiB) is smaller than the 8 MiB partition being moved.
        grow(&path, 66 << 20).unwrap();
        let total = (66u64 << 20) / SECTOR;
        check(&path, 66 << 20, (total - 34 + 1 - 8 * 2048) & !7);
    }

    #[test]
    fn refuses_to_shrink_or_damaged_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        disk(&path);
        assert!(grow(&path, 32 << 20).is_err());
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .write_all_at(&[1], 1024)
            .unwrap();
        assert!(grow(&path, 128 << 20)
            .unwrap_err()
            .to_string()
            .contains("checksum"));
    }
}
