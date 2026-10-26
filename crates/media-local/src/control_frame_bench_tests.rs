//! Offline time budget of one `observe_frame` picture inside the media service. Ignored by default; run with
//!   cargo test --offline --release -p vm-media-local --bin vm-media-local budget -- --ignored --nocapture
//! Inputs are 2560x1440 PNGs written by macOS ImageIO (`/usr/bin/sips`, the console helper's encoder family):
//! the live fixture's eight-colour noise screen and a synthetic "typical desktop".
use super::*;
use std::time::Instant;

/// The live fixture's eight-colour noise (same generator as `control_frame_tests.rs`), row-major R, G, B.
pub(crate) fn eight_colour_noise(width: usize, height: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9u32;
    let mut out = Vec::with_capacity(width * height * 3);
    for _ in 0..width * height {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        out.extend_from_slice(&[
            if state & 0x100 != 0 { 255 } else { 0 },
            if state & 0x1_0000 != 0 { 255 } else { 0 },
            if state & 0x100_0000 != 0 { 255 } else { 0 },
        ]);
    }
    out
}
/// A desktop-like screen: flat wallpaper, a menu bar, a white document window full of text-like glyph rows, a dark
/// terminal window with coloured text-like rows, a photo-like region (smooth gradients plus sensor-like grain)
/// and a row of flat icons.
pub(crate) fn desktop(width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; width * height * 3];
    let mut state = 0x1234_5678u32;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    let put = |out: &mut Vec<u8>, x: usize, y: usize, colour: [u8; 3]| {
        if x < width && y < height {
            out[(y * width + x) * 3..][..3].copy_from_slice(&colour);
        }
    };
    for y in 0..height {
        for x in 0..width {
            let colour = if y < height / 45 {
                [236, 236, 236]
            } else {
                [38, 79, 120]
            };
            put(&mut out, x, y, colour);
        }
    }
    // Text-like rows: 5x9 glyph cells with random strokes, word gaps, on a flat page.
    let mut text = |out: &mut Vec<u8>,
                    left: usize,
                    top: usize,
                    w: usize,
                    h: usize,
                    page: [u8; 3],
                    inks: &[[u8; 3]]| {
        for y in top..top + h {
            for x in left..left + w {
                put(out, x, y, page);
            }
        }
        let mut line = top + 12;
        while line + 12 < top + h {
            let ink = inks[(next() as usize) % inks.len()];
            let mut x = left + 16;
            let end = left + 16 + (w - 32) * (40 + (next() as usize % 60)) / 100;
            while x + 7 < end {
                if next() % 6 == 0 {
                    x += 7;
                    continue;
                }
                let glyph = next();
                for gy in 0..9 {
                    for gx in 0..5 {
                        if (glyph >> ((gy * 5 + gx) % 31)) & 1 == 1
                            && (gx + gy) % 2 == (glyph as usize) % 2
                        {
                            put(out, x + gx, line + gy, ink);
                        }
                    }
                }
                x += 7;
            }
            line += 18;
        }
    };
    let (w, h) = (width, height);
    text(
        &mut out,
        w / 20,
        h / 12,
        w * 9 / 20,
        h * 7 / 10,
        [255, 255, 255],
        &[[30, 30, 30]],
    );
    text(
        &mut out,
        w * 11 / 20,
        h / 2,
        w * 2 / 5,
        h * 2 / 5,
        [24, 24, 28],
        &[
            [220, 220, 220],
            [120, 200, 120],
            [230, 190, 90],
            [110, 170, 240],
        ],
    );
    // Photo-like region.
    let (left, top, pw, ph) = (w * 11 / 20, h / 12, w * 2 / 5, h * 7 / 20);
    for y in 0..ph {
        for x in 0..pw {
            let grain = (next() % 9) as i32 - 4;
            let fx = x as f32 / pw as f32;
            let fy = y as f32 / ph as f32;
            let r = 90.0 + 120.0 * fx + 30.0 * (fy * 9.0).sin();
            let g = 60.0 + 150.0 * fy + 25.0 * (fx * 7.0).cos();
            let b = 200.0 - 110.0 * fx * fy + 20.0 * ((fx + fy) * 11.0).sin();
            let clamp = |v: f32| (v as i32 + grain).clamp(0, 255) as u8;
            put(&mut out, left + x, top + y, [clamp(r), clamp(g), clamp(b)]);
        }
    }
    // Dock of flat icons.
    for icon in 0..12 {
        let colour = [
            (icon * 53 % 256) as u8,
            (icon * 97 % 256) as u8,
            (icon * 31 % 256) as u8,
        ];
        for y in 0..h / 18 {
            for x in 0..h / 18 {
                put(
                    &mut out,
                    w / 4 + icon * (h / 14) + x,
                    h - h / 14 + y,
                    colour,
                );
            }
        }
    }
    out
}
/// Write RGB pixels as a PNG with macOS ImageIO (through a 24-bit BMP and `/usr/bin/sips`). Returns the PNG and
/// the wall time of the `sips` process (BMP read + PNG encode + process start: an upper bound for the encode).
pub(crate) fn imageio_png(
    width: usize,
    height: usize,
    pixels: &[u8],
) -> Option<(Vec<u8>, std::time::Duration)> {
    if !std::path::Path::new("/usr/bin/sips").exists() {
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let row = (width * 3).div_ceil(4) * 4;
    let mut bmp = Vec::with_capacity(54 + row * height);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((54 + row * height) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&(width as u32).to_le_bytes());
    bmp.extend_from_slice(&(height as u32).to_le_bytes());
    bmp.extend_from_slice(&[1, 0, 24, 0]);
    bmp.extend_from_slice(&[0; 24]);
    for line in pixels.chunks(width * 3).rev() {
        for pixel in line.chunks_exact(3) {
            bmp.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
        bmp.resize(bmp.len() + row - width * 3, 0);
    }
    let (input, output) = (dir.path().join("in.bmp"), dir.path().join("out.png"));
    std::fs::write(&input, &bmp).unwrap();
    let started = Instant::now();
    let status = std::process::Command::new("/usr/bin/sips")
        .args(["-s", "format", "png"])
        .arg(&input)
        .arg("--out")
        .arg(&output)
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    assert!(status.status.success(), "sips: {status:?}");
    Some((std::fs::read(&output).unwrap(), elapsed))
}
fn idat(png: &[u8]) -> (Vec<u8>, usize) {
    let (mut position, mut out, mut count) = (8, Vec::new(), 0);
    while position + 12 <= png.len() {
        let length = u32::from_be_bytes(png[position..position + 4].try_into().unwrap()) as usize;
        if &png[position + 4..position + 8] == b"IDAT" {
            out.extend_from_slice(&png[position + 8..position + 8 + length]);
            count += 1;
        }
        position += 12 + length;
    }
    (out, count)
}
fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}
/// Best of `runs` (the machine is shared; the minimum is the least disturbed run).
fn best<T>(runs: usize, mut work: impl FnMut() -> T) -> (f64, T) {
    let mut result = None;
    let mut least = f64::MAX;
    for _ in 0..runs {
        let started = Instant::now();
        let value = work();
        least = least.min(ms(started));
        result = Some(value);
    }
    (least, result.unwrap())
}
const RUNS: usize = 5;

/// Per-stage budget of the ORIGINAL implementation (`reference`), stage by stage as `control_frame_from_screenshot`
/// and its callers ran them.
fn reference_budget(label: &str, width: usize, height: usize, png: &[u8]) -> f64 {
    let mut total = 0.0;
    let mut stage = |name: &str, value: f64, note: String| {
        total += value;
        eprintln!("budget before {label} stage={name} ms={value:.2} {note}");
    };
    let reply = {
        let mut line =
            serde_json::to_vec(&json!({"result":tests::screenshot(width, height, png)})).unwrap();
        line.push(b'\n');
        line
    };
    let (t, mut value) = best(RUNS, || serde_json::from_slice::<Value>(&reply).unwrap());
    stage("rpc_json_parse", t, format!("bytes={}", reply.len()));
    let value = value["result"].take();
    let encoded = value["data_base64"].as_str().unwrap();
    let (t, bytes) = best(RUNS, || {
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap()
    });
    stage("base64_decode", t, format!("bytes={}", bytes.len()));
    let (t, _) = best(RUNS, || {
        std::hint::black_box(reference_crc32(std::hint::black_box(&bytes)))
    });
    stage("png_chunk_crc", t, String::new());
    let (compressed, chunks) = idat(&bytes);
    let channels = match bytes[25] {
        0 | 3 => 1,
        2 => 3,
        4 => 2,
        _ => 4,
    };
    let expected = height * (width * channels + 1);
    let (t_inflate, _) = best(RUNS, || {
        reference::zlib_inflate(&compressed, expected).unwrap()
    });
    let (t_adler, _) = {
        let raw = reference::zlib_inflate(&compressed, expected).unwrap();
        best(RUNS, || {
            std::hint::black_box(reference_adler32(std::hint::black_box(&raw)))
        })
    };
    stage(
        "png_inflate",
        t_inflate - t_adler,
        format!(
            "idat_chunks={chunks} in={} out={expected}",
            compressed.len()
        ),
    );
    stage("png_adler32", t_adler, String::new());
    let (t_decode, source) = best(RUNS, || reference::decode_png(&bytes).unwrap());
    stage(
        "png_unfilter_and_rgb_copy",
        t_decode - t_inflate,
        format!("colour_type={} (decode_png total {t_decode:.2})", bytes[25]),
    );
    let mut chosen = None;
    let mut attempts = 0;
    'ladder: for divisor in [1usize, 2, 3, 4, 6, 8, 12, 16] {
        let scaled;
        let image = if divisor == 1 {
            &source
        } else {
            let (t, value) = best(RUNS, || reference::downscale(&source, divisor));
            stage(&format!("downscale_d{divisor}"), t, String::new());
            scaled = value;
            &scaled
        };
        for quality in [80u8, 60, 40] {
            let (t, jpeg) = best(RUNS, || reference::encode_jpeg(image, quality));
            attempts += 1;
            stage(
                &format!("jpeg_d{divisor}_q{quality}"),
                t,
                format!(
                    "bytes={} fits={}",
                    jpeg.len(),
                    jpeg.len() <= CONTROL_FRAME_MAX_BYTES
                ),
            );
            if jpeg.len() <= CONTROL_FRAME_MAX_BYTES {
                chosen = Some((jpeg, divisor, quality));
                break 'ladder;
            }
            if jpeg.len() > CONTROL_FRAME_MAX_BYTES * 3 {
                break;
            }
        }
    }
    let (jpeg, divisor, quality) = chosen.unwrap();
    let (t, _) = best(RUNS, || vm_session_auth::hash(&jpeg));
    stage("sha256", t, format!("bytes={}", jpeg.len()));
    let (t, text) = best(RUNS, || {
        base64::engine::general_purpose::STANDARD.encode(&jpeg)
    });
    stage("base64_encode", t, format!("bytes={}", text.len()));
    let frame = reference_frame(&value, &jpeg, divisor, quality);
    let (t, _) = best(RUNS, || verify_control_frame(&frame).unwrap());
    stage(
        "verify_control_frame",
        t,
        "(base64 decode + sha256 again)".into(),
    );
    let response = json!({"sequence":7,"result":frame});
    let (t, wire) = best(RUNS, || serde_json::to_vec(&response).unwrap());
    stage("response_json", t, format!("bytes={}", wire.len()));
    eprintln!(
        "budget before {label} total_ms={total:.2} attempts={attempts} divisor={divisor} quality={quality} jpeg_bytes={}",
        jpeg.len()
    );
    // The whole original transform in one call, as a cross-check of the sum above.
    let source_value = tests::screenshot(width, height, png);
    let (t, _) = best(3, || {
        let png = base64::engine::general_purpose::STANDARD
            .decode(source_value["data_base64"].as_str().unwrap())
            .unwrap();
        let source = reference::decode_png(&png).unwrap();
        reference::encode_control_frame(&source, CONTROL_FRAME_MAX_BYTES).unwrap()
    });
    eprintln!("budget before {label} transform_one_call_ms={t:.2}");
    t
}
/// The original bytewise checksums (the reference module shares the current ones).
fn reference_crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        let mut value = index as u32;
        for _ in 0..8 {
            value = if value & 1 == 1 {
                0xedb8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
        }
        *slot = value;
    }
    let mut crc = !0u32;
    for &byte in data {
        crc = table[((crc ^ byte as u32) & 255) as usize] ^ (crc >> 8);
    }
    !crc
}
fn reference_adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}
fn reference_frame(source: &Value, jpeg: &[u8], divisor: usize, quality: u8) -> Value {
    let mut value = source.clone();
    let (width, height) = (
        value["width"].as_u64().unwrap(),
        value["height"].as_u64().unwrap(),
    );
    let object = value.as_object_mut().unwrap();
    object.insert("mime_type".into(), json!("image/jpeg"));
    object.insert("byte_length".into(), json!(jpeg.len()));
    object.insert("sha256".into(), json!(vm_session_auth::hash(jpeg)));
    object.insert(
        "data_base64".into(),
        json!(base64::engine::general_purpose::STANDARD.encode(jpeg)),
    );
    object.insert(
        "frame_width".into(),
        json!(frame_extent(width, divisor as u64)),
    );
    object.insert(
        "frame_height".into(),
        json!(frame_extent(height, divisor as u64)),
    );
    object.insert("scale_divisor".into(), json!(divisor));
    object.insert("frame_quality".into(), json!(quality));
    value
}
/// Label, width, height, PNG bytes and the raw pixels.
pub(crate) type BenchInput = (&'static str, usize, usize, Vec<u8>, Vec<u8>);
pub(crate) fn bench_inputs() -> Vec<BenchInput> {
    let mut out = Vec::new();
    for (label, pixels) in [
        ("noise-2560x1440", eight_colour_noise(2560, 1440)),
        ("desktop-2560x1440", desktop(2560, 1440)),
    ] {
        let Some((png, elapsed)) = imageio_png(2560, 1440, &pixels) else {
            eprintln!("sips unavailable; budget not measured");
            return out;
        };
        eprintln!(
            "budget input {label} png_bytes={} sips_bmp_to_png_ms={:.0} (process start + BMP read + ImageIO PNG encode)",
            png.len(),
            elapsed.as_secs_f64() * 1000.0
        );
        out.push((label, 2560, 1440, png, pixels));
    }
    out
}
/// Per-stage budget of the current implementation, from its own `FrameTiming` (the numbers the service logs).
fn current_budget(label: &str, width: usize, height: usize, png: &[u8]) -> f64 {
    let reply = {
        let mut line =
            serde_json::to_vec(&json!({"result":tests::screenshot(width, height, png)})).unwrap();
        line.push(b'\n');
        line
    };
    let (parse, _) = best(RUNS, || serde_json::from_slice::<Value>(&reply).unwrap());
    let source = tests::screenshot(width, height, png);
    // The run with the least total is reported stage by stage.
    let mut least: Option<(f64, FrameTiming, Value)> = None;
    for _ in 0..RUNS * 2 {
        let value = source.clone();
        let mut timing = FrameTiming::default();
        let started = Instant::now();
        let frame = control_frame_from_screenshot_timed(value, &mut timing).unwrap();
        let elapsed = ms(started);
        if least.as_ref().is_none_or(|(best, _, _)| elapsed < *best) {
            least = Some((elapsed, timing, frame));
        }
    }
    let (transform, timing, frame) = least.unwrap();
    let (verify, _) = best(RUNS, || verify_control_frame(&frame).unwrap());
    let response = json!({"sequence":7,"result":frame});
    let (serialise, wire) = best(RUNS, || serde_json::to_vec(&response).unwrap());
    let us = |value: u64| value as f64 / 1000.0;
    for (name, value) in [
        ("rpc_json_parse", parse),
        ("base64_decode", us(timing.base64_decode_us)),
        ("png_chunk_crc", us(timing.png_chunks_us)),
        ("png_inflate_and_adler32", us(timing.inflate_us)),
        ("png_unfilter", us(timing.unfilter_us)),
        ("downscale_all", us(timing.scale_us)),
        ("jpeg_all_attempts", us(timing.jpeg_us)),
        ("sha256", us(timing.hash_us)),
        ("base64_encode", us(timing.base64_encode_us)),
        ("verify_control_frame", verify),
        ("response_json", serialise),
    ] {
        eprintln!("budget after {label} stage={name} ms={value:.2}");
    }
    let total = parse + transform + verify + serialise;
    eprintln!(
        "budget after {label} total_ms={total:.2} transform_one_call_ms={transform:.2} attempts={} divisor={} quality={} jpeg_bytes={} wire_bytes={} workers={}",
        timing.attempts,
        timing.divisor,
        timing.quality,
        timing.frame_bytes,
        wire.len(),
        workers(1440)
    );
    transform
}
#[test]
#[ignore = "benchmark; run in release with --ignored --nocapture"]
fn budget_of_one_1440p_observe_frame_before_and_after() {
    for (label, width, height, png, _) in bench_inputs() {
        let before = reference_budget(label, width, height, &png);
        let after = current_budget(label, width, height, &png);
        eprintln!(
            "budget summary {label} transform_before_ms={before:.1} transform_after_ms={after:.1}"
        );
    }
}
