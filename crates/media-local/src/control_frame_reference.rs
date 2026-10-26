//! The original, straightforward control-frame codec, kept verbatim as the test oracle for the fast path in
//! `control_frame.rs`: whole-stream inflate, whole-image unfilter, a separate downscale pass and one full JPEG
//! encode per attempt. Test-only; production never calls it.
use super::*;

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    count: u32,
}
impl<'a> Bits<'a> {
    fn refill(&mut self) {
        while self.count <= 56 && self.pos < self.data.len() {
            self.buf |= (self.data[self.pos] as u64) << self.count;
            self.pos += 1;
            self.count += 8;
        }
    }
    fn consume(&mut self, bits: u32) -> Result<()> {
        ensure!(bits <= self.count, "deflate stream truncated");
        self.buf >>= bits;
        self.count -= bits;
        Ok(())
    }
    /// At most 16 bits.
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
const FAST_BITS: u32 = 10;
struct Huffman {
    /// `(symbol << 4) | length` for codes of at most `FAST_BITS` bits, indexed by the next bits LSB-first. 0 = slow.
    fast: Vec<u16>,
    count: [u16; 16],
    symbol: Vec<u16>,
}
impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self> {
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
        let mut symbol = vec![0u16; lengths.len()];
        let mut fast = vec![0u16; 1 << FAST_BITS];
        for (value, &length) in lengths.iter().enumerate() {
            if length == 0 {
                continue;
            }
            let length = length as usize;
            symbol[offset[length] as usize] = value as u16;
            offset[length] += 1;
            let code = next_code[length];
            next_code[length] += 1;
            if length as u32 <= FAST_BITS {
                let reversed = (code.reverse_bits() >> (32 - length)) as usize;
                let mut index = reversed;
                while index < fast.len() {
                    fast[index] = ((value as u16) << 4) | length as u16;
                    index += 1 << length;
                }
            }
        }
        Ok(Self {
            fast,
            count,
            symbol,
        })
    }
    fn decode(&self, bits: &mut Bits) -> Result<u16> {
        bits.refill();
        let entry = self.fast[(bits.buf & ((1 << FAST_BITS) - 1)) as usize];
        if entry != 0 {
            bits.consume((entry & 15) as u32)?;
            return Ok(entry >> 4);
        }
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for length in 1..16 {
            code |= bits.take(1)? as i32;
            let count = self.count[length] as i32;
            if code - count < first {
                return Ok(self.symbol[(index + (code - first)) as usize]);
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
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(expected)
        .context("image buffer unavailable")?;
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
                ensure!(
                    out.len() + length <= expected,
                    "image data exceeds geometry"
                );
                out.extend_from_slice(&bits.data[start..end]);
                bits.pos = end;
                bits.buf = 0;
                bits.count = 0;
            }
            kind @ (1 | 2) => {
                let (literal, distance) = if kind == 1 {
                    let mut lengths = [8u8; 288];
                    lengths[144..256].fill(9);
                    lengths[256..280].fill(7);
                    (Huffman::new(&lengths)?, Huffman::new(&[5u8; 30])?)
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
                    let table = Huffman::new(&order)?;
                    let mut lengths = [0u8; 286 + 30];
                    let mut index = 0;
                    while index < literals + distances {
                        let value = table.decode(&mut bits)?;
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
                        Huffman::new(&lengths[..literals])?,
                        Huffman::new(&lengths[literals..literals + distances])?,
                    )
                };
                loop {
                    let value = literal.decode(&mut bits)? as usize;
                    if value < 256 {
                        ensure!(out.len() < expected, "image data exceeds geometry");
                        out.push(value as u8);
                        continue;
                    }
                    if value == 256 {
                        break;
                    }
                    ensure!(value <= 285, "deflate length code invalid");
                    let length = LENGTH_BASE[value - 257] as usize
                        + bits.take(LENGTH_EXTRA[value - 257] as u32)? as usize;
                    let code = distance.decode(&mut bits)? as usize;
                    ensure!(code < 30, "deflate distance code invalid");
                    let back = DISTANCE_BASE[code] as usize
                        + bits.take(DISTANCE_EXTRA[code] as u32)? as usize;
                    ensure!(back <= out.len(), "deflate distance before start");
                    ensure!(
                        out.len() + length <= expected,
                        "image data exceeds geometry"
                    );
                    let start = out.len() - back;
                    if back >= length {
                        out.extend_from_within(start..start + length);
                    } else {
                        for offset in 0..length {
                            out.push(out[start + offset]);
                        }
                    }
                }
            }
            _ => bail!("deflate block type invalid"),
        }
        if last {
            break;
        }
    }
    ensure!(out.len() == expected, "image data shorter than geometry");
    bits.align();
    let trailer = bits.byte_position();
    ensure!(trailer + 4 <= bits.data.len(), "zlib checksum missing");
    let stored = u32::from_be_bytes(bits.data[trailer..trailer + 4].try_into()?);
    ensure!(stored == adler32(&out), "zlib checksum mismatch");
    Ok(out)
}
/// Decode an 8-bit non-interlaced PNG to RGB. Alpha is dropped (the console frame is opaque). Every chunk CRC and
/// the zlib checksum are verified; anything else (16-bit, interlaced, sub-byte depths) is refused explicitly.
///
/// Failure codes: a well-formed PNG in a form this decoder does not handle (bit depth, interlace, display range) is
/// `png_decode_unsupported`; every other refusal is `png_decode_corrupt`.
pub fn decode_png(bytes: &[u8]) -> Result<Rgb> {
    FailureCode::PngDecodeCorrupt.on(decode_png_checked(bytes))
}
fn decode_png_checked(bytes: &[u8]) -> Result<Rgb> {
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
    let mut raw = zlib_inflate(&compressed, height * (stride + 1))?;
    drop(compressed);
    let mut data: Vec<u8> = Vec::new();
    data.try_reserve_exact(width * height * 3)
        .context("image buffer unavailable")?;
    let mut previous = vec![0u8; stride];
    for row in raw.chunks_exact_mut(stride + 1) {
        let (filter, line) = row.split_first_mut().context("PNG row missing")?;
        match *filter {
            0 => {}
            1 => {
                for i in channels..stride {
                    line[i] = line[i].wrapping_add(line[i - channels]);
                }
            }
            2 => {
                for i in 0..stride {
                    line[i] = line[i].wrapping_add(previous[i]);
                }
            }
            3 => {
                for i in 0..stride {
                    let left = if i >= channels { line[i - channels] } else { 0 };
                    line[i] = line[i].wrapping_add(((left as u16 + previous[i] as u16) / 2) as u8);
                }
            }
            4 => {
                for i in 0..stride {
                    let (a, c) = if i >= channels {
                        (line[i - channels] as i32, previous[i - channels] as i32)
                    } else {
                        (0, 0)
                    };
                    let b = previous[i] as i32;
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    let predictor = if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    };
                    line[i] = line[i].wrapping_add(predictor as u8);
                }
            }
            _ => bail!("PNG row filter invalid"),
        }
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
            2 => data.extend_from_slice(line),
            _ => line
                .chunks_exact(4)
                .for_each(|p| data.extend_from_slice(&p[..3])),
        }
        previous.copy_from_slice(line);
    }
    Ok(Rgb {
        width,
        height,
        data,
    })
}
// ---------------------------------------------------------------------------------------------------------------
// Integer box downscale
// ---------------------------------------------------------------------------------------------------------------
/// Frame pixel `(fx, fy)` is the rounded mean of source pixels `[fx*d, min((fx+1)*d, width))` by the same rows.
pub fn downscale(source: &Rgb, divisor: usize) -> Rgb {
    let width = source.width.div_ceil(divisor);
    let height = source.height.div_ceil(divisor);
    let mut data = Vec::with_capacity(width * height * 3);
    let mut sums = vec![0u32; width * 3];
    for fy in 0..height {
        sums.fill(0);
        let rows = (fy * divisor)..((fy + 1) * divisor).min(source.height);
        let row_count = rows.len() as u32;
        for sy in rows {
            let line = &source.data[sy * source.width * 3..(sy + 1) * source.width * 3];
            for (fx, block) in line.chunks(divisor * 3).enumerate() {
                for pixel in block.chunks_exact(3) {
                    sums[fx * 3] += pixel[0] as u32;
                    sums[fx * 3 + 1] += pixel[1] as u32;
                    sums[fx * 3 + 2] += pixel[2] as u32;
                }
            }
        }
        for fx in 0..width {
            let columns = ((fx + 1) * divisor).min(source.width) - fx * divisor;
            let area = columns as u32 * row_count;
            for channel in 0..3 {
                data.push(((sums[fx * 3 + channel] + area / 2) / area) as u8);
            }
        }
    }
    Rgb {
        width,
        height,
        data,
    }
}
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
/// AAN forward DCT (float). Output is scaled by `8 * aan[u] * aan[v]`, folded into the quantisation divisors.
fn forward_dct(block: &mut [f32; 64]) {
    fn pass(d: [f32; 8]) -> [f32; 8] {
        let (t0, t7) = (d[0] + d[7], d[0] - d[7]);
        let (t1, t6) = (d[1] + d[6], d[1] - d[6]);
        let (t2, t5) = (d[2] + d[5], d[2] - d[5]);
        let (t3, t4) = (d[3] + d[4], d[3] - d[4]);
        let (t10, t13) = (t0 + t3, t0 - t3);
        let (t11, t12) = (t1 + t2, t1 - t2);
        let z1 = (t12 + t13) * 0.707_106_77;
        let (u10, u11, u12) = (t4 + t5, t5 + t6, t6 + t7);
        let z5 = (u10 - u12) * 0.382_683_43;
        let z2 = 0.541_196_1 * u10 + z5;
        let z4 = 1.306_563 * u12 + z5;
        let z3 = u11 * 0.707_106_77;
        let (z11, z13) = (t7 + z3, t7 - z3);
        [
            t10 + t11,
            z11 + z4,
            t13 + z1,
            z13 - z2,
            t10 - t11,
            z13 + z2,
            t13 - z1,
            z11 - z4,
        ]
    }
    for row in 0..8 {
        let out = pass(block[row * 8..row * 8 + 8].try_into().unwrap());
        block[row * 8..row * 8 + 8].copy_from_slice(&out);
    }
    for column in 0..8 {
        let out = pass(std::array::from_fn(|row| block[row * 8 + column]));
        for row in 0..8 {
            block[row * 8 + column] = out[row];
        }
    }
}
struct Entropy {
    out: Vec<u8>,
    buf: u64,
    count: u32,
}
impl Entropy {
    fn put(&mut self, code: u16, length: u8) {
        self.buf = (self.buf << length) | code as u64;
        self.count += length as u32;
        while self.count >= 8 {
            let byte = (self.buf >> (self.count - 8)) as u8;
            self.out.push(byte);
            if byte == 0xff {
                self.out.push(0);
            }
            self.count -= 8;
        }
    }
    fn block(
        &mut self,
        samples: &mut [f32; 64],
        divisors: &[f32; 64],
        previous_dc: &mut i32,
        dc: &[(u16, u8); 256],
        ac: &[(u16, u8); 256],
    ) {
        forward_dct(samples);
        let mut quantised = [0i32; 64];
        for (position, &natural) in ZIGZAG.iter().enumerate() {
            quantised[position] = (samples[natural] * divisors[natural]).round() as i32;
        }
        let magnitude = |value: i32| -> (u16, u8) {
            let bits = 32 - value.unsigned_abs().leading_zeros();
            let raw = if value < 0 { value - 1 } else { value };
            ((raw & ((1 << bits) - 1)) as u16, bits as u8)
        };
        let difference = quantised[0] - *previous_dc;
        *previous_dc = quantised[0];
        let (bits, size) = magnitude(difference);
        let (code, length) = dc[size as usize];
        self.put(code, length);
        self.put(bits, size);
        let last = quantised.iter().rposition(|v| *v != 0).unwrap_or(0);
        let mut run = 0;
        for &value in &quantised[1..=last] {
            if value == 0 {
                run += 1;
                continue;
            }
            while run >= 16 {
                let (code, length) = ac[0xf0];
                self.put(code, length);
                run -= 16;
            }
            let (bits, size) = magnitude(value);
            let (code, length) = ac[(run << 4) | size as usize];
            self.put(code, length);
            self.put(bits, size);
            run = 0;
        }
        if last != 63 {
            let (code, length) = ac[0];
            self.put(code, length);
        }
    }
}
/// Baseline sequential JPEG, YCbCr 4:2:0, quality on the IJG 1..=100 scale.
pub fn encode_jpeg(image: &Rgb, quality: u8) -> Vec<u8> {
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
    let divisors: [[f32; 64]; 2] = std::array::from_fn(|table| {
        std::array::from_fn(|i| 1.0 / (quant[table][i] as f32 * AAN[i / 8] * AAN[i % 8] * 8.0))
    });
    let mut out = Vec::with_capacity(width * height / 4 + 1024);
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
    let tables: [(u8, &[u8; 16], &[u8]); 4] = [
        (0x00, &DC_LUMA_COUNTS, &DC_VALUES),
        (0x10, &AC_LUMA_COUNTS, &AC_LUMA_VALUES),
        (0x01, &DC_CHROMA_COUNTS, &DC_VALUES),
        (0x11, &AC_CHROMA_COUNTS, &AC_CHROMA_VALUES),
    ];
    for (id, counts, values) in tables {
        out.extend_from_slice(&[0xff, 0xc4]);
        out.extend_from_slice(&((19 + values.len()) as u16).to_be_bytes());
        out.push(id);
        out.extend_from_slice(counts);
        out.extend_from_slice(values);
    }
    out.extend_from_slice(&[0xff, 0xda, 0, 12, 3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);
    let codes = [
        code_table(&DC_LUMA_COUNTS, &DC_VALUES),
        code_table(&AC_LUMA_COUNTS, &AC_LUMA_VALUES),
        code_table(&DC_CHROMA_COUNTS, &DC_VALUES),
        code_table(&AC_CHROMA_COUNTS, &AC_CHROMA_VALUES),
    ];
    let mut entropy = Entropy {
        out,
        buf: 0,
        count: 0,
    };
    let mut dc = [0i32; 3];
    let mut luma = [0f32; 256];
    let mut blue = [0f32; 256];
    let mut red = [0f32; 256];
    let mut block = [0f32; 64];
    for top in (0..height).step_by(16) {
        for left in (0..width).step_by(16) {
            // Edge MCUs replicate the last row/column.
            for y in 0..16 {
                let row = (top + y).min(height - 1) * width;
                for x in 0..16 {
                    let at = (row + (left + x).min(width - 1)) * 3;
                    let (r, g, b) = (
                        image.data[at] as f32,
                        image.data[at + 1] as f32,
                        image.data[at + 2] as f32,
                    );
                    luma[y * 16 + x] = 0.299 * r + 0.587 * g + 0.114 * b - 128.0;
                    blue[y * 16 + x] = -0.168_736 * r - 0.331_264 * g + 0.5 * b;
                    red[y * 16 + x] = 0.5 * r - 0.418_688 * g - 0.081_312 * b;
                }
            }
            for quadrant in 0..4 {
                let (oy, ox) = (quadrant / 2 * 8, quadrant % 2 * 8);
                for y in 0..8 {
                    for x in 0..8 {
                        block[y * 8 + x] = luma[(oy + y) * 16 + ox + x];
                    }
                }
                entropy.block(&mut block, &divisors[0], &mut dc[0], &codes[0], &codes[1]);
            }
            for (component, plane) in [(1, &blue), (2, &red)] {
                for y in 0..8 {
                    for x in 0..8 {
                        let at = y * 32 + x * 2;
                        block[y * 8 + x] =
                            (plane[at] + plane[at + 1] + plane[at + 16] + plane[at + 17]) * 0.25;
                    }
                }
                entropy.block(
                    &mut block,
                    &divisors[1],
                    &mut dc[component],
                    &codes[2],
                    &codes[3],
                );
            }
        }
    }
    if entropy.count > 0 {
        let pad = 8 - entropy.count;
        entropy.put((1u16 << pad) - 1, pad as u8);
    }
    let mut out = entropy.out;
    out.extend_from_slice(&[0xff, 0xd9]);
    out
}
/// Smallest integer divisor, then highest quality, whose JPEG is at most `cap` bytes.
pub fn encode_control_frame(source: &Rgb, cap: usize) -> Result<ControlFrame> {
    ensure!(
        source.width >= 1
            && source.height >= 1
            && source.width <= MAX_SOURCE_EXTENT
            && source.height <= MAX_SOURCE_EXTENT
            && source.width * source.height <= MAX_SOURCE_PIXELS
            && source.data.len() == source.width * source.height * 3,
        "control frame source geometry invalid"
    );
    for divisor in DIVISORS {
        let scaled;
        let image = if divisor == 1 {
            source
        } else {
            scaled = downscale(source, divisor);
            &scaled
        };
        for quality in QUALITIES {
            let jpeg = encode_jpeg(image, quality);
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
