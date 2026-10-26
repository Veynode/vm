//! The control picture as the console helper's own scaled JPEG (decision 0025 item 2): which requests are made, what
//! a reply must satisfy to become a control frame, and how the helper's ImageIO quality number relates to ours.
//! The helper itself needs a SPICE server; ImageIO is reached here through `/usr/bin/sips`, as elsewhere in these
//! tests. No Guest is involved.
use super::*;
use vm_control::screenshot_scaled::Scaled;

/// A helper reply of the scaled form for `source`, made the way the helper makes it (block mean, then a JPEG) but
/// with this crate's encoder. Daemon-added fields included, no `sha256`.
pub(crate) fn helper_reply(source: &Rgb, divisor: usize, step: &HelperStep) -> Value {
    reply_with(source, divisor, step.quality, step.scaled.quality)
}
/// What a helper behind a correct controller answers to `asked`: the divisor its own rule picks for `source`.
pub(crate) fn scripted_reply(source: &Rgb, asked: &Scaled) -> Value {
    let divisor = asked
        .divisor(source.width as u64, source.height as u64)
        .expect("a divisor fits");
    reply_with(source, divisor as usize, 80, asked.quality)
}
fn reply_with(source: &Rgb, divisor: usize, encode_quality: u8, helper_quality: u64) -> Value {
    let scaled = reference::downscale(source, divisor);
    let jpeg = encode_jpeg(&scaled, encode_quality);
    json!({"frame_id":"01234567-89ab-4def-8123-456789abcdef","source_generation":4,"source_sequence":9,
        "width":source.width,"height":source.height,"coordinate_space":"guest-physical-pixels","display_id":0,
        "guest_ui_scale":null,"captured_at_us":1,"captured_monotonic_us":2,
        "mime_type":"image/jpeg","byte_length":jpeg.len(),
        "data_base64":base64::engine::general_purpose::STANDARD.encode(&jpeg),
        "frame_width":scaled.width,"frame_height":scaled.height,"scale_divisor":divisor,
        "frame_quality":helper_quality})
}
pub(crate) fn smooth(width: usize, height: usize) -> Rgb {
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
    Rgb {
        width,
        height,
        data,
    }
}

#[test]
fn requests_follow_the_transforms_own_order_and_stay_within_what_the_helper_accepts() {
    // Geometry unknown: only divisor 1 can be asked for, as the display maximum.
    let first = helper_steps(None);
    assert_eq!(first.len(), 3);
    for (step, quality) in first.iter().zip(QUALITIES) {
        assert_eq!((step.divisor, step.quality), (1, quality));
        assert_eq!(
            (step.scaled.max_width, step.scaled.max_height),
            (3840, 2160)
        );
    }
    // Known geometry: the pairs of `encode_control_picture`, smallest divisor first, then highest quality.
    let steps = helper_steps(Some((2560, 1440)));
    assert_eq!(steps.len(), HELPER_ATTEMPTS);
    assert_eq!(&steps[..3], &first[..]);
    let pairs: Vec<_> = steps
        .iter()
        .map(|step| (step.divisor, step.quality))
        .collect();
    assert_eq!(
        pairs,
        [(1, 80), (1, 60), (1, 40), (2, 80), (2, 60), (2, 40)]
    );
    for step in &steps[3..] {
        assert_eq!((step.scaled.max_width, step.scaled.max_height), (1280, 720));
    }
    // Each request is one the helper accepts, and the helper's own rule picks the intended divisor for it.
    for source in [
        (2560u64, 1440u64),
        (3840, 2160),
        (1920, 1080),
        (1280, 800),
        (1366, 768),
        (640, 480),
    ] {
        for step in helper_steps(Some(source)) {
            let mut request = json!({"id":"vm"});
            step.scaled.apply(&mut request);
            assert_eq!(Scaled::parse(&request).unwrap(), Some(step.scaled));
            assert_eq!(
                step.scaled.divisor(source.0, source.1),
                Some(step.divisor as u64),
                "{source:?} {step:?}"
            );
        }
    }
    // 1366x768 halves to an odd width, which the helper never makes: the next divisor asked for is 3.
    let odd: Vec<_> = helper_steps(Some((1366, 768)))
        .iter()
        .map(|step| step.divisor)
        .collect();
    assert_eq!(odd, [1, 1, 1, 3, 3, 3]);
    // A known screen with an odd side, or above the helper's bound, is not asked for unscaled at all.
    for source in [(1365u64, 768u64), (2560, 1441), (5120, 2880)] {
        let steps = helper_steps(Some(source));
        assert!(steps.iter().all(|step| step.divisor > 1), "{source:?}");
        for step in steps {
            assert_eq!(
                step.scaled.divisor(source.0, source.1),
                Some(step.divisor as u64),
                "{source:?}"
            );
        }
    }
    assert!(!helper_steps(Some((5120, 2880))).is_empty());
    // The helper's quality numbers are a fixed pairing with ours, strictly falling.
    assert_eq!(HELPER_QUALITIES.len(), QUALITIES.len());
    assert!(HELPER_QUALITIES.windows(2).all(|pair| pair[0] > pair[1]));
    assert!(HELPER_QUALITIES
        .iter()
        .all(|quality| (1..=100).contains(quality)));
    assert!(HELPER_ATTEMPTS <= DIVISORS.len() * QUALITIES.len());
}

#[test]
fn helper_jpeg_becomes_the_same_control_frame_shape_as_the_transform_writes() {
    let source = smooth(2560, 1440);
    let step = helper_steps(Some((2560, 1440)))[3];
    let reply = helper_reply(&source, 2, &step);
    let mut timing = FrameTiming::default();
    let frame = control_frame_from_helper_jpeg(reply.clone(), &step, &mut timing).unwrap();
    verify_control_frame(&frame).unwrap();
    // The image bytes are the helper's, untouched; the hash is of exactly those bytes.
    assert_eq!(frame["data_base64"], reply["data_base64"]);
    let jpeg = base64::engine::general_purpose::STANDARD
        .decode(frame["data_base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(frame["sha256"], vm_session_auth::hash(&jpeg));
    assert_eq!(frame["byte_length"], jpeg.len());
    assert_eq!(jpeg_dimensions(&jpeg).unwrap(), (1280, 720));
    // Every field the transform writes for a scaled frame, with the same meaning; the binding fields are the
    // source's.
    let transformed = {
        let png = tests::stored_png(2560, 1440, 1, &tests::noise(2560 * 1440, 3));
        control_frame_from_screenshot(tests::screenshot(2560, 1440, &png)).unwrap()
    };
    for field in [
        "mime_type",
        "frame_width",
        "frame_height",
        "scale_divisor",
        "frame_quality",
        "sha256",
        "byte_length",
    ] {
        assert_eq!(
            frame[field].is_u64(),
            transformed[field].is_u64(),
            "{field}"
        );
        assert_eq!(
            frame[field].is_string(),
            transformed[field].is_string(),
            "{field}"
        );
    }
    assert_eq!(frame["mime_type"], "image/jpeg");
    assert_eq!(
        frame["frame_quality"], step.quality,
        "quality is restated on our scale"
    );
    for field in [
        "frame_id",
        "source_generation",
        "source_sequence",
        "width",
        "height",
        "captured_monotonic_us",
    ] {
        assert_eq!(frame[field], reply[field], "{field}");
    }
    assert!(frame.get("source_mime_type").is_none() && frame.get("source_byte_length").is_none());
    assert!(timing.helper_jpeg && !timing.passthrough);
    assert_eq!(
        (timing.width, timing.height, timing.divisor),
        (2560, 1440, 2)
    );
    assert_eq!(
        (timing.quality, timing.helper_quality),
        (step.quality, step.scaled.quality as u8)
    );
    assert_eq!((timing.frame_bytes, timing.attempts), (jpeg.len(), 0));
    assert_eq!(
        (timing.inflate_us, timing.scale_us, timing.jpeg_us),
        (0, 0, 0)
    );
}

#[test]
fn helper_reply_that_disagrees_with_the_request_or_itself_is_refused_as_invalid() {
    let source = smooth(640, 480);
    let step = helper_steps(Some((640, 480)))[3];
    let good = helper_reply(&source, 2, &step);
    control_frame_from_helper_jpeg(good.clone(), &step, &mut FrameTiming::default()).unwrap();
    let other =
        base64::engine::general_purpose::STANDARD.encode(encode_jpeg(&smooth(322, 240), 80));
    type Change = Box<dyn Fn(&mut Value)>;
    let changes: Vec<Change> = vec![
        Box::new(|v| v["scale_divisor"] = json!(4)),
        Box::new(|v| v["scale_divisor"] = json!(5)),
        Box::new(|v| v["frame_width"] = json!(322)),
        Box::new(|v| v["width"] = json!(642)),
        Box::new(|v| v["frame_quality"] = json!(99)),
        Box::new(|v| v["byte_length"] = json!(7)),
        Box::new(|v| v["data_base64"] = json!("!")),
        Box::new(|v| v["mime_type"] = json!("image/png")),
        Box::new(|v| {
            v.as_object_mut().unwrap().remove("frame_height");
        }),
        // A real JPEG of another size under the declared one.
        Box::new(move |v| {
            v["byte_length"] = json!(other.len() * 3 / 4);
            v["data_base64"] = json!(other.clone());
        }),
    ];
    for (index, change) in changes.iter().enumerate() {
        let mut bad = good.clone();
        change(&mut bad);
        let error =
            control_frame_from_helper_jpeg(bad, &step, &mut FrameTiming::default()).unwrap_err();
        assert_eq!(
            FailureCode::of(&error).map(FailureCode::name),
            Some("screenshot_reply_invalid"),
            "change {index}: {error}"
        );
    }
    // A frame that is consistent but larger than the bound that was asked for (the helper ignored the request).
    let full = helper_reply(&source, 1, &step);
    assert!(control_frame_from_helper_jpeg(full, &step, &mut FrameTiming::default()).is_err());
    // A JPEG above the frame bound, honestly declared.
    let noisy = Rgb {
        width: 2560,
        height: 1440,
        data: tests::noise(2560 * 1440 * 3, 5),
    };
    let first = helper_steps(None)[0];
    let big = helper_reply(&noisy, 1, &first);
    assert!(big["byte_length"].as_u64().unwrap() > CONTROL_FRAME_MAX_BYTES as u64);
    let error =
        control_frame_from_helper_jpeg(big, &first, &mut FrameTiming::default()).unwrap_err();
    assert!(
        error.to_string().contains("exceeds the frame bound"),
        "{error}"
    );
}

/// BMP (24- or 32-bit, either row order) to RGB.
fn bmp_rgb(bmp: &[u8]) -> Rgb {
    let le = |at: usize| u32::from_le_bytes(bmp[at..at + 4].try_into().unwrap()) as usize;
    let (offset, width, raw_height, depth) = (le(10), le(18), le(22) as i32, le(28) & 0xffff);
    let (height, top_down) = (raw_height.unsigned_abs() as usize, raw_height < 0);
    assert!(depth == 24 || depth == 32);
    let stride = (width * depth / 8).div_ceil(4) * 4;
    let mut data = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        let row = offset + if top_down { y } else { height - 1 - y } * stride;
        for x in 0..width {
            let at = row + x * depth / 8;
            data.extend_from_slice(&[bmp[at + 2], bmp[at + 1], bmp[at]]);
        }
    }
    Rgb {
        width,
        height,
        data,
    }
}
fn rgb_bmp(image: &Rgb) -> Vec<u8> {
    let row = (image.width * 3).div_ceil(4) * 4;
    let mut bmp = Vec::with_capacity(54 + row * image.height);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((54 + row * image.height) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&(image.width as u32).to_le_bytes());
    bmp.extend_from_slice(&(image.height as u32).to_le_bytes());
    bmp.extend_from_slice(&[1, 0, 24, 0]);
    bmp.extend_from_slice(&[0; 24]);
    for line in image.data.chunks(image.width * 3).rev() {
        for pixel in line.chunks_exact(3) {
            bmp.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
        bmp.resize(bmp.len() + row - image.width * 3, 0);
    }
    bmp
}
fn sips(dir: &std::path::Path, input: &[u8], from: &str, to: &str, quality: Option<u8>) -> Vec<u8> {
    let (source, target) = (
        dir.join(format!("in.{from}")),
        dir.join(format!("out.{to}")),
    );
    std::fs::write(&source, input).unwrap();
    let mut command = std::process::Command::new("/usr/bin/sips");
    command.args(["-s", "format", if to == "jpg" { "jpeg" } else { to }]);
    if let Some(quality) = quality {
        // The helper's `kCGImageDestinationLossyCompressionQuality = quality / 100`.
        command.args(["-s", "formatOptions", &quality.to_string()]);
    }
    let status = command
        .arg(&source)
        .arg("--out")
        .arg(&target)
        .output()
        .unwrap();
    assert!(status.status.success(), "sips: {status:?}");
    std::fs::read(&target).unwrap()
}
/// Mean and worst absolute difference per channel sample.
fn difference(a: &Rgb, b: &Rgb) -> (f64, u8) {
    assert_eq!((a.width, a.height), (b.width, b.height));
    let (mut total, mut worst) = (0u64, 0u8);
    for (x, y) in a.data.iter().zip(&b.data) {
        total += x.abs_diff(*y) as u64;
        worst = worst.max(x.abs_diff(*y));
    }
    (total as f64 / a.data.len() as f64, worst)
}
/// The pictures the mapping is judged on: the synthetic desktop, the smooth gradient, and any stored frames named
/// in `VEYNODE_TEST_FRAME_PNGS` (colon-separated 8-bit PNG paths; never part of the repository).
fn mapping_pictures() -> Vec<(String, Rgb)> {
    let mut out = vec![
        (
            "desktop-2560x1440".to_string(),
            Rgb {
                width: 2560,
                height: 1440,
                data: bench_tests::desktop(2560, 1440),
            },
        ),
        ("smooth-2560x1440".to_string(), smooth(2560, 1440)),
    ];
    if let Some(paths) = std::env::var_os("VEYNODE_TEST_FRAME_PNGS") {
        for path in std::env::split_paths(&paths) {
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            out.push((name, decode_png(&std::fs::read(&path).unwrap()).unwrap()));
        }
    }
    out
}
/// What ImageIO produces at the helper's quality number for each of our qualities, at the first two divisors: the
/// sizes side by side, and how far each decoded JPEG is from the reference transform's scaled pixels.
#[cfg(target_os = "macos")]
#[test]
fn helper_quality_mapping_sizes_and_error() {
    if !std::path::Path::new("/usr/bin/sips").exists() {
        eprintln!("sips unavailable; helper quality mapping not checked");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sweep = std::env::var_os("VEYNODE_TEST_QUALITY_SWEEP").is_some();
    for (name, source) in mapping_pictures() {
        for divisor in [1usize, 2] {
            let scaled = reference::downscale(&source, divisor);
            let bmp = rgb_bmp(&scaled);
            if sweep {
                let sizes: Vec<String> = (10..=80u8)
                    .step_by(5)
                    .map(|quality| {
                        format!(
                            "{quality}:{}",
                            sips(dir.path(), &bmp, "bmp", "jpg", Some(quality)).len()
                        )
                    })
                    .collect();
                eprintln!(
                    "{name} /{divisor} ImageIO quality:bytes {}",
                    sizes.join(" ")
                );
            }
            for (quality, helper) in QUALITIES.into_iter().zip(HELPER_QUALITIES) {
                let ours = reference::encode_jpeg(&scaled, quality);
                let theirs = sips(dir.path(), &bmp, "bmp", "jpg", Some(helper));
                assert_eq!(
                    jpeg_dimensions(&theirs).unwrap(),
                    (scaled.width as u64, scaled.height as u64)
                );
                let ours_decoded = bmp_rgb(&sips(dir.path(), &ours, "jpg", "bmp", None));
                let theirs_decoded = bmp_rgb(&sips(dir.path(), &theirs, "jpg", "bmp", None));
                let (ours_error, _) = difference(&ours_decoded, &scaled);
                let (theirs_error, theirs_worst) = difference(&theirs_decoded, &scaled);
                let (between, _) = difference(&ours_decoded, &theirs_decoded);
                eprintln!(
                    "{name} /{divisor} ours q{quality} {} bytes mean error {ours_error:.3} | ImageIO q{helper} {} bytes \
                     mean error {theirs_error:.3} worst {theirs_worst} | between the two {between:.3}",
                    ours.len(),
                    theirs.len()
                );
                // Where the frame bound can matter (not the few tens of KiB of the gradient), the helper's JPEG is
                // no larger than ours: a picture that fits the bound here fits there at the same step.
                assert!(
                    name.starts_with("smooth") || theirs.len() <= ours.len(),
                    "{name} /{divisor} q{quality}: ImageIO q{helper} {} bytes > ours {}",
                    theirs.len(),
                    ours.len()
                );
                if name.starts_with("smooth") {
                    // The tolerance `imageio_decodes_our_jpeg_to_the_source_pixels` holds our own encoder to.
                    assert!(
                        theirs_error < 2.0 && theirs_worst < 24,
                        "{name} /{divisor} q{helper}: mean {theirs_error} worst {theirs_worst}"
                    );
                    assert!(between < 2.0, "{name} /{divisor}: {between}");
                }
            }
        }
    }
}
