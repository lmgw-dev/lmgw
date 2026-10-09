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
    // From the phone, as the owner's desktop is off: its scope names the
    // label, so it may attach it.
    let phone = pair(
        &w,
        "phone",
        json!({"tool_scope_mode": "allow", "tool_scope_patterns": "desktop__*"}),
    )
    .await;
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
        message.starts_with("MCP server 'desktop': device 'desktop' is not connected"),
        "{message}"
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
            .contains("device 'desktop' is not connected (never seen)"),
        "{v}"
    );
}
