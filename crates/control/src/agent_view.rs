//! The agent-facing half of the stdio MCP adapter: what an MCP session sends a
//! model, shaped for few tokens and few round trips. Everything here runs in
//! the adapter process on top of ordinary daemon calls; the daemon, workers and
//! Guests keep their own contracts and fences.
//!
//! - Accessibility trees come back as one compact line per element
//!   (`e12 push button "Save" @856,620 [focused]`) with short refs that this
//!   session maps to the Guest's refs, optionally only interactive elements.
//! - Input tools and `vm_accessibility_action` take `observe`: after dispatch
//!   they wait for the screen like `vm_screen_wait` from the pre-action frame,
//!   take a fresh frame for the next input and return what changed (the tree
//!   delta by ref where there is a tree, else the changed regions and their
//!   text), the compact tree, the screen text or a screenshot.
//! - A frame older than the backends' 5 s input window is re-anchored when the
//!   screen is provably unchanged since it was taken (same screenshot digest,
//!   geometry and generation). QEMU frames carry a console draw sequence and
//!   the worker checks that itself (`input::Frames`).
//! - `vm_screenshot` can scale, crop and re-encode on the host (`vm-vision
//!   image`) and reports how image pixels map back to input pixels.
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt::Write as _,
    io::{Read as _, Write as _},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::{Duration, Instant},
};

/// Tools this layer answers instead of forwarding them unchanged.
pub const TOOLS: [&str; 11] = [
    "vm_screenshot",
    "vm_screen_text",
    "vm_accessibility_tree",
    "vm_accessibility_find",
    "vm_accessibility_action",
    "vm_input_click",
    "vm_input_drag",
    "vm_input_scroll",
    "vm_input_key",
    "vm_input_text",
    "vm_input_batch",
];
pub const RETURNS: [&str; 5] = ["delta", "none", "compact_tree", "screenshot", "text"];
/// Re-anchor a little before the backends' 5 s window closes, so a drag still
/// finishes inside it.
pub const REANCHOR_AFTER: Duration = Duration::from_secs(4);
/// Image bytes one host-encoded screenshot may carry (as the Guest encoders).
pub const MAX_IMAGE_BYTES: usize = 700 * 1024;
const NAME_CHARS: usize = 80;
const TEXT_CHARS: usize = 160;
const MAX_DELTA_LINES: usize = 150;
const MAX_REFS: usize = 50_000;
const DEFAULT_SETTLE_MS: u64 = 300;
const DEFAULT_TIMEOUT_MS: u64 = 2000;

/// What this layer needs from the daemon and the host helper; a fixture in tests.
pub trait Daemon {
    fn call(&mut self, method: &str, args: Value) -> Result<Value>;
    /// One `vm-vision` mode on `image`; `before` is the earlier image for `diff`.
    fn vision(&mut self, args: &[String], image: &[u8], before: Option<&[u8]>) -> Result<Value>;
}

pub struct Live<'a>(pub &'a Path);
impl Daemon for Live<'_> {
    fn call(&mut self, method: &str, args: Value) -> Result<Value> {
        crate::call(self.0, method, args)
    }
    fn vision(&mut self, args: &[String], image: &[u8], before: Option<&[u8]>) -> Result<Value> {
        let Some(before) = before else {
            return crate::screen_text::vision_image(args, image.to_vec());
        };
        // A private file only this user can open, removed whatever happens.
        let mut random = [0u8; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("vm-vision-before-{name}"));
        struct Remove<'a>(&'a Path);
        impl Drop for Remove<'_> {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(self.0);
            }
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let _remove = Remove(&path);
        file.write_all(before)?;
        drop(file);
        let mut args = args.to_vec();
        args.extend(["--before".into(), path.to_string_lossy().into_owned()]);
        crate::screen_text::vision_image(&args, image.to_vec())
    }
}
/// A screenshot this session has seen, for re-anchoring.
struct Seen {
    anchor: Value,
    sha256: Value,
    source_sequence: Value,
    at: Instant,
}
/// The last accessibility tree read for a VM, keyed by Guest ref.
struct Snapshot {
    args: Value,
    order: Vec<String>,
    lines: HashMap<String, String>,
}
#[derive(Default)]
struct Vm {
    short: HashMap<String, String>,
    long: HashMap<String, String>,
    frames: VecDeque<Seen>,
    image: Option<(Value, Vec<u8>)>,
    tree: Option<Snapshot>,
    /// The tree arguments observe reuses: those of the last `vm_accessibility_tree`.
    tree_args: Option<Value>,
    accessibility: Option<bool>,
    /// Box centers of elements read in this session, for batch steps that click a ref.
    centers: HashMap<String, (u64, u64)>,
    /// The frame each OCR snapshot was read from (`ocr_window_state`).
    ocr_frames: HashMap<String, Value>,
}

#[derive(Default)]
pub struct Session {
    vms: BTreeMap<String, Vm>,
    next_ref: u64,
    /// Added to every frame age; tests stand in for the time a model's turn takes.
    skew: Duration,
}

/// Rows of an OCR snapshot, for a Guest without cua-driver.
const MAX_OCR_ROWS: usize = 200;

/// A host input tool's call, for its `ActionResult` (`action_result`).
struct Action {
    tool: String,
    target: Option<(f64, f64)>,
}
impl Action {
    fn result(&self, vm: &str, wait: Option<&Value>) -> Value {
        let keyboard = ["vm_input_key", "vm_input_text"].contains(&self.tool.as_str());
        let observed = crate::action_result::observed_from_wait(wait, self.target, keyboard);
        let summary = match self.target {
            Some((x, y)) => format!("{} delivered at {x},{y}", self.tool),
            None => format!("{} delivered", self.tool),
        };
        crate::action_result::host_input(vm, &summary, observed)
    }
}

/// The daemon's control-lease refusal as tool content: the input was not sent.
fn lease_refusal(vm: &str, error: &anyhow::Error) -> Option<Value> {
    let text = error.to_string();
    let rest = text.strip_prefix("person_in_control: ")?;
    let quiet = rest
        .split(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let refusal = crate::action_result::person_in_control(vm, quiet);
    Some(
        json!({"content":[{"type":"text","text":refusal.to_string()}],"structuredContent":{"action":refusal},"isError":true}),
    )
}

pub fn handles(name: &str) -> bool {
    TOOLS.contains(&name)
}

impl Session {
    /// Answers one tool call with MCP content. An error means the call did nothing
    /// observable to report; errors after dispatch are reported inside the content.
    pub fn call(&mut self, d: &mut impl Daemon, name: &str, mut args: Value) -> Result<Value> {
        let vm = args["id"].as_str().context("id required")?.to_string();
        match name {
            "vm_screenshot" => {
                let options = ImageOptions::take(&mut args)?;
                let shot = self.screenshot(d, &vm)?;
                screenshot_content(d, shot, &options)
            }
            "vm_screen_text" => {
                let flags = crate::screen_text::arguments(&args)?;
                let shot = self.screenshot(d, &vm)?;
                let image = self.vm(&vm).image.as_ref().unwrap().1.clone();
                let mut helper = vec!["text".to_string()];
                helper.extend(flags);
                let text = d.vision(&helper, &image, None)?;
                text_content(crate::screen_text::result(&shot, text)?)
            }
            "vm_accessibility_tree" | "vm_accessibility_find" => {
                let compact = take_format(&mut args)?;
                let interactive = take_bool(&mut args, "interactive_only")?;
                if name == "vm_accessibility_tree" {
                    let mut remembered = args.clone();
                    remembered["interactive_only"] = json!(interactive);
                    remembered.as_object_mut().unwrap().remove("id");
                    self.vm(&vm).tree_args = Some(remembered);
                }
                let method = name.strip_prefix("vm_").unwrap();
                let tree = self.tree(d, &vm, method, args, interactive)?;
                if !compact {
                    return text_content(tree);
                }
                let text =
                    self.render_tree(&vm, &tree, name == "vm_accessibility_tree", interactive);
                if name == "vm_accessibility_tree" {
                    self.remember_tree(&vm, &tree, interactive);
                }
                Ok(lines_content(&text))
            }
            "vm_input_batch" => self.batch(d, &vm, args),
            "vm_accessibility_action" => {
                let observe = Observe::take(&mut args)?;
                if let Some(r) = args["ref"].as_str() {
                    args["ref"] = json!(self.long_ref(&vm, r)?);
                }
                let baseline = match &observe {
                    Some(_) => Some(self.baseline(d, &vm)?),
                    None => None,
                };
                let mut result = d.call("accessibility_action", args)?;
                if let Some(long) = result["ref"].as_str().map(String::from) {
                    result["ref"] = json!(self.short_ref(&vm, &long));
                }
                if result["element_after"].is_object() {
                    let after = result["element_after"].take();
                    result["element_after"] = json!(self.node_line(&vm, &after));
                }
                match observe {
                    Some(observe) => self.observe(d, &vm, baseline, &observe, result, None),
                    None => text_content(result),
                }
            }
            _ => {
                let observe = Observe::take(&mut args)?;
                let reanchored = self.reanchor(d, &vm, &mut args)?;
                let baseline = args["frame_id"].clone();
                let action = Action {
                    tool: name.to_string(),
                    target: crate::action_result::target(name, &args),
                };
                let mut result = match d.call(name.strip_prefix("vm_").unwrap(), args) {
                    Ok(result) => result,
                    Err(error) => return lease_refusal(&vm, &error).ok_or(error),
                };
                if let Some(reanchored) = reanchored {
                    result["reanchored"] = reanchored;
                }
                match observe {
                    Some(observe) => {
                        result.as_object_mut().map(|r| r.remove("note"));
                        self.observe(d, &vm, Some(baseline), &observe, result, Some(action))
                    }
                    None => {
                        result["action"] = action.result(&vm, None);
                        text_content(result)
                    }
                }
            }
        }
    }

    /// `cua_get_window_state` for a Guest without cua-driver: the text the host reads
    /// on the screen, as rows with tokens `<snapshot>:N` like the driver's. They are
    /// not accessibility elements and are marked untrusted: `ocr_token_call` answers
    /// the cua tools given one, and `vm_input_batch` can click them by ref.
    pub fn ocr_window_state(&mut self, d: &mut impl Daemon, vm: &str) -> Result<Value> {
        let shot = self.screenshot(d, vm)?;
        let image = self.vm(vm).image.as_ref().unwrap().1.clone();
        let text = d.vision(&["text".into()], &image, None)?;
        let text = crate::screen_text::result(&shot, text)?;
        self.next_ref += 1;
        let snapshot = format!("ocr{}", self.next_ref);
        let frame = anchor(&shot);
        let state = self.vm(vm);
        state.ocr_frames.insert(snapshot.clone(), frame.clone());
        let mut rows = Vec::new();
        let mut elements = Vec::new();
        for (index, line) in text["lines"]
            .as_array()
            .into_iter()
            .flatten()
            .take(MAX_OCR_ROWS)
            .enumerate()
        {
            let token = format!("{snapshot}:{index}");
            if let (Some(x), Some(y)) = (line["center_x"].as_u64(), line["center_y"].as_u64()) {
                state.centers.insert(token.clone(), (x, y));
            }
            let words = line["text"].as_str().unwrap_or_default();
            rows.push(format!(
                "- [{index}] text {} {}",
                quoted(words, TEXT_CHARS),
                box_text(line, true)
            ));
            elements.push(json!({"element_token":token,"text":words,"x":line["x"],"y":line["y"],"width":line["width"],"height":line["height"],"center_x":line["center_x"],"center_y":line["center_y"],"confidence":line["confidence"]}));
        }
        let tree = rows.join("\n");
        let structured = json!({
            "snapshot_id":snapshot,"source":"host_ocr","trusted":false,"frame":frame,
            "tree_markdown":tree,"elements":elements,
            "hint":"This Guest has no cua-driver: rows are text the host read on the screen, not accessibility elements. Click one with vm_input_click at its center in this frame; verify_state reports unknown for them."
        });
        Ok(
            json!({"content":[{"type":"text","text":format!("host OCR of {vm} (untrusted, no cua-driver), frame {}:\n{tree}", frame["frame_id"])}],"structuredContent":structured,"isError":false}),
        )
    }

    /// A cua tool call that names an OCR token: answered here, never sent to a driver.
    /// `verify_state` cannot check text the host read, so it is unknown; an action is
    /// refused with the host pixel input that would do it as `next_call`.
    pub fn ocr_token_call(&mut self, vm: &str, tool: &str, args: &Value) -> Option<Value> {
        fn first_token(value: &Value) -> Option<&str> {
            match value {
                Value::String(s) if s.starts_with("ocr") && s.contains(':') => Some(s),
                Value::Array(items) => items.iter().find_map(first_token),
                Value::Object(map) => map.values().find_map(first_token),
                _ => None,
            }
        }
        let token = first_token(args)?.to_string();
        let structured = if tool == "verify_state" {
            json!({"status":"unknown","reason":"untrusted_source","element_token":token,"hint":"OCR rows are read from pixels; take a new cua_get_window_state or vm_screen_text and compare the text yourself."})
        } else {
            let state = self.vm(vm);
            let snapshot = token.split(':').next().unwrap_or_default();
            let mut next = json!({"tool":"vm_input_click","arguments":{"id":vm}});
            if let (Some((x, y)), Some(frame)) =
                (state.centers.get(&token), state.ocr_frames.get(snapshot))
            {
                next["arguments"]["x"] = json!(x);
                next["arguments"]["y"] = json!(y);
                for key in ["frame_id", "width", "height", "source_generation"] {
                    next["arguments"][key] = frame[key].clone();
                }
            }
            json!({"effect":"refused","route":"virtual_hid","error":{"code":"untrusted_element","hint":"An OCR row is not an accessibility element; act on its pixels on the host."},"escalation":{"target":"pixel","reason":"route_unavailable"},"next_call":next})
        };
        Some(
            json!({"content":[{"type":"text","text":structured.to_string()}],"structuredContent":structured,"isError":tool != "verify_state"}),
        )
    }

    /// The Guest's ref for a short ref of this session; a Guest ref as it is.
    fn long_ref(&mut self, vm: &str, r: &str) -> Result<String> {
        if !is_short(r) {
            return Ok(r.into());
        }
        self.vm(vm)
            .short
            .get(r)
            .cloned()
            .with_context(|| format!("unknown ref {r}; read vm_accessibility_tree again"))
    }

    fn vm(&mut self, vm: &str) -> &mut Vm {
        self.vms.entry(vm.into()).or_default()
    }

    fn screenshot(&mut self, d: &mut impl Daemon, vm: &str) -> Result<Value> {
        let at = Instant::now();
        let shot = d.call("screenshot", json!({"id":vm}))?;
        let image = crate::screen_text::decode(&shot)?;
        let state = self.vm(vm);
        state.frames.push_back(Seen {
            anchor: anchor(&shot),
            sha256: shot["sha256"].clone(),
            source_sequence: shot["source_sequence"].clone(),
            at,
        });
        while state.frames.len() > 16 {
            state.frames.pop_front();
        }
        state.image = Some((shot["frame_id"].clone(), image));
        Ok(shot)
    }

    /// The frame a wait after an action without one starts from: the latest this
    /// session saw, else a new one taken now, before the action.
    fn baseline(&mut self, d: &mut impl Daemon, vm: &str) -> Result<Value> {
        if let Some(seen) = self.vm(vm).frames.back() {
            return Ok(seen.anchor["frame_id"].clone());
        }
        Ok(self.screenshot(d, vm)?["frame_id"].clone())
    }

    /// Binds input to a fresh frame of the same screen when the caller's frame
    /// is about to leave the backend's input window but nothing has changed.
    fn reanchor(
        &mut self,
        d: &mut impl Daemon,
        vm: &str,
        args: &mut Value,
    ) -> Result<Option<Value>> {
        let skew = self.skew;
        let Some(seen) = self
            .vm(vm)
            .frames
            .iter()
            .find(|seen| seen.anchor["frame_id"] == args["frame_id"])
        else {
            return Ok(None);
        };
        // QEMU workers compare the console draw sequence themselves.
        if seen.at.elapsed() + skew < REANCHOR_AFTER
            || seen.source_sequence.is_u64()
            || !seen.sha256.is_string()
        {
            return Ok(None);
        }
        let (old, digest, age) = (
            seen.anchor.clone(),
            seen.sha256.clone(),
            seen.at.elapsed() + skew,
        );
        let shot = self.screenshot(d, vm)?;
        let same = shot["sha256"] == digest
            && ["width", "height", "source_generation"]
                .iter()
                .all(|k| shot[*k] == old[*k] && args[*k] == old[*k]);
        if !same {
            bail!(
                "stale_frame: the screen changed since frame {} was taken {:.1} s ago; take a new screenshot (input with observe returns a fresh frame each time)",
                old["frame_id"].as_str().unwrap_or("?"),
                age.as_secs_f64()
            )
        }
        args["frame_id"] = shot["frame_id"].clone();
        Ok(Some(
            json!({"from":old["frame_id"],"to":shot["frame_id"],"reason":"screen unchanged since the frame (same digest, geometry and generation)"}),
        ))
    }

    fn tree(
        &mut self,
        d: &mut impl Daemon,
        vm: &str,
        method: &str,
        mut args: Value,
        interactive: bool,
    ) -> Result<Value> {
        let wanted = args["max_nodes"].as_u64().unwrap_or(400);
        if interactive {
            // Filtering happens here, so read deeper into the tree than will be shown.
            // The daemon still refuses a max_nodes above 2000.
            args["max_nodes"] = json!(wanted.saturating_mul(4).min(2000).max(wanted));
        }
        let result = d.call(method, args);
        self.vm(vm).accessibility = Some(match &result {
            Ok(_) => true,
            Err(error) => !error.to_string().starts_with("unsupported"),
        });
        let mut tree = result?;
        if interactive {
            let nodes = tree["nodes"].as_array_mut().context("tree without nodes")?;
            nodes.retain(is_interactive);
            if nodes.len() as u64 > wanted {
                nodes.truncate(wanted as usize);
                tree["truncated"] = json!("max_nodes");
            }
            tree["count"] = json!(tree["nodes"].as_array().unwrap().len());
        }
        Ok(tree)
    }

    fn short_ref(&mut self, vm: &str, long: &str) -> String {
        if let Some(short) = self.vm(vm).long.get(long) {
            return short.clone();
        }
        let state = self.vms.get_mut(vm).unwrap();
        if state.long.len() >= MAX_REFS {
            // Numbers are never reused, so an old short ref is unknown, never another element.
            state.long.clear();
            state.short.clear();
        }
        self.next_ref += 1;
        let short = format!("e{}", self.next_ref);
        state.long.insert(long.into(), short.clone());
        state.short.insert(short.clone(), long.into());
        short
    }

    fn node_line(&mut self, vm: &str, node: &Value) -> String {
        let short = match node["ref"].as_str() {
            Some(long) => {
                if let (Some(x), Some(y)) = (
                    node["box"]["center_x"].as_u64(),
                    node["box"]["center_y"].as_u64(),
                ) {
                    self.vm(vm).centers.insert(long.into(), (x, y));
                }
                self.short_ref(vm, long)
            }
            None => "-".into(),
        };
        node_body(&short, node)
    }

    fn render_tree(&mut self, vm: &str, tree: &Value, indent: bool, interactive: bool) -> String {
        let nodes = tree["nodes"].as_array().cloned().unwrap_or_default();
        let mut out = tree_header(tree, interactive);
        for node in &nodes {
            out.push('\n');
            if indent && !interactive {
                let depth = node["depth"].as_u64().unwrap_or(0).min(40) as usize;
                out.extend(std::iter::repeat_n(' ', depth));
            }
            out += &self.node_line(vm, node);
        }
        out
    }

    fn remember_tree(&mut self, vm: &str, tree: &Value, interactive: bool) {
        let mut snapshot = Snapshot {
            args: self
                .vm(vm)
                .tree_args
                .clone()
                .unwrap_or(json!({"interactive_only":interactive})),
            order: Vec::new(),
            lines: HashMap::new(),
        };
        for node in tree["nodes"].as_array().into_iter().flatten() {
            let Some(long) = node["ref"].as_str() else {
                continue;
            };
            let line = self.node_line(vm, node);
            if snapshot.lines.insert(long.into(), line).is_none() {
                snapshot.order.push(long.into());
            }
        }
        self.vm(vm).tree = Some(snapshot);
    }

    fn observe(
        &mut self,
        d: &mut impl Daemon,
        vm: &str,
        baseline: Option<Value>,
        observe: &Observe,
        mut result: Value,
        action: Option<Action>,
    ) -> Result<Value> {
        let mut observed = json!({"return":observe.ret});
        let mut waited = None;
        if observe.timeout_ms > 0 {
            let mut wait =
                json!({"id":vm,"timeout_ms":observe.timeout_ms,"settle_ms":observe.settle_ms});
            if let Some(frame) = baseline.as_ref().filter(|f| f.is_string()) {
                wait["frame_id"] = frame.clone();
            }
            match d.call("screen_wait", wait) {
                Ok(wait) => {
                    for key in ["changed", "settled", "resized", "elapsed_ms"] {
                        if !wait[key].is_null() {
                            observed[key] = wait[key].clone();
                        }
                    }
                    waited = Some(wait);
                }
                Err(error) => observed["wait_error"] = json!(error.to_string()),
            }
        }
        if let Some(action) = &action {
            result["action"] = action.result(vm, waited.as_ref());
        }
        let before = self
            .vm(vm)
            .image
            .take_if(|(frame, _)| Some(&*frame) == baseline.as_ref());
        let shot = match self.screenshot(d, vm) {
            Ok(shot) => shot,
            Err(error) => {
                // The action happened; say what could not be observed.
                observed["error"] = json!(format!("no fresh frame: {error}"));
                result["observed"] = observed;
                return text_content(result);
            }
        };
        result["frame"] = anchor(&shot);
        let after = self.vm(vm).image.as_ref().unwrap().1.clone();
        let mut text = None;
        let mut image = None;
        let outcome = (|| -> Result<()> {
            match observe.ret {
                "none" => {}
                "screenshot" => image = Some(screenshot_content(d, shot.clone(), &observe.image)?),
                "text" => {
                    let lines = d.vision(&["text".into()], &after, None)?;
                    text = Some(text_lines("text", &lines, None));
                }
                "compact_tree" => {
                    let tree = self.observed_tree(d, vm)?;
                    let interactive = self.observed_args(vm)["interactive_only"] == true;
                    let rendered = self.render_tree(vm, &tree, true, interactive);
                    self.remember_tree(vm, &tree, interactive);
                    text = Some(rendered);
                }
                _ => {
                    text = Some(if self.vm(vm).accessibility != Some(false) {
                        match self.observed_tree(d, vm) {
                            Ok(tree) => self.tree_delta(vm, &tree),
                            // No tree here (QEMU) or not now (no at-spi2 in the image).
                            Err(_) => pixel_delta(d, before.as_ref().map(|b| &b.1[..]), &after)?,
                        }
                    } else {
                        pixel_delta(d, before.as_ref().map(|b| &b.1[..]), &after)?
                    });
                }
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            observed["error"] = json!(error.to_string());
        }
        result["observed"] = observed;
        let mut content = vec![json!({"type":"text","text":serde_json::to_string(&result)?})];
        if let Some(text) = text {
            content.push(json!({"type":"text","text":text}));
        }
        if let Some(image) = image {
            content.extend(image["content"].as_array().unwrap().iter().cloned());
        }
        Ok(json!({"content":content,"isError":false}))
    }

    fn observed_args(&mut self, vm: &str) -> Value {
        self.vm(vm)
            .tree_args
            .clone()
            .unwrap_or(json!({"interactive_only":true}))
    }

    fn observed_tree(&mut self, d: &mut impl Daemon, vm: &str) -> Result<Value> {
        let mut args = self.observed_args(vm);
        let interactive = args["interactive_only"] == true;
        args.as_object_mut().unwrap().remove("interactive_only");
        args["id"] = json!(vm);
        self.tree(d, vm, "accessibility_tree", args, interactive)
    }

    /// The elements added, removed or changed since the last tree, by ref.
    fn tree_delta(&mut self, vm: &str, tree: &Value) -> String {
        let args = self.observed_args(vm);
        let interactive = args["interactive_only"] == true;
        let old = self.vm(vm).tree.take().filter(|old| old.args == args);
        let Some(old) = old else {
            let text = self.render_tree(vm, tree, true, interactive);
            self.remember_tree(vm, tree, interactive);
            return format!("delta: no earlier tree; full tree follows\n{text}");
        };
        self.remember_tree(vm, tree, interactive);
        let new = self.vm(vm).tree.as_ref().unwrap();
        let mut lines = Vec::new();
        let (mut added, mut changed, mut removed) = (0, 0, 0);
        for long in &new.order {
            match old.lines.get(long) {
                None => {
                    added += 1;
                    lines.push(format!("+ {}", new.lines[long]));
                }
                Some(before) if *before != new.lines[long] => {
                    changed += 1;
                    lines.push(format!("~ {}", new.lines[long]));
                }
                _ => {}
            }
        }
        for long in &old.order {
            if !new.lines.contains_key(long) {
                removed += 1;
                lines.push(format!("- {}", old.lines[long]));
            }
        }
        let mut out = format!(
            "delta: +{added} ~{changed} -{removed} of {} elements",
            new.order.len()
        );
        if tree["truncated"].is_string() {
            let _ = write!(out, "; truncated: {}", tree["truncated"].as_str().unwrap());
        }
        let total = lines.len();
        for line in lines.into_iter().take(MAX_DELTA_LINES) {
            out.push('\n');
            out += &line;
        }
        if total > MAX_DELTA_LINES {
            let _ = write!(
                out,
                "\n... {} more; read vm_accessibility_tree",
                total - MAX_DELTA_LINES
            );
        }
        out
    }
}

/// Changed regions between two screenshots and the text inside them.
fn pixel_delta(d: &mut impl Daemon, before: Option<&[u8]>, after: &[u8]) -> Result<String> {
    let Some(before) = before else {
        return Ok("delta: no earlier image of this frame in this session; take vm_screenshot or use return screenshot".into());
    };
    let diff = d.vision(&["diff".into()], after, Some(before))?;
    let regions = diff["regions"].as_array().cloned().unwrap_or_default();
    let mut out = format!(
        "delta: {} changed region{} ({:.1}% of screen)",
        diff["region_count"]
            .as_u64()
            .unwrap_or(regions.len() as u64),
        if regions.len() == 1 { "" } else { "s" },
        diff["changed_fraction"].as_f64().unwrap_or(0.0) * 100.0
    );
    if regions.is_empty() {
        return Ok(out);
    }
    for region in &regions {
        let _ = write!(out, "\nregion {}", box_text(region, true));
    }
    let (width, height) = (
        diff["width"].as_i64().unwrap_or(0),
        diff["height"].as_i64().unwrap_or(0),
    );
    let pad = |r: &Value| {
        let v = |k: &str| r[k].as_i64().unwrap_or(0);
        let (x0, y0) = ((v("x") - 8).max(0), (v("y") - 8).max(0));
        let (x1, y1) = (
            (v("x") + v("width") + 8).min(width),
            (v("y") + v("height") + 8).min(height),
        );
        (x0, y0, x1, y1)
    };
    let padded: Vec<_> = regions.iter().map(pad).collect();
    let union = padded.iter().fold((width, height, 0, 0), |u, r| {
        (u.0.min(r.0), u.1.min(r.1), u.2.max(r.2), u.3.max(r.3))
    });
    let mut helper = vec!["text".to_string()];
    if (union.2 - union.0) * (union.3 - union.1) * 2 < width * height {
        helper.extend([
            "--crop".into(),
            format!(
                "{},{},{},{}",
                union.0,
                union.1,
                union.2 - union.0,
                union.3 - union.1
            ),
        ]);
    }
    let lines = d.vision(&helper, after, None)?;
    let inside = |line: &Value| {
        let v = |k: &str| line[k].as_i64().unwrap_or(-1);
        padded.iter().any(|r| {
            v("x") < r.2 && v("x") + v("width") > r.0 && v("y") < r.3 && v("y") + v("height") > r.1
        })
    };
    out.push('\n');
    out += &text_lines("text in changed regions", &lines, Some(&inside));
    Ok(out)
}

fn text_lines(title: &str, text: &Value, keep: Option<&dyn Fn(&Value) -> bool>) -> String {
    let lines: Vec<&Value> = text["lines"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|line| keep.is_none_or(|keep| keep(line)))
        .collect();
    let mut out = format!(
        "{title}: {} line{}",
        lines.len(),
        if lines.len() == 1 { "" } else { "s" }
    );
    for line in lines {
        let _ = write!(
            out,
            "\n{} {}",
            quoted(line["text"].as_str().unwrap_or(""), TEXT_CHARS),
            box_text(line, false)
        );
    }
    out
}

/// `@center_x,center_y`, with ` WxH` when `size`.
fn box_text(b: &Value, size: bool) -> String {
    let mut out = format!("@{},{}", b["center_x"], b["center_y"]);
    if size {
        let _ = write!(out, " {}x{}", b["width"], b["height"]);
    }
    out
}

fn quoted(text: &str, limit: usize) -> String {
    let mut cut: String = text.chars().take(limit).collect();
    if cut.len() < text.len() {
        cut.push('…');
    }
    serde_json::to_string(&cut).unwrap()
}

const WINDOW_ROLES: [&str; 8] = [
    "application",
    "frame",
    "window",
    "dialog",
    "alert",
    "file chooser",
    "sheet",
    "standard window",
];
const INTERACTIVE_ROLES: [&str; 34] = [
    "push button",
    "toggle button",
    "check box",
    "radio button",
    "menu item",
    "check menu item",
    "radio menu item",
    "menu",
    "combo box",
    "text",
    "entry",
    "password text",
    "spin button",
    "slider",
    "link",
    "page tab",
    "list item",
    "tree item",
    "table cell",
    "scroll bar",
    "button",
    "text field",
    "text area",
    "pop up button",
    "menu button",
    "menu bar item",
    "tab",
    "radio group",
    "incrementor",
    "disclosure triangle",
    "search field",
    "secure text field",
    "color well",
    "cell",
];

/// Elements an agent can act on, plus applications and windows for context.
pub fn is_interactive(node: &Value) -> bool {
    let role = node["role"].as_str().unwrap_or("");
    let states = node["states"].as_array();
    let has_state = |s: &str| states.is_some_and(|states| states.iter().any(|v| v == s));
    node["depth"] == 0
        || WINDOW_ROLES.contains(&role)
        || node["actions"].as_array().is_some_and(|a| !a.is_empty())
        || has_state("editable")
        || has_state("focused")
        || INTERACTIVE_ROLES.contains(&role)
}

fn is_short(r: &str) -> bool {
    r.len() > 1 && r.starts_with('e') && r[1..].bytes().all(|b| b.is_ascii_digit())
}

/// One element: `ref role "name" ="text" value=V @cx,cy [states] {actions} +N pid=P in "context"`.
pub fn node_body(short: &str, node: &Value) -> String {
    let mut out = short.to_string();
    let role = node["role"]
        .as_str()
        .filter(|r| !r.is_empty())
        .unwrap_or("?");
    let _ = write!(out, " {role}");
    if let Some(subrole) = node["subrole"]
        .as_str()
        .filter(|s| !s.is_empty() && *s != role)
    {
        let _ = write!(out, ":{subrole}");
    }
    let name = node["name"].as_str().unwrap_or("");
    if !name.is_empty() {
        let _ = write!(out, " {}", quoted(name, NAME_CHARS));
    }
    if let Some(text) = node["text"]
        .as_str()
        .filter(|t| !t.is_empty() && *t != name)
    {
        let _ = write!(out, " ={}", quoted(text, TEXT_CHARS));
    }
    if let Some(placeholder) = node["placeholder"].as_str().filter(|t| !t.is_empty()) {
        let _ = write!(out, " placeholder {}", quoted(placeholder, NAME_CHARS));
    }
    if let Some(value) = node.get("value").filter(|v| v.is_number() || v.is_string()) {
        match value.as_str() {
            Some(text) => {
                let _ = write!(out, " value={}", quoted(text, NAME_CHARS));
            }
            None => {
                let _ = write!(out, " value={value}");
            }
        }
    }
    if node["box"].is_object() {
        let windowish = WINDOW_ROLES.contains(&role);
        let _ = write!(out, " {}", box_text(&node["box"], windowish));
    }
    if let Some(states) = node["states"].as_array().filter(|s| !s.is_empty()) {
        let states: Vec<&str> = states.iter().filter_map(Value::as_str).collect();
        let _ = write!(out, " [{}]", states.join(","));
    }
    if let Some(actions) = node["actions"].as_array().filter(|a| a.len() > 1) {
        let actions: Vec<&str> = actions.iter().filter_map(Value::as_str).collect();
        let _ = write!(out, " {{{}}}", actions.join(","));
    }
    if let Some(more) = node["unexpanded_children"].as_u64() {
        let _ = write!(out, " +{more}");
    }
    if let Some(pid) = node["pid"].as_u64() {
        let _ = write!(out, " pid={pid}");
    }
    if let Some(context) = node["context"].as_str().filter(|c| !c.is_empty()) {
        let _ = write!(out, " in {}", quoted(context, NAME_CHARS));
    }
    out
}

fn tree_header(tree: &Value, interactive: bool) -> String {
    let mut out = format!("{} elements", tree["count"].as_u64().unwrap_or(0));
    if interactive {
        out += " (interactive only)";
    }
    if let Some(visited) = tree["visited"].as_u64() {
        let _ = write!(out, ", {visited} visited");
    }
    if let Some(truncated) = tree["truncated"].as_str() {
        let _ = write!(out, "; truncated: {truncated}");
    }
    if tree["accessibility_enabled_now"] == true {
        out += "; accessibility enabled now (start apps again to see Qt/Chromium)";
    }
    out
}

fn anchor(shot: &Value) -> Value {
    json!({"frame_id":shot["frame_id"],"width":shot["width"],"height":shot["height"],"source_generation":shot["source_generation"]})
}

fn take_bool(args: &mut Value, key: &str) -> Result<bool> {
    match args.as_object_mut().and_then(|a| a.remove(key)) {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(b),
        Some(_) => bail!("{key} must be a boolean"),
    }
}

/// `format`: compact (default) or json.
fn take_format(args: &mut Value) -> Result<bool> {
    match args.as_object_mut().and_then(|a| a.remove("format")) {
        None => Ok(true),
        Some(v) if v == "compact" => Ok(true),
        Some(v) if v == "json" => Ok(false),
        Some(_) => bail!("format must be compact or json"),
    }
}

fn text_content(value: Value) -> Result<Value> {
    Ok(json!({"content":[{"type":"text","text":serde_json::to_string(&value)?}],"isError":false}))
}

fn lines_content(text: &str) -> Value {
    json!({"content":[{"type":"text","text":text}],"isError":false})
}

/// Host-side screenshot options: scale, crop and encoding.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct ImageOptions {
    max_width: Option<u64>,
    max_height: Option<u64>,
    format: Option<String>,
    quality: Option<u64>,
    region: Option<[u64; 4]>,
}
impl ImageOptions {
    pub fn take(args: &mut Value) -> Result<Self> {
        let Some(object) = args.as_object_mut() else {
            return Ok(Self::default());
        };
        Self::from_map(object)
    }
    fn from_map(object: &mut Map<String, Value>) -> Result<Self> {
        let mut options = Self::default();
        let number = |v: Value, key: &str, range: std::ops::RangeInclusive<u64>| {
            v.as_u64().filter(|n| range.contains(n)).with_context(|| {
                format!(
                    "{key} must be an integer {}..{}",
                    range.start(),
                    range.end()
                )
            })
        };
        if let Some(v) = object.remove("max_width") {
            options.max_width = Some(number(v, "max_width", 16..=8192)?);
        }
        if let Some(v) = object.remove("max_height") {
            options.max_height = Some(number(v, "max_height", 16..=8192)?);
        }
        if let Some(v) = object.remove("quality") {
            options.quality = Some(number(v, "quality", 1..=100)?);
        }
        if let Some(v) = object.remove("format") {
            let format = v
                .as_str()
                .filter(|f| ["jpeg", "png"].contains(f))
                .context("format must be jpeg or png")?;
            options.format = Some(format.into());
        }
        if let Some(v) = object.remove("region") {
            let field = |k: &str| v[k].as_u64().filter(|n| *n <= 8192);
            let region = (|| Some([field("x")?, field("y")?, field("width")?, field("height")?]))()
                .filter(|r| r[2] > 0 && r[3] > 0 && v.as_object().is_some_and(|o| o.len() == 4))
                .context("region must be {x, y, width, height} in screenshot pixels")?;
            options.region = Some(region);
        }
        Ok(options)
    }
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
    fn flags(&self) -> Vec<String> {
        let mut flags = vec!["image".to_string()];
        if let Some([x, y, w, h]) = self.region {
            flags.extend(["--crop".into(), format!("{x},{y},{w},{h}")]);
        }
        for (flag, value) in [
            ("--max-width", self.max_width),
            ("--max-height", self.max_height),
            ("--quality", self.quality),
        ] {
            if let Some(value) = value {
                flags.extend([flag.into(), value.to_string()]);
            }
        }
        if let Some(format) = &self.format {
            flags.extend(["--format".into(), format.clone()]);
        }
        flags
    }
}

/// The screenshot as MCP content; with options, scaled, cropped or re-encoded on
/// the host, with the mapping from image pixels back to input pixels.
fn screenshot_content(
    d: &mut impl Daemon,
    mut shot: Value,
    options: &ImageOptions,
) -> Result<Value> {
    if options.is_empty() {
        return crate::screenshot_content(shot);
    }
    let data = shot
        .as_object_mut()
        .and_then(|v| v.remove("data_base64"))
        .context("screenshot missing image data")?;
    if let Some([x, y, w, h]) = options.region {
        let (width, height) = (
            shot["width"].as_u64().unwrap_or(0),
            shot["height"].as_u64().unwrap_or(0),
        );
        if x + w > width || y + h > height {
            bail!("region must lie inside the {width}x{height} screenshot")
        }
    }
    let image = base64::engine::general_purpose::STANDARD
        .decode(data.as_str().context("screenshot image is not base64")?)
        .context("invalid screenshot base64")?;
    let mut out = d.vision(&options.flags(), &image, None)?;
    let bytes = out["byte_length"]
        .as_u64()
        .context("image helper gave no size")?;
    if bytes as usize > MAX_IMAGE_BYTES {
        bail!("the encoded image is {bytes} bytes, above {MAX_IMAGE_BYTES}; ask for a smaller max_width/max_height or a lower quality")
    }
    if out["source_width"] != shot["width"] || out["source_height"] != shot["height"] {
        bail!("image helper read a different size than the screenshot")
    }
    let scale = out["scale"]
        .as_f64()
        .filter(|s| *s > 0.0)
        .context("image helper gave no scale")?;
    let crop = out["crop"].clone();
    let source = shot.as_object_mut().unwrap();
    let digest = source.remove("sha256").unwrap_or(Value::Null);
    source.insert("source_sha256".into(), digest);
    for key in ["image_width", "image_height", "mime_type", "byte_length"] {
        source.insert(key.into(), out[key].take());
    }
    source.insert("image_scale".into(), json!(scale));
    source.insert("image_region".into(), crop.clone());
    source.insert(
        "image_to_input".into(),
        json!("input x = image_region.x + image x / image_scale (y likewise); width, height and frame_id stay those of the full frame"),
    );
    if crop["x"] == 0
        && crop["y"] == 0
        && crop["width"] == shot["width"]
        && crop["height"] == shot["height"]
        && scale >= 0.125
    {
        // Image coordinates as input points: points * point_pixel_scale = source pixels.
        shot["image_point_pixel_scale"] = json!(1.0 / scale);
    }
    let data = out["data_base64"].take();
    Ok(
        json!({"content":[{"type":"image","mimeType":shot["mime_type"],"data":data},{"type":"text","text":serde_json::to_string(&shot)?}],"isError":false}),
    )
}

/// The `observe` argument of input tools and `vm_accessibility_action`.
#[derive(Debug, Clone, PartialEq)]
pub struct Observe {
    ret: &'static str,
    settle_ms: u64,
    timeout_ms: u64,
    image: ImageOptions,
}
impl Observe {
    pub fn take(args: &mut Value) -> Result<Option<Self>> {
        let Some(value) = args.as_object_mut().and_then(|a| a.remove("observe")) else {
            return Ok(None);
        };
        let Value::Object(mut object) = value else {
            bail!("observe must be an object")
        };
        let ret = match object.remove("return") {
            None => RETURNS[0],
            Some(v) => *RETURNS
                .iter()
                .find(|r| v == **r)
                .context("observe.return must be delta, none, compact_tree, screenshot or text")?,
        };
        let mut number = |key: &str, default: u64, max: u64| -> Result<u64> {
            object.remove(key).map_or(Ok(default), |v| {
                v.as_u64()
                    .filter(|n| *n <= max)
                    .with_context(|| format!("observe.{key} must be an integer 0..{max}"))
            })
        };
        let settle_ms = number(
            "settle_ms",
            DEFAULT_SETTLE_MS,
            crate::screen_wait::MAX_SETTLE_MS,
        )?;
        let timeout_ms = number(
            "timeout_ms",
            DEFAULT_TIMEOUT_MS,
            crate::screen_wait::MAX_TIMEOUT_MS,
        )?;
        let image = ImageOptions::from_map(&mut object)?;
        if image.region.is_some() {
            bail!("observe takes no region; crop with vm_screenshot")
        }
        if !image.is_empty() && ret != "screenshot" {
            bail!("observe.max_width, max_height, format and quality apply to return screenshot")
        }
        if let Some(key) = object.keys().next() {
            bail!("observe takes return, settle_ms, timeout_ms, max_width, max_height, format and quality, not {key}")
        }
        Ok(Some(Self {
            ret,
            settle_ms,
            timeout_ms,
            image,
        }))
    }
}

/// The JSON schema of `observe`.
pub fn observe_schema() -> Value {
    json!({"type":"object","description":"After dispatch, wait for the screen (as vm_screen_wait from the pre-action frame), take a fresh frame and return it with what you asked for, so the next input needs no separate screenshot. The reply's frame holds frame_id, width, height and source_generation for that input.","properties":{
        "return":{"type":"string","enum":RETURNS,"default":"delta","description":"delta: accessibility elements added (+), changed (~) or removed (-) since the last tree read in this session, by ref; without an accessibility tree (QEMU, Windows) the changed screen regions and the text recognized in them. compact_tree: the whole compact tree. text: the screen text. screenshot: the image. none: only the frame."},
        "settle_ms":{"type":"integer","minimum":0,"maximum":crate::screen_wait::MAX_SETTLE_MS,"default":DEFAULT_SETTLE_MS},
        "timeout_ms":{"type":"integer","minimum":0,"maximum":crate::screen_wait::MAX_TIMEOUT_MS,"default":DEFAULT_TIMEOUT_MS,"description":"How long to wait for a change; 0 takes the frame at once."},
        "max_width":{"type":"integer","minimum":16,"maximum":8192},"max_height":{"type":"integer","minimum":16,"maximum":8192},
        "format":{"type":"string","enum":["jpeg","png"]},"quality":{"type":"integer","minimum":1,"maximum":100}},
        "additionalProperties":false})
}

/// The extra `vm_screenshot` properties.
pub fn screenshot_properties() -> Value {
    json!({"id":{"type":"string"},
        "max_width":{"type":"integer","minimum":16,"maximum":8192,"description":"Scale down (never up) to at most this width, keeping the aspect ratio. Most clients downscale images over about 1568 px themselves; asking for that size here makes the pixels you see the ones the reply's scale describes."},
        "max_height":{"type":"integer","minimum":16,"maximum":8192},
        "format":{"type":"string","enum":["jpeg","png"],"description":"Default: jpeg when scaling, else the capture's own format."},
        "quality":{"type":"integer","minimum":1,"maximum":100,"default":80,"description":"JPEG quality."},
        "region":{"type":"object","description":"Crop to this rectangle in screenshot pixels before scaling.","properties":{"x":{"type":"integer","minimum":0},"y":{"type":"integer","minimum":0},"width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1}},"required":["x","y","width","height"],"additionalProperties":false}})
}

pub const MAX_BATCH_STEPS: usize = 50;
pub const MAX_BATCH_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_BATCH_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_UNTIL_MS: u64 = 5000;
const POLL: Duration = Duration::from_millis(300);
pub const STEP_TYPES: [&str; 7] = ["click", "text", "key", "scroll", "drag", "a11y", "wait"];
const FRAME_FIELDS: [&str; 4] = ["frame_id", "width", "height", "source_generation"];

/// The fields each batch step type takes besides `type`.
fn step_fields(kind: &str) -> &'static [&'static str] {
    match kind {
        "click" => &[
            "x",
            "y",
            "ref",
            "button",
            "coordinate_space",
            "point_pixel_scale",
        ],
        "scroll" => &[
            "x",
            "y",
            "ref",
            "direction",
            "ticks",
            "coordinate_space",
            "point_pixel_scale",
        ],
        "drag" => &[
            "x",
            "y",
            "end_x",
            "end_y",
            "button",
            "coordinate_space",
            "point_pixel_scale",
        ],
        "key" => &["keys", "hold_ms"],
        "text" => &["text"],
        "a11y" => &["ref", "action", "action_name", "text", "value"],
        "wait" => &[
            "settle_ms",
            "timeout_ms",
            "until_text",
            "until_name",
            "until_role",
        ],
        _ => &[],
    }
}

/// Checks a step's shape before anything of the batch is sent; values are checked
/// by the daemon as for the single tools.
fn check_step(step: &Value) -> Result<&'static str> {
    let object = step.as_object().context("a step must be an object")?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .and_then(|t| STEP_TYPES.iter().find(|k| **k == t))
        .context("step type must be click, text, key, scroll, drag, a11y or wait")?;
    let fields = step_fields(kind);
    if let Some(key) = object
        .keys()
        .find(|k| *k != "type" && !fields.contains(&k.as_str()))
    {
        bail!("a {kind} step takes {}, not {key}", fields.join(", "))
    }
    let has = |k: &str| object.contains_key(k);
    match *kind {
        "click" | "scroll" if has("ref") == (has("x") || has("y")) => {
            bail!("a {kind} step takes either ref or x and y")
        }
        "click" | "scroll" if has("ref") && has("coordinate_space") => {
            bail!("a {kind} step at a ref takes no coordinate_space")
        }
        "a11y" if !has("ref") || !has("action") => bail!("an a11y step needs ref and action"),
        "wait" => {
            for key in ["settle_ms", "timeout_ms"] {
                if let Some(v) = object.get(key) {
                    v.as_u64()
                        .filter(|n| *n <= crate::screen_wait::MAX_TIMEOUT_MS)
                        .with_context(|| format!("wait {key} must be 0..25000"))?;
                }
            }
            for key in ["until_text", "until_name", "until_role"] {
                if let Some(v) = object.get(key) {
                    v.as_str()
                        .filter(|t| !t.is_empty() && t.chars().count() <= 200)
                        .with_context(|| format!("wait {key} must be 1..200 characters"))?;
                }
            }
            if has("until_text") && (has("until_name") || has("until_role")) {
                bail!("a wait step waits for text or for an element, not both")
            }
        }
        _ => {}
    }
    Ok(kind)
}

impl Session {
    /// Runs `steps` in order and stops at the first failure.
    fn batch(&mut self, d: &mut impl Daemon, vm: &str, mut args: Value) -> Result<Value> {
        let observe = Observe::take(&mut args)?;
        let steps = args["steps"]
            .as_array()
            .cloned()
            .context("steps must be an array")?;
        if !(1..=MAX_BATCH_STEPS).contains(&steps.len()) {
            bail!("steps must hold 1..{MAX_BATCH_STEPS} steps")
        }
        let timeout = args
            .get("timeout_ms")
            .map_or(Ok(DEFAULT_BATCH_TIMEOUT_MS), |v| {
                v.as_u64()
                    .filter(|t| (1..=MAX_BATCH_TIMEOUT_MS).contains(t))
                    .with_context(|| format!("timeout_ms must be 1..{MAX_BATCH_TIMEOUT_MS}"))
            })?;
        let kinds = steps.iter().map(check_step).collect::<Result<Vec<_>>>()?;
        let physical = kinds.iter().any(|k| !["a11y", "wait"].contains(k));
        let given = FRAME_FIELDS
            .iter()
            .filter(|k| args.get(**k).is_some())
            .count();
        if given != 0 && given != FRAME_FIELDS.len() || physical && given == 0 {
            bail!("input steps need the screenshot's frame_id, width, height and source_generation")
        }
        let mut frame = json!({});
        for key in FRAME_FIELDS {
            frame[key] = args[key].clone();
        }
        // What observe compares with: the frame the batch starts from (else, as for
        // a single accessibility action, the last one seen or one taken now).
        let baseline = match (&observe, frame["frame_id"].is_string()) {
            (_, true) => Some(frame["frame_id"].clone()),
            (Some(_), false) => Some(self.baseline(d, vm)?),
            (None, false) => None,
        };
        let start = self
            .vm(vm)
            .image
            .clone()
            .filter(|(f, _)| Some(f) == baseline.as_ref());
        let deadline = Instant::now() + Duration::from_millis(timeout);
        let mut bound: Option<Instant> = None;
        let mut done = Vec::new();
        let mut failed = None;
        for (index, (step, kind)) in steps.iter().zip(&kinds).enumerate() {
            let outcome = if Instant::now() >= deadline {
                Err(anyhow::anyhow!(
                    "the batch timeout_ms ({timeout}) ran out before this step"
                ))
            } else {
                self.step(d, vm, step, kind, &mut frame, &mut bound, deadline)
            };
            match outcome {
                Ok(summary) => done.push(json!(summary)),
                Err(error) => {
                    failed = Some(json!({"index":index,"type":kind,"error":error.to_string()}));
                    break;
                }
            }
        }
        let mut result =
            json!({"ok":failed.is_none(),"completed":done.len(),"total":steps.len(),"steps":done});
        if let Some(failed) = failed.clone() {
            result["failed"] = failed;
        }
        let mut content = match observe {
            Some(observe) => {
                if let Some(start) = start {
                    self.vm(vm).image = Some(start);
                }
                self.observe(d, vm, baseline, &observe, result, None)?
            }
            None => text_content(result)?,
        };
        content["isError"] = json!(failed.is_some());
        Ok(content)
    }

    /// One batch step; the reply is a short summary of what was done.
    #[allow(clippy::too_many_arguments)]
    fn step(
        &mut self,
        d: &mut impl Daemon,
        vm: &str,
        step: &Value,
        kind: &str,
        frame: &mut Value,
        bound: &mut Option<Instant>,
        deadline: Instant,
    ) -> Result<String> {
        match kind {
            "a11y" => {
                let mut args = step.clone();
                let object = args.as_object_mut().unwrap();
                object.remove("type");
                let short = step["ref"]
                    .as_str()
                    .context("ref must be a string")?
                    .to_string();
                args["ref"] = json!(self.long_ref(vm, &short)?);
                args["id"] = json!(vm);
                let reply = d.call("accessibility_action", args)?;
                if reply["done"] == false {
                    bail!(
                        "the application did not accept {} on {short}",
                        step["action"]
                    )
                }
                Ok(format!(
                    "a11y {} {short}",
                    step["action"].as_str().unwrap_or("?")
                ))
            }
            "wait" => self.wait_step(d, vm, step, frame, deadline),
            _ => {
                self.bind(d, vm, frame, bound)?;
                let mut args = frame.clone();
                args["id"] = json!(vm);
                for key in step_fields(kind) {
                    if let Some(v) = step.get(*key) {
                        args[*key] = v.clone();
                    }
                }
                if let Some(r) = step["ref"].as_str() {
                    let long = self.long_ref(vm, r)?;
                    let (x, y) = *self.vm(vm).centers.get(&long).with_context(|| {
                        format!("no box known for {r}; read the tree or use x and y")
                    })?;
                    args.as_object_mut().unwrap().remove("ref");
                    args["x"] = json!(x);
                    args["y"] = json!(y);
                }
                d.call(&format!("input_{kind}"), args.clone())?;
                Ok(match kind {
                    "text" => format!(
                        "text {} chars",
                        step["text"].as_str().map_or(0, |t| t.chars().count())
                    ),
                    "key" => format!(
                        "key {}",
                        step["keys"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("+")
                    ),
                    _ => format!("{kind} @{},{}", args["x"], args["y"]),
                })
            }
        }
    }

    /// Keeps input bound to a frame the backend accepts: the caller's frame first
    /// (re-anchored when old), then while it is younger than REANCHOR_AFTER, then a
    /// fresh one that must keep the batch's geometry and generation.
    fn bind(
        &mut self,
        d: &mut impl Daemon,
        vm: &str,
        frame: &mut Value,
        bound: &mut Option<Instant>,
    ) -> Result<()> {
        match *bound {
            None => {
                self.reanchor(d, vm, frame)?;
                let seen = self
                    .vm(vm)
                    .frames
                    .iter()
                    .find(|s| s.anchor["frame_id"] == frame["frame_id"])
                    .map(|s| s.at);
                *bound = Some(seen.unwrap_or_else(Instant::now));
            }
            Some(at) if at.elapsed() + self.skew >= REANCHOR_AFTER => {
                let shot = self.screenshot(d, vm)?;
                if let Some(key) = ["width", "height", "source_generation"]
                    .iter()
                    .find(|k| shot[**k] != frame[**k])
                {
                    bail!(
                        "the screen's {key} changed during the batch ({} -> {}); take a new screenshot",
                        frame[*key], shot[*key]
                    )
                }
                frame["frame_id"] = shot["frame_id"].clone();
                *bound = Some(Instant::now());
            }
            Some(_) => {}
        }
        Ok(())
    }

    fn wait_step(
        &mut self,
        d: &mut impl Daemon,
        vm: &str,
        step: &Value,
        frame: &Value,
        deadline: Instant,
    ) -> Result<String> {
        let timeout = step["timeout_ms"].as_u64();
        if let Some(text) = step["until_text"].as_str() {
            let until = deadline
                .min(Instant::now() + Duration::from_millis(timeout.unwrap_or(DEFAULT_UNTIL_MS)));
            let wanted = text.to_lowercase();
            loop {
                self.screenshot(d, vm)?;
                let image = self.vm(vm).image.as_ref().unwrap().1.clone();
                let lines = d.vision(&["text".into()], &image, None)?;
                if let Some(line) = lines["lines"].as_array().into_iter().flatten().find(|l| {
                    l["text"]
                        .as_str()
                        .is_some_and(|t| t.to_lowercase().contains(&wanted))
                }) {
                    return Ok(format!(
                        "wait found {} {}",
                        quoted(line["text"].as_str().unwrap(), TEXT_CHARS),
                        box_text(line, false)
                    ));
                }
                if Instant::now() + POLL >= until {
                    bail!("text {} did not appear in time", quoted(text, NAME_CHARS))
                }
                std::thread::sleep(POLL);
            }
        }
        if step.get("until_name").is_some() || step.get("until_role").is_some() {
            let until = deadline
                .min(Instant::now() + Duration::from_millis(timeout.unwrap_or(DEFAULT_UNTIL_MS)));
            let mut find = json!({"id":vm,"max_nodes":1});
            for (from, to) in [("until_name", "name"), ("until_role", "role")] {
                if let Some(v) = step.get(from) {
                    find[to] = v.clone();
                }
            }
            loop {
                let found = d.call("accessibility_find", find.clone())?;
                if let Some(node) = found["nodes"].as_array().and_then(|n| n.first()) {
                    return Ok(format!("wait found {}", self.node_line(vm, node)));
                }
                if Instant::now() + POLL >= until {
                    bail!(
                        "no element matching {} appeared in time",
                        serde_json::to_string(&find).unwrap_or_default()
                    )
                }
                std::thread::sleep(POLL);
            }
        }
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .as_millis() as u64;
        let mut wait = json!({"id":vm,"timeout_ms":timeout.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1, remaining.max(1)),
            "settle_ms":step["settle_ms"].as_u64().unwrap_or(DEFAULT_SETTLE_MS)});
        if frame["frame_id"].is_string() {
            wait["frame_id"] = frame["frame_id"].clone();
        }
        let reply = d.call("screen_wait", wait)?;
        Ok(format!(
            "wait changed={} settled={} {} ms",
            reply["changed"], reply["settled"], reply["elapsed_ms"]
        ))
    }
}

/// The `vm_input_batch` tool (input permission).
pub fn batch_tool() -> Value {
    let int = json!({"type":"integer"});
    let step = json!({"type":"object","properties":{
        "type":{"type":"string","enum":STEP_TYPES},
        "x":{"type":"number"},"y":{"type":"number"},"end_x":{"type":"number"},"end_y":{"type":"number"},
        "ref":{"type":"string","description":"click/scroll: the center of an element read in this session; a11y: the element"},
        "button":{"type":"string","enum":["left","middle","right"]},"direction":{"type":"string","enum":["up","down","left","right"]},
        "ticks":{"type":"integer","minimum":1,"maximum":20},"keys":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":8},
        "hold_ms":int,"text":{"type":"string"},"action":{"type":"string","enum":crate::desktop_tools::ACCESSIBILITY_ACTIONS},
        "action_name":{"type":"string"},"value":{"type":"number"},
        "coordinate_space":{"type":"string","enum":["source_pixels","points"]},"point_pixel_scale":{"type":"number"},
        "settle_ms":int,"timeout_ms":int,"until_text":{"type":"string"},"until_name":{"type":"string"},"until_role":{"type":"string"}},
        "required":["type"],"additionalProperties":false});
    json!({"name":"vm_input_batch","description":"Run up to 50 input steps in order in one call and stop at the first failure: click, scroll (x,y or an element ref's center), drag, key, text (as the single input tools), a11y (ref, action as vm_accessibility_action) and wait (settle_ms; or until_text appears on screen; or until an element with until_name/until_role exists). Coordinates are in the pixels of the frame given; each input step stays bound to a frame the backend checks, renewed after 4 s with the batch failing if the screen's size or generation changed. Returns the steps done, the failed step's index and error, and observe as on the single tools (also on failure). timeout_ms bounds the whole batch.","inputSchema":{"type":"object","properties":{
        "id":{"type":"string"},"frame_id":{"type":"string"},"width":int,"height":int,"source_generation":int,
        "steps":{"type":"array","minItems":1,"maxItems":MAX_BATCH_STEPS,"items":step},
        "timeout_ms":{"type":"integer","minimum":1,"maximum":MAX_BATCH_TIMEOUT_MS,"default":DEFAULT_BATCH_TIMEOUT_MS},
        "observe":observe_schema()},"required":["id","steps"],"additionalProperties":false}})
}

#[cfg(test)]
#[path = "agent_view_tests.rs"]
mod tests;
