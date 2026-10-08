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
}
