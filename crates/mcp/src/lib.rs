//! VeynodeVM's MCP adapters and what they share with the daemon's MCP server.
//!
//! The MCP server itself is the core's (`vm-control mcp`, or a session in the
//! daemon through `mcp.serve`, docs/api.md). `vm-mcp-http` serves it over
//! HTTPS by running one stdio session per client; this library holds the
//! protocol tables both sides read: the MCP version, response bounds, tool
//! groups, the cua-driver tool scopes and resource URIs. It never links
//! `vm-control`.
use serde_json::Value;
use veynodevm_api::{MAX_LARGE_LINE, MAX_LINE};

pub mod cua;
pub mod resource;
pub mod tool_groups;

/// The MCP protocol version served.
pub const VERSION: &str = "2025-06-18";

/// The largest answer an MCP request may get: a screenshot-sized one for
/// tools and resources that may carry a frame, else one ordinary line.
pub fn response_limit(request: &Value) -> usize {
    // An observing input call or a subscribed screen resource may carry a screenshot too.
    let name = request["params"]["name"].as_str().unwrap_or("");
    let driver = name.starts_with(cua::PREFIX) || cua::scope(name).is_some();
    if request["method"] == "tools/call"
        && (name == "vm_screenshot"
            || driver
            || request["params"]["arguments"]["observe"]["return"] == "screenshot")
        || request["method"] == "resources/read"
            && request["params"]["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("/screen"))
    {
        MAX_LARGE_LINE
    } else {
        MAX_LINE
    }
}
