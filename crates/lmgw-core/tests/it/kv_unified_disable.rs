//! `ops::validate_kv_unified` must never block turning a row *off* (review
//! finding X3, final cross-phase review).
//!
//! Ladder §12 entries 63/76 already taught `validate_ladder` this rule: a
//! save whose *resulting* `enabled` is `false` skips the refusal, because
//! disabling a row is maintenance on a row the owner already wants off, not
//! an invitation to fix a setting first. `validate_kv_unified` never got the
//! same exemption, so a row whose effective KV shape trips one of its
//! refusals — most plausibly one saved before this validation existed, with
//! a freeform `--kv-unified`/`-kvu` (or `--no-kv-unified`/`-no-kvu`) that
//! `hoist_promoted_args_into` folds into `params.kv_unified` on every load —
//! could never be saved again at all, disable included: the owner could not
//! turn their own row off without first fixing a setting they never typed into
//! a typed field.
//!
//! These rows are built with `store::insert_local_model` directly rather
//! than through `ops::local_model_set`, because `local_model_set`'s own
//! `create` arm runs this exact refusal unconditionally (matching
//! `validate_ladder`'s create-time behaviour) — the invalid shape can only
//! exist on a row that predates the check, which is exactly what a direct
//! insert simulates.

use lmgw_core::config::{HoldFallbackMode, LlamaParams, Settings};
use lmgw_core::gguf::synth;
use lmgw_core::ops::{self, LocalModelPatch};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewLocalModel};

async fn gateway() -> (SharedState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));

    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = Settings::default();
    settings.router.models_dir = dir.path().display().to_string();
    settings.router.image = "localhost/lmgw-test-no-such-image:none".into();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    (state, dir)
}

async fn insert_pre_existing(state: &SharedState, model_id: &str, params: LlamaParams) -> i64 {
    let new = NewLocalModel {
        model_id: model_id.into(),
        gguf_path: "base.gguf".into(),
        params,
        args: vec![],
        idle_seconds: 300,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: HoldFallbackMode::Inherit,
        hold_fallback: None,
        capabilities_override: None,
        ladder: vec![],
    };
    let id = store::insert_local_model(&state.db, &new).await.unwrap();
    state.reload_snapshot().await.unwrap();
    id
}

async fn get_by_id(state: &SharedState, model_id: &str) -> lmgw_core::config::LocalModel {
    store::list_local_models(&state.db)
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.model_id == model_id)
        .unwrap()
}

/// `kv_unified: true` with more than one slot and no `n_predict` — the pool
/// ledger has no bound to guard. A row in this shape (e.g. hoisted from a
/// freeform `--kv-unified` on load) must still be disable-able via the
/// Models-list menu's `disable` action.
#[tokio::test]
async fn disable_action_never_blocks_on_an_unguarded_unified_pool() {
    let (state, _dir) = gateway().await;
    insert_pre_existing(
        &state,
        "unguarded-unified",
        LlamaParams {
            parallel: Some(2),
            kv_unified: Some(true),
            ..Default::default()
        },
    )
    .await;

    let out = ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "disable".into(),
            model_id: Some("unguarded-unified".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], true, "{out}");
    let m = get_by_id(&state, "unguarded-unified").await;
    assert!(!m.enabled);
    // The offending settings are untouched — only `enabled` flipped.
    assert_eq!(m.params.kv_unified, Some(true));
    assert_eq!(m.params.parallel, Some(2));
}

/// The editor's Save always sends `action: "update"` with `enabled: false`
/// when the owner unticks the checkbox (ladder §12 entry 76's own point) —
/// that path must be exempt too, not only the Models-list menu's `disable`.
#[tokio::test]
async fn an_update_that_disables_never_blocks_on_an_unguarded_unified_pool() {
    let (state, _dir) = gateway().await;
    insert_pre_existing(
        &state,
        "editor-disabled-kvu",
        LlamaParams {
            parallel: Some(2),
            kv_unified: Some(true),
            ..Default::default()
        },
    )
    .await;

    let out = ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "update".into(),
            model_id: Some("editor-disabled-kvu".into()),
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], true, "{out}");
    let m = get_by_id(&state, "editor-disabled-kvu").await;
    assert!(!m.enabled);
}

/// `kv_unified: false` with `parallel` left on auto is the other refusal
/// (llama-server always unifies with `parallel` unset) — disable must skip
/// this one too.
#[tokio::test]
async fn disable_action_never_blocks_on_a_no_op_split() {
    let (state, _dir) = gateway().await;
    insert_pre_existing(
        &state,
        "no-op-split",
        LlamaParams {
            kv_unified: Some(false),
            ..Default::default()
        },
    )
    .await;

    let out = ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "disable".into(),
            model_id: Some("no-op-split".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], true, "{out}");
    assert!(!get_by_id(&state, "no-op-split").await.enabled);
}

/// The exemption tracks the *resulting* `enabled` state, not "this row was
/// disabled once" (second-pass review finding S3's rule, carried over to
/// this validation too): turning it back on still refuses on the same
/// unguarded pool.
#[tokio::test]
async fn enabling_the_row_back_still_validates() {
    let (state, _dir) = gateway().await;
    insert_pre_existing(
        &state,
        "toggle-back-on",
        LlamaParams {
            parallel: Some(2),
            kv_unified: Some(true),
            ..Default::default()
        },
    )
    .await;

    ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "disable".into(),
            model_id: Some("toggle-back-on".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let err = ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "enable".into(),
            model_id: Some("toggle-back-on".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(err.contains("unified KV"), "{err}");
    assert!(!get_by_id(&state, "toggle-back-on").await.enabled);
}

/// A plain `update` that leaves the row enabled has no amnesty to give
/// either — only a save whose *result* is disabled does.
#[tokio::test]
async fn a_plain_update_that_leaves_it_enabled_still_validates() {
    let (state, _dir) = gateway().await;
    insert_pre_existing(
        &state,
        "still-enabled",
        LlamaParams {
            parallel: Some(2),
            kv_unified: Some(true),
            ..Default::default()
        },
    )
    .await;

    let err = ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "update".into(),
            model_id: Some("still-enabled".into()),
            idle_seconds: Some(600),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(err.contains("unified KV"), "{err}");
}
