//! The download before a model's first start, and the rules every other
//! wait keeps (machine-check design §5, the WP10 review): the GPU hold and
//! the outside-VRAM verdict end the wait for a download, never the download;
//! a guest never waits for one; a wait that ran to its end reads the row
//! again; a client that hangs up leaves the download to the next request.

use std::time::Instant;

use lmgw_core::backends::pull;
use lmgw_core::runtime::argv::never_pull;
use lmgw_core::runtime::Class;
use lmgw_core::vram::{Admission, ExternalFallback, Fit};

use super::engine_download::{engines, image_pulls, registry, scripted, ENGINE};
use super::ladder_admission::{ladder_fixture, ladder_hold, ladder_runs, LADDER};
use super::ladder_review_fixes::{spawn_climb, until_ladder};
use super::*;
use crate::support::scripted_pull::ScriptedPull;

/// The class images the fixture's two models run: chat in [`ENGINE`]
/// (missing), the embedder in an image that is here.
async fn chat_engine_missing(f: &Fixture) {
    engines(f, ENGINE, "ghcr.io/ggml-org/llama.cpp:server-cuda").await;
}

/// Until the chat model's download has moved its first bytes.
async fn downloading(f: &Fixture) {
    until_vram(f, "the chat model's download under way", |v| {
        v["downloading"][0]["model"] == "chat-model"
            && v["downloading"][0]["phase"]["done_bytes"].as_u64() > Some(0)
    })
    .await;
}

/// The pull is still going: one live job, and Podman's answer still read.
fn still_pulling(f: &Fixture, api: &ScriptedPull) {
    assert_eq!(image_pulls(f).len(), 1, "the download goes on");
    assert!(!api.hung_up(), "nobody dropped Podman's answer");
}

/// Until the pull job of [`ENGINE`] is recorded `done`.
async fn pull_done(f: &Fixture) {
    let mut wait = common::patience::Wait::new("the pull recorded done");
    loop {
        let row = store::latest_job_by_key(&f.state.db, "image_pull", &format!("image:{ENGINE}"))
            .await
            .unwrap();
        if row.as_ref().is_some_and(|r| r.status == "done") {
            return;
        }
        let status = row.map(|r| r.status);
        wait.again(status.as_ref().map(|s| s as &dyn std::fmt::Display))
            .await;
    }
}

async fn set_admission(f: &Fixture, on: bool) {
    let mut s = f.state.snapshot().settings.clone();
    s.vram.enabled = on;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// Delete local model `model_id`'s row, the way the Models page does.
async fn delete_row(f: &Fixture, model_id: &str) {
    let id = f
        .state
        .snapshot()
        .local_models
        .iter()
        .find(|m| m.model_id == model_id)
        .unwrap()
        .id;
    store::delete_local_model(&f.state.db, id).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// Set local model `model_id`'s own image.
async fn set_row_image(f: &Fixture, model_id: &str, image: &str) {
    let row = f
        .state
        .snapshot()
        .local_models
        .iter()
        .find(|m| m.model_id == model_id)
        .cloned()
        .unwrap();
    store::update_local_model(
        &f.state.db,
        row.id,
        &NewLocalModel {
            model_id: row.model_id,
            gguf_path: row.gguf_path,
            params: row.params,
            args: row.args,
            idle_seconds: row.idle_seconds,
            enabled: row.enabled,
            public: row.public,
            image: Some(image.to_string()),
            extra_run_args: row.extra_run_args,
            warm_start: row.warm_start,
            fallback: row.fallback,
            capabilities_override: row.capabilities_override,
            ladder: row.ladder,
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
}

async fn guest_chat(gw: &Gw) -> reqwest::Response {
    gw.client()
        .post(format!("{gw}/v1/chat/completions"))
        .header("x-lmgw-background", "on")
        .json(&json!({"model": "chat-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
}

/// The hold switched on while a request waits for its engine's download:
/// the request is refused at once with `gpu_hold`, nothing starts, and the
/// download goes on to the end.
#[tokio::test]
async fn the_hold_switched_on_mid_download_refuses_the_request_and_the_download_goes_on() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (api, release) = scripted(&f, None);
    chat_engine_missing(&f).await;

    let gw = f.gateway.clone();
    let req = tokio::spawn(async move { chat(&gw).await });
    downloading(&f).await;
    engage_hold(&f).await;
    let resp = tokio::time::timeout(Duration::from_secs(10), req)
        .await
        .expect("answered while the download still runs")
        .unwrap();
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_hold", "{body}");
    still_pulling(&f, &api);
    assert_eq!(vram_status(&f.gateway).await["downloading"], json!([]));

    release.send(true).unwrap();
    pull_done(&f).await;
    assert!(f.runs().is_empty(), "nothing started under the hold");
}

/// Admission off, a start nobody waits on (a warm start, `lmgw__container
/// start`, a group apply): the hold switched on during its download is
/// `Fit::Held`, not `Unchecked` — nothing goes on the card the owner took
/// back.
#[tokio::test]
async fn with_admission_off_a_background_start_held_mid_download_is_held() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    set_admission(&f, false).await;
    let _reg = registry(&f).await;
    let (api, release) = scripted(&f, None);
    chat_engine_missing(&f).await;

    let state = f.state.clone();
    let checking = tokio::spawn(async move {
        let snap = state.snapshot();
        match state
            .vram
            .check_background_start(&state, &snap, Class::Chat, "chat-model")
            .await
        {
            Fit::Held(why) => Ok(why),
            Fit::Go(_) => Err("go"),
            Fit::Full(_) => Err("full"),
            Fit::Failed(_) => Err("failed"),
            Fit::Downloaded => Err("downloaded"),
            Fit::Unchecked => Err("unchecked"),
        }
    });
    downloading(&f).await;
    engage_hold(&f).await;
    let why = tokio::time::timeout(Duration::from_secs(10), checking)
        .await
        .expect("answered while the download still runs")
        .unwrap()
        .expect("held");
    assert!(why.contains("'chat-model' is not started while"), "{why}");
    still_pulling(&f, &api);

    release.send(true).unwrap();
    pull_done(&f).await;
    assert!(f.runs().is_empty());
}

/// `lmgw__container start` reads the row again after the download: a row
/// deleted while it ran is not started from the stale snapshot.
#[tokio::test]
async fn a_container_start_reads_its_row_again_after_the_download() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (_api, release) = scripted(&f, None);
    chat_engine_missing(&f).await;

    let state = f.state.clone();
    let starting = tokio::spawn(async move {
        lmgw_core::ops::container(&state, None, Some("chat-model"), "start", false, None)
            .await
            .map(|_| ())
    });
    downloading(&f).await;
    delete_row(&f, "chat-model").await;
    release.send(true).unwrap();
    let err = starting.await.unwrap().expect_err("the row is gone");
    assert!(
        err.contains("no chat model 'chat-model' is configured"),
        "{err}"
    );
    assert!(f.runs().is_empty());
}

/// A request's admission reads the row again after the download, like a
/// start over after a failed climb: gone, it is the plain 502.
#[tokio::test]
async fn a_request_whose_row_went_during_the_download_is_told_so() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (_api, release) = scripted(&f, None);
    chat_engine_missing(&f).await;

    let gw = f.gateway.clone();
    let req = tokio::spawn(async move { chat(&gw).await });
    downloading(&f).await;
    delete_row(&f, "chat-model").await;
    release.send(true).unwrap();
    let resp = req.await.unwrap();
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains(
            "chat model 'chat-model' is no longer configured, so it could not be started after \
             its engine image downloaded"
        ),
        "{msg}"
    );
    assert!(f.runs().is_empty());
}

/// A request with a fallback whose room turns into VRAM outside lmgw's
/// control while its engine downloads: the fallback answers (§4.7) without
/// waiting for the rest of the download, which goes on.
#[tokio::test]
async fn outside_use_during_the_download_sends_the_request_to_its_fallback() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (api, release) = scripted(&f, None);
    chat_engine_missing(&f).await;

    let state = f.state.clone();
    let admitting = tokio::spawn(async move {
        let route = state.snapshot().resolve("chat-model").unwrap();
        lmgw_core::vram::admit_or_external(
            &state,
            &route,
            "chat-model",
            Some(ExternalFallback::usable()),
        )
        .await
        .map(|a| match a {
            Admission::External(short) => Some(short.outside_bytes),
            Admission::Admitted(_) => None,
        })
    });
    downloading(&f).await;
    // A game takes 3 GiB: with every lmgw model gone 5 GiB would be free,
    // and the chat model needs 6.5.
    f.attribute(3 * GIB);
    let outside = tokio::time::timeout(Duration::from_secs(10), admitting)
        .await
        .expect("answered while the download still runs")
        .unwrap()
        .expect("a verdict, not a refusal");
    assert_eq!(outside, Some(3 * GIB), "the fallback answers");
    still_pulling(&f, &api);
    release.send(true).unwrap();
    pull_done(&f).await;
    assert!(f.runs().is_empty());
}

/// Background traffic never waits for a download: a guest's start begins
/// it and is deferred at once (no fallback here), the download goes on,
/// and once it is done the next guest starts the model.
#[tokio::test]
async fn a_guest_does_not_wait_for_the_download_and_a_later_guest_starts_the_model() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (api, release) = scripted(&f, None);
    chat_engine_missing(&f).await;

    let resp = tokio::time::timeout(Duration::from_secs(10), guest_chat(&f.gateway))
        .await
        .expect("answered without waiting for the download");
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_deferred", "{body}");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains(&format!("its engine image {ENGINE} is still downloading")),
        "{msg}"
    );
    assert_eq!(api.calls(), vec![ENGINE.to_string()], "the download began");
    still_pulling(&f, &api);
    assert_eq!(
        vram_status(&f.gateway).await["downloading"],
        json!([]),
        "a guest holds no wait"
    );
    assert!(f.runs().is_empty());

    // A second guest meanwhile joins the same download, and does not wait
    // either.
    assert_eq!(guest_chat(&f.gateway).await.status(), 503);
    assert_eq!(api.calls().len(), 1, "one download");

    release.send(true).unwrap();
    pull_done(&f).await;
    assert_eq!(guest_chat(&f.gateway).await.status(), 200);
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
    let argv = f.world().argv["chat-model"].clone();
    assert!(argv.iter().any(|a| a == "--pull=never"), "{argv:?}");
    assert_eq!(api.calls().len(), 1);
}

/// A guest whose model has a fallback: the fallback answers while the
/// engine downloads, as for every other reason a guest cannot start it.
#[tokio::test]
async fn a_guests_fallback_answers_while_the_engine_downloads() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (api, _release) = scripted(&f, None);
    chat_engine_missing(&f).await;
    let _cloud = cloud_upstream(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;

    let resp = tokio::time::timeout(Duration::from_secs(10), guest_chat(&f.gateway))
        .await
        .expect("answered without waiting for the download");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("cloud-chat")
    );
    still_pulling(&f, &api);
    assert!(f.runs().is_empty());
}

/// A client that hangs up mid-download takes its wait with it — the entry
/// leaves the view — and not the download: the next request joins the same
/// job and is answered once it is done.
#[tokio::test]
async fn a_client_hanging_up_mid_download_leaves_the_download_to_the_next_request() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (api, release) = scripted(&f, None);
    chat_engine_missing(&f).await;

    let gw = f.gateway.clone();
    let first = tokio::spawn(async move { chat(&gw).await.status() });
    downloading(&f).await;
    first.abort();
    let _ = first.await;
    until_vram(&f, "the hung-up wait gone from the view", |v| {
        v["downloading"] == json!([])
    })
    .await;
    still_pulling(&f, &api);

    let gw = f.gateway.clone();
    let second = tokio::spawn(async move { chat(&gw).await.status() });
    downloading(&f).await;
    assert_eq!(api.calls().len(), 1, "it joined the download under way");
    release.send(true).unwrap();
    assert_eq!(second.await.unwrap(), 200);
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
    assert_eq!(api.calls().len(), 1);
}

/// A request sent back by a failed climb starts over from the row as it is
/// now (ladder design §12 entry 51) — and when that row names an image
/// that is not on this machine, it downloads it before it starts the base.
#[tokio::test]
async fn a_request_sent_back_by_a_failed_climb_downloads_the_image_the_row_names_now() {
    let f = ladder_fixture(16 * GIB, 0).await;
    let _reg = registry(&f).await;
    let (api, release) = scripted(&f, None);
    let holder = ladder_hold(&f).await;
    f.world().fail_run_file = Some("ladder-top.gguf".into());
    let (open, gate) = tokio::sync::watch::channel(false);
    *f.podman.gate.lock().unwrap() = Some(gate);

    let climbing = spawn_climb(&f, holder, 2);
    until_ladder(&f, "the new rung loading", |v| {
        v.state.as_str() == "starting"
    })
    .await;
    let waiting = tokio::spawn({
        let state = f.state.clone();
        async move {
            lmgw_core::gate::open(
                &state,
                LADDER,
                lmgw_core::gate::RouteCheck::Text("/v1/chat/completions"),
                lmgw_core::runtime::registry::Origin::Owner,
            )
            .await
            .map(|o| o.hold.is_some())
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished(), "parked on the climb");
    // Meanwhile the owner moves the row to an engine this machine lacks.
    set_row_image(&f, LADDER, ENGINE).await;
    f.world().missing_images.insert(ENGINE.into());
    open.send_replace(true);

    let (climbed, _holder) = climbing.await.unwrap();
    assert_eq!(climbed.unwrap_err().http_status(), 502);
    until_vram(&f, "the start over downloading", |v| {
        v["downloading"][0]["model"] == LADDER
    })
    .await;
    assert!(!waiting.is_finished(), "waiting for its engine");
    release.send(true).unwrap();
    assert!(waiting.await.unwrap().expect("admitted after the download"));
    assert_eq!(
        ladder_runs(&f),
        vec!["ladder-base.gguf", "ladder-top.gguf", "ladder-base.gguf"]
    );
    let argv = f.world().argv[LADDER].clone();
    assert!(argv.iter().any(|a| a == ENGINE), "{argv:?}");
    assert!(argv.iter().any(|a| a == "--pull=never"), "{argv:?}");
    assert_eq!(api.calls(), vec![ENGINE.to_string()]);
}

/// A pull whose outcome could not be recorded leaves its row `running` with
/// no live job behind it. A start that joins that row finds the image on
/// the machine and goes on at once, instead of waiting out the orphan
/// bound and telling the owner to cancel a pull that already succeeded.
#[tokio::test]
async fn a_pull_row_left_running_answers_once_the_image_is_here() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let key = format!("image:{ENGINE}");
    let id = store::claim_job(&f.state.db, "image_pull", Some(&key), "pull", "{}")
        .await
        .unwrap()
        .expect("the key is free");
    store::start_job(&f.state.db, id).await.unwrap();
    // The image is on the machine: the fake store does not list it missing.
    let began = Instant::now();
    tokio::time::timeout(
        Duration::from_secs(10),
        pull::pull_and_wait(&f.state, ENGINE, |_| {}),
    )
    .await
    .expect("not the 30 s orphan wait")
    .expect("the image is here");
    assert!(began.elapsed() < Duration::from_secs(10));
}

/// `--pull=never` goes right after `run` — after Podman's global options
/// before it — and only a `run` takes it.
#[test]
fn never_pull_follows_the_run_verb() {
    let argv = never_pull(["run", "-d", "img"].map(String::from).to_vec());
    assert_eq!(argv, ["run", "--pull=never", "-d", "img"]);
    let argv = never_pull(
        ["--cdi-spec-dir", "/x/cdi", "run", "-d", "img"]
            .map(String::from)
            .to_vec(),
    );
    assert_eq!(
        argv,
        [
            "--cdi-spec-dir",
            "/x/cdi",
            "run",
            "--pull=never",
            "-d",
            "img"
        ]
    );
}

#[test]
#[should_panic(expected = "with its verb")]
fn never_pull_refuses_anything_but_a_run() {
    never_pull(["pull", "img"].map(String::from).to_vec());
}
