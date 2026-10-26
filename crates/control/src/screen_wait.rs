//! `screen_wait`: block until the screen changes, then optionally until it
//! settles. Apple Virtualization Linux desktops do this in the guest agent
//! (tile hashes, changed regions). QEMU polls the console's SPICE draw
//! sequence; macOS Guests compare screenshot digests. Neither of those has
//! regions, so a region filter is refused there.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    os::fd::AsRawFd,
    os::unix::net::UnixStream,
    path::Path,
    thread,
    time::{Duration, Instant},
};

pub const MAX_TIMEOUT_MS: u64 = 25_000;
pub const MAX_SETTLE_MS: u64 = 5_000;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// Validated arguments, in the form the guest agent takes.
pub fn arguments(args: &Value) -> Result<Value> {
    let allowed = ["id", "frame_id", "timeout_ms", "settle_ms", "region"];
    if args
        .as_object()
        .is_none_or(|a| a.keys().any(|k| !allowed.contains(&k.as_str())))
    {
        bail!("screen_wait accepts only id, frame_id, timeout_ms, settle_ms and region")
    }
    let timeout = args.get("timeout_ms").map_or(Ok(DEFAULT_TIMEOUT_MS), |v| {
        v.as_u64()
            .filter(|t| (1..=MAX_TIMEOUT_MS).contains(t))
            .context("timeout_ms must be 1..25000")
    })?;
    let settle = args.get("settle_ms").map_or(Ok(0), |v| {
        v.as_u64()
            .filter(|t| *t <= MAX_SETTLE_MS)
            .context("settle_ms must be 0..5000")
    })?;
    let mut out = json!({"timeout_ms":timeout,"settle_ms":settle});
    if let Some(frame) = args.get("frame_id") {
        if frame.as_str().is_none_or(|f| f.len() != 36) {
            bail!("frame_id must be a screenshot frame id")
        }
        out["frame_id"] = frame.clone();
    }
    if let Some(region) = args.get("region") {
        let valid = region.as_object().is_some_and(|r| {
            r.len() == 4
                && ["x", "y", "width", "height"]
                    .iter()
                    .all(|k| r.get(*k).and_then(Value::as_u64).is_some_and(|v| v <= 8192))
        });
        if !valid {
            bail!("region must be {{x, y, width, height}} in screenshot pixels")
        }
        out["region"] = region.clone();
    }
    Ok(out)
}

/// Recent screenshot anchors per VM, so a wait can start from the frame the
/// caller last saw rather than from whenever its request arrives.
#[derive(Default)]
pub struct Baselines(BTreeMap<String, VecDeque<Value>>);
impl Baselines {
    pub fn record(&mut self, vm: &str, screenshot: &Value) {
        let frames = self.0.entry(vm.into()).or_default();
        frames.push_back(json!({"frame_id":screenshot["frame_id"],"source_sequence":screenshot["source_sequence"],"sha256":screenshot["sha256"],"width":screenshot["width"],"height":screenshot["height"]}));
        while frames.len() > 16 {
            frames.pop_front();
        }
    }
    pub fn get(&self, vm: &str, frame_id: &Value) -> Option<Value> {
        self.0
            .get(vm)?
            .iter()
            .find(|f| f["frame_id"] == *frame_id)
            .cloned()
    }
    pub fn forget(&mut self, vm: &str) {
        self.0.remove(vm);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// QEMU console `frame_status.source_sequence` (one step per SPICE draw).
    Sequence,
    /// macOS Guest screenshot SHA-256 (deterministic JPEG of the framebuffer).
    Digest,
}
impl Probe {
    fn interval(self) -> Duration {
        match self {
            Self::Sequence => Duration::from_millis(30),
            Self::Digest => Duration::from_millis(250),
        }
    }
    fn sample(self, dir: &Path, vm: &str) -> Result<(Value, u64, u64)> {
        let (value, field) = match self {
            Self::Sequence => (
                crate::call(dir, "frame_status", json!({"id":vm}))?,
                "source_sequence",
            ),
            Self::Digest => (crate::call(dir, "screenshot", json!({"id":vm}))?, "sha256"),
        };
        if self == Self::Sequence && value["frame_available"] != true {
            bail!("console frame unavailable")
        }
        let size = |k: &str| value[k].as_u64().context("probe returned no geometry");
        Ok((value[field].clone(), size("width")?, size("height")?))
    }
}

pub fn run(
    dir: &Path,
    vm: &str,
    probe: Probe,
    args: &Value,
    baseline: Option<Value>,
    peer: &UnixStream,
) -> Result<Value> {
    if args.get("region").is_some() {
        bail!("unsupported: region filtering needs an Apple Virtualization Linux desktop; this backend reports whole-screen changes only")
    }
    let timeout = Duration::from_millis(args["timeout_ms"].as_u64().unwrap_or(DEFAULT_TIMEOUT_MS));
    let settle = Duration::from_millis(args["settle_ms"].as_u64().unwrap_or(0));
    let field = match probe {
        Probe::Sequence => "source_sequence",
        Probe::Digest => "sha256",
    };
    let start = Instant::now();
    let base = match (&args["frame_id"], baseline) {
        (Value::Null, _) => {
            let (mark, width, height) = probe.sample(dir, vm)?;
            json!({field:mark,"width":width,"height":height})
        }
        (_, Some(frame)) if !frame[field].is_null() => frame,
        _ => bail!("unknown frame_id for this VM; take a new screenshot"),
    };
    let (mut changed_at, mut still_since, mut previous) = (None, start, Value::Null);
    let mut resized = false;
    loop {
        if crate::peer_disconnected(&[peer.as_raw_fd()]) {
            bail!("screen_wait client disconnected")
        }
        let (mark, width, height) = probe.sample(dir, vm)?;
        let now = Instant::now();
        if Some(width) != base["width"].as_u64() || Some(height) != base["height"].as_u64() {
            resized = true;
            changed_at = Some(now);
            break;
        }
        if changed_at.is_none() && mark != base[field] {
            (changed_at, still_since) = (Some(now), now);
        } else if changed_at.is_some() && mark != previous {
            still_since = now;
        }
        previous = mark;
        if changed_at.is_some() && now.duration_since(still_since) >= settle
            || now.duration_since(start) >= timeout
        {
            break;
        }
        thread::sleep(probe.interval());
    }
    let mut result = json!({"changed":changed_at.is_some(),"elapsed_ms":start.elapsed().as_millis() as u64,"width":base["width"],"height":base["height"],"regions":Value::Null,"detection":match probe {
        Probe::Sequence => "console-draw-sequence",
        Probe::Digest => "screenshot-digest",
    }});
    if let Some(at) = changed_at {
        result["first_change_ms"] = json!(at.duration_since(start).as_millis() as u64);
        if resized {
            result["resized"] = json!(true);
        } else {
            result["settled"] = json!(Instant::now().duration_since(still_since) >= settle);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arguments_are_bounded() {
        let ok = arguments(&json!({"id":"vm","timeout_ms":500,"settle_ms":200,"frame_id":"0".repeat(36),"region":{"x":0,"y":0,"width":10,"height":10}})).unwrap();
        assert_eq!(ok["timeout_ms"], 500);
        assert!(ok.get("id").is_none());
        assert_eq!(
            arguments(&json!({"id":"vm"})).unwrap()["timeout_ms"],
            10_000
        );
        for bad in [
            json!({"id":"vm","timeout_ms":0}),
            json!({"id":"vm","timeout_ms":25_001}),
            json!({"id":"vm","settle_ms":5_001}),
            json!({"id":"vm","frame_id":"short"}),
            json!({"id":"vm","region":{"x":0,"y":0,"width":10}}),
            json!({"id":"vm","region":{"x":-1,"y":0,"width":10,"height":1}}),
            json!({"id":"vm","extra":1}),
        ] {
            assert!(arguments(&bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn baselines_keep_recent_frames_per_vm() {
        let mut baselines = Baselines::default();
        for n in 0..20 {
            baselines.record(
                "a",
                &json!({"frame_id":format!("{n:036}"),"source_sequence":n,"width":8,"height":8}),
            );
        }
        assert!(baselines.get("a", &json!(format!("{:036}", 3))).is_none());
        assert_eq!(
            baselines.get("a", &json!(format!("{:036}", 19))).unwrap()["source_sequence"],
            19
        );
        assert!(baselines.get("b", &json!(format!("{:036}", 19))).is_none());
        baselines.forget("a");
        assert!(baselines.get("a", &json!(format!("{:036}", 19))).is_none());
    }
}
