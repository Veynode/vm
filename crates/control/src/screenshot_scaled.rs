//! The scaled-JPEG form of `screenshot` (decision 0025 item 2): the console helper scales the frame and encodes the
//! JPEG itself, so no full-size PNG crosses the four hops.
//!
//! The form is asked for with four fields, all together or not at all: `format` ("jpeg"), `max_width` (2..=3840),
//! `max_height` (2..=2160) and `quality` (1..=100, the helper's ImageIO scale), each a JSON integer. A request
//! without any of them is the PNG form and is not touched by anything here. Every hop (daemon, QEMU worker, console
//! broker) parses the fields with `Scaled::parse`, refuses anything partial or out of range, and checks a JPEG reply
//! with `Scaled::verify` before passing it on. Nothing is clamped, defaulted or dropped.
//!
//! A hop that cannot serve the form says so with one fixed word beside its error text (`REFUSAL_FIELD`), so the
//! caller can tell "ask again smaller" from "this path has no such form" without reading error prose.
use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

/// The four request fields, in the order they are written.
pub const FIELDS: [&str; 4] = ["format", "max_width", "max_height", "quality"];
/// Image bytes one scaled reply may carry: the browser control frame bound.
pub const MAX_BYTES: usize = 700 * 1024;
/// The divisors the helper chooses from, smallest first.
pub const DIVISORS: [u64; 8] = [1, 2, 3, 4, 6, 8, 12, 16];
pub const MAX_WIDTH: u64 = 3840;
pub const MAX_HEIGHT: u64 = 2160;
/// Beside `error` in a refused reply of the scaled form: one of the `Refusal` names.
pub const REFUSAL_FIELD: &str = "screenshot_refusal";

/// Why the scaled form was not served. Fixed words; no helper, Guest or request text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The four fields are partial, of the wrong type or out of range.
    InvalidRequest,
    /// This path has no scaled form: a helper without it, the one-shot capture helper, an Apple-backend Guest.
    FormatUnavailable,
    /// No allowed divisor brings the frame within the requested size with both sides even.
    ScaleUnavailable,
    /// The JPEG came out above `MAX_BYTES`. The helper does not retry; ask again with another quality or size.
    EncodedSizeLimit,
}
impl Refusal {
    pub const ALL: [Self; 4] = [
        Self::InvalidRequest,
        Self::FormatUnavailable,
        Self::ScaleUnavailable,
        Self::EncodedSizeLimit,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::FormatUnavailable => "format_unavailable",
            Self::ScaleUnavailable => "scale_unavailable",
            Self::EncodedSizeLimit => "encoded_size_limit",
        }
    }
    /// From `REFUSAL_FIELD` of a lower hop's reply; anything but one of the fixed words is no refusal.
    pub fn from_name(value: &Value) -> Option<Self> {
        let name = value.as_str()?;
        Self::ALL.into_iter().find(|refusal| refusal.name() == name)
    }
    /// From the console helper's error object for a request of the scaled form.
    pub fn from_helper(error: &Value) -> Option<Self> {
        match error["code"].as_str()? {
            "invalid_screenshot_request" => Some(Self::InvalidRequest),
            "screenshot_format_unavailable" => Some(Self::FormatUnavailable),
            "screenshot_scale_unavailable" => Some(Self::ScaleUnavailable),
            "screenshot_unavailable_or_too_large" => match error["diagnostic"]["stage"].as_str() {
                Some("encoded_size_limit") => Some(Self::EncodedSizeLimit),
                Some("scale_unavailable") => Some(Self::ScaleUnavailable),
                _ => None,
            },
            _ => None,
        }
    }
}
/// A refusal of the scaled form as an error: `Display` is the hop's ordinary error text, the word travels beside it.
#[derive(Debug)]
pub struct Refused {
    pub refusal: Refusal,
    text: String,
}
impl Refused {
    pub fn new(refusal: Refusal, text: impl Into<String>) -> Self {
        Self {
            refusal,
            text: text.into(),
        }
    }
    /// The refusal an error carries, if it is one.
    pub fn of(error: &anyhow::Error) -> Option<Refusal> {
        error.downcast_ref::<Self>().map(|refused| refused.refusal)
    }
    /// Add `REFUSAL_FIELD` to an error reply when `error` is a refusal of the scaled form; otherwise leave it.
    pub fn mark(reply: &mut Value, error: &anyhow::Error) {
        if let Some(refusal) = Self::of(error) {
            reply[REFUSAL_FIELD] = json!(refusal.name());
        }
    }
}
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}
impl std::error::Error for Refused {}

/// One validated request of the scaled form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scaled {
    pub max_width: u64,
    pub max_height: u64,
    pub quality: u64,
}
impl Scaled {
    /// `Ok(None)` when `object` has none of the four fields (the PNG form). All four, valid: the scaled form.
    /// Anything else is `Refusal::InvalidRequest`; a present `null` counts as present.
    pub fn parse(object: &Value) -> Result<Option<Self>> {
        if FIELDS.iter().all(|field| object.get(field).is_none()) {
            return Ok(None);
        }
        let whole = |field: &str, range: std::ops::RangeInclusive<u64>| {
            object
                .get(field)
                .and_then(Value::as_u64)
                .filter(|value| range.contains(value))
        };
        match (
            object.get("format").and_then(Value::as_str),
            whole("max_width", 2..=MAX_WIDTH),
            whole("max_height", 2..=MAX_HEIGHT),
            whole("quality", 1..=100),
        ) {
            (Some("jpeg"), Some(max_width), Some(max_height), Some(quality)) => Ok(Some(Self {
                max_width,
                max_height,
                quality,
            })),
            _ => Err(Refused::new(
                Refusal::InvalidRequest,
                "invalid scaled screenshot request: format jpeg, max_width 2..3840, max_height 2..2160 and quality 1..100 are required together",
            )
            .into()),
        }
    }
    /// Write the four fields onto a request object.
    pub fn apply(&self, request: &mut Value) {
        request["format"] = json!("jpeg");
        request["max_width"] = json!(self.max_width);
        request["max_height"] = json!(self.max_height);
        request["quality"] = json!(self.quality);
    }
    /// The helper's geometry rule: the smallest allowed divisor whose frame `ceil(source / divisor)` is within the
    /// requested size and has two even sides.
    pub fn divisor(&self, width: u64, height: u64) -> Option<u64> {
        if width == 0 || height == 0 {
            return None;
        }
        DIVISORS.into_iter().find(|divisor| {
            let (w, h) = (width.div_ceil(*divisor), height.div_ceil(*divisor));
            w <= self.max_width && h <= self.max_height && w % 2 == 0 && h % 2 == 0
        })
    }
    /// Check a `image/jpeg` reply to this request and return the JPEG bytes. Refused unless: the divisor is an
    /// smallest allowed one with two even frame sides within the requested size, `frame = ceil(source / divisor)`
    /// on both axes, the quality is the requested one, the base64 is valid, `byte_length` is the decoded length and at most
    /// `MAX_BYTES`, the bytes are a JPEG and the dimensions in its header are the declared frame.
    pub fn verify(&self, value: &Value) -> Result<Vec<u8>> {
        ensure!(
            value["mime_type"] == "image/jpeg",
            "scaled screenshot is not a JPEG"
        );
        let field = |name: &str| {
            value[name]
                .as_u64()
                .with_context(|| format!("scaled screenshot {name} missing"))
        };
        let (width, height) = (field("width")?, field("height")?);
        let (frame_width, frame_height, divisor) = (
            field("frame_width")?,
            field("frame_height")?,
            field("scale_divisor")?,
        );
        ensure!(
            (1..=8192).contains(&width) && (1..=8192).contains(&height),
            "scaled screenshot source dimensions invalid"
        );
        ensure!(
            DIVISORS.contains(&divisor)
                && frame_width == width.div_ceil(divisor)
                && frame_height == height.div_ceil(divisor),
            "scaled screenshot scale disagrees with its source geometry"
        );
        ensure!(
            frame_width <= self.max_width && frame_height <= self.max_height,
            "scaled screenshot exceeds the requested size"
        );
        ensure!(
            self.divisor(width, height) == Some(divisor),
            "scaled screenshot divisor is not the smallest allowed fit with even sides"
        );
        ensure!(
            field("frame_quality")? == self.quality,
            "scaled screenshot quality is not the requested one"
        );
        let encoded = value["data_base64"]
            .as_str()
            .context("scaled screenshot data missing")?;
        ensure!(
            encoded.len() <= MAX_BYTES.div_ceil(3) * 4,
            "scaled screenshot exceeds the frame bound"
        );
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("scaled screenshot base64 invalid")?;
        ensure!(
            bytes.len() <= MAX_BYTES && value["byte_length"].as_u64() == Some(bytes.len() as u64),
            "scaled screenshot size mismatch"
        );
        ensure!(
            bytes.starts_with(&[0xff, 0xd8, 0xff]),
            "scaled screenshot signature is not a JPEG"
        );
        ensure!(
            jpeg_dimensions(&bytes)? == (frame_width, frame_height),
            "scaled screenshot scale disagrees with its real dimensions"
        );
        Ok(bytes)
    }
}
/// `(width, height)` from the first frame header of a JPEG that starts with SOI and ends with EOI.
pub fn jpeg_dimensions(bytes: &[u8]) -> Result<(u64, u64)> {
    ensure!(
        bytes.len() >= 4 && bytes.starts_with(&[0xff, 0xd8]) && bytes.ends_with(&[0xff, 0xd9]),
        "JPEG boundaries invalid"
    );
    let mut position = 2;
    loop {
        ensure!(
            position + 4 <= bytes.len() && bytes[position] == 0xff,
            "JPEG frame header missing"
        );
        let marker = bytes[position + 1];
        let length = u16::from_be_bytes([bytes[position + 2], bytes[position + 3]]) as usize;
        ensure!(
            length >= 2 && position + 2 + length <= bytes.len(),
            "JPEG segment truncated"
        );
        if matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            ensure!(length >= 7, "JPEG frame header truncated");
            let height = u16::from_be_bytes([bytes[position + 5], bytes[position + 6]]) as u64;
            let width = u16::from_be_bytes([bytes[position + 7], bytes[position + 8]]) as u64;
            return Ok((width, height));
        }
        if marker == 0xda || marker == 0xd9 {
            bail!("JPEG frame header missing")
        }
        position += 2 + length;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    /// The smallest byte string `jpeg_dimensions` accepts for this size: SOI, a frame header, EOI. Not a picture.
    pub(crate) fn jpeg_stub(width: u16, height: u16, pad: usize) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8, 0xff, 0xc0, 0, 17, 8];
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        if pad > 0 {
            // One comment segment per 60000 bytes of padding.
            let mut left = pad;
            while left > 0 {
                let take = left.min(60000);
                out.extend_from_slice(&[0xff, 0xfe]);
                out.extend_from_slice(&((take + 2) as u16).to_be_bytes());
                out.extend_from_slice(&vec![b'x'; take]);
                left -= take;
            }
        }
        out.extend_from_slice(&[0xff, 0xd9]);
        out
    }
    /// A reply of the scaled form as the helper writes it, before any hop adds its own fields.
    pub(crate) fn reply(width: u64, height: u64, divisor: u64, quality: u64) -> Value {
        let (frame_width, frame_height) = (width.div_ceil(divisor), height.div_ceil(divisor));
        let jpeg = jpeg_stub(frame_width as u16, frame_height as u16, 0);
        json!({"byte_length":jpeg.len(),"captured_at_us":1,"captured_monotonic_us":2,
            "coordinate_space":"guest-physical-pixels",
            "data_base64":base64::engine::general_purpose::STANDARD.encode(&jpeg),
            "frame_height":frame_height,"frame_quality":quality,"frame_width":frame_width,"height":height,
            "mime_type":"image/jpeg","scale_divisor":divisor,"source_generation":3,"source_sequence":4,"width":width})
    }
    const ASK: Scaled = Scaled {
        max_width: 1280,
        max_height: 720,
        quality: 50,
    };

    #[test]
    fn request_fields_are_all_or_none_and_never_clamped() {
        assert_eq!(Scaled::parse(&json!({"id":"vm"})).unwrap(), None);
        assert_eq!(Scaled::parse(&json!({})).unwrap(), None);
        let good =
            json!({"id":"vm","format":"jpeg","max_width":1280,"max_height":720,"quality":50});
        assert_eq!(Scaled::parse(&good).unwrap(), Some(ASK));
        for (max_width, max_height, quality) in [(2, 2, 1), (3840, 2160, 100)] {
            let edge = json!({"format":"jpeg","max_width":max_width,"max_height":max_height,"quality":quality});
            let parsed = Scaled::parse(&edge).unwrap().unwrap();
            assert_eq!(
                (parsed.max_width, parsed.max_height, parsed.quality),
                (max_width, max_height, quality)
            );
            // What is written back is what was read.
            let mut written = json!({});
            parsed.apply(&mut written);
            assert_eq!(written, edge);
        }
        let mut bad = Vec::new();
        for field in FIELDS {
            // Each field missing, and each field alone.
            let mut missing = good.clone();
            missing.as_object_mut().unwrap().remove(field);
            bad.push(missing);
            bad.push(json!({field: good[field]}));
            let mut null = good.clone();
            null[field] = Value::Null;
            bad.push(null);
        }
        for (field, value) in [
            ("format", json!("png")),
            ("format", json!("JPEG")),
            ("format", json!(1)),
            ("max_width", json!(1)),
            ("max_width", json!(3841)),
            ("max_width", json!(1280.0)),
            ("max_width", json!("1280")),
            ("max_width", json!(-1280)),
            ("max_height", json!(1)),
            ("max_height", json!(2161)),
            ("max_height", json!(720.5)),
            ("quality", json!(0)),
            ("quality", json!(101)),
            ("quality", json!(50.0)),
            ("quality", json!(true)),
        ] {
            let mut changed = good.clone();
            changed[field] = value;
            bad.push(changed);
        }
        for request in bad {
            let error = Scaled::parse(&request).unwrap_err();
            assert_eq!(
                Refused::of(&error),
                Some(Refusal::InvalidRequest),
                "{request}"
            );
        }
    }
    #[test]
    fn divisor_is_the_smallest_that_fits_with_two_even_sides() {
        let ask = |max_width, max_height| Scaled {
            max_width,
            max_height,
            quality: 50,
        };
        assert_eq!(ask(3840, 2160).divisor(2560, 1440), Some(1));
        assert_eq!(ask(1280, 720).divisor(2560, 1440), Some(2));
        assert_eq!(ask(1279, 720).divisor(2560, 1440), Some(3));
        // 1366x768 halves to 683x384: odd, so the next divisor that is even on both sides is taken.
        assert_eq!(ask(683, 384).divisor(1366, 768), Some(3));
        assert_eq!(ask(2, 2).divisor(2560, 1440), None);
        assert_eq!(ask(160, 90).divisor(2560, 1440), Some(16));
        assert_eq!(ask(3840, 2160).divisor(0, 1440), None);
    }
    #[test]
    fn reply_geometry_matches_the_native_selection_rule_over_sizes_and_bounds() {
        // Independent transcription of AsyncScreenshot.m's divisorForWidth:height:maxWidth:maxHeight:.
        let native_divisor = |width: u64, height: u64, max_width: u64, max_height: u64| {
            [1u64, 2, 3, 4, 6, 8, 12, 16].into_iter().find(|&d| {
                let (fw, fh) = (width.div_ceil(d), height.div_ceil(d));
                fw <= max_width && fh <= max_height && fw % 2 == 0 && fh % 2 == 0
            })
        };
        let sizes = [
            (2560, 1440),
            (1280, 800),
            (1366, 768),
            (1920, 1080),
            (3840, 2160),
            (1365, 767),
            (2, 2),
            (3840, 2),
            (1, 1),
        ];
        let mut accepted = 0;
        let mut unavailable = 0;
        for (width, height) in sizes {
            for max_width in [2, 160, 456, 683, 684, 1280, 1920, 3840] {
                for max_height in [2, 90, 256, 384, 720, 800, 1080, 2160] {
                    let ask = Scaled {
                        max_width,
                        max_height,
                        quality: 50,
                    };
                    let expected = native_divisor(width, height, max_width, max_height);
                    assert_eq!(ask.divisor(width, height), expected);
                    unavailable += usize::from(expected.is_none());
                    // Even a completely self-consistent reply (including JPEG header) is invalid for every
                    // non-selected divisor. None means the helper refuses, so no successful reply may pass.
                    for divisor in [1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 17] {
                        let value = reply(width, height, divisor, ask.quality);
                        let fits = expected == Some(divisor);
                        assert_eq!(ask.verify(&value).is_ok(), fits,
                            "source {width}x{height}, bound {max_width}x{max_height}, divisor {divisor}, expected {expected:?}");
                        accepted += usize::from(fits);
                    }
                    if let Some(divisor) = expected {
                        let good = reply(width, height, divisor, ask.quality);
                        for side in ["frame_width", "frame_height"] {
                            // Wrong rounding, with JPEG dimensions changed to agree with the faulty metadata.
                            let mut bad = good.clone();
                            bad[side] = json!(bad[side].as_u64().unwrap() - 1);
                            let jpeg = jpeg_stub(
                                bad["frame_width"].as_u64().unwrap() as u16,
                                bad["frame_height"].as_u64().unwrap() as u16,
                                0,
                            );
                            bad["byte_length"] = json!(jpeg.len());
                            bad["data_base64"] =
                                json!(base64::engine::general_purpose::STANDARD.encode(jpeg));
                            assert!(ask.verify(&bad).is_err(), "incorrect {side}: {bad}");
                        }
                    }
                }
            }
        }
        assert!(accepted > 0 && unavailable > 0);
    }
    #[test]
    fn odd_halves_unnecessary_divisors_and_unavailable_scales_are_refused() {
        let ask = Scaled {
            max_width: 684,
            max_height: 384,
            quality: 50,
        };
        assert_eq!(ask.divisor(1366, 768), Some(3));
        assert!(ask.verify(&reply(1366, 768, 2, 50)).is_err());
        assert!(ask.verify(&reply(1366, 768, 3, 50)).is_ok());
        assert!(ASK.verify(&reply(2560, 1440, 4, 50)).is_err());
        // A side of 1 stays odd for every divisor; 3840x2 cannot shrink without making its height odd.
        for (width, height) in [(1, 1), (3840, 2)] {
            let tiny = Scaled {
                max_width: 2,
                max_height: 2,
                quality: 50,
            };
            assert_eq!(tiny.divisor(width, height), None);
            for divisor in DIVISORS {
                assert!(tiny.verify(&reply(width, height, divisor, 50)).is_err());
            }
        }
        let mut zero = reply(2560, 1440, 2, 50);
        zero["scale_divisor"] = json!(0);
        assert!(ASK.verify(&zero).is_err());
    }
    #[test]
    fn reply_is_refused_unless_every_declared_fact_agrees_with_the_bytes() {
        let good = reply(2560, 1440, 2, 50);
        let bytes = ASK.verify(&good).unwrap();
        assert_eq!(jpeg_dimensions(&bytes).unwrap(), (1280, 720));
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        type Change = Box<dyn Fn(&mut Value)>;
        let changes: Vec<(&str, Change)> = vec![
            (
                "not a JPEG",
                Box::new(|v| v["mime_type"] = json!("image/png")),
            ),
            (
                "scale disagrees with its source",
                Box::new(|v| v["scale_divisor"] = json!(4)),
            ),
            (
                "scale disagrees with its source",
                Box::new(|v| v["scale_divisor"] = json!(5)),
            ),
            (
                "scale disagrees with its source",
                Box::new(|v| v["frame_width"] = json!(1279)),
            ),
            (
                "scale disagrees with its source",
                Box::new(|v| v["width"] = json!(2561)),
            ),
            (
                "source dimensions invalid",
                Box::new(|v| v["height"] = json!(0)),
            ),
            (
                "source dimensions invalid",
                Box::new(|v| v["width"] = json!(8193)),
            ),
            (
                "frame_height missing",
                Box::new(|v| v["frame_height"] = json!("720")),
            ),
            (
                "scale_divisor missing",
                Box::new(|v| {
                    v.as_object_mut().unwrap().remove("scale_divisor");
                }),
            ),
            (
                "quality is not the requested",
                Box::new(|v| v["frame_quality"] = json!(80)),
            ),
            (
                "frame_quality missing",
                Box::new(|v| {
                    v.as_object_mut().unwrap().remove("frame_quality");
                }),
            ),
            (
                "base64 invalid",
                Box::new(|v| v["data_base64"] = json!("!not base64")),
            ),
            ("data missing", Box::new(|v| v["data_base64"] = Value::Null)),
            ("size mismatch", Box::new(|v| v["byte_length"] = json!(1))),
            (
                "signature is not a JPEG",
                Box::new(move |v| {
                    let png = b"\x89PNG\r\n\x1a\n\xff\xd9".to_vec();
                    v["byte_length"] = json!(png.len());
                    v["data_base64"] = json!(encode(&png));
                }),
            ),
            (
                "real dimensions",
                Box::new(move |v| {
                    let other = jpeg_stub(1280, 722, 0);
                    v["byte_length"] = json!(other.len());
                    v["data_base64"] = json!(encode(&other));
                }),
            ),
            (
                "JPEG boundaries invalid",
                Box::new(move |v| {
                    let mut cut = jpeg_stub(1280, 720, 0);
                    cut.truncate(cut.len() - 1);
                    v["byte_length"] = json!(cut.len());
                    v["data_base64"] = json!(encode(&cut));
                }),
            ),
        ];
        for (expected, change) in changes {
            let mut bad = good.clone();
            change(&mut bad);
            let error = ASK.verify(&bad).unwrap_err().to_string();
            assert!(error.contains(expected), "{expected}: {error}");
        }
        // Self-consistent but larger than what was asked for.
        let full = reply(2560, 1440, 1, 50);
        assert!(ASK
            .verify(&full)
            .unwrap_err()
            .to_string()
            .contains("exceeds the requested size"));
        // Exactly the bound passes, one byte more does not, whatever it declares.
        for (extra, fits) in [(0usize, true), (1, false)] {
            let stub = jpeg_stub(1280, 720, 0).len();
            let mut pad = MAX_BYTES + extra - stub;
            // Each 60000-byte comment segment costs four bytes of header.
            pad -= pad.div_ceil(60004) * 4;
            let jpeg = jpeg_stub(1280, 720, pad);
            assert_eq!(jpeg.len(), MAX_BYTES + extra);
            let mut big = good.clone();
            big["byte_length"] = json!(jpeg.len());
            big["data_base64"] = json!(encode(&jpeg));
            assert_eq!(ASK.verify(&big).is_ok(), fits, "{} bytes", jpeg.len());
        }
    }
    #[test]
    fn refusal_words_are_fixed_and_only_the_helper_codes_of_the_scaled_form_map_to_them() {
        for refusal in Refusal::ALL {
            let name = refusal.name();
            assert!(name.len() <= 24 && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
            assert_eq!(Refusal::from_name(&json!(name)), Some(refusal));
        }
        for other in [
            json!("SECRET"),
            json!(""),
            json!(1),
            Value::Null,
            json!(["encoded_size_limit"]),
        ] {
            assert_eq!(Refusal::from_name(&other), None);
        }
        let helper = |code: &str, stage: Option<&str>| {
            let mut error = json!({"code":code,"message":code});
            if let Some(stage) = stage {
                error["diagnostic"] = json!({"version":1,"stage":stage,"encoded_bytes":null,"maximum_encoded_bytes":716800});
            }
            Refusal::from_helper(&error)
        };
        assert_eq!(
            helper("invalid_screenshot_request", None),
            Some(Refusal::InvalidRequest)
        );
        assert_eq!(
            helper("screenshot_format_unavailable", None),
            Some(Refusal::FormatUnavailable)
        );
        assert_eq!(
            helper("screenshot_scale_unavailable", None),
            Some(Refusal::ScaleUnavailable)
        );
        assert_eq!(
            helper(
                "screenshot_unavailable_or_too_large",
                Some("encoded_size_limit")
            ),
            Some(Refusal::EncodedSizeLimit)
        );
        // Every other helper failure stays an ordinary error: not ready, busy, a failed encode.
        assert_eq!(
            helper(
                "screenshot_unavailable_or_too_large",
                Some("jpeg_finalize_failed")
            ),
            None
        );
        assert_eq!(helper("screenshot_unavailable_or_too_large", None), None);
        assert_eq!(
            helper("frame_unavailable", Some("encoded_size_limit")),
            None
        );
        assert_eq!(helper("screenshot_busy", None), None);
        assert_eq!(Refusal::from_helper(&json!("encoded_size_limit")), None);
        // The word travels beside the text and never replaces it.
        let error: anyhow::Error = Refused::new(Refusal::EncodedSizeLimit, "hop text").into();
        assert_eq!(error.to_string(), "hop text");
        let mut reply = json!({"error":error.to_string()});
        Refused::mark(&mut reply, &error);
        assert_eq!(
            reply,
            json!({"error":"hop text","screenshot_refusal":"encoded_size_limit"})
        );
        let mut plain = json!({"error":"x"});
        Refused::mark(&mut plain, &anyhow::anyhow!("x"));
        assert_eq!(plain, json!({"error":"x"}));
    }
}
