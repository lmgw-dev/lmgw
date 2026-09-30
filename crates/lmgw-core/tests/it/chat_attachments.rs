//! Chat file attachments (chat-archive-pin-attachments design §2): kind
//! sniffing, the draft/bind/delete lifecycle, the `max_body_mb` ceiling, how
//! an attachment renders into the upstream request (order, vision gating),
//! and the `GET .../attachments/{id}` byte-serving headers.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common;
use common::{serve, Gw};

/// PNG magic + a few bytes, valid enough for [`lmgw_core`]'s sniffer (it only
/// looks at the header, never decodes the image).
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest-of-file";
const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0];
const GIF: &[u8] = b"GIF89arest";
fn webp() -> Vec<u8> {
    let mut v = b"RIFF".to_vec();
    v.extend_from_slice(&[0, 0, 0, 0]);
    v.extend_from_slice(b"WEBPrest");
    v
}

/// A gateway with a `"vision"` alias (`capabilities_override` says
/// `vision: true`), a `"no-vision"` alias (`vision: false`) and a plain
/// `"my-model"` alias (no override — base capabilities are unreadable for a
/// bare Generic test upstream, so its `vision` reads as unknown/`None`), all
/// three routed at one wiremock upstream. A Generic (non-local, non-catalog)
/// upstream's own derived capabilities are always `None`, so an override that
/// touches `capabilities` at all has to spell out `task`/`endpoints`/`source`
/// too — there is nothing else to merge onto (capabilities design §7).
async fn setup(upstream_base: &str) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: upstream_base.trim_end_matches('/').to_string(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    let alias = |name: &str, vision: Option<bool>| NewAlias {
        alias: name.into(),
        upstream_id: up_id,
        upstream_model_id: "tgt-model".into(),
        param_overrides: Default::default(),
        enabled: true,
        capabilities_override: vision.map(|v| {
            json!({
                "capabilities": {
                    "task": "chat",
                    "endpoints": ["/v1/chat/completions"],
                    "source": "owner",
                    "vision": v,
                }
            })
        }),
    };
    store::insert_alias(&state.db, &alias("my-model", None))
        .await
        .unwrap();
    store::insert_alias(&state.db, &alias("vision", Some(true)))
        .await
        .unwrap();
    store::insert_alias(&state.db, &alias("no-vision", Some(false)))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

async fn new_thread(base: &Gw, model_alias: &str) -> i64 {
    base.client()
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": model_alias }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap()
}

/// Only the `/chat/completions` calls — `model_vision`'s capability lookup
/// (`capabilities::exposed::exposed_entry`, a single-model resolution) probes
/// its one effective alias's upstream catalog (`GET /models`), which lands on
/// this same wiremock server as harmless collateral; the assertions below
/// care about the chat turns, not the probe.
async fn chat_completion_requests(mock: &MockServer) -> Vec<wiremock::Request> {
    mock.received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/chat/completions")
        .collect()
}

async fn upload(base: &Gw, tid: i64, name: &str, bytes: &[u8]) -> reqwest::Response {
    base.client()
        .post(format!(
            "{base}/chat/api/threads/{tid}/attachments?name={name}"
        ))
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Kind detection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sniffs_every_supported_image_format() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "my-model").await;

    for (name, bytes, mime) in [
        ("a.png", PNG.to_vec(), "image/png"),
        ("a.jpg", JPEG.to_vec(), "image/jpeg"),
        ("a.gif", GIF.to_vec(), "image/gif"),
        ("a.webp", webp(), "image/webp"),
    ] {
        let resp = upload(&base, tid, name, &bytes).await;
        assert_eq!(resp.status(), 200, "{name}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["kind"], "image", "{name}: {body:#}");
        assert_eq!(body["mime"], mime, "{name}: {body:#}");
    }
}

#[tokio::test]
async fn valid_utf8_is_text_and_binary_garbage_is_415() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "my-model").await;

    let resp = upload(&base, tid, "notes.txt", b"hello, world\n").await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["kind"], "text");
    assert_eq!(body["mime"], "text/plain; charset=utf-8");

    let resp = upload(&base, tid, "junk.bin", &[0x00, 0xff, 0x01, 0xfe, 0x00]).await;
    assert_eq!(resp.status(), 415);
    let body: Value = resp.json().await.unwrap();
    // Flat `{code, message}` (review finding 2) — no `"error"` wrapper, unlike
    // `/v1`'s OpenAI/Anthropic-shaped bodies.
    assert_eq!(body["code"], "unsupported_attachment", "{body:#}");
}

#[tokio::test]
async fn an_empty_upload_is_refused_rather_than_becoming_an_empty_text_attachment() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "my-model").await;

    let resp = upload(&base, tid, "empty.txt", b"").await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "empty_attachment", "{body:#}");
}

// ---------------------------------------------------------------------------
// Size ceiling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_over_max_body_mb_is_413_naming_the_setting() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "my-model").await;

    let mut settings = state.snapshot().settings.clone();
    settings.max_body_mb = 1;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let big = vec![b'x'; 2 * 1024 * 1024];
    let resp = upload(&base, tid, "big.txt", &big).await;
    assert_eq!(resp.status(), 413);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "body_limit", "{body:#}");
    let msg = body["message"].as_str().unwrap();
    assert!(msg.contains("max_body_mb"), "{msg}");
}

/// A chunked (streamed) body declares no `Content-Length`, so the early
/// declared-size check cannot catch it — this is the case that used to slip
/// past `body_limit_mw` and surface as axum's own plain-text 413 instead of
/// this route's named one (review finding 8). `reqwest::Body::wrap_stream`
/// is what makes reqwest send the request chunked rather than buffering it
/// and setting `Content-Length` itself.
#[tokio::test]
async fn a_chunked_upload_over_max_body_mb_still_gets_the_named_413() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "my-model").await;

    let mut settings = state.snapshot().settings.clone();
    settings.max_body_mb = 1;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let big = vec![b'x'; 2 * 1024 * 1024];
    let stream = futures::stream::once(async move { Ok::<_, std::io::Error>(big) });
    let resp = base
        .client()
        .post(format!(
            "{base}/chat/api/threads/{tid}/attachments?name=big.txt"
        ))
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413, "chunked bodies must be bounded too");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "body_limit", "{body:#}");
    let msg = body["message"].as_str().unwrap();
    assert!(msg.contains("max_body_mb"), "{msg}");
}

// ---------------------------------------------------------------------------
// Draft delete / 409 once sent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_draft_can_be_deleted_but_not_once_sent() {
    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "my-model").await;
    let client = base.client();

    let att: Value = upload(&base, tid, "notes.txt", b"draft one")
        .await
        .json()
        .await
        .unwrap();
    let aid = att["id"].as_i64().unwrap();

    let del: Value = client
        .post(format!("{base}/chat/api/attachments/{aid}/delete"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(del["ok"], true);
    // Deleting an already-deleted (now missing) draft is a 404, not a 409.
    let resp = client
        .post(format!("{base}/chat/api/attachments/{aid}/delete"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Upload, send (binds it), then try to delete the now-sent attachment.
    let att: Value = upload(&base, tid, "notes2.txt", b"draft two")
        .await
        .json()
        .await
        .unwrap();
    let aid2 = att["id"].as_i64().unwrap();
    let _ = client
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "see attached", "attachments": [aid2] }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let resp = client
        .post(format!("{base}/chat/api/attachments/{aid2}/delete"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "attachment_sent", "{body:#}");
}

// ---------------------------------------------------------------------------
// send binds drafts; rejects foreign/already-sent ids
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_rejects_an_attachment_id_from_another_thread() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid_a = new_thread(&base, "my-model").await;
    let tid_b = new_thread(&base, "my-model").await;
    let att: Value = upload(&base, tid_a, "mine.txt", b"only thread A's")
        .await
        .json()
        .await
        .unwrap();
    let aid = att["id"].as_i64().unwrap();

    let resp = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid_b}/send"))
        .json(&json!({ "content": "steal it", "attachments": [aid] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // The foreign thread must not have consumed it — thread A can still send it.
    let resp = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid_a}/send"))
        .json(&json!({ "content": "mine for real", "attachments": [aid] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn send_rejects_an_attachment_id_that_was_already_sent() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "my-model").await;
    let att: Value = upload(&base, tid, "once.txt", b"one turn only")
        .await
        .json()
        .await
        .unwrap();
    let aid = att["id"].as_i64().unwrap();

    let resp = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "first", "attachments": [aid] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "again", "attachments": [aid] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        400,
        "an id already bound to a message is no longer a draft"
    );
}

// ---------------------------------------------------------------------------
// Rendering into the upstream request: order, and replay on a second turn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_upstream_body_carries_attachments_in_upload_order_before_the_typed_text() {
    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "vision").await;

    // Text uploaded first, image second — upload order.
    let text_att: Value = upload(&base, tid, "readme.txt", b"the file contents")
        .await
        .json()
        .await
        .unwrap();
    let img_att: Value = upload(&base, tid, "shot.png", PNG)
        .await
        .json()
        .await
        .unwrap();
    let text_id = text_att["id"].as_i64().unwrap();
    let img_id = img_att["id"].as_i64().unwrap();

    let _ = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "what do you make of these?", "attachments": [text_id, img_id] }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let reqs = chat_completion_requests(&mock).await;
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let parts = sent["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_array()
        .expect("multimodal content is an array")
        .clone();
    assert_eq!(parts.len(), 3, "{parts:#?}");
    assert_eq!(parts[0]["type"], "text");
    assert!(
        parts[0]["text"]
            .as_str()
            .unwrap()
            .contains("<file name=\"readme.txt\">"),
        "{parts:#?}"
    );
    assert!(parts[0]["text"]
        .as_str()
        .unwrap()
        .contains("the file contents"));
    assert_eq!(parts[1]["type"], "image_url");
    assert!(
        parts[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"),
        "{parts:#?}"
    );
    assert_eq!(parts[2]["type"], "text");
    assert_eq!(parts[2]["text"], "what do you make of these?");
}

#[tokio::test]
async fn the_send_list_not_upload_order_decides_the_parts_order() {
    // Two uploads from one pick race; the one that finishes first gets the
    // lower id. The chips' order, which the send lists, is what counts.
    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "vision").await;

    let first: Value = upload(&base, tid, "first.txt", b"uploaded first")
        .await
        .json()
        .await
        .unwrap();
    let second: Value = upload(&base, tid, "second.rs", b"fn uploaded_second() {}")
        .await
        .json()
        .await
        .unwrap();
    let (first_id, second_id) = (
        first["id"].as_i64().unwrap(),
        second["id"].as_i64().unwrap(),
    );

    let _ = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "", "attachments": [second_id, first_id] }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Two text-only parts reach an OpenAI upstream joined into one string.
    let reqs = chat_completion_requests(&mock).await;
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let text = sent["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap_or_else(|| panic!("{:#}", sent["messages"]))
        .to_string();
    let (s, f) = (
        text.find("<file name=\"second.rs\">")
            .expect("second.rs is sent"),
        text.find("<file name=\"first.txt\">")
            .expect("first.txt is sent"),
    );
    assert!(s < f, "the send listed second.rs first: {text}");

    // The chips on the reopened thread agree, and so does the title.
    let detail: Value = base
        .client()
        .get(format!("{base}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let user = detail["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .expect("the user turn")
        .clone();
    let names: Vec<&str> = user["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["second.rs", "first.txt"]);
    assert_eq!(detail["thread"]["title"], "second.rs");
}

#[tokio::test]
async fn attachments_are_replayed_on_the_second_turn() {
    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .expect(2)
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "vision").await;
    let att: Value = upload(&base, tid, "shot.png", PNG)
        .await
        .json()
        .await
        .unwrap();
    let aid = att["id"].as_i64().unwrap();

    let _ = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "first turn", "attachments": [aid] }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let _ = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "second turn", "attachments": [] }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let reqs = chat_completion_requests(&mock).await;
    assert_eq!(reqs.len(), 2);
    let second: Value = serde_json::from_slice(&reqs[1].body).unwrap();
    let msgs = second["messages"].as_array().unwrap();
    // The first user turn (after the thread's default system prompt),
    // replayed with its image still attached.
    let first_user = msgs.iter().find(|m| m["role"] == "user").unwrap();
    let parts = first_user["content"].as_array().expect("{first_user:#?}");
    assert!(
        parts.iter().any(|p| p["type"] == "image_url"),
        "the image must still be there on replay: {parts:#?}"
    );
}

// ---------------------------------------------------------------------------
// Vision gating
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_new_image_for_a_model_with_no_vision_is_refused() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "no-vision").await;
    let att: Value = upload(&base, tid, "shot.png", PNG)
        .await
        .json()
        .await
        .unwrap();
    let aid = att["id"].as_i64().unwrap();

    let resp = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "look at this", "attachments": [aid] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "model_no_vision", "{body:#}");
    assert!(
        body["message"].as_str().unwrap().contains("no-vision"),
        "{body:#}"
    );

    // The draft must still be a draft — refused before anything was written.
    let del: Value = base
        .client()
        .post(format!("{base}/chat/api/attachments/{aid}/delete"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        del["ok"], true,
        "a refused send must leave the attachment as a draft"
    );
}

/// An image already in history, replayed after the thread's model was
/// switched to one with no vision: the design calls for a placeholder, not a
/// refusal — only a *new* image refuses the whole send.
#[tokio::test]
async fn a_history_image_becomes_a_placeholder_once_the_model_has_no_vision() {
    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "vision").await;
    let att: Value = upload(&base, tid, "shot.png", PNG)
        .await
        .json()
        .await
        .unwrap();
    let aid = att["id"].as_i64().unwrap();
    let _ = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "first turn", "attachments": [aid] }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Switch the thread to the no-vision model, then send a second turn with
    // no new attachments.
    base.client()
        .post(format!("{base}/chat/api/threads/{tid}/settings"))
        .json(&json!({ "model_alias": "no-vision" }))
        .send()
        .await
        .unwrap();
    let _ = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "second turn" }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let reqs = chat_completion_requests(&mock).await;
    assert_eq!(reqs.len(), 2);
    let second: Value = serde_json::from_slice(&reqs[1].body).unwrap();
    let msgs = second["messages"].as_array().unwrap();
    let first_user = msgs.iter().find(|m| m["role"] == "user").unwrap();
    // No longer multimodal: the image became one placeholder text part.
    let text = first_user["content"]
        .as_str()
        .unwrap_or_else(|| panic!("expected plain text content: {first_user:#?}"));
    assert!(text.contains("shot.png"), "{text}");
    assert!(text.contains("not sent"), "{text}");
    assert!(text.contains("no-vision"), "{text}");
}

// ---------------------------------------------------------------------------
// GET bytes: content type + security headers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_attachment_serves_bytes_with_the_right_content_type_and_security_headers() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "my-model").await;
    let att: Value = upload(&base, tid, "shot.png", PNG)
        .await
        .json()
        .await
        .unwrap();
    let aid = att["id"].as_i64().unwrap();

    let resp = base
        .client()
        .get(format!("{base}/chat/api/attachments/{aid}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "image/png"
    );
    assert_eq!(
        resp.headers()
            .get("x-content-type-options")
            .unwrap()
            .to_str()
            .unwrap(),
        "nosniff"
    );
    assert_eq!(
        resp.headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap(),
        "sandbox"
    );
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(&bytes[..], PNG);
}

#[tokio::test]
async fn get_attachment_serves_text_as_utf8_plain() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "my-model").await;
    let att: Value = upload(&base, tid, "notes.txt", b"hello")
        .await
        .json()
        .await
        .unwrap();
    let aid = att["id"].as_i64().unwrap();

    let resp = base
        .client()
        .get(format!("{base}/chat/api/attachments/{aid}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/plain; charset=utf-8"
    );
}

// ---------------------------------------------------------------------------
// Cascade delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deleting_a_thread_cascades_its_attachments() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "my-model").await;
    let att: Value = upload(&base, tid, "shot.png", PNG)
        .await
        .json()
        .await
        .unwrap();
    let aid = att["id"].as_i64().unwrap();
    assert!(store::get_chat_attachment_full(&state.db, aid)
        .await
        .unwrap()
        .is_some());

    let _ = base
        .client()
        .post(format!("{base}/chat/api/threads/{tid}/delete"))
        .send()
        .await
        .unwrap();

    assert!(
        store::get_chat_attachment_full(&state.db, aid)
            .await
            .unwrap()
            .is_none(),
        "the attachment must be gone with its thread"
    );
}

// ---------------------------------------------------------------------------
// Atomic insert + bind (review finding 5): a concurrent send of the same
// drafts must not silently mail a message with none of the files it claimed.
// ---------------------------------------------------------------------------

/// The exact mechanism finding 5 describes, reproduced deterministically at
/// the store layer rather than by racing real HTTP requests against a
/// scheduler this test does not control: a second "send" that validated the
/// same draft a moment before the first one committed must find the draft
/// already bound when *it* tries to bind — and must roll back its own
/// message entirely rather than commit one silently missing the attachment.
#[tokio::test]
async fn append_user_message_with_attachments_refuses_a_draft_already_bound() {
    let state = AppState::init_for_tests().await.unwrap();
    let tid = store::create_chat_thread(&state.db, "my-model", "chat")
        .await
        .unwrap();
    let aid = store::insert_chat_attachment(
        &state.db,
        tid,
        "text",
        "f.txt",
        "text/plain; charset=utf-8",
        5,
        b"hello",
    )
    .await
    .unwrap();

    let first_id =
        match store::append_user_message_with_attachments(&state.db, tid, "first", &[aid])
            .await
            .unwrap()
        {
            store::SendMessageOutcome::Sent(id) => id,
            store::SendMessageOutcome::AttachmentNotDraft => {
                panic!("the first send must win the draft")
            }
        };

    let second = store::append_user_message_with_attachments(&state.db, tid, "second", &[aid])
        .await
        .unwrap();
    assert!(
        matches!(second, store::SendMessageOutcome::AttachmentNotDraft),
        "a second bind of an already-bound draft must be refused, not silently succeed with zero \
         rows affected"
    );

    // The losing send's message must not exist at all — the whole insert
    // rolled back with the bind, not just the bind.
    let messages = store::list_chat_messages(&state.db, tid).await.unwrap();
    assert_eq!(
        messages.len(),
        1,
        "the losing send must leave no half-sent message behind: {messages:?}"
    );
    assert_eq!(messages[0].id, first_id);

    // The attachment stayed bound to the winner.
    let atts = store::list_chat_attachments_meta(&state.db, tid)
        .await
        .unwrap();
    assert_eq!(atts[0].message_id, Some(first_id));
}

/// The end-to-end version, over real (if pool-serialized) concurrent HTTP
/// requests: whichever of two sends for the same draft loses the race must
/// not come back 200, and the draft must end up bound to exactly one message
/// — never zero (silently unbound forever) and never two.
#[tokio::test]
async fn concurrent_sends_of_the_same_draft_never_both_succeed() {
    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock)
        .await;
    let (state, base) = setup(&mock.uri()).await;
    let tid = new_thread(&base, "my-model").await;
    let att: Value = upload(&base, tid, "shared.txt", b"one file, two racing sends")
        .await
        .json()
        .await
        .unwrap();
    let aid = att["id"].as_i64().unwrap();

    let client = base.client();
    let send_a = client
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "first", "attachments": [aid] }))
        .send();
    let send_b = client
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "second", "attachments": [aid] }))
        .send();
    let (resp_a, resp_b) = tokio::join!(send_a, send_b);
    let status_a = resp_a.unwrap().status();
    let status_b = resp_b.unwrap().status();
    let ok_count = [status_a, status_b]
        .iter()
        .filter(|s| s.is_success())
        .count();
    assert_eq!(
        ok_count, 1,
        "exactly one of the two sends must win the draft: {status_a} / {status_b}"
    );
    // The loser is refused either at the up-front validation (400, if the
    // winner had already fully committed) or at the atomic bind (409, the
    // race this test is really after) — never a silent success.
    for s in [status_a, status_b] {
        assert!(
            s.is_success() || s.as_u16() == 400 || s.as_u16() == 409,
            "unexpected status {s}"
        );
    }

    let atts = store::list_chat_attachments_meta(&state.db, tid)
        .await
        .unwrap();
    assert_eq!(atts.len(), 1);
    assert!(
        atts[0].message_id.is_some(),
        "the attachment must end up bound to exactly one message: {:?}",
        atts[0]
    );
}
