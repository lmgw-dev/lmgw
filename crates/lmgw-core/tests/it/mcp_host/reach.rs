//! Who reaches a device-hosted label (§5.6, L16): the owner, the gateway's
//! own runs, the hosting device, and keys, devices and agents whose scope or
//! manifest names the label explicitly — never anonymous, `all`, `deny` or
//! a wildcard alone — on `/mcp`, `/v1/mcp/servers`, `/v1/responses`,
//! realtime and the Chat. An agent's manifest is read by the scope's own
//! unit tests (`mcp::scope`).

use serde_json::{json, Value};

use super::{host_world, linked, mcp_tools, World};
use crate::device_chat::{bearer, chat_thread, get, op, pair, post};
use crate::support::realtime_mcp::tools_session;

/// A client key with `policy`: a client presenting it, and its key.
async fn client_key(w: &World, name: &str, policy: Value) -> (reqwest::Client, String) {
    let mut body = json!({"name": name});
    for (k, v) in policy.as_object().unwrap() {
        body[k] = v.clone();
    }
    let (s, v) = op(w, "key_create", body).await;
    assert_eq!(s, 200, "{v}");
    let key = v["plaintext"].as_str().unwrap().to_string();
    (bearer(&key), key)
}

/// Whether `/v1/mcp/servers` lists `desktop` for `client`.
async fn discovers(w: &World, client: &reqwest::Client) -> bool {
    let (s, v) = get(w, client, "/v1/mcp/servers").await;
    assert_eq!(s, 200, "{v}");
    v["data"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .any(|e| e["server_label"] == "desktop")
}

fn allow(patterns: &str) -> Value {
    json!({"tool_scope_mode": "allow", "tool_scope_patterns": patterns})
}

#[tokio::test]
async fn only_the_owner_the_host_and_explicit_naming_reach_the_label() {
    let (w, d) = host_world().await;
    let _dev = linked(&w, &d, &["echo"]).await;
    let (all, _) = client_key(&w, "ci-all", json!({})).await;
    let (deny, _) = client_key(
        &w,
        "ci-deny",
        json!({"tool_scope_mode": "deny", "tool_scope_patterns": "github__*"}),
    )
    .await;
    let (star, _) = client_key(&w, "ci-star", allow("*")).await;
    let (d_star, _) = client_key(&w, "ci-d", allow("d*")).await;
    let (named, _) = client_key(&w, "ci-named", allow("desktop__*")).await;
    let (one, _) = client_key(&w, "ci-one", allow("desktop__echo")).await;
    let other = pair(&w, "phone", json!({})).await;
    let other_named = pair(&w, "tablet", allow("desktop__*")).await;
    let anonymous = reqwest::Client::new();
    let owner = w.gw.client();

    let cases: Vec<(&str, &reqwest::Client, bool)> = vec![
        ("the owner", &owner, true),
        ("the hosting device", &d.client, true),
        ("anonymous, auth off", &anonymous, false),
        ("an all key", &all, false),
        ("a deny key", &deny, false),
        ("allow *", &star, false),
        ("allow d*", &d_star, false),
        ("allow desktop__*", &named, true),
        ("allow desktop__echo", &one, true),
        ("another device, all", &other.client, false),
        ("another device naming it", &other_named.client, true),
    ];
    for (who, client, reaches) in cases {
        let names = mcp_tools(&w, client).await;
        assert_eq!(
            names.contains(&"desktop__echo".to_string()),
            reaches,
            "/mcp for {who}: {names:?}"
        );
        assert_eq!(
            discovers(&w, client).await,
            reaches,
            "/v1/mcp/servers for {who}"
        );
    }

    // A call is checked again, not trusted from a list.
    let sid = super::mcp_session(&w, &all).await;
    let v = super::mcp_rpc(
        &w,
        &all,
        &sid,
        "tools/call",
        json!({"name": "desktop__echo"}),
    )
    .await;
    assert!(
        v.to_string().contains("a paired device's hosted tools"),
        "{v}"
    );
}

/// `/v1/responses`: an anonymous, cross-origin request — what a page on any
/// site could send through the permissive CORS layer — gets no listing of
/// the label; the owner's request does.
#[tokio::test]
async fn responses_keep_a_browser_page_away() {
    let (w, d) = host_world().await;
    let _dev = linked(&w, &d, &["echo"]).await;
    let body = json!({"model": "chatty", "input": "hi",
                      "tools": [{"type": "mcp", "server_label": "desktop"}]});
    w.chat
        .push(crate::support::realtime_fakes::Turn::text(&["ok"]));
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/responses", w.gw))
        .header("origin", "http://evil.example")
        .json(&body)
        .send()
        .await
        .unwrap();
    let v: Value = resp.json().await.unwrap();
    let listing = v["output"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .find(|i| i["type"] == "mcp_list_tools")
        .cloned()
        .unwrap_or_else(|| panic!("{v}"));
    assert!(
        listing["error"].is_object() || listing["error"].is_string(),
        "{listing}"
    );
    assert!(
        listing["tools"].as_array().is_none_or(Vec::is_empty),
        "{listing}"
    );

    w.chat
        .push(crate::support::realtime_fakes::Turn::text(&["ok"]));
    let v: Value =
        w.gw.client()
            .post(format!("{}/v1/responses", w.gw))
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    let listing = v["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "mcp_list_tools")
        .cloned()
        .unwrap();
    assert_eq!(listing["tools"][0]["name"], "desktop__echo", "{listing}");
}

/// Realtime `mcp` tools: a key that names the label lists it; an `all`
/// key's listing fails as for a label that is not there.
#[tokio::test]
async fn realtime_lists_the_label_only_for_who_names_it() {
    let (w, d) = host_world().await;
    let _dev = linked(&w, &d, &["echo"]).await;
    let (_, named) = client_key(&w, "ci-named", allow("desktop__*")).await;
    let tools = json!([{"type": "mcp", "server_label": "desktop"}]);
    let _ws = tools_session(&w.addr(), Some(&named), tools.clone(), 1).await;

    let (_, all) = client_key(&w, "ci-all", json!({})).await;
    let mut ws = tools_session(&w.addr(), Some(&all), tools, 0).await;
    let events =
        crate::support::realtime_fakes::events_until(&mut ws, "conversation.item.done").await;
    assert!(
        events.iter().any(|e| e["type"] == "mcp_list_tools.failed"),
        "{events:?}"
    );
}

/// The Chat: the hosting device and a device that names the label may
/// attach it; an `all` device may not write another device's label into a
/// thread or a folder default (review W3-9).
#[tokio::test]
async fn an_all_device_may_not_write_another_device_s_label() {
    let (w, d) = host_world().await;
    let _dev = linked(&w, &d, &["echo"]).await;
    let set = json!({"mcp_tools": [{"server_label": "desktop"}]});

    let tid = chat_thread(&w, &d.client, "chatty").await;
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/threads/{tid}/settings"),
        set.clone(),
    )
    .await;
    assert_eq!(s, 200, "the host attaches its own label: {v}");

    let named = pair(&w, "tablet", allow("desktop__*")).await;
    let tid = chat_thread(&w, &named.client, "chatty").await;
    let (s, v) = post(
        &w,
        &named.client,
        &format!("/chat/api/threads/{tid}/settings"),
        set.clone(),
    )
    .await;
    assert_eq!(s, 200, "{v}");

    let phone = pair(&w, "phone", json!({})).await;
    let tid = chat_thread(&w, &phone.client, "chatty").await;
    let (s, v) = post(
        &w,
        &phone.client,
        &format!("/chat/api/threads/{tid}/settings"),
        set.clone(),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("tool_label_out_of_scope")),
        "{v}"
    );
    let (s, v) = post(
        &w,
        &phone.client,
        "/chat/api/folders",
        json!({"name": "mine", "defaults": {"mcp_tools": [{"server_label": "desktop"}]}}),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("tool_label_out_of_scope")),
        "{v}"
    );
}
