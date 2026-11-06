//! An image card's actions (design §8.3): `POST
//! /chat/api/attachments/{id}/media` re-rolls, varies and upscales a
//! picture of a thread with no chat model in the loop, streams `action`,
//! `progress` and `done`, and stores the result as the thread's `action`
//! attachment bound to the source's message — never sent to the model.

use lmgw_api_types::chat_media::MediaFacts;
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAttachment};
use serde_json::{json, Value};
use wiremock::MockServer;

use super::upscale::{caps_with_upscaler, mount_upscale, put_upscaler, upscales, UPSCALER};
use super::{gateway, image_row, job, mount_sd, png, rows_of, source_png, submitted, Z_IMAGE};
use crate::common::Gw;

/// A thread with one assistant message holding a picture made by
/// `facts`, its `source` given: (thread, message, attachment).
pub(super) async fn picture(
    state: &SharedState,
    facts: Option<MediaFacts>,
    source: &str,
) -> (i64, i64, i64) {
    let tid = store::create_chat_thread(&state.db, "my-model", "chat")
        .await
        .unwrap();
    let mid =
        store::append_chat_message(&state.db, tid, "assistant", "Here.", "", None, None, None)
            .await
            .unwrap();
    let mut att = NewAttachment::plain("image", "image-z-7.png", "image/png", &source_png());
    att.source = source.into();
    if let Some(f) = facts {
        att.meta = json!({"media": f});
        att.call_id = Some("call_1".into());
    }
    let id = store::insert_chat_attachment_new(&state.db, tid, &att)
        .await
        .unwrap();
    sqlx::query("UPDATE chat_attachments SET message_id = ?1 WHERE id = ?2")
        .bind(mid)
        .bind(id)
        .execute(&state.db)
        .await
        .unwrap();
    (tid, mid, id)
}

/// What `image__generate` recorded for a picture of `image/z`.
pub(super) fn made() -> MediaFacts {
    MediaFacts {
        tool: Some("image__generate".into()),
        model: Some("image/z".into()),
        prompt: Some("a red square".into()),
        seed: Some(7),
        steps: Some(6),
        cfg: Some(2.5),
        width: Some(64),
        height: Some(64),
        ..Default::default()
    }
}

/// Run `body` on attachment `id`: the status and, for a stream, its
/// `(event, data)` frames; for a refusal, its JSON as one `refused` frame.
pub(super) async fn act(gw: &Gw, id: i64, body: Value) -> (u16, Vec<(String, Value)>) {
    let resp = gw
        .client()
        .post(format!("{gw}/chat/api/attachments/{id}/media"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    if status != 200 {
        return (
            status,
            vec![("refused".into(), serde_json::from_str(&text).unwrap())],
        );
    }
    let mut frames = Vec::new();
    for block in text.split("\n\n") {
        let mut event = None;
        let mut data = String::new();
        for line in block.lines() {
            if let Some(e) = line.strip_prefix("event:") {
                event = Some(e.trim().to_string());
            } else if let Some(d) = line.strip_prefix("data:") {
                data.push_str(d.trim_start());
            }
        }
        if let Some(e) = event {
            frames.push((e, serde_json::from_str(&data).unwrap_or(Value::Null)));
        }
    }
    (status, frames)
}

pub(super) fn frame<'a>(frames: &'a [(String, Value)], event: &str) -> &'a Value {
    &frames
        .iter()
        .find(|(e, _)| e == event)
        .unwrap_or_else(|| panic!("no {event} frame in {frames:?}"))
        .1
}

/// `(message_id, source, call_id, meta)` of attachment `id`.
pub(super) async fn stored(
    state: &SharedState,
    id: i64,
) -> (Option<i64>, String, Option<String>, Value) {
    let (mid, source, call_id, meta): (Option<i64>, String, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT message_id, source, call_id, meta FROM chat_attachments WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(&state.db)
        .await
        .unwrap();
    let meta = meta
        .and_then(|m| serde_json::from_str(&m).ok())
        .unwrap_or(Value::Null);
    (mid, source, call_id, meta)
}

#[tokio::test]
async fn a_reroll_draws_again_with_a_new_seed_and_binds_to_the_message() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("generating"), job("completed")]).await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let (tid, mid, id) = picture(&state, Some(made()), "tool").await;

    let (status, frames) = act(&gw, id, json!({"action": "reroll"})).await;
    assert_eq!(status, 200, "{frames:?}");
    assert_eq!(frames[0].0, "action", "{frames:?}");
    let started = &frames[0].1;
    let call_id = started["call_id"].as_str().unwrap().to_string();
    assert!(call_id.starts_with("act-"), "{started}");
    assert_eq!(started["model"], json!("image/z"));
    assert_eq!(started["source"], json!(id));
    assert_eq!(frames.last().unwrap().0, "done", "{frames:?}");
    let done = frame(&frames, "done");
    assert_eq!(done["call_id"], json!(call_id));
    let new_id = done["attachment"]["id"].as_i64().unwrap();
    assert_eq!(done["attachment"]["source"], json!("action"));
    assert!(
        done["text"].as_str().unwrap().starts_with(&format!(
            "Made image att:{new_id} (64x64) with image/z: seed "
        )),
        "{done}"
    );

    // The same settings went out, the seed left to a new draw.
    let body = &submitted(&sd).await[0];
    assert_eq!(body["prompt"], json!("a red square"));
    assert_eq!(body["sample_params"]["sample_steps"], json!(6));
    assert_eq!(body["sample_params"]["guidance"]["txt_cfg"], json!(2.5));
    assert_eq!(
        (body["width"].clone(), body["height"].clone()),
        (json!(64), json!(64))
    );
    assert!(body["seed"].is_i64(), "{body}");

    let (bound, source, cid, meta) = stored(&state, new_id).await;
    assert_eq!(bound, Some(mid), "bound to the source's message");
    assert_eq!(source, "action");
    assert_eq!(cid.as_deref(), Some(call_id.as_str()));
    assert_eq!(meta["media"]["parent"], json!(id));
    assert_eq!(meta["media"]["tool"], json!("image__generate"));

    // Not shown to the model: the history's attachments are uploads only.
    let sent = store::list_sent_chat_attachments(&state.db, tid, None)
        .await
        .unwrap();
    assert!(sent.iter().all(|a| a.id != new_id), "{sent:?}");

    let rows = rows_of(&state, "image", 1).await;
    assert_eq!(rows[0]["proto"], json!("chat"), "{rows:?}");
    assert_eq!(rows[0]["images_out"], json!(1));
}

#[tokio::test]
async fn vary_takes_the_picture_and_the_body_and_edit_is_refused_on_a_variation_row() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let (_tid, _mid, id) = picture(&state, Some(made()), "tool").await;

    let (status, frames) = act(
        &gw,
        id,
        json!({"action": "vary", "prompt": "the square in blue", "strength": 0.3}),
    )
    .await;
    assert_eq!(status, 200, "{frames:?}");
    let done = frame(&frames, "done");
    assert!(
        done["text"].as_str().unwrap().contains("as a variation"),
        "{done}"
    );
    let body = &submitted(&sd).await[0];
    assert_eq!(body["prompt"], json!("the square in blue"));
    assert_eq!(body["strength"], json!(0.3));
    assert!(body.get("init_image").is_some() && body.get("ref_images").is_none());

    let (status, frames) = act(&gw, id, json!({"action": "edit"})).await;
    assert_eq!(status, 422);
    assert_eq!(frames[0].1["code"], json!("wrong_action"), "{frames:?}");
}

#[tokio::test]
async fn an_upscale_names_the_row_it_ran_on_and_keeps_the_parent() {
    let sd = MockServer::start().await;
    mount_upscale(&sd, caps_with_upscaler()).await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let (_tid, mid, id) = picture(&state, Some(made()), "tool").await;

    // No upscaler yet.
    let (status, frames) = act(&gw, id, json!({"action": "upscale"})).await;
    assert_eq!(status, 422);
    assert_eq!(frames[0].1["code"], json!("no_upscaler"), "{frames:?}");

    put_upscaler(&state);
    let (status, frames) = act(&gw, id, json!({"action": "upscale", "upscaler": UPSCALER})).await;
    assert_eq!(status, 200, "{frames:?}");
    assert!(frames[0].1.get("model").is_none(), "{frames:?}");
    // The row's load first, as a picture's (design §5.3), then the upscale.
    let phases: Vec<&Value> = frames
        .iter()
        .filter(|(e, _)| e == "progress")
        .map(|(_, f)| &f["phase"])
        .collect();
    assert_eq!(
        phases,
        [&json!("loading"), &json!("upscaling")],
        "{frames:?}"
    );
    let progress = &frames
        .iter()
        .find(|(e, f)| e == "progress" && f["phase"] == "upscaling")
        .unwrap()
        .1;
    assert_eq!(progress["model"], json!("image/z"));
    assert_eq!(progress["call_id"], frames[0].1["call_id"]);
    let done = frame(&frames, "done");
    assert_eq!(done["attachment"]["name"], json!("image-z-7-x4.png"));
    let new_id = done["attachment"]["id"].as_i64().unwrap();
    let (bound, _, _, meta) = stored(&state, new_id).await;
    assert_eq!(bound, Some(mid));
    assert_eq!(meta["media"]["parent"], json!(id));
    assert_eq!(meta["media"]["upscaler"], json!(UPSCALER));
    assert_eq!(meta["media"]["model"], json!("image/z"));
    assert_eq!(upscales(&sd).await.len(), 1);
}

#[tokio::test]
async fn what_an_action_cannot_start_from_is_refused_before_anything_runs() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;

    // An upload has no model to run again.
    let (_tid, _mid, upload) = picture(&state, None, "upload").await;
    let (status, frames) = act(&gw, upload, json!({"action": "reroll"})).await;
    assert_eq!(status, 422);
    assert_eq!(frames[0].1["code"], json!("not_rerollable"));
    let (status, frames) = act(&gw, upload, json!({"action": "vary"})).await;
    assert_eq!(status, 422);
    assert_eq!(frames[0].1["code"], json!("no_model"));

    // A draft belongs to no message yet.
    let tid = store::create_chat_thread(&state.db, "my-model", "chat")
        .await
        .unwrap();
    let draft = store::insert_chat_attachment_new(
        &state.db,
        tid,
        &NewAttachment::plain("image", "d.png", "image/png", &png()),
    )
    .await
    .unwrap();
    let (status, frames) = act(&gw, draft, json!({"action": "upscale"})).await;
    assert_eq!(status, 409);
    assert_eq!(frames[0].1["code"], json!("not_sent"));

    // Not a picture, and not there.
    let text = store::insert_chat_attachment_new(
        &state.db,
        tid,
        &NewAttachment::plain("text", "a.txt", "text/plain", b"hi"),
    )
    .await
    .unwrap();
    let (status, _) = act(&gw, text, json!({"action": "upscale"})).await;
    assert_eq!(status, 422);
    let (status, _) = act(&gw, 999_999, json!({"action": "upscale"})).await;
    assert_eq!(status, 404);

    // A model that is gone.
    let mut gone = made();
    gone.model = Some("image/removed".into());
    let (_tid, _mid, id) = picture(&state, Some(gone), "tool").await;
    let (status, frames) = act(&gw, id, json!({"action": "reroll"})).await;
    assert_eq!(status, 422);
    assert_eq!(frames[0].1["code"], json!("model_unavailable"));
    assert!(submitted(&sd).await.is_empty(), "nothing ran");
}
