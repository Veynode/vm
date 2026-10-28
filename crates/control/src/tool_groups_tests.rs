//! The tool groups against the tools this daemon defines.
use crate::tool_groups::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;

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
