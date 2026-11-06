//! A media call while it runs (design §5.1–§5.3, §6.2): the Chat turn's
//! `progress` frames — the stages of bringing the image row in, the image
//! server's queue, each step with its preview — the preview mode the job
//! is asked for, the result frame's files, and the chat model's own
//! `state` frames when it has to be loaded again after the picture.

use lmgw_api_types::chat_media::PreviewMode;
use serde_json::{json, Value};
use wiremock::MockServer;

use super::chat_turn::{answer, chat_model, send, thread, tool_call};
use super::{gateway, generating, image_row, job, mount_sd, submitted, Z_IMAGE};
use crate::common::serve;

/// The `data` of every SSE event named `event` in `body`, in order.
pub(super) fn events(body: &str, event: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|block| {
            let mut name = None;
            let mut data = String::new();
            for line in block.lines() {
                if let Some(n) = line.strip_prefix("event:") {
                    name = Some(n.trim().to_string());
                } else if let Some(d) = line.strip_prefix("data:") {
                    data.push_str(d.trim_start());
                }
            }
            (name.as_deref() == Some(event))
                .then(|| serde_json::from_str(&data).ok())
                .flatten()
        })
        .collect()
}

/// The `tool` frames of event `progress` in a turn's body.
pub(super) fn progress_frames(body: &str) -> Vec<Value> {
    events(body, "tool")
        .into_iter()
        .filter(|f| f["event"] == "progress")
        .collect()
}

/// The chat model asks for one picture of `image/z`, then answers.
async fn draws_once(state: &lmgw_core::state::SharedState, chat: &MockServer) {
    chat_model(
        state,
        chat,
        &[
            tool_call(
                "c1",
                "image__generate",
                json!({"model": "image/z", "prompt": "a red square"}),
            ),
            answer("here it is"),
        ],
    )
    .await;
}

#[tokio::test]
async fn a_turn_streams_the_job_s_queue_steps_and_previews() {
    let sd = MockServer::start().await;
    let mut queued = job("queued");
    queued["queue_position"] = json!(1);
    mount_sd(
        &sd,
        &[
            queued,
            generating(1, 3, "QUJD"),
            // The same step again: no new frame.
            generating(1, 3, "QUJD"),
            // Pass 2 starts its steps again.
            generating(2, 1, "REVG"),
            job("completed"),
        ],
    )
    .await;
    let chat = MockServer::start().await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    draws_once(&state, &chat).await;
    let tid = thread(&gw).await;
    let body = send(&gw, tid, "draw me a red square").await;
    assert!(body.contains("here it is"), "{body}");

    // The Chat shows previews: the row's default, the quick sketch the
    // probed build lists, at every step.
    let sent = &submitted(&sd).await[0];
    assert_eq!(sent["preview"], json!("proj"), "{sent}");
    assert_eq!(sent["preview_interval"], json!(1), "{sent}");

    let frames = progress_frames(&body);
    let phases: Vec<&str> = frames
        .iter()
        .map(|f| f["phase"].as_str().unwrap())
        .collect();
    assert_eq!(
        phases,
        ["loading", "queued", "generating", "generating"],
        "{frames:#?}"
    );
    assert!(frames.iter().all(|f| f["call_id"] == "c1"), "{frames:#?}");
    assert_eq!(frames[0]["model"], json!("image/z"));
    assert!(frames[0].get("cancel").is_none(), "no job yet: {frames:#?}");
    // 0518b34 cancels a queued job itself, and a generating one not.
    assert_eq!(frames[1]["queue_position"], json!(1));
    assert_eq!(frames[1]["cancel"], json!("native"));
    assert_eq!(
        (
            &frames[2]["pass"],
            &frames[2]["step"],
            &frames[2]["total_steps"]
        ),
        (&json!(1), &json!(3), &json!(8))
    );
    assert_eq!(frames[2]["preview"], json!("QUJD"));
    assert_eq!(frames[2]["cancel"], json!("restart"));
    assert_eq!(
        (&frames[3]["pass"], &frames[3]["step"]),
        (&json!(2), &json!(1))
    );
    assert_eq!(frames[3]["preview"], json!("REVG"));

    // The result frame names the file the call stored.
    let result = events(&body, "tool")
        .into_iter()
        .find(|f| f["event"] == "result")
        .expect("a result frame");
    let atts = result["attachments"].as_array().expect("attachments");
    assert_eq!(atts.len(), 1, "{result:#}");
    assert_eq!(atts[0]["source"], json!("tool"));
    assert_eq!(atts[0]["call_id"], json!("c1"));
    assert_eq!(atts[0]["kind"], json!("image"));
    assert_eq!(atts[0]["meta"]["media"]["model"], json!("image/z"));
}

#[tokio::test]
async fn a_row_with_previews_off_reports_its_time_only() {
    let sd = MockServer::start().await;
    let mut plain = job("generating");
    plain.as_object_mut().unwrap().remove("preview");
    mount_sd(&sd, &[plain, job("completed")]).await;
    let chat = MockServer::start().await;
    let mut row = image_row("z", Z_IMAGE, false);
    row.preview_mode = Some(PreviewMode::None);
    let (state, gw) = gateway(&sd, &[row]).await;
    draws_once(&state, &chat).await;
    let tid = thread(&gw).await;
    let body = send(&gw, tid, "draw me a red square").await;

    let sent = &submitted(&sd).await[0];
    assert!(sent.get("preview").is_none(), "{sent}");
    let frames = progress_frames(&body);
    let gen = frames
        .iter()
        .find(|f| f["phase"] == "generating")
        .unwrap_or_else(|| panic!("{frames:#?}"));
    for k in ["pass", "step", "total_steps", "preview"] {
        assert!(gen.get(k).is_none(), "{k}: {gen}");
    }
    assert!(gen["elapsed_ms"].is_u64());
}

/// The picture's admission names the chat model it evicts, and the chat
/// model's return says `loading` then `ready` as the turn's first
/// admission does (§5.3).
#[tokio::test]
async fn the_swap_names_the_evicted_model_and_the_regain_says_its_state() {
    let g = super::claim::world().await;
    let gw = serve(g.state.clone()).await;
    let body = super::claim::chat_turn(&gw, "gemma").await;

    let frames = progress_frames(&body);
    assert!(
        frames
            .iter()
            .any(|f| f["phase"] == "unloading" && f["model"] == "chat/gemma"),
        "{frames:#?}"
    );
    assert!(
        frames
            .iter()
            .any(|f| f["phase"] == "loading" && f["model"] == "image/z"),
        "{frames:#?}"
    );
    // The turn's first admission and the regain each say loading, ready.
    let states: Vec<(String, String)> = events(&body, "state")
        .into_iter()
        .filter(|s| s["stage"] == "chat")
        .map(|s| {
            (
                s["alias"].as_str().unwrap_or_default().to_string(),
                s["state"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let gemma = |s: &str| ("gemma".to_string(), s.to_string());
    assert_eq!(
        states,
        [
            gemma("loading"),
            gemma("ready"),
            gemma("loading"),
            gemma("ready")
        ],
        "{body}"
    );
    // And in that order: the regain after the picture.
    let unloading = body.find("\"phase\":\"unloading\"").unwrap();
    let regain = body.rfind("\"state\":\"loading\"").unwrap();
    assert!(regain > unloading, "{body}");
}
