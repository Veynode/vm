//! Offline tests of the bounded control frame: codec correctness against fixtures written by independent encoders
//! (zlib via Python, Pillow, macOS ImageIO), the byte bound at 2560x1440 and 3840x2160, and the coordinate rule.
//! No browser decoder and no Guest are involved.
use super::*;

/// Deterministic noise; incompressible for PNG and close to the worst case for JPEG.
pub(crate) fn noise(count: usize, mut state: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(count + 8);
    while out.len() < count {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(count);
    out
}
fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = (data.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
    out
}
/// Test-only PNG writer: filter 0, stored deflate blocks. `channels` 1 = grey (colour type 0), 3 = RGB (type 2).
pub(crate) fn stored_png(width: usize, height: usize, channels: usize, pixels: &[u8]) -> Vec<u8> {
    assert_eq!(pixels.len(), width * height * channels);
    let mut raw = Vec::with_capacity(pixels.len() + height);
    for row in pixels.chunks(width * channels) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut zlib = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (index, block) in blocks.iter().enumerate() {
        zlib.push((index + 1 == blocks.len()) as u8);
        zlib.extend_from_slice(&(block.len() as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut header = Vec::new();
    header.extend_from_slice(&(width as u32).to_be_bytes());
    header.extend_from_slice(&(height as u32).to_be_bytes());
    header.extend_from_slice(&[8, if channels == 1 { 0 } else { 2 }, 0, 0, 0]);
    let mut out = PNG_SIGNATURE.to_vec();
    out.extend(chunk(b"IHDR", &header));
    for part in zlib.chunks(1 << 20) {
        out.extend(chunk(b"IDAT", part));
    }
    out.extend(chunk(b"IEND", &[]));
    out
}
pub(crate) fn screenshot(width: usize, height: usize, png: &[u8]) -> Value {
    json!({"frame_id":"01234567-89ab-4def-8123-456789abcdef","source_generation":4,"width":width,"height":height,
        "mime_type":"image/png","byte_length":png.len(),
        "data_base64":base64::engine::general_purpose::STANDARD.encode(png),"capture_us":17})
}
fn formula(x: usize, y: usize) -> [u8; 3] {
    [
        (x * 7 + y * 3) as u8,
        (x * x + y * y) as u8,
        (x ^ (y * 5)) as u8,
    ]
}
macro_rules! fixture {
    ($name:literal) => {
        include_bytes!(concat!("fixtures/control_frame/", $name, ".png")).as_slice()
    };
}

#[test]
fn png_decoder_matches_independent_encoders_for_every_supported_form() {
    // Written by Python zlib: every row filter 0..=4, dynamic / fixed / Huffman-only blocks, split IDAT, ancillary
    // sRGB chunk, and colour types 0, 2, 3, 4, 6.
    for (name, bytes) in [
        ("rgb-dynamic", fixture!("rgb-dynamic")),
        ("rgb-multi-idat", fixture!("rgb-multi-idat")),
        ("rgba-fixed", fixture!("rgba-fixed")),
    ] {
        let image = decode_png(bytes).unwrap();
        assert_eq!((image.width, image.height), (96, 64), "{name}");
        for y in 0..64 {
            for x in 0..96 {
                assert_eq!(
                    image.data[(y * 96 + x) * 3..][..3],
                    formula(x, y),
                    "{name} {x},{y}"
                );
            }
        }
    }
    for (name, bytes) in [
        ("gray-dynamic", fixture!("gray-dynamic")),
        (
            "gray-alpha-huffman-only",
            fixture!("gray-alpha-huffman-only"),
        ),
        ("palette-dynamic", fixture!("palette-dynamic")),
    ] {
        let image = decode_png(bytes).unwrap();
        for y in 0..64 {
            for x in 0..96 {
                let g = formula(x, y)[1];
                let expected = if name == "palette-dynamic" {
                    [g, 255 - g, g.wrapping_mul(3)]
                } else {
                    [g, g, g]
                };
                assert_eq!(
                    image.data[(y * 96 + x) * 3..][..3],
                    expected,
                    "{name} {x},{y}"
                );
            }
        }
    }
    // A 640x400 desktop-like image written by Pillow and re-written by macOS ImageIO (the production console's
    // encoder family). The expected value is the SHA-256 of the RGB pixels as Pillow decoded both files.
    for bytes in [fixture!("desktop-pil"), fixture!("desktop-imageio")] {
        let image = decode_png(bytes).unwrap();
        assert_eq!((image.width, image.height), (640, 400));
        assert_eq!(
            vm_session_auth::hash(&image.data),
            "3563ac5c35e10d8e3b4a6611a00d36384f039cddb033d002785a3b2182e455e9"
        );
    }
}
#[test]
fn png_decoder_refuses_corruption_and_unsupported_encodings() {
    let good = fixture!("rgb-dynamic").to_vec();
    decode_png(&good).unwrap();
    for (label, mutate) in [
        (
            "signature",
            Box::new(|b: &mut Vec<u8>| b[0] = 0) as Box<dyn Fn(&mut Vec<u8>)>,
        ),
        ("header crc", Box::new(|b| b[20] ^= 1)),
        (
            "data bit",
            Box::new(|b| {
                let at = b.len() / 2;
                b[at] ^= 0x10
            }),
        ),
        ("truncated", Box::new(|b| b.truncate(b.len() - 13))),
        ("trailing", Box::new(|b| b.push(0))),
        ("no end", Box::new(|b| b.truncate(b.len() - 12))),
    ] {
        let mut changed = good.clone();
        mutate(&mut changed);
        assert!(decode_png(&changed).is_err(), "{label}");
    }
    let rebuilt = |header: [u8; 13], body: &[u8]| {
        let mut out = PNG_SIGNATURE.to_vec();
        out.extend(chunk(b"IHDR", &header));
        out.extend(chunk(b"IDAT", body));
        out.extend(chunk(b"IEND", &[]));
        out
    };
    let header = |w: u32, h: u32, depth: u8, color: u8, interlace: u8| {
        let mut v = [0u8; 13];
        v[..4].copy_from_slice(&w.to_be_bytes());
        v[4..8].copy_from_slice(&h.to_be_bytes());
        v[8] = depth;
        v[9] = color;
        v[12] = interlace;
        v
    };
    for (label, value) in [
        ("16-bit", header(4, 4, 16, 2, 0)),
        ("interlaced", header(4, 4, 8, 2, 1)),
        ("zero width", header(0, 4, 8, 2, 0)),
        ("above 4K pixel count", header(3841, 2160, 8, 2, 0)),
        ("above extent", header(8193, 1, 8, 2, 0)),
        ("bad colour type", header(4, 4, 8, 5, 0)),
    ] {
        let error = decode_png(&rebuilt(
            value,
            &[0x78, 0x01, 1, 0, 0, 0xff, 0xff, 0, 0, 0, 1],
        ))
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("unsupported") || error.contains("dimensions"),
            "{label}: {error}"
        );
    }
    // Geometry and stream length must agree exactly, both ways; the Adler-32 trailer is checked.
    let pixels = noise(8 * 8 * 3, 3);
    let exact = stored_png(8, 8, 3, &pixels);
    assert_eq!(decode_png(&exact).unwrap().data, pixels);
    let idat = exact.windows(4).position(|w| w == b"IDAT").unwrap() + 4;
    let stream = exact[idat..exact.len() - 16].to_vec();
    assert!(decode_png(&rebuilt(header(8, 9, 8, 2, 0), &stream)).is_err());
    assert!(decode_png(&rebuilt(header(8, 7, 8, 2, 0), &stream)).is_err());
    let mut wrong_adler = stream.clone();
    *wrong_adler.last_mut().unwrap() ^= 1;
    assert!(decode_png(&rebuilt(header(8, 8, 8, 2, 0), &wrong_adler))
        .unwrap_err()
        .to_string()
        .contains("checksum"));
    // A row filter outside 0..=4.
    let mut bad_filter = vec![0u8; 8 * 8 * 3 + 8];
    bad_filter[0] = 5;
    let mut zlib = vec![0x78, 0x01, 1];
    zlib.extend_from_slice(&(bad_filter.len() as u16).to_le_bytes());
    zlib.extend_from_slice(&(!(bad_filter.len() as u16)).to_le_bytes());
    zlib.extend_from_slice(&bad_filter);
    zlib.extend_from_slice(&adler32(&bad_filter).to_be_bytes());
    assert!(decode_png(&rebuilt(header(8, 8, 8, 2, 0), &zlib))
        .unwrap_err()
        .to_string()
        .contains("filter"));
}
#[test]
fn jpeg_huffman_tables_are_complete_and_prefix_free() {
    let ac: Vec<u8> = std::iter::once(0x00)
        .chain(std::iter::once(0xf0))
        .chain((0..16u8).flat_map(|run| (1..=10u8).map(move |size| (run << 4) | size)))
        .collect();
    let dc: Vec<u8> = (0..12).collect();
    for (counts, values, required) in [
        (&DC_LUMA_COUNTS, &DC_VALUES[..], &dc),
        (&DC_CHROMA_COUNTS, &DC_VALUES[..], &dc),
        (&AC_LUMA_COUNTS, &AC_LUMA_VALUES[..], &ac),
        (&AC_CHROMA_COUNTS, &AC_CHROMA_VALUES[..], &ac),
    ] {
        assert_eq!(
            counts.iter().map(|v| *v as usize).sum::<usize>(),
            values.len()
        );
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let mut wanted = required.clone();
        wanted.sort_unstable();
        assert_eq!(sorted, wanted, "every symbol exactly once");
        let table = code_table(counts, values);
        // Kraft sum below 1 (JPEG forbids the all-ones code) and no code is a prefix of another.
        let used: Vec<(u16, u8)> = required.iter().map(|v| table[*v as usize]).collect();
        let kraft: u32 = used.iter().map(|(_, length)| 1u32 << (16 - length)).sum();
        assert!(kraft < 1 << 16);
        for (i, (a, la)) in used.iter().enumerate() {
            assert!(*la >= 1 && *la <= 16 && (*a as u32) < (1u32 << la) - 1);
            for (b, lb) in &used[i + 1..] {
                let short = (*la).min(*lb);
                assert_ne!(a >> (la - short), b >> (lb - short), "prefix collision");
            }
        }
    }
}
#[test]
fn downscale_is_the_rounded_block_mean_with_partial_edge_blocks() {
    // 5x3 source, divisor 2 -> 3x2; the last column and last row are partial blocks.
    let mut data = Vec::new();
    for y in 0..3u8 {
        for x in 0..5u8 {
            data.extend_from_slice(&[x * 10 + y, 200, y]);
        }
    }
    let source = Rgb {
        width: 5,
        height: 3,
        data,
    };
    let scaled = downscale(&source, 2);
    assert_eq!((scaled.width, scaled.height), (3, 2));
    // Block (0,0): reds 0,10,1,11 -> 5.5 -> 6. Block (2,0): reds 40,41 -> 40.5 -> 41. Block (2,1): red 42.
    assert_eq!(scaled.data[0], 6);
    assert_eq!(scaled.data[2 * 3], 41);
    assert_eq!(scaled.data[(3 + 2) * 3], 42);
    assert!(scaled.data.chunks(3).all(|p| p[1] == 200));
    assert_eq!(downscale(&source, 1).data, source.data);
    for (width, height, divisor) in [
        (2560usize, 1440usize, 3usize),
        (3840, 2160, 16),
        (1279, 801, 2),
    ] {
        let source = Rgb {
            width,
            height,
            data: vec![9; width * height * 3],
        };
        let scaled = downscale(&source, divisor);
        assert_eq!(
            scaled.width as u64,
            frame_extent(width as u64, divisor as u64)
        );
        assert_eq!(
            scaled.height as u64,
            frame_extent(height as u64, divisor as u64)
        );
        assert!(scaled.data.iter().all(|v| *v == 9));
    }
}
fn check_frame(frame: &ControlFrame, width: usize, height: usize) {
    assert!(
        frame.jpeg.len() <= CONTROL_FRAME_MAX_BYTES,
        "{} bytes",
        frame.jpeg.len()
    );
    assert!(
        frame.jpeg.len().div_ceil(3) * 4 <= CONTROL_FRAME_MAX_BASE64,
        "base64 text bound"
    );
    assert_eq!(
        frame.width as u64,
        frame_extent(width as u64, frame.divisor as u64)
    );
    assert_eq!(
        frame.height as u64,
        frame_extent(height as u64, frame.divisor as u64)
    );
    assert_eq!(
        jpeg_dimensions(&frame.jpeg).unwrap(),
        (frame.width as u64, frame.height as u64)
    );
    assert!(frame.divisor as u64 <= MAX_DIVISOR);
    eprintln!(
        "{width}x{height} noise -> {}x{} divisor {} quality {} bytes {}",
        frame.width,
        frame.height,
        frame.divisor,
        frame.quality,
        frame.jpeg.len()
    );
}
#[test]
fn noisy_2560x1440_source_fits_the_control_frame_bound() {
    let source = Rgb {
        width: 2560,
        height: 1440,
        data: noise(2560 * 1440 * 3, 0x1440),
    };
    let frame = encode_control_frame(&source, CONTROL_FRAME_MAX_BYTES).unwrap();
    check_frame(&frame, 2560, 1440);
    // Noise cannot fit unscaled; the frame is genuinely scaled and still declares the exact rule.
    assert!(frame.divisor > 1);
}
#[test]
fn noisy_3840x2160_source_fits_the_control_frame_bound() {
    let source = Rgb {
        width: 3840,
        height: 2160,
        data: noise(3840 * 2160 * 3, 0x2160),
    };
    let frame = encode_control_frame(&source, CONTROL_FRAME_MAX_BYTES).unwrap();
    check_frame(&frame, 3840, 2160);
    assert!(frame.divisor > 1);
}
#[test]
fn noisy_full_size_controller_pngs_become_bound_scaled_jpeg_frames() {
    // The whole server transform on real controller-shaped replies. Grey noise keeps the lossless PNG under the
    // controller's 8 MiB bound at both sizes while staying incompressible.
    for (width, height) in [(2560usize, 1440usize), (3840, 2160)] {
        let png = stored_png(width, height, 1, &noise(width * height, width as u64));
        assert!(
            png.len() > CONTROL_FRAME_MAX_BYTES && png.len() <= vm_control::MAX_SCREENSHOT_BYTES
        );
        let original = screenshot(width, height, &png);
        let started = std::time::Instant::now();
        let frame = control_frame_from_screenshot(original.clone()).unwrap();
        eprintln!(
            "{width}x{height} PNG {} bytes -> JPEG {} bytes, {}x{}, divisor {}, quality {}, {:?}",
            png.len(),
            frame["byte_length"],
            frame["frame_width"],
            frame["frame_height"],
            frame["scale_divisor"],
            frame["frame_quality"],
            started.elapsed()
        );
        verify_control_frame(&frame).unwrap();
        assert_eq!(frame["mime_type"], "image/jpeg");
        assert!(frame["byte_length"].as_u64().unwrap() <= CONTROL_FRAME_MAX_BYTES as u64);
        assert!(frame["data_base64"].as_str().unwrap().len() <= CONTROL_FRAME_MAX_BASE64);
        // The binding fields are the source's, untouched.
        for field in [
            "frame_id",
            "source_generation",
            "width",
            "height",
            "capture_us",
        ] {
            assert_eq!(frame[field], original[field], "{field}");
        }
        let divisor = frame["scale_divisor"].as_u64().unwrap();
        assert_eq!(frame["frame_width"], frame_extent(width as u64, divisor));
        assert_eq!(frame["frame_height"], frame_extent(height as u64, divisor));
        assert_eq!(frame["source_byte_length"], png.len());
    }
}
#[test]
fn coordinate_round_trip_is_exact_from_the_frame_and_bounded_from_the_source() {
    for (width, height) in [
        (2560u64, 1440u64),
        (3840, 2160),
        (1280, 800),
        (1366, 768),
        (1, 1),
        (8192, 1),
    ] {
        for divisor in 1..=MAX_DIVISOR {
            let (frame_width, frame_height) =
                (frame_extent(width, divisor), frame_extent(height, divisor));
            let tolerance = divisor / 2;
            for (extent, frame) in [(width, frame_width), (height, frame_height)] {
                // Every frame pixel maps to a valid source pixel inside its own block and back to itself: exact.
                for f in 0..frame {
                    let s = frame_to_source(f, divisor, extent);
                    assert!(s < extent);
                    assert_eq!(source_to_frame(s, divisor), f);
                }
                // Every source pixel maps to a valid frame pixel and back within floor(divisor / 2) source pixels.
                for s in 0..extent {
                    let f = source_to_frame(s, divisor);
                    assert!(f < frame);
                    assert!(frame_to_source(f, divisor, extent).abs_diff(s) <= tolerance);
                }
            }
            // Corners and centre, stated explicitly.
            for (sx, sy) in [
                (0, 0),
                (width - 1, 0),
                (0, height - 1),
                (width - 1, height - 1),
                (width / 2, height / 2),
            ] {
                let (fx, fy) = (source_to_frame(sx, divisor), source_to_frame(sy, divisor));
                assert!(fx < frame_width && fy < frame_height);
                let (bx, by) = (
                    frame_to_source(fx, divisor, width),
                    frame_to_source(fy, divisor, height),
                );
                assert!(bx < width && by < height);
                assert!(bx.abs_diff(sx) <= tolerance && by.abs_diff(sy) <= tolerance);
                if divisor == 1 {
                    assert_eq!((bx, by), (sx, sy));
                }
            }
            assert_eq!(source_to_frame(width - 1, divisor), frame_width - 1);
            assert_eq!(source_to_frame(height - 1, divisor), frame_height - 1);
        }
    }
}
#[test]
fn frame_whose_declared_scale_disagrees_with_its_real_dimensions_is_refused() {
    let width = 2560;
    let height = 1440;
    let png = stored_png(width, height, 1, &noise(width * height, 5));
    let frame = control_frame_from_screenshot(screenshot(width, height, &png)).unwrap();
    verify_control_frame(&frame).unwrap();
    let divisor = frame["scale_divisor"].as_u64().unwrap();
    assert!(divisor > 1, "2560x1440 noise must be scaled");
    let refused = |mutate: &dyn Fn(&mut Value)| {
        let mut changed = frame.clone();
        mutate(&mut changed);
        verify_control_frame(&changed).unwrap_err().to_string()
    };
    // Declared scale or declared frame size no longer matches the source geometry.
    assert!(refused(&|v| v["scale_divisor"] = json!(divisor + 1)).contains("source geometry"));
    assert!(refused(&|v| v["scale_divisor"] = json!(1)).contains("source geometry"));
    assert!(refused(&|v| v["scale_divisor"] = json!(0)).contains("source geometry"));
    assert!(refused(&|v| v["scale_divisor"] = json!(MAX_DIVISOR + 1)).contains("source geometry"));
    assert!(
        refused(&|v| v["frame_width"] = json!(v["frame_width"].as_u64().unwrap() + 1))
            .contains("source geometry")
    );
    assert!(refused(&|v| v["width"] = json!(2561 + divisor)).contains("source geometry"));
    assert!(refused(&|v| {
        v.as_object_mut().unwrap().remove("frame_height");
    })
    .contains("missing"));
    assert!(refused(&|v| v["frame_height"] = json!("400")).contains("missing"));
    // Declared scale is self-consistent but the image really has other dimensions: a correctly hashed JPEG of a
    // different size, and the same image declared as a different source with a matching divisor.
    let other = encode_jpeg(
        &Rgb {
            width: 64,
            height: 40,
            data: vec![128; 64 * 40 * 3],
        },
        80,
    );
    let swapped = refused(&|v| {
        v["byte_length"] = json!(other.len());
        v["sha256"] = json!(vm_session_auth::hash(&other));
        v["data_base64"] = json!(base64::engine::general_purpose::STANDARD.encode(&other));
    });
    assert!(swapped.contains("real dimensions"), "{swapped}");
    let redeclared = refused(&|v| {
        v["width"] = json!(2560);
        v["height"] = json!(1600);
        v["frame_width"] = json!(frame_extent(2560, divisor));
        v["frame_height"] = json!(frame_extent(1600, divisor));
    });
    assert!(redeclared.contains("real dimensions"), "{redeclared}");
    // Bytes, length and hash are bound to each other.
    assert!(refused(&|v| v["sha256"] = json!("00".repeat(32))).contains("hash"));
    assert!(refused(&|v| {
        v.as_object_mut().unwrap().remove("sha256");
    })
    .contains("hash"));
    assert!(
        refused(&|v| v["byte_length"] = json!(v["byte_length"].as_u64().unwrap() + 1))
            .contains("size")
    );
    assert!(refused(&|v| v["mime_type"] = json!("image/webp")).contains("MIME"));
    // An unscaled PNG may not smuggle a scale.
    assert!(refused(&|v| v["mime_type"] = json!("image/png")).contains("must not declare"));
    // The transform itself refuses a PNG whose pixels are not the declared source geometry.
    for (w, h) in [(width + 1, height), (width, height - 1), (height, width)] {
        assert!(control_frame_from_screenshot(screenshot(w, h, &png))
            .unwrap_err()
            .to_string()
            .contains("declared geometry"));
    }
    let mut short = screenshot(width, height, &png);
    short["byte_length"] = json!(png.len() - 1);
    assert!(control_frame_from_screenshot(short).is_err());
    let mut huge = screenshot(width, height, &png);
    huge["byte_length"] = json!(vm_control::MAX_SCREENSHOT_BYTES + 1);
    assert!(control_frame_from_screenshot(huge).is_err());
}
#[test]
fn unscaled_small_frame_is_passed_through_byte_for_byte() {
    // Exactly at the bound and far below it: the controller reply is returned unchanged, with no scale fields,
    // and is not even decoded (the pre-existing behaviour, including for the 1-byte legacy fixture).
    let mut at_bound = PNG_SIGNATURE.to_vec();
    at_bound.resize(CONTROL_FRAME_MAX_BYTES, 0);
    let legacy = json!({"frame_id":"01234567-89ab-4def-8123-456789abcdef","source_generation":4,"width":1280,
        "height":800,"mime_type":"image/png","byte_length":1,"data_base64":"AA=="});
    for original in [
        screenshot(1280, 800, &at_bound),
        screenshot(96, 64, fixture!("rgb-dynamic")),
        legacy,
    ] {
        let frame = control_frame_from_screenshot(original.clone()).unwrap();
        assert_eq!(frame, original);
        verify_control_frame(&frame).unwrap();
        for field in ["frame_width", "frame_height", "scale_divisor", "sha256"] {
            assert!(frame[field].is_null());
        }
    }
    // One byte over the bound is transformed, never forwarded oversize.
    let mut over = PNG_SIGNATURE.to_vec();
    over.resize(CONTROL_FRAME_MAX_BYTES + 1, 0);
    assert!(control_frame_from_screenshot(screenshot(1280, 800, &over)).is_err());
    // A real, compressible image above the bound that fits unscaled as JPEG keeps divisor 1 and full dimensions.
    let (width, height) = (1280usize, 800usize);
    let mut pixels = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(&[(x / 5) as u8, (y / 4) as u8, ((x + y) / 9) as u8]);
        }
    }
    let png = stored_png(width, height, 3, &pixels);
    assert!(png.len() > CONTROL_FRAME_MAX_BYTES);
    let frame = control_frame_from_screenshot(screenshot(width, height, &png)).unwrap();
    verify_control_frame(&frame).unwrap();
    assert_eq!(frame["scale_divisor"], 1);
    assert_eq!(
        (
            frame["frame_width"].as_u64(),
            frame["frame_height"].as_u64()
        ),
        (Some(1280), Some(800))
    );
    assert_eq!(frame["frame_quality"], 80);
}
#[test]
fn jpeg_structure_is_baseline_jfif_with_exact_dimensions() {
    for (width, height) in [(1usize, 1usize), (16, 16), (17, 9), (333, 211)] {
        let image = Rgb {
            width,
            height,
            data: noise(width * height * 3, 11),
        };
        for quality in [40, 60, 80] {
            let jpeg = encode_jpeg(&image, quality);
            assert!(jpeg.starts_with(&[0xff, 0xd8, 0xff, 0xe0]) && jpeg.ends_with(&[0xff, 0xd9]));
            assert_eq!(
                jpeg_dimensions(&jpeg).unwrap(),
                (width as u64, height as u64)
            );
            // Inside the scan every 0xff is stuffed: the only markers after SOS are the final EOI.
            let scan = jpeg.windows(2).position(|w| w == [0xff, 0xda]).unwrap() + 14;
            let body = &jpeg[scan..jpeg.len() - 2];
            assert!(body.windows(2).all(|w| w[0] != 0xff || w[1] == 0));
            assert_ne!(body.last(), Some(&0xff));
        }
    }
    assert!(jpeg_dimensions(&[0xff, 0xd8, 0xff, 0xd9]).is_err());
    assert!(jpeg_dimensions(b"\x89PNG\r\n\x1a\n").is_err());
}
/// Independent decoder check: macOS ImageIO (via `/usr/bin/sips`) must decode our JPEG to the pixels we encoded.
#[cfg(target_os = "macos")]
#[test]
fn imageio_decodes_our_jpeg_to_the_source_pixels() {
    if !std::path::Path::new("/usr/bin/sips").exists() {
        eprintln!("sips unavailable; independent JPEG decode not checked");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // Smooth content so that 4:2:0 and quantisation error stay small; odd sizes exercise edge MCUs.
    for (width, height) in [(320usize, 208usize), (333, 211), (1280, 720)] {
        let mut data = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            for x in 0..width {
                data.extend_from_slice(&[
                    (x * 255 / width) as u8,
                    (y * 255 / height) as u8,
                    ((x + y) * 255 / (width + height)) as u8,
                ]);
            }
        }
        let image = Rgb {
            width,
            height,
            data,
        };
        for quality in [40u8, 80] {
            let input = dir.path().join(format!("{width}-{quality}.jpg"));
            let output = dir.path().join(format!("{width}-{quality}.bmp"));
            std::fs::write(&input, encode_jpeg(&image, quality)).unwrap();
            let status = std::process::Command::new("/usr/bin/sips")
                .args(["-s", "format", "bmp"])
                .arg(&input)
                .arg("--out")
                .arg(&output)
                .output()
                .unwrap();
            assert!(
                status.status.success(),
                "ImageIO refused our JPEG: {status:?}"
            );
            let bmp = std::fs::read(&output).unwrap();
            let le = |at: usize| u32::from_le_bytes(bmp[at..at + 4].try_into().unwrap()) as usize;
            let (offset, w, raw_height, depth) = (le(10), le(18), le(22) as i32, le(28) & 0xffff);
            let (h, top_down) = (raw_height.unsigned_abs() as usize, raw_height < 0);
            assert_eq!((w, h), (width, height), "decoded dimensions");
            assert!(depth == 24 || depth == 32);
            let stride = (w * depth / 8).div_ceil(4) * 4;
            let (mut total, mut worst) = (0u64, 0u8);
            for y in 0..h {
                let row = offset + if top_down { y } else { h - 1 - y } * stride;
                for x in 0..w {
                    let at = row + x * depth / 8;
                    let decoded = [bmp[at + 2], bmp[at + 1], bmp[at]];
                    for (c, value) in decoded.iter().enumerate() {
                        let error = value.abs_diff(image.data[(y * w + x) * 3 + c]);
                        total += error as u64;
                        worst = worst.max(error);
                    }
                }
            }
            let mean = total as f64 / (w * h * 3) as f64;
            eprintln!("ImageIO decode {width}x{height} q{quality}: mean abs error {mean:.3}, worst {worst}");
            assert!(mean < 2.0 && worst < 24, "mean {mean} worst {worst}");
        }
    }
}
/// A well-formed PNG of `total` bytes whose header declares 16-bit samples: valid, but not a form this decoder
/// handles. The padding is one ancillary chunk, so every chunk checksum is right.
pub(crate) fn unsupported_png(total: usize) -> Vec<u8> {
    let mut header = Vec::new();
    header.extend_from_slice(&2560u32.to_be_bytes());
    header.extend_from_slice(&1440u32.to_be_bytes());
    header.extend_from_slice(&[16, 2, 0, 0, 0]);
    let mut out = PNG_SIGNATURE.to_vec();
    out.extend(chunk(b"IHDR", &header));
    let fixed = out.len() + 12 + 12 + 12 + 2;
    out.extend(chunk(b"teXt", &vec![b'x'; total - fixed]));
    out.extend(chunk(b"IDAT", &[0x78, 0x01]));
    out.extend(chunk(b"IEND", &[]));
    assert_eq!(out.len(), total);
    out
}
fn code(error: &anyhow::Error) -> &'static str {
    FailureCode::of(error).map_or("untagged", FailureCode::name)
}
/// The run3 picture: the live fixture's eight-colour noise (same xorshift32 generator and bit taps as
/// `scripts/test-live-browser-direct-control.py`), row-major R, G, B.
fn eight_colour_noise(width: usize, height: usize) -> Vec<u8> {
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
fn png_dimensions(png: &[u8]) -> (usize, usize) {
    (
        u32::from_be_bytes(png[16..20].try_into().unwrap()) as usize,
        u32::from_be_bytes(png[20..24].try_into().unwrap()) as usize,
    )
}
fn idat_chunks(png: &[u8]) -> usize {
    let (mut position, mut count) = (8, 0);
    while position + 12 <= png.len() {
        let length = u32::from_be_bytes(png[position..position + 4].try_into().unwrap()) as usize;
        count += (&png[position + 4..position + 8] == b"IDAT") as usize;
        position += 12 + length;
    }
    count
}
/// The whole `observe_frame` transform on one controller-shaped reply; returns the scaled frame.
fn transform_noise_png(label: &str, png: &[u8], pixels: &[u8]) -> Value {
    let (width, height) = png_dimensions(png);
    assert!(
        png.len() > CONTROL_FRAME_MAX_BYTES && png.len() <= vm_control::MAX_SCREENSHOT_BYTES,
        "{label}: {} bytes",
        png.len()
    );
    let decoded = decode_png(png).unwrap_or_else(|e| panic!("{label}: {} ({e})", code(&e)));
    assert!(decoded.data == pixels, "{label}: decoded pixels differ");
    let started = std::time::Instant::now();
    let frame = control_frame_from_screenshot(screenshot(width, height, png))
        .unwrap_or_else(|e| panic!("{label}: {} ({e})", code(&e)));
    eprintln!(
        "{label}: PNG {} bytes in {} IDAT -> JPEG {} bytes, {}x{}, divisor {}, quality {}, {:?}",
        png.len(),
        idat_chunks(png),
        frame["byte_length"],
        frame["frame_width"],
        frame["frame_height"],
        frame["scale_divisor"],
        frame["frame_quality"],
        started.elapsed()
    );
    verify_control_frame(&frame).unwrap();
    assert_eq!(frame["mime_type"], "image/jpeg");
    assert!(frame["byte_length"].as_u64().unwrap() <= CONTROL_FRAME_MAX_BYTES as u64);
    assert!(frame["data_base64"].as_str().unwrap().len() <= CONTROL_FRAME_MAX_BASE64);
    assert_eq!(
        (frame["width"].as_u64(), frame["height"].as_u64()),
        (Some(width as u64), Some(height as u64))
    );
    frame
}
/// run3 offline: the console helper encodes screenshots with ImageIO (`CGImageDestination`, "public.png"), which
/// `/usr/bin/sips` also uses. The 2560x1440 eight-colour noise screen must come out as a bound scaled frame.
#[cfg(target_os = "macos")]
#[test]
fn imageio_png_of_the_run3_noise_screen_becomes_a_bound_scaled_frame() {
    if !std::path::Path::new("/usr/bin/sips").exists() {
        eprintln!("sips unavailable; ImageIO noise PNG not checked");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for (width, height) in [(2560usize, 1440usize), (1280, 800)] {
        let pixels = eight_colour_noise(width, height);
        // 24-bit bottom-up BMP (both widths are multiples of four, so rows need no padding).
        let mut bmp = Vec::with_capacity(54 + pixels.len());
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&((54 + pixels.len()) as u32).to_le_bytes());
        bmp.extend_from_slice(&[0; 4]);
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&(width as u32).to_le_bytes());
        bmp.extend_from_slice(&(height as u32).to_le_bytes());
        bmp.extend_from_slice(&[1, 0, 24, 0]);
        bmp.extend_from_slice(&[0; 24]);
        for row in pixels.chunks(width * 3).rev() {
            for pixel in row.chunks_exact(3) {
                bmp.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
            }
        }
        let (input, output) = (
            dir.path().join("n.bmp"),
            dir.path().join(format!("{width}.png")),
        );
        std::fs::write(&input, &bmp).unwrap();
        let status = std::process::Command::new("/usr/bin/sips")
            .args(["-s", "format", "png"])
            .arg(&input)
            .arg("--out")
            .arg(&output)
            .output()
            .unwrap();
        assert!(status.status.success(), "sips: {status:?}");
        let png = std::fs::read(&output).unwrap();
        assert_eq!(png_dimensions(&png), (width, height));
        assert!(idat_chunks(&png) > 1, "ImageIO splits IDAT");
        let frame = transform_noise_png(&format!("ImageIO {width}x{height}"), &png, &pixels);
        if width == 2560 {
            // Above the 700 KiB cap an older console helper applied to the PNG itself (the run3 refusal), far
            // below the 8 MiB contract, and scaled by exactly two.
            assert!(png.len() > 2 * 1024 * 1024, "{}", png.len());
            assert_eq!(frame["scale_divisor"], 2);
            assert_eq!(
                (
                    frame["frame_width"].as_u64(),
                    frame["frame_height"].as_u64()
                ),
                (Some(1280), Some(720))
            );
        }
    }
}
/// The same screen written by Python's zlib at levels 1, 3 and 6, as one IDAT and as 16 KiB IDATs,
/// and once with a Sub row filter. Every variant must decode to the same pixels and become a bound frame.
/// (Level 9 passed the same check once by hand; it costs about 50 s of zlib time per file on this content.
/// Level 0, stored blocks, is 11 MB here, above the 8 MiB contract; stored blocks are covered by `stored_png`.)
#[test]
fn python_zlib_pngs_of_the_run3_noise_screen_decode_at_every_level_and_idat_split() {
    let python = [
        "/usr/bin/python3",
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
    ]
    .into_iter()
    .find(|path| std::path::Path::new(path).exists());
    let Some(python) = python else {
        eprintln!("python3 unavailable; zlib noise PNGs not checked");
        return;
    };
    // Rust writes the two filtered scanline streams (filter 0 and filter 1, Sub); Python only deflates and frames.
    const SCRIPT: &str = r#"
import struct, sys, zlib
out, w, h = sys.argv[1], 2560, 1440
def chunk(kind, data): return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
def write(name, data, level, split):
    z = zlib.compress(data, level)
    parts = [z[i:i + split] for i in range(0, len(z), split)] if split else [z]
    body = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0)) + chunk(b'sRGB', b'\0')
    open(out + '/' + name + '.png', 'wb').write(body + b''.join(chunk(b'IDAT', p) for p in parts) + chunk(b'IEND', b''))
plain = open(out + '/plain.raw', 'rb').read()
for level in (1, 3, 6):
    write('level%d-single' % level, plain, level, 0)
    write('level%d-split' % level, plain, level, 16384)
write('sub-level6-split', open(out + '/sub.raw', 'rb').read(), 6, 16384)
"#;
    let dir = tempfile::tempdir().unwrap();
    let pixels = eight_colour_noise(2560, 1440);
    let (mut plain, mut sub) = (Vec::new(), Vec::new());
    for row in pixels.chunks(2560 * 3) {
        plain.push(0);
        plain.extend_from_slice(row);
        sub.push(1);
        sub.extend_from_slice(&row[..3]);
        sub.extend(
            row[3..]
                .iter()
                .zip(row)
                .map(|(value, left)| value.wrapping_sub(*left)),
        );
    }
    std::fs::write(dir.path().join("plain.raw"), &plain).unwrap();
    std::fs::write(dir.path().join("sub.raw"), &sub).unwrap();
    let status = std::process::Command::new(python)
        .args(["-I", "-c", SCRIPT])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(status.status.success(), "python: {status:?}");
    let mut names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "png"))
        .collect();
    names.sort();
    assert_eq!(names.len(), 7);
    let mut frames = Vec::new();
    for path in names {
        let png = std::fs::read(&path).unwrap();
        let label = path.file_stem().unwrap().to_string_lossy().into_owned();
        assert_eq!(idat_chunks(&png) > 1, label.ends_with("split"), "{label}");
        frames.push(transform_noise_png(&label, &png, &pixels)["sha256"].clone());
    }
    // Same pixels in, same frame out, whatever the PNG encoding was.
    assert!(frames.windows(2).all(|pair| pair[0] == pair[1]));
}
#[test]
fn every_refusal_of_the_frame_transform_names_one_fixed_sub_reason() {
    // Above the 8 MiB screenshot contract: refused from the declared size alone, before any decode.
    let mut over = screenshot(2560, 1440, b"x");
    over["byte_length"] = json!(vm_control::MAX_SCREENSHOT_BYTES + 1);
    assert_eq!(
        code(&control_frame_from_screenshot(over).unwrap_err()),
        "screenshot_reply_too_large"
    );
    let long = "A".repeat(vm_control::MAX_SCREENSHOT_BYTES.div_ceil(3) * 4 + 4);
    let mut padded = screenshot(2560, 1440, b"x");
    padded["byte_length"] = json!(vm_control::MAX_SCREENSHOT_BYTES);
    padded["data_base64"] = json!(long);
    assert_eq!(
        code(&control_frame_from_screenshot(padded).unwrap_err()),
        "screenshot_reply_too_large"
    );
    // Well-formed but not a form this decoder handles.
    let sixteen = unsupported_png(800 * 1024);
    assert_eq!(
        code(&decode_png(&sixteen).unwrap_err()),
        "png_decode_unsupported"
    );
    assert_eq!(
        code(&control_frame_from_screenshot(screenshot(2560, 1440, &sixteen)).unwrap_err()),
        "png_decode_unsupported"
    );
    // Damaged or self-contradicting.
    let good = stored_png(1024, 768, 1, &noise(1024 * 768, 5));
    assert!(good.len() > CONTROL_FRAME_MAX_BYTES);
    let mut flipped = good.clone();
    let middle = flipped.len() / 2;
    flipped[middle] ^= 1;
    assert_eq!(
        code(&control_frame_from_screenshot(screenshot(1024, 768, &flipped)).unwrap_err()),
        "png_decode_corrupt"
    );
    let mut short = screenshot(1024, 768, &good);
    short["byte_length"] = json!(good.len() + 1);
    assert_eq!(
        code(&control_frame_from_screenshot(short).unwrap_err()),
        "screenshot_reply_invalid"
    );
    assert_eq!(
        code(&control_frame_from_screenshot(screenshot(1024, 767, &good)).unwrap_err()),
        "screenshot_reply_invalid"
    );
    let mut missing = screenshot(1024, 768, &good);
    missing["data_base64"] = Value::Null;
    assert_eq!(
        code(&control_frame_from_screenshot(missing).unwrap_err()),
        "screenshot_reply_invalid"
    );
    // A picture that cannot be brought under the cap at the largest divisor.
    let source = Rgb {
        width: 1024,
        height: 768,
        data: noise(1024 * 768 * 3, 3),
    };
    let error = encode_control_frame(&source, 100).unwrap_err();
    assert_eq!(code(&error), "scaled_frame_over_cap");
    // The message stays available to callers; the code is the only thing a log line prints.
    assert!(error.to_string().contains("100 bytes"));
}
