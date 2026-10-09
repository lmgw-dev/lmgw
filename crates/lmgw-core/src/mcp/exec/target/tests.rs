//! A label's spellings of its tools' names (module doc of [`super`]).

use std::collections::HashMap;

use rmcp::model::Tool;
use serde_json::json;

use super::*;
use crate::config::{McpServer, McpTransport};
use crate::mcp::{build_aggregate, AggServerInput, McpToolOverride};

fn server(id: i64, name: &str) -> McpServer {
    McpServer {
        id,
        name: name.into(),
        enabled: true,
        transport: McpTransport::Http,
        command: None,
        args: vec![],
        env: vec![],
        cwd: None,
        container_image: None,
        extra_run_args: vec![],
        url: None,
        headers: vec![],
        tool_prefix: String::new(),
        timeout_ms: 60_000,
        autostart: false,
        idle_seconds: 0,
        allow_sampling: false,
        sampling_alias: None,
        agent_id: None,
        device_key_id: None,
    }
}

fn tools(names: &[&str]) -> Vec<Tool> {
    names
        .iter()
        .map(|n| {
            serde_json::from_value(json!({"name": n, "inputSchema": {"type": "object"}})).unwrap()
        })
        .collect()
}

/// A bare server `alpha` that lists `read` and a literal `alpha__x`, and,
/// with `beta`, another bare server offering `read`: `alpha`'s `read`
/// moves to `alpha__read`; its literal `alpha__x` is a tool of that name.
#[test]
fn only_a_moved_tool_reads_as_its_own_name_under_the_server_s_name() {
    let mut snap = Snapshot::default();
    snap.mcp_servers.insert(1, server(1, "alpha"));
    snap.mcp_servers.insert(2, server(2, "beta"));
    let (a, b) = (tools(&["read", "alpha__x"]), tools(&["read"]));
    let ov: HashMap<String, McpToolOverride> = HashMap::new();
    let input = |id, name, tools| AggServerInput {
        server_id: id,
        server_name: name,
        tool_prefix: "",
        tools,
        overrides: &ov,
        device: false,
    };
    let agg = build_aggregate(&mut [input(1, "alpha", &a), input(2, "beta", &b)]);
    assert!(agg.qualified.contains_key("alpha__read"));
    assert!(agg.reverse.contains_key("alpha__x"));

    let t = label_target_in(&snap, &agg, "alpha");
    assert_eq!(t.canonical("alpha__read"), "read");
    assert_eq!(t.canonical("alpha__x"), "alpha__x", "a literal name");
    assert_eq!(t.spellings("read"), ["alpha__read", "read"]);
    assert_eq!(t.spellings("alpha__x"), ["alpha__x"]);

    // Without the collision nothing moved: `alpha__read` is no spelling.
    let agg = build_aggregate(&mut [input(1, "alpha", &a)]);
    let t = label_target_in(&snap, &agg, "alpha");
    assert_eq!(t.canonical("alpha__read"), "alpha__read");
    assert_eq!(t.spellings("read"), ["read"]);
}
