//! Fixture tests for the agent view: a scripted daemon and `vm-vision` stand-in,
//! no daemon process, Guest or helper. The measurement at the end compares one
//! desktop action through the earlier MCP surface with the same action now.
use super::*;

const LINUX: &str = "linux-desktop";
const QEMU: &str = "qemu-desktop";

/// A desktop with a text editor whose unsaved-changes dialog closes on Save.
struct Fixture {
    qemu: bool,
    width: u64,
    height: u64,
    /// Bytes of one capture (a busy 1920x1080 desktop PNG is several hundred KiB).
    capture_bytes: usize,
    /// Screen content version; every action draws.
    version: u64,
    saved: bool,
    frames: u64,
    calls: Vec<String>,
    /// Bytes of every reply the daemon sent this layer.
    daemon_bytes: usize,
    /// A sign-up form in front: its five fields and whether it was submitted.
    form: Option<Vec<String>>,
    submitted: bool,
    focused: Option<usize>,
    inputs: u64,
    /// The display changes size after this many physical inputs.
    resize_after: Option<u64>,
}
impl Fixture {
    fn new(qemu: bool) -> Self {
        Self {
            qemu,
            width: 1920,
            height: 1080,
            capture_bytes: 420 * 1024,
            version: 0,
            saved: false,
            frames: 0,
            calls: Vec::new(),
            daemon_bytes: 0,
            form: None,
            submitted: false,
            focused: None,
            inputs: 0,
            resize_after: None,
        }
    }
    fn with_form(qemu: bool) -> Self {
        Self {
            form: Some(vec![String::new(); 5]),
            ..Self::new(qemu)
        }
    }
    fn frame_id(&mut self) -> String {
        self.frames += 1;
        format!("00000000-0000-4000-8000-{:012}", self.frames)
    }
    fn image(&self) -> Vec<u8> {
        let mut image = format!("screen-{}", self.version).into_bytes();
        image.resize(self.capture_bytes, 0);
        image
    }
    fn tree(&self, args: &Value) -> Value {
        let mut nodes = match &self.form {
            Some(fields) => form(fields, self.submitted),
            None => Vec::new(),
        };
        nodes.extend(desktop(self.saved || self.form.is_some()));
        let max = args["max_nodes"].as_u64().unwrap_or(400) as usize;
        let truncated = nodes.len() > max;
        let nodes: Vec<Value> = nodes.into_iter().take(max).collect();
        let mut tree = json!({"nodes":nodes,"count":nodes.len(),"visited":612,"coordinate_space":"screen-pixels"});
        if truncated {
            tree["truncated"] = json!("max_nodes");
        }
        tree
    }
}
impl Daemon for Fixture {
    fn call(&mut self, method: &str, args: Value) -> Result<Value> {
        self.calls.push(method.into());
        let reply = match method {
            "screenshot" => {
                let data = base64::engine::general_purpose::STANDARD.encode(self.image());
                let mut shot = json!({"frame_id":self.frame_id(),"mime_type":"image/png","data_base64":data,
                    "byte_length":self.capture_bytes,"width":self.width,"height":self.height,"source_width":self.width,
                    "source_height":self.height,"image_width":self.width,"image_height":self.height,
                    "coordinate_space":"guest-display-physical-pixels","source_generation":1,"input_anchor_available":true,
                    "capture_requested_at_us":1,"captured_at_us":2,"includes_cursor":false,"capture_duration_ms":21.5,
                    "capture_source":if self.qemu {"qemu-console"} else {"guest-drm-scanout"},"resized":false});
                if self.qemu {
                    shot["source_sequence"] = json!(self.version);
                } else {
                    shot["sha256"] = json!(format!("{:064x}", self.version));
                }
                shot
            }
            "screen_wait" => json!({"changed":true,"settled":true,"resized":false,"first_change_ms":40,"elapsed_ms":340}),
            "accessibility_tree" if self.qemu => bail!("unsupported: desktop window and application tools require an Apple Virtualization Linux desktop or macOS container"),
            "accessibility_tree" => self.tree(&args),
            "accessibility_find" => {
                let all = self.tree(&json!({"max_nodes":2000}));
                let name = args["name"].as_str().map(str::to_lowercase);
                let found: Vec<Value> = all["nodes"].as_array().unwrap().iter().filter(|n| {
                    name.as_ref().is_none_or(|w| n["name"].as_str().is_some_and(|t| t.to_lowercase().contains(w)))
                        && args["role"].as_str().is_none_or(|r| n["role"] == r)
                }).take(args["max_nodes"].as_u64().unwrap_or(400) as usize).cloned().collect();
                json!({"nodes":found,"count":found.len(),"visited":612})
            }
            "accessibility_action" => {
                assert!(args["ref"].as_str().unwrap().contains('|'), "the Guest gets its own ref");
                self.version += 1;
                self.saved = true;
                let target = args["ref"].as_str().unwrap();
                if let Some(fields) = &mut self.form {
                    if let Some(i) = (0..5).find(|i| target == form_ref(*i)) {
                        assert_eq!(args["action"], "set_text");
                        fields[i] = args["text"].as_str().unwrap().into();
                    } else if target == form_ref(5) {
                        self.submitted = true;
                    }
                }
                json!({"ref":args["ref"],"action":args["action"],"done":true,"element_after":null})
            }
            m if m.starts_with("input_") => {
                assert_eq!(args.get("observe"), None, "observe stays in the adapter");
                assert_eq!(args["frame_id"].as_str().unwrap().len(), 36, "every input is frame-bound");
                self.version += 1;
                self.saved = true;
                self.inputs += 1;
                if self.resize_after.is_some_and(|n| self.inputs >= n) {
                    self.width = 1280;
                }
                if let Some(fields) = &mut self.form {
                    let (x, y) = (args["x"].as_i64().unwrap_or(-1), args["y"].as_i64().unwrap_or(-1));
                    if m == "input_click" {
                        if (760..1160).contains(&x) {
                            self.focused = (0..5).find(|i| (300 + i * 60..332 + i * 60).contains(&(y as usize)));
                        }
                        if (880..1040).contains(&x) && (624..656).contains(&y) {
                            self.submitted = true;
                        }
                    }
                    if m == "input_text" {
                        fields[self.focused.unwrap()].push_str(args["text"].as_str().unwrap());
                    }
                }
                json!({"dispatched":true,"steps":3,"guest_delivery_confirmed":false,"note":"Backend dispatch acknowledgement is not application acceptance; physical keys use Guest layout."})
            }
            other => bail!("fixture has no {other}"),
        };
        self.daemon_bytes += serde_json::to_vec(&reply)?.len();
        Ok(reply)
    }
    fn vision(&mut self, args: &[String], image: &[u8], before: Option<&[u8]>) -> Result<Value> {
        self.calls.push(format!("vision {}", args[0]));
        let flag = |name: &str| {
            args.iter()
                .position(|a| a == name)
                .map(|i| args[i + 1].clone())
        };
        Ok(match args[0].as_str() {
            "image" => {
                let (w, h) = (self.width as f64, self.height as f64);
                let crop: Vec<f64> = flag("--crop").map_or(vec![0.0, 0.0, w, h], |c| {
                    c.split(',').map(|n| n.parse().unwrap()).collect()
                });
                let max_w = flag("--max-width").map_or(f64::MAX, |v| v.parse().unwrap());
                let max_h = flag("--max-height").map_or(f64::MAX, |v| v.parse().unwrap());
                let scale = (max_w / crop[2]).min(max_h / crop[3]).min(1.0);
                let (iw, ih) = ((crop[2] * scale).round(), (crop[3] * scale).round());
                // About 0.1 bytes per pixel for a desktop JPEG at quality 70.
                let bytes = (iw * ih / 10.0) as usize;
                json!({"source_width":self.width,"source_height":self.height,"image_width":iw as u64,"image_height":ih as u64,
                    "crop":{"x":crop[0] as u64,"y":crop[1] as u64,"width":crop[2] as u64,"height":crop[3] as u64},
                    "scale":iw / crop[2],"mime_type":"image/jpeg","byte_length":bytes,
                    "data_base64":base64::engine::general_purpose::STANDARD.encode(vec![0u8; bytes])})
            }
            "diff" => {
                assert_ne!(before.unwrap(), image, "diff compares two captures");
                json!({"width":self.width,"height":self.height,"resized":false,"cell":16,"region_count":2,"changed_fraction":0.083,
                    "regions":[{"x":704,"y":384,"width":512,"height":256,"center_x":960,"center_y":512},
                               {"x":320,"y":0,"width":640,"height":32,"center_x":640,"center_y":16}]})
            }
            _ => {
                let crop = flag("--crop");
                let mut text = json!({"width":self.width,"height":self.height,"recognizer":"vision-accurate","lines":[
                    {"text":"notes.txt - gedit","confidence":1.0,"x":560,"y":6,"width":160,"height":18,"center_x":640,"center_y":15},
                    {"text":"Saved","confidence":0.9,"x":900,"y":500,"width":60,"height":20,"center_x":930,"center_y":510},
                    {"text":if crop.is_some() {"(cropped read)"} else {"Activities"},"confidence":0.8,"x":10,"y":1040,"width":90,"height":20,"center_x":55,"center_y":1050}]});
                if self.submitted {
                    text["lines"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!({"text":"Thanks, you are signed up","confidence":0.95,
                        "x":760,"y":670,"width":400,"height":24,"center_x":960,"center_y":682}));
                }
                text
            }
        })
    }
}

const FIELDS: [&str; 5] = ["Name", "Email", "Phone", "City", "Company"];
const VALUES: [&str; 5] = [
    "Ada Lovelace",
    "ada@example.com",
    "+44 20 7946 0000",
    "London",
    "Analytical Engines",
];

fn form_ref(i: usize) -> String {
    format!(":1.77|/org/a11y/atspi/accessible/90{i}")
}

/// A sign-up dialog: five labelled entries and Submit, then a thank-you label.
fn form(fields: &[String], submitted: bool) -> Vec<Value> {
    let mut nodes = vec![
        json!({"ref":":1.77|/org/a11y/atspi/accessible/1","depth":0,"role":"application","name":"signup","pid":2001}),
        json!({"ref":":1.77|/org/a11y/atspi/accessible/2","depth":1,"role":"frame","name":"Sign up","states":["active"],
            "box":{"x":640,"y":200,"width":640,"height":520,"center_x":960,"center_y":460}}),
    ];
    for (i, label) in FIELDS.iter().enumerate() {
        let y = 300 + i as i64 * 60;
        nodes.push(json!({"ref":format!(":1.77|/org/a11y/atspi/accessible/8{i}"),"depth":2,"role":"label","name":label,
            "box":{"x":660,"y":y,"width":90,"height":32,"center_x":705,"center_y":y+16}}));
        let mut entry = json!({"ref":form_ref(i),"depth":2,"role":"text","name":label,"states":["editable","single_line"],
            "actions":["activate"],"box":{"x":760,"y":y,"width":400,"height":32,"center_x":960,"center_y":y+16}});
        if !fields[i].is_empty() {
            entry["text"] = json!(fields[i]);
        }
        nodes.push(entry);
    }
    nodes.push(
        json!({"ref":form_ref(5),"depth":2,"role":"push button","name":"Submit","actions":["click"],
        "box":{"x":880,"y":624,"width":160,"height":32,"center_x":960,"center_y":640}}),
    );
    if submitted {
        nodes.push(json!({"ref":":1.77|/org/a11y/atspi/accessible/99","depth":2,"role":"label","name":"Thanks, you are signed up",
            "box":{"x":760,"y":670,"width":400,"height":24,"center_x":960,"center_y":682}}));
    }
    nodes
}

/// A GNOME-like AT-SPI tree as the guest agent reports it: shell panel, a file
/// manager with a long list, and gedit with menus, a toolbar and (until saved)
/// its unsaved-changes dialog.
fn desktop(saved: bool) -> Vec<Value> {
    let mut nodes = Vec::new();
    let mut path = 100u64;
    let mut add = |bus: &str, depth: u64, role: &str, name: &str, extra: Value, b: [i64; 4]| {
        path += 1;
        let mut node = json!({"ref":format!("{bus}|/org/a11y/atspi/accessible/{path}"),"depth":depth,"role":role});
        if !name.is_empty() {
            node["name"] = json!(name);
        }
        if b[2] > 0 {
            node["box"] = json!({"x":b[0],"y":b[1],"width":b[2],"height":b[3],"center_x":b[0]+b[2]/2,"center_y":b[1]+b[3]/2});
        }
        for (k, v) in extra.as_object().unwrap() {
            node[k] = v.clone();
        }
        nodes.push(node);
        nodes.len()
    };
    let click = json!({"actions":["click"]});
    add(
        ":1.12",
        0,
        "application",
        "gnome-shell",
        json!({"pid":1201}),
        [0, 0, 0, 0],
    );
    add(":1.12", 1, "panel", "", json!({}), [0, 0, 1920, 32]);
    for (i, name) in [
        "Activities",
        "Applications",
        "Places",
        "Mon 26 Oct 10:42",
        "System",
        "Volume",
        "Network",
        "Battery",
    ]
    .iter()
    .enumerate()
    {
        add(
            ":1.12",
            2,
            "push button",
            name,
            click.clone(),
            [i as i64 * 140, 0, 130, 32],
        );
        add(
            ":1.12",
            3,
            "label",
            name,
            json!({}),
            [i as i64 * 140 + 8, 6, 110, 20],
        );
    }
    add(
        ":1.40",
        0,
        "application",
        "nautilus",
        json!({"pid":1544}),
        [0, 0, 0, 0],
    );
    add(
        ":1.40",
        1,
        "frame",
        "Documents",
        json!({"states":["active"]}),
        [40, 60, 900, 700],
    );
    add(":1.40", 2, "panel", "", json!({}), [40, 60, 900, 48]);
    for (i, name) in ["Back", "Forward", "Search", "View options", "Menu"]
        .iter()
        .enumerate()
    {
        add(
            ":1.40",
            3,
            "push button",
            name,
            click.clone(),
            [50 + i as i64 * 44, 66, 36, 36],
        );
    }
    add(
        ":1.40",
        2,
        "scroll pane",
        "",
        json!({}),
        [40, 108, 900, 652],
    );
    add(":1.40", 3, "table", "Files", json!({}), [40, 108, 900, 652]);
    for i in 0..110 {
        add(
            ":1.40",
            4,
            "table cell",
            &format!("report-2026-{:02}-{:02}.pdf", i % 12 + 1, i % 28 + 1),
            json!({"actions":["activate"]}),
            [40, 108 + i * 24, 600, 24],
        );
        if i % 2 == 0 {
            add(
                ":1.40",
                5,
                "label",
                "142 kB",
                json!({}),
                [700, 108 + i * 24, 80, 24],
            );
        }
    }
    add(
        ":1.51",
        0,
        "application",
        "gedit",
        json!({"pid":1610}),
        [0, 0, 0, 0],
    );
    add(
        ":1.51",
        1,
        "frame",
        if saved {
            "notes.txt - gedit"
        } else {
            "*notes.txt - gedit"
        },
        json!({"states":["active"]}),
        [320, 40, 1280, 960],
    );
    add(":1.51", 2, "menu bar", "", json!({}), [320, 72, 1280, 28]);
    for (i, menu) in [
        "File",
        "Edit",
        "View",
        "Search",
        "Tools",
        "Documents",
        "Help",
    ]
    .iter()
    .enumerate()
    {
        add(
            ":1.51",
            3,
            "menu",
            menu,
            click.clone(),
            [330 + i as i64 * 70, 72, 64, 28],
        );
        for item in 0..6 {
            add(
                ":1.51",
                4,
                "menu item",
                &format!("{menu} item {item}"),
                click.clone(),
                [0, 0, 0, 0],
            );
        }
    }
    add(":1.51", 2, "tool bar", "", json!({}), [320, 100, 1280, 40]);
    for (i, tool) in [
        "Open", "Save", "Undo", "Redo", "Cut", "Copy", "Paste", "Find", "Replace",
    ]
    .iter()
    .enumerate()
    {
        add(
            ":1.51",
            3,
            "push button",
            tool,
            click.clone(),
            [330 + i as i64 * 40, 104, 32, 32],
        );
    }
    add(
        ":1.51",
        2,
        "scroll pane",
        "",
        json!({}),
        [320, 140, 1280, 820],
    );
    add(
        ":1.51",
        3,
        "text",
        "",
        json!({"text":"Meeting notes\nAction items: ship the compact tree","states":["editable","multi_line","focused"]}),
        [320, 140, 1280, 820],
    );
    add(
        ":1.51",
        2,
        "status bar",
        "",
        json!({}),
        [320, 960, 1280, 40],
    );
    add(
        ":1.51",
        3,
        "label",
        if saved { "Saved" } else { "Ln 2, Col 38" },
        json!({}),
        [1400, 966, 180, 28],
    );
    for i in 0..40 {
        add(
            ":1.51",
            3,
            "filler",
            "",
            json!({}),
            [320 + i * 30, 960, 30, 40],
        );
    }
    if !saved {
        add(
            ":1.51",
            1,
            "dialog",
            "Save changes to document “notes.txt” before closing?",
            json!({"states":["modal","active"]}),
            [704, 384, 512, 256],
        );
        add(
            ":1.51",
            2,
            "label",
            "If you don’t save, changes will be permanently lost.",
            json!({}),
            [724, 440, 472, 40],
        );
        for (i, button) in ["Close without Saving", "Cancel", "Save"]
            .iter()
            .enumerate()
        {
            add(
                ":1.51",
                2,
                "push button",
                button,
                json!({"actions":["click"],"states":if *button == "Save" {json!(["focused","is_default"])} else {json!([])}}),
                [724 + i as i64 * 160, 580, 150, 36],
            );
        }
    }
    // The rest of the shell's hidden-but-showing chrome.
    let mut count = add(":1.12", 1, "panel", "", json!({}), [0, 1048, 1920, 32]);
    while count < 420 {
        count = add(
            ":1.12",
            2 + count as u64 % 4,
            "panel",
            "",
            json!({}),
            [0, 1048, 1920, 32],
        );
    }
    nodes
}

fn content_text(content: &Value) -> String {
    content["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn compact_tree_uses_short_refs_that_actions_accept() {
    let mut d = Fixture::new(false);
    let mut session = Session::default();
    let tree = session
        .call(&mut d, "vm_accessibility_tree", json!({"id":LINUX}))
        .unwrap();
    let text = content_text(&tree);
    assert!(
        text.starts_with("400 elements, 612 visited; truncated: max_nodes"),
        "{}",
        &text[..80]
    );
    let save = text
        .lines()
        .find(|l| l.contains("push button \"Save\"") && l.contains("focused"))
        .unwrap();
    assert_eq!(
        save.trim(),
        format!(
            "{} push button \"Save\" @1119,598 [focused,is_default]",
            save.trim().split(' ').next().unwrap()
        )
    );
    assert!(save.starts_with("  e"), "indented by depth: {save}");
    let short = save.trim().split(' ').next().unwrap().to_string();
    let reply = session
        .call(
            &mut d,
            "vm_accessibility_action",
            json!({"id":LINUX,"ref":short,"action":"do"}),
        )
        .unwrap();
    let reply: Value = serde_json::from_str(&content_text(&reply)).unwrap();
    assert_eq!(
        reply["ref"], short,
        "the reply names the element by the same short ref"
    );
    // A ref this session never issued is refused before anything is sent.
    let calls = d.calls.len();
    let error = session
        .call(
            &mut d,
            "vm_accessibility_action",
            json!({"id":LINUX,"ref":"e999999","action":"do"}),
        )
        .unwrap_err();
    assert!(error.to_string().contains("unknown ref"), "{error}");
    assert_eq!(d.calls.len(), calls);
    // The JSON form is the Guest's own.
    let json_tree = session
        .call(
            &mut d,
            "vm_accessibility_tree",
            json!({"id":LINUX,"format":"json"}),
        )
        .unwrap();
    let json_tree: Value = serde_json::from_str(&content_text(&json_tree)).unwrap();
    assert!(json_tree["nodes"][0]["ref"]
        .as_str()
        .unwrap()
        .contains("|/org/a11y/"));
}

#[test]
fn interactive_only_reads_deeper_and_keeps_context() {
    let mut d = Fixture::new(false);
    let mut session = Session::default();
    let tree = session
        .call(
            &mut d,
            "vm_accessibility_tree",
            json!({"id":LINUX,"interactive_only":true,"max_nodes":60}),
        )
        .unwrap();
    let text = content_text(&tree);
    let lines: Vec<&str> = text.lines().skip(1).collect();
    assert_eq!(lines.len(), 60);
    assert!(text.starts_with("60 elements (interactive only)"));
    assert!(
        lines
            .iter()
            .all(|l| !l.contains(" filler") && !l.contains(" label ")),
        "{text}"
    );
    assert!(lines[0].contains("application \"gnome-shell\" pid=1201"));
    assert!(
        !lines[0].starts_with(' '),
        "no indentation for a filtered list"
    );
    // The Guest was asked for more than is shown.
    let tree = Fixture::new(false).tree(&json!({"max_nodes":240}));
    assert_eq!(tree["count"], 240);
}

#[test]
fn node_lines_are_bounded_and_escaped() {
    let node = json!({"ref":"x|/a","depth":3,"role":"text","name":"say \"hi\"\nnow","text":"x".repeat(500),
        "value":0.5,"states":["editable"],"actions":["activate","copy"],"box":{"x":0,"y":0,"width":10,"height":10,"center_x":5,"center_y":5},
        "unexpanded_children":7,"context":"gedit > notes"});
    let line = node_body("e3", &node);
    assert!(
        line.starts_with("e3 text \"say \\\"hi\\\"\\nnow\" =\"xxx"),
        "{line}"
    );
    assert!(
        line.contains("…\" value=0.5 @5,5 [editable] {activate,copy} +7 in \"gedit > notes\""),
        "{line}"
    );
    assert!(line.len() < 260);
    let window = json!({"role":"dialog","name":"Confirm","box":{"x":10,"y":20,"width":300,"height":200,"center_x":160,"center_y":120}});
    assert_eq!(
        node_body("e4", &window),
        "e4 dialog \"Confirm\" @160,120 300x200"
    );
    let mac = json!({"role":"button","subrole":"close button","placeholder":"Search"});
    assert_eq!(
        node_body("e5", &mac),
        "e5 button:close button placeholder \"Search\""
    );
}

#[test]
fn short_refs_are_stable_and_never_reused() {
    let mut session = Session::default();
    let a = session.short_ref(LINUX, "b|/1");
    assert_eq!(session.short_ref(LINUX, "b|/1"), a);
    let b = session.short_ref(LINUX, "b|/2");
    assert_ne!(a, b);
    session
        .vm(LINUX)
        .long
        .extend((0..MAX_REFS).map(|i| (format!("f|/{i}"), format!("x{i}"))));
    // The next new element clears the full map; numbers continue.
    let c = session.short_ref(LINUX, "b|/3");
    assert_eq!(session.vm(LINUX).long.len(), 1);
    assert!(!session.vm(LINUX).short.contains_key(&a));
    let again = session.short_ref(LINUX, "b|/1");
    assert!(again != a && again != b && again != c);
}

#[test]
fn observe_returns_the_tree_delta_and_a_fresh_frame() {
    let mut d = Fixture::new(false);
    let mut session = Session::default();
    session
        .call(
            &mut d,
            "vm_accessibility_tree",
            json!({"id":LINUX,"interactive_only":true}),
        )
        .unwrap();
    let shot = session
        .call(&mut d, "vm_screenshot", json!({"id":LINUX}))
        .unwrap();
    let shot: Value = serde_json::from_str(content_text(&shot).as_str()).unwrap();
    d.calls.clear();
    let reply = session
        .call(&mut d, "vm_input_click", json!({"id":LINUX,"frame_id":shot["frame_id"],"width":1920,"height":1080,"source_generation":1,"x":1119,"y":598,"observe":{}}))
        .unwrap();
    assert_eq!(
        d.calls,
        [
            "input_click",
            "screen_wait",
            "screenshot",
            "accessibility_tree"
        ]
    );
    let parts = reply["content"].as_array().unwrap();
    let head: Value = serde_json::from_str(parts[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(head["frame"]["frame_id"].as_str().unwrap().len(), 36);
    assert_ne!(head["frame"]["frame_id"], shot["frame_id"]);
    assert_eq!(head["observed"]["changed"], true);
    assert!(head.get("note").is_none());
    let delta = parts[1]["text"].as_str().unwrap();
    assert!(delta.starts_with("delta: +0 ~1 -4 of"), "{delta}");
    assert!(
        delta.contains("~ ") && delta.contains("frame \"notes.txt - gedit\""),
        "{delta}"
    );
    assert!(
        delta.contains("- ") && delta.contains("push button \"Save\""),
        "{delta}"
    );
    // The next input chains on the returned frame without a screenshot.
    d.calls.clear();
    let mut next = json!({"id":LINUX,"width":1920,"height":1080,"source_generation":1,"keys":["ctrl","s"],"observe":{"return":"none"}});
    next["frame_id"] = head["frame"]["frame_id"].clone();
    let reply = session.call(&mut d, "vm_input_key", next).unwrap();
    assert_eq!(d.calls, ["input_key", "screen_wait", "screenshot"]);
    assert_eq!(reply["content"].as_array().unwrap().len(), 1);
}

#[test]
fn observe_without_a_tree_reports_changed_regions_and_their_text() {
    let mut d = Fixture::new(true);
    let mut session = Session::default();
    let shot = session
        .call(&mut d, "vm_screenshot", json!({"id":QEMU}))
        .unwrap();
    let shot: Value = serde_json::from_str(content_text(&shot).as_str()).unwrap();
    d.calls.clear();
    let reply = session
        .call(&mut d, "vm_input_click", json!({"id":QEMU,"frame_id":shot["frame_id"],"width":1920,"height":1080,"source_generation":1,"x":1119,"y":598,"observe":{"return":"delta"}}))
        .unwrap();
    assert_eq!(
        d.calls,
        [
            "input_click",
            "screen_wait",
            "screenshot",
            "accessibility_tree",
            "vision diff",
            "vision text"
        ]
    );
    let text = content_text(&reply);
    assert!(text.contains("delta: 2 changed regions (8.3% of screen)\nregion @960,512 512x256\nregion @640,16 640x32"), "{text}");
    // Only lines inside the changed regions are kept.
    assert!(
        !text.contains("(cropped read)") && text.contains("\"Saved\" @930,510"),
        "{text}"
    );
    assert!(text.contains("\"notes.txt - gedit\" @640,15"), "{text}");
    // Known to have no tree now: the next delta goes straight to pixels.
    d.calls.clear();
    let head: Value = serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
    let mut next = json!({"id":QEMU,"width":1920,"height":1080,"source_generation":1,"x":10,"y":10,"observe":{}});
    next["frame_id"] = head["frame"]["frame_id"].clone();
    session.call(&mut d, "vm_input_click", next).unwrap();
    assert_eq!(
        d.calls,
        [
            "input_click",
            "screen_wait",
            "screenshot",
            "vision diff",
            "vision text"
        ]
    );
}

#[test]
fn observe_screenshot_scales_and_reports_the_mapping() {
    let mut d = Fixture::new(true);
    let mut session = Session::default();
    let shot = session
        .call(&mut d, "vm_screenshot", json!({"id":QEMU,"max_width":1280}))
        .unwrap();
    let meta: Value = serde_json::from_str(shot["content"][1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        (
            meta["width"].as_u64(),
            meta["image_width"].as_u64(),
            meta["image_height"].as_u64()
        ),
        (Some(1920), Some(1280), Some(720))
    );
    assert_eq!(meta["mime_type"], "image/jpeg");
    assert_eq!(shot["content"][0]["mimeType"], "image/jpeg");
    assert!((meta["image_point_pixel_scale"].as_f64().unwrap() - 1.5).abs() < 1e-9);
    // Image coordinates become source pixels through the points form.
    let args = json!({"x":400,"y":300,"coordinate_space":"points","point_pixel_scale":meta["image_point_pixel_scale"]});
    let pixels = crate::input::source_pixel_arguments("input_click", &args).unwrap();
    assert_eq!(
        (pixels["x"].as_u64(), pixels["y"].as_u64()),
        (Some(600), Some(450))
    );
    let crop = session
        .call(
            &mut d,
            "vm_screenshot",
            json!({"id":QEMU,"region":{"x":704,"y":384,"width":512,"height":256}}),
        )
        .unwrap();
    let meta: Value = serde_json::from_str(crop["content"][1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        meta["image_region"],
        json!({"x":704,"y":384,"width":512,"height":256})
    );
    assert!(
        meta.get("image_point_pixel_scale").is_none(),
        "a crop needs the offset too"
    );
    for bad in [
        json!({"region":{"x":1800,"y":0,"width":400,"height":10}}),
        json!({"max_width":4}),
        json!({"format":"gif"}),
    ] {
        let mut args = bad;
        args["id"] = json!(QEMU);
        assert!(session.call(&mut d, "vm_screenshot", args).is_err());
    }
    let reply = session
        .call(&mut d, "vm_input_scroll", json!({"id":QEMU,"frame_id":meta["frame_id"],"width":1920,"height":1080,"source_generation":1,"x":5,"y":5,"direction":"down","ticks":2,"observe":{"return":"screenshot","max_width":1568}}))
        .unwrap();
    let parts = reply["content"].as_array().unwrap();
    assert_eq!(parts[1]["type"], "image");
    let meta: Value = serde_json::from_str(parts[2]["text"].as_str().unwrap()).unwrap();
    assert_eq!(meta["image_width"], 1568);
}

#[test]
fn observe_arguments_are_checked() {
    for (observe, ok) in [
        (json!({}), true),
        (json!({"return":"text","settle_ms":0,"timeout_ms":0}), true),
        (
            json!({"return":"screenshot","max_width":800,"quality":60}),
            true,
        ),
        (json!({"return":"tree"}), false),
        (json!({"timeout_ms":25001}), false),
        (json!({"max_width":800}), false),
        (
            json!({"return":"screenshot","region":{"x":0,"y":0,"width":1,"height":1}}),
            false,
        ),
        (json!({"wait":true}), false),
        (json!(true), false),
    ] {
        let mut args = json!({"observe":observe});
        assert_eq!(Observe::take(&mut args).is_ok(), ok, "{args}");
    }
    let mut args = json!({"observe":{}});
    let observe = Observe::take(&mut args).unwrap().unwrap();
    assert_eq!(
        (observe.ret, observe.settle_ms, observe.timeout_ms),
        ("delta", 300, 2000)
    );
    assert_eq!(args, json!({}));
}

#[test]
fn an_old_frame_of_an_unchanged_screen_is_reanchored() {
    let mut d = Fixture::new(false);
    let mut session = Session::default();
    let shot = session
        .call(&mut d, "vm_screenshot", json!({"id":LINUX}))
        .unwrap();
    let shot: Value = serde_json::from_str(content_text(&shot).as_str()).unwrap();
    let click = json!({"id":LINUX,"frame_id":shot["frame_id"],"width":1920,"height":1080,"source_generation":1,"x":5,"y":5});
    // Fresh: passed through untouched.
    d.calls.clear();
    session
        .call(&mut d, "vm_input_click", click.clone())
        .unwrap();
    assert_eq!(d.calls, ["input_click"]);
    // The click drew; an aged frame of the old screen is refused before input.
    for seen in &mut session.vm(LINUX).frames {
        seen.at -= Duration::from_secs(30);
    }
    d.calls.clear();
    let error = session
        .call(&mut d, "vm_input_click", click.clone())
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("stale_frame: the screen changed since frame"),
        "{error}"
    );
    assert_eq!(d.calls, ["screenshot"]);
    // An aged frame of the screen as it still is gets a fresh anchor.
    let latest = session.vm(LINUX).frames.back().unwrap().anchor.clone();
    for seen in &mut session.vm(LINUX).frames {
        seen.at -= Duration::from_secs(30);
    }
    let mut again = click.clone();
    again["frame_id"] = latest["frame_id"].clone();
    d.calls.clear();
    let reply = session.call(&mut d, "vm_input_click", again).unwrap();
    assert_eq!(d.calls, ["screenshot", "input_click"]);
    let reply: Value = serde_json::from_str(&content_text(&reply)).unwrap();
    assert_eq!(reply["reanchored"]["from"], latest["frame_id"]);
    assert_ne!(reply["reanchored"]["to"], latest["frame_id"]);
    // QEMU frames are left to the worker, which compares the draw sequence.
    let mut q = Fixture::new(true);
    let shot = session
        .call(&mut q, "vm_screenshot", json!({"id":QEMU}))
        .unwrap();
    let shot: Value = serde_json::from_str(content_text(&shot).as_str()).unwrap();
    for seen in &mut session.vm(QEMU).frames {
        seen.at -= Duration::from_secs(30);
    }
    q.calls.clear();
    let mut click = click;
    click["id"] = json!(QEMU);
    click["frame_id"] = shot["frame_id"].clone();
    session.call(&mut q, "vm_input_click", click).unwrap();
    assert_eq!(q.calls, ["input_click"]);
}

#[test]
fn accessibility_action_observes_from_the_last_seen_frame() {
    let mut d = Fixture::new(false);
    let mut session = Session::default();
    let tree = content_text(
        &session
            .call(
                &mut d,
                "vm_accessibility_tree",
                json!({"id":LINUX,"interactive_only":true}),
            )
            .unwrap(),
    );
    let save = tree
        .lines()
        .find(|l| l.contains("\"Save\" @1119"))
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .to_string();
    d.calls.clear();
    // No frame seen yet: one is taken before the action, as the wait's baseline.
    let reply = session
        .call(
            &mut d,
            "vm_accessibility_action",
            json!({"id":LINUX,"ref":save,"action":"do","observe":{}}),
        )
        .unwrap();
    assert_eq!(
        d.calls,
        [
            "screenshot",
            "accessibility_action",
            "screen_wait",
            "screenshot",
            "accessibility_tree"
        ]
    );
    assert!(content_text(&reply).contains("delta: +0 ~1 -4"));
}

/// Approximate model tokens: text at 4 bytes per token; an image as Claude
/// counts it (width x height / 750 after fitting 1568 px and 1.15 megapixels).
fn tokens(content: &Value) -> (usize, usize) {
    let mut text = 0;
    let mut image = 0;
    for part in content["content"].as_array().unwrap() {
        match part["type"].as_str().unwrap() {
            "text" => text += part["text"].as_str().unwrap().len().div_ceil(4),
            _ => {
                let meta = content["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .rev()
                    .find_map(|p| {
                        p["text"]
                            .as_str()
                            .and_then(|t| serde_json::from_str::<Value>(t).ok())
                            .filter(|m| m["image_width"].is_u64())
                    });
                let meta = meta.unwrap();
                let (w, h) = (
                    meta["image_width"].as_f64().unwrap(),
                    meta["image_height"].as_f64().unwrap(),
                );
                let fit = (1568.0 / w.max(h))
                    .min((1_150_000.0 / (w * h)).sqrt())
                    .min(1.0);
                image += ((w * fit) * (h * fit) / 750.0).round() as usize;
            }
        }
    }
    (text, image)
}

struct Cost {
    calls: usize,
    text: usize,
    image: usize,
    bytes: usize,
}
impl Cost {
    fn add(&mut self, content: &Value) {
        let (text, image) = tokens(content);
        self.calls += 1;
        self.text += text;
        self.image += image;
        self.bytes += serde_json::to_vec(content).unwrap().len();
    }
}

/// The earlier MCP surface, replayed: JSON trees, full-size screenshots and a
/// separate wait and screenshot to verify each action.
fn before_flow(d: &mut Fixture, vm: &str, tree: bool) -> Cost {
    let mut cost = Cost {
        calls: 0,
        text: 0,
        image: 0,
        bytes: 0,
    };
    let shot = |d: &mut Fixture, cost: &mut Cost| {
        let shot = d.call("screenshot", json!({"id":vm})).unwrap();
        let frame = shot["frame_id"].clone();
        cost.add(&crate::screenshot_content(shot).unwrap());
        frame
    };
    if tree {
        // Read the tree, act by ref, read it again to see the result.
        let read = |d: &mut Fixture, cost: &mut Cost| {
            let tree = d.call("accessibility_tree", json!({"id":vm})).unwrap();
            cost.add(&text_content(tree.clone()).unwrap());
            tree
        };
        let tree = read(d, &mut cost);
        let save = tree["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["name"] == "Save" && n["role"] == "push button")
            .unwrap()["ref"]
            .clone();
        cost.add(
            &text_content(
                d.call(
                    "accessibility_action",
                    json!({"id":vm,"ref":save,"action":"do"}),
                )
                .unwrap(),
            )
            .unwrap(),
        );
        read(d, &mut cost);
    } else {
        // Screenshot, click, wait, screenshot to verify; plus one more screenshot
        // when the model's turn outlasted the 5 s window.
        shot(d, &mut cost);
        let frame = shot(d, &mut cost);
        let click = json!({"id":vm,"frame_id":frame,"width":1920,"height":1080,"source_generation":1,"x":1119,"y":598});
        cost.add(&text_content(d.call("input_click", click).unwrap()).unwrap());
        cost.add(
            &text_content(
                d.call("screen_wait", json!({"id":vm,"frame_id":frame}))
                    .unwrap(),
            )
            .unwrap(),
        );
        shot(d, &mut cost);
    }
    cost
}

/// The same action now: the compact tree (or the frame from the previous
/// observing action) and one observing call.
fn after_flow(d: &mut Fixture, vm: &str, tree: bool, ret: &str) -> Cost {
    let mut session = Session::default();
    let mut cost = Cost {
        calls: 0,
        text: 0,
        image: 0,
        bytes: 0,
    };
    if tree {
        let content = session
            .call(
                d,
                "vm_accessibility_tree",
                json!({"id":vm,"interactive_only":true}),
            )
            .unwrap();
        cost.add(&content);
        let save = content_text(&content)
            .lines()
            .find(|l| l.contains("\"Save\" @1119"))
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_string();
        cost.add(
            &session
                .call(
                    d,
                    "vm_accessibility_action",
                    json!({"id":vm,"ref":save,"action":"do","observe":{"return":ret}}),
                )
                .unwrap(),
        );
    } else {
        // The frame a previous observing action returned (not counted: it came with that action).
        let mut prior = Cost {
            calls: 0,
            text: 0,
            image: 0,
            bytes: 0,
        };
        let first = session
            .call(d, "vm_screenshot", json!({"id":vm,"max_width":1568}))
            .unwrap();
        prior.add(&first);
        let meta: Value =
            serde_json::from_str(first["content"][1]["text"].as_str().unwrap()).unwrap();
        let mut observe = json!({"return":ret});
        if ret == "screenshot" {
            observe["max_width"] = json!(1568);
        }
        let click = json!({"id":vm,"frame_id":meta["frame_id"],"width":1920,"height":1080,"source_generation":1,"x":1119,"y":598,"observe":observe});
        cost.add(&session.call(d, "vm_input_click", click).unwrap());
    }
    cost
}

#[test]
fn measure_one_desktop_action_before_and_after() {
    let mut rows = Vec::new();
    let mut check = |label: &str, before: Cost, after: Cost| {
        rows.push(format!(
            "| {label} | {} | {} | {} | {} | {} | {} | {} | {} |",
            before.calls,
            before.text,
            before.image,
            before.bytes,
            after.calls,
            after.text,
            after.image,
            after.bytes
        ));
        assert!(
            after.calls < before.calls && after.text + after.image < before.text + before.image,
            "{label}"
        );
    };
    check(
        "Linux desktop, a11y: tree, action, verify (delta)",
        before_flow(&mut Fixture::new(false), LINUX, true),
        after_flow(&mut Fixture::new(false), LINUX, true, "delta"),
    );
    check(
        "Linux desktop, a11y: tree, action, verify (compact_tree)",
        before_flow(&mut Fixture::new(false), LINUX, true),
        after_flow(&mut Fixture::new(false), LINUX, true, "compact_tree"),
    );
    check(
        "QEMU 1920x1080, click and verify (delta: regions + OCR)",
        before_flow(&mut Fixture::new(true), QEMU, false),
        after_flow(&mut Fixture::new(true), QEMU, false, "delta"),
    );
    check(
        "QEMU 1920x1080, click and verify (screenshot 1568)",
        before_flow(&mut Fixture::new(true), QEMU, false),
        after_flow(&mut Fixture::new(true), QEMU, false, "screenshot"),
    );
    // The tree alone: the earlier default against the compact forms.
    let mut d = Fixture::new(false);
    let json_tree =
        text_content(d.call("accessibility_tree", json!({"id":LINUX})).unwrap()).unwrap();
    let mut session = Session::default();
    let compact = session
        .call(&mut d, "vm_accessibility_tree", json!({"id":LINUX}))
        .unwrap();
    let interactive = session
        .call(
            &mut d,
            "vm_accessibility_tree",
            json!({"id":LINUX,"interactive_only":true}),
        )
        .unwrap();
    println!(
        "compact sample:\n{}",
        content_text(&compact)
            .lines()
            .take(8)
            .collect::<Vec<_>>()
            .join("\n")
    );
    println!(
        "interactive sample:\n{}",
        content_text(&interactive)
            .lines()
            .take(6)
            .collect::<Vec<_>>()
            .join("\n")
    );
    let t = |c: &Value| tokens(c).0;
    let b = |c: &Value| serde_json::to_vec(c).unwrap().len();
    println!("tree, 400 elements: json {} tokens / {} B; compact {} / {} B; compact interactive_only {} / {} B",
        t(&json_tree), b(&json_tree), t(&compact), b(&compact), t(&interactive), b(&interactive));
    assert!(t(&compact) * 4 < t(&json_tree));
    println!("| flow | before calls | before text tok | before image tok | before bytes | after calls | after text tok | after image tok | after bytes |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for row in rows {
        println!("{row}");
    }
}

fn batch(session: &mut Session, d: &mut Fixture, args: Value) -> (Value, Value) {
    let content = session.call(d, "vm_input_batch", args).unwrap();
    let head: Value =
        serde_json::from_str(content["content"][0]["text"].as_str().unwrap()).unwrap();
    (content, head)
}

fn frame_args(session: &mut Session, d: &mut Fixture, vm: &str) -> Value {
    let shot = session.call(d, "vm_screenshot", json!({"id":vm})).unwrap();
    let shot: Value = serde_json::from_str(content_text(&shot).as_str()).unwrap();
    json!({"id":vm,"frame_id":shot["frame_id"],"width":shot["width"],"height":shot["height"],"source_generation":shot["source_generation"]})
}

fn coordinate_steps() -> Vec<Value> {
    let mut steps = Vec::new();
    for (i, value) in VALUES.iter().enumerate() {
        steps.push(json!({"type":"click","x":960,"y":316 + i * 60}));
        steps.push(json!({"type":"text","text":value}));
    }
    steps.push(json!({"type":"click","x":960,"y":640}));
    steps
}

#[test]
fn batch_fills_a_form_by_ref_and_observes_once() {
    let mut d = Fixture::with_form(false);
    let mut session = Session::default();
    let tree = content_text(
        &session
            .call(
                &mut d,
                "vm_accessibility_tree",
                json!({"id":LINUX,"interactive_only":true}),
            )
            .unwrap(),
    );
    let short = |name: &str, role: &str| {
        tree.lines()
            .find(|l| l.contains(&format!(" {role} \"{name}\"")))
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_string()
    };
    let mut steps: Vec<Value> = FIELDS
        .iter()
        .zip(VALUES)
        .map(|(f, v)| json!({"type":"a11y","ref":short(f, "text"),"action":"set_text","text":v}))
        .collect();
    steps.push(json!({"type":"a11y","ref":short("Submit", "push button"),"action":"do"}));
    steps.push(json!({"type":"wait","until_name":"Thanks"}));
    d.calls.clear();
    let (content, head) = batch(
        &mut session,
        &mut d,
        json!({"id":LINUX,"steps":steps,"observe":{}}),
    );
    assert_eq!(content["isError"], false);
    assert_eq!(
        (head["ok"].as_bool(), head["completed"].as_u64()),
        (Some(true), Some(7))
    );
    assert_eq!(d.form.as_ref().unwrap(), &VALUES.map(String::from).to_vec());
    assert!(
        head["steps"][6]
            .as_str()
            .unwrap()
            .starts_with("wait found e"),
        "{head}"
    );
    // No frame was given and the session had seen none: one is taken as the wait's baseline.
    assert_eq!(
        d.calls
            .iter()
            .filter(|c| *c == "accessibility_action")
            .count(),
        6
    );
    assert_eq!(
        &d.calls[d.calls.len() - 3..],
        ["screen_wait", "screenshot", "accessibility_tree"]
    );
    let delta = content["content"][1]["text"].as_str().unwrap();
    // The thank-you label is static text, outside interactive_only; the wait step saw it.
    assert!(delta.starts_with("delta: +0 ~5 -0"), "{delta}");
    assert!(
        delta.contains("~ e") && delta.contains("=\"ada@example.com\""),
        "{delta}"
    );
    assert!(
        head["steps"][6]
            .as_str()
            .unwrap()
            .contains("label \"Thanks, you are signed up\""),
        "{head}"
    );
}

#[test]
fn batch_clicks_coordinates_and_refs_bound_to_frames() {
    let mut d = Fixture::with_form(true);
    let mut session = Session::default();
    let mut args = frame_args(&mut session, &mut d, QEMU);
    args["steps"] = json!(coordinate_steps());
    args["steps"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"wait","until_text":"thanks"}));
    args["observe"] = json!({"return":"delta"});
    d.calls.clear();
    let (content, head) = batch(&mut session, &mut d, args);
    assert_eq!(head["ok"], true, "{head}");
    assert!(d.submitted && d.form.as_ref().unwrap()[4] == "Analytical Engines");
    assert_eq!(head["steps"][0], "click @960,316");
    assert_eq!(head["steps"][1], "text 12 chars");
    assert!(head["steps"][11]
        .as_str()
        .unwrap()
        .starts_with("wait found \"Thanks, you are signed up\""));
    // A fast batch reuses the caller's frame: no screenshot between inputs.
    assert_eq!(
        d.calls
            .iter()
            .take(11)
            .filter(|c| *c == "screenshot")
            .count(),
        0,
        "{:?}",
        d.calls
    );
    assert!(content_text(&content).contains("delta: 2 changed regions"));
    // A ref clicks the center of the element as last read.
    let mut d = Fixture::with_form(false);
    let tree = content_text(
        &session
            .call(
                &mut d,
                "vm_accessibility_tree",
                json!({"id":LINUX,"interactive_only":true}),
            )
            .unwrap(),
    );
    let submit = tree
        .lines()
        .find(|l| l.contains("\"Submit\""))
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .to_string();
    let mut args = frame_args(&mut session, &mut d, LINUX);
    args["steps"] = json!([{"type":"click","ref":submit}]);
    let (_, head) = batch(&mut session, &mut d, args);
    assert_eq!(head["steps"][0], "click @960,640");
    assert!(d.submitted);
}

#[test]
fn batch_renews_old_frames_and_fails_when_the_screen_changes_size() {
    let mut d = Fixture::with_form(false);
    let mut session = Session::default();
    let mut args = frame_args(&mut session, &mut d, LINUX);
    // Every step now looks 5 s old: the caller's frame is re-anchored (unchanged
    // screen) and later steps take fresh frames.
    session.skew = Duration::from_secs(5);
    args["steps"] = json!(coordinate_steps()[..4]);
    d.calls.clear();
    let (_, head) = batch(&mut session, &mut d, args.clone());
    assert_eq!(head["ok"], true, "{head}");
    assert_eq!(
        d.calls,
        [
            "screenshot",
            "input_click",
            "screenshot",
            "input_text",
            "screenshot",
            "input_click",
            "screenshot",
            "input_text"
        ]
    );
    // The display resizes after the first click: the next step is refused, nothing more is sent.
    let mut d = Fixture::with_form(false);
    d.resize_after = Some(1);
    session.skew = Duration::ZERO;
    let mut args = frame_args(&mut session, &mut d, LINUX);
    session.skew = Duration::from_secs(5);
    args["steps"] = json!(coordinate_steps());
    args["observe"] = json!({"return":"none","timeout_ms":0});
    let (content, head) = batch(&mut session, &mut d, args);
    assert_eq!(content["isError"], true);
    assert_eq!(
        (head["ok"].as_bool(), head["completed"].as_u64()),
        (Some(false), Some(1))
    );
    assert_eq!(head["failed"]["index"], 1);
    assert!(
        head["failed"]["error"]
            .as_str()
            .unwrap()
            .contains("width changed during the batch (1920 -> 1280)"),
        "{head}"
    );
    assert_eq!(d.inputs, 1);
    // Observed at the failure, with the fresh frame.
    assert_eq!(head["frame"]["width"], 1280);
}

#[test]
fn batch_is_checked_before_anything_is_sent() {
    let mut d = Fixture::with_form(false);
    let mut session = Session::default();
    let frame = frame_args(&mut session, &mut d, LINUX);
    d.calls.clear();
    let many: Vec<Value> = (0..51)
        .map(|_| json!({"type":"wait","settle_ms":0}))
        .collect();
    for (steps, with_frame, error) in [
        (json!([]), true, "1..50"),
        (json!(many), true, "1..50"),
        (json!([{"type":"tap","x":1,"y":1}]), true, "step type"),
        (
            json!([{"type":"click","x":1,"y":1,"ref":"e1"}]),
            true,
            "either ref or x and y",
        ),
        (
            json!([{"type":"click","x":1,"y":1,"text":"no"}]),
            true,
            "not text",
        ),
        (
            json!([{"type":"a11y","ref":"e1"}]),
            true,
            "needs ref and action",
        ),
        (
            json!([{"type":"wait","until_text":"a","until_name":"b"}]),
            true,
            "not both",
        ),
        (
            json!([{"type":"wait","timeout_ms":30000}]),
            true,
            "0..25000",
        ),
        (json!([{"type":"click","x":1,"y":1}]), false, "frame_id"),
    ] {
        let mut args = if with_frame {
            frame.clone()
        } else {
            json!({"id":LINUX})
        };
        args["steps"] = steps;
        let message = session
            .call(&mut d, "vm_input_batch", args)
            .unwrap_err()
            .to_string();
        assert!(message.contains(error), "{message}");
    }
    let mut args = frame.clone();
    args["steps"] = json!([{"type":"key","keys":["tab"]}]);
    args["timeout_ms"] = json!(120_001);
    assert!(session.call(&mut d, "vm_input_batch", args).is_err());
    assert!(d.calls.is_empty(), "{:?}", d.calls);
    // A wait that never sees its text fails the step within its own timeout.
    args = frame;
    args["steps"] = json!([{"type":"wait","until_text":"never","timeout_ms":400}]);
    let (_, head) = batch(&mut session, &mut d, args);
    assert!(head["failed"]["error"]
        .as_str()
        .unwrap()
        .contains("did not appear in time"));
    // Only with input permission.
    let tools = crate::tools_with_input(false, false, false, false).to_string();
    assert!(!tools.contains("vm_input_batch"));
    assert!(crate::tools_with_input(false, false, false, true)
        .to_string()
        .contains("vm_input_batch"));
}

/// Five fields and Submit, then a check that it went through: the earlier MCP
/// surface against one batch.
#[test]
fn measure_form_fill_before_and_after() {
    let empty = || Cost {
        calls: 0,
        text: 0,
        image: 0,
        bytes: 0,
    };
    let mut rows = Vec::new();
    // Before, a11y: JSON tree, five set_text and a press by ref, JSON tree to verify.
    let mut d = Fixture::with_form(false);
    let mut before = empty();
    let tree = d.call("accessibility_tree", json!({"id":LINUX})).unwrap();
    before.add(&text_content(tree).unwrap());
    for i in 0..6 {
        let mut action =
            json!({"id":LINUX,"ref":form_ref(i),"action":if i < 5 {"set_text"} else {"do"}});
        if let Some(value) = VALUES.get(i) {
            action["text"] = json!(value);
        }
        before.add(&text_content(d.call("accessibility_action", action).unwrap()).unwrap());
    }
    before.add(&text_content(d.call("accessibility_tree", json!({"id":LINUX})).unwrap()).unwrap());
    // After, a11y: compact interactive tree, one batch with the delta.
    let mut d = Fixture::with_form(false);
    let mut session = Session::default();
    let mut after = empty();
    let tree = session
        .call(
            &mut d,
            "vm_accessibility_tree",
            json!({"id":LINUX,"interactive_only":true}),
        )
        .unwrap();
    after.add(&tree);
    let tree = content_text(&tree);
    let short = |name: &str, role: &str| {
        tree.lines()
            .find(|l| l.contains(&format!(" {role} \"{name}\"")))
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_string()
    };
    let mut steps: Vec<Value> = FIELDS
        .iter()
        .zip(VALUES)
        .map(|(f, v)| json!({"type":"a11y","ref":short(f, "text"),"action":"set_text","text":v}))
        .collect();
    steps.push(json!({"type":"a11y","ref":short("Submit", "push button"),"action":"do"}));
    after.add(
        &session
            .call(
                &mut d,
                "vm_input_batch",
                json!({"id":LINUX,"steps":steps,"observe":{}}),
            )
            .unwrap(),
    );
    rows.push(("Linux desktop form, a11y refs", before, after));
    // Before, QEMU: each model turn outlasts the 5 s frame, so every field is
    // screenshot, click, text; then screenshot, click Submit, wait, screenshot (19 calls).
    let mut d = Fixture::with_form(true);
    let mut before = empty();
    let shot = |d: &mut Fixture, cost: &mut Cost| {
        let shot = d.call("screenshot", json!({"id":QEMU})).unwrap();
        let frame = json!({"id":QEMU,"frame_id":shot["frame_id"],"width":1920,"height":1080,"source_generation":1});
        cost.add(&crate::screenshot_content(shot).unwrap());
        frame
    };
    let mut frame = Value::Null;
    for step in coordinate_steps().chunks(2) {
        frame = shot(&mut d, &mut before);
        for s in step {
            let mut args = frame.clone();
            for (k, v) in s.as_object().unwrap() {
                if k != "type" {
                    args[k] = v.clone();
                }
            }
            let method = format!("input_{}", s["type"].as_str().unwrap());
            before.add(&text_content(d.call(&method, args).unwrap()).unwrap());
        }
    }
    before.add(
        &text_content(
            d.call(
                "screen_wait",
                json!({"id":QEMU,"frame_id":frame["frame_id"]}),
            )
            .unwrap(),
        )
        .unwrap(),
    );
    shot(&mut d, &mut before);
    // After, QEMU: a screenshot at 1568 px, one batch with the pixel delta.
    let mut d = Fixture::with_form(true);
    let mut session = Session::default();
    let mut after = empty();
    let first = session
        .call(&mut d, "vm_screenshot", json!({"id":QEMU,"max_width":1568}))
        .unwrap();
    after.add(&first);
    let meta: Value = serde_json::from_str(first["content"][1]["text"].as_str().unwrap()).unwrap();
    let args = json!({"id":QEMU,"frame_id":meta["frame_id"],"width":1920,"height":1080,"source_generation":1,"steps":coordinate_steps(),"observe":{}});
    after.add(&session.call(&mut d, "vm_input_batch", args).unwrap());
    assert!(d.submitted);
    rows.push(("QEMU 1920x1080 form, coordinates", before, after));
    println!("| form fill (5 fields + submit) | before calls | before text tok | before image tok | before bytes | after calls | after text tok | after image tok | after bytes |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for (label, before, after) in rows {
        println!(
            "| {label} | {} | {} | {} | {} | {} | {} | {} | {} |",
            before.calls,
            before.text,
            before.image,
            before.bytes,
            after.calls,
            after.text,
            after.image,
            after.bytes
        );
        assert!(
            after.calls < before.calls && after.text + after.image < before.text + before.image,
            "{label}"
        );
    }
}

#[test]
fn host_input_reports_an_action_result_from_what_it_saw() {
    let mut d = Fixture::new(true);
    let mut session = Session::default();
    let shot = session
        .call(&mut d, "vm_screenshot", json!({"id":QEMU}))
        .unwrap();
    let shot: Value = serde_json::from_str(content_text(&shot).as_str()).unwrap();
    let frame = json!({"id":QEMU,"frame_id":shot["frame_id"],"width":1920,"height":1080,"source_generation":1});
    let mut click = frame.clone();
    click["x"] = json!(10);
    click["y"] = json!(10);
    click["observe"] = json!({"return":"none"});
    let reply = session.call(&mut d, "vm_input_click", click).unwrap();
    let head: Value = serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
    crate::action_result::check(&head["action"]).unwrap();
    assert_eq!(head["action"]["route"], "virtual_hid");
    // QEMU's wait cannot place the change, so a click is not confirmed.
    assert_eq!(head["action"]["effect"], "unverifiable");
    assert_eq!(head["action"]["evidence"][0]["kind"], "window_change");
    // Without observe nothing was checked.
    let mut key = head["frame"].clone();
    key["id"] = json!(QEMU);
    key["keys"] = json!(["Return"]);
    let reply = session.call(&mut d, "vm_input_key", key).unwrap();
    let head: Value = serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
    crate::action_result::check(&head["action"]).unwrap();
    assert_eq!(head["action"]["effect"], "unverifiable");
    assert!(head["action"].get("evidence").is_none());
}

#[test]
fn a_guest_without_its_driver_gets_untrusted_ocr_rows() {
    let mut d = Fixture::new(true);
    let mut session = Session::default();
    let state = session.ocr_window_state(&mut d, QEMU).unwrap();
    let structured = &state["structuredContent"];
    assert_eq!(structured["trusted"], false);
    assert_eq!(structured["source"], "host_ocr");
    let save = structured["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["text"] == "Saved")
        .unwrap();
    let token = save["element_token"].as_str().unwrap();
    assert!(structured["tree_markdown"]
        .as_str()
        .unwrap()
        .contains("\"Saved\" @930,510"));
    let verify = session
        .ocr_token_call(
            QEMU,
            "verify_state",
            &json!({"expect":[{"element_token":token,"exists":true}]}),
        )
        .unwrap();
    assert_eq!(verify["structuredContent"]["status"], "unknown");
    assert_eq!(verify["structuredContent"]["reason"], "untrusted_source");
    let click = session
        .ocr_token_call(QEMU, "click", &json!({"element_token":token}))
        .unwrap();
    let refusal = &click["structuredContent"];
    crate::action_result::check(refusal).unwrap();
    assert_eq!(refusal["next_call"]["tool"], "vm_input_click");
    assert_eq!(refusal["next_call"]["arguments"]["x"], 930);
    assert_eq!(
        refusal["next_call"]["arguments"]["frame_id"],
        structured["frame"]["frame_id"]
    );
    // Driver tokens are left to the driver.
    assert!(session
        .ocr_token_call(QEMU, "click", &json!({"element_token":"s3:12"}))
        .is_none());
}
