//! Local native console input wire. No text injection, raw QMP or remote authority.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const PROTOCOL: u64 = 1;
pub const MAX_LINE: usize = 8192;
pub fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DisplayedFrame {
    pub display_instance: String,
    pub presentation_serial: u64,
    pub window_epoch: u64,
    pub invalidation_epoch: u64,
    pub source_generation: u64,
    pub source_sequence: u64,
    pub width: u32,
    pub height: u32,
}
impl DisplayedFrame {
    pub fn validate(&self) -> Result<()> {
        if !hex(&self.display_instance, 32)
            || self.presentation_serial == 0
            || self.window_epoch == 0
            || self.width == 0
            || self.height == 0
            || self.width > 3840
            || self.height > 2160
        {
            bail!("invalid displayed frame reference")
        }
        Ok(())
    }
    pub fn matches_record(&self, record: &Value) -> bool {
        serde_json::to_value(self)
            .unwrap()
            .as_object()
            .unwrap()
            .iter()
            .all(|(key, value)| {
                &record[match key.as_str() {
                    "width" => "source_width",
                    "height" => "source_height",
                    other => other,
                }] == value
            })
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Move {
        x: u32,
        y: u32,
    },
    Button {
        button: String,
        down: bool,
        x: u32,
        y: u32,
    },
    Key {
        qcode: String,
        down: bool,
    },
    Scroll {
        direction: String,
        ticks: u32,
        x: u32,
        y: u32,
    },
}
impl Event {
    pub fn qmp(&self, frame: &DisplayedFrame) -> Result<Vec<Value>> {
        frame.validate()?;
        let point = |x: u32, y: u32| -> Result<Vec<Value>> {
            Ok(vec![
                json!({"type":"abs","data":{"axis":"x","value":crate::input::physical(x as u64,frame.width as u64)?}}),
                json!({"type":"abs","data":{"axis":"y","value":crate::input::physical(y as u64,frame.height as u64)?}}),
            ])
        };
        match self {
            Self::Move { x, y } => point(*x, *y),
            Self::Button { button, down, x, y } => {
                if !["left", "middle", "right"].contains(&button.as_str()) {
                    bail!("unsupported button")
                }
                let mut events = point(*x, *y)?;
                events.push(crate::input::button(button, *down));
                Ok(events)
            }
            Self::Key { qcode, down } => {
                if !crate::input::key_allowed(qcode) {
                    bail!("unsupported physical key")
                }
                Ok(vec![crate::input::key(qcode, *down)])
            }
            Self::Scroll {
                direction,
                ticks,
                x,
                y,
            } => {
                let button = match direction.as_str() {
                    "up" => "wheel-up",
                    "down" => "wheel-down",
                    "left" => "wheel-left",
                    "right" => "wheel-right",
                    _ => bail!("unsupported scroll direction"),
                };
                if !(1..=20).contains(ticks) {
                    bail!("scroll ticks outside bound")
                }
                let mut events = point(*x, *y)?;
                for _ in 0..*ticks {
                    events.push(crate::input::button(button, true));
                    events.push(crate::input::button(button, false));
                }
                Ok(events)
            }
        }
    }
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attach {
    pub protocol_version: u64,
    pub id: String,
    pub worker_instance: String,
    pub display_instance: String,
}
#[derive(Debug, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    TakeControl {
        sequence: u64,
    },
    Heartbeat {
        sequence: u64,
        epoch: Option<u64>,
    },
    Event {
        sequence: u64,
        epoch: u64,
        frame: DisplayedFrame,
        event: Event,
    },
    Suspend {
        sequence: u64,
        epoch: u64,
    },
    ReturnAgent {
        sequence: u64,
        epoch: u64,
    },
    Detach {
        sequence: u64,
    },
}
impl Message {
    pub fn sequence(&self) -> u64 {
        match self {
            Self::TakeControl { sequence }
            | Self::Heartbeat { sequence, .. }
            | Self::Event { sequence, .. }
            | Self::Suspend { sequence, .. }
            | Self::ReturnAgent { sequence, .. }
            | Self::Detach { sequence } => *sequence,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn frame() -> DisplayedFrame {
        DisplayedFrame {
            display_instance: "a".repeat(32),
            presentation_serial: 1,
            window_epoch: 1,
            invalidation_epoch: 1,
            source_generation: 2,
            source_sequence: 4,
            width: 1280,
            height: 800,
        }
    }
    #[test]
    fn typed_events_preserve_edges_and_reject_raw_qmp_or_text() {
        let frame = frame();
        assert_eq!(
            Event::Move { x: 1279, y: 799 }.qmp(&frame).unwrap()[0]["data"]["value"],
            32755
        );
        assert!(Event::Move { x: 1280, y: 0 }.qmp(&frame).is_err());
        assert!(Event::Key {
            qcode: "unicode".into(),
            down: true
        }
        .qmp(&frame)
        .is_err());
        assert!(serde_json::from_value::<Event>(
            json!({"kind":"key","qcode":"ret","down":true,"raw":{}})
        )
        .is_err());
        assert!(serde_json::from_value::<Message>(
            json!({"method":"event","sequence":1,"epoch":1,"events":[]})
        )
        .is_err());
        assert_eq!(
            Event::Scroll {
                direction: "down".into(),
                ticks: 20,
                x: 1,
                y: 1
            }
            .qmp(&frame)
            .unwrap()
            .len(),
            42
        );
    }
}
