//! The aggregate's names when tools collide (module doc of [`super`]).

use std::collections::HashMap;

use rmcp::model::Tool;
use serde_json::json;

use super::super::{AggServerInput, McpToolOverride};
use super::*;

fn tool(name: &str) -> Tool {
    serde_json::from_value(json!({"name": name, "inputSchema": {"type": "object"}})).unwrap()
}

fn tools(names: &[&str]) -> Vec<Tool> {
    names.iter().map(|n| tool(n)).collect()
}

fn none() -> HashMap<String, McpToolOverride> {
    HashMap::new()
}

fn input<'a>(
    id: i64,
    name: &'a str,
    prefix: &'a str,
    tools: &'a [Tool],
    ov: &'a HashMap<String, McpToolOverride>,
) -> AggServerInput<'a> {
    AggServerInput {
        server_id: id,
        server_name: name,
        tool_prefix: prefix,
        tools,
        overrides: ov,
        device: false,
    }
}

fn names(agg: &Aggregate) -> Vec<&str> {
    agg.tools.iter().map(|t| t.name.as_ref()).collect()
}

/// Two bare servers' `read`: both take their server's name as a prefix, the
/// bare name routes nowhere, each routes to its own server's `read` — in
/// whichever order the servers come.
#[test]
fn two_bare_servers_both_take_their_prefix() {
    let (a, b) = (tools(&["read", "only_a"]), tools(&["read"]));
    let ov = none();
    for flip in [false, true] {
        let mut inputs = vec![
            input(1, "alpha", "", &a, &ov),
            input(2, "beta", "", &b, &ov),
        ];
        if flip {
            inputs.reverse();
        }
        let agg = build(&mut inputs, &mut []);
        assert_eq!(names(&agg), ["alpha__read", "beta__read", "only_a"]);
        assert_eq!(agg.reverse["alpha__read"], (1, "read".to_string()));
        assert_eq!(agg.reverse["beta__read"], (2, "read".to_string()));
        assert!(!agg.reverse.contains_key("read"));
        assert!(agg.skipped.is_empty(), "{:?}", agg.skipped);
        assert_eq!(agg.qualified["alpha__read"], "read");
        assert!(
            !agg.qualified.contains_key("only_a"),
            "no collision, no move"
        );
        assert_eq!(agg.moved_from("read"), ["alpha__read", "beta__read"]);
        assert_eq!(agg.moved_on("read", 2), Some("beta__read"));
    }
}

/// A server that is not connected claims what it last listed: the name
/// does not change when it is reaped, and only its colliding tool moves the
/// other's.
#[test]
fn a_server_not_connected_still_claims_its_names() {
    let (a, b) = (tools(&["read"]), tools(&["read", "write"]));
    let ov = none();
    let mut live = vec![input(2, "beta", "", &b, &ov)];
    let mut known = vec![input(1, "alpha", "", &a, &ov)];
    let agg = build(&mut live, &mut known);
    assert_eq!(names(&agg), ["beta__read", "write"]);
    assert!(
        !agg.reverse.contains_key("alpha__read"),
        "a known tool routes nowhere"
    );
}

/// A bare server's literal `gh__search` beside the `search` of the server
/// prefixed `gh`: the prefixed one keeps its name, the bare one moves.
#[test]
fn a_prefix_holds_its_name_over_a_bare_literal() {
    let (bare, gh) = (tools(&["gh__search"]), tools(&["search"]));
    let ov = none();
    let mut inputs = vec![
        input(1, "aaa", "", &bare, &ov),
        input(2, "github", "gh", &gh, &ov),
    ];
    let agg = build(&mut inputs, &mut []);
    assert_eq!(names(&agg), ["aaa__gh__search", "gh__search"]);
    assert_eq!(agg.reverse["gh__search"], (2, "search".to_string()));
    assert_eq!(
        agg.reverse["aaa__gh__search"],
        (1, "gh__search".to_string())
    );
}

/// Two servers that share a prefix: both tools take their server's name.
#[test]
fn twins_of_one_prefix_take_their_names() {
    let (a, b) = (tools(&["search", "a_only"]), tools(&["search"]));
    let ov = none();
    let mut inputs = vec![
        input(1, "gh work", "gh", &a, &ov),
        input(2, "gh-home", "gh", &b, &ov),
    ];
    let agg = build(&mut inputs, &mut []);
    assert_eq!(
        names(&agg),
        ["gh-home__search", "gh__a_only", "gh_work__search"]
    );
}

/// A server's tool in one of lmgw's own namespaces moves out of it under
/// its server's name, a rename into one too; one that cannot (a prefixed
/// server whose prefix runs into `kb__`) is skipped and surfaced.
#[test]
fn a_tool_in_a_reserved_namespace_moves_out_of_it() {
    let t = tools(&["lmgw__settings_set", "safe"]);
    let mut ov = HashMap::new();
    ov.insert(
        "safe".to_string(),
        McpToolOverride {
            hidden: false,
            rename: Some("kb__search".into()),
        },
    );
    let mut inputs = vec![input(5, "impostor", "", &t, &ov)];
    let agg = build(&mut inputs, &mut []);
    assert_eq!(
        names(&agg),
        ["impostor__kb__search", "impostor__lmgw__settings_set"]
    );
    assert!(agg.skipped.is_empty());

    let k = tools(&["x"]);
    let none = none();
    let mut inputs = vec![input(6, "kb", "kb_", &k, &none)];
    let agg = build(&mut inputs, &mut []);
    assert!(agg.tools.is_empty() && agg.reverse.is_empty());
    assert_eq!(agg.skipped.len(), 1);
    assert!(
        agg.skipped[0].reason.contains("reserved"),
        "{:?}",
        agg.skipped
    );
}

/// A name a qualified one runs into moves on too, and nothing else does.
#[test]
fn a_moved_name_that_meets_another_moves_that_one() {
    let (a, b, g) = (
        tools(&["read"]),
        tools(&["read"]),
        tools(&["alpha__read", "keep"]),
    );
    let ov = none();
    let mut inputs = vec![
        input(1, "alpha", "", &a, &ov),
        input(2, "beta", "", &b, &ov),
        input(3, "gamma", "", &g, &ov),
    ];
    let agg = build(&mut inputs, &mut []);
    assert_eq!(
        names(&agg),
        ["alpha__read", "beta__read", "gamma__alpha__read", "keep"]
    );
    assert_eq!(agg.reverse["alpha__read"], (1, "read".to_string()));
}

/// Two servers whose names spell alike still collide under them: the first
/// by server name keeps the name, the other is skipped and surfaced.
#[test]
fn what_still_collides_goes_to_the_first_by_server_name() {
    let (a, b) = (tools(&["read"]), tools(&["read"]));
    let ov = none();
    let mut inputs = vec![input(2, "a_b", "", &b, &ov), input(1, "a b", "", &a, &ov)];
    let agg = build(&mut inputs, &mut []);
    assert_eq!(names(&agg), ["a_b__read"]);
    assert_eq!(agg.reverse["a_b__read"].0, 1, "'a b' sorts first");
    assert_eq!(agg.skipped.len(), 1);
    assert_eq!(agg.skipped[0].server_id, 2);
}

/// A tool's UI resource stays in its server's namespace whatever name the
/// tool took: a bare server's is not namespaced, a prefixed one's is.
#[test]
fn a_moved_tool_keeps_its_resource_namespace() {
    let with_ui = |name: &str| -> Tool {
        serde_json::from_value(json!({"name": name, "inputSchema": {"type": "object"},
            "_meta": {"ui": {"resourceUri": "ui://w/card"}}}))
        .unwrap()
    };
    let (a, b) = (vec![with_ui("show")], vec![with_ui("show")]);
    let (c, d) = (vec![with_ui("show")], vec![with_ui("show")]);
    let ov = none();
    let mut inputs = vec![
        input(1, "alpha", "", &a, &ov),
        input(2, "beta", "", &b, &ov),
        input(3, "one", "wx", &c, &ov),
        input(4, "two", "wx", &d, &ov),
    ];
    let agg = build(&mut inputs, &mut []);
    let uri = |n: &str| {
        let t = agg.tools.iter().find(|t| t.name == n).unwrap();
        crate::mcp::resources::ui_resource(t.meta.as_ref()).map(str::to_string)
    };
    assert_eq!(uri("alpha__show").as_deref(), Some("ui://w/card"));
    assert_eq!(uri("one__show").as_deref(), Some("ui://wx__w/card"));
    assert_eq!(uri("two__show").as_deref(), Some("ui://wx__w/card"));
}

/// The owner's switch set under the name a tool had before a collision
/// holds for it under its prefix.
#[test]
fn a_switch_under_the_old_name_holds() {
    let (a, b) = (tools(&["read"]), tools(&["read"]));
    let ov = none();
    let mut inputs = vec![
        input(1, "alpha", "", &a, &ov),
        input(2, "beta", "", &b, &ov),
    ];
    let agg = build(&mut inputs, &mut []);
    let mut snap = crate::config::Snapshot::default();
    snap.disabled_tools.insert(
        "read".into(),
        crate::config::DisabledTool {
            source: "alpha".into(),
            disabled_at: String::new(),
        },
    );
    assert!(agg.tool_disabled(&snap, "alpha__read"));
    assert!(agg.tool_disabled(&snap, "beta__read"));
}

#[test]
fn a_name_reads_as_a_prefix() {
    assert_eq!(name_qualifier("my server"), "my_server");
    assert_eq!(name_qualifier("device:phone"), "device_phone");
    assert_eq!(name_qualifier("ok-name_1"), "ok-name_1");
}

/// A built-in tool's switch does not reach a server's tool that moved out
/// of the built-in's namespace.
#[test]
fn a_built_in_s_switch_stays_the_built_in_s() {
    let t = tools(&["kb__search"]);
    let ov = none();
    let mut inputs = vec![input(1, "imp", "", &t, &ov)];
    let agg = build(&mut inputs, &mut []);
    let mut snap = crate::config::Snapshot::default();
    snap.disabled_tools.insert(
        "kb__search".into(),
        crate::config::DisabledTool {
            source: "kb".into(),
            disabled_at: String::new(),
        },
    );
    assert!(!agg.tool_disabled(&snap, "imp__kb__search"));
}

fn switched(names: &[&str]) -> crate::config::Snapshot {
    let mut snap = crate::config::Snapshot::default();
    for n in names {
        snap.disabled_tools.insert(
            n.to_string(),
            crate::config::DisabledTool {
                source: "alpha".into(),
                disabled_at: String::new(),
            },
        );
    }
    snap
}

/// The owner's switch set under the name a collision gave a tool holds
/// once the collision ends, and the one set under its own name holds while
/// it is moved: a switch under any of its names holds it off, and says
/// which.
#[test]
fn a_switch_under_any_name_of_the_tool_holds() {
    let (a, b) = (tools(&["read", "other"]), tools(&["read"]));
    let ov = none();
    let both = || {
        build(
            &mut [
                input(1, "alpha", "", &a, &ov),
                input(2, "beta", "", &b, &ov),
            ],
            &mut [],
        )
    };
    let alone = || build(&mut [input(1, "alpha", "", &a, &ov)], &mut []);

    // Off as `alpha__read` while both offer `read`; beta's stays on.
    let snap = switched(&["alpha__read"]);
    let agg = both();
    assert!(agg.tool_disabled(&snap, "alpha__read"));
    assert!(!agg.tool_disabled(&snap, "beta__read"));
    // The collision ends: alpha's `read` stays off, saying which switch.
    let agg = alone();
    assert_eq!(names(&agg), ["other", "read"]);
    assert!(agg.tool_disabled(&snap, "read"));
    assert_eq!(agg.disabled_by(&snap, "read"), Some("alpha__read"));
    assert!(!agg.tool_disabled(&snap, "other"));

    // Off as `read` before the collision: both moved tools stay off.
    let snap = switched(&["read"]);
    let agg = both();
    assert_eq!(agg.disabled_by(&snap, "alpha__read"), Some("read"));
    assert_eq!(agg.disabled_by(&snap, "beta__read"), Some("read"));
    assert!(alone().tool_disabled(&snap, "read"));
}

/// A server that lists one name twice offers the first; the other is
/// skipped and surfaced, and neither moves for the other.
#[test]
fn a_name_a_server_lists_twice_is_offered_once() {
    let mut twice = tools(&["read", "read"]);
    twice[1].description = Some("second".into());
    let ov = none();
    let agg = build(&mut [input(1, "alpha", "", &twice, &ov)], &mut []);
    assert_eq!(names(&agg), ["read"]);
    assert!(agg.tools[0].description.is_none(), "the first is kept");
    assert!(agg.qualified.is_empty());
    assert_eq!(agg.skipped.len(), 1);
    assert_eq!(agg.skipped[0].exposed_name, "read");
    assert!(
        agg.skipped[0]
            .reason
            .contains("lists 'read' more than once"),
        "{:?}",
        agg.skipped
    );
}

/// A device row's tools never leave its label's namespace: two of its own
/// tools of one name (a duplicate listing, a rename onto another) keep the
/// first under `<label>__`, never a `device_<key>__…` name; a bare
/// server's literal in the label's namespace moves out of it instead.
#[test]
fn a_device_row_s_tools_stay_in_its_label() {
    let dev = tools(&["see", "see", "shot"]);
    let mut ov = HashMap::new();
    ov.insert(
        "shot".to_string(),
        McpToolOverride {
            hidden: false,
            rename: Some("see".into()),
        },
    );
    let bare = tools(&["phone__see"]);
    let none = none();
    let mut device = input(9, "device:phone", "phone", &dev, &ov);
    device.device = true;
    let agg = build(&mut [device, input(2, "zeta", "", &bare, &none)], &mut []);
    assert_eq!(names(&agg), ["phone__see", "zeta__phone__see"]);
    assert_eq!(agg.reverse["phone__see"], (9, "see".to_string()));
    assert!(
        agg.tools.iter().all(|t| !t.name.starts_with("device")),
        "{:?}",
        names(&agg)
    );
    assert_eq!(agg.skipped.len(), 2, "{:?}", agg.skipped);
    assert!(agg
        .skipped
        .iter()
        .all(|s| s.server_id == 9 && s.exposed_name == "phone__see"));
    assert!(!agg.spellings.contains_key("phone__see"));
}

/// Who a moved tool's name is claimed by: the other server, said to be a
/// last listing when it is not connected; lmgw's own namespace.
#[test]
fn a_moved_tool_names_who_claims_its_name() {
    let (a, b, c) = (tools(&["read"]), tools(&["read"]), tools(&["kb__x"]));
    let ov = none();
    let mut live = vec![input(1, "alpha", "", &a, &ov), input(3, "imp", "", &c, &ov)];
    let mut known = vec![input(2, "beta", "", &b, &ov)];
    let agg = build(&mut live, &mut known);
    assert_eq!(
        agg.moved_by["alpha__read"],
        [Claimant::Server {
            id: 2,
            name: "beta".into(),
            connected: false
        }]
    );
    assert_eq!(agg.moved_by["imp__kb__x"], [Claimant::Builtin("kb__")]);
    assert_eq!(
        agg.spellings["alpha__read"],
        ["read".to_string(), "alpha__read".to_string()]
    );
}
