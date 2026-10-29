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
pub const HIDDEN_BY_DEFAULT: [&str; 1] = ["cua"];

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
        (crate::cua::PREFIX, "cua"),
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
        | "vm_start" | "vm_shutdown" | "vm_force_stop" | "vm_show_window" | "vm_close_window"
        | "vm_sleep" | "vm_keep_awake" => Some("core"),
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
