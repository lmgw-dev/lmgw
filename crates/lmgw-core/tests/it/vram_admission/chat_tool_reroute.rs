//! The Chat tool loop on a request the gate re-routes before anything was
//! sent (review R1 finding 2): a ladder row whose climb has no room hands the
//! model call to its cloud fallback. What goes out there is decided for that
//! route — a continue is refused where there is no prefill, the llama-server
//! continuation fields never reach the cloud, the thread's sampling fields
//! are split for it — and the reply says who answered.

use super::*;

/// An Admin Chat thread on the ladder model, `self_admin` read-only so the
/// loop has tools to offer.
async fn admin_thread(f: &Fixture) -> i64 {
    let s = Settings {
        self_admin: lmgw_core::config::SelfAdmin::ReadOnly,
        ..f.state.snapshot().settings.clone()
    };
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    let r: Value = f
        .gateway
        .client()
        .post(format!("{}/chat/api/threads", f.gateway))
        .json(&json!({"model_alias": LADDER, "kind": "admin"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    r["id"].as_i64().unwrap()
}

async fn post_sse(f: &Fixture, route: &str, body: Value) -> Vec<(String, Value)> {
    let text = f
        .gateway
        .client()
        .post(format!("{}{route}", f.gateway))
        .json(&body)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    crate::chat_actions::sse_events(&text)
}

async fn cloud_bodies(cloud: &MockServer) -> Vec<Value> {
    cloud
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/chat/completions"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// The fallback a climb hands the call to is an OpenAI-compatible cloud API:
/// a continue is refused by name there, and the cloud never sees the
/// llama-server continuation fields.
#[tokio::test]
async fn a_continue_rerouted_to_a_cloud_fallback_is_refused() {
    let f = ladder_fixture(12 * GIB, 0).await;
    f.attribute(6 * GIB);
    let cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;
    let tid = admin_thread(&f).await;
    store::append_chat_message(&f.state.db, tid, "user", "q", "", None, None, None)
        .await
        .unwrap();
    let rid = store::append_chat_message(
        &f.state.db,
        tid,
        "assistant",
        &words("half", 20),
        "",
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let events = post_sse(&f, &format!("/chat/api/threads/{tid}/continue"), json!({})).await;
    let error = events
        .iter()
        .find(|(e, _)| e == "error")
        .unwrap_or_else(|| panic!("no error: {events:?}"));
    let msg = error.1["message"].as_str().unwrap();
    assert!(msg.contains("cannot continue this reply"), "{msg}");
    assert!(msg.contains("OpenAI-compatible"), "{msg}");
    for body in cloud_bodies(&cloud).await {
        assert!(
            body.get("continue_final_message").is_none()
                && body.get("add_generation_prompt").is_none(),
            "llama-server fields reached the cloud: {body}"
        );
    }
    let row = store::get_chat_message(&f.state.db, tid, rid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.content, words("half", 20), "the reply is as it was");
}

/// A fresh turn the climb hands to the cloud: the thread's `top_k` (which
/// llama-server takes and an OpenAI cloud does not) is left out there and
/// named in `reasoning_ignored`, and the saved reply records that the
/// fallback answered.
#[tokio::test]
async fn a_rerouted_tool_turn_is_split_for_the_route_that_answers() {
    let f = ladder_fixture(12 * GIB, 0).await;
    f.attribute(6 * GIB);
    let cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;
    let tid = admin_thread(&f).await;
    let r = f
        .gateway
        .client()
        .post(format!("{}/chat/api/threads/{tid}/settings", f.gateway))
        .json(&json!({"top_k": 40}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let events = post_sse(
        &f,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": words("w", 40)}),
    )
    .await;
    let done = &events.last().unwrap().1;
    assert_eq!(done["aborted"], false, "{events:?}");
    assert_eq!(done["reasoning_ignored"], json!(["top_k"]), "{events:?}");
    let bodies = cloud_bodies(&cloud).await;
    assert!(!bodies.is_empty(), "the cloud answered");
    for body in &bodies {
        assert!(
            body.get("top_k").is_none(),
            "top_k reached the cloud: {body}"
        );
    }
    let msgs = store::list_chat_messages(&f.state.db, tid).await.unwrap();
    let reply = msgs.last().unwrap();
    assert_eq!(reply.role, "assistant");
    assert!(reply.content.contains("from the cloud"), "{reply:?}");
    assert_eq!(reply.model.as_deref(), Some(LADDER));
    assert_eq!(reply.answered_by.as_deref(), Some("cloud-chat"));
}
