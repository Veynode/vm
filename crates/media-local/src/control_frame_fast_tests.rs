//! The fast control-frame codec against the original implementation (`reference`), which is the oracle: same
//! bytes out of the inflater, the PNG decoder and the downscaler, the same JPEG bytes whatever the thread count,
//! and the same divisor / quality choice. Inputs cover every PNG row filter and colour type, stored / fixed /
//! dynamic DEFLATE blocks, split IDAT, odd sizes and extents that are no multiple of the divisor.
use super::bench_tests::{desktop, eight_colour_noise, imageio_png};
use super::tests::{noise, screenshot, stored_png};
use super::*;

// --- a small test-only DEFLATE writer: stored blocks and fixed-Huffman blocks with real LZ77 matches ---
struct BitWriter {
    out: Vec<u8>,
    buf: u64,
    count: u32,
}
impl BitWriter {
    fn put(&mut self, value: u32, bits: u32) {
        self.buf |= (value as u64) << self.count;
        self.count += bits;
        while self.count >= 8 {
            self.out.push(self.buf as u8);
            self.buf >>= 8;
            self.count -= 8;
        }
    }
    /// Huffman codes are written most significant bit first.
    fn code(&mut self, code: u32, bits: u32) {
        self.put(code.reverse_bits() >> (32 - bits), bits);
    }
    fn align(&mut self) {
        if self.count > 0 {
            self.put(0, 8 - self.count);
        }
    }
    fn literal(&mut self, value: usize) {
        match value {
            0..=143 => self.code(0x30 + value as u32, 8),
            144..=255 => self.code(0x190 + (value as u32 - 144), 9),
            256..=279 => self.code(value as u32 - 256, 7),
            _ => self.code(0xc0 + (value as u32 - 280), 8),
        }
    }
}
const TEST_LENGTH_BASE: [usize; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const TEST_DISTANCE_BASE: [usize; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
/// One fixed-Huffman block holding `data[from..to]`; matches may reach back before `from` and may overlap.
fn fixed_block(writer: &mut BitWriter, data: &[u8], from: usize, to: usize, last: bool) {
    writer.put(last as u32, 1);
    writer.put(1, 2);
    let mut recent = std::collections::HashMap::<[u8; 3], usize>::new();
    for at in from.saturating_sub(32768)..from {
        if at + 3 <= data.len() {
            recent.insert([data[at], data[at + 1], data[at + 2]], at);
        }
    }
    let mut at = from;
    while at < to {
        let key = (at + 3 <= to).then(|| [data[at], data[at + 1], data[at + 2]]);
        let candidate = key.and_then(|key| recent.get(&key).copied());
        let mut length = 0;
        if let Some(candidate) = candidate.filter(|candidate| at - candidate <= 32768) {
            while length < 258 && at + length < to && data[candidate + length] == data[at + length]
            {
                length += 1;
            }
        }
        if length >= 3 {
            let back = at - candidate.unwrap();
            let symbol = TEST_LENGTH_BASE
                .iter()
                .rposition(|base| *base <= length)
                .unwrap();
            writer.literal(257 + symbol);
            let extra = [
                0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5,
                0,
            ][symbol];
            writer.put((length - TEST_LENGTH_BASE[symbol]) as u32, extra);
            let code = TEST_DISTANCE_BASE
                .iter()
                .rposition(|base| *base <= back)
                .unwrap();
            writer.code(code as u32, 5);
            writer.put(
                (back - TEST_DISTANCE_BASE[code]) as u32,
                (code as u32 / 2).saturating_sub(1),
            );
        } else {
            writer.literal(data[at] as usize);
            length = 1;
        }
        for position in at..at + length {
            if position + 3 <= to {
                recent.insert(
                    [data[position], data[position + 1], data[position + 2]],
                    position,
                );
            }
        }
        at += length;
    }
    writer.literal(256);
}
fn stored_block(writer: &mut BitWriter, data: &[u8], last: bool) {
    assert!(data.len() <= 65535);
    writer.put(last as u32, 1);
    writer.put(0, 2);
    writer.align();
    writer.put(data.len() as u32, 16);
    writer.put(!(data.len() as u32) & 0xffff, 16);
    writer.out.extend_from_slice(data);
}
/// A zlib stream of `data` cut into blocks of about `block` bytes; `kinds` is cycled: 0 stored, 1 fixed.
fn test_zlib(data: &[u8], block: usize, kinds: &[u8]) -> Vec<u8> {
    let mut writer = BitWriter {
        out: vec![0x78, 0x01],
        buf: 0,
        count: 0,
    };
    let starts: Vec<usize> = (0..data.len().max(1)).step_by(block).collect();
    for (index, &from) in starts.iter().enumerate() {
        let to = (from + block).min(data.len());
        let last = index + 1 == starts.len();
        if kinds[index % kinds.len()] == 0 {
            stored_block(&mut writer, &data[from..to], last);
        } else {
            fixed_block(&mut writer, data, from, to, last);
        }
    }
    writer.align();
    let mut out = writer.out;
    out.extend_from_slice(&simple_adler32(data).to_be_bytes());
    out
}
fn simple_adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}
fn simple_crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                0xedb8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
/// Data with literals, short and long matches, far matches and overlapping runs.
fn mixed_data(length: usize, seed: u64) -> Vec<u8> {
    let random = noise(length, seed);
    let mut out = Vec::with_capacity(length);
    let mut index = 0;
    while out.len() < length {
        let choice = random[index % random.len()] % 6;
        let span =
            1 + random[(index + 1) % random.len()] as usize * (1 + (choice as usize % 3) * 3);
        index += 2;
        match choice {
            0 => out.extend_from_slice(&random[index % 977..][..span.min(600)]),
            1 => out.extend(std::iter::repeat_n(random[index % random.len()], span)),
            2 if out.len() > 4 => {
                // An overlapping run with a short period.
                let period = 1 + (span % 7).min(out.len() - 1);
                for _ in 0..span {
                    out.push(out[out.len() - period]);
                }
            }
            3 if out.len() > 30_000 => {
                let from = out.len() - 30_000 - (span % 2000);
                out.extend_from_within(from..from + span.min(700));
            }
            4 if out.len() > 300 => {
                let from = out.len() - 9 - (span % 280);
                for offset in 0..span.min(400) {
                    out.push(out[from + offset]);
                }
            }
            _ => out.extend((0..span.min(300)).map(|i| (i * 7) as u8)),
        }
    }
    out.truncate(length);
    out
}
fn python() -> Option<&'static str> {
    [
        "/usr/bin/python3",
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
    ]
    .into_iter()
    .find(|path| std::path::Path::new(path).exists())
}
/// Both inflaters on one stream: both refuse, or both return the same bytes.
fn same_inflate(label: &str, stream: &[u8], expected: usize) -> Option<Vec<u8>> {
    let fast = zlib_inflate(stream, expected);
    let slow = reference::zlib_inflate(stream, expected);
    match (fast, slow) {
        (Ok(fast), Ok(slow)) => {
            assert!(fast == slow, "{label}: inflated bytes differ");
            Some(fast)
        }
        (Err(_), Err(_)) => None,
        (fast, slow) => panic!(
            "{label}: fast {:?} but reference {:?}",
            fast.map(|v| v.len()).map_err(|e| e.to_string()),
            slow.map(|v| v.len()).map_err(|e| e.to_string())
        ),
    }
}

#[test]
fn checksums_match_the_bytewise_definitions() {
    assert_eq!(crc32(&[b"123456789"]), 0xcbf4_3926);
    assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    let data = noise(70_001, 9);
    for length in [0, 1, 7, 8, 9, 15, 16, 17, 5551, 5552, 5553, 11_104, 70_001] {
        assert_eq!(
            crc32(&[&data[..length]]),
            simple_crc32(&data[..length]),
            "{length}"
        );
        assert_eq!(
            adler32(&data[..length]),
            simple_adler32(&data[..length]),
            "{length}"
        );
        // Split anywhere, the CRC of the parts is the CRC of the whole.
        let cut = length / 3;
        assert_eq!(
            crc32(&[&data[..cut], &data[cut..length]]),
            simple_crc32(&data[..length])
        );
    }
    // The largest sums the Adler block form can meet: all 0xff, whole blocks.
    let full = vec![0xffu8; 5552 * 3 + 5];
    assert_eq!(adler32(&full), simple_adler32(&full));
}
#[test]
fn fast_inflate_matches_the_reference_on_stored_fixed_and_mixed_blocks() {
    for (label, data) in [
        ("empty", Vec::new()),
        ("one", vec![7]),
        ("mixed", mixed_data(200_000, 1)),
        ("mixed-2", mixed_data(70_000, 2)),
        ("noise", noise(50_000, 3)),
        ("zeros", vec![0; 100_000]),
        (
            "period-3",
            (0..90_000).map(|i| [10u8, 200, 30][i % 3]).collect(),
        ),
        ("far", {
            let mut data = noise(30_000, 4);
            data.extend_from_within(..);
            data.extend_from_within(..3000);
            data
        }),
    ] {
        for (block, kinds) in [
            (65_535usize, &[1u8][..]),
            (65_535, &[0][..]),
            (9000, &[0, 1][..]),
            (4000, &[1, 1, 0][..]),
            (700, &[1, 0, 0, 1][..]),
        ] {
            let stream = test_zlib(&data, block, kinds);
            let name = format!("{label} block {block} kinds {kinds:?}");
            // The test writer really emits matches (overlapping ones for the runs), not only literals.
            // (The first 30,000 bytes of "far" are incompressible; everything after them is far matches.)
            if kinds == [1] {
                match label {
                    "zeros" | "period-3" => assert!(stream.len() < data.len() / 20, "{name}"),
                    "far" => assert!(stream.len() < 30_000 * 9 / 8 + 1000, "{name}"),
                    _ => {}
                }
            }
            let inflated = same_inflate(&name, &stream, data.len()).expect(&name);
            assert!(inflated == data, "{name}: wrong bytes");
            // Geometry and stream must agree exactly, both ways.
            assert!(same_inflate(&name, &stream, data.len() + 1).is_none());
            if !data.is_empty() {
                assert!(same_inflate(&name, &stream, data.len() - 1).is_none());
            }
        }
    }
}
#[test]
fn fast_inflate_matches_the_reference_on_damaged_streams() {
    // Every single-bit flip of a short stream, and many flips and truncations of longer ones: the two inflaters
    // must agree on refusal and, when both accept, on the bytes.
    let data = mixed_data(600, 5);
    let stream = test_zlib(&data, 200, &[1, 0, 1]);
    for bit in 0..stream.len() * 8 {
        let mut changed = stream.clone();
        changed[bit / 8] ^= 1 << (bit % 8);
        same_inflate(&format!("flip {bit}"), &changed, data.len());
    }
    for cut in 0..stream.len() {
        assert!(same_inflate(&format!("cut {cut}"), &stream[..cut], data.len()).is_none());
    }
    let data = mixed_data(40_000, 6);
    let mut streams = vec![test_zlib(&data, 7000, &[1, 1, 0])];
    for name in [
        "rgb-dynamic",
        "rgba-fixed",
        "gray-alpha-huffman-only",
        "desktop-pil",
    ] {
        streams.push(fixture_idat(name).0);
    }
    for (index, stream) in streams.iter().enumerate() {
        let expected = reference::zlib_inflate(stream, usize::MAX >> 8)
            .map(|v| v.len())
            .unwrap_or(data.len());
        let random = noise(4000, 40 + index as u64);
        for (round, pair) in random.chunks_exact(4).enumerate() {
            let mut changed = stream.clone();
            let at = u32::from_le_bytes(pair.try_into().unwrap()) as usize % (changed.len() * 8);
            changed[at / 8] ^= 1 << (at % 8);
            if round % 5 == 0 {
                let second = (at * 31 + round) % (changed.len() * 8);
                changed[second / 8] ^= 1 << (second % 8);
            }
            if round % 7 == 0 {
                changed.truncate(at / 8 + 1);
            }
            same_inflate(&format!("stream {index} round {round}"), &changed, expected);
        }
    }
}
fn fixture_bytes(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/fixtures/control_frame")
        .join(format!("{name}.png"));
    std::fs::read(path).unwrap()
}
/// The concatenated IDAT data of a fixture and its bytes.
fn fixture_idat(name: &str) -> (Vec<u8>, Vec<u8>) {
    let png = fixture_bytes(name);
    let (mut position, mut out) = (8, Vec::new());
    while position + 12 <= png.len() {
        let length = u32::from_be_bytes(png[position..position + 4].try_into().unwrap()) as usize;
        if &png[position + 4..position + 8] == b"IDAT" {
            out.extend_from_slice(&png[position + 8..position + 8 + length]);
        }
        position += 12 + length;
    }
    (out, png)
}
#[test]
fn fast_inflate_matches_the_reference_on_python_zlib_streams() {
    let Some(python) = python() else {
        eprintln!(
            "python3 unavailable; dynamic-block streams from zlib not checked (fixtures still are)"
        );
        return;
    };
    // Dynamic blocks at several levels and strategies (default, filtered, Huffman-only, RLE, fixed), with full
    // flushes in the middle so that one stream holds many blocks of different kinds, and a 512-byte window.
    const SCRIPT: &str = r#"
import sys, zlib
out = sys.argv[1]
for name in ('mixed', 'noise', 'picture'):
    data = open(out + '/' + name + '.raw', 'rb').read()
    for level in (0, 1, 6, 9):
        for strategy in (0, 1, 2, 3, 4):
            for wbits in (15, 9):
                c = zlib.compressobj(level, zlib.DEFLATED, wbits, 8, strategy)
                third = len(data) // 3
                z = c.compress(data[:third]) + c.flush(zlib.Z_FULL_FLUSH)
                z += c.compress(data[third:2 * third]) + c.flush(zlib.Z_SYNC_FLUSH)
                z += c.compress(data[2 * third:]) + c.flush()
                open('%s/%s-%d-%d-%d.z' % (out, name, level, strategy, wbits), 'wb').write(z)
"#;
    let dir = tempfile::tempdir().unwrap();
    let inputs = [
        ("mixed", mixed_data(300_000, 7)),
        ("noise", eight_colour_noise(300, 200)),
        ("picture", desktop(400, 300)),
    ];
    for (name, data) in &inputs {
        std::fs::write(dir.path().join(format!("{name}.raw")), data).unwrap();
    }
    let status = std::process::Command::new(python)
        .args(["-I", "-c", SCRIPT])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(status.status.success(), "python: {status:?}");
    let mut checked = 0;
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "z") {
            continue;
        }
        let label = path.file_stem().unwrap().to_string_lossy().into_owned();
        let data = &inputs
            .iter()
            .find(|(name, _)| label.starts_with(name))
            .unwrap()
            .1;
        let stream = std::fs::read(&path).unwrap();
        let inflated = same_inflate(&label, &stream, data.len()).expect(&label);
        assert!(&inflated == data, "{label}");
        // A few damaged copies of each.
        for (round, pair) in noise(160, checked).chunks_exact(4).enumerate() {
            let mut changed = stream.clone();
            let at = u32::from_le_bytes(pair.try_into().unwrap()) as usize % (changed.len() * 8);
            changed[at / 8] ^= 1 << (at % 8);
            same_inflate(&format!("{label} damaged {round}"), &changed, data.len());
        }
        checked += 1;
    }
    assert_eq!(checked, 3 * 4 * 5 * 2);
}

// --- PNG ---
fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = (data.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&simple_crc32(&[kind.as_slice(), data].concat()).to_be_bytes());
    out
}
/// A PNG of `samples` (`channels` bytes per pixel) with row filter `filter(y)` applied, compressed by the test
/// writer and cut into IDAT chunks of `idat` bytes.
fn filtered_png(
    (width, height): (usize, usize),
    color: u8,
    samples: &[u8],
    filter: impl Fn(usize) -> u8,
    block: usize,
    kinds: &[u8],
    idat: usize,
) -> Vec<u8> {
    let channels = match color {
        0 | 3 => 1,
        2 => 3,
        4 => 2,
        _ => 4,
    };
    let stride = width * channels;
    assert_eq!(samples.len(), stride * height);
    let mut raw = Vec::with_capacity((stride + 1) * height);
    let zero = vec![0u8; stride];
    for y in 0..height {
        let line = &samples[y * stride..(y + 1) * stride];
        let above = if y == 0 {
            &zero[..]
        } else {
            &samples[(y - 1) * stride..y * stride]
        };
        let kind = filter(y);
        raw.push(kind);
        for i in 0..stride {
            let a = if i >= channels {
                line[i - channels] as i32
            } else {
                0
            };
            let b = above[i] as i32;
            let c = if i >= channels {
                above[i - channels] as i32
            } else {
                0
            };
            let predictor = match kind {
                0 => 0,
                1 => a,
                2 => b,
                3 => (a + b) / 2,
                _ => {
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
            };
            raw.push(line[i].wrapping_sub(predictor as u8));
        }
    }
    let zlib = test_zlib(&raw, block, kinds);
    let mut header = Vec::new();
    header.extend_from_slice(&(width as u32).to_be_bytes());
    header.extend_from_slice(&(height as u32).to_be_bytes());
    header.extend_from_slice(&[8, color, 0, 0, 0]);
    let mut out = PNG_SIGNATURE.to_vec();
    out.extend(png_chunk(b"IHDR", &header));
    if color == 3 {
        let palette: Vec<u8> = (0..256usize)
            .flat_map(|i| [i as u8, (255 - i) as u8, (i * 3) as u8])
            .collect();
        out.extend(png_chunk(b"PLTE", &palette));
    }
    for part in zlib.chunks(idat) {
        out.extend(png_chunk(b"IDAT", part));
    }
    out.extend(png_chunk(b"IEND", &[]));
    out
}
/// What the PNG must decode to, stated independently of both decoders.
fn expected_rgb(color: u8, samples: &[u8]) -> Vec<u8> {
    match color {
        0 => samples.iter().flat_map(|g| [*g, *g, *g]).collect(),
        2 => samples.to_vec(),
        3 => samples
            .iter()
            .flat_map(|i| [*i, 255 - *i, i.wrapping_mul(3)])
            .collect(),
        4 => samples
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0]])
            .collect(),
        _ => samples
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect(),
    }
}
#[test]
fn fast_png_decode_matches_the_reference_for_every_filter_colour_type_and_size() {
    let mut checked = 0;
    for (width, height) in [
        (1usize, 1usize),
        (1, 9),
        (2, 3),
        (5, 3),
        (7, 16),
        (33, 17),
        (96, 64),
        (257, 31),
    ] {
        for color in [0u8, 2, 3, 4, 6] {
            let channels = match color {
                0 | 3 => 1,
                2 => 3,
                4 => 2,
                _ => 4,
            };
            // Smooth and noisy halves so that every Paeth branch and every carry is met.
            let mut samples = noise(
                width * height * channels,
                (width * 31 + height + color as usize) as u64,
            );
            for (index, value) in samples
                .iter_mut()
                .enumerate()
                .take(width * height * channels / 2)
            {
                *value = (index / channels * 3 + index % channels * 40) as u8;
            }
            let expected = expected_rgb(color, &samples);
            for mode in 0..7u8 {
                // 0..=4: that filter on every row; 5: a different filter per row; 6: per row, reversed order.
                let filter = |y: usize| match mode {
                    0..=4 => mode,
                    5 => (y % 5) as u8,
                    _ => (4 - y % 5) as u8,
                };
                for (block, kinds, idat) in [
                    (65_535usize, &[1u8][..], usize::MAX),
                    (300, &[0, 1][..], 97),
                    (1000, &[0][..], 1),
                ] {
                    if idat == 1 && width * height > 600 {
                        continue;
                    }
                    let png =
                        filtered_png((width, height), color, &samples, filter, block, kinds, idat);
                    let label =
                        format!("{width}x{height} colour {color} filter mode {mode} idat {idat}");
                    let fast = decode_png(&png).unwrap_or_else(|e| panic!("{label}: {e}"));
                    let slow = reference::decode_png(&png).unwrap();
                    assert_eq!((fast.width, fast.height), (width, height), "{label}");
                    assert!(
                        fast.data == slow.data,
                        "{label}: differs from the reference"
                    );
                    assert!(fast.data == expected, "{label}: wrong pixels");
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 700, "{checked}");
    // The in-place form used by production (rows keep their filter byte) holds the same pixels.
    let samples = noise(33 * 17 * 3, 8);
    let png = filtered_png((33, 17), 2, &samples, |y| (y % 5) as u8, 500, &[1, 0], 64);
    let decoded = decode_png_picture(&png, &mut FrameTiming::default()).unwrap();
    let picture = decoded.picture();
    for y in 0..17 {
        assert_eq!(picture.row(y), &samples[y * 99..(y + 1) * 99]);
    }
    // Every fixture written by an independent encoder.
    for name in [
        "rgb-dynamic",
        "rgb-multi-idat",
        "rgba-fixed",
        "gray-dynamic",
        "gray-alpha-huffman-only",
        "palette-dynamic",
        "desktop-pil",
        "desktop-imageio",
    ] {
        let png = fixture_bytes(name);
        assert!(
            decode_png(&png).unwrap().data == reference::decode_png(&png).unwrap().data,
            "{name}"
        );
    }
}
#[test]
fn fast_png_decode_refuses_what_the_reference_refuses() {
    let samples = noise(40 * 30 * 3, 12);
    let png = filtered_png((40, 30), 2, &samples, |y| (y % 5) as u8, 800, &[1, 0], 500);
    let random = noise(8000, 13);
    let (mut accepted, mut refused) = (0, 0);
    for (round, pair) in random.chunks_exact(4).enumerate() {
        let mut changed = png.clone();
        let at = u32::from_le_bytes(pair.try_into().unwrap()) as usize % (changed.len() * 8);
        changed[at / 8] ^= 1 << (at % 8);
        // Repair the chunk checksums so that the damage reaches the inflater and the filters.
        if round % 2 == 0 {
            let mut position = 8;
            while position + 12 <= changed.len() {
                let length = u32::from_be_bytes(changed[position..position + 4].try_into().unwrap())
                    as usize;
                if position + 12 + length > changed.len() {
                    break;
                }
                let crc = simple_crc32(&changed[position + 4..position + 8 + length]);
                changed[position + 8 + length..position + 12 + length]
                    .copy_from_slice(&crc.to_be_bytes());
                position += 12 + length;
            }
        }
        let code = |result: &Result<Rgb>| {
            result
                .as_ref()
                .err()
                .map(|e| FailureCode::of(e).map(FailureCode::name))
        };
        let (fast, slow) = (decode_png(&changed), reference::decode_png(&changed));
        assert_eq!(code(&fast), code(&slow), "round {round}: refusal differs");
        match (fast, slow) {
            (Ok(fast), Ok(slow)) => {
                assert!(fast.data == slow.data, "round {round}");
                accepted += 1;
            }
            _ => refused += 1,
        }
    }
    assert!(
        accepted > 0 && refused > 500,
        "{accepted} accepted, {refused} refused"
    );
}

// --- downscale ---
#[test]
fn reciprocal_division_is_exact_for_every_block_area() {
    for area in 1..=256u32 {
        let scale = reciprocal(area);
        for x in 0..=255 * area + area / 2 {
            assert_eq!(divide(x, scale) as u32, x / area, "{x} / {area}");
        }
    }
}
#[test]
fn fast_downscale_matches_the_reference_for_every_divisor_and_odd_extents() {
    let mut sizes: Vec<(usize, usize)> = Vec::new();
    for width in [1usize, 2, 3, 5, 7, 16, 17, 31, 33, 47, 48, 49] {
        for height in [1usize, 2, 3, 15, 16, 17, 35] {
            sizes.push((width, height));
        }
    }
    // Large enough for several worker bands, with partial last blocks in both directions.
    sizes.extend([(641, 1027), (1283, 517), (300, 4099), (2561, 1441)]);
    for (width, height) in sizes {
        let mut data = noise(width * height * 3, (width * 131 + height) as u64);
        // Saturated rows and columns: the largest block sums.
        for value in data.iter_mut().take(width * 3 * (height / 2)) {
            *value = 255;
        }
        let source = Rgb {
            width,
            height,
            data,
        };
        for divisor in DIVISORS {
            if width * height > 1_000_000 && !matches!(divisor, 2 | 3 | 16) {
                continue;
            }
            let (fast, slow) = (
                downscale(&source, divisor),
                reference::downscale(&source, divisor),
            );
            assert_eq!((fast.width, fast.height), (slow.width, slow.height));
            assert_eq!(
                fast.width as u64,
                frame_extent(width as u64, divisor as u64)
            );
            assert_eq!(
                fast.height as u64,
                frame_extent(height as u64, divisor as u64)
            );
            assert!(fast.data == slow.data, "{width}x{height} divisor {divisor}");
        }
    }
}

// --- JPEG ---
fn test_pictures() -> Vec<(String, Rgb)> {
    let mut out = Vec::new();
    for (width, height) in [
        (1usize, 1usize),
        (2, 1),
        (1, 3),
        (8, 8),
        (15, 17),
        (16, 16),
        (17, 16),
        (16, 17),
        (33, 49),
        (100, 7),
        (7, 100),
        (333, 211),
    ] {
        out.push((
            format!("noise {width}x{height}"),
            Rgb {
                width,
                height,
                data: noise(width * height * 3, (width * 1000 + height) as u64),
            },
        ));
        out.push((
            format!("eight-colour {width}x{height}"),
            Rgb {
                width,
                height,
                data: eight_colour_noise(width, height),
            },
        ));
        let mut smooth = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            for x in 0..width {
                smooth.extend_from_slice(&[(x * 3) as u8, (y * 5) as u8, ((x + y) / 2) as u8]);
            }
        }
        out.push((
            format!("smooth {width}x{height}"),
            Rgb {
                width,
                height,
                data: smooth,
            },
        ));
    }
    for value in [0u8, 128, 255] {
        out.push((
            format!("flat {value}"),
            Rgb {
                width: 70,
                height: 50,
                data: vec![value; 70 * 50 * 3],
            },
        ));
    }
    out.push((
        "desktop 644x402".into(),
        Rgb {
            width: 644,
            height: 402,
            data: desktop(644, 402),
        },
    ));
    // Black and white only: the largest coefficients and the longest codes.
    let checker: Vec<u8> = (0..96 * 80)
        .flat_map(|i| [if (i % 96 + i / 96) % 2 == 0 { 255u8 } else { 0 }; 3])
        .collect();
    out.push((
        "checker".into(),
        Rgb {
            width: 96,
            height: 80,
            data: checker,
        },
    ));
    out
}
#[test]
fn fast_jpeg_is_byte_identical_to_the_reference_for_every_band_count() {
    for (label, image) in test_pictures() {
        for quality in [1u8, 40, 60, 80, 100] {
            let expected = reference::encode_jpeg(&image, quality);
            assert!(
                encode_jpeg(&image, quality) == expected,
                "{label} q{quality}"
            );
            for bands in 1..=7 {
                let fast =
                    encode_jpeg_bounded(image.picture(), quality, usize::MAX, bands).unwrap();
                assert!(fast == expected, "{label} q{quality} bands {bands}");
            }
        }
    }
}
#[test]
fn fast_jpeg_is_byte_identical_to_the_reference_at_display_sizes() {
    // Sizes at which production really uses several threads, including extents that are no multiple of 16.
    for (label, image) in [
        (
            "noise 1281x721",
            Rgb {
                width: 1281,
                height: 721,
                data: eight_colour_noise(1281, 721),
            },
        ),
        (
            "desktop 1366x768",
            Rgb {
                width: 1366,
                height: 768,
                data: desktop(1366, 768),
            },
        ),
    ] {
        assert!(
            workers(image.height) > 1 || std::thread::available_parallelism().unwrap().get() == 1
        );
        for quality in QUALITIES {
            assert!(
                encode_jpeg(&image, quality) == reference::encode_jpeg(&image, quality),
                "{label} q{quality}"
            );
        }
    }
}
#[test]
fn bounded_jpeg_stops_only_when_the_reference_is_larger_than_the_limit() {
    for (label, image) in test_pictures() {
        let expected = reference::encode_jpeg(&image, 80);
        for limit in [
            0,
            1,
            300,
            600,
            1000,
            4000,
            expected.len() - 1,
            expected.len(),
            usize::MAX,
        ] {
            for bands in [1, 3] {
                match encode_jpeg_bounded(image.picture(), 80, limit, bands) {
                    Some(fast) => assert!(fast == expected, "{label} limit {limit}"),
                    None => assert!(
                        expected.len() > limit,
                        "{label} limit {limit}: stopped wrongly"
                    ),
                }
            }
        }
        // At and above the real size the encode always completes.
        assert!(encode_jpeg_bounded(image.picture(), 80, expected.len(), 2).is_some());
    }
    // A picture far above its limit is abandoned.
    let image = Rgb {
        width: 333,
        height: 211,
        data: noise(333 * 211 * 3, 5),
    };
    assert!(encode_jpeg_bounded(image.picture(), 80, 2000, 2).is_none());
}

// --- the divisor / quality ladder and the whole transform ---
fn same_frame(label: &str, source: &Rgb, cap: usize) -> Option<(usize, u8)> {
    let fast = encode_control_frame(source, cap);
    let slow = reference::encode_control_frame(source, cap);
    match (fast, slow) {
        (Ok(fast), Ok(slow)) => {
            assert_eq!(
                (fast.divisor, fast.quality, fast.width, fast.height),
                (slow.divisor, slow.quality, slow.width, slow.height),
                "{label} cap {cap}"
            );
            assert!(
                fast.jpeg == slow.jpeg,
                "{label} cap {cap}: JPEG bytes differ"
            );
            Some((fast.divisor, fast.quality))
        }
        (Err(fast), Err(slow)) => {
            assert_eq!(fast.to_string(), slow.to_string());
            assert_eq!(
                FailureCode::of(&fast).map(FailureCode::name),
                Some("scaled_frame_over_cap")
            );
            None
        }
        _ => panic!("{label} cap {cap}: one implementation refused"),
    }
}
#[test]
fn ladder_choice_and_bytes_match_the_reference_at_every_cap() {
    let mut seen = std::collections::BTreeSet::new();
    for (label, image) in [
        (
            "noise 333x211",
            Rgb {
                width: 333,
                height: 211,
                data: noise(333 * 211 * 3, 21),
            },
        ),
        (
            "eight-colour 401x299",
            Rgb {
                width: 401,
                height: 299,
                data: eight_colour_noise(401, 299),
            },
        ),
        (
            "desktop 640x400",
            Rgb {
                width: 640,
                height: 400,
                data: desktop(640, 400),
            },
        ),
    ] {
        // Caps placed on, just below and between the sizes of every rung, so that each exit of the ladder (fits,
        // next quality, "far above" break, next divisor, nothing fits) is taken.
        let mut caps = vec![
            100usize, 700, 1500, 3000, 6000, 12_000, 25_000, 50_000, 100_000, 400_000,
        ];
        for divisor in [1usize, 2, 3, 4] {
            let scaled = reference::downscale(&image, divisor);
            for quality in QUALITIES {
                let size = reference::encode_jpeg(&scaled, quality).len();
                caps.extend([size - 1, size, size / 3, size / 3 + 1, size.div_ceil(3)]);
            }
        }
        for cap in caps {
            if let Some(choice) = same_frame(label, &image, cap) {
                seen.insert(choice);
            } else {
                seen.insert((0, 0));
            }
        }
    }
    // Several divisors, every quality and the refusal were all reached.
    assert!(seen.contains(&(0, 0)) && seen.len() >= 10, "{seen:?}");
    for quality in QUALITIES {
        assert!(seen.iter().any(|(_, q)| *q == quality), "{seen:?}");
    }
}
/// The original whole transform, for comparison with `control_frame_from_screenshot`.
fn reference_transform(value: &Value) -> Value {
    let png = base64::engine::general_purpose::STANDARD
        .decode(value["data_base64"].as_str().unwrap())
        .unwrap();
    let source = reference::decode_png(&png).unwrap();
    let frame = reference::encode_control_frame(&source, CONTROL_FRAME_MAX_BYTES).unwrap();
    let mut value = value.clone();
    let object = value.as_object_mut().unwrap();
    object.insert("mime_type".into(), json!("image/jpeg"));
    object.insert("byte_length".into(), json!(frame.jpeg.len()));
    object.insert("sha256".into(), json!(vm_session_auth::hash(&frame.jpeg)));
    object.insert(
        "data_base64".into(),
        json!(base64::engine::general_purpose::STANDARD.encode(&frame.jpeg)),
    );
    object.insert("frame_width".into(), json!(frame.width));
    object.insert("frame_height".into(), json!(frame.height));
    object.insert("scale_divisor".into(), json!(frame.divisor));
    object.insert("frame_quality".into(), json!(frame.quality));
    object.insert("source_mime_type".into(), json!("image/png"));
    object.insert("source_byte_length".into(), json!(png.len()));
    value
}
#[test]
fn whole_transform_reply_is_identical_to_the_reference_at_display_sizes() {
    let mut inputs: Vec<(String, usize, usize, Vec<u8>)> = Vec::new();
    // Stored PNGs need no external tool: grey noise at both display maxima and an odd-sized RGB desktop.
    for (width, height) in [(2560usize, 1440usize), (3840, 2160)] {
        let png = stored_png(width, height, 1, &noise(width * height, width as u64));
        inputs.push((
            format!("stored grey noise {width}x{height}"),
            width,
            height,
            png,
        ));
    }
    inputs.push((
        "stored desktop 1367x769".into(),
        1367,
        769,
        stored_png(1367, 769, 3, &desktop(1367, 769)),
    ));
    // The console helper's encoder family, when available.
    for (label, pixels) in [
        ("ImageIO noise 2560x1440", eight_colour_noise(2560, 1440)),
        ("ImageIO desktop 2560x1440", desktop(2560, 1440)),
    ] {
        match imageio_png(2560, 1440, &pixels) {
            Some((png, _)) => inputs.push((label.into(), 2560, 1440, png)),
            None => eprintln!("sips unavailable; {label} not checked"),
        }
    }
    for (label, width, height, png) in inputs {
        assert!(png.len() > CONTROL_FRAME_MAX_BYTES, "{label}");
        let original = screenshot(width, height, &png);
        let mut timing = FrameTiming::default();
        let fast = control_frame_from_screenshot_timed(original.clone(), &mut timing).unwrap();
        assert!(
            fast == reference_transform(&original),
            "{label}: reply differs from the reference"
        );
        verify_control_frame(&fast).unwrap();
        // The timing record describes this frame and nothing else.
        assert!(!timing.passthrough && timing.attempts >= 1);
        assert_eq!((timing.width, timing.height), (width, height));
        assert_eq!(timing.source_bytes, png.len());
        assert_eq!(
            Some(timing.frame_bytes as u64),
            fast["byte_length"].as_u64()
        );
        assert_eq!(Some(timing.divisor as u64), fast["scale_divisor"].as_u64());
        assert_eq!(Some(timing.quality as u64), fast["frame_quality"].as_u64());
    }
    // A small PNG is forwarded untouched and says so.
    let mut timing = FrameTiming::default();
    let small = screenshot(96, 64, &fixture_bytes("rgb-dynamic"));
    assert_eq!(
        control_frame_from_screenshot_timed(small.clone(), &mut timing).unwrap(),
        small
    );
    assert!(timing.passthrough && timing.attempts == 0);
}
