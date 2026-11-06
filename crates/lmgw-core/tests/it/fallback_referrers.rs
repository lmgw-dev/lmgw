//! A write that changes what *another* row's stored fallback points at is
//! checked for that row too (openai egress design §14.2, decision 29: a
//! fallback of another type must not be settable, a loop is refused at
//! save): a create whose name a dangling fallback holds, a rename onto such
//! a name, an alias retargeted, an aux kind or audio task changed, a class
//! prefix changed, an alias enabled that shadows a local name. Each is
//! refused naming the row it would break; what stood before the write does
//! not refuse it. An upstream's new prefix rewrites the passthrough ids its
//! fallbacks name, and its delete names the rows it leaves without one.

use std::time::Duration;

use lmgw_core::state::AppState;
use lmgw_core::{ops, store, vram};
use serde_json::{json, Value};

use crate::support::fallback_world::{self, world, World};
use crate::support::fallbacks::set_fallback;
use crate::support::gpu_world::{Gpu, GIB};

fn patch<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).unwrap()
}

async fn names(w: &World, table: &str, owner: &str, fallback: &str) {
    set_fallback(&w.state, table, owner, Some(fallback)).await;
}

#[tokio::test]
async fn a_create_whose_name_a_dangling_fallback_holds_is_checked_for_it() {
    let w = world().await;
    names(&w, "aux_models", "bge", "x").await;
    let e = ops::model_set(
        &w.state,
        patch(
            json!({"action": "create", "alias": "x", "upstream": "cloud",
                     "upstream_model": "x-model",
                     "capabilities_override": {"capabilities": {"task": "chat",
                         "input_modalities": ["text"]}}}),
        ),
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("embedding model 'bge' falls back to 'x', which would be a chat model"),
        "{e}"
    );
    assert!(
        !w.state.snapshot().aliases.contains_key("x"),
        "nothing written"
    );
    // An embedder under that name is what the fallback asked for.
    ops::model_set(
        &w.state,
        patch(
            json!({"action": "create", "alias": "x", "upstream": "cloud",
                     "upstream_model": "x-model",
                     "capabilities_override": {"capabilities": {"task": "embedding",
                         "input_modalities": ["text"]}}}),
        ),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_rename_onto_a_dangling_fallback_that_closes_a_loop_is_refused() {
    let w = world().await;
    names(&w, "models", "cloud-mini", "x").await;
    names(&w, "models", "cloud-b", "cloud-mini").await;
    let id = w.id("models", "cloud-b").await;
    let e = ops::model_set(
        &w.state,
        patch(json!({"action": "update", "id": id, "alias": "x"})),
    )
    .await
    .unwrap_err();
    assert!(e.contains("would close a loop"), "{e}");
    assert!(e.contains("cloud-mini → x → cloud-mini"), "{e}");
    assert_eq!(
        w.fallback("models", "cloud-b").await.as_deref(),
        Some("cloud-mini")
    );
    assert!(
        w.state.snapshot().aliases.contains_key("cloud-b"),
        "not renamed"
    );
}

#[tokio::test]
async fn an_alias_retargeted_to_another_type_is_refused_for_its_referrers() {
    let w = world().await;
    names(&w, "local_models", "qwen3", "cloud-mini").await;
    let e = ops::model_set(
        &w.state,
        patch(json!({"action": "update", "alias": "cloud-mini",
                     "capabilities_override": {"capabilities": {"task": "embedding",
                         "input_modalities": ["text"]}}})),
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("chat model 'qwen3' falls back to 'cloud-mini', which would be an embedding model while it is a chat model"),
        "{e}"
    );
}

#[tokio::test]
async fn an_aux_kind_and_an_audio_task_change_are_refused_for_their_referrers() {
    let w = world().await;
    names(&w, "models", "cloud-embed", "embed/bge").await;
    let id = w.id("aux_models", "bge").await;
    let e = ops::aux_model_set(
        &w.state,
        patch(json!({"action": "update", "id": id, "kind": "rerank"})),
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("alias 'cloud-embed' falls back to 'embed/bge', which would be a reranker while it is an embedding model"),
        "{e}"
    );

    names(&w, "models", "cloud-asr", "audio/whisper").await;
    let id = w.id("audio_models", "whisper").await;
    let e = ops::audio_model_set(
        &w.state,
        patch(json!({"action": "update", "id": id, "task": "tts"})),
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("alias 'cloud-asr' falls back to 'audio/whisper', which would be a text-to-speech model while it is a speech-to-text model"),
        "{e}"
    );
}

#[tokio::test]
async fn an_alias_enabled_over_a_local_name_is_checked_for_who_named_it() {
    let w = world().await;
    // A disabled chat alias spelled like the embedder: it shadows nothing.
    crate::support::fallback_world::alias(
        &w.state,
        "embed/bge",
        w.cloud,
        Some(json!({"capabilities": {"task": "chat", "input_modalities": ["text"]}})),
    )
    .await;
    sqlx::query("UPDATE models SET enabled = 0 WHERE alias = 'embed/bge'")
        .execute(&w.state.db)
        .await
        .unwrap();
    names(&w, "models", "cloud-embed", "embed/bge").await;
    let e = ops::model_set(
        &w.state,
        patch(json!({"action": "enable", "alias": "embed/bge"})),
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("alias 'cloud-embed' falls back to 'embed/bge', which would be a chat model while it is an embedding model"),
        "{e}"
    );
}

#[tokio::test]
async fn a_class_prefix_that_gives_a_dangling_name_its_target_is_checked() {
    let w = world().await;
    names(&w, "local_models", "qwen3", "vec/bge").await;
    let gw = crate::common::serve(w.state.clone()).await;
    let r = gw
        .client()
        .post(format!("{gw}/api/op/settings_set_full"))
        .json(&json!({"aux_router": {"public_prefix": "vec"}}))
        .send()
        .await
        .unwrap();
    let status = r.status();
    let body = r.text().await.unwrap();
    assert_ne!(status, 200, "{body}");
    assert!(
        body.contains(
            "chat model 'qwen3' falls back to 'vec/bge', which would be an embedding model"
        ),
        "{body}"
    );
    assert_eq!(
        w.state.snapshot().settings.aux_router.public_prefix,
        "embed"
    );
}

/// A settings save the prefix dry run refuses creates nothing on disk
/// (review N3): the image class's LoRA and upscaler directories are made
/// only once the save goes through.
#[tokio::test]
async fn a_refused_prefix_save_leaves_no_image_directories() {
    let w = world().await;
    names(&w, "local_models", "qwen3", "vec/bge").await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("image-models");
    let gw = crate::common::serve(w.state.clone()).await;
    let save = |prefix: &'static str| {
        let (gw, root) = (&gw, root.display().to_string());
        async move {
            gw.client()
                .post(format!("{gw}/api/op/settings_set_full"))
                .json(&json!({"aux_router": {"public_prefix": prefix},
                              "image": {"models_dir": root}}))
                .send()
                .await
                .unwrap()
        }
    };
    let r = save("vec").await;
    assert_ne!(r.status(), 200, "{}", r.text().await.unwrap());
    assert!(!root.exists(), "nothing was created for a refused save");

    let r = save("embed").await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    assert!(root.join("loras").is_dir() && root.join("upscalers").is_dir());
}

#[tokio::test]
async fn a_mismatch_that_stood_before_does_not_refuse_an_unrelated_write() {
    let w = world().await;
    // Kept by migration 0095, say: an embedder falling back to a chat alias.
    names(&w, "aux_models", "bge", "cloud-chat").await;
    ops::model_set(
        &w.state,
        patch(json!({"action": "update", "alias": "cloud-chat", "enabled": true})),
    )
    .await
    .unwrap();
    let id = w.id("aux_models", "bge").await;
    ops::aux_model_set(
        &w.state,
        patch(json!({"action": "update", "id": id, "idle_seconds": 60})),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn an_upstream_prefix_rewrites_the_passthrough_ids_its_fallbacks_name() {
    let w = world().await;
    names(&w, "local_models", "qwen3", "kilo/openai/gpt-x").await;
    names(&w, "models", "cloud-b", "cloud-mini").await;
    let kilo = lmgw_core::store::get_upstream_by_name(&w.state.db, "kilo")
        .await
        .unwrap()
        .unwrap()
        .id;
    let saved = ops::upstream_set(
        &w.state,
        patch(json!({"action": "update", "id": kilo, "expose_prefix": "kc"})),
        ops::RowWriter::Dashboard,
    )
    .await
    .unwrap();
    assert_eq!(
        w.fallback("local_models", "qwen3").await.as_deref(),
        Some("kc/openai/gpt-x")
    );
    assert_eq!(
        w.fallback("models", "cloud-b").await.as_deref(),
        Some("cloud-mini")
    );
    assert!(
        saved
            .fallback_notes
            .iter()
            .any(|n| n.contains("chat model 'qwen3' now names 'kc/openai/gpt-x'")),
        "{:?}",
        saved.fallback_notes
    );
}

#[tokio::test]
async fn an_upstream_delete_names_the_fallbacks_it_leaves_without_a_target() {
    let w = world().await;
    names(&w, "local_models", "qwen3", "cloud-mini").await;
    names(&w, "local_models", "other", "kilo/openai/gpt-x").await;
    names(&w, "aux_models", "bge", "cloud-embed").await;
    let saved = ops::upstream_set(
        &w.state,
        patch(json!({"action": "delete", "id": w.cloud})),
        ops::RowWriter::Dashboard,
    )
    .await
    .unwrap();
    let notes = saved.fallback_notes.join(" | ");
    assert!(
        notes.contains("chat model 'qwen3' falls back to 'cloud-mini'"),
        "{notes}"
    );
    assert!(
        notes.contains("embedding model 'bge' falls back to 'cloud-embed'"),
        "{notes}"
    );
    assert!(!notes.contains("kilo"), "{notes}");
}

/// A rename's rewrite is computed before the save's checks; a fallback
/// another write changed meanwhile is that write's (review A4).
#[tokio::test]
async fn a_rewrite_leaves_a_fallback_changed_since_it_was_computed() {
    use lmgw_core::store::{rewrite_fallbacks_in, FallbackRewrite, FallbackTable};
    let w = world().await;
    names(&w, "local_models", "qwen3", "cloud-mini").await;
    let id = w.id("local_models", "qwen3").await;
    let rewrite = FallbackRewrite {
        table: FallbackTable::LocalModels,
        id,
        was: "cloud-mini".into(),
        fallback: "cloud-mini-2".into(),
    };
    // Cleared by another write after the rewrite was computed.
    set_fallback(&w.state, "local_models", "qwen3", None).await;
    let rewrite_now = |r: FallbackRewrite| {
        let db = w.state.db.clone();
        async move {
            let mut conn = db.acquire().await.unwrap();
            rewrite_fallbacks_in(&mut conn, &[r]).await.unwrap();
        }
    };
    rewrite_now(rewrite.clone()).await;
    assert_eq!(w.fallback("local_models", "qwen3").await, None);
    // Still holding the name, it is rewritten.
    set_fallback(&w.state, "local_models", "qwen3", Some("cloud-mini")).await;
    rewrite_now(rewrite).await;
    assert_eq!(
        w.fallback("local_models", "qwen3").await.as_deref(),
        Some("cloud-mini-2")
    );
}

/// A decision row's fallback that is a disabled decision row is saved with
/// the disabled note, not refused as one that does not resolve (review A3).
#[tokio::test]
async fn a_decision_rows_disabled_fallback_is_saved_with_its_note() {
    let w = world().await;
    sqlx::query("UPDATE aux_models SET kind = 'decision' WHERE model_id IN ('bge', 'bge-rr')")
        .execute(&w.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE aux_models SET enabled = 0 WHERE model_id = 'bge-rr'")
        .execute(&w.state.db)
        .await
        .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let id = w.id("aux_models", "bge").await;
    let saved = ops::aux_model_set(
        &w.state,
        patch(json!({"action": "update", "id": id, "fallback": "decide/bge-rr"})),
    )
    .await
    .unwrap();
    assert!(
        saved
            .fallback_notes
            .iter()
            .any(|n| n.contains("is disabled")),
        "{:?}",
        saved.fallback_notes
    );
}

/// A duplicate's fallback is checked as a save's: one of another type that
/// the original kept (migration 0095 keeps them) is left off the copy, and
/// the answer says why (review A8).
#[tokio::test]
async fn a_duplicate_copies_only_a_fallback_a_save_would_take() {
    let w = world().await;
    names(&w, "local_models", "other", "cloud-embed").await;
    let saved = ops::local_model_set(
        &w.state,
        patch(json!({"action": "duplicate", "model_id": "other"})),
    )
    .await
    .unwrap();
    let notes = saved.fallback_notes.join(" | ");
    assert!(notes.contains("the copy has no fallback"), "{notes}");
    assert!(notes.contains("an embedding model"), "{notes}");
    assert_eq!(w.fallback("local_models", "other-copy").await, None);

    names(&w, "local_models", "qwen3", "cloud-mini").await;
    ops::local_model_set(
        &w.state,
        patch(json!({"action": "duplicate", "model_id": "qwen3"})),
    )
    .await
    .unwrap();
    assert_eq!(
        w.fallback("local_models", "qwen3-copy").await.as_deref(),
        Some("cloud-mini")
    );
}

/// Routing writes are serialized from the snapshot their check reads to
/// their commit (review R1): a retarget that starts while another write is
/// in progress waits for it, and its dry run then reads what that write
/// committed — here a fallback onto the alias it retargets — so it is
/// refused, rather than both passing against the routing before either.
#[tokio::test]
async fn a_routing_write_waits_for_the_one_in_progress_and_checks_what_it_committed() {
    let w = world().await;
    let in_progress = w.state.routing_write().await;
    let retarget = tokio::spawn({
        let state = w.state.clone();
        async move {
            ops::model_set(
                &state,
                patch(json!({"action": "update", "alias": "cloud-mini",
                             "capabilities_override": {"capabilities": {"task": "embedding",
                                 "input_modalities": ["text"]}}})),
            )
            .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !retarget.is_finished(),
        "it waits for the write in progress"
    );
    // What the write in progress commits: a chat row's fallback onto the
    // alias.
    names(&w, "local_models", "qwen3", "cloud-mini").await;
    drop(in_progress);
    let e = retarget.await.unwrap().unwrap_err();
    assert!(
        e.contains(
            "chat model 'qwen3' falls back to 'cloud-mini', which would be an embedding model"
        ),
        "{e}"
    );
}

/// A delete whose handler is dropped mid-way — its client hung up — keeps
/// the routing write in its own task until its snapshot reload (review V3):
/// a save after it checks against the routing without the deleted alias,
/// never against the old snapshot while the delete still commits.
#[tokio::test]
async fn a_delete_whose_handler_is_dropped_holds_the_routing_write_until_its_reload() {
    // A file database, where the write lock behaves as in production: a
    // write waits for another one's (the busy timeout), a read does not.
    let dir = tempfile::tempdir().unwrap();
    let db = store::open(&dir.path().join("lmgw.sqlite")).await.unwrap();
    let state = AppState::init_for_tests_on(db).await.unwrap();
    let cloud = fallback_world::upstream(&state, "cloud", false).await;
    fallback_world::alias(&state, "gone", cloud, None).await;
    state.reload_snapshot().await.unwrap();
    // The store's write lock, held: the delete waits at its commit.
    let mut blocker = state.db.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let handler = tokio::spawn({
        let state = state.clone();
        async move { ops::model_set(&state, patch(json!({"action": "delete", "alias": "gone"}))).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    handler.abort();
    assert!(handler.await.unwrap_err().is_cancelled());
    let waited = tokio::time::timeout(Duration::from_millis(300), state.routing_write()).await;
    assert!(waited.is_err(), "the delete's task holds the routing write");
    assert!(state.snapshot().aliases.contains_key("gone"));

    sqlx::query("ROLLBACK")
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    let routing = tokio::time::timeout(Duration::from_secs(10), state.routing_write())
        .await
        .expect("the delete lets it go after its reload");
    assert!(!state.snapshot().aliases.contains_key("gone"));
    drop(routing);
}

/// The routing write ends with a delete's reload (review V3), not after the
/// stop of the deleted model's container: a save waits for the delete's
/// commit and reload, never for podman.
#[tokio::test]
async fn a_local_models_delete_lets_the_routing_write_go_before_its_container_stops() {
    let g = Gpu::new(16 * GIB, 2, 1).await;
    g.model("a", 4 * GIB).await;
    drop(
        vram::admit(&g.state, &g.route("a"), "a")
            .await
            .unwrap()
            .expect("a local model"),
    );
    let stops = g.gate_stops();
    let delete = tokio::spawn({
        let state = g.state.clone();
        async move {
            ops::local_model_set(&state, patch(json!({"action": "delete", "model_id": "a"}))).await
        }
    });
    let reloaded = tokio::time::timeout(Duration::from_secs(10), async {
        while g
            .state
            .snapshot()
            .local_models
            .iter()
            .any(|m| m.model_id == "a")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(reloaded.is_ok(), "the delete reloaded the snapshot");
    let routing = tokio::time::timeout(Duration::from_secs(2), g.state.routing_write())
        .await
        .expect("free while podman stops the container");
    assert!(!delete.is_finished(), "the stop is still held");
    drop(routing);

    stops.send(true).unwrap();
    delete.await.unwrap().unwrap();
    assert_eq!(g.stops(), ["a"]);
}
