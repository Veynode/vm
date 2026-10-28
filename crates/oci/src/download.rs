//! Pinned downloads: a file fetched once from a fixed URL and kept only when
//! its size and SHA-256 are the ones pinned in the source.
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    time::Duration,
};

/// A file VeynodeVM downloads and checks before use.
pub struct Pinned {
    pub url: &'static str,
    pub size: u64,
    /// Lowercase hex SHA-256.
    pub sha256: &'static str,
}

/// `pinned` at `dest`, downloaded when it is missing. A file already at
/// `dest` is trusted only if it matches; one that doesn't is fetched again.
pub fn fetch(pinned: &Pinned, dest: &Path, progress: &mut dyn FnMut(&str)) -> Result<()> {
    if dest.is_file() && matches(pinned, &mut File::open(dest)?)? {
        return Ok(());
    }
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    let part = dest.with_extension("part");
    progress(&format!(
        "Downloading {} ({} MB)",
        pinned.url,
        pinned.size >> 20
    ));
    let response = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(120))
        .user_agent("veynodevm/0.1")
        .build()
        .get(pinned.url)
        .call()
        .with_context(|| format!("downloading {}", pinned.url))?;
    let mut body = response.into_reader().take(pinned.size + 1);
    let mut out = File::create(&part)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let (mut total, mut reported) = (0u64, 0u64);
    loop {
        let n = body.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        out.write_all(&buffer[..n])?;
        total += n as u64;
        if total - reported >= 32 << 20 {
            reported = total;
            progress(&format!(
                "Downloaded {} of {} MB",
                total >> 20,
                pinned.size >> 20
            ));
        }
    }
    out.sync_all()?;
    let digest = format!("{:x}", hasher.finalize());
    if total != pinned.size || digest != pinned.sha256 {
        let _ = fs::remove_file(&part);
        bail!(
            "{} is not the pinned file: {total} bytes, sha256 {digest} (expected {} bytes, {})",
            pinned.url,
            pinned.size,
            pinned.sha256
        );
    }
    fs::rename(&part, dest)?;
    Ok(())
}

fn matches(pinned: &Pinned, file: &mut File) -> Result<bool> {
    if file.metadata()?.len() != pinned.size {
        return Ok(false);
    }
    let mut hasher = Sha256::new();
    std::io::copy(file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()) == pinned.sha256)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_in_place_is_used_only_when_it_matches() {
        let dir = std::env::temp_dir().join(format!("vvm-pinned-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("f");
        fs::write(&dest, b"abc").unwrap();
        let good = Pinned {
            url: "http://127.0.0.1:9/never",
            size: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        let mut said = Vec::new();
        fetch(&good, &dest, &mut |m| said.push(m.to_owned())).unwrap();
        assert!(said.is_empty());
        let bad = Pinned {
            sha256: "00",
            ..good
        };
        // A mismatch fetches again; nothing listens on port 9.
        assert!(fetch(&bad, &dest, &mut |_| {}).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
