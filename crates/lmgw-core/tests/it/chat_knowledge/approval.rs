//! Tool mode attaches the `kb` toolset once, in place of a `kb` entry the
//! thread carries by hand — and keeps that entry's `require_approval`
//! (client-apps design §6.6): turning tool mode on never ungates the
//! owner's `kb: always`, nor does a device dropping the entry or spelling
//! its label with a space in the same write.

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::{kbfix, tool_call_sse, world, World};

/// The next model call asks for `kb__search`.
async fn search_call(w: &World) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    tool_call_sse("c1", "kb__search", json!({"query": "rent"})),
                    "text/event-stream",
                ),
        )
        .up_to_n_times(1)
        .mount(&w.chat)
        .await;
}

/// A turn of `tid`'s that calls `kb__search` waits for an approval, and
/// nothing runs.
async fn search_waits(w: &World, tid: i64) {
    search_call(w).await;
    let events = w.send(tid, json!({"content": "what is the rent?"})).await;
    let asks: Vec<_> = events
        .iter()
        .filter(|(e, d)| e == "tool" && d["event"] == "approval")
        .map(|(_, d)| d)
        .collect();
    assert_eq!(
        asks.len(),
        1,
        "the search waits for an approval: {events:?}"
    );
    assert_eq!(asks[0]["server_label"], "kb", "{}", asks[0]);
    assert!(
        !events
            .iter()
            .any(|(e, d)| e == "tool" && d["event"] == "result"),
        "nothing ran: {events:?}"
    );
}

/// A device paired on `w`, unscoped: a client presenting its key.
async fn device(w: &World) -> reqwest::Client {
    let r =
        w.gw.client()
            .post(format!("{}/api/op/key_create", w.gw))
            .json(&json!({ "kind": "device", "name": "phone" }))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    crate::device_chat::bearer(v["key"].as_str().unwrap())
}

/// `client`'s settings write to `tid`.
async fn settings_as(w: &World, client: &reqwest::Client, tid: i64, body: Value) -> (u16, Value) {
    let r = client
        .post(format!("{}/chat/api/threads/{tid}/settings", w.gw))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn tool_mode_keeps_the_thread_s_kb_approval() {
    let w = world().await;
    let taxes = w.base("Taxes", ("notes.md", kbfix::NOTES)).await;
    let tid = w.thread(json!({})).await;
    let (status, v) = w
        .settings(
            tid,
            json!({"mcp_tools": [{"server_label": "kb", "require_approval": "always"}],
                   "kb_ids": [taxes], "kb_mode": "tool"}),
        )
        .await;
    assert_eq!(status, 200, "{v}");
    search_waits(&w, tid).await;
    kbfix::cleanup(&w.state);
}

/// A device that drops the owner's `kb: always` entry and turns tool mode
/// on in one write: the owner's floor for `kb` still gates the toolset
/// tool mode attaches (review finding 1A).
#[tokio::test]
async fn a_device_dropping_the_kb_entry_for_tool_mode_is_still_gated() {
    let w = world().await;
    let taxes = w.base("Taxes", ("notes.md", kbfix::NOTES)).await;
    let tid = w.thread(json!({})).await;
    let (status, v) = w
        .settings(
            tid,
            json!({"mcp_tools": [{"server_label": "kb", "require_approval": "always"}]}),
        )
        .await;
    assert_eq!(status, 200, "{v}");
    let phone = device(&w).await;
    let (status, v) = settings_as(
        &w,
        &phone,
        tid,
        json!({"mcp_tools": [], "kb_ids": [taxes], "kb_mode": "tool"}),
    )
    .await;
    assert_eq!(status, 200, "a removal is no loosening: {v}");
    assert_eq!(w.thread_json(tid).await["thread"]["mcp_tools"], json!([]));
    search_waits(&w, tid).await;
    kbfix::cleanup(&w.state);
}

/// A device that rewrites `kb` as ` kb` with tool mode: the label is the
/// same toolset, stored trimmed, and its rule still gates the search
/// (review finding 1B).
#[tokio::test]
async fn a_spaced_kb_label_is_the_kb_toolset_and_stays_gated() {
    let w = world().await;
    let taxes = w.base("Taxes", ("notes.md", kbfix::NOTES)).await;
    let tid = w.thread(json!({})).await;
    let (status, v) = w
        .settings(
            tid,
            json!({"mcp_tools": [{"server_label": "kb", "require_approval": "always"}]}),
        )
        .await;
    assert_eq!(status, 200, "{v}");
    let phone = device(&w).await;
    let spaced = |ra: &str| {
        json!({"mcp_tools": [{"server_label": " kb", "require_approval": ra}],
               "kb_ids": [taxes], "kb_mode": "tool"})
    };
    let (status, v) = settings_as(&w, &phone, tid, spaced("never")).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (403, Some("approval_loosen_refused")),
        "{v}"
    );
    let (status, v) = settings_as(&w, &phone, tid, spaced("always")).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        w.thread_json(tid).await["thread"]["mcp_tools"][0]["server_label"],
        "kb",
        "stored trimmed"
    );
    search_waits(&w, tid).await;
    kbfix::cleanup(&w.state);
}
