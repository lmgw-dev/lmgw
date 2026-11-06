//! A chat turn whose model waits for its engine's download (machine-check
//! design §5, D40, the WP11 review): each way the wait can end after the
//! turn said `downloading` — `held`, `fallback`, `failed` — the phase after
//! the download said even when admission says none of its own, and a
//! candidate alias, whose pick is admission's, saying the download its pick
//! waits for.
//!
//! Podman's API is the scripted recording of [`super::engine_download`],
//! the registry its mock.

use lmgw_core::store::NewCandidateAlias;

use super::engine_download::{engines, registry, scripted, ENGINE, TOTAL};
use super::*;

/// The chat class's image is [`ENGINE`], missing; the embedder's is here.
async fn chat_engine_missing(f: &Fixture) {
    engines(f, ENGINE, "ghcr.io/ggml-org/llama.cpp:server-cuda").await;
}

/// A thread on `alias`, and a turn sent into it: the turn's SSE body once
/// it has ended.
pub(super) async fn send_turn(f: &Fixture, alias: &str) -> tokio::task::JoinHandle<String> {
    let tid = f
        .gateway
        .client()
        .post(format!("{}/chat/api/threads", f.gateway))
        .json(&json!({"model_alias": alias}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let gateway = f.gateway.clone();
    tokio::spawn(async move {
        gateway
            .client()
            .post(format!("{gateway}/chat/api/threads/{tid}/send"))
            .json(&json!({"content": "hi"}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    })
}

/// The turn's body once it ended, within a bound a download still running
/// does not reach.
async fn ended(turn: tokio::task::JoinHandle<String>) -> String {
    tokio::time::timeout(Duration::from_secs(20), turn)
        .await
        .expect("the turn ended")
        .unwrap()
}

/// The `state` frames of a turn's body, and the `state` each says.
pub(super) fn states(body: &str) -> (Vec<Value>, Vec<String>) {
    let frames: Vec<Value> = crate::chat_actions::sse_events(body)
        .into_iter()
        .filter(|(e, _)| e == "state")
        .map(|(_, d)| d)
        .collect();
    let said = frames
        .iter()
        .map(|s| s["state"].as_str().unwrap_or_default().to_string())
        .collect();
    (frames, said)
}

/// The turn said `downloading` and then, as its last frame, `last`.
fn downloading_then(body: &str, last: &str) -> Value {
    let (frames, said) = states(body);
    let Some(first_dl) = said.iter().position(|s| s == "downloading") else {
        panic!("no downloading frame: {said:?}\n{body}");
    };
    assert_eq!(said[0], "loading", "{said:?}");
    assert!(first_dl > 0, "{said:?}");
    assert_eq!(said.last().map(String::as_str), Some(last), "{said:?}");
    assert!(
        !said[first_dl..].iter().any(|s| s == "ready"),
        "no ready after the download: {said:?}"
    );
    frames.last().unwrap().clone()
}

/// Until the chat model's download has moved its first bytes.
async fn downloading(f: &Fixture) {
    until_vram(f, "the chat model's download under way", |v| {
        v["downloading"][0]["model"] == "chat-model"
            && v["downloading"][0]["phase"]["done_bytes"].as_u64() > Some(0)
    })
    .await;
}

/// The hold switched on while the turn's model downloads: `held`, after
/// the `downloading` frames, and the turn's error says `gpu_hold`.
#[tokio::test]
async fn the_hold_mid_download_ends_the_turn_held() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (_api, release) = scripted(&f, None);
    chat_engine_missing(&f).await;

    let turn = send_turn(&f, "chat-model").await;
    downloading(&f).await;
    engage_hold(&f).await;
    let body = ended(turn).await;
    let held = downloading_then(&body, "held");
    assert_eq!(held["cause"], "gpu_hold", "{held}");
    let error = crate::chat_actions::sse_events(&body)
        .into_iter()
        .find(|(e, _)| e == "error")
        .unwrap_or_else(|| panic!("an error frame: {body}"));
    assert_eq!(error.1["code"], "gpu_hold", "{body}");
    release.send(true).unwrap();
    assert!(f.runs().is_empty(), "nothing started under the hold");
}

/// The room turns into VRAM outside lmgw's control while the turn's model
/// downloads, and the model has a fallback: `fallback`, naming it.
#[tokio::test]
async fn outside_use_mid_download_ends_the_turn_on_its_fallback() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (_api, release) = scripted(&f, None);
    chat_engine_missing(&f).await;
    let _cloud = cloud_upstream(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;

    let turn = send_turn(&f, "chat-model").await;
    downloading(&f).await;
    // With every lmgw model gone 5 GiB would be free, and the chat model
    // needs 6.5.
    f.attribute(3 * GIB);
    let body = ended(turn).await;
    let fell = downloading_then(&body, "fallback");
    assert_eq!(fell["answered_by"], "cloud-chat", "{fell}");
    release.send(true).unwrap();
    assert!(f.runs().is_empty());
}

/// The download fails: `failed`, after the `downloading` frame its first
/// step said, with the reason naming the image.
#[tokio::test]
async fn a_failed_download_ends_the_turn_failed() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (_api, _release) = scripted(&f, Some("manifest unknown: manifest unknown"));
    chat_engine_missing(&f).await;

    let body = ended(send_turn(&f, "chat-model").await).await;
    let failed = downloading_then(&body, "failed");
    let message = failed["message"].as_str().unwrap_or_default();
    assert!(message.contains(ENGINE), "{failed}");
    assert!(message.contains("manifest unknown"), "{failed}");
    assert!(f.runs().is_empty());
}

async fn set_admission(f: &Fixture, on: bool) {
    let mut s = f.state.snapshot().settings.clone();
    s.vram.enabled = on;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// Admission switched off says no phase of its own after the download: the
/// download's end says `loading` all the same (review M1), and `ready`
/// carries the load's own time.
#[tokio::test]
async fn with_admission_off_the_download_still_hands_over_to_loading() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    set_admission(&f, false).await;
    let _reg = registry(&f).await;
    let (_api, release) = scripted(&f, None);
    chat_engine_missing(&f).await;

    let turn = send_turn(&f, "chat-model").await;
    downloading(&f).await;
    // The turn reads its bytes at the pull's own pace; give it one look.
    tokio::time::sleep(Duration::from_millis(600)).await;
    release.send(true).unwrap();
    let body = ended(turn).await;
    let (frames, said) = states(&body);
    let last_dl = said
        .iter()
        .rposition(|s| s == "downloading")
        .unwrap_or_else(|| panic!("no downloading frame: {said:?}\n{body}"));
    assert_eq!(said[0], "loading", "{said:?}");
    assert_eq!(&said[last_dl + 1..], ["loading", "ready"], "{said:?}");
    let ready = frames.last().unwrap();
    assert!(ready["ms"].as_u64().is_some(), "a load time: {ready}");
    assert!(
        frames
            .iter()
            .any(|s| s["state"] == "downloading" && s["total_bytes"] == TOTAL),
        "{frames:?}"
    );
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
}

/// A candidate alias says nothing up front — its pick may be a model that
/// is up — but a download its pick waits for is said like any turn's, then
/// `loading` and `ready` (review L6).
#[tokio::test]
async fn a_candidate_alias_turn_says_the_download_its_pick_waits_for() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (_api, release) = scripted(&f, None);
    chat_engine_missing(&f).await;
    store::insert_candidate_alias(
        &f.state.db,
        &NewCandidateAlias {
            alias: "picks-chat".into(),
            candidates: vec!["chat-model".into()],
            background: false,
            fallback: None,
            capabilities_disabled: vec![],
            capabilities_enabled: vec![],
            enabled: true,
            notes: String::new(),
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();

    let turn = send_turn(&f, "picks-chat").await;
    downloading(&f).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    release.send(true).unwrap();
    let body = ended(turn).await;
    let (frames, said) = states(&body);
    assert_eq!(
        said.first().map(String::as_str),
        Some("downloading"),
        "{said:?}\n{body}"
    );
    let last_dl = said.iter().rposition(|s| s == "downloading").unwrap();
    assert_eq!(&said[last_dl + 1..], ["loading", "ready"], "{said:?}");
    assert!(
        frames.iter().all(|s| s["alias"] == "picks-chat"),
        "{frames:?}"
    );

    // Its model is up now: the next turn says nothing.
    let body = ended(send_turn(&f, "picks-chat").await).await;
    assert_eq!(states(&body).1, Vec::<String>::new(), "{body}");
}
