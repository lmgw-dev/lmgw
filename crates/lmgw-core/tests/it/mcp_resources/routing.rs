//! The capabilities both ways, the listings and the routing of a read.

use futures::StreamExt;
use serde_json::{json, Value};

use super::{connect_all, listed, rpc, server, weather, CARD};
use crate::realtime_chat_thread::world;
use crate::support::mcp_apps_stub::{page, Apps};

const UI: &str = "io.modelcontextprotocol/ui";
const MIME: &str = "text/html;profile=mcp-app";

/// `/mcp` says it serves resources and passes the MCP Apps metadata; and
/// lmgw tells every server it connects that it is an MCP Apps host, which
/// the extension asks a server to check before it offers UI tools.
#[tokio::test]
async fn initialize_offers_resources_and_the_apps_extension_both_ways() {
    let w = world(|_| {}).await;
    let wx = server(&w, "wx", "wx", weather()).await;
    connect_all(&w).await;

    let resp =
        w.gw.client()
            .post(format!("{}/mcp", w.gw))
            .header("accept", "application/json")
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                          "params": {"protocolVersion": "2025-11-25", "capabilities": {}}}))
            .send()
            .await
            .unwrap();
    let v: Value = resp.json().await.unwrap();
    let caps = &v["result"]["capabilities"];
    assert_eq!(caps["resources"]["listChanged"], true, "{v}");
    assert!(caps["resources"].get("subscribe").is_none(), "{v}");
    assert_eq!(caps["extensions"][UI]["mimeTypes"], json!([MIME]), "{v}");

    let inits = wx.inits();
    assert!(!inits.is_empty(), "the stub was never connected");
    assert_eq!(
        inits[0]["capabilities"]["extensions"][UI]["mimeTypes"],
        json!([MIME]),
        "{}",
        inits[0]
    );
}

/// A namespaced URI goes to its prefix's server, in the server's spelling,
/// and comes back namespaced; a bare URI goes to the first server by name
/// that lists it or has a template it fits; a URI in a namespace that no
/// server lists still goes to that namespace's server; one nobody has is
/// `-32002`; a server's own error passes on with its code. The listings
/// show each URI once, under the server a read would reach.
#[tokio::test]
async fn list_and_read_route_by_namespace_and_by_who_claims() {
    let w = world(|_| {}).await;
    let wx = server(&w, "wx", "wx", weather()).await;
    let plain_apps = Apps {
        tools: json!([{"name": "ping", "inputSchema": {"type": "object"}}]),
        resources: json!([{"uri": "ui://plain/a", "name": "a"}]),
        templates: json!([{"uriTemplate": "ui://plain/{id}/b", "name": "b"}]),
        ..Apps::default()
    };
    let plain = server(&w, "plain", "", plain_apps).await;
    // Later by name than `plain`, and claiming a URI in `wx`'s namespace.
    let shadow_apps = Apps {
        tools: json!([{"name": "pong", "inputSchema": {"type": "object"}}]),
        resources: json!([{"uri": "ui://plain/a", "name": "dup"},
                          {"uri": "ui://wx__weather/card", "name": "squat"}]),
        templates: json!([]),
        ..Apps::default()
    };
    let shadow = server(&w, "zz", "", shadow_apps).await;
    connect_all(&w).await;
    let owner = w.gw.client();

    let mut uris = listed(&w, &owner).await;
    uris.sort();
    assert_eq!(uris, ["ui://plain/a", "ui://wx__weather/card"]);
    let v = rpc(&w, &owner, "resources/templates/list", json!({})).await;
    let mut templates: Vec<&str> = v["result"]["resourceTemplates"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .map(|t| t["uriTemplate"].as_str().unwrap())
        .collect();
    templates.sort();
    assert_eq!(
        templates,
        ["ui://plain/{id}/b", "ui://wx__weather/{city}/card"]
    );

    let read = |uri: &'static str| {
        let (w, owner) = (&w, &owner);
        async move { rpc(w, owner, "resources/read", json!({"uri": uri})).await }
    };
    let v = read("ui://wx__weather/card").await;
    let c = &v["result"]["contents"][0];
    assert_eq!(c["uri"], "ui://wx__weather/card", "{v}");
    assert_eq!(c["text"], page(CARD), "{v}");
    assert_eq!(c["mimeType"], MIME, "{v}");
    assert_eq!(c["_meta"]["ui"]["prefersBorder"], true, "{v}");

    let v = read("ui://wx__weather/unlisted").await;
    assert_eq!(
        v["result"]["contents"][0]["uri"], "ui://wx__weather/unlisted",
        "{v}"
    );
    assert_eq!(wx.reads(), [CARD, "ui://weather/unlisted"]);

    for uri in ["ui://plain/a", "ui://plain/7/b"] {
        let v = read(uri).await;
        assert_eq!(v["result"]["contents"][0]["text"], page(uri), "{v}");
    }
    assert_eq!(plain.reads(), ["ui://plain/a", "ui://plain/7/b"]);
    assert!(shadow.reads().is_empty(), "{:?}", shadow.reads());

    let v = read("ui://nothing/x").await;
    assert_eq!(v["error"]["code"], -32002, "{v}");
    let v = read("ui://wx__missing").await;
    assert_eq!(v["error"]["code"], -32002, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("server 'wx'"),
        "{v}"
    );

    let v = rpc(&w, &owner, "resources/read", json!({})).await;
    assert_eq!(v["error"]["code"], -32602, "{v}");
    let v = rpc(&w, &owner, "resources/list", json!({"cursor": "x"})).await;
    assert_eq!(v["error"]["code"], -32602, "{v}");
    let v = rpc(&w, &owner, "resources/subscribe", json!({"uri": CARD})).await;
    assert_eq!(v["error"]["code"], -32601, "{v}");
}

/// A server with no tools of its own beyond `tool`, listing `resources` and
/// offering `templates`.
fn offering(tool: Value, resources: Value, templates: Value) -> Apps {
    Apps {
        tools: json!([tool]),
        resources,
        templates,
        ..Apps::default()
    }
}

/// Of the servers a URI can belong to, the one whose own tool names it has
/// it, then one that lists it, then one with a template it fits — the first
/// by name only within the same kind of claim. A prefixed server's template
/// without a literal `scheme://` claims nothing (it would take every other
/// server's `urn:…`), and is not listed.
#[tokio::test]
async fn a_tool_s_claim_beats_a_listing_and_a_listing_beats_a_template() {
    let w = world(|_| {}).await;
    let tool = |name: &str| json!({"name": name, "inputSchema": {"type": "object"}});
    let aa = server(
        &w,
        "aa",
        "",
        offering(
            tool("a1"),
            json!([]),
            json!([{"uriTemplate": "ui://{any}", "name": "any"}]),
        ),
    )
    .await;
    let bb = server(
        &w,
        "bb",
        "",
        offering(
            tool("b1"),
            json!([{"uri": "ui://shared/card", "name": "card"},
                   {"uri": "ui://bb/listed", "name": "listed"}]),
            json!([]),
        ),
    )
    .await;
    let mm = server(
        &w,
        "mm",
        "",
        offering(
            json!({"name": "m1", "inputSchema": {"type": "object"},
                   "_meta": {"ui": {"resourceUri": "ui://shared/card"}}}),
            json!([{"uri": "ui://shared/card", "name": "card"}]),
            json!([]),
        ),
    )
    .await;
    let ap = server(
        &w,
        "ap",
        "ap",
        offering(
            tool("p1"),
            json!([]),
            json!([{"uriTemplate": "{u}", "name": "everything"},
                   {"uriTemplate": "urn:ap:{n}", "name": "no authority"},
                   {"uriTemplate": "ui://ok/{n}", "name": "ok"}]),
        ),
    )
    .await;
    let zz = server(
        &w,
        "zz",
        "",
        offering(
            tool("z1"),
            json!([{"uri": "urn:zz:1", "name": "urn"}]),
            json!([]),
        ),
    )
    .await;
    connect_all(&w).await;
    let owner = w.gw.client();

    let mut uris = listed(&w, &owner).await;
    uris.sort();
    assert_eq!(uris, ["ui://bb/listed", "ui://shared/card", "urn:zz:1"]);
    let v = rpc(&w, &owner, "resources/templates/list", json!({})).await;
    let mut templates: Vec<&str> = v["result"]["resourceTemplates"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .map(|t| t["uriTemplate"].as_str().unwrap())
        .collect();
    templates.sort();
    assert_eq!(templates, ["ui://ap__ok/{n}", "ui://{any}"]);

    for uri in [
        "ui://bb/listed",
        "ui://shared/card",
        "ui://other/x",
        "urn:zz:1",
        "ui://ap__ok/7",
    ] {
        let v = rpc(&w, &owner, "resources/read", json!({"uri": uri})).await;
        assert_eq!(v["result"]["contents"][0]["uri"], uri, "{uri}: {v}");
    }
    for uri in ["urn:zz:2", "urn:ap:5"] {
        let v = rpc(&w, &owner, "resources/read", json!({"uri": uri})).await;
        assert_eq!(v["error"]["code"], -32002, "{uri}: {v}");
    }
    assert_eq!(aa.reads(), ["ui://other/x"]);
    assert_eq!(bb.reads(), ["ui://bb/listed"]);
    assert_eq!(mm.reads(), ["ui://shared/card"]);
    assert_eq!(zz.reads(), ["urn:zz:1"]);
    assert_eq!(ap.reads(), ["ui://ok/7"]);
}

/// A URI made to make a backtracking template match take forever — many
/// places for each expression, and no fit at the end — is answered at once.
#[tokio::test]
async fn a_long_uri_against_many_expressions_is_answered_at_once() {
    let w = world(|_| {}).await;
    let _t = server(
        &w,
        "t",
        "",
        offering(
            json!({"name": "t1", "inputSchema": {"type": "object"}}),
            json!([]),
            json!([{"uriTemplate": "x://{a}/{b}/{c}/{d}/{e}/{f}/{g}/{h}/end", "name": "deep"}]),
        ),
    )
    .await;
    connect_all(&w).await;
    let uri = format!("x://{}nope", "/".repeat(100_000));
    let started = std::time::Instant::now();
    let v = rpc(&w, &w.gw.client(), "resources/read", json!({"uri": uri})).await;
    assert_eq!(v["error"]["code"], -32002, "{}", v["error"]);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
}

/// A read of a server that is not connected connects it, as a call does.
#[tokio::test]
async fn a_read_connects_its_server() {
    let w = world(|_| {}).await;
    let wx = server(&w, "wx", "wx", weather()).await;
    let v = rpc(
        &w,
        &w.gw.client(),
        "resources/read",
        json!({"uri": "ui://wx__weather/card"}),
    )
    .await;
    assert_eq!(v["result"]["contents"][0]["text"], page(CARD), "{v}");
    assert_eq!(wx.reads(), [CARD]);
}

/// `GET /mcp` says `resources/list_changed` when the aggregate's
/// composition changes, beside `tools/list_changed`.
#[tokio::test]
async fn the_stream_says_the_resources_changed() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let sid = crate::mcp_host::mcp_session(&w, &owner).await;
    let resp = owner
        .get(format!("{}/mcp", w.gw))
        .header("mcp-session-id", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let state = w.state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        state.mcp.notify_tools_changed_for_tests();
    });
    let mut body = resp.bytes_stream();
    let saw = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut buf = String::new();
        while let Some(chunk) = body.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            if buf.contains("notifications/resources/list_changed")
                && buf.contains("notifications/tools/list_changed")
            {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(saw, "no resources/list_changed beside tools/list_changed");
}
