//! `GET /api/settings-full` over the real router: the live answer is read into
//! the document's `SettingsFull` and written back equal (`common::round_trips`),
//! on the defaults and on every block set away from them, so a key the server
//! sends without the type, or the type without the server, fails here. The
//! text of the answer is pinned where the type alone cannot say: a secret never
//! appears, and an `f32` is written as the `f64` of itself, as it always was.

use std::path::Path;

use lmgw_api_types::{AdminLevel, SettingsFull};
use lmgw_core::config::{BudgetPeriod, CatalogRevision, SelfAdmin, Settings};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::Value;

use crate::common::{self, Gw};

/// The answer's text and its value.
async fn read(gw: &Gw) -> (String, Value) {
    let r = gw
        .client()
        .get(format!("{gw}/api/settings-full"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    let live = serde_json::from_str(&text).unwrap();
    (text, live)
}

async fn save(state: &SharedState, s: &Settings) {
    store::save_settings(&state.db, s).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

#[tokio::test]
async fn the_defaults_are_the_documented_type() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state.clone()).await;
    let (_, live) = read(&gw).await;
    let full: SettingsFull = common::round_trips("settings-full defaults", &live);

    let d = Settings::default();
    assert_eq!(full.chat_archive_days, d.chat_archive_days);
    assert_eq!(full.chat_purge_days, d.chat_purge_days);
    assert_eq!(full.self_admin, AdminLevel::ReadOnly);
    assert_eq!(full.router.image, d.router.image);
    assert_eq!(full.audio.catalog_revision, "pinned");
    assert!(full.vram.fallback_on_external && full.vram.enabled);
    assert_eq!(full.hold, Default::default());
    assert_eq!(full.docs_search.rrf_k, 60.0);
    assert!(full.api_keys.iter().any(|k| k.name == "owner:dashboard"));
    assert_eq!(full.builds_dir, None);
    assert_eq!(full.agent_origin_suffix_warning, None);
    assert!(full.chat_voice_language_notes.is_empty());
    assert_eq!(full.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(full.data_dir, state.data_dir.display().to_string());
    // The CPUs are read once from this machine.
    assert!(full.host_cpu.logical_cpus >= 1);
    assert!(["topology", "logical"].contains(&full.host_cpu.source.as_str()));
}

#[tokio::test]
async fn every_block_set_away_from_its_default_reads_back_as_set() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state.clone()).await;
    let mut s = state.snapshot().settings.clone();
    s.global_budget_period = BudgetPeriod::Day;
    s.global_budget_micro = 12_500_000;
    s.currency = "EUR".into();
    s.router.extra_run_args = vec!["--a".into(), "--b=1".into()];
    s.router.request_timeout_seconds = 0;
    s.audio.backend = "cpu".into();
    s.audio.threads = 6;
    s.audio.max_request_body_mb = 64;
    s.audio.catalog_revision = CatalogRevision::Latest;
    s.image.extra_run_args = vec!["--sd".into()];
    s.realtime.default_model = "m1".into();
    s.realtime.threshold = 0.35;
    s.mcp.host_max_message_mb = 33;
    s.vram.enabled = false;
    s.vram.fallback_on_external = false;
    s.vram.budget_mb = 8192;
    s.hold.active = true;
    s.hold.fallback_alias = Some("openai/gpt-y".into());
    s.chat_archive_days = 0;
    s.chat_purge_days = 90;
    s.chat_system_prompt = Some("Be terse.".into());
    s.self_admin = SelfAdmin::Full;
    s.docs_search.rrf_k = 0.1;
    s.docs_search.fts_weights.derived_summary = 0.35;
    s.docs_search.k_rerank = 0;
    s.hf_token = "hf_never_shown".into();
    s.update_token = "upd_never_shown".into();
    s.forge_tokens
        .insert("github.com".into(), "ghp_never_shown".into());
    save(&state, &s).await;

    let (text, live) = read(&gw).await;
    let full: SettingsFull = common::round_trips("settings-full set", &live);
    assert_eq!(full.global_budget_period, "day");
    assert_eq!(full.global_budget_micro, 12_500_000);
    assert_eq!(full.router.extra_run_args, ["--a", "--b=1"]);
    assert_eq!(full.router.request_timeout_seconds, 0);
    assert_eq!(full.audio.catalog_revision, "latest");
    assert_eq!(full.audio.max_request_body_mb, 64);
    assert_eq!(full.image.extra_run_args, ["--sd"]);
    assert_eq!(full.realtime.default_model, "m1");
    assert_eq!(full.mcp.host_max_message_mb, 33);
    assert!(!full.vram.enabled && !full.vram.fallback_on_external);
    assert_eq!(full.vram.budget_mb, 8192);
    assert!(full.hold.active);
    assert_eq!(full.hold.fallback_alias.as_deref(), Some("openai/gpt-y"));
    assert_eq!((full.chat_archive_days, full.chat_purge_days), (0, 90));
    assert_eq!(full.chat_system_prompt, "Be terse.");
    assert_eq!(
        full.chat_system_prompt_builtin,
        lmgw_core::config::BUILTIN_CHAT_SYSTEM_PROMPT
    );
    assert_eq!(full.self_admin, AdminLevel::Full);
    assert_eq!(full.docs_search.k_rerank, 0);

    // A secret is the flag that says one is stored, and never its text.
    assert!(full.has_hf_token && full.has_update_token);
    assert_eq!(full.forge_tokens["github.com"], "<set>");
    assert!(!text.contains("never_shown"), "{text}");

    // The ratios are `f32`, written as the `f64` of themselves: `0.1` is
    // `0.10000000149011612`, as the answer was when it was built with `json!`.
    assert!(text.contains(r#""rrf_k":0.10000000149011612"#), "{text}");
    assert!(
        text.contains(r#""derived_summary":0.3499999940395355"#),
        "{text}"
    );
}

#[tokio::test]
async fn what_is_absent_is_null_and_what_is_set_is_the_value() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state.clone()).await;
    let mut s = state.snapshot().settings.clone();
    // The built-in profile a new thread could start with: it is seeded, and a
    // setting that names a profile that is gone is cleared on save.
    let profiles: Value = gw
        .client()
        .get(format!("{gw}/chat/api/profiles"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let profile = profiles["profiles"][0]["id"].as_i64().unwrap();
    s.chat_profile = Some(profile);
    s.builds_dir = Some("/dev/shm/lmgw-test-never-created/builds".into());
    s.agent_origin_suffix = "local".into();
    save(&state, &s).await;
    let (_, live) = read(&gw).await;
    let full: SettingsFull = common::round_trips("settings-full named", &live);
    assert_eq!(full.chat_profile, Some(profile));
    assert_eq!(
        full.builds_dir.as_deref(),
        Some("/dev/shm/lmgw-test-never-created/builds")
    );
    assert_eq!(full.builds_dir_effective, full.builds_dir.clone().unwrap());
    // Asked per read: the warning is whatever the directory's filesystem says.
    assert_eq!(
        full.builds_dir_warning,
        lmgw_core::backends::paths::tmpfs_refusal(Path::new(&full.builds_dir_effective))
    );
    assert!(full.agent_origin_suffix_warning.is_some(), "{live}");

    s.chat_profile = None;
    s.builds_dir = None;
    s.agent_origin_suffix = "localhost".into();
    save(&state, &s).await;
    let (_, live) = read(&gw).await;
    let full: SettingsFull = common::round_trips("settings-full unset", &live);
    for key in ["chat_profile", "builds_dir", "agent_origin_suffix_warning"] {
        assert!(live[key].is_null(), "{key} is sent as null: {live}");
    }
    assert_eq!(full.chat_profile, None);
    // The default directory is spelled out, and judged like any other.
    assert_ne!(full.builds_dir_effective, "");
    assert_eq!(
        full.builds_dir_warning,
        lmgw_core::backends::paths::tmpfs_refusal(Path::new(&full.builds_dir_effective))
    );
}
