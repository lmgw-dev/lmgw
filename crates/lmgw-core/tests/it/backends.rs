//! Container builds' foundation (container-builds design §3–§5, §7, §10): the
//! `builds` / `build_runs` store, the boot sweep of interrupted runs, the three
//! new settings on every surface that reads or writes them, and the dev flag.
//!
//! The pure halves — validation, tags, the config hash — are unit tests in
//! `backends::{validate,tags}`; this file is what needs a database or a
//! gateway.

use lmgw_core::backends::validate::validate_build;
use lmgw_core::backends::{
    BuildExtra, BuildRunPatch, BuildRunStatus, BuildSpec, BuildTrigger, Engine, Forge, NewBuildRun,
    ResolvedExtra, ResolvedInputs,
};
use lmgw_core::error::GatewayError;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::{ops, store};
use serde_json::{json, Value};

use crate::common;
use common::serve;

fn spec(slug: &str) -> BuildSpec {
    validate_build(BuildSpec {
        slug: slug.into(),
        name: format!("{slug} build"),
        engine: Engine::Llama,
        repo_url: "https://github.com/ggml-org/llama.cpp".into(),
        forge: Forge::Github,
        git_ref: "master".into(),
        extras: vec![BuildExtra::Pr {
            number: 16391,
            pin: None,
        }],
        arch: Some(vec!["89".into()]),
        build_args: "GGML_CUDA_FA_ALL_QUANTS=ON".into(),
        keep_runs: Some(3),
        ..BuildSpec::default()
    })
    .expect("the fixture is a valid build")
}

async fn state() -> SharedState {
    AppState::init_for_tests().await.unwrap()
}

fn bad_request(e: GatewayError) -> String {
    match e {
        GatewayError::BadRequest(m) => m,
        other => panic!("expected a BadRequest, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Builds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_build_round_trips_through_the_store_with_every_field() {
    let st = state().await;
    let mut s = spec("official-master");
    s.edits = Some(vec![]);
    s.cpus = Some("0-15".into());
    s.dockerfile = Some(".devops/cuda.Dockerfile".into());
    let id = store::insert_build(&st.db, &s).await.unwrap();

    let got = store::get_build(&st.db, id).await.unwrap().unwrap();
    assert_eq!(got.id, id);
    assert_eq!(got.spec, s);
    assert!(!got.created_at.is_empty());
    assert_eq!(
        got.moving_tag(),
        "localhost/lmgw-llama-server:official-master"
    );
    // `Some([])` (an explicit "no edits") and `None` (the preset's) are
    // different things and must stay different through the column.
    assert_eq!(got.spec.edits, Some(vec![]));
    assert_eq!(
        store::get_build_by_slug(&st.db, "official-master")
            .await
            .unwrap()
            .unwrap()
            .id,
        id
    );

    let bare = store::insert_build(&st.db, &spec("bare")).await.unwrap();
    let bare = store::get_build(&st.db, bare).await.unwrap().unwrap();
    assert_eq!(bare.spec.edits, None);
    assert_eq!(bare.spec.cuda_version, None);

    let slugs: Vec<String> = store::list_builds(&st.db)
        .await
        .unwrap()
        .into_iter()
        .map(|b| b.spec.slug)
        .collect();
    assert_eq!(slugs, ["bare", "official-master"]);
}

#[tokio::test]
async fn a_taken_slug_is_refused_in_words() {
    let st = state().await;
    store::insert_build(&st.db, &spec("x")).await.unwrap();
    let msg = bad_request(store::insert_build(&st.db, &spec("x")).await.unwrap_err());
    assert!(msg.contains("'x' already exists"), "{msg}");

    let other = store::insert_build(&st.db, &spec("y")).await.unwrap();
    let msg = bad_request(
        store::update_build(&st.db, other, &spec("x"))
            .await
            .unwrap_err(),
    );
    assert!(msg.contains("already exists"), "{msg}");
}

#[tokio::test]
async fn the_slug_is_free_until_the_first_run_and_fixed_after() {
    let st = state().await;
    let id = store::insert_build(&st.db, &spec("draft")).await.unwrap();

    // No run yet: renaming is fine, and everything else changes with it.
    let mut renamed = spec("official-master");
    renamed.git_ref = "b6000".into();
    store::update_build(&st.db, id, &renamed).await.unwrap();
    let b = store::get_build(&st.db, id).await.unwrap().unwrap();
    assert_eq!(b.spec.slug, "official-master");
    assert_eq!(b.spec.git_ref, "b6000");

    store::insert_build_run(&st.db, &NewBuildRun::of(&b, BuildTrigger::Manual))
        .await
        .unwrap();

    // With a run, every other field still changes…
    let mut edited = renamed.clone();
    edited.notes = "tracks master".into();
    store::update_build(&st.db, id, &edited).await.unwrap();
    // …but the slug does not, and the refusal says why and what to do.
    let msg = bad_request(
        store::update_build(&st.db, id, &spec("new-slug"))
            .await
            .unwrap_err(),
    );
    assert!(msg.contains("1 run(s)"), "{msg}");
    assert!(msg.contains("duplicate"), "{msg}");
    let b = store::get_build(&st.db, id).await.unwrap().unwrap();
    assert_eq!(b.spec.slug, "official-master");
    assert_eq!(b.spec.notes, "tracks master");

    assert!(matches!(
        store::update_build(&st.db, 9999, &spec("z")).await,
        Err(GatewayError::NotFound(_))
    ));
}

#[tokio::test]
async fn duplicate_copies_everything_under_a_free_slug() {
    let st = state().await;
    let id = store::insert_build(&st.db, &spec("official-master"))
        .await
        .unwrap();
    let src = store::get_build(&st.db, id).await.unwrap().unwrap();

    let copy = store::duplicate_build(&st.db, id, None, None)
        .await
        .unwrap();
    let copy = store::get_build(&st.db, copy).await.unwrap().unwrap();
    assert_eq!(copy.spec.slug, "official-master-copy");
    assert_eq!(copy.spec.name, "official-master build (copy)");
    assert_eq!(copy.spec.extras, src.spec.extras);
    assert_eq!(copy.spec.build_args, src.spec.build_args);

    let again = store::duplicate_build(&st.db, id, None, None)
        .await
        .unwrap();
    let again = store::get_build(&st.db, again).await.unwrap().unwrap();
    assert_eq!(again.spec.slug, "official-master-copy-2");

    let named = store::duplicate_build(&st.db, id, Some("master-pr16391"), Some("with the PR"))
        .await
        .unwrap();
    let named = store::get_build(&st.db, named).await.unwrap().unwrap();
    assert_eq!(named.spec.slug, "master-pr16391");
    assert_eq!(named.spec.name, "with the PR");

    let msg = bad_request(
        store::duplicate_build(&st.db, id, Some("Bad Slug"), None)
            .await
            .unwrap_err(),
    );
    assert!(msg.contains("slug"), "{msg}");
}

#[tokio::test]
async fn a_stored_row_that_does_not_parse_fails_loudly_not_empty() {
    let st = state().await;
    let id = store::insert_build(&st.db, &spec("x")).await.unwrap();
    sqlx::query("UPDATE builds SET extras = 'not json' WHERE id = ?1")
        .bind(id)
        .execute(&st.db)
        .await
        .unwrap();
    // An `extras` read as `[]` would build master without the PR.
    let err = store::get_build(&st.db, id).await.unwrap_err().to_string();
    assert!(err.contains("build 'x'"), "{err}");
    assert!(err.contains("extras"), "{err}");
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_run_opens_running_takes_patches_and_finishes() {
    let st = state().await;
    let id = store::insert_build(&st.db, &spec("official-master"))
        .await
        .unwrap();
    let b = store::get_build(&st.db, id).await.unwrap().unwrap();
    let run = store::insert_build_run(&st.db, &NewBuildRun::of(&b, BuildTrigger::Mcp))
        .await
        .unwrap();

    let r = store::get_build_run(&st.db, run).await.unwrap().unwrap();
    assert_eq!(r.build_id, Some(id));
    assert_eq!(r.status, BuildRunStatus::Running);
    assert_eq!(r.trigger, BuildTrigger::Mcp);
    assert_eq!(r.slug, "official-master");
    assert_eq!(r.engine, Engine::Llama);
    assert_eq!(r.inputs.config, b.spec, "the definition is snapshotted");
    assert_eq!(r.inputs.resolved, None);
    assert!(r.finished_at.is_none());

    let resolved = ResolvedInputs {
        base_sha: "4b1a27fa0e4c1d2b3a4958677a8b9c0d1e2f3a4b".into(),
        extras: vec![ResolvedExtra {
            extra: b.spec.extras[0].clone(),
            sha: "1".repeat(40),
        }],
        cuda_version: Some("13.0.0".into()),
        arch: vec!["89".into()],
        dockerfile: ".devops/cuda.Dockerfile".into(),
        target: "server".into(),
        edits: vec![],
    };
    let hash = lmgw_core::backends::tags::cfg_hash(&b.spec, &resolved).unwrap();
    let mut inputs = r.inputs.clone();
    inputs.resolved = Some(resolved.clone());
    inputs.cfg_hash = Some(hash.clone());
    store::update_build_run(
        &st.db,
        run,
        &BuildRunPatch {
            job_id: Some(42),
            inputs: Some(inputs.clone()),
            base_sha: Some(resolved.base_sha.clone()),
            cfg_hash: Some(hash.clone()),
            log_path: Some("/b/logs/1.log".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // A second patch touches only what it names.
    store::update_build_run(
        &st.db,
        run,
        &BuildRunPatch {
            image_id: Some("f".repeat(64)),
            tags: Some(vec![
                "localhost/lmgw-llama-server:official-master-4b1a27f-x".into(),
            ]),
            size_bytes: Some(9_000_000_000),
            verify: Some(json!({"help": "ok"})),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let r = store::get_build_run(&st.db, run).await.unwrap().unwrap();
    assert_eq!(r.job_id, Some(42));
    assert_eq!(r.inputs, inputs);
    assert_eq!(r.cfg_hash.as_deref(), Some(hash.as_str()));
    assert_eq!(r.log_path.as_deref(), Some("/b/logs/1.log"));
    assert_eq!(r.size_bytes, Some(9_000_000_000));
    assert_eq!(r.tags.len(), 1);
    assert_eq!(r.verify, Some(json!({"help": "ok"})));
    assert_eq!(r.status, BuildRunStatus::Running);

    // A terminal status goes through `finish`, which stamps the end.
    let err = store::update_build_run(
        &st.db,
        run,
        &BuildRunPatch {
            status: Some(BuildRunStatus::Succeeded),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("finish_build_run"), "{err}");
    assert!(
        store::finish_build_run(&st.db, run, BuildRunStatus::Running, None)
            .await
            .is_err()
    );
    store::finish_build_run(&st.db, run, BuildRunStatus::Unverified, None)
        .await
        .unwrap();
    let r = store::get_build_run(&st.db, run).await.unwrap().unwrap();
    assert_eq!(r.status, BuildRunStatus::Unverified);
    assert!(r.finished_at.is_some());

    assert!(matches!(
        store::finish_build_run(&st.db, 9999, BuildRunStatus::Failed, None).await,
        Err(GatewayError::NotFound(_))
    ));
}

#[tokio::test]
async fn runs_list_newest_first_and_promotion_is_one_per_build() {
    let st = state().await;
    let a = store::insert_build(&st.db, &spec("a")).await.unwrap();
    let b = store::insert_build(&st.db, &spec("b")).await.unwrap();
    let build_a = store::get_build(&st.db, a).await.unwrap().unwrap();
    let build_b = store::get_build(&st.db, b).await.unwrap().unwrap();
    let a1 = store::insert_build_run(&st.db, &NewBuildRun::of(&build_a, BuildTrigger::Manual))
        .await
        .unwrap();
    let b1 = store::insert_build_run(&st.db, &NewBuildRun::of(&build_b, BuildTrigger::Manual))
        .await
        .unwrap();
    let a2 = store::insert_build_run(&st.db, &NewBuildRun::of(&build_a, BuildTrigger::Manual))
        .await
        .unwrap();

    let ids = |runs: Vec<lmgw_core::backends::BuildRun>| -> Vec<i64> {
        runs.into_iter().map(|r| r.id).collect()
    };
    assert_eq!(
        ids(store::list_runs_for_build(&st.db, a).await.unwrap()),
        [a2, a1]
    );
    assert_eq!(
        ids(store::list_build_runs(&st.db, None, 0).await.unwrap()),
        [a2, b1, a1]
    );
    assert_eq!(
        ids(store::list_build_runs(&st.db, None, 2).await.unwrap()),
        [a2, b1]
    );
    assert_eq!(store::count_build_runs(&st.db, a).await.unwrap(), 2);

    store::promote_build_run(&st.db, a1).await.unwrap();
    store::promote_build_run(&st.db, b1).await.unwrap();
    store::promote_build_run(&st.db, a2).await.unwrap();
    let promoted = |id| {
        let st = st.clone();
        async move {
            store::get_build_run(&st.db, id)
                .await
                .unwrap()
                .unwrap()
                .promoted
        }
    };
    assert!(!promoted(a1).await, "Make current moved it to a2");
    assert!(promoted(a2).await);
    assert!(promoted(b1).await, "another build's marker is untouched");
    // Rollback: back to a1.
    store::promote_build_run(&st.db, a1).await.unwrap();
    assert!(promoted(a1).await);
    assert!(!promoted(a2).await);

    assert!(matches!(
        store::promote_build_run(&st.db, 9999).await,
        Err(GatewayError::NotFound(_))
    ));
}

#[tokio::test]
async fn deleting_a_build_keeps_its_runs() {
    let st = state().await;
    let id = store::insert_build(&st.db, &spec("gone")).await.unwrap();
    let b = store::get_build(&st.db, id).await.unwrap().unwrap();
    let run = store::insert_build_run(&st.db, &NewBuildRun::of(&b, BuildTrigger::Manual))
        .await
        .unwrap();

    assert!(store::delete_build(&st.db, id).await.unwrap());
    assert!(!store::delete_build(&st.db, id).await.unwrap());
    assert!(store::get_build(&st.db, id).await.unwrap().is_none());

    let r = store::get_build_run(&st.db, run).await.unwrap().unwrap();
    assert_eq!(r.build_id, None, "ON DELETE SET NULL");
    assert_eq!(r.slug, "gone", "still readable from its snapshot");
    assert_eq!(r.inputs.config.slug, "gone");
    // Its build's moving tag is gone with the build: nothing to promote to.
    let msg = bad_request(store::promote_build_run(&st.db, run).await.unwrap_err());
    assert!(msg.contains("deleted build"), "{msg}");
}

#[tokio::test]
async fn the_boot_sweep_fails_runs_left_running_and_only_those() {
    let st = state().await;
    let id = store::insert_build(&st.db, &spec("x")).await.unwrap();
    let b = store::get_build(&st.db, id).await.unwrap().unwrap();
    let open = store::insert_build_run(&st.db, &NewBuildRun::of(&b, BuildTrigger::Manual))
        .await
        .unwrap();
    let done = store::insert_build_run(&st.db, &NewBuildRun::of(&b, BuildTrigger::Manual))
        .await
        .unwrap();
    store::finish_build_run(&st.db, done, BuildRunStatus::Succeeded, None)
        .await
        .unwrap();

    assert_eq!(store::fail_orphaned_build_runs(&st.db).await.unwrap(), 1);
    let r = store::get_build_run(&st.db, open).await.unwrap().unwrap();
    assert_eq!(r.status, BuildRunStatus::Failed);
    assert_eq!(r.error.as_deref(), Some("interrupted by shutdown"));
    assert!(r.finished_at.is_some());
    let d = store::get_build_run(&st.db, done).await.unwrap().unwrap();
    assert_eq!(d.status, BuildRunStatus::Succeeded);
    assert_eq!(d.error, None);
    assert_eq!(store::fail_orphaned_build_runs(&st.db).await.unwrap(), 0);
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

const TOKEN: &str = "ghp_do-not-leak-this-0123456789";

async fn get_settings_full(gw: &common::Gw) -> Value {
    gw.client()
        .get(format!("{gw}/api/settings-full"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn op(gw: &common::Gw, name: &str, args: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}

#[tokio::test]
async fn forge_tokens_are_redacted_per_host_on_every_read() {
    let st = state().await;
    let gw = serve(st.clone()).await;

    let (status, body) = op(
        &gw,
        "settings_set_full",
        json!({
            "forge_tokens": {"GitHub.com": TOKEN, "git.example.com": "glpat-also-secret"},
            "build_update_check_hours": 0,
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    // Stored under the lowercased host, which is how a repo URL finds it.
    let s = st.snapshot().settings.clone();
    assert_eq!(
        s.forge_tokens.get("github.com").map(String::as_str),
        Some(TOKEN)
    );
    assert_eq!(s.build_update_check_hours, 0);

    let full = get_settings_full(&gw).await;
    assert_eq!(
        full["forge_tokens"],
        json!({"git.example.com": "<set>", "github.com": "<set>"})
    );
    assert_eq!(full["build_update_check_hours"], 0);
    let dto: lmgw_api_types::SettingsFull = serde_json::from_value(full.clone()).unwrap();
    assert_eq!(dto.forge_tokens.len(), 2);

    let tool = ops::settings(&st).await.unwrap();
    assert_eq!(
        tool["forge_tokens"],
        json!({"git.example.com": "<set>", "github.com": "<set>"})
    );
    for (surface, v) in [("settings-full", &full), ("lmgw__settings", &tool)] {
        let text = v.to_string();
        assert!(!text.contains(TOKEN), "{surface} leaked a forge token");
        assert!(!text.contains("glpat"), "{surface} leaked a forge token");
    }

    // An empty value keeps a token; a clear erases exactly the named host.
    let (status, body) = op(
        &gw,
        "settings_set_full",
        json!({
            "forge_tokens": {"github.com": ""},
            "clear_forge_tokens": ["git.example.com"],
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let s = st.snapshot().settings.clone();
    assert_eq!(s.forge_tokens.len(), 1);
    assert_eq!(
        s.forge_tokens.get("github.com").map(String::as_str),
        Some(TOKEN)
    );
}

#[tokio::test]
async fn a_forge_token_that_would_break_a_header_is_refused() {
    let st = state().await;
    let gw = serve(st.clone()).await;
    for bad in [
        json!({"forge_tokens": {"github.com": "abc\r\nX-Evil: 1"}}),
        json!({"forge_tokens": {"https://github.com": "abc"}}),
    ] {
        let (status, body) = op(&gw, "settings_set_full", bad.clone()).await;
        assert_eq!(status, 400, "{bad} -> {body}");
    }
    assert!(st.snapshot().settings.forge_tokens.is_empty());
}

#[tokio::test]
async fn the_builds_dir_shows_its_effective_value_and_is_validated() {
    let st = state().await;
    let gw = serve(st.clone()).await;

    let full = get_settings_full(&gw).await;
    assert_eq!(full["builds_dir"], Value::Null);
    assert_eq!(
        full["builds_dir_effective"],
        st.data_dir.join("builds").display().to_string()
    );

    let (status, body) = op(
        &gw,
        "settings_set_full",
        json!({"builds_dir": "relative/dir"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("builds").display().to_string();
    let (status, body) = op(&gw, "settings_set_full", json!({"builds_dir": path})).await;
    assert_eq!(status, 200, "{body}");
    let full = get_settings_full(&gw).await;
    assert_eq!(full["builds_dir"], path);
    assert_eq!(full["builds_dir_effective"], path);
    assert_eq!(st.builds_dir().display().to_string(), path);

    // Blank is "back to the default".
    let (status, _) = op(&gw, "settings_set_full", json!({"builds_dir": " "})).await;
    assert_eq!(status, 200);
    assert_eq!(st.snapshot().settings.builds_dir, None);
}

#[tokio::test]
async fn the_update_interval_is_settable_from_the_tool_plane_and_the_rest_is_not() {
    let st = state().await;
    let gw = serve(st.clone()).await;
    let (status, body) = op(&gw, "settings_set", json!({"build_update_check_hours": 12})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(st.snapshot().settings.build_update_check_hours, 12);
    assert_eq!(
        ops::settings(&st).await.unwrap()["build_update_check_hours"],
        12
    );
    // Secrets and the write location stay on the dashboard.
    for field in [
        json!({"forge_tokens": {"github.com": TOKEN}}),
        json!({"builds_dir": "/srv/builds"}),
    ] {
        let (status, body) = op(&gw, "settings_set", field.clone()).await;
        assert_ne!(status, 200, "{field} -> {body}");
    }
}

/// A huge interval used to be accepted and then panic the scheduler
/// (`last + hours` past the last date chrono can hold).
#[tokio::test]
async fn the_update_interval_is_zero_to_a_year_on_both_settings_paths() {
    let st = state().await;
    let gw = serve(st.clone()).await;
    for name in ["settings_set", "settings_set_full"] {
        for bad in [8761_u64, u64::from(u32::MAX)] {
            let (status, body) = op(&gw, name, json!({"build_update_check_hours": bad})).await;
            assert_eq!(status, 400, "{name} {bad} -> {body}");
            let msg = body["message"].as_str().unwrap_or_default();
            assert!(msg.contains("between 0 (off) and 8760"), "{name}: {msg}");
        }
        let (status, body) = op(&gw, name, json!({"build_update_check_hours": 8760})).await;
        assert_eq!(status, 200, "{name} -> {body}");
        assert_eq!(st.snapshot().settings.build_update_check_hours, 8760);
    }
}

#[tokio::test]
async fn a_settings_blob_from_before_builds_loads_with_the_defaults() {
    let s: lmgw_core::config::Settings = serde_json::from_value(json!({})).unwrap();
    assert_eq!(s.builds_dir, None);
    assert!(s.forge_tokens.is_empty());
    assert_eq!(s.build_update_check_hours, 6);
}

// ---------------------------------------------------------------------------
// The dev flag
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_dev_instance_refuses_image_deletion_and_builds_off_tmpfs() {
    let st = state().await;
    assert!(
        !st.dev(),
        "a test gateway is never a dev instance by accident"
    );
    assert!(st.refuse_in_dev("deleting an image").is_ok());
    assert_eq!(st.builds_dir(), st.data_dir.join("builds"));

    st.set_dev_for_tests(true);
    let msg = st.refuse_in_dev("deleting an image").unwrap_err();
    assert!(msg.contains("deleting an image"), "{msg}");
    assert!(msg.contains("shared with the production"), "{msg}");
    assert_eq!(
        st.builds_dir(),
        lmgw_core::backends::paths::dev_builds_dir(),
        "a dev data dir is on tmpfs, so its builds default elsewhere"
    );
}
