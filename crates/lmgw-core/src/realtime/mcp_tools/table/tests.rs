//! The tool table's lookups (realtime-server-tools §1.1–§1.3), on listings
//! made by hand: what a session.update plans, what a response offers and
//! narrows, what an `mcp` tool_choice maps to, and whose a name is.

use serde_json::{json, Value};

use super::super::super::protocol::{Tool, ToolChoice};
use super::*;
use crate::mcp::exec::{LabelTool, LabelTools};

fn tools(v: Value) -> Vec<Tool> {
    serde_json::from_value(v).unwrap()
}

fn choice(v: Value) -> ToolChoice {
    serde_json::from_value(v).unwrap()
}

fn tool(exposed: &str, wire: &str) -> LabelTool {
    LabelTool {
        def: ToolDef {
            name: exposed.into(),
            description: None,
            parameters: json!({"type": "object"}),
        },
        wire: wire.into(),
    }
}

fn spec(label: &str, allowed: Option<&[&str]>) -> McpToolSpec {
    McpToolSpec {
        server_label: label.into(),
        allowed_tools: allowed.map(|a| a.iter().map(|s| s.to_string()).collect()),
        require_approval: Default::default(),
    }
}

/// The server ids of `listed`'s registered servers.
const HA: i64 = 1;
const BARE: i64 = 2;

/// `docs` (a built-in), `Home Assistant` (a prefixed server addressed by
/// its name: `ha__…`) and `bare` (a server without a prefix), all listed.
fn listed() -> McpTable {
    let mut t = McpTable::default();
    for (label, tools, builtin) in [
        (
            "docs",
            vec![
                tool("docs__resolve", "resolve"),
                tool("docs__query", "query"),
            ],
            true,
        ),
        (
            "Home Assistant",
            vec![
                tool("ha__lights_on", "lights_on"),
                tool("ha__state", "state"),
            ],
            false,
        ),
        ("bare", vec![tool("x__y", "x__y")], false),
    ] {
        let seq = t.start(&spec(label, None));
        let (builtin, server) = match (builtin, label) {
            (true, _) => (tools.iter().map(|t| t.def.name.clone()).collect(), None),
            (false, "Home Assistant") => (Vec::new(), Some(HA)),
            (false, _) => (Vec::new(), Some(BARE)),
        };
        t.settle(
            label,
            seq,
            Ok(LabelTools {
                tools,
                builtin,
                server,
            }),
        );
    }
    t
}

fn session_tools() -> Vec<Tool> {
    tools(json!([
        {"type": "function", "name": "f"},
        {"type": "mcp", "server_label": "docs"},
        {"type": "mcp", "server_label": "Home Assistant"},
        {"type": "mcp", "server_label": "bare"},
    ]))
}

const PARAMS: (&str, &str) = ("session.tools", "session.tool_choice");

fn names(offer: &McpOffer) -> Vec<&str> {
    offer.tools.iter().map(|t| t.name.as_str()).collect()
}

#[test]
fn a_plan_lists_new_and_redefined_labels_keeps_the_rest_and_drops_the_gone() {
    let t = listed();
    let plan = t.plan(
        &tools(json!([
            {"type": "mcp", "server_label": "docs"},
            {"type": "mcp", "server_label": "bare", "allowed_tools": ["x__y"]},
            {"type": "mcp", "server_label": "kb"},
        ])),
        true,
    );
    assert_eq!(plan.keep, vec!["docs"]);
    assert_eq!(
        plan.list,
        vec![spec("bare", Some(&["x__y"])), spec("kb", None)]
    );
    assert_eq!(plan.drop, vec!["Home Assistant"]);
    // `{tool_names}` and the list are one definition.
    let mut t = McpTable::default();
    let seq = t.start(&spec("docs", Some(&["query"])));
    assert!(t.listing());
    let plan = t.plan(
        &tools(
            json!([{"type": "mcp", "server_label": "docs", "allowed_tools": {"tool_names": ["query"]}}]),
        ),
        true,
    );
    assert_eq!(plan.keep, vec!["docs"], "a listing in flight is kept too");
    assert!(plan.list.is_empty());
    t.settle("docs", seq, Err("down".into()));
    assert!(!t.listing());
}

/// A failed label is listed again by an update that names it, unchanged as
/// it is; an update without `tools` leaves it failed (§1.2).
#[test]
fn a_failed_label_is_listed_again_when_an_update_names_it() {
    let mut t = McpTable::default();
    let seq = t.start(&spec("ha", None));
    t.settle("ha", seq, Err("down".into()));
    let session = tools(json!([{"type": "mcp", "server_label": "ha"}]));
    let plan = t.plan(&session, true);
    assert_eq!(plan.list, vec![spec("ha", None)]);
    assert!(plan.keep.is_empty() && plan.drop.is_empty());
    let plan = t.plan(&session, false);
    assert_eq!(plan.keep, vec!["ha"]);
    assert!(plan.list.is_empty());
    // A listed label stays as it is either way.
    let listed = listed();
    let plan = listed.plan(
        &tools(json!([{"type": "mcp", "server_label": "docs"}])),
        true,
    );
    assert_eq!(plan.keep, vec!["docs"]);
}

#[test]
fn only_the_current_listing_of_a_label_settles_it() {
    let mut t = McpTable::default();
    let first = t.start(&spec("docs", None));
    let second = t.start(&spec("docs", Some(&["query"])));
    assert!(!t.current("docs", first) && t.current("docs", second));
    t.settle(
        "docs",
        first,
        Ok(LabelTools {
            tools: vec![tool("docs__resolve", "resolve")],
            builtin: vec![],
            server: None,
        }),
    );
    assert!(t.listing(), "the superseded result changed nothing");
    assert!(t.owner("docs__resolve").is_none());
    t.drop_label("docs");
    assert!(!t.listing() && !t.current("docs", second));
}

#[test]
fn the_session_s_tools_offer_every_listed_tool_by_exposed_name_in_order() {
    let offer = listed()
        .offer(&session_tools(), None, PARAMS, Moment::Created)
        .unwrap();
    assert_eq!(
        names(&offer),
        vec![
            "docs__resolve",
            "docs__query",
            "ha__lights_on",
            "ha__state",
            "x__y"
        ]
    );
    assert_eq!((offer.choice, offer.only), (None, None));
}

/// A response's own entry selects or narrows a listed label by either name;
/// a label alone takes the session's definition (§1.1).
#[test]
fn a_response_entry_selects_or_narrows_a_listed_label() {
    let t = listed();
    let params = ("response.tools", "response.tool_choice");
    let offer = t
        .offer(
            &tools(json!([
                {"type": "mcp", "server_label": "Home Assistant", "allowed_tools": ["state"]},
                {"type": "mcp", "server_label": "docs",
                 "allowed_tools": {"tool_names": ["docs__query"]}},
            ])),
            None,
            params,
            Moment::Created,
        )
        .unwrap();
    assert_eq!(names(&offer), vec!["ha__state", "docs__query"]);

    let offer = t
        .offer(
            &tools(json!([{"type": "mcp", "server_label": "docs"}])),
            None,
            params,
            Moment::Created,
        )
        .unwrap();
    assert_eq!(names(&offer), vec!["docs__resolve", "docs__query"]);

    let e = t
        .offer(
            &tools(json!([{"type": "function", "name": "f"},
                          {"type": "mcp", "server_label": "docs", "allowed_tools": ["request"]}])),
            None,
            params,
            Moment::Created,
        )
        .unwrap_err();
    assert_eq!(e.param.as_deref(), Some("response.tools[1].allowed_tools"));
    assert!(
        e.message.contains("(it listed: resolve, query)"),
        "{}",
        e.message
    );
}

#[test]
fn a_label_the_session_has_not_listed_is_refused_when_a_response_is_created() {
    let e = listed()
        .offer(
            &tools(json!([{"type": "mcp", "server_label": "kb"}])),
            None,
            ("response.tools", "response.tool_choice"),
            Moment::Created,
        )
        .unwrap_err();
    assert_eq!(e.code.as_deref(), Some("invalid_value"));
    assert_eq!(e.param.as_deref(), Some("response.tools[0].server_label"));
    assert!(
        e.message.contains("list it in session.update first"),
        "{}",
        e.message
    );
    // At the launch the label was valid when the response was created; a
    // later update dropped it, and it offers nothing.
    let offer = listed()
        .offer(
            &tools(json!([{"type": "mcp", "server_label": "kb"}])),
            None,
            ("response.tools", "response.tool_choice"),
            Moment::Launched,
        )
        .unwrap();
    assert!(offer.tools.is_empty());
}

#[test]
fn a_label_still_being_listed_is_decided_at_the_launch() {
    let mut t = listed();
    t.start(&spec("kb", None));
    let with_kb = tools(json!([{"type": "mcp", "server_label": "kb"},
                               {"type": "mcp", "server_label": "docs"}]));
    let c = choice(json!({"type": "mcp", "server_label": "kb", "name": "search"}));
    let offer = t
        .offer(&with_kb, Some(&c), PARAMS, Moment::Created)
        .unwrap();
    assert_eq!(names(&offer), vec!["docs__resolve", "docs__query"]);
    assert_eq!(offer.choice, None, "decided at the launch");
}

#[test]
fn a_failed_label_offers_nothing_and_only_a_choice_of_it_is_refused() {
    let mut t = listed();
    let seq = t.start(&spec("kb", None));
    t.settle("kb", seq, Err("the 'kb' toolset offers nothing".into()));
    let with_kb = tools(json!([{"type": "mcp", "server_label": "kb"},
                               {"type": "mcp", "server_label": "docs"}]));
    let offer = t.offer(&with_kb, None, PARAMS, Moment::Created).unwrap();
    assert_eq!(names(&offer), vec!["docs__resolve", "docs__query"]);
    let e = t
        .offer(
            &with_kb,
            Some(&choice(json!({"type": "mcp", "server_label": "kb"}))),
            PARAMS,
            Moment::Created,
        )
        .unwrap_err();
    assert_eq!(e.param.as_deref(), Some("session.tool_choice"));
    assert!(
        e.message.contains("whose listing failed: the 'kb' toolset"),
        "{}",
        e.message
    );
}

/// `(label, name)` maps through the table to the exposed name, whichever
/// name the client wrote; without a name the response is narrowed to the
/// label and one of its tools is required (§1.1).
#[test]
fn an_mcp_tool_choice_maps_through_the_table() {
    let t = listed();
    let all = session_tools();
    for name in ["lights_on", "ha__lights_on"] {
        let c = choice(json!({"type": "mcp", "server_label": "Home Assistant", "name": name}));
        let offer = t.offer(&all, Some(&c), PARAMS, Moment::Launched).unwrap();
        assert_eq!(
            offer.choice,
            Some(ir::ToolChoice::Tool {
                name: "ha__lights_on".into()
            }),
            "{name}"
        );
        assert_eq!(offer.only, None);
        assert_eq!(offer.tools.len(), 5);
    }
    // A bare server's name is its own.
    let c = choice(json!({"type": "mcp", "server_label": "bare", "name": "x__y"}));
    let offer = t.offer(&all, Some(&c), PARAMS, Moment::Launched).unwrap();
    assert_eq!(
        offer.choice,
        Some(ir::ToolChoice::Tool {
            name: "x__y".into()
        })
    );

    let c = choice(json!({"type": "mcp", "server_label": "docs"}));
    let offer = t.offer(&all, Some(&c), PARAMS, Moment::Launched).unwrap();
    assert_eq!(offer.choice, Some(ir::ToolChoice::Required));
    assert_eq!(offer.only.as_deref(), Some("docs"));
    assert_eq!(names(&offer), vec!["docs__resolve", "docs__query"]);
}

#[test]
fn an_mcp_tool_choice_naming_what_the_response_lacks_is_refused() {
    let t = listed();
    let all = session_tools();
    for (c, says) in [
        (
            json!({"type": "mcp", "server_label": "docs", "name": "request"}),
            "(it has: resolve, query)",
        ),
        (
            json!({"type": "mcp", "server_label": "kb"}),
            "not among this response's tools",
        ),
        // The server's prefix is not the label this session wrote.
        (
            json!({"type": "mcp", "server_label": "ha", "name": "state"}),
            "not among this response's tools",
        ),
    ] {
        let e = t
            .offer(&all, Some(&choice(c.clone())), PARAMS, Moment::Created)
            .unwrap_err();
        assert_eq!(e.code.as_deref(), Some("invalid_value"), "{c}");
        assert_eq!(e.param.as_deref(), Some("session.tool_choice"), "{c}");
        assert!(e.message.contains(says), "{c}: {}", e.message);
    }
    let narrowed = tools(json!([
        {"type": "mcp", "server_label": "docs", "allowed_tools": ["resolve"]}]));
    let e = t
        .offer(
            &narrowed,
            Some(&choice(
                json!({"type": "mcp", "server_label": "docs", "name": "query"}),
            )),
            ("response.tools", "response.tool_choice"),
            Moment::Created,
        )
        .unwrap_err();
    assert_eq!(e.param.as_deref(), Some("response.tool_choice"));
    assert!(e.message.contains("(it has: resolve)"), "{}", e.message);
}

/// Whose a name is, as the client wrote the label and the wire names it
/// (§1.3) — never derived from the name itself.
#[test]
fn the_reverse_map_names_the_client_s_label_and_the_wire_name() {
    let t = listed();
    assert_eq!(
        t.owner("ha__state"),
        Some(&Owner {
            label: "Home Assistant".into(),
            wire: "state".into(),
            builtin: false,
            server: Some(HA),
        })
    );
    assert_eq!(
        t.owner("docs__query"),
        Some(&Owner {
            label: "docs".into(),
            wire: "query".into(),
            builtin: true,
            server: None,
        })
    );
    assert_eq!(t.owner("x__y").map(|o| o.label.as_str()), Some("bare"));
    assert_eq!(t.owner("f"), None, "a client function");
    assert_eq!(t.builtin(), vec!["docs__query", "docs__resolve"]);
}

#[test]
fn a_function_named_like_a_listed_tool_is_refused() {
    let t = listed();
    let with = tools(json!([
        {"type": "mcp", "server_label": "docs"},
        {"type": "function", "name": "docs__query"},
    ]));
    let e = t.clash(&with, "session.tools", |_| true).unwrap_err();
    assert_eq!(e.param.as_deref(), Some("session.tools[1].name"));
    assert!(e.message.contains("'docs'"), "{}", e.message);
    // A label the update drops (or lists anew) is not held against it.
    t.clash(&with, "session.tools", |l| l != "docs").unwrap();

    // Listed after the function: the label is the one that fails.
    let fresh = LabelTools {
        tools: vec![tool("kb__f", "f"), tool("f", "f")],
        builtin: vec![],
        server: Some(3),
    };
    let why = t
        .clash_of(
            "kb",
            &fresh,
            &tools(json!([{"type": "function", "name": "f"}])),
        )
        .unwrap();
    assert!(why.contains("the session's function 'f'"), "{why}");
    // Two labels for one server.
    let twice = LabelTools {
        tools: vec![tool("ha__state", "state")],
        builtin: vec![],
        server: Some(HA),
    };
    let why = t.clash_of("ha", &twice, &[]).unwrap();
    assert!(why.contains("'Home Assistant'"), "{why}");
    assert!(why.contains("both labels name the same server"), "{why}");
    assert_eq!(t.clash_of("Home Assistant", &twice, &[]), None);
    // Another server whose tool has come to own the name (final review
    // #2): not "the same server".
    let sibling = LabelTools {
        tools: vec![tool("x__y", "x__y")],
        builtin: vec![],
        server: Some(7),
    };
    let why = t.clash_of("alpha", &sibling, &[]).unwrap();
    assert!(
        why.contains("'bare', another server") && !why.contains("same server"),
        "{why}"
    );
}
