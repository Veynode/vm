//! Local native console input wire. No text injection, raw QMP or remote authority.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Fixed internal rejection telemetry. Never contains key, coordinates, owner or error text.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RejectionStage {
    Admission,
    BeginDisplay,
    Authority,
    FrameDecode,
    FrameInstance,
    EventValidation,
    DisplayValidationIo,
    DisplayValidationRejected,
    SourceStatusIo,
    SourceChanged,
    ValidationExpired,
    QmpDispatch,
    DisplayInvalidToken,
    DisplayNoSnapshot,
    DisplayInstanceMismatch,
    DisplayEpochMismatch,
    DisplayHistoryMiss,
    DisplayHistoricalExpired,
    DisplayRecordMismatch,
}
/// Only helper token-validation codes may become typed display rejections.
impl RejectionStage {
    pub fn display_rejection(value: &Value) -> Option<Self> {
        let stage: Self = serde_json::from_value(value.clone()).ok()?;
        match stage {
            Self::DisplayInvalidToken
            | Self::DisplayNoSnapshot
            | Self::DisplayInstanceMismatch
            | Self::DisplayEpochMismatch
            | Self::DisplayHistoryMiss
            | Self::DisplayHistoricalExpired
            | Self::DisplayRecordMismatch => Some(stage),
            _ => None,
        }
    }
}
impl std::fmt::Display for RejectionStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("native displayed frame rejected")
    }
}
impl std::error::Error for RejectionStage {}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RejectionDiagnostic {
    pub version: u8,
    pub stage: RejectionStage,
    pub elapsed_ms: u64,
}
impl RejectionDiagnostic {
    pub fn from_wire(value: &Value) -> Option<Self> {
        let value: Self = serde_json::from_value(value.clone()).ok()?;
        (value.version == 1 && value.elapsed_ms <= 60_000).then_some(value)
    }
}
impl std::fmt::Display for RejectionDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("native input operation rejected")
    }
}
impl std::error::Error for RejectionDiagnostic {}

/// Optional correlation only: never input authority or a displayed-frame token field.
/// A present null, float, boolean or out-of-range integer is invalid.
pub fn diagnostic_id(value: Option<&Value>) -> Result<Option<u64>> {
    match value {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|id| (1..=9_007_199_254_740_991).contains(id))
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("invalid native diagnostic identity")),
    }
}
pub fn deserialize_diagnostic_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<u64>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    diagnostic_id(Some(&value)).map_err(serde::de::Error::custom)
}

/// Presence selects a distinct eligibility protocol; null is never legacy.
pub fn frame_acquisition_protocol(value: Option<&Value>) -> Result<Option<u64>> {
    match value {
        None => Ok(None),
        Some(value) if value.as_u64() == Some(1) => Ok(Some(1)),
        _ => bail!("invalid frame acquisition protocol"),
    }
}
pub fn deserialize_frame_acquisition_protocol<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<u64>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    frame_acquisition_protocol(Some(&value)).map_err(serde::de::Error::custom)
}
pub fn acquisition_receipt(value: Option<&Value>) -> Result<Option<String>> {
    match value {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .filter(|s| hex(s, 32))
            .map(|s| Some(s.to_owned()))
            .ok_or_else(|| anyhow::anyhow!("invalid frame acquisition receipt")),
    }
}
pub fn deserialize_acquisition_receipt<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    acquisition_receipt(Some(&value)).map_err(serde::de::Error::custom)
}

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
    #[serde(default, deserialize_with = "deserialize_frame_acquisition_protocol")]
    pub frame_acquisition_protocol: Option<u64>,
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
        #[serde(default, deserialize_with = "deserialize_diagnostic_id")]
        diagnostic_id: Option<u64>,
        #[serde(default, deserialize_with = "deserialize_acquisition_receipt")]
        acquisition_receipt: Option<String>,
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

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    #[test]
    fn display_rejection_allows_only_fixed_helper_validation_codes() {
        for name in [
            "display_invalid_token",
            "display_no_snapshot",
            "display_instance_mismatch",
            "display_epoch_mismatch",
            "display_history_miss",
            "display_historical_expired",
            "display_record_mismatch",
        ] {
            let stage = RejectionStage::display_rejection(&json!(name)).unwrap();
            let value = json!({"version":1,"stage":stage,"elapsed_ms":23});
            assert!(RejectionDiagnostic::from_wire(&value).is_some());
        }
        for value in [
            json!("authority"),
            json!("display_validation_io"),
            json!("display_history_miss SECRET"),
            json!({"code":"display_history_miss"}),
            Value::Null,
        ] {
            assert!(RejectionStage::display_rejection(&value).is_none());
        }
    }
    #[test]
    fn diagnostic_has_strict_fixed_schema_and_bounds() {
        let valid = json!({"version":1,"stage":"qmp_dispatch","elapsed_ms":60000});
        assert!(RejectionDiagnostic::from_wire(&valid).is_some());
        for (key, value) in [
            ("version", json!(2)),
            ("elapsed_ms", json!(-1)),
            ("elapsed_ms", json!(1.5)),
            ("elapsed_ms", json!(true)),
            ("stage", json!("raw key / secret")),
        ] {
            let mut bad = valid.clone();
            bad[key] = value;
            assert!(RejectionDiagnostic::from_wire(&bad).is_none());
        }
        let mut bad = valid;
        bad["raw_error"] = json!("secret");
        assert!(RejectionDiagnostic::from_wire(&bad).is_none());
    }
}
