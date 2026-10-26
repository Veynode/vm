//! Window, application and environment tools for Apple Virtualization Linux
//! desktop containers and macOS containers. On Linux the guest agent answers
//! them as an X11 client of the container's display (EWMH), so nothing is
//! installed in the image; macOS containers run the `vm-macos-desktop` helper
//! over SSH instead (`container_tools::desktop`), with the same arguments.
//! `desktop_state` and `desktop_apps` observe; `window_action` and
//! `app_launch` act and need the input permission.
//! The accessibility tools read the desktop's AT-SPI tree over its D-Bus
//! accessibility bus: `accessibility_tree` and `accessibility_find` (the same
//! guest method with a name or role filter) observe; `accessibility_action`
//! acts like the window tools.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub const METHODS: [&str; 7] = [
    "desktop_state",
    "desktop_apps",
    "window_action",
    "app_launch",
    "accessibility_tree",
    "accessibility_find",
    "accessibility_action",
];
pub const ACCESSIBILITY_ACTIONS: [&str; 4] = ["do", "focus", "set_text", "set_value"];
const MAX_TEXT: usize = 16 * 1024;

/// The guest method that answers a tool method.
pub fn guest_method(method: &str) -> &str {
    if method == "accessibility_find" {
        "accessibility_tree"
    } else {
        method
    }
}
pub const WINDOW_ACTIONS: [&str; 8] = [
    "activate",
    "close",
    "minimize",
    "maximize",
    "unmaximize",
    "fullscreen",
    "unfullscreen",
    "move_resize",
];

/// Whether a method acts on the desktop (input permission).
pub fn acts(method: &str) -> bool {
    matches!(
        method,
        "window_action" | "app_launch" | "accessibility_action"
    )
}

/// Validated arguments, in the form the guest agent takes.
pub fn arguments(method: &str, args: &Value) -> Result<Value> {
    let allowed: &[&str] = match method {
        "window_action" => &["window", "action", "x", "y", "width", "height"],
        "app_launch" => &["app"],
        "accessibility_tree" => &["app", "max_depth", "max_nodes", "include_hidden"],
        "accessibility_find" => &["name", "role", "app", "max_nodes", "include_hidden"],
        "accessibility_action" => &["ref", "action", "action_name", "text", "value"],
        _ => &[],
    };
    let extra = args.as_object().is_none_or(|a| {
        a.keys()
            .any(|k| k != "id" && !allowed.contains(&k.as_str()))
    });
    if extra {
        bail!(
            "{method} accepts only id{}",
            allowed.iter().map(|k| format!(", {k}")).collect::<String>()
        )
    }
    let mut out = json!({});
    match method {
        "window_action" => {
            out["window"] = json!(args["window"]
                .as_u64()
                .filter(|w| (1..=u32::MAX as u64).contains(w))
                .context("window must be a window id from vm_desktop_state")?);
            let action = args["action"]
                .as_str()
                .filter(|a| WINDOW_ACTIONS.contains(a))
                .context("unknown window action")?;
            out["action"] = json!(action);
            let mut geometry = false;
            for key in ["x", "y", "width", "height"] {
                if let Some(value) = args.get(key) {
                    let value = value
                        .as_i64()
                        .filter(|v| (-16384..=16384).contains(v))
                        .with_context(|| format!("{key} must be an integer within 16384"))?;
                    if key.len() > 1 && value < 1 {
                        bail!("{key} must be positive")
                    }
                    out[key] = json!(value);
                    geometry = true;
                }
            }
            if geometry != (action == "move_resize") {
                bail!("x, y, width and height apply to move_resize, which needs at least one")
            }
        }
        "app_launch" => {
            let app = args["app"]
                .as_str()
                .filter(|a| {
                    (1..=200).contains(&a.len())
                        && a.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
                        && a.as_bytes()[0].is_ascii_alphanumeric()
                })
                .context("app must be an application id from vm_desktop_apps")?;
            out["app"] = json!(app);
        }
        "accessibility_tree" | "accessibility_find" => {
            for key in ["app", "name", "role"] {
                if let Some(value) = args.get(key) {
                    let text = value
                        .as_str()
                        .filter(|t| !t.is_empty() && t.chars().count() <= 200)
                        .with_context(|| format!("{key} must be 1..200 characters"))?;
                    out[key] = json!(text);
                }
            }
            if method == "accessibility_find"
                && out.get("name").is_none()
                && out.get("role").is_none()
            {
                bail!("accessibility_find needs name or role")
            }
            for (key, limit) in [("max_depth", 40), ("max_nodes", 2000)] {
                if let Some(value) = args.get(key) {
                    out[key] = json!(value
                        .as_u64()
                        .filter(|v| (1..=limit).contains(v))
                        .with_context(|| format!("{key} must be 1..{limit}"))?);
                }
            }
            if let Some(value) = args.get("include_hidden") {
                out["include_hidden"] = json!(value
                    .as_bool()
                    .context("include_hidden must be a boolean")?);
            }
        }
        "accessibility_action" => {
            out["ref"] = json!(args["ref"]
                .as_str()
                .filter(|r| (3..=1300).contains(&r.len()) && r.contains('|'))
                .context("ref must be an element ref from vm_accessibility_tree")?);
            let action = args["action"]
                .as_str()
                .filter(|a| ACCESSIBILITY_ACTIONS.contains(a))
                .context("action must be do, focus, set_text or set_value")?;
            out["action"] = json!(action);
            if let Some(name) = args.get("action_name") {
                out["action_name"] = json!(name
                    .as_str()
                    .filter(|n| !n.is_empty() && n.len() <= 100 && action == "do")
                    .context("action_name names one of the element's actions, for do")?);
            }
            match (action, args.get("text"), args.get("value")) {
                ("set_text", Some(text), None) => {
                    out["text"] = json!(text
                        .as_str()
                        .filter(|t| t.len() <= MAX_TEXT)
                        .context("text must be a string of at most 16 KiB")?)
                }
                ("set_value", None, Some(value)) => {
                    out["value"] = json!(value
                        .as_f64()
                        .filter(|v| v.is_finite())
                        .context("value must be a number")?)
                }
                ("do" | "focus", None, None) => {}
                _ => bail!(
                    "set_text takes text and set_value takes value; other actions take neither"
                ),
            }
        }
        _ => {}
    }
    Ok(out)
}

pub fn tools(input: bool) -> Vec<Value> {
    let id = json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false});
    let filters = json!({"app":{"type":"string","description":"application name substring or process id"},"max_nodes":{"type":"integer","minimum":1,"maximum":2000,"default":400},"include_hidden":{"type":"boolean","default":false}});
    let mut filters = filters;
    filters["format"] = json!({"type":"string","enum":["compact","json"],"default":"compact","description":"compact: one line per element, `ref role \"name\" =\"text\" value=V @center_x,center_y [states] {actions}` (actions only when there are several; windows add WxH; +N unexpanded children; find adds `in \"context\"`), indented by depth in the tree, with short refs (e12) that the accessibility action (input permission) accepts for this MCP session. json: the Guest's elements as they are, with full refs and boxes."});
    filters["interactive_only"] = json!({"type":"boolean","default":false,"description":"Only elements that can be acted on (actions, editable or focused, controls by role) plus applications and windows; reads deeper so max_nodes counts what is shown."});
    let mut tree = filters.clone();
    tree["id"] = json!({"type":"string"});
    tree["max_depth"] = json!({"type":"integer","minimum":1,"maximum":40,"default":25});
    let mut find = filters;
    find["id"] = json!({"type":"string"});
    find["name"] = json!({"type":"string","description":"case-insensitive substring of the element's name or text"});
    find["role"] = json!({"type":"string","description":"exact role name as the tree reports it. Linux (AT-SPI): push button, text, menu item, check box, page tab, label. macOS (role or subrole, AX names in words): button, text field, text area, static text, menu item, menu bar item, check box, pop up button, close button"});
    let mut tools = vec![
        json!({"name":"vm_accessibility_tree","description":"Read the accessibility tree of a Linux desktop container (AT-SPI) or macOS container (Accessibility API) instead of a screenshot: applications, windows and controls in tree order with depth, role, name, text, value, states, actions and a box with center in screenshot pixels. Each element has a ref for the accessibility action (input permission). Showing elements only, unless include_hidden. Linux: GTK apps appear when at-spi2 runs; reading turns accessibility on for Qt and Chromium apps started afterwards. macOS: every regular app, frontmost first with its menu bar (open a menu to see its items); adds subrole and identifier; app matches name, pid or bundle id.","inputSchema":{"type":"object","properties":tree,"required":["id"],"additionalProperties":false}}),
        json!({"name":"vm_accessibility_find","description":"Search a Linux desktop or macOS container's accessibility tree by name (substring of name or text) and/or exact role; returns matches only, each with context (application, window, named ancestors), box with center in screenshot pixels and ref.","inputSchema":{"type":"object","properties":find,"required":["id"],"additionalProperties":false}}),
        json!({"name":"vm_desktop_state","description":"Read the desktop without a screenshot (Apple Virtualization Linux desktop and macOS containers): screen size, cursor, keyboard layout, focused application and windows top-most first with id, title, application, process, type, states and geometry in screenshot pixels. Linux adds workspaces, window manager, class and frames; macOS adds the work area (minus menu bar and Dock), scale and bundle ids. Window ids are for the window actions (input permission).","inputSchema":id}),
        json!({"name":"vm_desktop_apps","description":"List installed applications of a Linux desktop container (visible .desktop entries: id, name, generic name, comment, categories, terminal flag, command) or macOS container (bundles in /Applications and /System/Applications: bundle id, name, path, category, running). Ids are for launching (input permission).","inputSchema":id}),
    ];
    if input {
        tools.push(json!({"name":"vm_window_action","description":"Act on a window of a Linux desktop container (window manager) or macOS container (Accessibility): activate (raise, focus; macOS also unminimizes), close (as the close button), minimize, maximize (macOS: fill the work area), unmaximize (Linux only), fullscreen, unfullscreen, or move_resize with x, y (Linux: client area origin; macOS: window origin) and/or width, height in screenshot pixels. Returns the window shortly after (null once closed); the window manager or app may adjust or refuse.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"window":{"type":"integer","minimum":1},"action":{"type":"string","enum":WINDOW_ACTIONS},"x":{"type":"integer"},"y":{"type":"integer"},"width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1}},"required":["id","window","action"],"additionalProperties":false}}));
        tools.push(json!({"name":"vm_accessibility_action","description":"Act on one element of a Linux desktop or macOS container's accessibility tree by its ref from vm_accessibility_tree or vm_accessibility_find, without coordinates: do (default action such as click, press or toggle, or action_name), focus, set_text (replace an editable field's text) or set_value (sliders, spin buttons). Returns whether the app accepted it and the element 100 ms later (null once gone) as a compact line. Short refs (e12) from this session's compact trees and full Guest refs both work. macOS refs survive elements shifting; the reply carries the current ref. observe returns what changed (by default the tree delta) and a fresh frame.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"ref":{"type":"string"},"action":{"type":"string","enum":ACCESSIBILITY_ACTIONS},"action_name":{"type":"string"},"text":{"type":"string"},"value":{"type":"number"},"observe":crate::agent_view::observe_schema()},"required":["id","ref","action"],"additionalProperties":false}}));
        tools.push(json!({"name":"vm_app_launch","description":"Start an installed application of a Linux desktop or macOS container by its vm_desktop_apps id (macOS: bundle id) as the desktop user with its environment (Linux terminal apps open in the installed terminal). Waits up to 5 s for its new window and activates it so keys reach it; returns the pid and that window (null if none yet).","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"app":{"type":"string","minLength":1,"maxLength":200}},"required":["id","app"],"additionalProperties":false}}));
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arguments_are_bounded() {
        let ok = arguments(
            "window_action",
            &json!({"id":"vm","window":4194307,"action":"move_resize","x":-5,"width":800}),
        )
        .unwrap();
        assert_eq!(
            ok,
            json!({"window":4194307,"action":"move_resize","x":-5,"width":800})
        );
        assert_eq!(
            arguments(
                "app_launch",
                &json!({"id":"vm","app":"org.gnome.Calculator"})
            )
            .unwrap(),
            json!({"app":"org.gnome.Calculator"})
        );
        assert_eq!(
            arguments("desktop_state", &json!({"id":"vm"})).unwrap(),
            json!({})
        );
        assert_eq!(
            arguments(
                "accessibility_find",
                &json!({"id":"vm","name":"OK","role":"push button","max_nodes":50})
            )
            .unwrap(),
            json!({"name":"OK","role":"push button","max_nodes":50})
        );
        assert_eq!(guest_method("accessibility_find"), "accessibility_tree");
        // macOS refs: pid, child path, AX role and key.
        assert_eq!(
            arguments(
                "accessibility_action",
                &json!({"id":"vm","ref":"983|0.0.0.5|AXButton|Seven","action":"do"})
            )
            .unwrap(),
            json!({"ref":"983|0.0.0.5|AXButton|Seven","action":"do"})
        );
        assert_eq!(
            arguments(
                "accessibility_action",
                &json!({"id":"vm","ref":":1.5|/a/1","action":"set_text","text":""})
            )
            .unwrap(),
            json!({"ref":":1.5|/a/1","action":"set_text","text":""})
        );
        for (method, bad) in [
            ("accessibility_find", json!({"id":"vm","app":"x"})),
            ("accessibility_tree", json!({"id":"vm","max_nodes":0})),
            ("accessibility_tree", json!({"id":"vm","name":"x"})),
            (
                "accessibility_action",
                json!({"id":"vm","ref":"x","action":"do"}),
            ),
            (
                "accessibility_action",
                json!({"id":"vm","ref":":1.5|/a","action":"set_text"}),
            ),
            (
                "accessibility_action",
                json!({"id":"vm","ref":":1.5|/a","action":"do","value":1}),
            ),
            (
                "accessibility_action",
                json!({"id":"vm","ref":":1.5|/a","action":"focus","action_name":"x"}),
            ),
            (
                "accessibility_action",
                json!({"id":"vm","ref":":1.5|/a","action":"set_text","text":"x".repeat(16 * 1024 + 1)}),
            ),
            ("desktop_state", json!({"id":"vm","x":1})),
            (
                "window_action",
                json!({"id":"vm","window":0,"action":"close"}),
            ),
            (
                "window_action",
                json!({"id":"vm","window":1,"action":"explode"}),
            ),
            (
                "window_action",
                json!({"id":"vm","window":1,"action":"close","x":3}),
            ),
            (
                "window_action",
                json!({"id":"vm","window":1,"action":"move_resize"}),
            ),
            (
                "window_action",
                json!({"id":"vm","window":1,"action":"move_resize","width":0}),
            ),
            (
                "window_action",
                json!({"id":"vm","window":1,"action":"move_resize","x":20000}),
            ),
            ("app_launch", json!({"id":"vm","app":"../etc/passwd"})),
            ("app_launch", json!({"id":"vm","app":"-x"})),
            ("app_launch", json!({"id":"vm"})),
        ] {
            assert!(arguments(method, &bad).is_err(), "{method} {bad}");
        }
    }
}
