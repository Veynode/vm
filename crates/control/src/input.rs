//! Bounded physical input plans (no text/layout translation), and Unicode text plans that the
//! Guest service delivers. Nothing here injects into a Guest.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    borrow::Cow,
    collections::VecDeque,
    time::{Duration, Instant},
};
pub const FRAME_TTL: Duration = Duration::from_secs(5);
pub fn physical(value: u64, extent: u64) -> Result<u64> {
    if extent == 0 || extent > 8192 || value >= extent {
        bail!("coordinate outside frame")
    }
    Ok(((2 * value + 1) * 32768) / (2 * extent))
}
pub fn key_allowed(key: &str) -> bool {
    key_names().iter().any(|allowed| allowed == key)
}
pub fn key_names() -> Vec<String> {
    let mut names: Vec<String> = (b'a'..=b'z')
        .chain(b'0'..=b'9')
        .map(|c| (c as char).to_string())
        .collect();
    names.extend(
        [
            "ret",
            "esc",
            "tab",
            "spc",
            "backspace",
            "delete",
            "insert",
            "home",
            "end",
            "pgup",
            "pgdn",
            "up",
            "down",
            "left",
            "right",
            "shift",
            "shift_r",
            "ctrl",
            "ctrl_r",
            "alt",
            "alt_r",
            "meta_l",
            "meta_r",
            "f1",
            "f2",
            "f3",
            "f4",
            "f5",
            "f6",
            "f7",
            "f8",
            "f9",
            "f10",
            "f11",
            "f12",
            // QEMU 10.0 QKeyCode ANSI punctuation names (qapi/ui.json).
            // These describe physical positions, never text/layout translation.
            "minus",
            "equal",
            "bracket_left",
            "bracket_right",
            "semicolon",
            "apostrophe",
            "grave_accent",
            "backslash",
            "comma",
            "dot",
            "slash",
        ]
        .into_iter()
        .map(String::from),
    );
    names
}
pub fn button(name: &str, down: bool) -> Value {
    json!({"type":"btn","data":{"button":name,"down":down}})
}
pub fn key(name: &str, down: bool) -> Value {
    json!({"type":"key","data":{"key":{"type":"qcode","data":name},"down":down}})
}
fn point(x: u64, y: u64, w: u64, h: u64) -> Result<Vec<Value>> {
    Ok(vec![
        json!({"type":"abs","data":{"axis":"x","value":physical(x,w)?}}),
        json!({"type":"abs","data":{"axis":"y","value":physical(y,h)?}}),
    ])
}
/// Longest key hold. The macOS Guest service retires an operation idle for one second.
pub const MAX_HOLD_MS: u64 = 900;
/// Milliseconds between a plan's first and second step: a key's press and release, which
/// games sample per frame, so a 20 ms tap can be missed while a held key steers or accelerates.
pub fn first_gap_ms(method: &str, args: &Value) -> Result<u64> {
    match args.get("hold_ms") {
        None => Ok(20),
        Some(value) if method == "input_key" => match value.as_u64() {
            Some(ms) if ms <= MAX_HOLD_MS => Ok(ms.max(20)),
            _ => bail!("hold_ms must be an integer from 0 to {MAX_HOLD_MS}"),
        },
        Some(_) => bail!("hold_ms applies only to input_key"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinateError {
    InvalidSpace,
    ScaleNotAllowed,
    InvalidScale,
    InvalidPoint(&'static str),
}
impl CoordinateError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidSpace => "invalid_coordinate_space",
            Self::ScaleNotAllowed | Self::InvalidScale => "invalid_point_pixel_scale",
            Self::InvalidPoint(_) => "invalid_point_coordinate",
        }
    }
}
impl std::fmt::Display for CoordinateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: ", self.code())?;
        match self {
            Self::InvalidSpace => write!(f, "coordinate_space must be source_pixels or points"),
            Self::ScaleNotAllowed => write!(f, "point_pixel_scale requires coordinate_space points"),
            Self::InvalidScale => write!(f, "points require finite point_pixel_scale greater than 0 and at most 8"),
            Self::InvalidPoint(field) => write!(f, "{field} must be finite and nonnegative with a representable source pixel coordinate"),
        }
    }
}
impl std::error::Error for CoordinateError {}

/// Convert caller-supplied, display-local points before any frame checks. Screenshot metadata
/// is not consulted. Round to nearest source pixel, ties away from zero; never clamp.
pub fn source_pixel_arguments<'a>(method: &str, args: &'a Value) -> Result<Cow<'a, Value>> {
    if !["input_click", "input_drag", "input_scroll"].contains(&method) {
        return Ok(Cow::Borrowed(args));
    }
    let space = match args.get("coordinate_space") {
        None => "source_pixels",
        Some(value) => value.as_str().ok_or(CoordinateError::InvalidSpace)?,
    };
    match space {
        "source_pixels" => {
            if args.get("point_pixel_scale").is_some() {
                return Err(CoordinateError::ScaleNotAllowed.into());
            }
            return Ok(Cow::Borrowed(args));
        }
        "points" => {}
        _ => return Err(CoordinateError::InvalidSpace.into()),
    }
    let scale = args["point_pixel_scale"]
        .as_f64()
        .filter(|scale| scale.is_finite() && *scale > 0.0 && *scale <= 8.0)
        .ok_or(CoordinateError::InvalidScale)?;
    let mut pixels = args.clone();
    let fields: &[&str] = if method == "input_drag" {
        &["x", "y", "end_x", "end_y"]
    } else {
        &["x", "y"]
    };
    for &field in fields {
        let point = args[field]
            .as_f64()
            .filter(|point| point.is_finite() && *point >= 0.0)
            .ok_or(CoordinateError::InvalidPoint(field))?;
        let pixel = (point * scale).round();
        if !pixel.is_finite() || pixel >= u64::MAX as f64 {
            return Err(CoordinateError::InvalidPoint(field).into());
        }
        pixels[field] = json!(pixel as u64);
    }
    // Internal steps use the existing pixel wire format, and cannot apply the scale twice.
    pixels.as_object_mut().unwrap().remove("coordinate_space");
    pixels.as_object_mut().unwrap().remove("point_pixel_scale");
    Ok(Cow::Owned(pixels))
}

/// Extend only pointer tools, retaining integer coordinates for the default pixel contract.
pub(crate) fn coordinate_tool_schema(method: &str, mut schema: Value) -> Value {
    if !["input_click", "input_drag", "input_scroll"].contains(&method) {
        return schema;
    }
    let properties = &mut schema["properties"];
    properties["coordinate_space"] = json!({"type":"string","enum":["source_pixels","points"],"default":"source_pixels",
        "description":"Source pixels by default. Points are display-local: multiply by caller-supplied point_pixel_scale, round to nearest (ties away from zero), then check pixel bounds without clamping. Frame dimensions remain pixels; scroll ticks are unchanged."});
    properties["point_pixel_scale"] = json!({"type":"number","exclusiveMinimum":0,"maximum":8,
        "description":"Required finite scale for points only, supplied by the caller (for example from vm_windows or display knowledge); never inferred from the screenshot binding."});
    let mut integers = json!({});
    for field in ["x", "y", "end_x", "end_y"] {
        if properties.get(field).is_some() {
            properties[field]["type"] = json!("number");
            integers[field] = json!({"type":"integer"});
        }
    }
    schema["allOf"] = json!([{
        "if":{"properties":{"coordinate_space":{"const":"points"}},"required":["coordinate_space"]},
        "then":{"required":["point_pixel_scale"]},
        "else":{"properties":integers,"not":{"required":["point_pixel_scale"]}}
    }]);
    schema
}
pub fn plan(method: &str, args: &Value) -> Result<Vec<Vec<Value>>> {
    let args = source_pixel_arguments(method, args)?;
    let w = args["width"].as_u64().context("width required")?;
    let h = args["height"].as_u64().context("height required")?;
    physical(0, w)?;
    physical(0, h)?;
    if args["frame_id"].as_str().is_none_or(|s| s.len() != 36)
        || args["source_generation"].as_u64().is_none()
    {
        bail!("screenshot frame_id and source_generation required")
    }
    first_gap_ms(method, &args)?;
    let mut steps = Vec::new();
    if method == "input_key" {
        let keys = args["keys"].as_array().context("keys must be an array")?;
        if keys.is_empty() || keys.len() > 8 {
            bail!("keys must contain 1..8 explicit qcodes")
        }
        let keys: Vec<&str> = keys
            .iter()
            .map(|v| v.as_str().context("key must be string"))
            .collect::<Result<_>>()?;
        if keys.iter().any(|k| !key_allowed(k))
            || keys.iter().enumerate().any(|(i, k)| keys[..i].contains(k))
        {
            bail!("unsupported or duplicate physical qcode")
        }
        steps.push(keys.iter().map(|k| key(k, true)).collect());
        steps.push(keys.iter().rev().map(|k| key(k, false)).collect());
    } else {
        let x = args["x"]
            .as_u64()
            .context("x must be nonnegative integer")?;
        let y = args["y"]
            .as_u64()
            .context("y must be nonnegative integer")?;
        steps.push(point(x, y, w, h)?);
        match method {
            "input_click" | "input_drag" => {
                let b = args
                    .get("button")
                    .map(|v| v.as_str().context("button must be string"))
                    .transpose()?
                    .unwrap_or("left");
                if !["left", "middle", "right"].contains(&b) {
                    bail!("unsupported button")
                }
                steps.push(vec![button(b, true)]);
                if method == "input_drag" {
                    let end_x = args["end_x"].as_u64().context("end_x required")?;
                    let end_y = args["end_y"].as_u64().context("end_y required")?;
                    physical(end_x, w)?;
                    physical(end_y, h)?;
                    for i in 1..=20 {
                        let px = (x as i64 + (end_x as i64 - x as i64) * i / 20) as u64;
                        let py = (y as i64 + (end_y as i64 - y as i64) * i / 20) as u64;
                        steps.push(point(px, py, w, h)?);
                    }
                }
                steps.push(vec![button(b, false)]);
            }
            "input_scroll" => {
                let direction = args["direction"].as_str().context("direction required")?;
                let b = match direction {
                    "up" => "wheel-up",
                    "down" => "wheel-down",
                    "left" => "wheel-left",
                    "right" => "wheel-right",
                    _ => bail!("unsupported scroll direction"),
                };
                let ticks = args["ticks"].as_u64().context("ticks required")?;
                if !(1..=20).contains(&ticks) {
                    bail!("ticks must be 1..20")
                }
                for _ in 0..ticks {
                    steps.push(vec![button(b, true)]);
                    steps.push(vec![button(b, false)]);
                }
            }
            _ => bail!("unsupported input operation"),
        }
    }
    Ok(steps)
}
// ---- Unicode text input -------------------------------------------------------------------
// Text is never translated to physical keys. It is validated here, split into bounded chunks and
// delivered by the Guest service (`text_input`), which either injects every character into the
// focused application or answers with an explicit error. `plan` above is unchanged.

/// Whole-call bounds. A longer text is rejected; the caller splits it.
pub const TEXT_MAX_BYTES: usize = 1024;
pub const TEXT_MAX_SCALARS: usize = 256;
/// Per Guest request. Equals `maxTextScalars` in `guest/veynode-agent/text_input.go`; the Guest
/// completes one chunk well inside the 1 s serial response fence.
pub const TEXT_CHUNK_SCALARS: usize = 16;
pub const TEXT_GUEST_METHOD: &str = "text_input";
/// Daemon-to-worker method carrying one chunk (see `qemu_native_input::text_step`).
pub const TEXT_STEP_METHOD: &str = "input_text_step";
/// Refusal for every VM other than a Linux Guest on QEMU registered with `qemu_guest_exec`.
pub const TEXT_UNSUPPORTED_VM: &str = "unsupported: Unicode text input requires a Linux Guest on QEMU registered with qemu_guest_exec and a Guest service started with --allow-text-input";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextErrorKind {
    /// The text or its arguments violate the contract. Nothing was sent.
    Invalid,
    /// The Guest has no text service (old or disabled service, no display, no XTEST).
    Unsupported,
    Busy,
    /// No focused window, or a modifier/CapsLock is active in the Guest.
    NotReady,
    /// The Guest reported a failure before any key event of the failing chunk.
    NotDelivered,
    /// The failing chunk was delivered in part, or its outcome is unknown.
    Partial,
    /// Every character was injected but the temporary Guest keymap was not restored.
    CleanupFailed,
    /// The Guest reply was malformed or the transport failed; the chunk's outcome is unknown.
    Protocol,
}
impl TextErrorKind {
    pub fn code(self) -> &'static str {
        match self {
            Self::Invalid => "invalid_text",
            Self::Unsupported => "unsupported",
            Self::Busy => "busy",
            Self::NotReady => "not_ready",
            Self::NotDelivered => "not_delivered",
            Self::Partial => "partial_delivery",
            Self::CleanupFailed => "cleanup_failed",
            Self::Protocol => "guest_protocol",
        }
    }
}
/// A text failure with exact delivery accounting. `delivered_scalars` were acknowledged by the
/// Guest; up to `uncertain_scalars` more may have reached the application; the rest were not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextError {
    pub kind: TextErrorKind,
    pub message: String,
    pub delivered_scalars: usize,
    pub uncertain_scalars: usize,
    pub total_scalars: usize,
}
impl TextError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: TextErrorKind::Invalid,
            message: message.into(),
            delivered_scalars: 0,
            uncertain_scalars: 0,
            total_scalars: 0,
        }
    }
    pub fn to_json(&self) -> Value {
        json!({"code":self.kind.code(),"message":self.message,"delivered_scalars":self.delivered_scalars,
            "uncertain_scalars":self.uncertain_scalars,"total_scalars":self.total_scalars})
    }
}
impl std::fmt::Display for TextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.code(), self.message)?;
        if self.total_scalars != 0 {
            write!(
                f,
                " ({} of {} characters delivered",
                self.delivered_scalars, self.total_scalars
            )?;
            if self.uncertain_scalars != 0 {
                write!(f, ", up to {} more unconfirmed", self.uncertain_scalars)?;
            }
            write!(f, "; the remainder was not sent)")?;
        }
        Ok(())
    }
}
impl std::error::Error for TextError {}

/// The complete character policy, identical to `allowedTextRune` in the Guest service.
/// LF and TAB are the only control characters; the Guest presses Return and Tab for them.
pub fn text_char_allowed(c: char) -> bool {
    let v = c as u32;
    if c == '\n' || c == '\t' {
        return true;
    }
    !(v < 0x20
        || (0x7f..=0x9f).contains(&v)
        || (0xfdd0..=0xfdef).contains(&v)
        || v & 0xfffe == 0xfffe)
}
/// Bounded, policy-checked text. Errors name a position and code point, never the text itself.
pub fn validate_text(text: &str) -> Result<usize, TextError> {
    if text.is_empty() {
        return Err(TextError::invalid("text must not be empty"));
    }
    if text.len() > TEXT_MAX_BYTES {
        return Err(TextError::invalid(format!(
            "text exceeds {TEXT_MAX_BYTES} UTF-8 bytes; send it in several calls"
        )));
    }
    let mut scalars = 0;
    for (index, c) in text.chars().enumerate() {
        if !text_char_allowed(c) {
            let hint = if c == '\r' {
                "; use U+000A for a line break"
            } else {
                ""
            };
            // The position only: an error message must not restate any character of the text.
            return Err(TextError::invalid(format!(
                "character {index} is not allowed: only U+000A and U+0009 among control characters, no noncharacters{hint}"
            )));
        }
        scalars = index + 1;
    }
    if scalars > TEXT_MAX_SCALARS {
        return Err(TextError::invalid(format!(
            "text exceeds {TEXT_MAX_SCALARS} Unicode scalar values; send it in several calls"
        )));
    }
    Ok(scalars)
}
/// For callers holding raw bytes: rejects ill-formed UTF-8 (which includes encoded surrogates
/// and values above U+10FFFF) before the character policy.
pub fn validate_text_bytes(bytes: &[u8]) -> Result<&str, TextError> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        TextError::invalid(format!(
            "text is not valid UTF-8 at byte {}",
            error.valid_up_to()
        ))
    })?;
    validate_text(text)?;
    Ok(text)
}
/// Splits at Unicode scalar boundaries only, so the concatenation of the chunks is the input
/// byte for byte. No normalization, no reordering.
pub fn text_chunks(text: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    let (mut start, mut count) = (0, 0);
    for (offset, _) in text.char_indices() {
        if count == TEXT_CHUNK_SCALARS {
            chunks.push(&text[start..offset]);
            start = offset;
            count = 0;
        }
        count += 1;
    }
    if start < text.len() {
        chunks.push(&text[start..]);
    }
    chunks
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPlan {
    pub chunks: Vec<String>,
    pub scalars: usize,
    pub bytes: usize,
}
/// Plans `input_text`. The frame binding is the same as for physical input: a recent screenshot
/// of this VM, so that text goes to the application the caller has just observed.
pub fn plan_text(args: &Value) -> Result<TextPlan> {
    let w = args["width"].as_u64().context("width required")?;
    let h = args["height"].as_u64().context("height required")?;
    physical(0, w)?;
    physical(0, h)?;
    if args["frame_id"].as_str().is_none_or(|s| s.len() != 36)
        || args["source_generation"].as_u64().is_none()
    {
        bail!("screenshot frame_id and source_generation required")
    }
    let text = match &args["text"] {
        Value::String(text) => text,
        _ => return Err(TextError::invalid("text must be a string").into()),
    };
    let scalars = validate_text(text)?;
    Ok(TextPlan {
        chunks: text_chunks(text).into_iter().map(String::from).collect(),
        scalars,
        bytes: text.len(),
    })
}
/// MCP schema fragment for the `text` argument.
pub fn text_tool_properties() -> Value {
    json!({"text":{"type":"string","minLength":1,"maxLength":TEXT_MAX_SCALARS,
        "description":format!("Unicode text for the focused Guest application: at most {TEXT_MAX_SCALARS} scalar values and {TEXT_MAX_BYTES} UTF-8 bytes; U+000A (Return) and U+0009 (Tab) are the only control characters")}})
}
fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
/// The Guest request for one chunk. The text travels as lowercase hex of its UTF-8 bytes so
/// that no JSON layer can substitute or re-escape a character.
pub fn guest_text_request(id: &str, chunk: &str) -> Result<Value> {
    if id.is_empty() || id.len() > 128 {
        bail!("invalid text request id")
    }
    let scalars = validate_text(chunk)?;
    if scalars > TEXT_CHUNK_SCALARS {
        return Err(TextError::invalid("text chunk exceeds the Guest request bound").into());
    }
    Ok(json!({"id":id,"method":TEXT_GUEST_METHOD,"text_hex":hex_lower(chunk.as_bytes())}))
}
fn guest_words(value: &Value) -> String {
    // Guest-controlled text: printable ASCII only, bounded.
    value
        .as_str()
        .unwrap_or("no reason given")
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(160)
        .collect()
}
fn reply_body(reply: &Value) -> &Value {
    // The worker forwards `{id,result}`/`{id,error}`; the daemon may already have unwrapped it.
    match reply.get("result") {
        Some(result) if result.get("result").is_some() || result.get("error").is_some() => result,
        _ => reply,
    }
}
/// Requires a Guest `info` reply that advertises a live text service. Sending `text_input` to a
/// service that predates it would fence the execution transport, so this check is mandatory
/// (`deliver_text` performs it) and absence is an explicit `unsupported`.
pub fn guest_text_capability(info: &Value) -> Result<String, TextError> {
    let body = reply_body(info);
    let info = body.get("result").unwrap_or(body);
    let service = &info["text_input"];
    let unsupported = |message: String| TextError {
        kind: TextErrorKind::Unsupported,
        message,
        delivered_scalars: 0,
        uncertain_scalars: 0,
        total_scalars: 0,
    };
    if info["capabilities"]["text_input"] != true || service["available"] != true {
        let reason = if service.is_object() {
            guest_words(&service["reason"])
        } else {
            "the Guest service does not implement text input".into()
        };
        return Err(unsupported(format!(
            "Guest text input service unavailable: {reason}"
        )));
    }
    if service["max_scalars_per_request"]
        .as_u64()
        .is_none_or(|n| n < TEXT_CHUNK_SCALARS as u64)
        || service["max_bytes_per_request"]
            .as_u64()
            .is_none_or(|n| n < 4 * TEXT_CHUNK_SCALARS as u64)
        || !service["backend"].is_string()
    {
        return Err(unsupported(
            "Guest text input service advertises incompatible limits".into(),
        ));
    }
    Ok(guest_words(&service["backend"]))
}
/// What one chunk's Guest reply means for the characters of that chunk.
/// The daemon's or worker's refusal of one chunk, in the place of a Guest reply: the chunk was
/// not sent. Only the daemon builds this; Guest replies pass through `guest_text_summary`,
/// which cannot produce the key.
pub fn host_refusal(message: &str) -> Value {
    json!({ "host_refusal": message })
}
/// Reduces one Guest `text_input` reply to the fields `deliver_text` accounts with. Anything
/// else the Guest sends (per-key diagnostics would restate the typed text) is dropped, and
/// Guest-controlled strings are reduced to bounded printable ASCII.
pub fn guest_text_summary(reply: &Value) -> Value {
    let body = reply_body(reply);
    if let Some(error) = body.get("error") {
        return json!({"error":{"code":guest_words(&error["code"]),"message":guest_words(&error["message"])}});
    }
    let mut result = serde_json::Map::new();
    for key in [
        "injected_scalars",
        "injected_bytes",
        "batches",
        "waited_ms",
        "pool_keycodes",
    ] {
        if let Some(number) = body["result"][key].as_u64() {
            result.insert(key.into(), json!(number));
        }
    }
    for key in ["backend", "keymap_restore"] {
        if body["result"][key].is_string() {
            result.insert(key.into(), json!(guest_words(&body["result"][key])));
        }
    }
    if let Some(flag) = body["result"]["guest_delivery_confirmed"].as_bool() {
        result.insert("guest_delivery_confirmed".into(), json!(flag));
    }
    json!({ "result": result })
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkDelivery {
    None,
    Unknown,
    All,
}
fn guest_text_result(
    reply: &Value,
    chunk: &str,
) -> std::result::Result<(), (TextErrorKind, String, ChunkDelivery)> {
    if let Some(refusal) = reply.get("host_refusal") {
        let words = guest_words(refusal);
        let kind = if words.contains("busy") {
            TextErrorKind::Busy
        } else if words.contains("unsupported") {
            TextErrorKind::Unsupported
        } else {
            TextErrorKind::NotDelivered
        };
        return Err((kind, format!("not sent: {words}"), ChunkDelivery::None));
    }
    let body = reply_body(reply);
    if let Some(error) = body.get("error") {
        let (kind, delivery) = match error["code"].as_str() {
            Some("unsupported") => (TextErrorKind::Unsupported, ChunkDelivery::None),
            Some("invalid_request") => (TextErrorKind::Invalid, ChunkDelivery::None),
            Some("busy") => (TextErrorKind::Busy, ChunkDelivery::None),
            Some("not_ready") => (TextErrorKind::NotReady, ChunkDelivery::None),
            Some("injection_failed") => (TextErrorKind::NotDelivered, ChunkDelivery::None),
            Some("partial_delivery") => (TextErrorKind::Partial, ChunkDelivery::Unknown),
            Some("cleanup_failed") => (TextErrorKind::CleanupFailed, ChunkDelivery::All),
            // An unknown code proves nothing about what was typed.
            _ => (TextErrorKind::Protocol, ChunkDelivery::Unknown),
        };
        return Err((
            kind,
            format!("Guest: {}", guest_words(&error["message"])),
            delivery,
        ));
    }
    let result = &body["result"];
    if result["injected_scalars"].as_u64() != Some(chunk.chars().count() as u64)
        || result["injected_bytes"].as_u64() != Some(chunk.len() as u64)
        // The Guest keeps its temporary key bindings until the focused client
        // has had time to resolve the events, then restores them itself.
        || result["keymap_restore"] != "deferred"
        || !result["backend"].is_string()
    {
        return Err((
            TextErrorKind::Protocol,
            "Guest text reply does not account for the whole chunk".into(),
            ChunkDelivery::Unknown,
        ));
    }
    Ok(())
}
/// Delivers a plan chunk by chunk through `send(index, chunk)`, which returns the Guest reply
/// for that chunk. Stops at the first failure: no retry, no skipped or substituted character.
/// Success means the Guest X server acknowledged every key event; it is not application
/// acceptance, which only an observer inside the Guest can show.
pub fn deliver_text(
    plan: &TextPlan,
    guest_info: &Value,
    mut send: impl FnMut(usize, &str) -> Result<Value>,
) -> std::result::Result<Value, TextError> {
    let backend = guest_text_capability(guest_info).map_err(|mut error| {
        error.total_scalars = plan.scalars;
        error
    })?;
    let mut delivered = 0;
    for (index, chunk) in plan.chunks.iter().enumerate() {
        let scalars = chunk.chars().count();
        let outcome = match send(index, chunk) {
            Ok(reply) => guest_text_result(&reply, chunk),
            // The request may have reached the Guest before the transport failed.
            Err(error) => Err((
                TextErrorKind::Protocol,
                format!(
                    "text chunk transport failed: {}",
                    guest_words(&json!(error.to_string()))
                ),
                ChunkDelivery::Unknown,
            )),
        };
        match outcome {
            Ok(()) => delivered += scalars,
            Err((kind, message, delivery)) => {
                return Err(TextError {
                    kind,
                    message,
                    delivered_scalars: delivered
                        + if delivery == ChunkDelivery::All {
                            scalars
                        } else {
                            0
                        },
                    uncertain_scalars: if delivery == ChunkDelivery::Unknown {
                        scalars
                    } else {
                        0
                    },
                    total_scalars: plan.scalars,
                })
            }
        }
    }
    Ok(
        json!({"dispatched":true,"chunks":plan.chunks.len(),"scalars":plan.scalars,"bytes":plan.bytes,"backend":backend,
            "guest_delivery_confirmed":false,
            "note":"The Guest X server acknowledged every key event. Application acceptance is not observed; verify with a screenshot."}),
    )
}
pub struct Observation {
    value: Value,
    /// The console draw sequence of the captured frame, when the capture reports one.
    source_sequence: Option<u64>,
    observed: Instant,
}
#[derive(Default)]
pub struct Frames {
    values: VecDeque<Observation>,
}
impl Frames {
    pub fn record(&mut self, value: &Value, now: Instant) {
        self.values.push_back(Observation{value:json!({"frame_id":value["frame_id"],"width":value["width"],"height":value["height"],"source_generation":value["source_generation"]}),source_sequence:value["source_sequence"].as_u64(),observed:now});
        while self.values.len() > 16 {
            self.values.pop_front();
        }
    }
    pub fn check(&self, args: &Value, current: &Value, now: Instant) -> Result<()> {
        let frame = self
            .values
            .iter()
            .find(|v| v.value["frame_id"] == args["frame_id"])
            .context("unknown frame_id for this VM instance")?;
        // Freshness is checked when an operation begins; its later steps run under the
        // operation's own 4 s deadline and may follow redraws caused by its first steps.
        // A frame older than FRAME_TTL still binds while the console has drawn nothing
        // since it was captured: the caller saw exactly what is on screen now.
        let unchanged = frame
            .source_sequence
            .is_some_and(|sequence| current["source_sequence"].as_u64() == Some(sequence));
        if args["begin"] != false && now.duration_since(frame.observed) > FRAME_TTL && !unchanged {
            bail!("frame observation expired and the screen changed since; take a new screenshot")
        }
        if current["capture_connected"] != true || current["frame_available"] != true {
            bail!("console frame unavailable: {current}")
        }
        for field in ["source_generation", "width", "height"] {
            if !args[field].is_u64()
                || args[field] != frame.value[field]
                || args[field] != current[field]
            {
                bail!("frame geometry or generation changed: {field}")
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn point_input_dispatch_is_shared_by_macos_and_qemu_linux() {
        let temp = tempfile::tempdir().unwrap();
        let unused = temp.path().join("unused");
        let mut manager = crate::Manager::load(
            crate::test_private_state(temp.path()),
            unused.clone(),
            crate::MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        manager.qemu = Some(crate::QemuRegistration {
            worker: unused.clone(),
            launcher: unused.clone(),
            library: unused.clone(),
            resources: temp.path().into(),
            spice_library: None,
            screenshot_helper: None,
            console_broker: Some(unused.clone()),
            console_helper: Some(unused),
            venus_runtime: None,
        });
        for (id, backend, guest) in [
            ("mac", "apple-virtualization", "macos"),
            ("linux", "qemu-hvf", "linux"),
        ] {
            manager.configs.insert(
                id.into(),
                serde_json::from_value(json!({"id":id,"backend":backend,"guest":guest,
                    "cpus":1,"memory_mib":512}))
                .unwrap(),
            );
            for method in ["input_click", "input_drag", "input_scroll"] {
                let legacy = json!({"id":id,
                    "frame_id":"00000000-0000-0000-0000-000000000000",
                    "width":2560,"height":1440,"source_generation":4,
                    "x":400,"y":300,"end_x":800,"end_y":600,"direction":"down","ticks":3});
                for points in [false, true] {
                    let mut args = legacy.clone();
                    if points {
                        args["coordinate_space"] = json!("points");
                        args["point_pixel_scale"] = json!(2.0);
                        for field in ["x", "y", "end_x", "end_y"] {
                            // Only drag consumes end positions.
                            if method == "input_drag" || !field.starts_with("end_") {
                                args[field] = json!(args[field].as_u64().unwrap() / 2);
                            }
                        }
                    }
                    let mut deferred = None;
                    manager
                        .dispatch(json!({"method":method,"arguments":args}), &mut deferred)
                        .unwrap();
                    let Some(crate::Completion::Input {
                        arguments,
                        method: dispatched,
                        ..
                    }) = deferred
                    else {
                        panic!("expected deferred input for {id} {method}");
                    };
                    assert_eq!(dispatched, method);
                    assert_eq!(
                        serde_json::to_vec(&arguments).unwrap(),
                        serde_json::to_vec(&legacy).unwrap()
                    );
                    assert_eq!(
                        plan(method, &arguments).unwrap(),
                        plan(method, &legacy).unwrap()
                    );
                }
            }
        }
        // This exercises preparation only: no worker, socket, Guest, or input delivery.
        assert!(manager.running.is_empty());
    }
    #[test]
    fn point_input_tool_schemas_preserve_pixel_contract() {
        let tools = crate::tools_with_input(false, false, false, true);
        for tool in tools["tools"].as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            let schema = &tool["inputSchema"];
            let properties = &schema["properties"];
            if ["vm_input_click", "vm_input_drag", "vm_input_scroll"].contains(&name) {
                assert_eq!(schema["additionalProperties"], false);
                assert_eq!(
                    properties["coordinate_space"]["enum"],
                    json!(["source_pixels", "points"])
                );
                assert_eq!(properties["coordinate_space"]["default"], "source_pixels");
                assert_eq!(properties["point_pixel_scale"]["exclusiveMinimum"], 0);
                assert_eq!(properties["point_pixel_scale"]["maximum"], 8);
                assert_eq!(
                    schema["allOf"][0]["if"],
                    json!({"properties":{"coordinate_space":{"const":"points"}},"required":["coordinate_space"]})
                );
                assert_eq!(
                    schema["allOf"][0]["then"]["required"],
                    json!(["point_pixel_scale"])
                );
                assert_eq!(
                    schema["allOf"][0]["else"]["not"],
                    json!({"required":["point_pixel_scale"]})
                );
                for field in ["x", "y", "end_x", "end_y"] {
                    if properties.get(field).is_some() {
                        assert_eq!(properties[field]["type"], "number");
                        assert_eq!(
                            schema["allOf"][0]["else"]["properties"][field]["type"],
                            "integer"
                        );
                    }
                }
                for field in [
                    "id",
                    "frame_id",
                    "width",
                    "height",
                    "source_generation",
                    "x",
                    "y",
                ] {
                    assert!(schema["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(field)));
                }
            } else {
                assert!(properties.get("coordinate_space").is_none());
                assert!(properties.get("point_pixel_scale").is_none());
            }
        }
    }
    #[test]
    fn pixel_centres_and_bounds() {
        assert_eq!(physical(1279, 1280).unwrap(), 32755);
        assert_eq!(physical(200, 1280).unwrap(), 5132);
        assert!(physical(1280, 1280).is_err());
        assert!(physical(u64::MAX, 1280).is_err());
        assert!(physical(0, u64::MAX).is_err());
    }
    #[test]
    fn pixel_centres_at_1x_and_2x_require_source_pixels() {
        assert_eq!(
            point(200, 150, 1280, 720).unwrap(),
            vec![
                json!({"type":"abs","data":{"axis":"x","value":5132}}),
                json!({"type":"abs","data":{"axis":"y","value":6849}}),
            ]
        );
        // At 2x, the caller must convert (200,150) points to (400,300) pixels.
        assert_eq!(
            point(400, 300, 2560, 1440).unwrap(),
            vec![
                json!({"type":"abs","data":{"axis":"x","value":5126}}),
                json!({"type":"abs","data":{"axis":"y","value":6838}}),
            ]
        );
        assert_eq!(
            point(200, 150, 2560, 1440).unwrap(),
            vec![
                json!({"type":"abs","data":{"axis":"x","value":2566}}),
                json!({"type":"abs","data":{"axis":"y","value":3424}}),
            ]
        );
    }
    #[test]
    fn frame_binding_retains_only_pixel_geometry_and_generation() {
        let now = Instant::now();
        let screenshot = json!({"frame_id":"a","width":2560,"height":1440,
            "source_generation":4,"point_pixel_scale":2,
            "coordinate_space":"guest-display-physical-pixels",
            "console_identity":{"display_id":1}});
        let mut frames = Frames::default();
        frames.record(&screenshot, now);
        // This Rust cache is not the macOS Guest's separate console identity fence.
        assert_eq!(
            frames.values[0].value,
            json!({"frame_id":"a","width":2560,"height":1440,"source_generation":4})
        );
    }
    #[test]
    fn frame_vm_generation_geometry_and_age() {
        let now = Instant::now();
        let a = json!({"frame_id":"a","width":1280,"height":800,"source_generation":4});
        let current = json!({"capture_connected":true,"frame_available":true,"width":1280,"height":800,"source_generation":4});
        let mut frames = Frames::default();
        frames.record(&a, now);
        assert!(frames.check(&a, &current, now).is_ok());
        assert!(Frames::default().check(&a, &current, now).is_err());
        assert!(frames
            .check(&a, &current, now + FRAME_TTL + Duration::from_millis(1))
            .is_err());
        for field in ["width", "height", "source_generation"] {
            let mut c = current.clone();
            c[field] = json!(999);
            assert!(frames.check(&a, &c, now).is_err());
        }
        let mut c = current;
        c["frame_available"] = json!(false);
        assert!(frames.check(&a, &c, now).is_err());
    }
    #[test]
    fn aged_frame_binds_while_the_console_has_not_drawn() {
        let now = Instant::now();
        let late = now + FRAME_TTL * 6;
        let shot = json!({"frame_id":"a","width":1280,"height":800,"source_generation":4,"source_sequence":77});
        let args =
            json!({"frame_id":"a","width":1280,"height":800,"source_generation":4,"begin":true});
        let current = json!({"capture_connected":true,"frame_available":true,"width":1280,"height":800,"source_generation":4,"source_sequence":77});
        let mut frames = Frames::default();
        frames.record(&shot, now);
        assert!(frames.check(&args, &current, late).is_ok());
        // One draw since the capture: the caller may not have seen the screen.
        let mut drawn = current.clone();
        drawn["source_sequence"] = json!(78);
        let error = frames.check(&args, &drawn, late).unwrap_err().to_string();
        assert!(error.contains("expired"), "{error}");
        assert!(frames.check(&args, &drawn, now).is_ok());
        // Later steps of an admitted operation follow its own redraws.
        let mut step = args.clone();
        step["begin"] = json!(false);
        assert!(frames.check(&step, &drawn, late).is_ok());
        // Geometry still fences every step.
        let mut resized = drawn;
        resized["width"] = json!(1920);
        assert!(frames.check(&step, &resized, late).is_err());
        // A capture without a sequence never outlives FRAME_TTL.
        let mut frames = Frames::default();
        frames.record(
            &json!({"frame_id":"a","width":1280,"height":800,"source_generation":4}),
            now,
        );
        assert!(frames.check(&args, &current, late).is_err());
    }
    #[test]
    fn punctuation_plans_and_native_events_preserve_physical_edges() {
        use crate::native_input::{DisplayedFrame, Event};
        let frame = DisplayedFrame {
            display_instance: "a".repeat(32),
            presentation_serial: 1,
            window_epoch: 1,
            invalidation_epoch: 0,
            source_generation: 4,
            source_sequence: 1,
            width: 1280,
            height: 800,
        };
        for qcode in [
            "minus",
            "equal",
            "bracket_left",
            "bracket_right",
            "semicolon",
            "apostrophe",
            "grave_accent",
            "backslash",
            "comma",
            "dot",
            "slash",
        ] {
            let args = json!({"frame_id":"00000000-0000-0000-0000-000000000000",
                "width":1280,"height":800,"source_generation":4,"keys":["shift",qcode]});
            let steps = plan("input_key", &args).unwrap();
            assert_eq!(
                steps,
                vec![
                    vec![key("shift", true), key(qcode, true)],
                    vec![key(qcode, false), key("shift", false)],
                ],
                "{qcode}"
            );
            for down in [true, false] {
                let event: Event = serde_json::from_value(json!({
                    "kind":"key", "qcode":qcode, "down":down,
                }))
                .unwrap();
                assert_eq!(event.qmp(&frame).unwrap(), vec![key(qcode, down)]);
            }
        }
        for raw in [
            "-", "=", "[", "]", ";", "'", "`", "\\", ",", ".", "/", "?", ":", "SLASH", "period",
            "unknown", "你好", "／",
        ] {
            let args = json!({"frame_id":"00000000-0000-0000-0000-000000000000",
                "width":1280,"height":800,"source_generation":4,"keys":[raw]});
            assert!(plan("input_key", &args).is_err(), "{raw}");
            for down in [true, false] {
                let event = Event::Key {
                    qcode: raw.into(),
                    down,
                };
                assert!(event.qmp(&frame).is_err(), "{raw}");
            }
        }
        // Capability enumeration stays a unique list; no raw-text aliases.
        let names = key_names();
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(names.len(), unique.len());
    }
    #[test]
    fn plans_are_bounded_and_reject_text() {
        let mut a = json!({"frame_id":"00000000-0000-0000-0000-000000000000","width":1280,"height":800,"source_generation":4,"x":0,"y":0,"end_x":1279,"end_y":799,"keys":["ctrl","a"],"direction":"down","ticks":20});
        assert_eq!(plan("input_drag", &a).unwrap().len(), 23);
        assert_eq!(plan("input_scroll", &a).unwrap().len(), 41);
        assert_eq!(plan("input_key", &a).unwrap().len(), 2);
        a["keys"] = json!(["你好"]);
        assert!(plan("input_key", &a).is_err());
        a["ticks"] = json!(21);
        assert!(plan("input_scroll", &a).is_err());
    }
    #[test]
    fn key_hold_is_bounded_and_key_only() {
        let mut a = json!({"frame_id":"00000000-0000-0000-0000-000000000000","width":1280,"height":800,"source_generation":4,"x":0,"y":0,"keys":["up"]});
        assert_eq!(first_gap_ms("input_key", &a).unwrap(), 20);
        a["hold_ms"] = json!(0);
        assert_eq!(first_gap_ms("input_key", &a).unwrap(), 20);
        a["hold_ms"] = json!(MAX_HOLD_MS);
        assert_eq!(first_gap_ms("input_key", &a).unwrap(), MAX_HOLD_MS);
        assert_eq!(plan("input_key", &a).unwrap().len(), 2);
        assert!(plan("input_click", &a).is_err());
        for bad in [json!(MAX_HOLD_MS + 1), json!(-1), json!(1.5), json!("500")] {
            a["hold_ms"] = bad;
            assert!(plan("input_key", &a).is_err());
        }
    }

    const FRAME: &str = "00000000-0000-0000-0000-000000000000";
    const MIXED: &str = "你好e\u{301}😀"; // CJK, combining acute, emoji outside the BMP
    fn text_args(text: &str) -> Value {
        json!({"frame_id":FRAME,"width":1280,"height":800,"source_generation":4,"text":text})
    }
    fn available() -> Value {
        json!({"id":"1","result":{"capabilities":{"text_input":true,"desktop":false},
            "text_input":{"enabled":true,"available":true,"backend":"x11-xtest-keymap",
                "max_scalars_per_request":16,"max_bytes_per_request":64}}})
    }
    fn accepted(chunk: &str) -> Value {
        json!({"id":"t","result":{"injected_scalars":chunk.chars().count(),"injected_bytes":chunk.len(),
            "backend":"x11-xtest-keymap","keymap_restore":"deferred","guest_delivery_confirmed":false}})
    }
    #[test]
    fn text_policy_accepts_unicode_and_rejects_controls_and_noncharacters() {
        for good in [
            "a",
            MIXED,
            "line\n\ttab",
            "\u{200d}\u{fe0f}",
            "\u{fffd}",
            "\u{10fffd}",
            "\u{e000}",
            "\u{a0}",
            " ~",
        ] {
            assert_eq!(
                validate_text(good).unwrap(),
                good.chars().count(),
                "{good:?}"
            );
            assert_eq!(validate_text_bytes(good.as_bytes()).unwrap(), good);
        }
        for bad in [
            "",
            "\0",
            "\r",
            "a\r\n",
            "\u{1b}[A",
            "\u{7f}",
            "\u{85}",
            "\u{9f}",
            "\u{fffe}",
            "\u{ffff}",
            "\u{fdd0}",
            "\u{fdef}",
            "\u{1ffff}",
            "\u{10ffff}",
            "secret\u{8}",
        ] {
            let error = validate_text(bad).unwrap_err();
            assert_eq!(error.kind, TextErrorKind::Invalid, "{bad:?}");
            assert!(
                !error.to_string().contains("secret"),
                "error echoes the text"
            );
        }
        assert!(validate_text("\r").unwrap_err().message.contains("U+000A"));
        // Ill-formed UTF-8: encoded surrogate, overlong, above U+10FFFF, truncated.
        for bytes in [
            &[0xed, 0xa0, 0x80][..],
            &[0xc0, 0x80],
            &[0xf4, 0x90, 0x80, 0x80],
            &[0xe4, 0xbd],
            &[0x61, 0xff],
        ] {
            assert_eq!(
                validate_text_bytes(bytes).unwrap_err().kind,
                TextErrorKind::Invalid
            );
        }
        // A lone surrogate escape never becomes a string: JSON decoding refuses it first.
        assert!(serde_json::from_str::<Value>(r#"{"text":"\ud800"}"#).is_err());
    }
    #[test]
    fn text_bounds_are_enforced_before_planning() {
        let emoji = "😀".repeat(TEXT_MAX_SCALARS);
        assert_eq!(emoji.len(), TEXT_MAX_BYTES);
        let plan = plan_text(&text_args(&emoji)).unwrap();
        assert_eq!(
            (plan.scalars, plan.bytes, plan.chunks.len()),
            (256, 1024, 16)
        );
        assert!(plan_text(&text_args(&"😀".repeat(TEXT_MAX_SCALARS + 1))).is_err());
        assert!(plan_text(&text_args(&"a".repeat(TEXT_MAX_SCALARS + 1))).is_err());
        assert_eq!(
            plan_text(&text_args(&"a".repeat(TEXT_MAX_SCALARS)))
                .unwrap()
                .chunks
                .len(),
            16
        );
        for (key, value) in [
            ("text", json!(7)),
            ("text", Value::Null),
            ("text", json!(["a"])),
            ("text", json!("")),
            ("frame_id", json!("short")),
            ("width", json!(0)),
            ("height", json!(9000)),
            ("source_generation", json!("4")),
        ] {
            let mut args = text_args("a");
            args[key] = value;
            assert!(plan_text(&args).is_err(), "{key}");
        }
        let error = plan_text(&text_args("a\u{7}")).unwrap_err();
        assert_eq!(
            error.downcast_ref::<TextError>().unwrap().kind,
            TextErrorKind::Invalid
        );
    }
    #[test]
    fn text_chunks_are_scalar_aligned_and_byte_identical() {
        let long = format!("{MIXED}\n{}", "é😀\u{301}語a".repeat(40));
        for text in [
            "a".to_string(),
            MIXED.to_string(),
            "a".repeat(16),
            "a".repeat(17),
            "😀".repeat(33),
            long,
        ] {
            let chunks = text_chunks(&text);
            assert_eq!(chunks.concat(), text);
            assert_eq!(chunks.concat().as_bytes(), text.as_bytes());
            assert_eq!(
                chunks.len(),
                text.chars().count().div_ceil(TEXT_CHUNK_SCALARS)
            );
            for (index, chunk) in chunks.iter().enumerate() {
                let n = chunk.chars().count();
                assert!(
                    (1..=TEXT_CHUNK_SCALARS).contains(&n) && chunk.len() <= 4 * TEXT_CHUNK_SCALARS
                );
                assert!(index + 1 == chunks.len() || n == TEXT_CHUNK_SCALARS);
            }
        }
        assert!(text_chunks("").is_empty());
        assert_eq!(text_chunks(MIXED), vec![MIXED]);
    }
    #[test]
    fn physical_key_path_is_unchanged_by_text_input() {
        let args = json!({"frame_id":FRAME,"width":1280,"height":800,"source_generation":4,
            "keys":["shift","a"],"text":"a"});
        assert_eq!(
            plan("input_key", &args).unwrap(),
            vec![
                vec![key("shift", true), key("a", true)],
                vec![key("a", false), key("shift", false)],
            ]
        );
        // Text never becomes key codes, and physical planning never accepts a text method.
        assert!(plan("input_text", &args).is_err());
        assert!(plan(TEXT_STEP_METHOD, &args).is_err());
        assert_eq!(key_names().len(), 82);
        assert!(!key_allowed("text") && !key_allowed("你"));
        let plan = plan_text(&text_args("abc XYZ 123")).unwrap();
        assert_eq!(plan.chunks, vec!["abc XYZ 123"]);
    }
    #[test]
    fn guest_text_request_is_exact() {
        let request = guest_text_request("text-1", MIXED).unwrap();
        assert_eq!(
            request,
            json!({"id":"text-1","method":"text_input","text_hex":"e4bda0e5a5bd65cc81f09f9880"})
        );
        assert!(guest_text_request("", "a").is_err());
        assert!(guest_text_request(&"i".repeat(129), "a").is_err());
        assert!(guest_text_request("i", "").is_err());
        assert!(guest_text_request("i", "\u{1b}").is_err());
        assert!(guest_text_request("i", &"a".repeat(TEXT_CHUNK_SCALARS + 1)).is_err());
    }
    #[test]
    fn missing_guest_text_service_is_unsupported_and_sends_nothing() {
        let plan = plan_text(&text_args(MIXED)).unwrap();
        let old_service = json!({"id":"1","result":{"capabilities":{"exec":true,"desktop":false}}});
        let disabled = json!({"capabilities":{"text_input":false},
            "text_input":{"enabled":false,"available":false,"reason":"text input disabled at guest service launch (--allow-text-input)"}});
        let no_display = json!({"result":{"id":"1","result":{"capabilities":{"text_input":false},
            "text_input":{"enabled":true,"available":false,"reason":"X display socket not reachable\u{1b}[31m"}}}});
        let mut claims_only = available();
        claims_only["result"]["text_input"]["available"] = json!(false);
        let mut small = available();
        small["result"]["text_input"]["max_scalars_per_request"] = json!(8);
        let mut numeric = available();
        numeric["result"]["capabilities"]["text_input"] = json!(1);
        for (info, words) in [
            (old_service, "does not implement text input"),
            (disabled, "--allow-text-input"),
            (no_display, "X display socket not reachable[31m"),
            (claims_only, "no reason given"),
            (small, "incompatible limits"),
            (numeric, "unavailable"),
            (Value::Null, "does not implement text input"),
        ] {
            let mut sent = 0;
            let error = deliver_text(&plan, &info, |_, _| {
                sent += 1;
                Ok(Value::Null)
            })
            .unwrap_err();
            assert_eq!(error.kind, TextErrorKind::Unsupported, "{info}");
            assert!(error.to_string().starts_with("unsupported: "), "{error}");
            assert!(error.message.contains(words), "{error}");
            assert!(error.message.is_ascii() && !error.message.contains('\u{1b}'));
            assert_eq!(
                (
                    sent,
                    error.delivered_scalars,
                    error.uncertain_scalars,
                    error.total_scalars
                ),
                (0, 0, 0, 5)
            );
        }
        assert_eq!(
            guest_text_capability(&available()).unwrap(),
            "x11-xtest-keymap"
        );
    }
    #[test]
    fn delivery_sends_every_chunk_in_order_and_reports_exact_totals() {
        let text = format!("{MIXED}{}", "語".repeat(40));
        let plan = plan_text(&text_args(&text)).unwrap();
        let mut seen = Vec::new();
        let result = deliver_text(&plan, &available(), |index, chunk| {
            seen.push((index, chunk.to_string()));
            // Both reply shapes: the Guest response, and the same wrapped by the worker.
            Ok(if index % 2 == 0 {
                accepted(chunk)
            } else {
                json!({"id":9,"result":accepted(chunk)})
            })
        })
        .unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen.iter().enumerate().all(|(i, (index, _))| i == *index));
        assert_eq!(
            seen.iter()
                .map(|(_, c)| c.as_str())
                .collect::<String>()
                .as_bytes(),
            text.as_bytes()
        );
        assert_eq!(
            result,
            json!({"dispatched":true,"chunks":3,"scalars":45,"bytes":text.len(),"backend":"x11-xtest-keymap",
                "guest_delivery_confirmed":false,"note":result["note"]})
        );
    }
    #[test]
    fn guest_errors_map_to_explicit_kinds_with_delivery_accounting() {
        let text = "語".repeat(40); // chunks of 16, 16, 8
        let plan = plan_text(&text_args(&text)).unwrap();
        let failure =
            |code: &str| json!({"id":"t","error":{"code":code,"message":"detail\n\u{1b}x"}});
        for (code, kind, delivered, uncertain) in [
            ("unsupported", TextErrorKind::Unsupported, 16, 0),
            ("invalid_request", TextErrorKind::Invalid, 16, 0),
            ("busy", TextErrorKind::Busy, 16, 0),
            ("not_ready", TextErrorKind::NotReady, 16, 0),
            ("injection_failed", TextErrorKind::NotDelivered, 16, 0),
            ("partial_delivery", TextErrorKind::Partial, 16, 16),
            ("cleanup_failed", TextErrorKind::CleanupFailed, 32, 0),
            ("forbidden", TextErrorKind::Protocol, 16, 16),
            ("containment_failed", TextErrorKind::Protocol, 16, 16),
        ] {
            for wrapped in [false, true] {
                let mut calls = 0;
                let error = deliver_text(&plan, &available(), |index, chunk| {
                    calls += 1;
                    let reply = if index == 1 {
                        failure(code)
                    } else {
                        accepted(chunk)
                    };
                    Ok(if wrapped {
                        json!({"id":3,"result":reply})
                    } else {
                        reply
                    })
                })
                .unwrap_err();
                assert_eq!(calls, 2, "{code}: delivery continued after a failure");
                assert_eq!(error.kind, kind, "{code}");
                assert_eq!(
                    (
                        error.delivered_scalars,
                        error.uncertain_scalars,
                        error.total_scalars
                    ),
                    (delivered, uncertain, 40),
                    "{code}"
                );
                assert_eq!(error.message, "Guest: detailx");
                assert!(error.to_string().starts_with(kind.code()));
                assert_eq!(error.to_json()["code"], kind.code());
            }
        }
        assert_eq!(
            deliver_text(&plan, &available(), |_, _| Ok(
                json!({"id":"t","error":{"code":"partial_delivery","message":"m"}})
            ))
            .unwrap_err()
            .to_string(),
            "partial_delivery: Guest: m (0 of 40 characters delivered, up to 16 more unconfirmed; the remainder was not sent)"
        );
    }
    #[test]
    fn host_refusals_are_definite_and_guest_replies_are_reduced_to_counts() {
        let plan = plan_text(&text_args(&"ab".repeat(16))).unwrap();
        assert_eq!(plan.chunks.len(), 2);
        for (message, kind) in [
            ("backend: serial execution busy", TextErrorKind::Busy),
            (
                "backend: screenshot frame expired",
                TextErrorKind::NotDelivered,
            ),
            ("unsupported: no transport", TextErrorKind::Unsupported),
        ] {
            // Refused on the second chunk: the first is delivered, nothing is uncertain.
            let error = deliver_text(&plan, &available(), |index, chunk| {
                Ok(if index == 0 {
                    accepted(chunk)
                } else {
                    host_refusal(message)
                })
            })
            .unwrap_err();
            assert_eq!(error.kind, kind, "{message}");
            assert_eq!((error.delivered_scalars, error.uncertain_scalars), (16, 0));
            assert!(error.message.starts_with("not sent: "), "{}", error.message);
        }
        // A Guest cannot produce a host refusal: its reply is reduced first.
        let forged = json!({"id":"t","result":{"host_refusal":"x"},"host_refusal":"x"});
        assert_eq!(guest_text_summary(&forged), json!({"result":{}}));
        let mut noisy = accepted(MIXED);
        noisy["result"]["bindings"] =
            json!([{"keycode":255,"keysym":0x0100_4f60,"readback":[0x0100_4f60],"reused":false}]);
        noisy["result"]["text"] = json!(MIXED);
        let summary = guest_text_summary(&noisy);
        assert_eq!(
            summary,
            json!({"result":{"injected_scalars":5,"injected_bytes":13,"backend":"x11-xtest-keymap","keymap_restore":"deferred","guest_delivery_confirmed":false}})
        );
        assert!(deliver_text(
            &plan_text(&text_args(MIXED)).unwrap(),
            &available(),
            |_, _| Ok(summary.clone())
        )
        .is_ok());
        // Wrapped as the worker forwards it, and with a hostile error message.
        let refused = guest_text_summary(
            &json!({"result":{"id":"t","error":{"code":"not_ready","message":"no focus\u{7}\u{4f60}","text":MIXED}}}),
        );
        assert_eq!(
            refused,
            json!({"error":{"code":"not_ready","message":"no focus"}})
        );
        let error = deliver_text(
            &plan_text(&text_args(MIXED)).unwrap(),
            &available(),
            |_, _| Ok(refused.clone()),
        )
        .unwrap_err();
        assert_eq!(error.kind, TextErrorKind::NotReady);
        assert_eq!((error.delivered_scalars, error.uncertain_scalars), (0, 0));
    }
    #[test]
    fn malformed_guest_replies_and_transport_errors_are_never_success() {
        let plan = plan_text(&text_args(MIXED)).unwrap();
        let mut short = accepted(MIXED);
        short["result"]["injected_scalars"] = json!(4);
        let mut bytes = accepted(MIXED);
        bytes["result"]["injected_bytes"] = json!(12);
        let mut dirty = accepted(MIXED);
        dirty["result"]["keymap_restore"] = json!("failed");
        let mut legacy = accepted(MIXED);
        legacy["result"]["keymap_restore"] = Value::Null;
        legacy["result"]["keymap_restored"] = json!(true);
        let mut text_count = accepted(MIXED);
        text_count["result"]["injected_scalars"] = json!("5");
        for reply in [
            Value::Null,
            json!({}),
            json!({"id":"t"}),
            json!({"id":"t","result":{}}),
            json!({"id":"t","result":{"dispatched":true}}),
            json!({"id":"t","error":"text"}),
            json!({"id":"t","error":{}}),
            short,
            bytes,
            dirty,
            legacy,
            text_count,
        ] {
            let error = deliver_text(&plan, &available(), |_, _| Ok(reply.clone())).unwrap_err();
            assert_eq!(error.kind, TextErrorKind::Protocol, "{reply}");
            assert_eq!((error.delivered_scalars, error.uncertain_scalars), (0, 5));
        }
        let error = deliver_text(&plan, &available(), |_, _| {
            bail!("serial execution busy\u{7}")
        })
        .unwrap_err();
        assert_eq!(error.kind, TextErrorKind::Protocol);
        assert_eq!(
            error.message,
            "text chunk transport failed: serial execution busy"
        );
        assert_eq!((error.delivered_scalars, error.uncertain_scalars), (0, 5));
    }
}
