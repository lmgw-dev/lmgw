//! The Chat's tool picker on the Chat plane (chat-plane design §2.2, D4,
//! D5): `GET /chat/api/tools` lists the toolsets a caller may attach under
//! its tool scope — a device what `GET /v1/mcp/servers` lists it, `lmgw`
//! only at `full` — and `GET /chat/api/tools/{label}` one's tools, the
//! same resolution a turn runs, starting no service agent (D16, tested in
//! `agents_service::picker` beside its fake container).

use serde_json::{json, Value};

use super::{get, op, pair, Device};
use crate::mcp_host::{host_world, linked};
use crate::realtime_chat_thread::{world, World};
use crate::support::mcp_stub;

/// Two registered servers: `pfx` with a tool prefix, `bare` without one,
/// both enabled and never connected (their URL answers nothing).
async fn servers(w: &World) {
    for (name, prefix) in [("bare", ""), ("pfx", "pfx")] {
        sqlx::query(
            "INSERT INTO mcp_servers (name, enabled, transport, url, tool_prefix, autostart) \
             VALUES (?1, 1, 'http', 'http://127.0.0.1:9/mcp', ?2, 0)",
        )
        .bind(name)
        .bind(prefix)
        .execute(&w.state.db)
        .await
        .unwrap();
    }
    w.state.reload_snapshot().await.unwrap();
}

/// `(server_label, kind)` of every listed source, in order.
fn sources(v: &Value) -> Vec<(String, String)> {
    v["sources"]
        .as_array()
        .unwrap_or_else(|| panic!("no sources in {v}"))
        .iter()
        .map(|s| {
            (
                s["server_label"].as_str().unwrap().to_string(),
                s["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn labels(v: &Value) -> Vec<String> {
    sources(v).into_iter().map(|(l, _)| l).collect()
}

fn source<'a>(v: &'a Value, label: &str) -> &'a Value {
    v["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["server_label"] == label)
        .unwrap_or_else(|| panic!("{label} is not listed: {v}"))
}

fn tool_names(source: &Value) -> Vec<String> {
    source["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tools in {source}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// The labels `GET /v1/mcp/servers` lists device `d`.
async fn discovered(w: &World, d: &Device) -> Vec<String> {
    let (s, v) = get(w, &d.client, "/v1/mcp/servers").await;
    assert_eq!(s, 200, "{v}");
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["server_label"].as_str().unwrap().to_string())
        .collect()
}

/// The owner is listed every built-in toolset, in their fixed order, one
/// that offers nothing with its reason, then every enabled server — as the
/// inventory and the server list showed the picker before.
#[tokio::test]
async fn the_owner_is_listed_every_toolset_and_server() {
    let w = world(|_| {}).await;
    servers(&w).await;
    let (s, v) = get(&w, &w.gw.client(), "/chat/api/tools").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        sources(&v),
        [
            ("lmgw", "builtin"),
            ("docs", "builtin"),
            ("kb", "builtin"),
            ("image", "builtin"),
            ("audio", "builtin"),
            ("bare", "server"),
            ("pfx", "server"),
        ]
        .map(|(l, k)| (l.to_string(), k.to_string()))
        .to_vec()
    );
    let docs = source(&v, "docs");
    assert_eq!(docs["status"], "ready", "{v}");
    let names = tool_names(docs);
    assert!(
        !names.is_empty() && names.iter().all(|n| n.starts_with("docs__")),
        "{v}"
    );
    assert_eq!(docs["tool_count"].as_u64(), Some(names.len() as u64));
    // A toolset that offers tools lists them; one that offers none (no
    // model of its kind) is listed with why.
    for label in ["lmgw", "docs", "kb", "image", "audio"] {
        let b = source(&v, label);
        match b["status"].as_str() {
            Some("ready") => assert!(!tool_names(b).is_empty(), "{b}"),
            Some("unavailable") => {
                assert_eq!(b["tools"], json!([]), "{b}");
                assert!(b["reason"].as_str().is_some_and(|r| !r.is_empty()), "{b}");
            }
            other => panic!("{label}: {other:?} in {b}"),
        }
    }
    // A server's tools are its detail's: none inline, nothing listed yet.
    let pfx = source(&v, "pfx");
    assert_eq!(
        (pfx["tools"].clone(), pfx["tool_count"].as_u64()),
        (json!([]), Some(0))
    );
}

/// A device is listed what it may attach: no toolset that offers it
/// nothing, no `lmgw` below `full`, and under an allow list only what the
/// list reaches — the labels `/v1/mcp/servers` lists the same device.
#[tokio::test]
async fn a_device_is_listed_what_its_scope_reaches() {
    let w = world(|_| {}).await;
    servers(&w).await;
    let phone = pair(&w, "phone", json!({})).await;
    let (s, v) = get(&w, &phone.client, "/chat/api/tools").await;
    assert_eq!(s, 200, "{v}");
    let listed = labels(&v);
    for label in ["docs", "kb", "bare", "pfx"] {
        assert!(listed.contains(&label.to_string()), "{label}: {v}");
    }
    assert!(!listed.contains(&"lmgw".to_string()), "{v}");
    assert_eq!(listed, discovered(&w, &phone).await);
    // Every toolset listed offers it tools.
    for s in v["sources"].as_array().unwrap() {
        assert_ne!(s["status"], "unavailable", "{s}");
        assert!(s["reason"].is_null(), "{s}");
    }

    let scoped = pair(
        &w,
        "tablet",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "docs__*\npfx__*" }),
    )
    .await;
    let (s, v) = get(&w, &scoped.client, "/chat/api/tools").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        labels(&v),
        ["docs", "pfx"].map(String::from).to_vec(),
        "{v}"
    );
    assert_eq!(labels(&v), discovered(&w, &scoped).await);
    // Out of its scope: not found, as an unknown label is.
    for label in ["kb", "bare", "nosuch"] {
        let (s, d) = get(&w, &scoped.client, &format!("/chat/api/tools/{label}")).await;
        assert_eq!(
            (s, d["code"].as_str()),
            (404, Some("not_found")),
            "{label}: {d}"
        );
    }
    let (s, d) = get(&w, &scoped.client, "/chat/api/tools/docs").await;
    assert_eq!(s, 200, "{d}");
    assert_eq!(d["server_label"], "docs");
    let names = tool_names(&d);
    assert!(
        !names.is_empty() && names.iter().all(|n| n.starts_with("docs__")),
        "{d}"
    );
}

/// D5: `lmgw` is offered to a device only while its admin tools are at
/// `full` (its own level capped by the gateway's): below it the device uses
/// the threads that carry it and attaches it to none. The owner always is.
#[tokio::test]
async fn lmgw_is_offered_to_a_device_only_at_full() {
    let w = world(|s| s.self_admin = lmgw_core::config::SelfAdmin::Full).await;
    let read_only = pair(&w, "laptop", json!({ "self_admin": "read_only" })).await;
    let full = pair(&w, "desktop", json!({ "self_admin": "full" })).await;
    let (_, v) = get(&w, &read_only.client, "/chat/api/tools").await;
    assert!(!labels(&v).contains(&"lmgw".to_string()), "{v}");
    let (s, d) = get(&w, &read_only.client, "/chat/api/tools/lmgw").await;
    assert_eq!((s, d["code"].as_str()), (404, Some("not_found")), "{d}");
    // /v1/mcp/servers lists it to the same device: it reaches the toolset
    // in the threads that carry it, and attaches it to none.
    assert!(discovered(&w, &read_only)
        .await
        .contains(&"lmgw".to_string()));

    for client in [&full.client, &w.gw.client()] {
        let (_, v) = get(&w, client, "/chat/api/tools").await;
        let lmgw = source(&v, "lmgw");
        assert_eq!(lmgw["status"], "ready", "{v}");
        assert!(
            tool_names(lmgw).contains(&"lmgw__status".to_string()),
            "{v}"
        );
        let (s, d) = get(&w, client, "/chat/api/tools/lmgw").await;
        assert_eq!(s, 200, "{d}");
    }
}

/// A tool the owner switched off is neither listed nor counted, for the
/// owner or a device.
#[tokio::test]
async fn a_switched_off_tool_is_not_offered() {
    let w = world(|_| {}).await;
    let phone = pair(&w, "phone", json!({})).await;
    let (s, v) = op(
        &w,
        "tool_set",
        json!({ "name": "docs__request", "enabled": false }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    for client in [&w.gw.client(), &phone.client] {
        let (_, v) = get(&w, client, "/chat/api/tools").await;
        let docs = source(&v, "docs");
        let names = tool_names(docs);
        assert!(!names.is_empty(), "{v}");
        assert!(!names.contains(&"docs__request".to_string()), "{v}");
        assert_eq!(docs["tool_count"].as_u64(), Some(names.len() as u64));
        let (_, d) = get(&w, client, "/chat/api/tools/docs").await;
        assert!(
            !tool_names(&d).contains(&"docs__request".to_string()),
            "{d}"
        );
    }
}

/// D9: a server's count is the tools of it, as listed, the caller may tick:
/// under a narrow scope only those it admits, and never one the owner
/// switched off.
#[tokio::test]
async fn a_server_s_count_is_what_the_caller_may_tick() {
    let w = world(|_| {}).await;
    let stub = mcp_stub::stub(
        json!(["alpha", "beta", "gamma"]
            .map(|n| json!({"name": n, "inputSchema": {"type": "object"}}))),
        false,
    )
    .await;
    mcp_stub::register(&w.state, "stubby", "stub", &stub.url, true, None).await;
    let narrow = pair(
        &w,
        "phone",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "stub__alpha\nstub__beta" }),
    )
    .await;
    let owner = w.gw.client();
    // The detail connects it; the list then counts what it listed.
    let (s, d) = get(&w, &owner, "/chat/api/tools/stub").await;
    assert_eq!(s, 200, "{d}");
    assert_eq!(
        tool_names(&d),
        ["stub__alpha", "stub__beta", "stub__gamma"].map(String::from)
    );
    let count = |v: &Value| {
        let stub = source(v, "stub");
        assert_eq!(stub["status"], "ready", "{v}");
        assert_eq!(stub["kind"], "server", "{v}");
        stub["tool_count"].as_u64()
    };
    let (_, v) = get(&w, &owner, "/chat/api/tools").await;
    assert_eq!(count(&v), Some(3));
    let (_, v) = get(&w, &narrow.client, "/chat/api/tools").await;
    assert_eq!(count(&v), Some(2));
    let (s, d) = get(&w, &narrow.client, "/chat/api/tools/stub").await;
    assert_eq!(s, 200, "{d}");
    assert_eq!(
        tool_names(&d),
        ["stub__alpha", "stub__beta"].map(String::from)
    );

    let (s, v) = op(
        &w,
        "tool_set",
        json!({ "name": "stub__beta", "enabled": false }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = get(&w, &owner, "/chat/api/tools").await;
    assert_eq!(count(&v), Some(2));
    let (_, v) = get(&w, &narrow.client, "/chat/api/tools").await;
    assert_eq!(count(&v), Some(1));
}

/// L16: a device-hosted label is listed to another device only with its
/// device-tools grant, as `/v1/mcp/servers` lists it; without it the detail
/// is the 404 of a label that is not there. The hosting device and the
/// owner are listed it.
#[tokio::test]
async fn another_device_s_hosted_tools_need_its_grant() {
    let (w, desktop) = host_world().await;
    let _dev = linked(&w, &desktop, &["echo"]).await;
    let phone = pair(&w, "phone", json!({})).await;
    let tablet = pair(&w, "tablet", json!({ "device_tools": ["desktop"] })).await;

    let (s, v) = get(&w, &phone.client, "/chat/api/tools").await;
    assert_eq!(s, 200, "{v}");
    assert!(!labels(&v).contains(&"desktop".to_string()), "{v}");
    assert_eq!(labels(&v), discovered(&w, &phone).await);
    let (s, d) = get(&w, &phone.client, "/chat/api/tools/desktop").await;
    assert_eq!((s, d["code"].as_str()), (404, Some("not_found")), "{d}");

    for (who, client) in [
        ("the granted device", &tablet.client),
        ("the hosting device", &desktop.client),
        ("the owner", &w.gw.client()),
    ] {
        let (s, v) = get(&w, client, "/chat/api/tools").await;
        assert_eq!(s, 200, "{who}: {v}");
        let row = source(&v, "desktop");
        assert_eq!(
            (row["kind"].as_str(), row["status"].as_str()),
            (Some("server"), Some("ready")),
            "{who}: {row}"
        );
        assert_eq!(row["tool_count"].as_u64(), Some(1), "{who}: {row}");
        let (s, d) = get(&w, client, "/chat/api/tools/desktop").await;
        assert_eq!(s, 200, "{who}: {d}");
        assert_eq!(tool_names(&d), vec!["desktop__echo".to_string()], "{who}");
    }
    for d in [&tablet, &desktop] {
        let (_, v) = get(&w, &d.client, "/chat/api/tools").await;
        assert_eq!(labels(&v), discovered(&w, d).await);
    }
}
