//! MCP tool groups: which of its permitted tools a session lists.
//!
//! Permissions (`--allow-*`) decide what a session may call at all; groups only
//! narrow what it sees, so an agent's context carries the tools its task needs.
//! A group never adds a tool its permissions withhold. The mapping is by tool
//! name and kept apart from the tool definitions: a new tool needs one line
//! here (or a matching prefix), and `every_tool_has_a_group` fails until it has.
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// Group names in listing order, with what `vm_tools_enable` says about each.
pub const GROUPS: &[(&str, &str)] = &[
    ("core", "VM and container status, lifecycle and images"),
    ("exec", "Guest commands (execute permission)"),
    ("files", "Guest file transfer (files permission)"),
    ("screen", "screenshot, wait for change, read text"),
    ("input", "pointer, key and text input (input permission)"),
    ("desktop", "windows, applications and accessibility tree"),
    (
        "snapshots",
        "full-state snapshots, container checkpoints, rollback and forks",
    ),
    (
        "cua",
        "cua-driver in Linux desktops, Windows machines and macOS containers: accessibility trees, element actions, browser pages and clipboard",
    ),
];

/// Groups a session lists only when asked (`--tools cua`, `--tools all` or `vm_tools_enable`).
const HIDDEN_BY_DEFAULT: [&str; 1] = ["cua"];

/// Turns on hidden groups mid-session; listed only while some are hidden.
pub const ENABLE_TOOL: &str = "vm_tools_enable";

/// The group of a tool, or None for a tool this mapping does not know yet.
pub fn group(tool: &str) -> Option<&'static str> {
    let prefixed = [
        ("vm_exec", "exec"),
        ("vm_file_", "files"),
        ("vm_screen", "screen"),
        ("vm_input_", "input"),
        ("vm_desktop_", "desktop"),
        ("vm_accessibility_", "desktop"),
        ("vm_window", "desktop"),
        ("vm_app_", "desktop"),
        ("vm_snapshot_", "snapshots"),
        ("container_", "core"),
        ("image_", "core"),
        (crate::cua_bridge::PREFIX, "cua"),
    ];
    let named = match tool {
        "container_exec" => Some("exec"),
        "vm_control_lease" => Some("input"),
        "container_checkpoint"
        | "container_checkpoint_list"
        | "container_checkpoint_delete"
        | "container_rollback"
        | "container_fork"
        | "container_fork_rm" => Some("snapshots"),
        "vm_list" | "vm_status" | "vm_capabilities" | "vm_guest_info" | "vm_create"
        | "vm_start" | "vm_shutdown" | "vm_force_stop" | "vm_show_window" | "vm_close_window" => {
            Some("core")
        }
        _ => None,
    };
    named.or_else(|| {
        prefixed
            .iter()
            .find(|(prefix, _)| tool.starts_with(prefix))
            .map(|(_, group)| *group)
    })
}

/// All groups, as `--tools all` asks.
pub fn all() -> BTreeSet<&'static str> {
    GROUPS.iter().map(|(name, _)| *name).collect()
}

/// What a session lists without `--tools`: every group but the opt-in ones.
pub fn default() -> BTreeSet<&'static str> {
    let mut groups = all();
    for hidden in HIDDEN_BY_DEFAULT {
        groups.remove(hidden);
    }
    groups
}

/// Parses `--tools core,screen` (or `all`) into group names.
pub fn parse(list: &str) -> Result<BTreeSet<&'static str>> {
    let mut groups = BTreeSet::new();
    for name in list.split(',').map(str::trim) {
        if name == "all" {
            groups.extend(all());
        } else if let Some((known, _)) = GROUPS.iter().find(|(known, _)| *known == name) {
            groups.insert(*known);
        } else {
            let names: Vec<_> = GROUPS.iter().map(|(name, _)| *name).collect();
            bail!(
                "unknown tool group {name:?}; groups are all, {}",
                names.join(", ")
            )
        }
    }
    if groups.is_empty() {
        bail!("--tools needs at least one group")
    }
    Ok(groups)
}

/// Keeps the tools of the enabled groups. When a permitted group is hidden,
/// adds `vm_tools_enable` offering exactly the hidden groups that hold tools.
/// Unmapped tools stay listed: grouping must never hide what a permission grants
/// only because the mapping lags behind.
pub fn select(mut tools: Vec<Value>, enabled: &BTreeSet<&str>) -> Vec<Value> {
    let hidden = hidden(&tools, enabled);
    tools.retain(|tool| {
        group(tool["name"].as_str().unwrap_or("")).is_none_or(|g| enabled.contains(g))
    });
    if !hidden.is_empty() {
        let offered: Vec<_> = GROUPS
            .iter()
            .filter(|(name, _)| hidden.contains(name))
            .collect();
        let summary = offered
            .iter()
            .map(|(name, about)| format!("{name} ({about})"))
            .collect::<Vec<_>>()
            .join("; ");
        tools.push(json!({"name":ENABLE_TOOL,"description":format!("List more of this session's permitted tools: {summary}. Then sends notifications/tools/list_changed; call tools/list again if your client ignores it."),"inputSchema":{"type":"object","properties":{"groups":{"type":"array","items":{"type":"string","enum":offered.iter().map(|(name, _)| *name).collect::<Vec<_>>()},"minItems":1,"uniqueItems":true}},"required":["groups"],"additionalProperties":false}}));
    }
    tools
}

/// Permitted groups with at least one tool that `enabled` hides.
pub fn hidden(tools: &[Value], enabled: &BTreeSet<&str>) -> BTreeSet<&'static str> {
    tools
        .iter()
        .filter_map(|tool| group(tool["name"].as_str()?))
        .filter(|g| !enabled.contains(g))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(tools: &[Value]) -> BTreeSet<String> {
        tools
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn every_tool_has_a_group() {
        let everything = crate::McpOptions {
            lifecycle: true,
            execute: true,
            files: true,
            input: true,
            browser: true,
            clipboard: true,
            vm: None,
        };
        for tool in &crate::tools_for(&everything) {
            let name = tool["name"].as_str().unwrap();
            assert!(
                group(name).is_some(),
                "map {name} to a group in tool_groups.rs"
            );
        }
        assert_eq!(group("vm_screenshot"), Some("screen"));
        assert_eq!(group("vm_windows"), Some("desktop"));
        assert_eq!(group("vm_exec"), Some("exec"));
        assert_eq!(group("container_exec"), Some("exec"));
        assert_eq!(group("container_run"), Some("core"));
        assert_eq!(group("vm_snapshot_restore"), Some("snapshots"));
        assert_eq!(group("cua_get_window_state"), Some("cua"));
    }

    #[test]
    fn cua_is_hidden_by_default_and_offered() {
        let tools = crate::tools_for(&crate::McpOptions::default());
        assert!(tools.iter().any(|t| t["name"] == "cua_get_window_state"));
        let listed = select(tools, &default());
        assert!(!listed
            .iter()
            .any(|t| group(t["name"].as_str().unwrap()) == Some("cua")));
        let enable = listed.iter().find(|t| t["name"] == ENABLE_TOOL).unwrap();
        assert_eq!(
            enable["inputSchema"]["properties"]["groups"]["items"]["enum"],
            json!(["cua"])
        );
        assert!(parse("all").unwrap().contains("cua"));
    }

    #[test]
    fn parse_accepts_known_groups_only() {
        assert_eq!(parse("core, desktop").unwrap(), ["core", "desktop"].into());
        assert_eq!(parse("all").unwrap(), all());
        assert!(parse("core,shell").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn groups_never_widen_permissions() {
        let permitted = crate::tools_with_input(false, false, false, false)["tools"]
            .as_array()
            .unwrap()
            .clone();
        let everything = select(permitted.clone(), &all());
        assert_eq!(names(&everything), names(&permitted));
        // Observe-only: input, exec and files hold no tools, so they are not offered.
        let core = select(permitted.clone(), &parse("core").unwrap());
        let enable = core.iter().find(|t| t["name"] == ENABLE_TOOL).unwrap();
        assert_eq!(
            enable["inputSchema"]["properties"]["groups"]["items"]["enum"],
            json!(["screen", "desktop", "snapshots"])
        );
        assert!(names(&core).iter().all(|n| n == ENABLE_TOOL
            || n.starts_with("container_")
            || n.starts_with("image_")
            || ["vm_list", "vm_status", "vm_capabilities", "vm_guest_info"].contains(&n.as_str())));
        let input = select(permitted, &parse("input").unwrap());
        assert!(!names(&input).iter().any(|n| n.starts_with("vm_input_")));
    }

    #[test]
    fn unmapped_tools_stay_listed() {
        let tools = vec![json!({"name":"vm_future"}), json!({"name":"vm_screenshot"})];
        let listed = select(tools, &parse("core").unwrap());
        assert!(names(&listed).contains("vm_future"));
        assert!(!names(&listed).contains("vm_screenshot"));
    }
}
