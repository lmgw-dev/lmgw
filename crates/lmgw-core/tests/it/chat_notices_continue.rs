//! A reply's notices across a continue: the MCP labels that offered a turn
//! no tools are kept with the reply (`chat_messages.notices`), and a
//! continue adds its own to them, one per label and code, the latest
//! sentence kept (`LabelNotice::merge_into`). The plain send is
//! `chat_tab::a_label_that_offered_no_tools_is_kept_with_the_reply`.

use lmgw_core::config::{Protocol, UpstreamKind};
use serde_json::{json, Value};
use wiremock::MockServer;

use crate::chat_actions::{gateway, get_json, mount_openai_reply, post};
use crate::chat_tab::{mcp_stub, patch_settings, register_mcp};

/// The `(server_label, code, message)` of the thread's reply's notices.
async fn reply_notices(gw: &crate::common::Gw, tid: i64) -> Vec<(String, String, String)> {
    let t = get_json(gw, &format!("/chat/api/threads/{tid}")).await;
    let reply = t["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap_or_else(|| panic!("a saved reply: {t:#}"))
        .clone();
    reply["notices"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|n| {
            let s = |k: &str| n[k].as_str().unwrap_or_default().to_string();
            (s("server_label"), s("code"), s("message"))
        })
        .collect()
}

#[tokio::test]
async fn a_continue_merges_its_notices_into_the_replys_by_label_and_code() {
    let mock = MockServer::start().await;
    mount_openai_reply(&mock, "an answer", 5, 2).await;
    let (state, gw) = gateway(&mock, UpstreamKind::LlamaServer, Protocol::LlamaCpp).await;
    let stub = mcp_stub().await;
    register_mcp(&state, "stub", &stub).await;
    let tid = post(&gw, "/chat/api/threads", json!({"model_alias": "m"}))
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    patch_settings(
        &gw,
        tid,
        json!({"mcp_tools": [{"server_label": "ghost"}, {"server_label": "stub"}]}),
    )
    .await;
    let body = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hello"}),
    )
    .await
    .text()
    .await
    .unwrap();
    assert!(body.contains("event: done"), "{body}");
    let sent = reply_notices(&gw, tid).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    let (label, code, sentence) = sent[0].clone();
    assert_eq!((label.as_str(), code.as_str()), ("ghost", "tool_unknown"));

    // The stored sentence is an older wording of the same label and code (a
    // gateway that said it differently); a second label is attached for the
    // continue.
    let rid: i64 = sqlx::query_scalar(
        "SELECT id FROM chat_messages WHERE thread_id = ?1 AND role = 'assistant'",
    )
    .bind(tid)
    .fetch_one(&state.db)
    .await
    .unwrap();
    sqlx::query("UPDATE chat_messages SET notices = ?2 WHERE id = ?1")
        .bind(rid)
        .bind(
            json!([{"server_label": "ghost", "code": "tool_unknown",
                    "message": "an older sentence about ghost"}])
            .to_string(),
        )
        .execute(&state.db)
        .await
        .unwrap();
    patch_settings(
        &gw,
        tid,
        json!({"mcp_tools": [{"server_label": "ghost"}, {"server_label": "stub"},
                             {"server_label": "phantom"}]}),
    )
    .await;

    let body = post(&gw, &format!("/chat/api/threads/{tid}/continue"), json!({}))
        .await
        .text()
        .await
        .unwrap();
    assert!(body.contains("event: done"), "{body}");
    assert!(body.contains("\"server_label\":\"phantom\""), "{body}");
    let after = reply_notices(&gw, tid).await;
    assert_eq!(
        after,
        vec![
            ("ghost".into(), "tool_unknown".into(), sentence),
            (
                "phantom".into(),
                "tool_unknown".into(),
                after
                    .get(1)
                    .map(|n| n.2.clone())
                    .filter(|m| m.contains("phantom"))
                    .unwrap_or_else(|| panic!("phantom's notice second: {after:?}")),
            ),
        ],
        "ghost once, in its place, with the latest sentence; phantom after it"
    );
}
