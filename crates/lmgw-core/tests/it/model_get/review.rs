//! WP5's review round (find-a-model design D81 to D89, revised): what a get
//! shares with others and what it stops, a waiting get that a row's change
//! outside any transfer has to reach, two gets at once, a request dropped
//! mid-way, the status read, the names an upstream passes through, the
//! context settled again at the create, Fit again's confirm against a card
//! that moves and on an aux row, and the aux re-plan's rounds.

use std::sync::atomic::{AtomicUsize, Ordering};

use lmgw_api_types::model_get::{
    GetWaiting, ModelGetStage, ModelGetState, ModelGetStatus, WaitReason,
};
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::store::NewUpstream;

use super::*;

const WEIGHTS2: &str = "demo-chat-Q8_0.gguf";

/// The chat repo with a second weights file: both take the same projector
/// and drafter.
fn two_weights_repo() -> Repo<'static> {
    let mut repo = chat_repo();
    repo.files
        .push((WEIGHTS2, chat_header(131_072, 4, 3 * MIB).bytes()));
    repo
}

async fn status(gw: &Gw, get_id: i64) -> ModelGetStatus {
    let (code, live) = op(gw, "model_get_status", json!({ "get_id": get_id })).await;
    assert_eq!(code, 200, "{live}");
    common::round_trips::<ModelGetStatus>("POST /api/op/model_get_status", &live)
}

/// The download row of `file` in `repo`.
async fn download_of(state: &SharedState, file: &str) -> store::HfModelRow {
    store::list_hf_models(&state.db)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.file == file)
        .unwrap_or_else(|| panic!("no download row of {file}"))
}

fn aux_row(model_id: &str, gguf_path: &str) -> store::NewAuxModel {
    store::NewAuxModel {
        model_id: model_id.into(),
        gguf_path: gguf_path.into(),
        kind: lmgw_core::config::AuxKind::Embed,
        pooling: None,
        ctx_size: None,
        ubatch_size: None,
        batch_size: None,
        mmproj_path: None,
        backend: None,
        threads: None,
        args: vec![],
        idle_seconds: 0,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        fallback: None,
    }
}

async fn set_budget(state: &SharedState, mib: u64) {
    let mut settings = state.snapshot().settings.clone();
    settings.vram.budget_mb = mib;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

/// A get waiting on a download that nothing transfers (paused on a metered
/// connection): a get set up as the boot leaves one, its row `queued` with
/// the pause's note and no job.
async fn waiting_get(w: &World) -> (i64, i64) {
    let dest = format!("{REPO}/{WEIGHTS}");
    let id = store::upsert_hf_model(&w.state.db, REPO, WEIGHTS, &dest, "chat")
        .await
        .unwrap();
    store::set_hf_status(
        &w.state.db,
        id,
        "queued",
        Some(hf_download::PAUSED_METERED_NOTE),
    )
    .await
    .unwrap();
    let get_id = store::insert_model_get(
        &w.state.db,
        &store::NewModelGet {
            repo: REPO,
            file: WEIGHTS,
            target: "chat",
            measure: "beside",
            model_id: "demo-chat",
            kind: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    store::set_model_get_downloads(&w.state.db, get_id, &[id], &[id])
        .await
        .unwrap();
    model_get::start(&w.state, get_id, REPO, WEIGHTS)
        .await
        .unwrap();
    // It says it waits, and why.
    let gw = w.gw.clone();
    patience::until_async("the get to say it is paused", || {
        let gw = gw.clone();
        async move { status(&gw, get_id).await.stage == Some(ModelGetStage::Paused) }
    })
    .await;
    // The real pause's note, typed on the wire (WP7, D108): the file and
    // the reason, the note as the Downloads page shows it.
    let s = status(&w.gw, get_id).await;
    assert_eq!(
        s.waiting,
        [GetWaiting {
            file: WEIGHTS.into(),
            reason: WaitReason::Metered,
            note: Some(hf_download::PAUSED_METERED_NOTE.into()),
        }],
        "{s:?}"
    );
    (get_id, id)
}

/// Why a row nothing transfers waits, by its note (WP7, D108): the boot's
/// metered pause is `metered` by the server's own constant, no note is
/// `not_started`, and any other note is `other`, never parsed.
#[test]
fn a_waiting_rows_note_is_its_reason() {
    assert_eq!(
        model_get::wait_reason(Some(hf_download::PAUSED_METERED_NOTE)),
        WaitReason::Metered
    );
    assert_eq!(model_get::wait_reason(None), WaitReason::NotStarted);
    assert_eq!(
        model_get::wait_reason(Some("Paused: metered; but another sentence")),
        WaitReason::Other
    );
}

/// A Cancel of the paused row, and an untrack of it, reach the get waiting
/// for it: neither is a transfer's end, so each publishes its own frame.
#[tokio::test]
async fn a_waiting_get_sees_its_paused_download_cancelled_or_untracked() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let (_hub, _cdn_server, _cdn) = hub(&[chat_repo()], Cdn::default()).await;

    let (get_id, id) = waiting_get(&w).await;
    let (code, body) = op(&w.gw, "hf_set", json!({"action": "cancel", "id": id})).await;
    assert_eq!(code, 200, "{body}");
    let (row, _, _) = ended(&w.state, get_id).await;
    assert_eq!(row.status, "canceled");
    assert_eq!(status(&w.gw, get_id).await.state, ModelGetState::Canceled);

    let (get_id, id) = waiting_get(&w).await;
    let (code, body) = op(&w.gw, "hf_set", json!({"action": "delete", "id": id})).await;
    assert_eq!(code, 200, "{body}");
    let (row, _, _) = ended(&w.state, get_id).await;
    assert_eq!(row.status, "failed");
    let said = row.error.unwrap();
    assert!(said.contains("removed from the Downloads page"), "{said}");
    assert!(said.contains("Get it again"), "{said}");
    assert!(w.state.snapshot().local_models.is_empty());
}

/// Two gets of one repo share the projector and the drafter: the first
/// started those downloads, the second joined them. The first's cancel
/// stops only what the other doesn't wait for, so the second finishes with
/// the shared files whole.
#[tokio::test]
async fn a_cancelled_get_leaves_the_downloads_another_get_shares() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    // The shared files slow enough that the second get queues while the
    // first's transfers of them still run, on a loaded machine too.
    let cdn = Cdn {
        delay: Some(Duration::from_millis(1500)),
        slow: Arc::new(vec![
            (PROJ_F16.to_string(), Duration::from_secs(15)),
            (DRAFTER.to_string(), Duration::from_secs(15)),
            (WEIGHTS.to_string(), Duration::from_secs(15)),
        ]),
        ..Cdn::default()
    };
    let (_hub, _cdn_server, _cdn) = hub(&[two_weights_repo()], cdn).await;
    let first = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS, "target": "chat"}),
    )
    .await;
    let second = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS2, "target": "chat"}),
    )
    .await;
    assert_ne!(first.get_id, second.get_id);
    assert_eq!(
        second.model_id, "demo-chat-2",
        "the first get holds the name"
    );
    let proj = download_of(&w.state, PROJ_F16).await.id;
    let (own_weights, other_weights) = (
        download_of(&w.state, WEIGHTS).await.id,
        download_of(&w.state, WEIGHTS2).await.id,
    );
    let a = store::get_model_get(&w.state.db, first.get_id.unwrap())
        .await
        .unwrap()
        .unwrap();
    let b = store::get_model_get(&w.state.db, second.get_id.unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(a.queued_ids.contains(&proj) && a.queued_ids.contains(&own_weights));
    assert!(b.download_ids.contains(&proj), "{b:?}");
    assert_eq!(
        b.queued_ids,
        vec![other_weights],
        "it joined the shared ones"
    );

    lmgw_core::jobs::cancel(&w.state, first.job_id.unwrap())
        .await
        .unwrap();
    let (row, _, _) = ended(&w.state, first.get_id.unwrap()).await;
    assert_eq!(row.status, "canceled");
    let (row, done, error) = ended(&w.state, second.get_id.unwrap()).await;
    assert_eq!(row.status, "done", "{error:?}");
    assert!(done.unwrap().created);
    let weights = download_of(&w.state, WEIGHTS).await;
    assert_eq!(
        (weights.status.as_str(), weights.error.as_deref()),
        ("failed", Some(hf_download::CANCELED_NOTE)),
        "the cancelled get's own weights stopped"
    );
    assert_eq!(download_of(&w.state, PROJ_F16).await.status, "done");
    assert!(w
        .dir
        .path()
        .join("chat")
        .join(REPO)
        .join(PROJ_F16)
        .is_file());
    let snap = w.state.snapshot();
    assert_eq!(snap.local_models.len(), 1);
    assert_eq!(snap.local_models[0].model_id, "demo-chat-2");
}

/// A get's own Cancel stops it and its downloads; a Cancel of one of its
/// downloads cancels the get.
#[tokio::test]
async fn a_get_ends_cancelled_by_its_own_cancel_or_a_downloads() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    // No file arrives before the cancels, on a loaded machine too.
    let cdn = Cdn {
        delay: Some(Duration::from_secs(20)),
        ..Cdn::default()
    };
    let (_hub, _cdn_server, _cdn) = hub(&[chat_repo()], cdn).await;

    let started = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS, "target": "chat"}),
    )
    .await;
    lmgw_core::jobs::cancel(&w.state, started.job_id.unwrap())
        .await
        .unwrap();
    let (row, _, _) = ended(&w.state, started.get_id.unwrap()).await;
    assert_eq!(row.status, "canceled");
    let db = w.state.db.clone();
    patience::until_async("the get's downloads to stop", || {
        let db = db.clone();
        async move {
            store::list_hf_models(&db).await.unwrap().iter().all(|d| {
                d.status == "failed" && d.error.as_deref() == Some(hf_download::CANCELED_NOTE)
            })
        }
    })
    .await;

    let again = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS, "target": "chat"}),
    )
    .await;
    assert!(!again.joined);
    let proj = download_of(&w.state, PROJ_F16).await.id;
    let (code, body) = op(&w.gw, "hf_set", json!({"action": "cancel", "id": proj})).await;
    assert_eq!(code, 200, "{body}");
    let (row, _, _) = ended(&w.state, again.get_id.unwrap()).await;
    assert_eq!(row.status, "canceled");
    assert!(w.state.snapshot().local_models.is_empty());
}

/// Two Get its of one file at once are one get: one queues, the other
/// joins it, and every file is downloaded once.
#[tokio::test]
async fn two_gets_of_one_file_at_once_are_one_get() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let (_hub, _cdn_server, cdn) = hub(&[chat_repo()], Cdn::default()).await;
    let args = json!({"repo": REPO, "file": WEIGHTS, "target": "chat"});
    let (a, b) = tokio::join!(get(&w.gw, args.clone()), get(&w.gw, args));
    assert_eq!(a.get_id, b.get_id);
    assert_eq!(a.model_id, b.model_id);
    assert!(a.joined != b.joined, "one queued, one joined: {a:?} {b:?}");
    let (row, _, error) = ended(&w.state, a.get_id.unwrap()).await;
    assert_eq!(row.status, "done", "{error:?}");
    assert_eq!(w.state.snapshot().local_models.len(), 1);
    assert_eq!(store::list_hf_models(&w.state.db).await.unwrap().len(), 3);
    let whole: Vec<_> = cdn
        .ranges_of(WEIGHTS)
        .into_iter()
        .filter(Option::is_none)
        .collect();
    assert_eq!(whole.len(), 1, "the weights downloaded once");
}

/// A request dropped while the op queues the files (its client gone): the
/// get is written first and the op runs on as a task of its own, so the
/// downloads are its and it ends with the row.
#[tokio::test]
async fn a_dropped_request_still_leaves_a_whole_get() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let (hub_server, _cdn_server, _cdn) = hub(&[chat_repo()], Cdn::default()).await;
    // The listing as the hub gives it, then every later listing slow: the
    // op's second one (the queue's) outlasts the client.
    let tree = format!("{}/api/models/{REPO}/tree/main", hub_server.uri());
    let listing: Value = reqwest::get(&tree).await.unwrap().json().await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    Mock::given(method("GET"))
        .and(path(format!("/api/models/{REPO}/tree/main")))
        .respond_with(move |_: &Request| {
            let answer = ResponseTemplate::new(200).set_body_json(listing.clone());
            match seen.fetch_add(1, Ordering::SeqCst) {
                0 => answer,
                _ => answer.set_delay(Duration::from_secs(6)),
            }
        })
        .with_priority(1)
        .mount(&hub_server)
        .await;

    let sent =
        w.gw.client()
            .post(format!("{}/api/op/model_get", w.gw))
            .timeout(Duration::from_millis(2500))
            .json(&json!({"repo": REPO, "file": WEIGHTS, "target": "chat"}))
            .send()
            .await;
    assert!(sent.is_err(), "the client gave up first: {sent:?}");
    assert!(calls.load(Ordering::SeqCst) >= 2, "it was queueing");

    let db = w.state.db.clone();
    patience::until_async("the get's row", || {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM model_gets")
                .fetch_one(&db)
                .await
                .unwrap()
                == 1
        }
    })
    .await;
    let get_id: i64 = sqlx::query_scalar("SELECT id FROM model_gets")
        .fetch_one(&w.state.db)
        .await
        .unwrap();
    let (row, done, error) = ended(&w.state, get_id).await;
    assert_eq!(row.status, "done", "{error:?}");
    assert_eq!(row.download_ids.len(), 3, "{row:?}");
    assert!(done.unwrap().created);
    let s = status(&w.gw, get_id).await;
    assert_eq!(s.state, ModelGetState::Done);
    assert_eq!(s.row_id, Some(w.state.snapshot().local_models[0].id));
}

/// The status read follows a get to its end: running with its stage while
/// it downloads, done with the row it made and the result.
#[tokio::test]
async fn the_status_read_says_how_a_get_stands_and_the_row_it_made() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let cdn = Cdn {
        delay: Some(Duration::from_secs(3)),
        ..Cdn::default()
    };
    let (_hub, _cdn_server, _cdn) = hub(&[chat_repo()], cdn).await;
    let started = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS, "target": "chat"}),
    )
    .await;
    let get_id = started.get_id.unwrap();
    let now = status(&w.gw, get_id).await;
    assert_eq!(now.state, ModelGetState::Running);
    assert_eq!(now.job_id, started.job_id);
    assert_eq!(now.downloads.len(), 3);
    let (_, done, _) = ended(&w.state, get_id).await;
    let done = done.unwrap();
    let after = status(&w.gw, get_id).await;
    assert_eq!(after.state, ModelGetState::Done);
    assert_eq!(after.row_id, Some(done.row_id));
    assert_eq!(after.model_id, done.model_id);
    assert_eq!(after.result.as_ref(), Some(&done));
    let (code, _) = op(&w.gw, "model_get_status", json!({"get_id": get_id + 99})).await;
    assert_ne!(code, 200, "no such get");
}

/// A row taking the get's name while it downloads: the create settles the
/// id again (D28), and the status read says the one it got.
#[tokio::test]
async fn a_name_taken_during_the_download_is_suffixed_at_the_create() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let cdn = Cdn {
        delay: Some(Duration::from_secs(5)),
        ..Cdn::default()
    };
    let (_hub, _cdn_server, _cdn) = hub(&[embed_repo()], cdn).await;
    let started = get(&w.gw, json!({"repo": EMBED_REPO, "file": EMBED})).await;
    assert_eq!(started.model_id, "demo-embedder");
    embedder().write_to(&w.dir.path().join("aux/x/e.gguf"));
    store::insert_aux_model(&w.state.db, &aux_row("demo-embedder", "x/e.gguf"))
        .await
        .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let get_id = started.get_id.unwrap();
    let (row, done, error) = ended(&w.state, get_id).await;
    assert_eq!(row.status, "done", "{error:?}");
    let done = done.unwrap();
    assert!(done.created);
    assert_eq!(done.model_id, "demo-embedder-2");
    assert_eq!(done.public_name, "embed/demo-embedder-2");
    assert_eq!(status(&w.gw, get_id).await.model_id, "demo-embedder-2");
}

/// A `model_id` an upstream passes through is refused, naming it: under a
/// prefix (no suffix gets out from under it) and one a bare upstream lists.
#[tokio::test]
async fn a_given_id_an_upstream_passes_through_is_refused() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let (_hub, _cdn_server, _cdn) = hub(&[chat_repo()], Cdn::default()).await;
    let provider = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"object": "list", "data": [{"id": "demo-cloud-model"}]})),
        )
        .mount(&provider)
        .await;
    for (name, prefix) in [("demo-prefixed", "demo-cloud"), ("demo-bare", "")] {
        store::insert_upstream(
            &w.state.db,
            &NewUpstream {
                name: name.into(),
                protocol: Protocol::OpenaiCompatible,
                kind: UpstreamKind::Generic,
                base_url: format!("{}/v1", provider.uri()),
                api_key: None,
                extra_headers: vec![],
                timeout_ms: 5_000,
                enabled: true,
                expose_all: true,
                expose_prefix: prefix.into(),
                supports_responses: false,
                catalog_all_modalities: true,
            },
        )
        .await
        .unwrap();
    }
    w.state.reload_snapshot().await.unwrap();
    for (id, upstream) in [
        ("demo-cloud/some-model", "demo-prefixed"),
        ("demo-cloud-model", "demo-bare"),
    ] {
        let (code, body) = op(
            &w.gw,
            "model_get",
            json!({"repo": REPO, "file": WEIGHTS, "target": "chat", "model_id": id}),
        )
        .await;
        assert_ne!(code, 200, "{body}");
        let said = body.to_string();
        assert!(
            said.contains(upstream) && said.contains("passes through"),
            "{said}"
        );
    }
    assert!(store::list_hf_models(&w.state.db).await.unwrap().is_empty());
    assert!(store::running_model_gets(&w.state.db)
        .await
        .unwrap()
        .is_empty());
}

/// Fit again's confirm against a card that moves: the budget a little
/// larger than when it was shown still saves what was shown; a budget the
/// shown context no longer fits answers `outdated` and writes nothing; and
/// a row changed under the confirm is `outdated` too. A chat row's batches
/// never move with it.
#[tokio::test]
async fn fit_again_saves_what_was_shown_while_it_still_fits() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let (_hub, _cdn_server, _cdn) = hub(&[chat_repo()], Cdn::default()).await;
    let started = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS, "target": "chat"}),
    )
    .await;
    let (_, done, error) = ended(&w.state, started.get_id.unwrap()).await;
    let fitted = done
        .unwrap_or_else(|| panic!("{error:?}"))
        .ctx_size
        .unwrap();
    let ask = json!({"target": "chat", "model_id": "demo-chat"});
    let fit_again = |confirm: Option<Vec<FitChange>>| {
        let mut args = ask.clone();
        if let Some(c) = confirm {
            args["confirm"] = json!(c);
        }
        let gw = w.gw.clone();
        async move {
            let (code, live) = op(&gw, "model_fit_again", args).await;
            assert_eq!(code, 200, "{live}");
            serde_json::from_value::<ModelFitAgain>(live).unwrap()
        }
    };

    // Shown at 170 MiB; confirmed at 172: saved, what was shown.
    set_budget(&w.state, 170).await;
    let shown = fit_again(None).await;
    assert_eq!(shown.changes.len(), 1, "the context alone: {shown:?}");
    let to = shown.changes[0].to.unwrap();
    assert!(to > fitted, "{shown:?}");
    set_budget(&w.state, 172).await;
    let saved = fit_again(Some(shown.changes.clone())).await;
    assert!(saved.saved && !saved.outdated, "{saved:?}");
    let row = w.state.snapshot().local_models[0].clone();
    assert_eq!(row.params.ctx_size, Some(to as i64));
    assert_eq!(saved.note.as_ref().map(|n| n.ctx), Some(to));

    // Shown at 180; the card shrinks to 160 before the confirm: outdated.
    set_budget(&w.state, 180).await;
    let shown = fit_again(None).await;
    let bigger = shown.changes[0].to.unwrap();
    set_budget(&w.state, 160).await;
    let late = fit_again(Some(shown.changes.clone())).await;
    assert!(late.outdated && !late.saved, "{late:?}");
    assert!(
        late.sentence.contains("no longer fits"),
        "{}",
        late.sentence
    );
    assert_eq!(
        w.state.snapshot().local_models[0].params.ctx_size,
        Some(to as i64),
        "nothing written"
    );
    assert!(bigger > to);

    // The row's context edited after the offer was shown: outdated.
    set_budget(&w.state, 180).await;
    let shown = fit_again(None).await;
    let (code, body) = op(
        &w.gw,
        "local_model_set",
        json!({"action": "update", "id": row.id, "ctx_size": 2048}),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    let late = fit_again(Some(shown.changes)).await;
    assert!(late.outdated && !late.saved, "{late:?}");
    assert!(
        late.sentence.contains("settings changed"),
        "{}",
        late.sentence
    );
    assert_eq!(
        w.state.snapshot().local_models[0].params.ctx_size,
        Some(2048)
    );
    assert_eq!(
        w.state.snapshot().local_models[0].params.batch_size,
        row.params.batch_size,
        "a chat row's batch stays"
    );
}

/// Fit again on an aux row: its ubatch and batch, tied to its context, move
/// with it, and the note goes once the whole context fits. The note is on
/// the row's read.
#[tokio::test]
async fn fit_again_moves_an_aux_rows_tied_batches_with_its_context() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let (_hub, _cdn_server, _cdn) = hub(&[embed_repo()], Cdn::default()).await;
    let started = get(&w.gw, json!({"repo": EMBED_REPO, "file": EMBED})).await;
    let (row, _, error) = ended(&w.state, started.get_id.unwrap()).await;
    assert_eq!(row.status, "done", "{error:?}");
    let read = |gw: Gw| async move {
        let r: Value = gw
            .client()
            .get(format!(
                "{gw}/api/local-model?target=aux&model_id=demo-embedder"
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        r
    };
    let before = read(w.gw.clone()).await;
    let fitted = before["ctx_size"].as_u64().unwrap();
    assert_eq!(before["fit_note"]["ctx"].as_u64(), Some(fitted), "{before}");

    set_budget(&w.state, 4096).await;
    let ask = json!({"target": "aux", "model_id": "demo-embedder"});
    let (_, live) = op(&w.gw, "model_fit_again", ask.clone()).await;
    let shown: ModelFitAgain = serde_json::from_value(live).unwrap();
    let keys: Vec<&str> = shown.changes.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(keys, ["ctx_size", "ubatch_size", "batch_size"], "{shown:?}");
    assert!(
        shown.changes.iter().all(|c| c.to == Some(32_768)),
        "{shown:?}"
    );
    let mut confirm = ask.clone();
    confirm["confirm"] = json!(shown.changes);
    let (_, live) = op(&w.gw, "model_fit_again", confirm).await;
    let saved: ModelFitAgain = serde_json::from_value(live).unwrap();
    assert!(saved.saved, "{saved:?}");
    let snap = w.state.snapshot();
    let aux = snap
        .aux_models
        .iter()
        .find(|m| m.model_id == "demo-embedder")
        .unwrap();
    assert_eq!(
        (aux.ctx_size, aux.ubatch_size, aux.batch_size),
        (Some(32_768), Some(32_768), Some(32_768))
    );
    let after = read(w.gw.clone()).await;
    assert!(after["fit_note"].is_null(), "{after}");
}

/// A projector whose images need a larger ubatch than the context the card
/// fits: the re-plan at that context puts the projector's ubatch floor
/// back, and is sized again before it is taken. The floor costs nothing
/// more there (the compute buffer is charged at the ubatch the context
/// clamps it to), so the second sizing confirms the first fit's context:
/// the row keeps it with the floor above it, and Fit again neither moves
/// the context nor takes the floor down.
#[tokio::test]
async fn an_aux_replan_under_a_projector_floor_keeps_the_floor_at_the_fitted_context() {
    let _env = common::process_env_lock().await;
    // The first fit's context at 100 MiB is 1 075 tokens, under Gemma 4's
    // 1 280-token image floor.
    let budget = 100;
    let w = world(budget).await;
    let (_hub, _cdn_server, _cdn) = hub(&[embed_repo()], Cdn::default()).await;
    let from_hub = lmgw_core::modelinfo::local_model_plan_of(
        &w.state,
        None,
        Some(EMBED_REPO),
        Some(EMBED),
        Some("aux"),
    )
    .await
    .unwrap();
    let first = from_hub["fit"]["beside"]["ctx"].as_u64().unwrap();
    let started = get(&w.gw, json!({"repo": EMBED_REPO, "file": EMBED})).await;
    let (row, done, error) = ended(&w.state, started.get_id.unwrap()).await;
    assert_eq!(row.status, "done", "{error:?}");
    let done = done.unwrap();
    let snap = w.state.snapshot();
    let aux = snap
        .aux_models
        .iter()
        .find(|m| m.model_id == "demo-embedder")
        .unwrap();
    let ctx = aux.ctx_size.unwrap() as u64;
    let ubatch = aux.ubatch_size.unwrap() as u64;
    assert!(
        first < ubatch,
        "the first fit ({first}) is below the projector's floor ({ubatch})"
    );
    assert_eq!(ctx, first, "the floor fits at the first fit's context");
    assert_eq!(
        aux.batch_size, aux.ubatch_size,
        "the batch holds the floor too"
    );
    assert_eq!(done.ctx_size, Some(ctx));
    assert_eq!(done.note.as_ref().map(|n| n.ctx), Some(ctx));
    let (_, live) = op(
        &w.gw,
        "model_fit_again",
        json!({"target": "aux", "model_id": "demo-embedder"}),
    )
    .await;
    let again: ModelFitAgain = serde_json::from_value(live).unwrap();
    assert!(again.changes.is_empty(), "the row is the fit: {again:?}");

    // A larger card: the context grows past the floor, and the batches
    // with it.
    set_budget(&w.state, 4096).await;
    let (_, live) = op(
        &w.gw,
        "model_fit_again",
        json!({"target": "aux", "model_id": "demo-embedder"}),
    )
    .await;
    let grown: ModelFitAgain = serde_json::from_value(live).unwrap();
    assert!(
        grown.changes.iter().all(|c| c.to == Some(32_768)),
        "{grown:?}"
    );
    assert_eq!(grown.changes.len(), 3, "{grown:?}");
}
