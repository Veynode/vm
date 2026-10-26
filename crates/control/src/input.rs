//! Bounded physical input plans. No text/layout translation or guest injection.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
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
pub fn plan(method: &str, args: &Value) -> Result<Vec<Vec<Value>>> {
    let w = args["width"].as_u64().context("width required")?;
    let h = args["height"].as_u64().context("height required")?;
    physical(0, w)?;
    physical(0, h)?;
    if args["frame_id"].as_str().is_none_or(|s| s.len() != 36)
        || args["source_generation"].as_u64().is_none()
    {
        bail!("screenshot frame_id and source_generation required")
    }
    first_gap_ms(method, args)?;
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
pub struct Observation {
    value: Value,
    observed: Instant,
}
#[derive(Default)]
pub struct Frames {
    values: VecDeque<Observation>,
}
impl Frames {
    pub fn record(&mut self, value: &Value, now: Instant) {
        self.values.push_back(Observation{value:json!({"frame_id":value["frame_id"],"width":value["width"],"height":value["height"],"source_generation":value["source_generation"]}),observed:now});
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
        if now.duration_since(frame.observed) > FRAME_TTL {
            bail!("frame observation expired; take a new screenshot")
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
    fn pixel_centres_and_bounds() {
        assert_eq!(physical(1279, 1280).unwrap(), 32755);
        assert_eq!(physical(200, 1280).unwrap(), 5132);
        assert!(physical(1280, 1280).is_err());
        assert!(physical(u64::MAX, 1280).is_err());
        assert!(physical(0, u64::MAX).is_err());
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
}
