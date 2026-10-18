//! Who reaches a device-hosted label (§5.6, L16 as changed 2026-10-15): the
//! owner, the gateway's own runs, the hosting device, keys and devices whose
//! device-tools grant names the label (their tool scope filtering its tools
//! as any server's), and agents whose manifest names it — never anonymous,
//! and never a key through its tool scope alone — on `/mcp`,
//! `/v1/mcp/servers`, `/v1/responses`, realtime and the Chat. An agent's
//! manifest is read by the scope's own unit tests (`mcp::scope`).

use serde_json::{json, Value};

use super::{host_world, linked, mcp_tools, World};
use crate::chat_ongoing::{current_ok, ongoing_folder};
use crate::device_chat::{bearer, chat_thread, get, op, pair, post, sse};
use crate::support::realtime_fakes::Turn;
use crate::support::realtime_mcp::{calls, tools_session};

/// A client key with `policy`: a client presenting it, and its key.
async fn client_key(w: &World, name: &str, policy: Value) -> (reqwest::Client, String) {
    let mut body = json!({"name": name});
    for (k, v) in policy.as_object().unwrap() {
        body[k] = v.clone();
    }
    let (s, v) = op(w, "key_create", body).await;
    assert_eq!(s, 200, "{v}");
    let key = v["key"].as_str().unwrap().to_string();
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

fn deny(patterns: &str) -> Value {
    json!({"tool_scope_mode": "deny", "tool_scope_patterns": patterns})
}

/// `policy` with the device-tools grant `desktop`.
fn granted(mut policy: Value) -> Value {
    policy["device_tools"] = json!(["desktop"]);
    policy
}

#[tokio::test]
async fn the_grant_decides_reach_and_the_scope_filters_the_tools() {
    let (w, d) = host_world().await;
    let _dev = linked(&w, &d, &["echo"]).await;
    let (all, _) = client_key(&w, "ci-all", json!({})).await;
    let (all_g, _) = client_key(&w, "ci-all-g", granted(json!({}))).await;
    let (deny_k, _) = client_key(&w, "ci-deny", deny("github__*")).await;
    let (deny_g, _) = client_key(&w, "ci-deny-g", granted(deny("github__*"))).await;
    let (deny_out, _) = client_key(&w, "ci-deny-out", granted(deny("desktop__*"))).await;
    let (star, _) = client_key(&w, "ci-star", allow("*")).await;
    let (star_g, _) = client_key(&w, "ci-star-g", granted(allow("*"))).await;
    let (named, _) = client_key(&w, "ci-named", allow("desktop__*")).await;
    let (one_g, _) = client_key(&w, "ci-one-g", granted(allow("desktop__echo"))).await;
    let (docs_g, _) = client_key(&w, "ci-docs-g", granted(allow("docs__*"))).await;
    let other = pair(&w, "phone", json!({})).await;
    let other_g = pair(&w, "tablet", granted(json!({}))).await;
    let anonymous = reqwest::Client::new();
    let owner = w.gw.client();

    let cases: Vec<(&str, &reqwest::Client, bool)> = vec![
        ("the owner", &owner, true),
        ("the hosting device", &d.client, true),
        ("anonymous, auth off", &anonymous, false),
        ("an all key", &all, false),
        ("an all key, granted", &all_g, true),
        ("a deny key", &deny_k, false),
        ("a deny key, granted", &deny_g, true),
        ("a deny key denying the label, granted", &deny_out, false),
        ("allow *", &star, false),
        ("allow *, granted", &star_g, true),
        ("allow desktop__* without the grant", &named, false),
        ("allow desktop__echo, granted", &one_g, true),
        ("allow docs__*, granted", &docs_g, false),
        ("another device, all", &other.client, false),
        ("another device, all, granted", &other_g.client, true),
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
    for client in [&all, &named] {
        let sid = super::mcp_session(&w, client).await;
        let v = super::mcp_rpc(
            &w,
            client,
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

    // The card lists who reaches the label: the grants, not the patterns.
    let (s, v) = get(&w, &owner, "/api/usage/keys").await;
    assert_eq!(s, 200, "{v}");
    let row = v["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == d.id)
        .cloned()
        .unwrap();
    let reaches: Vec<&str> = row["reaches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for who in ["owner", "key 'ci-all-g'", "device 'tablet'"] {
        assert!(reaches.contains(&who), "{who}: {reaches:?}");
    }
    assert!(!reaches.contains(&"key 'ci-named'"), "{reaches:?}");
    assert!(
        !reaches.contains(&"key 'ci-docs-g'"),
        "a grant whose scope admits none of the label's tools reaches nothing: {reaches:?}"
    );
    let granted_row = v["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["name"] == "ci-all-g")
        .cloned()
        .unwrap();
    assert_eq!(granted_row["device_tools"], json!(["desktop"]));
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
    w.chat.push(Turn::text(&["ok"]));
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
    // Answered as a label that is not there: a caller-supplied spec learns
    // nothing by probing.
    assert!(
        listing["error"]
            .to_string()
            .contains("no MCP server with label 'desktop'"),
        "{listing}"
    );

    w.chat.push(Turn::text(&["ok"]));
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

/// Realtime `mcp` tools: a key granted the label lists it; an `all` key's
/// listing fails as for a label that is not there.
#[tokio::test]
async fn realtime_lists_the_label_only_for_who_is_granted_it() {
    let (w, d) = host_world().await;
    let _dev = linked(&w, &d, &["echo"]).await;
    let (_, named) = client_key(&w, "ci-granted", granted(json!({}))).await;
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

/// The Chat: the hosting device and a device granted the label may attach
/// it; an `all` device may not write another device's label into a thread
/// or a folder default (review W3-9), and is told the grant is what it
/// lacks.
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

    let named = pair(&w, "tablet", granted(json!({}))).await;
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
    assert_eq!(
        v["message"],
        "'desktop': this device may not use the tools of a paired device's hosted label: grant \
         it under the device's policy (Device tools)",
        "the writer chose the label: no host named: {v}"
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

/// The Kai-folder case: an ongoing folder whose defaults attach a desktop's
/// hosted label. A phone with an `all` scope and the grant sends in the
/// folder's current thread through the chat send route, and the model is
/// offered the desktop's tool and calls it on the desktop. A tablet without
/// the grant hears why in a `tool_unreachable` frame naming the label, and
/// its turn answers as a plain chat turn, as with an offline desktop.
#[tokio::test]
async fn a_folder_s_desktop_label_resolves_for_a_granted_phone_and_says_why_otherwise() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(
        &w,
        &owner,
        "Kai",
        0,
        json!({"mcp_tools": [{"server_label": "desktop"}]}),
    )
    .await;

    let phone = pair(&w, "phone", granted(json!({}))).await;
    let (tid, _, _) = current_ok(&w, &phone.client, folder, false).await;
    w.chat
        .push(calls(&[(0, "c1", "desktop__echo", "{}")], "tool_calls"));
    w.chat.push(Turn::text(&["done"]));
    let (s, said) = sse(
        &w,
        &phone.client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "what is on my desktop?"}),
    )
    .await;
    assert_eq!(s, 200);
    assert!(!said.iter().any(|(e, _)| e == "error"), "{said:?}");
    let call = dev.next_call().await;
    assert_eq!(call["params"]["name"], "echo", "{call}");
    let offered = w.chat.seen.chat(0)["tools"].to_string();
    assert!(offered.contains("desktop__echo"), "{offered}");

    let tablet = pair(&w, "tablet", json!({})).await;
    let (tid, _, _) = current_ok(&w, &tablet.client, folder, false).await;
    let before = w.chat.seen.chat_count();
    w.chat.push(Turn::text(&["Plainly", " then."]));
    let (s, said) = sse(
        &w,
        &tablet.client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "and now?"}),
    )
    .await;
    assert_eq!(s, 200);
    let error = said
        .iter()
        .find(|(e, _)| e == "error")
        .unwrap_or_else(|| panic!("the label is reported: {said:?}"));
    assert_eq!(
        (&error.1["code"], &error.1["server_label"]),
        (&json!("tool_unreachable"), &json!("desktop")),
        "{said:?}"
    );
    assert_eq!(
        error.1["message"],
        "MCP server 'desktop': this device may not use the tools of device 'desktop' (label \
         desktop): grant it under the device's policy (Device tools)",
        "{said:?}"
    );
    let done = said.iter().find(|(e, _)| e == "done").unwrap();
    assert_ne!(done.1["aborted"], true, "{said:?}");
    assert!(
        said.iter()
            .any(|(e, d)| e == "delta" && d["text"] == " then."),
        "{said:?}"
    );
    assert!(
        w.chat.seen.chat(before)["tools"].is_null(),
        "a plain chat turn: {}",
        w.chat.seen.chat(before)
    );
}

/// A `/v1/responses` run asking for `label` as `client`: its listing item.
async fn responses_listing(w: &World, client: &reqwest::Client, label: &str) -> Value {
    w.chat.push(Turn::text(&["ok"]));
    let v: Value = client
        .post(format!("{}/v1/responses", w.gw))
        .json(&json!({"model": "chatty", "input": "hi",
                      "tools": [{"type": "mcp", "server_label": label}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["output"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .find(|i| i["type"] == "mcp_list_tools")
        .cloned()
        .unwrap_or_else(|| panic!("{v}"))
}

/// `/v1/responses`: a key without the grant, `all` or `allow desktop__*`,
/// hears exactly what it hears for a label nothing registers; a granted
/// key lists the tools.
#[tokio::test]
async fn responses_answer_an_ungranted_key_as_for_an_unknown_label() {
    let (w, d) = host_world().await;
    let _dev = linked(&w, &d, &["echo"]).await;
    let (all, _) = client_key(&w, "ci-all", json!({})).await;
    let (named, _) = client_key(&w, "ci-named", allow("desktop__*")).await;
    let (granted_key, _) = client_key(&w, "ci-granted", granted(allow("desktop__*"))).await;

    for (who, client) in [("an all key", &all), ("allow desktop__*", &named)] {
        let desktop = responses_listing(&w, client, "desktop").await;
        let ghost = responses_listing(&w, client, "ghost").await;
        let text = desktop["error"]
            .as_str()
            .unwrap_or_else(|| panic!("{desktop}"));
        assert!(
            text.contains("no MCP server with label 'desktop' is registered"),
            "{who}: {desktop}"
        );
        assert_eq!(
            text,
            ghost["error"].as_str().unwrap().replace("ghost", "desktop"),
            "{who}: the same words as for a label that is not there"
        );
        assert!(
            desktop["tools"].as_array().is_none_or(Vec::is_empty),
            "{who}: {desktop}"
        );
    }
    let listing = responses_listing(&w, &granted_key, "desktop").await;
    assert_eq!(listing["tools"][0]["name"], "desktop__echo", "{listing}");
}

/// The realtime listing: a key without the grant, `all` or
/// `allow desktop__*`, gets the failure it gets for an unknown label.
#[tokio::test]
async fn realtime_answers_an_ungranted_key_as_for_an_unknown_label() {
    let (w, d) = host_world().await;
    let _dev = linked(&w, &d, &["echo"]).await;
    let (_, all) = client_key(&w, "ci-all", json!({})).await;
    let (_, named) = client_key(&w, "ci-named", allow("desktop__*")).await;

    for (who, key) in [("an all key", &all), ("allow desktop__*", &named)] {
        let mut said = Vec::new();
        for label in ["desktop", "ghost"] {
            let tools = json!([{"type": "mcp", "server_label": label}]);
            let mut ws = tools_session(&w.addr(), Some(key), tools, 0).await;
            let events =
                crate::support::realtime_fakes::events_until(&mut ws, "conversation.item.done")
                    .await;
            assert!(
                events.iter().any(|e| e["type"] == "mcp_list_tools.failed"),
                "{who}: {events:?}"
            );
            let err = crate::support::realtime_fakes::next_event(&mut ws).await;
            assert_eq!(err["type"], "error", "{who}: {err}");
            said.push(
                err["error"]["message"]
                    .as_str()
                    .unwrap()
                    .replace(label, "LABEL"),
            );
        }
        assert!(
            said[0].contains("no MCP server with label"),
            "{who}: {said:?}"
        );
        assert_eq!(
            said[0], said[1],
            "{who}: the same words as for an unknown label"
        );
    }
}

/// `GET /v1/mcp/servers/{label}`: not found for a key without the grant,
/// whatever its scope; the granted key and the owner read it.
#[tokio::test]
async fn a_label_s_detail_is_not_found_for_an_ungranted_key() {
    let (w, d) = host_world().await;
    let _dev = linked(&w, &d, &["echo"]).await;
    let (all, _) = client_key(&w, "ci-all", json!({})).await;
    let (named, _) = client_key(&w, "ci-named", allow("desktop__*")).await;
    let (granted_key, _) = client_key(&w, "ci-granted", granted(json!({}))).await;
    for client in [&all, &named] {
        let (s, v) = get(&w, client, "/v1/mcp/servers/desktop").await;
        assert_eq!(s, 404, "{v}");
    }
    let (s, v) = get(&w, &granted_key, "/v1/mcp/servers/desktop").await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = get(&w, &w.gw.client(), "/v1/mcp/servers/desktop").await;
    assert_eq!(s, 200, "{v}");
}
