//! Domain-separated physical-console requests. No text injection, file paths or VM selection.
use super::{identifier, unhex, Canon, MAX_SAFE_INTEGER};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FrameAnchor {
    pub frame_id: String,
    pub source_generation: u64,
    pub width: u64,
    pub height: u64,
}
impl FrameAnchor {
    fn fields(&self, c: &mut Canon) -> Result<()> {
        ensure!(
            self.frame_id.len() == 36
                && self.frame_id.bytes().enumerate().all(
                    |(i, b)| if [8, 13, 18, 23].contains(&i) {
                        b == b'-'
                    } else {
                        b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
                    }
                ),
            "invalid screenshot frame id"
        );
        ensure!(
            self.source_generation <= MAX_SAFE_INTEGER
                && (1..=8192).contains(&self.width)
                && (1..=8192).contains(&self.height),
            "invalid frame geometry/generation"
        );
        c.text(&self.frame_id);
        c.int(self.source_generation);
        c.int(self.width);
        c.int(self.height);
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlAction {
    Acquire,
    Release {
        epoch: u64,
    },
    ObserveFrame,
    Click {
        epoch: u64,
        frame: FrameAnchor,
        x: u64,
        y: u64,
        button: String,
    },
    Drag {
        epoch: u64,
        frame: FrameAnchor,
        x: u64,
        y: u64,
        end_x: u64,
        end_y: u64,
        button: String,
    },
    Scroll {
        epoch: u64,
        frame: FrameAnchor,
        x: u64,
        y: u64,
        direction: String,
        ticks: u64,
    },
    Key {
        epoch: u64,
        frame: FrameAnchor,
        keys: Vec<String>,
    },
}
impl ControlAction {
    pub fn required_scope(&self) -> &'static str {
        if matches!(self, Self::ObserveFrame) {
            "observe"
        } else {
            "input"
        }
    }
    pub fn frame(&self) -> Option<&FrameAnchor> {
        match self {
            Self::Click { frame, .. }
            | Self::Drag { frame, .. }
            | Self::Scroll { frame, .. }
            | Self::Key { frame, .. } => Some(frame),
            _ => None,
        }
    }
    pub fn epoch(&self) -> Option<u64> {
        match self {
            Self::Release { epoch }
            | Self::Click { epoch, .. }
            | Self::Drag { epoch, .. }
            | Self::Scroll { epoch, .. }
            | Self::Key { epoch, .. } => Some(*epoch),
            _ => None,
        }
    }
    fn fields(&self, c: &mut Canon) -> Result<()> {
        if let Some(epoch) = self.epoch() {
            ensure!(
                (1..=MAX_SAFE_INTEGER).contains(&epoch),
                "invalid lease epoch"
            )
        };
        match self {
            Self::Acquire => c.text("acquire"),
            Self::ObserveFrame => c.text("observe_frame"),
            Self::Release { epoch } => {
                c.text("release");
                c.int(*epoch)
            }
            Self::Click {
                epoch,
                frame,
                x,
                y,
                button,
            } => {
                c.text("click");
                c.int(*epoch);
                frame.fields(c)?;
                point(frame, *x, *y)?;
                valid_button(button)?;
                c.int(*x);
                c.int(*y);
                c.text(button)
            }
            Self::Drag {
                epoch,
                frame,
                x,
                y,
                end_x,
                end_y,
                button,
            } => {
                c.text("drag");
                c.int(*epoch);
                frame.fields(c)?;
                point(frame, *x, *y)?;
                point(frame, *end_x, *end_y)?;
                valid_button(button)?;
                c.int(*x);
                c.int(*y);
                c.int(*end_x);
                c.int(*end_y);
                c.text(button)
            }
            Self::Scroll {
                epoch,
                frame,
                x,
                y,
                direction,
                ticks,
            } => {
                c.text("scroll");
                c.int(*epoch);
                frame.fields(c)?;
                point(frame, *x, *y)?;
                ensure!(
                    ["up", "down", "left", "right"].contains(&direction.as_str())
                        && (1..=20).contains(ticks),
                    "invalid discrete scroll"
                );
                c.int(*x);
                c.int(*y);
                c.text(direction);
                c.int(*ticks)
            }
            Self::Key { epoch, frame, keys } => {
                c.text("key");
                c.int(*epoch);
                frame.fields(c)?;
                ensure!(!keys.is_empty() && keys.len() <= 8, "invalid key count");
                for (i, key) in keys.iter().enumerate() {
                    ensure!(
                        !key.is_empty()
                            && key.len() <= 16
                            && key
                                .bytes()
                                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
                            && !keys[..i].contains(key),
                        "invalid/duplicate physical qcode"
                    )
                }
                c.strings(keys)
            }
        };
        Ok(())
    }
}
fn point(frame: &FrameAnchor, x: u64, y: u64) -> Result<()> {
    ensure!(
        x < frame.width && y < frame.height,
        "point outside screenshot"
    );
    Ok(())
}
fn valid_button(button: &str) -> Result<()> {
    ensure!(
        ["left", "middle", "right"].contains(&button),
        "invalid mouse button"
    );
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    pub session_id: String,
    pub transcript_sha256: String,
    pub sequence: u64,
    pub expires_at: u64,
    pub action: ControlAction,
}
impl ControlRequest {
    pub fn canonical(&self) -> Result<Vec<u8>> {
        identifier(&self.session_id)?;
        unhex(&self.transcript_sha256, 32)?;
        ensure!(
            (1..=MAX_SAFE_INTEGER).contains(&self.sequence) && self.expires_at <= MAX_SAFE_INTEGER,
            "control integer out of range"
        );
        let mut c = Canon::new("control");
        c.text(&self.session_id);
        c.text(&self.transcript_sha256);
        c.int(self.sequence);
        c.int(self.expires_at);
        self.action.fields(&mut c)?;
        Ok(c.0)
    }
    pub fn check_time(&self, now: u64) -> Result<()> {
        ensure!(
            self.expires_at > now && self.expires_at - now <= 5,
            "control request expired or too far in future"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedControl {
    pub request: ControlRequest,
    pub signature: String,
}
