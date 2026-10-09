//! The owner's floor at the turn, and the write checks' order and reach
//! (client-apps design §6.6, the re-review's findings 2–5): a label that
//! names its server only after the write (a server deleted and added back,
//! an owner's re-prefix) runs under the floor all the same; two stored
//! entries for one server keep the stricter when a device drops one; the
//! two-entries `400` waits for the scope check; a stored value the rules
//! now refuse does not block an unrelated write.

use serde_json::{json, Value};

use super::{chat_thread, device_world, get, pair, post};
use crate::chat_approvals::{approval_frames, done, send};
use crate::realtime_chat_thread::World;
use crate::support::realtime_mcp::calls;

fn entry(label: &str, ra: Value) -> Value {
    json!({ "server_label": label, "require_approval": ra })
}

fn tools(entries: &[Value]) -> Value {
    json!({ "mcp_tools": entries })
}

fn settings(tid: i64) -> String {
    format!("/chat/api/threads/{tid}/settings")
}

/// A live stub MCP server named `stubsrv` with the tool prefix `stub` (one
/// tool, `echo`): its URL, to add it back with.
async fn stub(w: &World) -> String {
    let (url, _) = crate::responses_api::mcp_stub().await;
    crate::responses_api::register_mcp(&w.state, "stubsrv", "stub", &url).await;
    url
}

fn stub_id(w: &World) -> i64 {
    w.state
        .snapshot()
        .mcp_servers
        .values()
        .find(|s| s.name == "stubsrv")
        .expect("registered")
        .id
}

async fn reloaded(w: &World) {
    w.state.reload_snapshot().await.unwrap();
    w.state.mcp.reconcile(&w.state.snapshot()).await;
}

async fn set_prefix(w: &World, prefix: &str) {
    sqlx::query("UPDATE mcp_servers SET tool_prefix = ?2 WHERE id = ?1")
        .bind(stub_id(w))
        .bind(prefix)
        .execute(&w.state.db)
        .await
        .unwrap();
    reloaded(w).await;
}

/// The owner's turn of `tid` whose model calls `stub__echo`: it stops on
/// that call's approval, and nothing ran.
async fn echo_waits(w: &World, tid: i64) {
    w.chat
        .push(calls(&[(0, "call_1", "stub__echo", "{}")], "tool_calls"));
    let frames = send(w, &w.gw.client(), tid, "echo it").await;
    let asks = approval_frames(&frames);
    assert_eq!(asks.len(), 1, "the call waits for an approval: {frames:?}");
    assert_eq!(asks[0]["name"], "echo", "{}", asks[0]);
    assert!(
        !frames
            .iter()
            .any(|(e, d)| e == "tool" && d["event"] == "result"),
        "nothing ran: {frames:?}"
    );
    assert_eq!(
        done(&frames)["pending_approvals"].as_array().map(Vec::len),
        Some(1)
    );
}

/// A server deleted after the owner gated it by its name: a device's entry
/// by its prefix names nothing then, so the write passes — and once the
/// server is added back, the turn runs it under the owner's floor.
#[tokio::test]
async fn a_label_that_names_its_server_again_runs_under_the_floor() {
    let (w, d) = device_world().await;
    let url = stub(&w).await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &owner, "chatty").await;
    let path = settings(tid);
    let (s, v) = post(
        &w,
        &owner,
        &path,
        tools(&[entry("stubsrv", json!("always"))]),
    )
    .await;
    assert_eq!(s, 200, "{v}");

    lmgw_core::store::delete_mcp_server(&w.state.db, stub_id(&w))
        .await
        .unwrap();
    reloaded(&w).await;
    let (s, v) = post(
        &w,
        &d.client,
        &path,
        tools(&[entry("stub", json!("never"))]),
    )
    .await;
    assert_eq!(
        s, 200,
        "a label naming nothing is compared with itself: {v}"
    );

    crate::responses_api::register_mcp(&w.state, "stubsrv", "stub", &url).await;
    echo_waits(&w, tid).await;
}

/// The owner re-prefixes a server: the floor's label names nothing while
/// a device writes the server by its name, and gates it again once the
/// prefix is back.
#[tokio::test]
async fn an_owner_s_re_prefix_does_not_lose_the_floor() {
    let (w, d) = device_world().await;
    stub(&w).await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &owner, "chatty").await;
    let path = settings(tid);
    let (s, v) = post(&w, &owner, &path, tools(&[entry("stub", json!("always"))])).await;
    assert_eq!(s, 200, "{v}");

    set_prefix(&w, "st").await;
    let (s, v) = post(
        &w,
        &d.client,
        &path,
        tools(&[entry("stubsrv", json!("never"))]),
    )
    .await;
    assert_eq!(
        s, 200,
        "neither the floor's label nor the stored one names it now: {v}"
    );

    set_prefix(&w, "stub").await;
    echo_waits(&w, tid).await;
}

/// Two entries for one server stored before they were refused: a device
/// dropping the stricter is a loosening, the pair sent back unchanged is
/// none, and a turn runs the server under the stricter.
#[tokio::test]
async fn dropping_one_of_two_stored_entries_is_compared_with_the_other() {
    let (w, d) = device_world().await;
    stub(&w).await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &owner, "chatty").await;
    let both = json!([
        entry("stubsrv", json!("never")),
        entry("stub", json!("always"))
    ]);
    // As a row from before 0076 reads: its rules are its floor.
    sqlx::query("UPDATE chat_threads SET mcp_tools = ?2, approval_floor = ?2 WHERE id = ?1")
        .bind(tid)
        .bind(both.to_string())
        .execute(&w.state.db)
        .await
        .unwrap();
    let path = settings(tid);

    let (s, v) = post(
        &w,
        &d.client,
        &path,
        tools(&[entry("stubsrv", json!("never"))]),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("approval_loosen_refused")),
        "{v}"
    );
    let (s, v) = post(&w, &d.client, &path, json!({ "mcp_tools": both })).await;
    assert_eq!(s, 200, "carried unchanged: {v}");
    echo_waits(&w, tid).await;
}

/// A scoped device writing two labels for a server out of its reach hears
/// the scope's refusal, not that the two name one server.
#[tokio::test]
async fn the_two_entries_400_waits_for_the_scope_check() {
    let (w, _) = device_world().await;
    sqlx::query(
        "INSERT INTO mcp_servers (name, enabled, transport, url, tool_prefix) \
         VALUES ('github', 0, 'http', 'http://127.0.0.1:9/mcp', 'gh')",
    )
    .execute(&w.state.db)
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let d = pair(
        &w,
        "watch",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "docs__*" }),
    )
    .await;
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let both = tools(&[
        entry("gh", json!("always")),
        entry("github", json!("never")),
    ]);
    let (s, v) = post(&w, &d.client, &settings(tid), both.clone()).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("tool_label_out_of_scope")),
        "{v}"
    );
    assert!(
        !v["message"].as_str().unwrap().contains("same server"),
        "{v}"
    );
    // The owner, who has no scope, hears the 400.
    let (s, v) = post(&w, &w.gw.client(), &settings(tid), both).await;
    assert_eq!((s, v["code"].as_str()), (400, Some("bad_request")), "{v}");
}

/// A folder default stored before malformed shapes were refused does not
/// block a device's `defaults_patch {profile_id}` — the personality
/// switch, which sends the stored `mcp_tools` back — nor the owner's.
#[tokio::test]
async fn a_stored_malformed_default_does_not_block_an_unrelated_patch() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let (s, f) = post(
        &w,
        &owner,
        "/chat/api/folders",
        json!({ "name": "F", "defaults": { "model_alias": "chatty",
                "mcp_tools": [entry("docs", json!("always"))] } }),
    )
    .await;
    assert_eq!(s, 200, "{f}");
    let fid = f["id"].as_i64().unwrap();
    let legacy = json!([entry("docs", json!({ "always": ["a"] }))]);
    sqlx::query(
        "UPDATE chat_folders SET defaults = json_set(defaults, '$.mcp_tools', json(?2)) \
         WHERE id = ?1",
    )
    .bind(fid)
    .bind(legacy.to_string())
    .execute(&w.state.db)
    .await
    .unwrap();

    let (_, list) = get(&w, &d.client, "/chat/api/profiles").await;
    let list: lmgw_client::requests::ProfileList = serde_json::from_value(list).unwrap();
    let concise = lmgw_client::requests::profile_named(&list, "concise")
        .unwrap()
        .unwrap();
    let path = format!("/chat/api/folders/{fid}");
    for client in [&d.client, &owner] {
        let (s, v) = post(
            &w,
            client,
            &path,
            json!({ "defaults_patch": { "profile_id": concise } }),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(
            v["defaults"]["mcp_tools"][0]["require_approval"],
            json!({ "always": ["a"] }),
            "kept as stored: {v}"
        );
    }
    // A changed entry is checked as ever.
    let (s, v) = post(
        &w,
        &d.client,
        &path,
        json!({ "defaults_patch": { "mcp_tools": [entry("docs", json!({ "never": ["a"] }))] } }),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (400, Some("bad_request")), "{v}");
}
