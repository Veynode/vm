//! Live desktop input from the local browser desktop (`veynodevm desktop --stream`).
//!
//! Events carry physical keys (`KeyboardEvent.code`), absolute pointer
//! positions in source pixels and wheel ticks; there is no text or layout
//! translation. QEMU injects them through QMP `input-send-event`; Apple
//! Virtualization Linux guests receive Linux evdev codes for a uinput device.
use crate::input::physical;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

pub const MAX_EVENTS: usize = 64;
const MAX_WHEEL_TICKS: i64 = 10;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Move { x: u64, y: u64, w: u64, h: u64 },
    Button { button: String, down: bool },
    Wheel { dx: i64, dy: i64 },
    Key { code: String, down: bool },
}

/// `KeyboardEvent.code` → (QEMU QKeyCode, Linux evdev code).
const KEYS: &[(&str, &str, u16)] = &[
    ("Escape", "esc", 1),
    ("Digit1", "1", 2),
    ("Digit2", "2", 3),
    ("Digit3", "3", 4),
    ("Digit4", "4", 5),
    ("Digit5", "5", 6),
    ("Digit6", "6", 7),
    ("Digit7", "7", 8),
    ("Digit8", "8", 9),
    ("Digit9", "9", 10),
    ("Digit0", "0", 11),
    ("Minus", "minus", 12),
    ("Equal", "equal", 13),
    ("Backspace", "backspace", 14),
    ("Tab", "tab", 15),
    ("KeyQ", "q", 16),
    ("KeyW", "w", 17),
    ("KeyE", "e", 18),
    ("KeyR", "r", 19),
    ("KeyT", "t", 20),
    ("KeyY", "y", 21),
    ("KeyU", "u", 22),
    ("KeyI", "i", 23),
    ("KeyO", "o", 24),
    ("KeyP", "p", 25),
    ("BracketLeft", "bracket_left", 26),
    ("BracketRight", "bracket_right", 27),
    ("Enter", "ret", 28),
    ("ControlLeft", "ctrl", 29),
    ("KeyA", "a", 30),
    ("KeyS", "s", 31),
    ("KeyD", "d", 32),
    ("KeyF", "f", 33),
    ("KeyG", "g", 34),
    ("KeyH", "h", 35),
    ("KeyJ", "j", 36),
    ("KeyK", "k", 37),
    ("KeyL", "l", 38),
    ("Semicolon", "semicolon", 39),
    ("Quote", "apostrophe", 40),
    ("Backquote", "grave_accent", 41),
    ("ShiftLeft", "shift", 42),
    ("Backslash", "backslash", 43),
    ("KeyZ", "z", 44),
    ("KeyX", "x", 45),
    ("KeyC", "c", 46),
    ("KeyV", "v", 47),
    ("KeyB", "b", 48),
    ("KeyN", "n", 49),
    ("KeyM", "m", 50),
    ("Comma", "comma", 51),
    ("Period", "dot", 52),
    ("Slash", "slash", 53),
    ("ShiftRight", "shift_r", 54),
    ("NumpadMultiply", "kp_multiply", 55),
    ("AltLeft", "alt", 56),
    ("Space", "spc", 57),
    ("CapsLock", "caps_lock", 58),
    ("F1", "f1", 59),
    ("F2", "f2", 60),
    ("F3", "f3", 61),
    ("F4", "f4", 62),
    ("F5", "f5", 63),
    ("F6", "f6", 64),
    ("F7", "f7", 65),
    ("F8", "f8", 66),
    ("F9", "f9", 67),
    ("F10", "f10", 68),
    ("NumLock", "num_lock", 69),
    ("ScrollLock", "scroll_lock", 70),
    ("Numpad7", "kp_7", 71),
    ("Numpad8", "kp_8", 72),
    ("Numpad9", "kp_9", 73),
    ("NumpadSubtract", "kp_subtract", 74),
    ("Numpad4", "kp_4", 75),
    ("Numpad5", "kp_5", 76),
    ("Numpad6", "kp_6", 77),
    ("NumpadAdd", "kp_add", 78),
    ("Numpad1", "kp_1", 79),
    ("Numpad2", "kp_2", 80),
    ("Numpad3", "kp_3", 81),
    ("Numpad0", "kp_0", 82),
    ("NumpadDecimal", "kp_decimal", 83),
    ("IntlBackslash", "less", 86),
    ("F11", "f11", 87),
    ("F12", "f12", 88),
    ("NumpadEnter", "kp_enter", 96),
    ("ControlRight", "ctrl_r", 97),
    ("NumpadDivide", "kp_divide", 98),
    ("PrintScreen", "sysrq", 99),
    ("AltRight", "alt_r", 100),
    ("Home", "home", 102),
    ("ArrowUp", "up", 103),
    ("PageUp", "pgup", 104),
    ("ArrowLeft", "left", 105),
    ("ArrowRight", "right", 106),
    ("End", "end", 107),
    ("ArrowDown", "down", 108),
    ("PageDown", "pgdn", 109),
    ("Insert", "insert", 110),
    ("Delete", "delete", 111),
    ("Pause", "pause", 119),
    ("MetaLeft", "meta_l", 125),
    ("MetaRight", "meta_r", 126),
    ("ContextMenu", "menu", 127),
];

fn key(code: &str) -> Result<(&'static str, u16)> {
    KEYS.iter()
        .find(|(name, _, _)| *name == code)
        .map(|(_, qcode, evdev)| (*qcode, *evdev))
        .with_context(|| format!("unsupported key code {code:?}"))
}

fn button(name: &str) -> Result<(&'static str, u16)> {
    Ok(match name {
        "left" => ("left", 0x110),
        "right" => ("right", 0x111),
        "middle" => ("middle", 0x112),
        _ => bail!("unsupported button"),
    })
}

/// Parse and validate one batch from the relay.
pub fn parse(events: &Value) -> Result<Vec<Event>> {
    let list = events.as_array().context("events must be an array")?;
    if list.is_empty() || list.len() > MAX_EVENTS {
        bail!("events must contain 1..{MAX_EVENTS} entries")
    }
    let events: Vec<Event> = list
        .iter()
        .map(|e| serde_json::from_value(e.clone()).context("invalid desktop event"))
        .collect::<Result<_>>()?;
    for event in &events {
        match event {
            Event::Move { x, y, w, h } => {
                physical(*x, *w)?;
                physical(*y, *h)?;
            }
            Event::Button { button: b, .. } => {
                button(b)?;
            }
            Event::Wheel { dx, dy } => {
                if dx.abs() > MAX_WHEEL_TICKS || dy.abs() > MAX_WHEEL_TICKS {
                    bail!("wheel ticks out of range")
                }
            }
            Event::Key { code, .. } => {
                key(code)?;
            }
        }
    }
    Ok(events)
}

/// QMP `input-send-event` events for a validated batch.
pub fn qmp(events: &[Event]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for event in events {
        match event {
            Event::Move { x, y, w, h } => {
                out.push(json!({"type":"abs","data":{"axis":"x","value":physical(*x,*w)?}}));
                out.push(json!({"type":"abs","data":{"axis":"y","value":physical(*y,*h)?}}));
            }
            Event::Button { button: b, down } => {
                out.push(json!({"type":"btn","data":{"button":button(b)?.0,"down":down}}))
            }
            Event::Wheel { dx, dy } => {
                for (ticks, negative, positive) in
                    [(*dy, "wheel-up", "wheel-down"), (*dx, "wheel-left", "wheel-right")]
                {
                    let name = if ticks < 0 { negative } else { positive };
                    for _ in 0..ticks.abs() {
                        out.push(json!({"type":"btn","data":{"button":name,"down":true}}));
                        out.push(json!({"type":"btn","data":{"button":name,"down":false}}));
                    }
                }
            }
            Event::Key { code, down } => out.push(
                json!({"type":"key","data":{"key":{"type":"qcode","data":key(code)?.0},"down":down}}),
            ),
        }
    }
    Ok(out)
}

/// Guest uinput events: absolute axes scaled to 0..32767 and evdev codes.
pub fn evdev(events: &[Event]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for event in events {
        out.push(match event {
            Event::Move { x, y, w, h } => {
                json!({"t":"abs","x":physical(*x,*w)?,"y":physical(*y,*h)?})
            }
            Event::Button { button: b, down } => {
                json!({"t":"key","code":button(b)?.1,"down":down})
            }
            Event::Wheel { dx, dy } => json!({"t":"wheel","dx":dx,"dy":dy}),
            Event::Key { code, down } => json!({"t":"key","code":key(code)?.1,"down":down}),
        });
    }
    Ok(out)
}

/// Planned MCP input (`input::plan`, QMP form) as guest uinput events for
/// Apple Virtualization Linux. A wheel button press is one wheel tick; its
/// release has no evdev counterpart and maps to nothing.
pub fn planned_evdev(events: &[Value]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut x = None;
    for event in events {
        let data = &event["data"];
        match event["type"].as_str() {
            Some("abs") => {
                let value = data["value"]
                    .as_u64()
                    .filter(|v| *v < 32768)
                    .context("invalid absolute axis value")?;
                match data["axis"].as_str() {
                    Some("x") if x.is_none() => x = Some(value),
                    Some("y") => {
                        let x = x.take().context("absolute y without x")?;
                        out.push(json!({"t":"abs","x":x,"y":value}));
                    }
                    _ => bail!("invalid absolute axis"),
                }
            }
            Some("btn") => {
                let down = data["down"].as_bool().context("button state required")?;
                let name = data["button"].as_str().context("button required")?;
                let wheel = match name {
                    "wheel-up" => Some((0, -1)),
                    "wheel-down" => Some((0, 1)),
                    "wheel-left" => Some((-1, 0)),
                    "wheel-right" => Some((1, 0)),
                    _ => None,
                };
                match wheel {
                    Some((dx, dy)) if down => out.push(json!({"t":"wheel","dx":dx,"dy":dy})),
                    Some(_) => {}
                    None => out.push(json!({"t":"key","code":button(name)?.1,"down":down})),
                }
            }
            Some("key") => {
                let down = data["down"].as_bool().context("key state required")?;
                let qcode = data["key"]["data"]
                    .as_str()
                    .filter(|_| data["key"]["type"] == "qcode")
                    .context("qcode key required")?;
                let code = KEYS
                    .iter()
                    .find(|(_, q, _)| *q == qcode)
                    .map(|(_, _, evdev)| *evdev)
                    .with_context(|| format!("no evdev code for qcode {qcode:?}"))?;
                out.push(json!({"t":"key","code":code,"down":down}));
            }
            _ => bail!("unsupported planned input event"),
        }
    }
    if x.is_some() {
        bail!("absolute x without y")
    }
    Ok(out)
}

/// Release events for whatever a session still holds down.
pub fn releases(held: &[Event]) -> Vec<Event> {
    held.iter()
        .rev()
        .filter_map(|event| match event {
            Event::Button { button, down: true } => Some(Event::Button {
                button: button.clone(),
                down: false,
            }),
            Event::Key { code, down: true } => Some(Event::Key {
                code: code.clone(),
                down: false,
            }),
            _ => None,
        })
        .collect()
}

/// Track held keys/buttons so a closed session never leaves one pressed.
#[derive(Default)]
pub struct Held(Vec<Event>);
impl Held {
    pub fn update(&mut self, events: &[Event]) {
        for event in events {
            let (pressed, down) = match event {
                Event::Button { button, down } => (
                    Event::Button {
                        button: button.clone(),
                        down: true,
                    },
                    *down,
                ),
                Event::Key { code, down } => (
                    Event::Key {
                        code: code.clone(),
                        down: true,
                    },
                    *down,
                ),
                _ => continue,
            };
            self.0.retain(|e| e != &pressed);
            if down {
                self.0.push(pressed);
            }
        }
    }
    pub fn releases(&mut self) -> Vec<Event> {
        let out = releases(&self.0);
        self.0.clear();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn batches_translate_to_qmp_and_evdev() {
        let events = parse(&json!([
            {"t":"move","x":640,"y":400,"w":1280,"h":800},
            {"t":"button","button":"left","down":true},
            {"t":"key","code":"KeyA","down":true},
            {"t":"wheel","dx":0,"dy":-2}
        ]))
        .unwrap();
        let q = qmp(&events).unwrap();
        assert_eq!(q[0]["data"]["value"], 16396);
        assert_eq!(q[2]["data"]["button"], "left");
        assert_eq!(q[3]["data"]["key"]["data"], "a");
        assert_eq!(q.len(), 4 + 4);
        assert_eq!(q[4]["data"]["button"], "wheel-up");
        let e = evdev(&events).unwrap();
        assert_eq!(e[1], json!({"t":"key","code":0x110,"down":true}));
        assert_eq!(e[2], json!({"t":"key","code":30,"down":true}));
    }
    #[test]
    fn planned_input_translates_to_evdev() {
        let args = json!({"x":640,"y":400,"width":1280,"height":800,"frame_id":"0".repeat(36),"source_generation":1,"direction":"up","ticks":1});
        let steps = crate::input::plan("input_scroll", &args).unwrap();
        let evdev: Vec<Vec<Value>> = steps.iter().map(|s| planned_evdev(s).unwrap()).collect();
        assert_eq!(
            evdev,
            vec![
                vec![json!({"t":"abs","x":16396,"y":16404})],
                vec![json!({"t":"wheel","dx":0,"dy":-1})],
                vec![]
            ]
        );
        let click = crate::input::plan("input_click", &json!({"x":0,"y":0,"width":10,"height":10,"frame_id":"0".repeat(36),"source_generation":1,"button":"right"})).unwrap();
        assert_eq!(
            planned_evdev(&click[1]).unwrap(),
            vec![json!({"t":"key","code":0x111,"down":true})]
        );
        // Every physical key MCP accepts has an evdev code.
        for name in crate::input::key_names() {
            let event = crate::input::key(&name, true);
            assert!(planned_evdev(&[event]).is_ok(), "{name}");
        }
        assert!(planned_evdev(&[json!({"type":"abs","data":{"axis":"x","value":1}})]).is_err());
        assert!(planned_evdev(&[json!({"type":"rel","data":{}})]).is_err());
    }
    #[test]
    fn invalid_batches_are_refused() {
        for bad in [
            json!([]),
            json!([{"t":"move","x":1280,"y":0,"w":1280,"h":800}]),
            json!([{"t":"key","code":"Fn","down":true}]),
            json!([{"t":"wheel","dx":0,"dy":11}]),
            json!([{"t":"button","button":"back","down":true}]),
            json!([{"t":"key","code":"KeyA","down":true,"extra":1}]),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn held_inputs_are_released_in_reverse() {
        let mut held = Held::default();
        held.update(
            &parse(&json!([
                {"t":"key","code":"ShiftLeft","down":true},
                {"t":"key","code":"KeyA","down":true},
                {"t":"key","code":"KeyA","down":false},
                {"t":"button","button":"left","down":true}
            ]))
            .unwrap(),
        );
        assert_eq!(
            held.releases(),
            vec![
                Event::Button {
                    button: "left".into(),
                    down: false
                },
                Event::Key {
                    code: "ShiftLeft".into(),
                    down: false
                }
            ]
        );
        assert!(held.releases().is_empty());
    }
}
