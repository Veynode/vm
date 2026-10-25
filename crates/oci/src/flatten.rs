//! Flatten OCI layers into one tar stream with whiteouts applied.
//!
//! Layers are first decompressed to seekable temporary tar files. A top-down
//! pass selects the visible entry for every path (OCI `.wh.` whiteouts, opaque
//! directories, upper non-directories hiding lower descendants). The output is
//! written sorted by path components so parents precede children, with hard
//! links last so their targets exist. Extended attributes are not preserved.
use anyhow::{bail, Context, Result};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
};

const WHITEOUT: &str = ".wh.";
const OPAQUE: &str = ".wh..wh..opq";

/// Decompress a layer blob (gzip, zstd or plain tar, detected by magic).
pub fn decompress(blob: &Path, out: &mut File) -> Result<()> {
    let mut input = BufReader::new(File::open(blob)?);
    let mut magic = [0u8; 4];
    let n = input.read(&mut magic)?;
    input.seek(SeekFrom::Start(0))?;
    match &magic[..n.min(4)] {
        [0x1f, 0x8b, ..] => {
            std::io::copy(&mut flate2::read::MultiGzDecoder::new(input), out)?;
        }
        [0x28, 0xb5, 0x2f, 0xfd] => {
            std::io::copy(&mut zstd::stream::read::Decoder::new(input)?, out)?;
        }
        _ => {
            std::io::copy(&mut input, out)?;
        }
    }
    out.flush()?;
    Ok(())
}

/// Normalize a tar path: no leading `./` or `/`, no trailing `/`, no `..`.
pub fn normalize(path: &str) -> Result<String> {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => bail!("layer path escapes root: {path}"),
            p => parts.push(p),
        }
    }
    Ok(parts.join("/"))
}

fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(move |(i, _)| &path[..i])
}

#[derive(Clone)]
struct Selected {
    layer: usize,
    header: tar::Header,
    data_position: u64,
    size: u64,
    link: Option<String>,
    is_dir: bool,
    is_hardlink: bool,
}

#[derive(Default)]
struct View {
    selected: HashMap<String, Selected>,
    /// Paths (and their descendants) whited out by an upper layer.
    whiteouts: HashSet<String>,
    /// Directories whose lower-layer descendants are hidden.
    opaque: HashSet<String>,
    /// Non-directory paths chosen in an upper layer: they hide lower descendants.
    non_dirs: HashSet<String>,
}

impl View {
    fn hidden(&self, path: &str) -> bool {
        if self.whiteouts.contains(path) {
            return true;
        }
        ancestors(path).any(|a| {
            self.whiteouts.contains(a) || self.opaque.contains(a) || self.non_dirs.contains(a)
        })
    }
}

pub struct Stats {
    pub entries: usize,
    pub skipped_hardlinks: usize,
}

/// Flatten decompressed layer tars (bottom first) into `out`.
pub fn flatten(layers: &[File], out: impl Write) -> Result<Stats> {
    let mut view = View::default();
    for (index, file) in layers.iter().enumerate().rev() {
        let mut layer_whiteouts = HashSet::new();
        let mut layer_opaque = HashSet::new();
        let mut layer_selected: BTreeMap<String, Selected> = BTreeMap::new();
        let mut reader = file.try_clone()?;
        reader.seek(SeekFrom::Start(0))?;
        let mut archive = tar::Archive::new(BufReader::new(reader));
        for entry in archive.entries()? {
            let entry = entry?;
            let kind = entry.header().entry_type();
            if matches!(
                kind,
                tar::EntryType::XHeader
                    | tar::EntryType::XGlobalHeader
                    | tar::EntryType::GNULongName
                    | tar::EntryType::GNULongLink
            ) {
                continue;
            }
            let raw = entry.path()?;
            let path = normalize(raw.to_str().context("non-UTF-8 layer path")?)?;
            if path.is_empty() {
                continue;
            }
            let (parent, name) = match path.rsplit_once('/') {
                Some((p, n)) => (p, n),
                None => ("", path.as_str()),
            };
            if name == OPAQUE {
                layer_opaque.insert(parent.to_owned());
                continue;
            }
            if let Some(target) = name.strip_prefix(WHITEOUT) {
                let target = if parent.is_empty() {
                    target.to_owned()
                } else {
                    format!("{parent}/{target}")
                };
                layer_whiteouts.insert(target);
                continue;
            }
            if view.selected.contains_key(&path) || view.hidden(&path) {
                continue;
            }
            let link = entry
                .link_name()?
                .map(|l| -> Result<String> {
                    let l = l.to_str().context("non-UTF-8 link name")?.to_owned();
                    Ok(if kind == tar::EntryType::Link {
                        normalize(&l)?
                    } else {
                        l
                    })
                })
                .transpose()?;
            let selected = Selected {
                layer: index,
                header: entry.header().clone(),
                data_position: entry.raw_file_position(),
                size: entry.header().size()?,
                link,
                is_dir: kind == tar::EntryType::Directory,
                is_hardlink: kind == tar::EntryType::Link,
            };
            // Within one layer a later duplicate wins, as when extracting.
            layer_selected.insert(path, selected);
        }
        for (path, selected) in layer_selected {
            if !selected.is_dir {
                view.non_dirs.insert(path.clone());
            }
            view.selected.insert(path, selected);
        }
        view.whiteouts.extend(layer_whiteouts);
        view.opaque.extend(layer_opaque);
    }
    // A lower descendant of a later-hidden upper path never got selected; but a
    // path selected before an upper non-directory was known can't occur since
    // layers are processed top-down. Drop anything still under a non-directory.
    let mut ordered: Vec<(&String, &Selected)> = view
        .selected
        .iter()
        .filter(|(p, _)| !ancestors(p).any(|a| view.selected.get(a).is_some_and(|s| !s.is_dir)))
        .collect();
    ordered.sort_by(|(a, sa), (b, sb)| {
        sa.is_hardlink
            .cmp(&sb.is_hardlink)
            .then_with(|| a.split('/').cmp(b.split('/')))
    });
    let mut builder = tar::Builder::new(out);
    builder.mode(tar::HeaderMode::Complete);
    let mut files: Vec<File> = layers
        .iter()
        .map(File::try_clone)
        .collect::<std::io::Result<_>>()?;
    let mut stats = Stats {
        entries: 0,
        skipped_hardlinks: 0,
    };
    for (path, s) in ordered {
        let mut header = s.header.clone();
        if s.is_hardlink {
            let target = s.link.as_deref().unwrap_or_default();
            if !view
                .selected
                .get(target)
                .is_some_and(|t| !t.is_dir && !t.is_hardlink)
            {
                stats.skipped_hardlinks += 1;
                continue;
            }
            header.set_size(0);
            builder.append_link(&mut header, path, target)?;
        } else if header.entry_type() == tar::EntryType::Symlink {
            header.set_size(0);
            builder.append_link(&mut header, path, s.link.as_deref().unwrap_or_default())?;
        } else if header.entry_type().is_file() || header.entry_type() == tar::EntryType::Continuous
        {
            let file = &mut files[s.layer];
            file.seek(SeekFrom::Start(s.data_position))?;
            header.set_entry_type(tar::EntryType::Regular);
            builder.append_data(&mut header, path, file.take(s.size))?;
        } else {
            header.set_size(0);
            builder.append_data(&mut header, path, std::io::empty())?;
        }
        stats.entries += 1;
    }
    builder.into_inner()?.flush()?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn layer(entries: &[(&str, Option<&[u8]>)]) -> File {
        let mut file = tempfile::tempfile().unwrap();
        {
            let mut b = tar::Builder::new(&mut file);
            for (path, data) in entries {
                let mut h = tar::Header::new_gnu();
                h.set_mode(0o755);
                match data {
                    Some(d) => {
                        h.set_entry_type(tar::EntryType::Regular);
                        h.set_size(d.len() as u64);
                        b.append_data(&mut h, path, *d).unwrap();
                    }
                    None => {
                        h.set_entry_type(tar::EntryType::Directory);
                        h.set_size(0);
                        b.append_data(&mut h, path, std::io::empty()).unwrap();
                    }
                }
            }
            b.finish().unwrap();
        }
        file
    }
    fn read(out: &[u8]) -> Vec<(String, Vec<u8>)> {
        tar::Archive::new(out)
            .entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                let mut d = Vec::new();
                e.read_to_end(&mut d).unwrap();
                (e.path().unwrap().to_string_lossy().into_owned(), d)
            })
            .collect()
    }
    #[test]
    fn whiteouts_opaque_and_overrides() {
        let lower = layer(&[
            ("etc/", None),
            ("etc/a", Some(b"lower-a")),
            ("etc/b", Some(b"lower-b")),
            ("opt/", None),
            ("opt/old/", None),
            ("opt/old/file", Some(b"x")),
            ("var/", None),
            ("var/keep", Some(b"k")),
            ("dir-becomes-file/", None),
            ("dir-becomes-file/child", Some(b"c")),
        ]);
        let upper = layer(&[
            ("etc/a", Some(b"upper-a")),
            ("etc/.wh.b", Some(b"")),
            ("opt/", None),
            ("opt/.wh..wh..opq", Some(b"")),
            ("opt/new", Some(b"n")),
            ("dir-becomes-file", Some(b"f")),
        ]);
        let mut out = Vec::new();
        let stats = flatten(&[lower, upper], &mut out).unwrap();
        let entries = read(&out);
        let names: Vec<_> = entries.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            names,
            [
                "dir-becomes-file",
                "etc",
                "etc/a",
                "opt",
                "opt/new",
                "var",
                "var/keep"
            ]
        );
        assert_eq!(entries[2].1, b"upper-a");
        assert_eq!(stats.entries, 7);
    }
    #[test]
    fn parents_sort_before_children() {
        let l = layer(&[
            ("a/b/", None),
            ("a/", None),
            ("a/b/c", Some(b"1")),
            ("a-b", Some(b"2")),
        ]);
        let mut out = Vec::new();
        flatten(&[l], &mut out).unwrap();
        let names: Vec<_> = read(&out).into_iter().map(|(p, _)| p).collect();
        assert_eq!(names, ["a", "a/b", "a/b/c", "a-b"]);
    }
    #[test]
    fn traversal_is_refused() {
        assert!(normalize("../etc/passwd").is_err());
        assert_eq!(normalize("./usr//bin/").unwrap(), "usr/bin");
    }
}
