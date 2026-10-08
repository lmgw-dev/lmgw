//! What a device may write (client-apps design L3, L5, review W2-4): no
//! Admin Chat thread; tool labels and knowledge bases only within its tool
//! scope — refused `403 tool_label_out_of_scope`, nothing written.

use quickdoc_core::embed::EmbedIdentity;
use serde_json::{json, Value};

use super::{chat_thread, device_world, get, pair, post};
use crate::realtime_chat_thread::{world, World};

/// Two registered servers, both switched off (nothing connects): `bare`
/// without a tool prefix, `pfx` with one.
async fn servers(w: &World) {
    for (name, prefix) in [("bare", ""), ("pfx", "pfx")] {
        sqlx::query(
            "INSERT INTO mcp_servers (name, enabled, transport, url, tool_prefix) \
             VALUES (?1, 0, 'http', 'http://127.0.0.1:9/mcp', ?2)",
        )
        .bind(name)
        .bind(prefix)
        .execute(&w.state.db)
        .await
        .unwrap();
    }
    w.state.reload_snapshot().await.unwrap();
}

/// A knowledge base of the owner's, with nothing in it: its id.
async fn knowledge_base(w: &World) -> i64 {
    lmgw_core::knowledge::store::insert_kb(
        &w.state.knowledge.pool,
        &lmgw_core::knowledge::store::NewKb {
            name: "Taxes".into(),
            description: String::new(),
            embed_alias: "e".into(),
            embed: EmbedIdentity::new("u", "m", 4),
            rerank_alias: String::new(),
            vision_alias: String::new(),
            chunk_tokens: 512,
            chunk_overlap: 64,
            mcp_visible: false,
        },
    )
    .await
    .unwrap()
}

fn refused(status: u16, v: &Value, naming: &str) {
    assert_eq!(status, 403, "{v}");
    assert_eq!(v["code"], "tool_label_out_of_scope", "{v}");
    let message = v["message"].as_str().unwrap();
    assert!(message.contains(naming), "names {naming}: {v}");
}

#[tokio::test]
async fn a_device_creates_no_admin_thread() {
    let (w, d) = device_world().await;
    let (s, v) = post(
        &w,
        &d.client,
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "kind": "admin" }),
    )
    .await;
    assert_eq!(s, 403, "{v}");
    assert_eq!(v["code"], "forbidden", "{v}");
    let (_, all) = get(&w, &w.gw.client(), "/chat/api/threads?archived=all").await;
    assert_eq!(all["threads"], json!([]), "nothing was created: {all}");
}

#[tokio::test]
async fn the_self_admin_toolset_is_never_a_device_s() {
    let (w, d) = device_world().await;
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let path = format!("/chat/api/threads/{tid}/settings");
    let (s, v) = post(
        &w,
        &d.client,
        &path,
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    refused(s, &v, "'lmgw'");
    let (_, t) = get(&w, &d.client, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(t["thread"]["mcp_tools"], json!([]), "nothing written: {t}");

    // The owner may attach it, and the thread then drives the self-admin
    // plane: for the device it is gone, writes included (review W3-1).
    let (s, v) = post(
        &w,
        &w.gw.client(),
        &path,
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(
        &w,
        &d.client,
        &path,
        json!({ "mcp_tools": [{ "server_label": "lmgw" }], "temperature": 0.4 }),
    )
    .await;
    assert_eq!((s, &v["code"]), (404, &json!("not_found")), "{v}");
}

#[tokio::test]
async fn labels_are_checked_against_the_device_s_tool_scope() {
    let w = world(|_| {}).await;
    servers(&w).await;
    let d = pair(
        &w,
        "phone",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "docs__*\npfx__*" }),
    )
    .await;
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let path = format!("/chat/api/threads/{tid}/settings");
    let set = |label: &str| json!({ "mcp_tools": [{ "server_label": label }] });

    // A built-in toolset: every tool it exposes must be in reach.
    let (s, v) = post(&w, &d.client, &path, set("kb")).await;
    refused(s, &v, "kb__");
    let (s, v) = post(&w, &d.client, &path, set("docs")).await;
    assert_eq!(s, 200, "{v}");

    // A prefixed server that lists nothing now: judged by its namespace.
    let (s, v) = post(&w, &d.client, &path, set("pfx")).await;
    assert_eq!(s, 200, "{v}");
    // A bare one cannot be checked, and says so.
    let (s, v) = post(&w, &d.client, &path, set("bare")).await;
    refused(s, &v, "without a tool prefix");
    // An unknown label is refused without naming any other.
    let (s, v) = post(&w, &d.client, &path, set("nowhere")).await;
    refused(s, &v, "'nowhere'");
    assert!(!v["message"].as_str().unwrap().contains("bare"), "{v}");

    // A narrower scope does not admit the whole namespace.
    let narrow = pair(
        &w,
        "watch",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "pfx__read" }),
    )
    .await;
    let t2 = chat_thread(&w, &narrow.client, "chatty").await;
    let (s, v) = post(
        &w,
        &narrow.client,
        &format!("/chat/api/threads/{t2}/settings"),
        set("pfx"),
    )
    .await;
    refused(s, &v, "'pfx__'");
}

#[tokio::test]
async fn folder_defaults_pass_the_same_check() {
    let w = world(|_| {}).await;
    let d = pair(
        &w,
        "phone",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "docs__*" }),
    )
    .await;
    let (s, v) = post(
        &w,
        &d.client,
        "/chat/api/folders",
        json!({ "name": "F", "defaults": { "mcp_tools": [{ "server_label": "kb" }] } }),
    )
    .await;
    refused(s, &v, "kb__");
    let (s, f) = post(&w, &d.client, "/chat/api/folders", json!({ "name": "F" })).await;
    assert_eq!(s, 200, "{f}");
    let id = f["id"].as_i64().unwrap();
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/folders/{id}"),
        json!({ "defaults": { "mcp_tools": [{ "server_label": "lmgw" }] } }),
    )
    .await;
    refused(s, &v, "'lmgw'");
}

#[tokio::test]
async fn knowledge_bases_need_kb_search_in_the_device_s_scope() {
    let w = world(|_| {}).await;
    let kb = knowledge_base(&w).await;
    let d = pair(
        &w,
        "phone",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "docs__*" }),
    )
    .await;
    let tid = chat_thread(&w, &d.client, "chatty").await;

    // A thread's bases, a message's own, a folder's default — refused
    // without the base's name, and an id that names no base answers alike
    // (review W3-5): the device learns nothing of the owner's bases.
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "kb_ids": [kb] }),
    )
    .await;
    refused(s, &v, "kb__search");
    assert!(!v.to_string().contains("Taxes"), "{v}");
    let (s2, unknown) = post(
        &w,
        &d.client,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "kb_ids": [4242] }),
    )
    .await;
    assert_eq!(
        (s2, &unknown),
        (s, &v),
        "a known and an unknown id answer alike"
    );
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "what is due?", "kb_refs": [kb] }),
    )
    .await;
    refused(s, &v, "kb__search");
    let (s, v) = post(
        &w,
        &d.client,
        "/chat/api/folders",
        json!({ "name": "F", "defaults": { "kb_ids": [kb] } }),
    )
    .await;
    refused(s, &v, "kb__search");
    assert!(!v.to_string().contains("Taxes"), "{v}");
    let (_, t) = get(&w, &d.client, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(t["messages"], json!([]), "nothing was sent: {t}");
    // An edit's new `#` picks alike (review W3-11).
    let (message, _) = super::seed(&w, tid, "what is due?").await;
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/threads/{tid}/messages/{message}/edit"),
        json!({ "content": "what is due now?", "kb_refs": [kb] }),
    )
    .await;
    refused(s, &v, "kb__search");

    // A scope that admits the knowledge tools may name them.
    let reader = pair(
        &w,
        "tablet",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "kb__*" }),
    )
    .await;
    let (s, v) = post(
        &w,
        &reader.client,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "kb_ids": [kb] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
}

/// The model and voice aliases a device writes are within its own alias
/// scope (review W3-4): the owner's later turns, dictation and read-aloud
/// go to them. An alias the thread or folder already carries passes.
#[tokio::test]
async fn the_aliases_a_device_writes_are_within_its_scope() {
    let w = world(|_| {}).await;
    let d = pair(
        &w,
        "phone",
        json!({ "scope_mode": "allow", "scope_patterns": "other" }),
    )
    .await;
    let key_scope = |s: u16, v: &Value, alias: &str| {
        assert_eq!((s, &v["code"]), (403, &json!("key_scope")), "{v}");
        assert!(v["message"].as_str().unwrap().contains(alias), "{v}");
    };
    let (s, v) = post(
        &w,
        &d.client,
        "/chat/api/threads",
        json!({ "model_alias": "chatty" }),
    )
    .await;
    key_scope(s, &v, "chatty");
    let tid = chat_thread(&w, &d.client, "other").await;
    let path = format!("/chat/api/threads/{tid}/settings");
    let (s, v) = post(&w, &d.client, &path, json!({ "model_alias": "chatty" })).await;
    key_scope(s, &v, "chatty");
    let (s, v) = post(
        &w,
        &d.client,
        &path,
        json!({ "voice": { "tts_alias": "speak" } }),
    )
    .await;
    key_scope(s, &v, "speak");
    let (_, t) = get(&w, &d.client, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(t["thread"]["model_alias"], "other", "nothing written: {t}");

    // The owner may set it; the device sending it back unchanged passes.
    let (s, v) = post(
        &w,
        &w.gw.client(),
        &path,
        json!({ "model_alias": "chatty" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(
        &w,
        &d.client,
        &path,
        json!({ "model_alias": "chatty", "temperature": 0.3 }),
    )
    .await;
    assert_eq!(s, 200, "{v}");

    // A folder's defaults alike.
    let (s, v) = post(
        &w,
        &d.client,
        "/chat/api/folders",
        json!({ "name": "F", "defaults": { "model_alias": "chatty" } }),
    )
    .await;
    key_scope(s, &v, "chatty");
    let (s, v) = post(
        &w,
        &w.gw.client(),
        "/chat/api/folders",
        json!({ "name": "G", "defaults": { "model_alias": "chatty" } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let g = v["id"].as_i64().unwrap();
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/folders/{g}"),
        json!({ "defaults": { "model_alias": "chatty", "temperature": 0.2 } }),
    )
    .await;
    assert_eq!(s, 200, "unchanged passes: {v}");
    // A thread created in it takes the folder's model, which the owner chose.
    let (s, v) = post(
        &w,
        &d.client,
        "/chat/api/threads",
        json!({ "model_alias": "other", "folder_id": g }),
    )
    .await;
    assert_eq!((s, &v["model_alias"]), (200, &json!("chatty")), "{v}");
}
