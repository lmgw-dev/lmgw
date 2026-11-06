//! A card action and the thread around it (design §6.3, §8.3): Keep gives
//! every kept picture's `meta.media.parent` the kept parent's id, so a
//! re-roll of a kept edit edits its input again; a thread deleted or kept
//! while an action runs stops the action, says so, and no file of it is
//! left behind unbound; and a card action's picture is not the model's.

use std::time::{Duration, Instant};

use lmgw_api_types::chat_media::MediaFacts;
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, ChatMessageRow, ChatThread, KeptAttachment, NewAttachment};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::actions::{act, frame, made, picture, stored};
use super::upscale::{caps_with_upscaler, put_upscaler, upscales, UPSCALER};
use super::{b64, gateway, image_row, job, mount_sd, png, source_png, submitted, Z_IMAGE};
use crate::chat_actions::{mount_openai_reply, post};
use crate::common::Gw;

const AT: &str = "2026-10-21 12:00:00";

/// What `image__edit` recorded for an edit of picture `parent`.
fn edited(parent: i64) -> MediaFacts {
    MediaFacts {
        tool: Some("image__edit".into()),
        model: Some("image/z".into()),
        prompt: Some("the square in blue".into()),
        seed: Some(9),
        steps: Some(6),
        parent: Some(parent),
        ..Default::default()
    }
}

/// A temporary thread's picture as Keep hands it to the store.
fn file(id: i64, source: &str, call: &str, facts: MediaFacts) -> KeptAttachment {
    let data = source_png();
    KeptAttachment {
        id,
        message_id: Some(-2),
        kind: "image".into(),
        name: format!("image-z-{}.png", -id),
        mime: "image/png".into(),
        size: data.len() as i64,
        ord: 0,
        data,
        created_at: AT.into(),
        extracted: None,
        meta: json!({ "media": facts }),
        mode: None,
        source: source.into(),
        call_id: Some(call.into()),
        pages: Vec::new(),
    }
}

/// A reply's record: `image__edit` of `att:-3`, its result `att:-5`.
fn record() -> Value {
    json!([
        {"role": "assistant", "content": [{"type": "tool_use", "id": "c2",
            "name": "image__edit", "args": {"image": "att:-3", "prompt": "the square in blue"}}]},
        {"role": "tool", "content": [{"type": "tool_result", "id": "c2", "name": "image__edit",
            "content": [{"type": "text", "text": "Made image att:-5 (64x64)."}],
            "is_error": false}]},
    ])
}

/// The kept thread's attachment ids by their call id.
async fn by_call(state: &SharedState, thread: i64) -> Vec<(String, i64, Value)> {
    let rows: Vec<(Option<String>, i64, String)> = sqlx::query_as(
        "SELECT call_id, id, meta FROM chat_attachments WHERE thread_id = ?1 ORDER BY id",
    )
    .bind(thread)
    .fetch_all(&state.db)
    .await
    .unwrap();
    rows.into_iter()
        .map(|(c, id, m)| (c.unwrap_or_default(), id, serde_json::from_str(&m).unwrap()))
        .collect()
}

#[tokio::test]
async fn a_reroll_after_keep_edits_the_kept_input_again() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;

    // A temporary thread: a generation (-3), an edit of it by the tool
    // (-5), and a card action's re-roll of that edit (-7) — both edits'
    // parent the generation's temporary id. The reply's record names the
    // edit's input in the call's arguments and its picture in the result.
    let thread = ChatThread {
        title: "kept".into(),
        model_alias: "my-model".into(),
        kind: "chat".into(),
        created_at: AT.into(),
        updated_at: AT.into(),
        ..Default::default()
    };
    let messages =
        [("user", -1, "draw"), ("assistant", -2, "Here.")].map(|(role, id, c)| ChatMessageRow {
            id,
            thread_id: -9,
            role: role.into(),
            content: c.into(),
            created_at: AT.into(),
            ir_messages: (role == "assistant").then(|| record().to_string()),
            ..Default::default()
        });
    let files = [
        file(-3, "tool", "c1", made()),
        file(-5, "tool", "c2", edited(-3)),
        file(-7, "action", "act-0-1", edited(-3)),
    ];
    let kept = store::insert_kept_chat_thread(&state.db, &thread, &messages, &files, None)
        .await
        .unwrap();

    let rows = by_call(&state, kept).await;
    let id_of = |call: &str| rows.iter().find(|(c, ..)| c == call).unwrap().1;
    let (generated, edit, action) = (id_of("c1"), id_of("c2"), id_of("act-0-1"));
    for (call, _, meta) in &rows[1..] {
        assert_eq!(meta["media"]["parent"], json!(generated), "{call}: {meta}");
    }
    assert!(rows[0].2["media"].get("parent").is_none(), "{:?}", rows[0]);
    let ir: Option<String> = sqlx::query_scalar(
        "SELECT ir_messages FROM chat_messages WHERE thread_id = ?1 AND role = 'assistant'",
    )
    .bind(kept)
    .fetch_one(&state.db)
    .await
    .unwrap();
    let ir: Value = serde_json::from_str(&ir.expect("the record")).unwrap();
    let text = ir.to_string();
    assert!(!text.contains("att:-"), "{text}");
    assert_eq!(
        ir[0]["content"][0]["args"]["image"],
        json!(format!("att:{generated}")),
        "the call's input names the kept generation: {text}"
    );
    assert!(text.contains(&format!("att:{edit}")), "{text}");

    // Re-rolling either edit edits the kept generation again.
    for src in [edit, action] {
        let (status, frames) = act(&gw, src, json!({"action": "reroll"})).await;
        assert_eq!(status, 200, "{frames:?}");
        let done = frame(&frames, "done");
        let new_id = done["attachment"]["id"].as_i64().unwrap();
        let (_, _, _, meta) = stored(&state, new_id).await;
        assert_eq!(meta["media"]["parent"], json!(generated), "{meta}");
    }
    let sent = submitted(&sd).await;
    assert_eq!(sent.len(), 2);
    assert!(
        sent.iter().all(|b| b.get("init_image").is_some()),
        "{sent:?}"
    );
}

/// sd-server on `sd` whose upscale takes `delay` to answer.
async fn mount_slow_upscale(sd: &MockServer, delay: Duration) {
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(caps_with_upscaler()))
        .mount(sd)
        .await;
    Mock::given(method("POST"))
        .and(path("/sdcpp/v1/upscale"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(delay)
                .set_body_json(json!({
                    "images": [{"index": 0, "b64_json": b64(&png())}],
                    "upscaler": UPSCALER, "scale": 4, "repeats": 1,
                    "width": 256, "height": 256, "output_format": "png",
                })),
        )
        .mount(sd)
        .await;
}

/// Wait until the image server has the upscale (bounded: the test fails
/// rather than hangs).
async fn upscale_reached(sd: &MockServer) {
    for _ in 0..500 {
        if !upscales(sd).await.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the upscale never reached the image server");
}

/// An upscale of `id` started in the background: its frames when it ends.
fn upscaling(gw: &Gw, id: i64) -> tokio::task::JoinHandle<(u16, Vec<(String, Value)>)> {
    let gw = gw.clone();
    tokio::spawn(async move { act(&gw, id, json!({"action": "upscale"})).await })
}

/// A file of `source` other than an upload with no message, in any thread.
async fn unbound(state: &SharedState) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_attachments WHERE message_id IS NULL AND source <> 'upload'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap()
}

/// The action ended at once with `thread_gone`, well before the image
/// server's answer.
fn stopped_as_gone(frames: &[(String, Value)], took: Duration, delay: Duration) {
    let (event, last) = frames.last().unwrap();
    assert_eq!(event, "error", "{frames:?}");
    assert_eq!(last["code"], json!("thread_gone"), "{last}");
    assert!(
        last["message"]
            .as_str()
            .unwrap()
            .contains("kept as a stored chat"),
        "{last}"
    );
    assert!(took < delay, "stopped, not waited out: {took:?}");
}

#[tokio::test]
async fn deleting_the_thread_stops_its_running_action() {
    let delay = Duration::from_secs(20);
    let sd = MockServer::start().await;
    mount_slow_upscale(&sd, delay).await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    put_upscaler(&state);
    let (tid, _mid, id) = picture(&state, Some(made()), "tool").await;

    let started = Instant::now();
    let run = upscaling(&gw, id);
    upscale_reached(&sd).await;
    let r = post(&gw, &format!("/chat/api/threads/{tid}/delete"), json!({})).await;
    assert_eq!(r.status(), 200);
    let (status, frames) = run.await.unwrap();
    assert_eq!(status, 200, "{frames:?}");
    stopped_as_gone(&frames, started.elapsed(), delay);
    assert_eq!(unbound(&state).await, 0);
}

#[tokio::test]
async fn keeping_a_temporary_thread_stops_its_running_action_and_keeps_no_file_of_it() {
    let delay = Duration::from_secs(20);
    let sd = MockServer::start().await;
    mount_slow_upscale(&sd, delay).await;
    mount_openai_reply(&sd, "a reply", 3, 2).await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    put_upscaler(&state);
    // The chat model the temporary thread's send asks.
    let up = store::insert_upstream(
        &state.db,
        &store::NewUpstream {
            name: "chat".into(),
            protocol: Protocol::OpenaiCompatible,
            kind: UpstreamKind::Generic,
            base_url: sd.uri(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &store::NewAlias {
            alias: "m".into(),
            upstream_id: up,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let t: Value = post(
        &gw,
        "/chat/api/threads",
        json!({"model_alias": "m", "temporary": true}),
    )
    .await
    .json()
    .await
    .unwrap();
    let tid = t["id"].as_i64().unwrap();
    let att: Value = gw
        .client()
        .post(format!(
            "{gw}/chat/api/threads/{tid}/attachments?name=p.png"
        ))
        .body(png())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let aid = att["id"].as_i64().unwrap();
    assert_eq!(att["kind"], json!("image"), "{att}");
    let body = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "look", "attachments": [aid]}),
    )
    .await
    .text()
    .await
    .unwrap();
    assert!(body.contains("event: done"), "{body}");

    let started = Instant::now();
    let run = upscaling(&gw, aid);
    upscale_reached(&sd).await;
    let kept: Value = post(&gw, &format!("/chat/api/threads/{tid}/persist"), json!({}))
        .await
        .json()
        .await
        .unwrap();
    let kept = kept["id"].as_i64().unwrap();
    let (status, frames) = run.await.unwrap();
    assert_eq!(status, 200, "{frames:?}");
    stopped_as_gone(&frames, started.elapsed(), delay);

    // The kept thread holds the upload and nothing of the action.
    let sources: Vec<(String, Option<i64>)> =
        sqlx::query_as("SELECT source, message_id FROM chat_attachments WHERE thread_id = ?1")
            .bind(kept)
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert_eq!(sources.len(), 1, "{sources:?}");
    assert_eq!(sources[0].0, "upload");
    assert!(sources[0].1.is_some());
    assert_eq!(unbound(&state).await, 0);
}

#[tokio::test]
async fn a_card_actions_picture_is_never_the_models_by_ref() {
    let db = store::open_in_memory().await.unwrap();
    let t = store::create_chat_thread(&db, "m", "chat").await.unwrap();
    let upload = store::insert_chat_attachment_new(
        &db,
        t,
        &NewAttachment::plain("image", "cat.png", "image/png", &png()),
    )
    .await
    .unwrap();
    let mut a = NewAttachment::plain("image", "image-z-7-x4.png", "image/png", &png());
    a.source = "action".into();
    a.call_id = Some("act-0-1".into());
    let action = store::insert_chat_attachment_new(&db, t, &a).await.unwrap();

    let named: Vec<i64> = store::chat_media_attachment_ids(&db, t)
        .await
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(named, vec![upload]);
    assert!(action > upload);
}
