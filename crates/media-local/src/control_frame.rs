//! Bounded browser control frame.
//!
//! The controller screenshot is a lossless PNG at full Guest resolution (up to 8 MiB, decision 0024). The browser
//! control transport carries at most 700 KiB of image per frame. A screenshot above that bound is decoded here and
//! re-encoded as a baseline JPEG, box-downscaled by the smallest integer divisor that fits.
//!
//! Coordinate binding is unchanged: the signed `FrameAnchor` keeps the SOURCE geometry (`width`/`height` are Guest
//! pixels) and input coordinates stay in source pixels. The scaled image only adds `frame_width`, `frame_height` and
//! `scale_divisor`, which must satisfy `frame = ceil(source / divisor)` and must equal the dimensions in the JPEG
//! header. Mapping rule (shared with `browser/src/session-control.ts`):
//!   source -> frame: `floor(s / d)`;  frame -> source: `min(f * d + floor(d / 2), source - 1)`.
//! frame -> source -> frame is exact; source -> frame -> source moves at most `floor(d / 2)` source pixels.
//!
//! Everything here is safe Rust with explicit bounds; inputs come from the private controller socket.
//!
//! Speed: the PNG is inflated with a register-held bit buffer and unfiltered in place (an RGB PNG is never copied
//! into a second buffer); the downscale and the JPEG encode split their rows over at most `FRAME_WORKERS` scoped
//! threads, and a JPEG attempt stops as soon as it is certainly more than three times the cap. None of this
//! changes a byte: `control_frame_reference.rs` keeps the original straightforward codec as the test oracle, and
//! `control_frame_fast_tests.rs` requires identical decoded pixels, identical JPEG bytes for every thread count
//! and the identical divisor / quality choice. Each transform records its stage times in `FrameTiming`.
use crate::control_channel::FailureCode;
use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

/// Image bytes one control frame may carry. Unchanged transport bound.
pub const CONTROL_FRAME_MAX_BYTES: usize = 700 * 1024;
/// Base64 text bound for `CONTROL_FRAME_MAX_BYTES` (historical constant, slightly above the exact 955,736).
pub const CONTROL_FRAME_MAX_BASE64: usize = 956_000;
/// Largest source this path decodes: the 3840x2160 display maximum, in either orientation.
pub const MAX_SOURCE_PIXELS: usize = 3840 * 2160;
pub const MAX_SOURCE_EXTENT: usize = 8192;
pub const MAX_DIVISOR: u64 = 16;
const DIVISORS: [usize; 8] = [1, 2, 3, 4, 6, 8, 12, 16];
const QUALITIES: [u8; 3] = [80, 60, 40];

pub fn frame_extent(source: u64, divisor: u64) -> u64 {
    source.div_ceil(divisor)
}
/// Reference statement of the mapping rule. The server never maps: input arrives in source pixels and is checked
/// against the source geometry. The browser implements the same two functions; both sides test the same properties.
#[cfg(test)]
pub fn source_to_frame(source: u64, divisor: u64) -> u64 {
    source / divisor
}
#[cfg(test)]
pub fn frame_to_source(frame: u64, divisor: u64, source_extent: u64) -> u64 {
    (frame * divisor + divisor / 2).min(source_extent - 1)
}

/// A tightly packed RGB picture. Tests and the reference codec use it; production works on `Picture`.
#[cfg(test)]
#[derive(Debug)]
pub struct Rgb {
    pub width: usize,
    pub height: usize,
    /// `width * height * 3` bytes, row-major R, G, B.
    pub data: Vec<u8>,
}

/// A borrowed RGB picture whose rows may be embedded in a larger buffer: row `y` is the `width * 3` bytes at
/// `y * row_bytes + first`. A decoded RGB PNG is used in place this way (each scanline keeps its filter byte in
/// front), so the full-size picture is never copied.
#[derive(Clone, Copy)]
pub(crate) struct Picture<'a> {
    pub width: usize,
    pub height: usize,
    data: &'a [u8],
    row_bytes: usize,
    first: usize,
}
impl<'a> Picture<'a> {
    #[inline]
    fn row(&self, y: usize) -> &'a [u8] {
        let start = y * self.row_bytes + self.first;
        &self.data[start..start + self.width * 3]
    }
}
#[cfg(test)]
impl Rgb {
    pub(crate) fn picture(&self) -> Picture<'_> {
        Picture {
            width: self.width,
            height: self.height,
            data: &self.data,
            row_bytes: self.width * 3,
            first: 0,
        }
    }
}
/// An owned decoded picture; see `Picture` for the layout.
pub(crate) struct Decoded {
    pub width: usize,
    pub height: usize,
    data: Vec<u8>,
    row_bytes: usize,
    first: usize,
}
impl Decoded {
    pub(crate) fn picture(&self) -> Picture<'_> {
        Picture {
            width: self.width,
            height: self.height,
            data: &self.data,
            row_bytes: self.row_bytes,
            first: self.first,
        }
    }
    #[cfg(test)]
    fn into_rgb(self) -> Rgb {
        if self.first == 0 && self.row_bytes == self.width * 3 {
            return Rgb {
                width: self.width,
                height: self.height,
                data: self.data,
            };
        }
        let picture = self.picture();
        let mut data = Vec::with_capacity(self.width * self.height * 3);
        for y in 0..self.height {
            data.extend_from_slice(picture.row(y));
        }
        Rgb {
            width: self.width,
            height: self.height,
            data,
        }
    }
}
/// Worker threads one transform may use for the scale and JPEG stages (the calling thread is one of them). Fixed
/// and small: the transform runs on one blocking thread per input session, next to the media pipeline.
pub const FRAME_WORKERS: usize = 4;
/// Below this many output rows per worker a stage stays on the calling thread.
const MIN_ROWS_PER_WORKER: usize = 64;
fn workers(rows: usize) -> usize {
    let available = std::thread::available_parallelism().map_or(1, |count| count.get());
    FRAME_WORKERS
        .min(available)
        .min(rows / MIN_ROWS_PER_WORKER)
        .max(1)
}

/// Stage times and sizes of one transform. Numbers only: no pixels, no hashes, no controller text.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameTiming {
    /// The controller PNG was within the frame bound and was forwarded untouched.
    pub passthrough: bool,
    pub base64_decode_us: u64,
    /// PNG chunk walk: every chunk CRC and the IDAT concatenation.
    pub png_chunks_us: u64,
    /// DEFLATE and the Adler-32 check.
    pub inflate_us: u64,
    /// Row unfiltering, and the conversion to RGB for colour types other than RGB.
    pub unfilter_us: u64,
    /// Every box downscale that was tried.
    pub scale_us: u64,
    /// Every JPEG encode that was tried.
    pub jpeg_us: u64,
    pub hash_us: u64,
    pub base64_encode_us: u64,
    pub source_bytes: usize,
    pub raw_bytes: usize,
    pub frame_bytes: usize,
    pub width: usize,
    pub height: usize,
    /// JPEG encodes started (one per quality per divisor tried), and the pair that was kept.
    pub attempts: u32,
    pub divisor: usize,
    pub quality: u8,
    /// The picture is the console helper's own scaled JPEG (no PNG was fetched or decoded for it).
    pub helper_jpeg: bool,
    /// The helper's quality number of that JPEG (ImageIO scale); `quality` stays on the scale of `QUALITIES`.
    pub helper_quality: u8,
    /// `screenshot` requests this picture cost: every ask of the scaled form, plus the PNG request if one followed.
    pub requests: u32,
}
fn micros(started: std::time::Instant) -> u64 {
    started.elapsed().as_micros() as u64
}

// ---------------------------------------------------------------------------------------------------------------
// zlib / DEFLATE (RFC 1950, RFC 1951)
// ---------------------------------------------------------------------------------------------------------------
#[derive(Clone, Copy)]
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    /// Bits `count..` are either zero or a copy of the stream bits that follow, never anything else.
    buf: u64,
    count: u32,
}
impl<'a> Bits<'a> {
    /// At least 56 bits afterwards unless the stream ends first.
    #[inline(always)]
    fn refill(&mut self) {
        if let Some(word) = self.data.get(self.pos..self.pos + 8) {
            let word = u64::from_le_bytes([
                word[0], word[1], word[2], word[3], word[4], word[5], word[6], word[7],
            ]);
            self.buf |= word << self.count;
            self.pos += ((63 - self.count) >> 3) as usize;
            self.count |= 56;
        } else {
            while self.count <= 56 && self.pos < self.data.len() {
                self.buf |= (self.data[self.pos] as u64) << self.count;
                self.pos += 1;
                self.count += 8;
            }
        }
    }
    #[inline(always)]
    fn consume(&mut self, bits: u32) -> Result<()> {
        if bits > self.count {
            return Err(truncated());
        }
        self.buf >>= bits;
        self.count -= bits;
        Ok(())
    }
    /// At most 16 bits.
    #[inline(always)]
    fn take(&mut self, bits: u32) -> Result<u32> {
        if self.count < bits {
            self.refill();
        }
        let value = (self.buf & ((1u64 << bits) - 1)) as u32;
        self.consume(bits)?;
        Ok(value)
    }
    fn align(&mut self) {
        let drop = self.count % 8;
        self.buf >>= drop;
        self.count -= drop;
    }
    /// Byte position of the next unread bit; call only when byte-aligned.
    fn byte_position(&self) -> usize {
        self.pos - (self.count / 8) as usize
    }
}
#[cold]
fn truncated() -> anyhow::Error {
    anyhow::anyhow!("deflate stream truncated")
}
/// Root table width of the literal/length code and of the distance and code-length codes.
const LITERAL_BITS: u32 = 11;
const DISTANCE_BITS: u32 = 9;
/// Table entry: bits 0..4 code length (0 = not in the root table), bits 4..8 extra-bit count, bits 8..10 kind,
/// bits 16..32 the literal, the length base, the distance base or the code-length symbol.
const KIND_LITERAL: u32 = 0;
const KIND_COPY: u32 = 1 << 8;
const KIND_END: u32 = 2 << 8;
const KIND_INVALID: u32 = 3 << 8;
const KIND_MASK: u32 = 3 << 8;
struct Huffman {
    /// Indexed by the next root bits, LSB-first.
    fast: Vec<u32>,
    mask: usize,
    count: [u16; 16],
    /// Entries (without a length) in canonical order, for codes longer than the root.
    ordered: Vec<u32>,
}
impl Huffman {
    fn new(lengths: &[u8], root: u32, payload: impl Fn(usize) -> u32) -> Result<Self> {
        let mut count = [0u16; 16];
        for &length in lengths {
            ensure!(length <= 15, "deflate code length invalid");
            count[length as usize] += 1;
        }
        count[0] = 0;
        let mut left = 1i32;
        for used in &count[1..] {
            left = (left << 1) - *used as i32;
            ensure!(left >= 0, "deflate code over-subscribed");
        }
        let mut offset = [0u16; 16];
        for length in 1..15 {
            offset[length + 1] = offset[length] + count[length];
        }
        let mut next_code = [0u32; 16];
        let mut code = 0u32;
        for length in 1..16 {
            code = (code + count[length - 1] as u32) << 1;
            next_code[length] = code;
        }
        let mut ordered = vec![0u32; lengths.len()];
        let mut fast = vec![0u32; 1 << root];
        for (value, &length) in lengths.iter().enumerate() {
            if length == 0 {
                continue;
            }
            let entry = payload(value);
            let length = length as usize;
            ordered[offset[length] as usize] = entry;
            offset[length] += 1;
            let code = next_code[length];
            next_code[length] += 1;
            if length as u32 <= root {
                let mut index = (code.reverse_bits() >> (32 - length)) as usize;
                while index < fast.len() {
                    fast[index] = entry | length as u32;
                    index += 1 << length;
                }
            }
        }
        Ok(Self {
            mask: fast.len() - 1,
            fast,
            count,
            ordered,
        })
    }
    /// The caller has refilled. Returns the entry of the decoded symbol.
    ///
    /// `bits` is the caller's local copy and is handed to the rare bit-by-bit path by value: its address is never
    /// taken, so the compiler keeps the bit buffer in registers across the whole decode loop.
    #[inline(always)]
    fn decode(&self, bits: &mut Bits) -> Result<u32> {
        let entry = self.fast[bits.buf as usize & self.mask];
        let length = entry & 15;
        if length != 0 {
            bits.consume(length)?;
            return Ok(entry);
        }
        let (entry, rest) = self.decode_long(*bits)?;
        *bits = rest;
        Ok(entry)
    }
    /// `decode` for a length or distance symbol: also takes the symbol's extra bits and returns their value.
    /// The code and its extra bits leave the bit buffer in one step.
    #[inline(always)]
    fn decode_extra(&self, bits: &mut Bits) -> Result<(u32, usize)> {
        let entry = self.fast[bits.buf as usize & self.mask];
        let length = entry & 15;
        let extra = (entry >> 4) & 15;
        if length != 0 {
            let value = (bits.buf >> length) as usize & ((1 << extra) - 1);
            bits.consume(length + extra)?;
            return Ok((entry, value));
        }
        let (entry, rest) = self.decode_long(*bits)?;
        *bits = rest;
        let extra = (entry >> 4) & 15;
        let value = bits.take(extra)? as usize;
        Ok((entry, value))
    }
    /// Canonical bit-by-bit decode: codes longer than the root table, and every bit pattern that is no code.
    #[cold]
    #[inline(never)]
    fn decode_long<'a>(&self, mut bits: Bits<'a>) -> Result<(u32, Bits<'a>)> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for length in 1..16 {
            code |= bits.take(1)? as i32;
            let count = self.count[length] as i32;
            if code - count < first {
                return Ok((self.ordered[(index + (code - first)) as usize], bits));
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        bail!("deflate code invalid")
    }
}
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const CODE_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];
fn literal_entry(value: usize) -> u32 {
    match value {
        0..=255 => KIND_LITERAL | (value as u32) << 16,
        256 => KIND_END,
        257..=285 => {
            KIND_COPY
                | (LENGTH_BASE[value - 257] as u32) << 16
                | (LENGTH_EXTRA[value - 257] as u32) << 4
        }
        _ => KIND_INVALID,
    }
}
fn distance_entry(value: usize) -> u32 {
    match value {
        0..=29 => (DISTANCE_BASE[value] as u32) << 16 | (DISTANCE_EXTRA[value] as u32) << 4,
        _ => KIND_INVALID,
    }
}

/// Inflate a zlib stream that must produce exactly `expected` bytes; the Adler-32 trailer is checked.
pub fn zlib_inflate(data: &[u8], expected: usize) -> Result<Vec<u8>> {
    ensure!(data.len() >= 6, "zlib stream truncated");
    ensure!(
        data[0] & 15 == 8
            && data[0] >> 4 <= 7
            && data[1] & 0x20 == 0
            && (((data[0] as u32) << 8) | data[1] as u32).is_multiple_of(31),
        "zlib header invalid"
    );
    let mut bits = Bits {
        data: &data[2..],
        pos: 0,
        buf: 0,
        count: 0,
    };
    // The whole output is allocated up front and written by index; `at` is the number of bytes produced.
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(expected)
        .context("image buffer unavailable")?;
    out.resize(expected, 0);
    let mut at = 0usize;
    loop {
        let last = bits.take(1)? == 1;
        match bits.take(2)? {
            0 => {
                bits.align();
                let length = bits.take(16)? as usize;
                let inverse = bits.take(16)? as usize;
                ensure!(length == inverse ^ 0xffff, "deflate stored length invalid");
                let start = bits.byte_position();
                let end = start + length;
                ensure!(end <= bits.data.len(), "deflate stream truncated");
                ensure!(at + length <= expected, "image data exceeds geometry");
                out[at..at + length].copy_from_slice(&bits.data[start..end]);
                at += length;
                bits.pos = end;
                bits.buf = 0;
                bits.count = 0;
            }
            kind @ (1 | 2) => {
                let (literal, distance) = if kind == 1 {
                    let mut lengths = [8u8; 288];
                    lengths[144..256].fill(9);
                    lengths[256..280].fill(7);
                    (
                        Huffman::new(&lengths, LITERAL_BITS, literal_entry)?,
                        Huffman::new(&[5u8; 30], DISTANCE_BITS, distance_entry)?,
                    )
                } else {
                    let literals = bits.take(5)? as usize + 257;
                    let distances = bits.take(5)? as usize + 1;
                    let codes = bits.take(4)? as usize + 4;
                    ensure!(
                        literals <= 286 && distances <= 30,
                        "deflate table size invalid"
                    );
                    let mut order = [0u8; 19];
                    for &slot in &CODE_ORDER[..codes] {
                        order[slot] = bits.take(3)? as u8;
                    }
                    let table = Huffman::new(&order, 7, |value| (value as u32) << 16)?;
                    let mut lengths = [0u8; 286 + 30];
                    let mut index = 0;
                    while index < literals + distances {
                        bits.refill();
                        let value = table.decode(&mut bits)? >> 16;
                        let (fill, repeat) = match value {
                            0..=15 => (value as u8, 1),
                            16 => {
                                ensure!(index > 0, "deflate repeat without a previous length");
                                (lengths[index - 1], 3 + bits.take(2)? as usize)
                            }
                            17 => (0, 3 + bits.take(3)? as usize),
                            _ => (0, 11 + bits.take(7)? as usize),
                        };
                        ensure!(
                            index + repeat <= literals + distances,
                            "deflate lengths overflow"
                        );
                        lengths[index..index + repeat].fill(fill);
                        index += repeat;
                    }
                    ensure!(lengths[256] != 0, "deflate block has no end code");
                    (
                        Huffman::new(&lengths[..literals], LITERAL_BITS, literal_entry)?,
                        Huffman::new(
                            &lengths[literals..literals + distances],
                            DISTANCE_BITS,
                            distance_entry,
                        )?,
                    )
                };
                (at, bits) = inflate_block(bits, &literal, &distance, &mut out, at)?;
            }
            _ => bail!("deflate block type invalid"),
        }
        if last {
            break;
        }
    }
    ensure!(at == expected, "image data shorter than geometry");
    bits.align();
    let trailer = bits.byte_position();
    ensure!(trailer + 4 <= bits.data.len(), "zlib checksum missing");
    let stored = u32::from_be_bytes(bits.data[trailer..trailer + 4].try_into()?);
    ensure!(stored == adler32(&out), "zlib checksum mismatch");
    Ok(out)
}
/// One Huffman-coded block. `out[..at]` is the output so far; returns the new `at`. Bytes of `out` past `at` are
/// scratch: a copy may write up to `COPY_SLACK` bytes beyond its end, all of which later output overwrites.
fn inflate_block<'a>(
    mut bits: Bits<'a>,
    literal: &Huffman,
    distance: &Huffman,
    out: &mut [u8],
    mut at: usize,
) -> Result<(usize, Bits<'a>)> {
    let bits = &mut bits;
    /// Room a word-wise copy needs after its last byte.
    const COPY_SLACK: usize = 16;
    /// For a distance below 8, the smallest multiple of it that is at least 8.
    const PERIOD: [usize; 8] = [0, 8, 8, 9, 8, 10, 12, 14];
    /// Row `d`: `i % d` for a distance below 8, `i` for row 8.
    const SHUFFLE: [[u8; 8]; 9] = [
        [0; 8],
        [0; 8],
        [0, 1, 0, 1, 0, 1, 0, 1],
        [0, 1, 2, 0, 1, 2, 0, 1],
        [0, 1, 2, 3, 0, 1, 2, 3],
        [0, 1, 2, 3, 4, 0, 1, 2],
        [0, 1, 2, 3, 4, 5, 0, 1],
        [0, 1, 2, 3, 4, 5, 6, 0],
        [0, 1, 2, 3, 4, 5, 6, 7],
    ];
    let expected = out.len();
    loop {
        // One refill (56 bits unless the stream ends) covers two literals and a length with its extra bits:
        // 15 + 15 + 15 + 5 bits. Literals carry no extra bits.
        bits.refill();
        let (mut entry, mut extra) = literal.decode_extra(bits)?;
        if entry & KIND_MASK == KIND_LITERAL {
            ensure!(at < expected, "image data exceeds geometry");
            out[at] = (entry >> 16) as u8;
            at += 1;
            (entry, extra) = literal.decode_extra(bits)?;
            if entry & KIND_MASK == KIND_LITERAL {
                ensure!(at < expected, "image data exceeds geometry");
                out[at] = (entry >> 16) as u8;
                at += 1;
                (entry, extra) = literal.decode_extra(bits)?;
                if entry & KIND_MASK == KIND_LITERAL {
                    ensure!(at < expected, "image data exceeds geometry");
                    out[at] = (entry >> 16) as u8;
                    at += 1;
                    continue;
                }
            }
        }
        if entry & KIND_MASK == KIND_END {
            return Ok((at, *bits));
        }
        ensure!(
            entry & KIND_MASK == KIND_COPY,
            "deflate length code invalid"
        );
        let length = (entry >> 16) as usize + extra;
        // The distance needs at most 15 + 13 bits.
        if bits.count < 32 {
            bits.refill();
        }
        let (entry, extra) = distance.decode_extra(bits)?;
        ensure!(entry & KIND_MASK == 0, "deflate distance code invalid");
        let back = (entry >> 16) as usize + extra;
        ensure!(back <= at, "deflate distance before start");
        ensure!(at + length <= expected, "image data exceeds geometry");
        let start = at - back;
        if at + length + COPY_SLACK > expected {
            // Near the end of the output: byte by byte, which is correct for every distance.
            for offset in 0..length {
                out[at + offset] = out[start + offset];
            }
        } else {
            // The first eight bytes without a branch on the distance: byte `i` is source byte `i % back` (plain
            // `i` from a distance of eight up), read from one eight-byte load of the source.
            let pattern: [u8; 8] = out[start..start + 8].try_into()?;
            let shuffle = &SHUFFLE[back.min(8)];
            let word: [u8; 8] = std::array::from_fn(|i| pattern[shuffle[i] as usize]);
            out[at..at + 8].copy_from_slice(&word);
            if length > 8 {
                // The rest eight bytes at a time from `period` bytes back: a multiple of the distance that is
                // at least 8 (so the two words never overlap) and at most 8 more than the distance (so the
                // source is already written).
                let period = if back < 8 { PERIOD[back] } else { back };
                let mut offset = 8;
                while offset < length {
                    let from = at + offset - period;
                    let word: [u8; 8] = out[from..from + 8].try_into()?;
                    out[at + offset..at + offset + 8].copy_from_slice(&word);
                    offset += 8;
                }
            }
        }
        at += length;
    }
}
/// Adler-32. Per block of at most 5552 bytes the two sums are plain dot products, which the compiler vectorises:
/// `a += sum(x[i])`, `b += n * a_before + sum((n - i) * x[i])`.
fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u64, 0u64);
    for chunk in data.chunks(5552) {
        let count = chunk.len() as u32;
        let (mut sum, mut weighted) = (0u32, 0u32);
        for (index, &byte) in chunk.iter().enumerate() {
            sum += byte as u32;
            // At most 255 * 5552 * 5553 / 2, which is below 2^32.
            weighted += (count - index as u32) * byte as u32;
        }
        b = (b + count as u64 * a + weighted as u64) % 65521;
        a = (a + sum as u64) % 65521;
    }
    ((b as u32) << 16) | a as u32
}
/// CRC-32 (IEEE), eight bytes per step.
pub(crate) fn crc32(parts: &[&[u8]]) -> u32 {
    static TABLE: std::sync::OnceLock<Box<[[u32; 256]; 8]>> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = Box::new([[0u32; 256]; 8]);
        for index in 0..256 {
            let mut value = index as u32;
            for _ in 0..8 {
                value = if value & 1 == 1 {
                    0xedb8_8320 ^ (value >> 1)
                } else {
                    value >> 1
                };
            }
            table[0][index] = value;
        }
        for index in 0..256 {
            for slice in 1..8 {
                let previous = table[slice - 1][index];
                table[slice][index] = table[0][(previous & 255) as usize] ^ (previous >> 8);
            }
        }
        table
    });
    let mut crc = !0u32;
    for part in parts {
        let mut words = part.chunks_exact(8);
        for word in &mut words {
            let low = crc ^ u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
            crc = table[7][(low & 255) as usize]
                ^ table[6][((low >> 8) & 255) as usize]
                ^ table[5][((low >> 16) & 255) as usize]
                ^ table[4][(low >> 24) as usize]
                ^ table[3][word[4] as usize]
                ^ table[2][word[5] as usize]
                ^ table[1][word[6] as usize]
                ^ table[0][word[7] as usize];
        }
        for &byte in words.remainder() {
            crc = table[0][((crc ^ byte as u32) & 255) as usize] ^ (crc >> 8);
        }
    }
    !crc
}

// ---------------------------------------------------------------------------------------------------------------
// PNG (8-bit, non-interlaced)
// ---------------------------------------------------------------------------------------------------------------
pub const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Decode an 8-bit non-interlaced PNG to RGB. Alpha is dropped (the console frame is opaque). Every chunk CRC and
/// the zlib checksum are verified; anything else (16-bit, interlaced, sub-byte depths) is refused explicitly.
///
/// Failure codes: a well-formed PNG in a form this decoder does not handle (bit depth, interlace, display range) is
/// `png_decode_unsupported`; every other refusal is `png_decode_corrupt`.
#[cfg(test)]
pub fn decode_png(bytes: &[u8]) -> Result<Rgb> {
    decode_png_picture(bytes, &mut FrameTiming::default()).map(Decoded::into_rgb)
}
/// `decode_png` without the final copy: an RGB PNG is returned as its unfiltered scanlines, used in place.
pub(crate) fn decode_png_picture(bytes: &[u8], timing: &mut FrameTiming) -> Result<Decoded> {
    FailureCode::PngDecodeCorrupt.on(decode_png_checked(bytes, timing))
}
fn decode_png_checked(bytes: &[u8], timing: &mut FrameTiming) -> Result<Decoded> {
    let started = std::time::Instant::now();
    ensure!(bytes.starts_with(PNG_SIGNATURE), "PNG signature missing");
    let mut position = 8;
    let mut header: Option<(usize, usize, u8)> = None;
    let mut palette: &[u8] = &[];
    let mut compressed: Vec<u8> = Vec::new();
    let mut ended = false;
    while position < bytes.len() {
        ensure!(position + 12 <= bytes.len(), "PNG chunk truncated");
        let length = u32::from_be_bytes(bytes[position..position + 4].try_into()?) as usize;
        ensure!(
            length <= bytes.len() - position - 12,
            "PNG chunk exceeds the image"
        );
        let kind = &bytes[position + 4..position + 8];
        let data = &bytes[position + 8..position + 8 + length];
        let stored =
            u32::from_be_bytes(bytes[position + 8 + length..position + 12 + length].try_into()?);
        ensure!(
            stored == crc32(&[kind, data]),
            "PNG chunk checksum mismatch"
        );
        position += 12 + length;
        ensure!(
            header.is_some() || kind == b"IHDR",
            "PNG header is not the first chunk"
        );
        match kind {
            b"IHDR" => {
                ensure!(header.is_none() && length == 13, "PNG header invalid");
                let width = u32::from_be_bytes(data[0..4].try_into()?) as usize;
                let height = u32::from_be_bytes(data[4..8].try_into()?) as usize;
                FailureCode::PngDecodeUnsupported.on((|| {
                    ensure!(
                        (1..=MAX_SOURCE_EXTENT).contains(&width)
                            && (1..=MAX_SOURCE_EXTENT).contains(&height)
                            && width * height <= MAX_SOURCE_PIXELS,
                        "PNG dimensions outside the supported display range"
                    );
                    ensure!(
                        data[8] == 8
                            && matches!(data[9], 0 | 2 | 3 | 4 | 6)
                            && data[10] == 0
                            && data[11] == 0
                            && data[12] == 0,
                        "unsupported PNG encoding: need 8-bit non-interlaced"
                    );
                    Ok(())
                })())?;
                header = Some((width, height, data[9]));
            }
            b"PLTE" => {
                ensure!(
                    length.is_multiple_of(3) && (3..=768).contains(&length),
                    "PNG palette invalid"
                );
                palette = data;
            }
            b"IDAT" => {
                ensure!(
                    compressed.len() + length <= vm_control::MAX_SCREENSHOT_BYTES,
                    "PNG data exceeds the screenshot bound"
                );
                compressed.extend_from_slice(data);
            }
            b"IEND" => {
                ended = true;
                break;
            }
            _ => ensure!(kind[0] & 0x20 != 0, "unknown critical PNG chunk"),
        }
    }
    ensure!(
        ended && position == bytes.len(),
        "PNG end missing or trailing bytes"
    );
    let (width, height, color) = header.context("PNG header missing")?;
    let channels = match color {
        0 | 3 => 1,
        2 => 3,
        4 => 2,
        _ => 4,
    };
    ensure!(color != 3 || !palette.is_empty(), "PNG palette missing");
    let stride = width * channels;
    timing.png_chunks_us += micros(started);
    let started = std::time::Instant::now();
    let mut raw = zlib_inflate(&compressed, height * (stride + 1))?;
    drop(compressed);
    timing.inflate_us += micros(started);
    timing.raw_bytes = raw.len();
    let started = std::time::Instant::now();
    // Unfilter every scanline in place; the row above is read from the same buffer.
    let zero = vec![0u8; stride];
    for y in 0..height {
        let (above, rest) = raw.split_at_mut(y * (stride + 1));
        let previous = match y {
            0 => &zero[..],
            _ => &above[(y - 1) * (stride + 1) + 1..],
        };
        let (filter, line) = rest[..stride + 1]
            .split_first_mut()
            .context("PNG row missing")?;
        match channels {
            1 => unfilter::<1>(*filter, line, previous)?,
            2 => unfilter::<2>(*filter, line, previous)?,
            3 => unfilter::<3>(*filter, line, previous)?,
            _ => unfilter::<4>(*filter, line, previous)?,
        }
    }
    let decoded = if color == 2 {
        Decoded {
            width,
            height,
            data: raw,
            row_bytes: stride + 1,
            first: 1,
        }
    } else {
        let mut data: Vec<u8> = Vec::new();
        data.try_reserve_exact(width * height * 3)
            .context("image buffer unavailable")?;
        for row in raw.chunks_exact(stride + 1) {
            let line = &row[1..];
            match color {
                0 | 4 => line
                    .chunks_exact(channels)
                    .for_each(|p| data.extend_from_slice(&[p[0], p[0], p[0]])),
                3 => {
                    for &index in line.iter() {
                        let entry = palette
                            .get(index as usize * 3..index as usize * 3 + 3)
                            .context("PNG palette index out of range")?;
                        data.extend_from_slice(entry);
                    }
                }
                _ => line
                    .chunks_exact(4)
                    .for_each(|p| data.extend_from_slice(&p[..3])),
            }
        }
        Decoded {
            width,
            height,
            data,
            row_bytes: width * 3,
            first: 0,
        }
    };
    timing.unfilter_us += micros(started);
    Ok(decoded)
}
/// Reverse one PNG row filter in place. `B` is the bytes per pixel; `previous` is the unfiltered row above (zeros
/// for the first row). The left neighbour is carried in `B` registers instead of being re-read from the row.
#[inline]
fn unfilter<const B: usize>(filter: u8, line: &mut [u8], previous: &[u8]) -> Result<()> {
    match filter {
        0 => {}
        1 => {
            let mut left = [0u8; B];
            for pixel in line.chunks_exact_mut(B) {
                for k in 0..B {
                    left[k] = pixel[k].wrapping_add(left[k]);
                    pixel[k] = left[k];
                }
            }
        }
        2 => {
            for (value, up) in line.iter_mut().zip(previous) {
                *value = value.wrapping_add(*up);
            }
        }
        3 => {
            let mut left = [0u8; B];
            for (pixel, up) in line.chunks_exact_mut(B).zip(previous.chunks_exact(B)) {
                for k in 0..B {
                    left[k] = pixel[k].wrapping_add(((left[k] as u16 + up[k] as u16) / 2) as u8);
                    pixel[k] = left[k];
                }
            }
        }
        4 => {
            let (mut left, mut corner) = ([0i32; B], [0i32; B]);
            for (pixel, up) in line.chunks_exact_mut(B).zip(previous.chunks_exact(B)) {
                for k in 0..B {
                    let (a, b, c) = (left[k], up[k] as i32, corner[k]);
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    let predictor = if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    };
                    let value = pixel[k].wrapping_add(predictor as u8);
                    pixel[k] = value;
                    left[k] = value as i32;
                    corner[k] = b;
                }
            }
        }
        _ => bail!("PNG row filter invalid"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Integer box downscale
// ---------------------------------------------------------------------------------------------------------------
/// Frame pixel `(fx, fy)` is the rounded mean of source pixels `[fx*d, min((fx+1)*d, width))` by the same rows.
#[cfg(test)]
pub fn downscale(source: &Rgb, divisor: usize) -> Rgb {
    downscale_picture(source.picture(), divisor).into_rgb()
}
pub(crate) fn downscale_picture(source: Picture, divisor: usize) -> Decoded {
    let width = source.width.div_ceil(divisor);
    let height = source.height.div_ceil(divisor);
    let mut data = vec![0u8; width * height * 3];
    let count = workers(height);
    let rows_each = height.div_ceil(count);
    if count == 1 {
        downscale_rows(source, divisor, 0, &mut data);
    } else {
        let all_started = std::thread::scope(|scope| {
            let mut bands = data.chunks_mut(rows_each * width * 3).enumerate();
            let own = bands.next();
            let mut all_started = true;
            for (index, band) in bands {
                all_started &= std::thread::Builder::new()
                    .spawn_scoped(scope, move || {
                        downscale_rows(source, divisor, index * rows_each, band)
                    })
                    .is_ok();
            }
            if let Some((_, band)) = own {
                downscale_rows(source, divisor, 0, band);
            }
            all_started
        });
        // A thread that could not be started left its rows unwritten: do the whole picture here instead.
        if !all_started {
            downscale_rows(source, divisor, 0, &mut data);
        }
    }
    Decoded {
        width,
        height,
        data,
        row_bytes: width * 3,
        first: 0,
    }
}
/// `floor(x / area)` as a multiplication: exact for every `x < 2^24` and `area <= 256` (tested exhaustively).
#[inline(always)]
fn divide(x: u32, reciprocal: u64) -> u8 {
    ((x as u64 * reciprocal) >> 32) as u8
}
fn reciprocal(area: u32) -> u64 {
    (1u64 << 32).div_ceil(area as u64)
}
/// Fill `out` (whole frame rows, starting at frame row `first_row`).
fn downscale_rows(source: Picture, divisor: usize, first_row: usize, out: &mut [u8]) {
    let width = source.width.div_ceil(divisor);
    if divisor == 1 {
        for (index, target) in out.chunks_exact_mut(width * 3).enumerate() {
            target.copy_from_slice(source.row(first_row + index));
        }
        return;
    }
    // Columns that are whole blocks; at most one partial block follows.
    let whole = source.width / divisor;
    let mut sums = vec![0u32; width * 3];
    for (index, target) in out.chunks_exact_mut(width * 3).enumerate() {
        let fy = first_row + index;
        let rows = (fy * divisor)..((fy + 1) * divisor).min(source.height);
        let row_count = rows.len() as u32;
        sums.fill(0);
        for sy in rows {
            let line = source.row(sy);
            if divisor == 2 {
                for (sum, block) in sums.chunks_exact_mut(3).zip(line.chunks_exact(6)) {
                    sum[0] += block[0] as u32 + block[3] as u32;
                    sum[1] += block[1] as u32 + block[4] as u32;
                    sum[2] += block[2] as u32 + block[5] as u32;
                }
            } else {
                for (sum, block) in sums.chunks_exact_mut(3).zip(line.chunks_exact(divisor * 3)) {
                    let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
                    for pixel in block.chunks_exact(3) {
                        r += pixel[0] as u32;
                        g += pixel[1] as u32;
                        b += pixel[2] as u32;
                    }
                    sum[0] += r;
                    sum[1] += g;
                    sum[2] += b;
                }
            }
            if whole < width {
                for pixel in line[whole * divisor * 3..].chunks_exact(3) {
                    sums[whole * 3] += pixel[0] as u32;
                    sums[whole * 3 + 1] += pixel[1] as u32;
                    sums[whole * 3 + 2] += pixel[2] as u32;
                }
            }
        }
        let area = divisor as u32 * row_count;
        let (scale, half) = (reciprocal(area), area / 2);
        for (value, sum) in target[..whole * 3].iter_mut().zip(&sums) {
            *value = divide(*sum + half, scale);
        }
        if whole < width {
            let area = (source.width - whole * divisor) as u32 * row_count;
            let (scale, half) = (reciprocal(area), area / 2);
            for channel in 0..3 {
                target[whole * 3 + channel] = divide(sums[whole * 3 + channel] + half, scale);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Baseline JPEG encoder (JFIF, YCbCr 4:2:0, Annex K tables)
// ---------------------------------------------------------------------------------------------------------------
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
const LUMA_QUANT: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
const CHROMA_QUANT: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];
pub(crate) const DC_LUMA_COUNTS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
pub(crate) const DC_CHROMA_COUNTS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
pub(crate) const DC_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
pub(crate) const AC_LUMA_COUNTS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
pub(crate) const AC_LUMA_VALUES: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
pub(crate) const AC_CHROMA_COUNTS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
pub(crate) const AC_CHROMA_VALUES: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
/// `(code, length)` per symbol value, canonical JPEG assignment.
fn code_table(counts: &[u8; 16], values: &[u8]) -> [(u16, u8); 256] {
    let mut table = [(0u16, 0u8); 256];
    let (mut code, mut index) = (0u16, 0);
    for (length, &count) in counts.iter().enumerate() {
        for _ in 0..count {
            table[values[index] as usize] = (code, length as u8 + 1);
            code += 1;
            index += 1;
        }
        code <<= 1;
    }
    table
}
fn scaled_quant(base: &[u8; 64], quality: u8) -> [u8; 64] {
    let quality = quality.clamp(1, 100) as u32;
    let scale = if quality < 50 {
        5000 / quality
    } else {
        200 - quality * 2
    };
    let mut table = [0u8; 64];
    for (slot, &value) in table.iter_mut().zip(base) {
        *slot = ((value as u32 * scale + 50) / 100).clamp(1, 255) as u8;
    }
    table
}
/// Eight values handled together: one per row in the first DCT pass, one per column in the second.
type Lanes = [f32; 8];
#[inline(always)]
fn add(a: Lanes, b: Lanes) -> Lanes {
    std::array::from_fn(|i| a[i] + b[i])
}
#[inline(always)]
fn sub(a: Lanes, b: Lanes) -> Lanes {
    std::array::from_fn(|i| a[i] - b[i])
}
#[inline(always)]
fn mul(a: Lanes, factor: f32) -> Lanes {
    std::array::from_fn(|i| a[i] * factor)
}
/// One AAN pass over eight independent lanes. The arithmetic per lane is exactly the scalar pass of the reference
/// encoder (same operations in the same order), so the result is bit-identical.
#[inline(always)]
fn dct_pass(d: [Lanes; 8]) -> [Lanes; 8] {
    let (t0, t7) = (add(d[0], d[7]), sub(d[0], d[7]));
    let (t1, t6) = (add(d[1], d[6]), sub(d[1], d[6]));
    let (t2, t5) = (add(d[2], d[5]), sub(d[2], d[5]));
    let (t3, t4) = (add(d[3], d[4]), sub(d[3], d[4]));
    let (t10, t13) = (add(t0, t3), sub(t0, t3));
    let (t11, t12) = (add(t1, t2), sub(t1, t2));
    let z1 = mul(add(t12, t13), 0.707_106_77);
    let (u10, u11, u12) = (add(t4, t5), add(t5, t6), add(t6, t7));
    let z5 = mul(sub(u10, u12), 0.382_683_43);
    let z2 = add(mul(u10, 0.541_196_1), z5);
    let z4 = add(mul(u12, 1.306_563), z5);
    let z3 = mul(u11, 0.707_106_77);
    let (z11, z13) = (add(t7, z3), sub(t7, z3));
    [
        add(t10, t11),
        add(z11, z4),
        add(t13, z1),
        sub(z13, z2),
        sub(t10, t11),
        add(z13, z2),
        sub(t13, z1),
        sub(z11, z4),
    ]
}
/// AAN forward DCT (float) of the 8x8 block whose top-left sample is `plane[at]`, rows `stride` apart, then
/// quantisation. Output is in natural order. The DCT output is scaled by `8 * aan[u] * aan[v]`, folded into the
/// quantisation `divisors`.
#[inline(always)]
fn transform(plane: &[f32], at: usize, stride: usize, divisors: &[f32; 64]) -> [i32; 64] {
    // Row pass with one lane per row: `columns[x][y]` is the sample at column x of row y.
    let mut columns = [[0f32; 8]; 8];
    for y in 0..8 {
        let row: &[f32; 8] = plane[at + y * stride..at + y * stride + 8]
            .try_into()
            .unwrap();
        for x in 0..8 {
            columns[x][y] = row[x];
        }
    }
    let rows_done = dct_pass(columns);
    // Column pass with one lane per column: `rows[y][u]` is row y of the row-pass result.
    let mut rows = [[0f32; 8]; 8];
    for u in 0..8 {
        for y in 0..8 {
            rows[y][u] = rows_done[u][y];
        }
    }
    let done = dct_pass(rows);
    let mut quantised = [0i32; 64];
    for v in 0..8 {
        for u in 0..8 {
            quantised[v * 8 + u] = (done[v][u] * divisors[v * 8 + u]).round() as i32;
        }
    }
    quantised
}
/// `(code << 8) | length` per symbol value, canonical JPEG assignment.
type Codes = [u32; 256];
fn packed_codes(counts: &[u8; 16], values: &[u8]) -> Codes {
    let table = code_table(counts, values);
    std::array::from_fn(|value| (table[value].0 as u32) << 8 | table[value].1 as u32)
}
/// Entropy-coded bits, MSB first. With `STUFF` every 0xff byte is followed by 0x00 (the JPEG scan); without it the
/// bytes are the raw bit string of one band, to be appended to the scan later.
struct Writer<const STUFF: bool> {
    out: Vec<u8>,
    /// The low `count` bits are pending; bits above them are stale.
    buf: u64,
    count: u32,
}
impl<const STUFF: bool> Writer<STUFF> {
    /// At most 32 bits; `code` has no bits above `length`.
    #[inline(always)]
    fn put(&mut self, code: u32, length: u32) {
        self.buf = (self.buf << length) | code as u64;
        self.count += length;
        if self.count >= 32 {
            self.count -= 32;
            let word = (self.buf >> self.count) as u32;
            let inverted = !word;
            // True when any byte of `word` is 0xff.
            if STUFF && inverted.wrapping_sub(0x0101_0101) & !inverted & 0x8080_8080 != 0 {
                for byte in word.to_be_bytes() {
                    self.out.push(byte);
                    if byte == 0xff {
                        self.out.push(0);
                    }
                }
            } else {
                self.out.extend_from_slice(&word.to_be_bytes());
            }
        }
    }
    /// Write out the whole pending bytes; fewer than eight bits stay pending.
    fn flush_bytes(&mut self) {
        while self.count >= 8 {
            self.count -= 8;
            let byte = (self.buf >> self.count) as u8;
            self.out.push(byte);
            if STUFF && byte == 0xff {
                self.out.push(0);
            }
        }
    }
    /// One block in zigzag order.
    #[inline(always)]
    fn block(&mut self, zigzag: &[i32; 64], previous_dc: &mut i32, dc: &Codes, ac: &Codes) {
        #[inline(always)]
        fn magnitude(value: i32) -> (u32, u32) {
            let bits = 32 - value.unsigned_abs().leading_zeros();
            let raw = if value < 0 { value - 1 } else { value };
            ((raw & ((1 << bits) - 1)) as u32 & 0xffff, bits)
        }
        let difference = zigzag[0] - *previous_dc;
        *previous_dc = zigzag[0];
        let (bits, size) = magnitude(difference);
        let code = dc[size as usize & 255];
        self.put(code >> 8, code & 255);
        self.put(bits, size);
        let mut nonzero = 0u64;
        for (position, value) in zigzag.iter().enumerate().skip(1) {
            nonzero |= ((*value != 0) as u64) << position;
        }
        let mut position = 1;
        while nonzero != 0 {
            let next = nonzero.trailing_zeros() as usize;
            let mut run = next - position;
            while run >= 16 {
                self.put(ac[0xf0] >> 8, ac[0xf0] & 255);
                run -= 16;
            }
            let (bits, size) = magnitude(zigzag[next]);
            let code = ac[((run << 4) | size as usize) & 255];
            self.put(code >> 8, code & 255);
            self.put(bits, size);
            nonzero &= nonzero - 1;
            position = next + 1;
        }
        if position != 64 {
            self.put(ac[0] >> 8, ac[0] & 255);
        }
    }
}
/// The entropy-coded MCU rows `rows` of one picture, coded independently of the bands before it.
struct Band {
    /// The six blocks of the band's first MCU in zigzag order. Their DC differences depend on the band before, so
    /// they are coded when the bands are joined.
    head: [[i32; 64]; 6],
    /// Raw (unstuffed) bits of every other MCU of the band, then `tail_bits` more bits in `tail`.
    bits: Vec<u8>,
    tail: u32,
    tail_bits: u32,
    /// Quantised DC of the band's last Y, Cb and Cr blocks.
    last_dc: [i32; 3],
}
struct JpegTables {
    divisors: [[f32; 64]; 2],
    /// DC luma, AC luma, DC chroma, AC chroma.
    codes: [Codes; 4],
}
/// Code the MCU rows `rows`. Returns `None` when `produced` (the bytes all bands have written so far) passes
/// `limit`: the picture is then certainly larger than `limit`.
fn encode_band(
    image: Picture,
    tables: &JpegTables,
    rows: std::ops::Range<usize>,
    produced: &std::sync::atomic::AtomicUsize,
    limit: usize,
) -> Option<Band> {
    use std::sync::atomic::Ordering;
    let (width, height) = (image.width, image.height);
    // Planes of one MCU row, padded to whole MCUs by replicating the last column (and the last picture row).
    let padded = width.div_ceil(16) * 16;
    let mut luma = vec![0f32; padded * 16];
    let mut blue = vec![0f32; padded * 16];
    let mut red = vec![0f32; padded * 16];
    let (mut blue_half, mut red_half) = (vec![0f32; padded * 4], vec![0f32; padded * 4]);
    let mut writer = Writer::<false> {
        out: Vec::with_capacity(rows.len() * padded * 6),
        buf: 0,
        count: 0,
    };
    let mut head = [[0i32; 64]; 6];
    let mut dc = [0i32; 3];
    let mut reported = 0;
    let first_row = rows.start;
    for mcu_row in rows {
        for y in 0..16 {
            let sy = mcu_row * 16 + y;
            let (start, end) = (y * padded, (y + 1) * padded);
            if sy >= height {
                // Below the picture: the same samples as the row above.
                luma.copy_within(start - padded..start, start);
                blue.copy_within(start - padded..start, start);
                red.copy_within(start - padded..start, start);
                continue;
            }
            let line = image.row(sy);
            let (luma, blue, red) = (
                &mut luma[start..end],
                &mut blue[start..end],
                &mut red[start..end],
            );
            for (((pixel, l), cb), cr) in line
                .chunks_exact(3)
                .zip(luma.iter_mut())
                .zip(blue.iter_mut())
                .zip(red.iter_mut())
            {
                let (r, g, b) = (pixel[0] as f32, pixel[1] as f32, pixel[2] as f32);
                *l = 0.299 * r + 0.587 * g + 0.114 * b - 128.0;
                *cb = -0.168_736 * r - 0.331_264 * g + 0.5 * b;
                *cr = 0.5 * r - 0.418_688 * g - 0.081_312 * b;
            }
            let (l, cb, cr) = (luma[width - 1], blue[width - 1], red[width - 1]);
            luma[width..].fill(l);
            blue[width..].fill(cb);
            red[width..].fill(cr);
        }
        // 4:2:0: each chroma sample is the mean of a 2x2 block.
        for (half, plane) in [(&mut blue_half, &blue), (&mut red_half, &red)] {
            for y in 0..8 {
                let upper = &plane[y * 2 * padded..(y * 2 + 1) * padded];
                let lower = &plane[(y * 2 + 1) * padded..(y * 2 + 2) * padded];
                let target = &mut half[y * (padded / 2)..(y + 1) * (padded / 2)];
                for ((value, upper), lower) in target
                    .iter_mut()
                    .zip(upper.chunks_exact(2))
                    .zip(lower.chunks_exact(2))
                {
                    *value = (upper[0] + upper[1] + lower[0] + lower[1]) * 0.25;
                }
            }
        }
        for mcu in 0..padded / 16 {
            let blocks: [[i32; 64]; 6] = std::array::from_fn(|block| {
                let natural = match block {
                    0..=3 => transform(
                        &luma,
                        (block / 2 * 8) * padded + mcu * 16 + block % 2 * 8,
                        padded,
                        &tables.divisors[0],
                    ),
                    4 => transform(&blue_half, mcu * 8, padded / 2, &tables.divisors[1]),
                    _ => transform(&red_half, mcu * 8, padded / 2, &tables.divisors[1]),
                };
                std::array::from_fn(|position| natural[ZIGZAG[position]])
            });
            if mcu_row == first_row && mcu == 0 {
                head = blocks;
                dc = [blocks[3][0], blocks[4][0], blocks[5][0]];
                continue;
            }
            for block in &blocks[..4] {
                writer.block(block, &mut dc[0], &tables.codes[0], &tables.codes[1]);
            }
            writer.block(&blocks[4], &mut dc[1], &tables.codes[2], &tables.codes[3]);
            writer.block(&blocks[5], &mut dc[2], &tables.codes[2], &tables.codes[3]);
        }
        let total = produced.fetch_add(writer.out.len() - reported, Ordering::Relaxed)
            + (writer.out.len() - reported);
        reported = writer.out.len();
        if total > limit {
            return None;
        }
    }
    writer.flush_bytes();
    Some(Band {
        head,
        tail: (writer.buf & ((1 << writer.count) - 1)) as u32,
        tail_bits: writer.count,
        bits: writer.out,
        last_dc: dc,
    })
}
/// Baseline sequential JPEG, YCbCr 4:2:0, quality on the IJG 1..=100 scale.
#[cfg(test)]
pub fn encode_jpeg(image: &Rgb, quality: u8) -> Vec<u8> {
    let bands = workers(image.height.div_ceil(16) * 16);
    encode_jpeg_bounded(image.picture(), quality, usize::MAX, bands).expect("unbounded")
}
/// The JPEG of `image`, or `None` when it is certainly larger than `limit` bytes (the encode stops as soon as the
/// entropy-coded data alone passes `limit`; a returned JPEG may still be larger than `limit`). `bands` is the
/// number of threads: the MCU rows are split into that many bands, coded concurrently and joined bit-exactly, so
/// the output does not depend on `bands`.
pub(crate) fn encode_jpeg_bounded(
    image: Picture,
    quality: u8,
    limit: usize,
    bands: usize,
) -> Option<Vec<u8>> {
    const AAN: [f32; 8] = [
        1.0,
        1.387_039_8,
        1.306_563,
        1.175_875_6,
        1.0,
        0.785_694_96,
        0.541_196_1,
        0.275_899_38,
    ];
    let (width, height) = (image.width, image.height);
    let quant = [
        scaled_quant(&LUMA_QUANT, quality),
        scaled_quant(&CHROMA_QUANT, quality),
    ];
    let tables = JpegTables {
        divisors: std::array::from_fn(|table| {
            std::array::from_fn(|i| 1.0 / (quant[table][i] as f32 * AAN[i / 8] * AAN[i % 8] * 8.0))
        }),
        codes: [
            packed_codes(&DC_LUMA_COUNTS, &DC_VALUES),
            packed_codes(&AC_LUMA_COUNTS, &AC_LUMA_VALUES),
            packed_codes(&DC_CHROMA_COUNTS, &DC_VALUES),
            packed_codes(&AC_CHROMA_COUNTS, &AC_CHROMA_VALUES),
        ],
    };
    let mcu_rows = height.div_ceil(16);
    let bands = bands.clamp(1, mcu_rows);
    let range = |band: usize| band * mcu_rows / bands..(band + 1) * mcu_rows / bands;
    let produced = std::sync::atomic::AtomicUsize::new(0);
    let coded: Vec<Option<Band>> = if bands == 1 {
        vec![encode_band(image, &tables, 0..mcu_rows, &produced, limit)]
    } else {
        std::thread::scope(|scope| {
            let (tables, produced) = (&tables, &produced);
            let spawned: Vec<_> = (1..bands)
                .map(|band| {
                    std::thread::Builder::new()
                        .spawn_scoped(scope, move || {
                            encode_band(image, tables, range(band), produced, limit)
                        })
                        .ok()
                })
                .collect();
            let mut coded = vec![encode_band(image, tables, range(0), produced, limit)];
            for (index, handle) in spawned.into_iter().enumerate() {
                coded.push(match handle.map(|handle| handle.join()) {
                    Some(Ok(band)) => band,
                    Some(Err(panic)) => std::panic::resume_unwind(panic),
                    // The thread could not be started: code that band here.
                    None => encode_band(image, tables, range(index + 1), produced, limit),
                });
            }
            coded
        })
    };
    let coded: Vec<Band> = coded.into_iter().collect::<Option<_>>()?;
    let scan: usize = coded.iter().map(|band| band.bits.len()).sum();
    let mut out = Vec::with_capacity(scan + scan / 64 + 1024);
    out.extend_from_slice(&[0xff, 0xd8, 0xff, 0xe0, 0, 16]);
    out.extend_from_slice(b"JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00");
    for (id, table) in quant.iter().enumerate() {
        out.extend_from_slice(&[0xff, 0xdb, 0, 67, id as u8]);
        out.extend(ZIGZAG.iter().map(|&natural| table[natural]));
    }
    out.extend_from_slice(&[0xff, 0xc0, 0, 17, 8]);
    out.extend_from_slice(&(height as u16).to_be_bytes());
    out.extend_from_slice(&(width as u16).to_be_bytes());
    out.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    let huffman: [(u8, &[u8; 16], &[u8]); 4] = [
        (0x00, &DC_LUMA_COUNTS, &DC_VALUES),
        (0x10, &AC_LUMA_COUNTS, &AC_LUMA_VALUES),
        (0x01, &DC_CHROMA_COUNTS, &DC_VALUES),
        (0x11, &AC_CHROMA_COUNTS, &AC_CHROMA_VALUES),
    ];
    for (id, counts, values) in huffman {
        out.extend_from_slice(&[0xff, 0xc4]);
        out.extend_from_slice(&((19 + values.len()) as u16).to_be_bytes());
        out.push(id);
        out.extend_from_slice(counts);
        out.extend_from_slice(values);
    }
    out.extend_from_slice(&[0xff, 0xda, 0, 12, 3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);
    // Join the bands: the first MCU of each is coded here against the DC values of the band before it, then the
    // band's bits follow at whatever bit offset the scan has reached.
    let mut scan = Writer::<true> {
        out,
        buf: 0,
        count: 0,
    };
    let mut dc = [0i32; 3];
    for band in &coded {
        for block in &band.head[..4] {
            scan.block(block, &mut dc[0], &tables.codes[0], &tables.codes[1]);
        }
        scan.block(
            &band.head[4],
            &mut dc[1],
            &tables.codes[2],
            &tables.codes[3],
        );
        scan.block(
            &band.head[5],
            &mut dc[2],
            &tables.codes[2],
            &tables.codes[3],
        );
        let mut words = band.bits.chunks_exact(4);
        for word in &mut words {
            scan.put(u32::from_be_bytes([word[0], word[1], word[2], word[3]]), 32);
        }
        for &byte in words.remainder() {
            scan.put(byte as u32, 8);
        }
        scan.put(band.tail, band.tail_bits);
        dc = band.last_dc;
    }
    // Pad the last byte with one bits.
    let pad = (8 - scan.count % 8) % 8;
    scan.put((1 << pad) - 1, pad);
    scan.flush_bytes();
    let mut out = scan.out;
    out.extend_from_slice(&[0xff, 0xd9]);
    Some(out)
}
/// `(width, height)` from the first frame header of a JPEG that starts with SOI and ends with EOI. One
/// implementation for every hop of the screenshot path; it lives with the scaled-form checks in `vm-control`.
pub use vm_control::screenshot_scaled::jpeg_dimensions;

// ---------------------------------------------------------------------------------------------------------------
// Control frame
// ---------------------------------------------------------------------------------------------------------------
#[derive(Debug)]
pub struct ControlFrame {
    pub jpeg: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub divisor: usize,
    pub quality: u8,
}
/// Smallest integer divisor, then highest quality, whose JPEG is at most `cap` bytes.
#[cfg(test)]
pub fn encode_control_frame(source: &Rgb, cap: usize) -> Result<ControlFrame> {
    ensure!(
        source.data.len() == source.width * source.height * 3,
        "control frame source geometry invalid"
    );
    encode_control_picture(source.picture(), cap, &mut FrameTiming::default())
}
pub(crate) fn encode_control_picture(
    source: Picture,
    cap: usize,
    timing: &mut FrameTiming,
) -> Result<ControlFrame> {
    ensure!(
        source.width >= 1
            && source.height >= 1
            && source.width <= MAX_SOURCE_EXTENT
            && source.height <= MAX_SOURCE_EXTENT
            && source.width * source.height <= MAX_SOURCE_PIXELS,
        "control frame source geometry invalid"
    );
    for divisor in DIVISORS {
        let scaled;
        let image = if divisor == 1 {
            source
        } else {
            let started = std::time::Instant::now();
            scaled = downscale_picture(source, divisor);
            timing.scale_us += micros(started);
            scaled.picture()
        };
        let bands = workers(image.height.div_ceil(16) * 16);
        for quality in QUALITIES {
            let started = std::time::Instant::now();
            // An encode that passes three times the cap is abandoned: that is the "far above the cap" case below.
            let jpeg = encode_jpeg_bounded(image, quality, cap.saturating_mul(3), bands);
            timing.jpeg_us += micros(started);
            timing.attempts += 1;
            let Some(jpeg) = jpeg else { break };
            if jpeg.len() <= cap {
                return Ok(ControlFrame {
                    jpeg,
                    width: image.width,
                    height: image.height,
                    divisor,
                    quality,
                });
            }
            // The lowest quality is roughly 0.4x the highest; far above the cap means this size cannot fit.
            if jpeg.len() > cap * 3 {
                break;
            }
        }
    }
    Err(FailureCode::ScaledFrameOverCap.tag(anyhow::anyhow!(
        "control frame cannot be encoded within {cap} bytes"
    )))
}
/// Turn a controller screenshot into a browser control frame. A PNG already within the transport bound is returned
/// untouched (the pre-existing behaviour). A larger PNG becomes a JPEG with `frame_width`, `frame_height`,
/// `scale_divisor` and `sha256`; `frame_id`, `source_generation`, `width` and `height` are never altered.
///
/// Failure codes: `screenshot_reply_too_large` (above the 8 MiB screenshot contract), `png_decode_unsupported`,
/// `png_decode_corrupt`, `scaled_frame_over_cap`, and `screenshot_reply_invalid` for every other disagreement.
#[cfg(test)]
pub fn control_frame_from_screenshot(value: Value) -> Result<Value> {
    control_frame_from_screenshot_timed(value, &mut FrameTiming::default())
}
/// `control_frame_from_screenshot`, also recording where the time went.
pub fn control_frame_from_screenshot_timed(
    value: Value,
    timing: &mut FrameTiming,
) -> Result<Value> {
    FailureCode::ScreenshotReplyInvalid.on(control_frame_from_screenshot_checked(value, timing))
}
fn control_frame_from_screenshot_checked(
    mut value: Value,
    timing: &mut FrameTiming,
) -> Result<Value> {
    let declared = value["byte_length"].as_u64().unwrap_or(0) as usize;
    timing.source_bytes = declared;
    if value["mime_type"] != "image/png" || declared <= CONTROL_FRAME_MAX_BYTES {
        timing.passthrough = true;
        timing.frame_bytes = declared;
        return Ok(value);
    }
    let encoded = value["data_base64"]
        .as_str()
        .context("controller screenshot data missing")?;
    FailureCode::ScreenshotReplyTooLarge.on((|| {
        ensure!(
            declared <= vm_control::MAX_SCREENSHOT_BYTES,
            "controller screenshot exceeds the screenshot bound"
        );
        ensure!(
            encoded.len() <= vm_control::MAX_SCREENSHOT_BYTES.div_ceil(3) * 4,
            "controller screenshot exceeds the screenshot bound"
        );
        Ok(())
    })())?;
    let started = std::time::Instant::now();
    let png = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("controller screenshot base64 invalid")?;
    timing.base64_decode_us = micros(started);
    ensure!(png.len() == declared, "controller screenshot size mismatch");
    let source = decode_png_picture(&png, timing)?;
    drop(png);
    (timing.width, timing.height) = (source.width, source.height);
    ensure!(
        value["width"].as_u64() == Some(source.width as u64)
            && value["height"].as_u64() == Some(source.height as u64),
        "controller screenshot pixels disagree with its declared geometry"
    );
    let frame = encode_control_picture(source.picture(), CONTROL_FRAME_MAX_BYTES, timing)?;
    drop(source);
    (timing.divisor, timing.quality, timing.frame_bytes) =
        (frame.divisor, frame.quality, frame.jpeg.len());
    let started = std::time::Instant::now();
    let sha256 = vm_session_auth::hash(&frame.jpeg);
    timing.hash_us = micros(started);
    let started = std::time::Instant::now();
    let data_base64 = base64::engine::general_purpose::STANDARD.encode(&frame.jpeg);
    timing.base64_encode_us = micros(started);
    let object = value
        .as_object_mut()
        .context("controller screenshot is not an object")?;
    object.insert("mime_type".into(), json!("image/jpeg"));
    object.insert("byte_length".into(), json!(frame.jpeg.len()));
    object.insert("sha256".into(), json!(sha256));
    object.insert("data_base64".into(), json!(data_base64));
    object.insert("frame_width".into(), json!(frame.width));
    object.insert("frame_height".into(), json!(frame.height));
    object.insert("scale_divisor".into(), json!(frame.divisor));
    object.insert("frame_quality".into(), json!(frame.quality));
    object.insert("source_mime_type".into(), json!("image/png"));
    object.insert("source_byte_length".into(), json!(declared));
    Ok(value)
}
// ---------------------------------------------------------------------------------------------------------------
// Scaled JPEG made by the console helper (decision 0025 item 2)
// ---------------------------------------------------------------------------------------------------------------
/// `screenshot` requests of the scaled form one picture may cost. After that many `encoded_size_limit` refusals the
/// picture is made the old way, from the PNG, which tries every remaining divisor locally. Six covers the three
/// qualities at the first two divisors.
pub const HELPER_ATTEMPTS: usize = 6;
/// The helper's quality number for each entry of `QUALITIES`. The helper encodes with ImageIO, whose
/// `quality / 100` is not the IJG table scale `encode_jpeg_bounded` uses: at the same number ImageIO quantises far
/// more finely (its 80 is close to IJG 94) and writes a JPEG about 1.7 times the size of ours. Each entry here is
/// the highest ImageIO number, in steps of five, whose JPEG was no larger than ours at the paired quality on every
/// picture measured where the frame bound can matter (two stored 2560x1440 game frames, a stored 2096x1011
/// Blender frame and the synthetic desktop, at divisors 1 and 2: 0.84 to 0.94 of our size;
/// `helper_quality_mapping_sizes_and_error` prints the table). A picture that fits the bound here therefore fits
/// there at the same step, and the divisor that results is the one the transform would have chosen or smaller.
const HELPER_QUALITIES: [u8; 3] = [50, 35, 25];

/// One request of the scaled form: the divisor and quality the transform's own search is at, and what to ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelperStep {
    pub divisor: usize,
    /// On the scale of `QUALITIES`; `scaled.quality` is the helper's number for it.
    pub quality: u8,
    pub scaled: vm_control::screenshot_scaled::Scaled,
}
/// The requests to make for one picture, in the order `encode_control_picture` tries its pairs: smallest divisor
/// first, then highest quality; the next one is made only when the helper answers `encoded_size_limit`.
///
/// `source` is the Guest geometry of the last picture of this session. The helper is asked for a size bound, not a
/// divisor, and takes the smallest divisor that fits it, so divisor 1 is asked as the display maximum and needs no
/// geometry; every larger divisor is asked as exactly `ceil(source / divisor)` and is left out while the geometry
/// is unknown, when that frame would have an odd side (the helper makes none), or when it is outside what the
/// helper accepts. If the geometry has changed since, the helper still answers with a frame within the bound and
/// the reply is checked on its own terms.
pub fn helper_steps(source: Option<(u64, u64)>) -> Vec<HelperStep> {
    use vm_control::screenshot_scaled::{Scaled, MAX_HEIGHT, MAX_WIDTH};
    let mut steps = Vec::new();
    for divisor in DIVISORS {
        let (max_width, max_height) = if divisor == 1 {
            // A known screen the helper cannot send unscaled (odd side, or above its bound) is not asked for so.
            if source.is_some_and(|(width, height)| {
                width % 2 != 0 || height % 2 != 0 || width > MAX_WIDTH || height > MAX_HEIGHT
            }) {
                continue;
            }
            (MAX_WIDTH, MAX_HEIGHT)
        } else {
            let Some((width, height)) = source else { break };
            let frame = (
                frame_extent(width, divisor as u64),
                frame_extent(height, divisor as u64),
            );
            if !frame.0.is_multiple_of(2)
                || !frame.1.is_multiple_of(2)
                || !(2..=MAX_WIDTH).contains(&frame.0)
                || !(2..=MAX_HEIGHT).contains(&frame.1)
            {
                continue;
            }
            frame
        };
        for (quality, helper) in QUALITIES.into_iter().zip(HELPER_QUALITIES) {
            steps.push(HelperStep {
                divisor,
                quality,
                scaled: Scaled {
                    max_width,
                    max_height,
                    quality: helper as u64,
                },
            });
        }
    }
    steps.truncate(HELPER_ATTEMPTS);
    steps
}
/// Turn the helper's scaled JPEG into a browser control frame: the same fields the transform writes, with the
/// helper's image bytes. The reply must agree with the request and with itself (`Scaled::verify`: divisor, frame
/// size, size bound, JPEG header); `sha256` is computed here, and `frame_quality` is restated on the scale of
/// `QUALITIES` so that it means the same on both paths. `frame_id`, `source_generation`, `width` and `height` are
/// never altered. There is no PNG, so `source_mime_type` and `source_byte_length` are not written.
///
/// Failure code: `screenshot_reply_invalid` for every disagreement.
pub fn control_frame_from_helper_jpeg(
    value: Value,
    step: &HelperStep,
    timing: &mut FrameTiming,
) -> Result<Value> {
    FailureCode::ScreenshotReplyInvalid
        .on(control_frame_from_helper_jpeg_checked(value, step, timing))
}
fn control_frame_from_helper_jpeg_checked(
    mut value: Value,
    step: &HelperStep,
    timing: &mut FrameTiming,
) -> Result<Value> {
    let started = std::time::Instant::now();
    let jpeg = step.scaled.verify(&value)?;
    timing.base64_decode_us = micros(started);
    let geometry = |name: &str| value[name].as_u64().unwrap_or(0) as usize;
    let (width, height) = (geometry("width"), geometry("height"));
    ensure!(
        width <= MAX_SOURCE_EXTENT
            && height <= MAX_SOURCE_EXTENT
            && width * height <= MAX_SOURCE_PIXELS,
        "controller screenshot geometry outside the supported display range"
    );
    (timing.width, timing.height) = (width, height);
    (timing.source_bytes, timing.frame_bytes) = (jpeg.len(), jpeg.len());
    (timing.divisor, timing.quality) = (geometry("scale_divisor"), step.quality);
    (timing.helper_jpeg, timing.helper_quality) = (true, step.scaled.quality as u8);
    let started = std::time::Instant::now();
    let sha256 = vm_session_auth::hash(&jpeg);
    timing.hash_us = micros(started);
    let object = value
        .as_object_mut()
        .context("controller screenshot is not an object")?;
    object.insert("sha256".into(), json!(sha256));
    object.insert("frame_quality".into(), json!(step.quality));
    Ok(value)
}
/// Refuse a frame whose declared scale, declared frame size, real image size or hash disagree with each other or
/// with the source geometry. An unscaled PNG carries none of the scale fields.
pub fn verify_control_frame(value: &Value) -> Result<()> {
    let scale = ["frame_width", "frame_height", "scale_divisor"];
    if value["mime_type"] == "image/png" {
        ensure!(
            scale.iter().all(|field| value[*field].is_null()),
            "unscaled PNG control frame must not declare a scale"
        );
        return Ok(());
    }
    ensure!(
        value["mime_type"] == "image/jpeg",
        "control frame MIME type unsupported"
    );
    let field = |name: &str| {
        value[name]
            .as_u64()
            .context("control frame geometry missing")
    };
    let (width, height) = (field("width")?, field("height")?);
    let (frame_width, frame_height, divisor) = (
        field("frame_width")?,
        field("frame_height")?,
        field("scale_divisor")?,
    );
    ensure!(
        (1..=MAX_DIVISOR).contains(&divisor)
            && width >= 1
            && height >= 1
            && frame_width == frame_extent(width, divisor)
            && frame_height == frame_extent(height, divisor),
        "control frame scale disagrees with its source geometry"
    );
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value["data_base64"].as_str().unwrap_or(""))
        .context("control frame base64 invalid")?;
    ensure!(
        value["byte_length"].as_u64() == Some(bytes.len() as u64),
        "control frame size mismatch"
    );
    ensure!(
        jpeg_dimensions(&bytes)? == (frame_width, frame_height),
        "control frame scale disagrees with its real dimensions"
    );
    ensure!(
        value["sha256"].as_str() == Some(vm_session_auth::hash(&bytes).as_str()),
        "control frame hash mismatch"
    );
    Ok(())
}

#[cfg(test)]
#[path = "control_frame_bench_tests.rs"]
mod bench_tests;
#[cfg(test)]
#[path = "control_frame_fast_tests.rs"]
mod fast_tests;
#[cfg(test)]
#[path = "control_frame_helper_tests.rs"]
pub(crate) mod helper_tests;
/// The previous implementation, kept as the oracle for the fast path.
#[cfg(test)]
#[path = "control_frame_reference.rs"]
pub(crate) mod reference;
#[cfg(test)]
#[path = "control_frame_tests.rs"]
pub(crate) mod tests;
