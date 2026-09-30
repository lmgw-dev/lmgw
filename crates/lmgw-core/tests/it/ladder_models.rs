//! `ops::local_model_set`'s ladder plumbing, end to end (ladder design §4,
//! §7 item 10): a real create/update/clear round trip through the store, on
//! top of `ops/tests.rs`'s own unit tests for each §4.3 refusal
//! (`validate_ladder`) and `models_endpoint.rs`'s coverage of §4.4's
//! `/v1/models` numbers (§7 item 11).
//!
//! `init_for_tests` installs a runtime that refuses every podman verb, so the
//! save-time `--help` probe (`config_warnings`) degrades to an advisory
//! rather than blocking — the same shape a card that has not pulled the
//! image yet gets. No container is ever started here.

use lmgw_core::config::Settings;
use lmgw_core::gguf::synth;
use lmgw_core::ladder::Rung;
use lmgw_core::ops::{self, LocalModelPatch};
use lmgw_core::runtime::argv::podman_run_argv;
use lmgw_core::runtime::descriptor::model_runtime;
use lmgw_core::runtime::Class;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;

/// A gateway pointed at a temp models dir carrying three same-architecture,
/// same-(absent)-tokenizer GGUFs — enough for a ladder to pass every §4.3
/// rule (rules 5/6/7 all trivially satisfied: no tokenizer keys at all means
/// every rung's signature is identical, and nothing loads a projector or
/// asks for MTP speculation).
async fn gateway() -> (SharedState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    synth::chat("qwen3", 65536).write_to(&dir.path().join("mid.gguf"));
    synth::chat("qwen3", 131072).write_to(&dir.path().join("top.gguf"));

    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = Settings::default();
    settings.router.models_dir = dir.path().display().to_string();
    // Nothing can pull this, which is the point (module doc).
    settings.router.image = "localhost/lmgw-test-no-such-image:none".into();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    (state, dir)
}

fn two_rung_ladder() -> Vec<Rung> {
    vec![
        Rung {
            gguf_path: "mid.gguf".into(),
            ctx_size: 65536,
        },
        Rung {
            gguf_path: "top.gguf".into(),
            ctx_size: 131072,
        },
    ]
}

async fn get_by_id(state: &SharedState, model_id: &str) -> lmgw_core::config::LocalModel {
    store::list_local_models(&state.db)
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.model_id == model_id)
        .unwrap_or_else(|| panic!("no local model '{model_id}'"))
}

#[tokio::test]
async fn creating_a_valid_ladder_persists_every_rung() {
    let (state, _dir) = gateway().await;
    let out = ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "create".into(),
            model_id: Some("laddered".into()),
            gguf_path: Some("base.gguf".into()),
            ctx_size: Some(32768),
            parallel: Some(2),
            n_predict: Some(4096),
            ladder: Some(two_rung_ladder()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], true, "{out}");

    let m = get_by_id(&state, "laddered").await;
    assert!(m.is_ladder());
    assert_eq!(m.ladder, two_rung_ladder());
    assert_eq!(m.top_rung(), 2);
    // The base is unaffected — it stays the row's own `gguf_path`/`ctx_size`.
    assert_eq!(m.gguf_path, "base.gguf");
    assert_eq!(m.params.ctx_size, Some(32768));
}

#[tokio::test]
async fn a_rule_1_violation_is_refused_and_never_reaches_the_store() {
    let (state, _dir) = gateway().await;
    let err = ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "create".into(),
            model_id: Some("no-max-output".into()),
            gguf_path: Some("base.gguf".into()),
            ctx_size: Some(32768),
            parallel: Some(2),
            // n_predict deliberately unset — §4.3 rule 1.
            ladder: Some(two_rung_ladder()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(err.contains("Max output"), "{err}");
    assert!(
        store::list_local_models(&state.db)
            .await
            .unwrap()
            .iter()
            .all(|m| m.model_id != "no-max-output"),
        "a refused create must not leave a row behind"
    );
}

#[tokio::test]
async fn clear_ladder_round_trips_back_to_not_a_ladder() {
    let (state, _dir) = gateway().await;
    ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "create".into(),
            model_id: Some("shrinking".into()),
            gguf_path: Some("base.gguf".into()),
            ctx_size: Some(32768),
            parallel: Some(2),
            n_predict: Some(4096),
            ladder: Some(two_rung_ladder()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(get_by_id(&state, "shrinking").await.is_ladder());

    let out = ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "update".into(),
            model_id: Some("shrinking".into()),
            clear: Some("ladder".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], true, "{out}");

    let m = get_by_id(&state, "shrinking").await;
    assert!(!m.is_ladder(), "clear: \"ladder\" must reset it to empty");
    assert_eq!(m.ladder, Vec::<Rung>::new());
    // Everything else on the row survives the clear untouched.
    assert_eq!(m.gguf_path, "base.gguf");
    assert_eq!(m.params.ctx_size, Some(32768));

    // And it can be set again — `clear` is not a one-way door.
    let out = ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "update".into(),
            model_id: Some("shrinking".into()),
            ladder: Some(vec![Rung {
                gguf_path: "top.gguf".into(),
                ctx_size: 131072,
            }]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], true, "{out}");
    let m = get_by_id(&state, "shrinking").await;
    assert!(m.is_ladder());
    assert_eq!(m.top_rung(), 1);
}

/// An update that touches nothing about the ladder leaves it exactly as it
/// was — `ladder: None` on the patch means "unchanged", the same convention
/// every other field on `LocalModelPatch` follows.
#[tokio::test]
async fn an_unrelated_update_leaves_the_ladder_alone() {
    let (state, _dir) = gateway().await;
    ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "create".into(),
            model_id: Some("stable".into()),
            gguf_path: Some("base.gguf".into()),
            ctx_size: Some(32768),
            parallel: Some(2),
            n_predict: Some(4096),
            ladder: Some(two_rung_ladder()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "update".into(),
            model_id: Some("stable".into()),
            temp: Some(0.6),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let m = get_by_id(&state, "stable").await;
    assert_eq!(m.ladder, two_rung_ladder());
    assert_eq!(m.params.temp, Some(0.6));
}

/// Review finding 10: a rung file going missing (or unreadable) after save
/// must never block `disable` — the owner has to be able to turn a broken
/// model off without first clearing its whole ladder to satisfy
/// `validate_ladder` again.
#[tokio::test]
async fn a_missing_rung_file_never_blocks_disable() {
    let (state, dir) = gateway().await;
    ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "create".into(),
            model_id: Some("going-stale".into()),
            gguf_path: Some("base.gguf".into()),
            ctx_size: Some(32768),
            parallel: Some(2),
            n_predict: Some(4096),
            ladder: Some(two_rung_ladder()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Rung 2's file (the first higher rung, "mid.gguf") disappears — renamed
    // or deleted outside lmgw.
    std::fs::remove_file(dir.path().join("mid.gguf")).unwrap();

    let out = ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "disable".into(),
            model_id: Some("going-stale".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], true, "{out}");
    let m = get_by_id(&state, "going-stale").await;
    assert!(!m.enabled);
    // The ladder itself is untouched by the disable — only enabled flipped.
    assert_eq!(m.ladder, two_rung_ladder());

    // The exemption tracks the *resulting* enabled state, not "this row was
    // disabled once" — turning it back on still refuses on the same broken
    // rung (second-pass review finding S3: "validation runs at enable and
    // at any save with enabled: true").
    let err = ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "enable".into(),
            model_id: Some("going-stale".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(err.contains("rung 2"), "{err}");
}

/// Second-pass review finding S3: the editor's Save always sends
/// `action: "update"` — including when the only change is unticking
/// "enabled" — so exempting only `action == "disable"` (finding 10's first
/// fix) still refused the one path an owner actually uses to turn a broken
/// model off from the UI. The exemption is on the *resulting* `enabled`
/// state now, not the action name.
#[tokio::test]
async fn an_update_that_disables_never_blocks_on_a_stale_rung_file_either() {
    let (state, dir) = gateway().await;
    ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "create".into(),
            model_id: Some("editor-disabled".into()),
            gguf_path: Some("base.gguf".into()),
            ctx_size: Some(32768),
            parallel: Some(2),
            n_predict: Some(4096),
            ladder: Some(two_rung_ladder()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    std::fs::remove_file(dir.path().join("mid.gguf")).unwrap();

    let out = ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "update".into(),
            model_id: Some("editor-disabled".into()),
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], true, "{out}");
    let m = get_by_id(&state, "editor-disabled").await;
    assert!(!m.enabled);
    assert_eq!(m.ladder, two_rung_ladder());
}

/// Review finding 12 (revised by second-pass finding S5): a freeform
/// `--spec-type draft-mtp` (no typed `spec_type`, no `draft_gguf_path`) used
/// to bypass rule 6 entirely — the row saved even though none of its rungs
/// carry MTP layers and it names no drafter. The fix is *not* promoting
/// `spec-type`/`model-draft` into `PROMOTED_ARGS` (that would hoist them on
/// every row at every load, not only a ladder's — S5's point exactly);
/// `validate_ladder` instead reads the effective value itself
/// (`effective_spec_type`/`effective_draft_path_is_set`), typed field or
/// freeform, without ever writing it back.
#[tokio::test]
async fn a_freeform_spec_type_no_longer_bypasses_rule_6() {
    let (state, _dir) = gateway().await;
    let err = ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "create".into(),
            model_id: Some("freeform-mtp".into()),
            gguf_path: Some("base.gguf".into()),
            ctx_size: Some(32768),
            parallel: Some(2),
            n_predict: Some(4096),
            extra_args: Some("--spec-type draft-mtp".into()),
            ladder: Some(two_rung_ladder()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(err.contains("MTP"), "{err}");
    assert!(
        store::list_local_models(&state.db)
            .await
            .unwrap()
            .iter()
            .all(|m| m.model_id != "freeform-mtp"),
        "a refused create must not leave a row behind"
    );
}

/// Second-pass review finding S5, the positive half: a **non-ladder** row
/// with the same freeform `--spec-type`/`--model-draft` must be completely
/// unaffected — no hoisting into the typed fields, no `/models/` rewrite on
/// the drafter path, and the same argv it would have rendered before any
/// ladder work existed. This is exactly the "rows without a ladder are
/// unchanged" guarantee the first version of finding 12's fix broke by
/// promoting both flags for every row.
#[tokio::test]
async fn a_non_ladder_row_with_freeform_spec_type_is_unhoisted_and_unchanged() {
    let (state, dir) = gateway().await;
    ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "create".into(),
            model_id: Some("plain-drafter".into()),
            gguf_path: Some("base.gguf".into()),
            ctx_size: Some(32768),
            n_predict: Some(4096),
            extra_args: Some("--spec-type draft-mtp --model-draft mid.gguf".into()),
            // Deliberately no `ladder` field at all.
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let m = get_by_id(&state, "plain-drafter").await;
    assert!(!m.is_ladder());
    // Neither flag was hoisted into its typed field...
    assert_eq!(m.params.spec_type, None);
    assert_eq!(m.params.draft_gguf_path, None);
    // ...and the freeform text survives exactly as written — a hoisted
    // `--model-draft` would have gained a `/models/` prefix it never gets
    // here.
    assert_eq!(
        m.args,
        vec!["--spec-type", "draft-mtp", "--model-draft", "mid.gguf"]
    );

    // The rendered argv puts both in the freeform tail, verbatim.
    let snap = state.snapshot();
    let rt = model_runtime(&snap, Class::Chat, "plain-drafter").unwrap();
    let spec = rt
        .preview_spec(
            "lmgw",
            9101,
            &dir.path().display().to_string(),
            std::path::Path::new("/nonexistent"),
        )
        .unwrap();
    let argv = podman_run_argv(&spec);
    let i = argv.iter().position(|a| a == "--spec-type").unwrap();
    assert_eq!(argv[i + 1], "draft-mtp");
    let i = argv.iter().position(|a| a == "--model-draft").unwrap();
    assert_eq!(
        argv[i + 1],
        "mid.gguf",
        "must not be rewritten onto /models/…"
    );
}

/// `lmgw__local_model_get`'s "rungs" (ladder design §6, §8 WP6): every rung's
/// own command line, per-slot context and switchover, base included — `null`
/// on a row without a ladder, so the field a caller has to check first is a
/// single `is_null()`, not "is this array present at all".
#[tokio::test]
async fn local_model_get_shows_every_rungs_command_line_and_derived_numbers() {
    let (state, _dir) = gateway().await;
    ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "create".into(),
            model_id: Some("laddered".into()),
            gguf_path: Some("base.gguf".into()),
            ctx_size: Some(32768),
            parallel: Some(2),
            n_predict: Some(4096),
            ladder: Some(two_rung_ladder()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let got = ops::local_model_get(&state, None, Some("laddered"), None)
        .await
        .unwrap();
    let rungs = got["rungs"].as_array().unwrap_or_else(|| panic!("{got}"));
    assert_eq!(rungs.len(), 3, "{got}");

    for (i, (gguf, ctx)) in [
        ("base.gguf", 32768),
        ("mid.gguf", 65536),
        ("top.gguf", 131072),
    ]
    .into_iter()
    .enumerate()
    {
        let r = &rungs[i];
        assert_eq!(r["rung"], i + 1, "{r}");
        assert_eq!(r["of"], 3, "{r}");
        assert_eq!(r["gguf_path"], gguf, "{r}");
        assert_eq!(r["ctx_size"], ctx, "{r}");
        assert_eq!(r["per_slot_ctx"], ctx / 2, "{r}");
        assert_eq!(r["switchover"], ctx / 2 - 4096, "{r}");
        let cmd = r["command_line"].as_str().unwrap_or_else(|| panic!("{r}"));
        assert!(cmd.contains(gguf), "rung {}: {cmd}", i + 1);
    }

    // A row without a ladder gets `null`, not an empty array — the top-level
    // `command_line` field stays the one place its (single) command line
    // lives, exactly as before this field existed.
    ops::local_model_set(
        &state,
        LocalModelPatch {
            action: "create".into(),
            model_id: Some("plain".into()),
            gguf_path: Some("base.gguf".into()),
            ctx_size: Some(32768),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let plain = ops::local_model_get(&state, None, Some("plain"), None)
        .await
        .unwrap();
    assert!(plain["rungs"].is_null(), "{plain}");
    assert!(
        !plain["command_line"].as_str().unwrap().is_empty(),
        "{plain}"
    );
}

/// Second-pass review finding S10: `/api/ladder-rung-plan`
/// (`ops::ladder_rung_plan`) resolved only `gguf_path` through
/// `modelinfo::resolve` — `mmproj_path`/`draft_gguf_path` went into the probe
/// row exactly as the caller spelled them, with none of `resolve`'s `..`/
/// absolute-path guard and none of its existence check. Both companions now
/// get the same treatment.
#[tokio::test]
async fn ladder_rung_plan_resolves_its_companion_paths_like_gguf_path() {
    let (state, _dir) = gateway().await;

    // Accepted: a real file already sitting under the models dir (one of
    // the gateway's own fixtures).
    let ok = ops::ladder_rung_plan(
        &state,
        ops::RungPlanInput {
            gguf_path: "base.gguf",
            ctx_size: 32768,
            cache_type_k: None,
            cache_type_v: None,
            mmproj_path: Some("mid.gguf"),
            draft_gguf_path: None,
            n_gpu_layers: None,
        },
    )
    .await
    .unwrap();
    assert!(ok["footprint"].is_object(), "{ok}");

    // Refused: `..` reaching outside the models dir.
    let err = ops::ladder_rung_plan(
        &state,
        ops::RungPlanInput {
            gguf_path: "base.gguf",
            ctx_size: 32768,
            cache_type_k: None,
            cache_type_v: None,
            mmproj_path: Some("../outside.gguf"),
            draft_gguf_path: None,
            n_gpu_layers: None,
        },
    )
    .await
    .unwrap_err();
    assert!(err.contains("plain path"), "{err}");

    // Refused: a companion that does not exist.
    let err = ops::ladder_rung_plan(
        &state,
        ops::RungPlanInput {
            gguf_path: "base.gguf",
            ctx_size: 32768,
            cache_type_k: None,
            cache_type_v: None,
            mmproj_path: None,
            draft_gguf_path: Some("nope.gguf"),
            n_gpu_layers: None,
        },
    )
    .await
    .unwrap_err();
    assert!(err.contains("not in the"), "{err}");

    // Left unset, a companion is just absent — no refusal on an incomplete
    // edit.
    let ok = ops::ladder_rung_plan(
        &state,
        ops::RungPlanInput {
            gguf_path: "base.gguf",
            ctx_size: 32768,
            cache_type_k: None,
            cache_type_v: None,
            mmproj_path: None,
            draft_gguf_path: None,
            n_gpu_layers: None,
        },
    )
    .await
    .unwrap();
    assert!(ok["footprint"].is_object(), "{ok}");
}

/// Review third pass, T4: `local_model_get`'s per-slot context and
/// switchover are capped at each rung's own GGUF's trained context — the
/// same number the gate judges and publishes a running rung on
/// (`gate::ladder::target_rung`, `ladder::slot_ctx`) — not the configured
/// value, which can be higher on a row §4.3 rule 3b would refuse today but
/// which an older row (or a hand-edited database) can still carry. Written
/// straight through the store, bypassing `validate_ladder`, the way such a
/// stale row would actually have gotten there.
#[tokio::test]
async fn local_model_get_caps_the_shown_per_slot_context_at_the_trained_one() {
    let (state, _dir) = gateway().await;
    // top.gguf's own trained context is 131072 (`gateway()`'s fixture); this
    // rung's configured ctx_size asks for more than that.
    store::insert_local_model(
        &state.db,
        &store::NewLocalModel {
            model_id: "stale-ladder".into(),
            gguf_path: "base.gguf".into(),
            params: lmgw_core::config::LlamaParams {
                ctx_size: Some(32768),
                parallel: Some(1),
                n_predict: Some(4096),
                ..Default::default()
            },
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            ladder: vec![Rung {
                gguf_path: "top.gguf".into(),
                ctx_size: 262144,
            }],
        },
    )
    .await
    .unwrap();

    let got = ops::local_model_get(&state, None, Some("stale-ladder"), None)
        .await
        .unwrap();
    let rungs = got["rungs"].as_array().unwrap_or_else(|| panic!("{got}"));
    assert_eq!(
        rungs[1]["ctx_size"], 262144,
        "the configured value is shown as-is: {got}"
    );
    assert_eq!(
        rungs[1]["per_slot_ctx"], 131072,
        "capped at top.gguf's own trained context, not the configured 262144: {got}"
    );
    assert_eq!(rungs[1]["switchover"], 131072 - 4096, "{got}");
}
