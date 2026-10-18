//! An offline device's label (§5.3): resolving it answers at once, without
//! the lazy-list budget's wait, and a turn is never refused for it — it is
//! reported, and the turn runs without that label's tools; with only such
//! labels, as a plain chat turn (R6).

use std::time::{Duration, Instant};

use serde_json::json;

use super::{host_world, mcp_rpc, mcp_session};
use crate::device_chat::{chat_thread, pair, post, sse};
use crate::support::realtime_fakes::Turn;

#[tokio::test]
async fn an_offline_label_fails_at_once_and_the_turn_still_answers() {
    let (w, _d) = host_world().await;
    // From the phone, as the desktop is off: its policy grants the
    // label, so it may attach it.
    let phone = pair(&w, "phone", json!({"device_tools": ["desktop"]})).await;
    let tid = chat_thread(&w, &phone.client, "chatty").await;
    let (s, v) = post(
        &w,
        &phone.client,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "desktop"}]}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    w.chat.push(Turn::text(&["Here", " anyway."]));
    let began = Instant::now();
    let (s, said) = sse(
        &w,
        &phone.client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "what is on my screen?"}),
    )
    .await;
    assert_eq!(s, 200);
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "no lazy-list wait: {:?}",
        began.elapsed()
    );
    let error = said
        .iter()
        .find(|(e, _)| e == "error")
        .unwrap_or_else(|| panic!("the label is reported: {said:?}"));
    let message = error.1["message"].as_str().unwrap();
    assert!(
        message.starts_with("MCP server 'desktop': device 'desktop' is offline"),
        "{message}"
    );
    assert_eq!(
        (&error.1["code"], &error.1["server_label"]),
        (&json!("tool_offline"), &json!("desktop")),
        "{said:?}"
    );
    let done = said.iter().find(|(e, _)| e == "done").unwrap();
    assert_ne!(done.1["aborted"], true, "{said:?}");
    assert!(
        said.iter()
            .any(|(e, d)| e == "delta" && d["text"] == " anyway."),
        "{said:?}"
    );

    // A label that fails for another reason still refuses the turn.
    let tid = chat_thread(&w, &w.gw.client(), "chatty").await;
    let (s, v) = post(
        &w,
        &w.gw.client(),
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "desktop"}, {"server_label": "nosuch"}]}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (_, said) = sse(
        &w,
        &w.gw.client(),
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi"}),
    )
    .await;
    let done = said.iter().find(|(e, _)| e == "done").unwrap();
    assert_eq!(done.1["aborted"], true, "{said:?}");
}

/// A voice session bound to such a thread: the label's frame is relayed as
/// every frame is, and the response completes as the plain turn — the
/// frame is no response error (client-apps design §5.3, 2026-10-15).
#[tokio::test]
async fn a_bound_session_answers_past_an_offline_label() {
    use crate::realtime_chat_thread::{next, of_type, say, until_type};
    use crate::support::realtime_fakes::send as ws_send;

    let (w, _d) = host_world().await;
    let phone = pair(&w, "phone", json!({"device_tools": ["desktop"]})).await;
    let tid = chat_thread(&w, &phone.client, "chatty").await;
    let (s, v) = post(
        &w,
        &phone.client,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "desktop"}]}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let mut ws = w
        .connect_as(&phone, &format!("chat_thread={tid}"))
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut ws).await["type"], "session.created");
    ws_send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
            "output_modalities": ["text"], "audio": {"input": {"turn_detection": null}}}}),
    )
    .await;
    assert_eq!(next(&mut ws).await["type"], "session.updated");
    w.chat.push(Turn::text(&["Here", " anyway."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "response.done").await;
    let done = of_type(&events, "response.done")[0];
    assert_eq!(done["response"]["status"], "completed", "{done}");
    assert!(
        of_type(&events, "lmgw.chat.frame")
            .iter()
            .any(|e| e["event"] == "error" && e["data"]["code"] == "tool_offline"),
        "{events:#?}"
    );
}

/// The counterpart: a label the device's scope keeps out whole is no absent
/// label. Its frame is relayed, then the turn refuses, with no tool left —
/// and that error still fails the bound response.
#[tokio::test]
async fn a_bound_session_fails_when_the_turn_is_refused_past_a_label_frame() {
    use crate::realtime_chat_thread::{next, of_type, say, until_type};
    use crate::support::realtime_fakes::send as ws_send;

    let (w, d) = host_world().await;
    let _dev = super::linked(&w, &d, &["echo"]).await;
    let phone = pair(
        &w,
        "phone",
        json!({"device_tools": ["desktop"], "tool_scope_mode": "deny",
               "tool_scope_patterns": "desktop__*"}),
    )
    .await;
    let tid = chat_thread(&w, &phone.client, "chatty").await;
    let owner = w.gw.client();
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "desktop"}]}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let mut ws = w
        .connect_as(&phone, &format!("chat_thread={tid}"))
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut ws).await["type"], "session.created");
    ws_send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
            "output_modalities": ["text"], "audio": {"input": {"turn_detection": null}}}}),
    )
    .await;
    assert_eq!(next(&mut ws).await["type"], "session.updated");
    say(&mut ws).await;
    let events = until_type(&mut ws, "response.done").await;
    let done = of_type(&events, "response.done")[0];
    assert_eq!(done["response"]["status"], "failed", "{done}");
    assert!(
        of_type(&events, "lmgw.chat.frame")
            .iter()
            .any(|e| e["event"] == "error" && e["data"]["code"] == "tool_unreachable"),
        "{events:#?}"
    );
}

/// A call of an offline device's tool on `/mcp` says it is offline.
#[tokio::test]
async fn a_call_to_an_offline_device_says_so() {
    let (w, _d) = host_world().await;
    let owner = w.gw.client();
    let sid = mcp_session(&w, &owner).await;
    let began = Instant::now();
    let v = mcp_rpc(
        &w,
        &owner,
        &sid,
        "tools/call",
        json!({"name": "desktop__echo"}),
    )
    .await;
    assert!(began.elapsed() < Duration::from_secs(5));
    assert!(
        v.to_string()
            .contains("device 'desktop' is offline (never seen)"),
        "{v}"
    );
}
