//! Candidate aliases (candidate-aliases design §4.1, §4.6, §7 item 12) — the
//! data-layer surface: `ops::candidate_alias_set`'s save-time rules,
//! `candidates::derive`, `Snapshot::request_fallback`, `/v1/models`
//! publishing and `ops::models kind=alias`.
//!
//! Every candidate row uses `capabilities_override` to pin its facets
//! deterministically (no real chat template/projector needed) — the pattern
//! `capabilities_override.rs`'s own module doc recommends for exactly this
//! reason.

use lmgw_core::candidates::Facet;
use lmgw_core::capabilities::exposed;
use lmgw_core::config::{HoldFallbackMode, Protocol, UpstreamKind};
use lmgw_core::ops::{self, CandidateAliasPatch};
use lmgw_core::runtime::Class;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewLocalModel, NewUpstream};
use lmgw_core::vram::Target;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A header-only GGUF (the header reader needs no tensors) — same shape as
/// `models_endpoint.rs`'s own writer.
enum Kv<'a> {
    Str(&'a str, &'a str),
    U32(&'a str, u32),
}

fn write_gguf(path: &std::path::Path, kvs: &[Kv<'_>]) {
    fn put_str(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
        buf.extend_from_slice(s.as_bytes());
    }
    const VT_U32: u32 = 4;
    const VT_STRING: u32 = 8;
    let mut kv = Vec::new();
    for entry in kvs {
        match entry {
            Kv::Str(k, v) => {
                put_str(&mut kv, k);
                kv.extend_from_slice(&VT_STRING.to_le_bytes());
                put_str(&mut kv, v);
            }
            Kv::U32(k, v) => {
                put_str(&mut kv, k);
                kv.extend_from_slice(&VT_U32.to_le_bytes());
                kv.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"GGUF");
    out.extend_from_slice(&3u32.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&(kvs.len() as u64).to_le_bytes());
    out.extend_from_slice(&kv);
    std::fs::write(path, out).unwrap();
}

async fn fixture() -> (SharedState, tempfile::TempDir) {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    write_gguf(
        &dir.path().join("m.gguf"),
        &[
            Kv::Str("general.architecture", "qwen3"),
            Kv::U32("qwen3.context_length", 131_072),
        ],
    );
    let mut settings = state.snapshot().settings.clone();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    (state, dir)
}

/// A local chat row with deterministic facets (via `capabilities_override`)
/// and a controllable published context/max-output.
#[allow(clippy::too_many_arguments)]
fn local_row(
    id: &str,
    vision: bool,
    audio: bool,
    tool_calls: bool,
    reasoning: bool,
    structured: bool,
    ctx_size: Option<i64>,
    n_predict: Option<i64>,
) -> NewLocalModel {
    let mut modalities = vec!["text".to_string()];
    if vision {
        modalities.push("image".to_string());
    }
    if audio {
        modalities.push("audio".to_string());
    }
    let mut m = NewLocalModel {
        model_id: id.into(),
        gguf_path: "m.gguf".into(),
        params: Default::default(),
        args: vec![],
        idle_seconds: 300,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: Some(json!({
            "capabilities": {
                "vision": vision,
                "input_modalities": modalities,
                "tool_calls": {"kind": if tool_calls { "native" } else { "none" }},
                "reasoning": {"kind": "fixed", "enabled": reasoning},
                "structured_output": {"json_schema": structured, "json_object": structured},
            }
        })),
        ladder: vec![],
    };
    m.params.ctx_size = ctx_size;
    m.params.n_predict = n_predict;
    m
}

/// Every facet on, generous context — the common case for tests that only
/// care about the alias mechanics, not any one facet.
fn full_row(id: &str) -> NewLocalModel {
    local_row(id, true, true, true, true, true, Some(32_768), Some(4_096))
}

async fn create(state: &SharedState, p: CandidateAliasPatch) -> Result<serde_json::Value, String> {
    ops::candidate_alias_set(state, p).await
}

fn patch(action: &str, alias: &str, candidates: &str) -> CandidateAliasPatch {
    CandidateAliasPatch {
        action: action.to_string(),
        alias: Some(alias.to_string()),
        candidates: Some(candidates.to_string()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// §7 item 12: capability save rules
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_new_alias_enables_every_common_facet() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    store::insert_local_model(&state.db, &full_row("m2"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();

    let out = create(&state, patch("create", "smart", "m1, m2"))
        .await
        .unwrap();
    let enabled: Vec<String> = out["enabled_facets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    for f in Facet::ALL {
        assert!(enabled.contains(&f.as_str().to_string()), "{enabled:?}");
    }
    assert_eq!(out["routable"], json!(["m1", "m2"]));
}

#[tokio::test]
async fn enabling_a_facet_no_candidate_supports_is_refused() {
    let (state, _dir) = fixture().await;
    // Neither candidate supports audio.
    store::insert_local_model(
        &state.db,
        &local_row("m1", true, false, true, true, true, Some(8192), Some(1024)),
    )
    .await
    .unwrap();
    store::insert_local_model(
        &state.db,
        &local_row("m2", true, false, true, true, true, Some(8192), Some(1024)),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    // Named as disabled at create — accepted even though nothing supports
    // it: a create never refuses an uncommon facet (§12 entry 49).
    let mut p = patch("create", "smart", "m1, m2");
    p.capabilities_disabled = Some("audio".to_string());
    create(&state, p).await.unwrap();

    // Switching it on later (clearing the disabled list) is refused: the
    // owner is asking for a facet no candidate supports.
    let mut p = patch("update", "smart", "m1, m2");
    p.clear = Some("capabilities_disabled".to_string());
    let err = create(&state, p).await.unwrap_err();
    assert!(err.contains("audio"), "{err}");
    assert!(err.contains("m1") || err.contains("m2"), "{err}");
}

#[tokio::test]
async fn adding_a_candidate_that_lacks_an_enabled_facet_is_refused_naming_it() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    store::insert_local_model(
        &state.db,
        &local_row("m2", true, false, true, true, true, Some(8192), Some(1024)),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    // m1 alone: audio is common, on by default.
    let out = create(&state, patch("create", "smart", "m1"))
        .await
        .unwrap();
    assert!(out["enabled_facets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "audio"));

    // Adding m2 (no audio) without touching capabilities_disabled: refused.
    let err = create(&state, patch("update", "smart", "m1, m2"))
        .await
        .unwrap_err();
    assert!(err.contains("audio"), "{err}");
    assert!(err.contains("m2"), "{err}");
}

#[tokio::test]
async fn removing_the_odd_one_out_reenables_a_facet_only_if_not_switched_off() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    store::insert_local_model(
        &state.db,
        &local_row("m2", true, false, true, true, true, Some(8192), Some(1024)),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    // audio is not common (m2 lacks it) — simply left off by the lenient
    // create default, never an explicit switch (§12 entry 49).
    let out = create(&state, patch("create", "smart", "m1, m2"))
        .await
        .unwrap();
    assert!(!out["enabled_facets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "audio"));

    // Remove m2 without touching capabilities_disabled: nothing was ever
    // switched off, so audio turns back on by itself now that it is common
    // (only m1 left).
    let out = create(&state, patch("update", "smart", "m1"))
        .await
        .unwrap();
    assert!(
        out["enabled_facets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "audio"),
        "{out}"
    );
}

#[tokio::test]
async fn a_facet_switched_off_by_hand_stays_off_even_once_every_candidate_supports_it() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    store::insert_local_model(
        &state.db,
        &local_row("m2", true, false, true, true, true, Some(8192), Some(1024)),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    // The owner explicitly switches audio off, even though m2 already lacks
    // it — a real switch, not an automatic side effect of it being
    // uncommon.
    let mut p = patch("create", "smart", "m1, m2");
    p.capabilities_disabled = Some("audio".to_string());
    create(&state, p).await.unwrap();

    // Remove m2 (audio becomes common) without touching
    // capabilities_disabled: the explicit switch carries forward, so audio
    // stays off.
    let out = create(&state, patch("update", "smart", "m1"))
        .await
        .unwrap();
    assert!(
        !out["enabled_facets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "audio"),
        "{out}"
    );

    // Clearing capabilities_disabled turns it back on — the owner's own
    // reversal, not an automatic one.
    let mut p = patch("update", "smart", "m1");
    p.clear = Some("capabilities_disabled".to_string());
    let out = create(&state, p).await.unwrap();
    assert!(
        out["enabled_facets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "audio"),
        "{out}"
    );
}

#[tokio::test]
async fn a_candidate_edited_to_drop_a_facet_leaves_routable_with_the_enabled_set_unchanged() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    let m2_id = store::insert_local_model(&state.db, &full_row("m2"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();

    create(&state, patch("create", "smart", "m1, m2"))
        .await
        .unwrap();

    // Edit m2 directly (bypassing candidate_alias_set) to drop vision.
    let mut edited = full_row("m2");
    edited.capabilities_override = Some(json!({
        "capabilities": {
            "vision": false,
            "input_modalities": ["text", "audio"],
            "tool_calls": {"kind": "native"},
            "reasoning": {"kind": "fixed", "enabled": true},
            "structured_output": {"json_schema": true, "json_object": true},
        }
    }));
    store::update_local_model(&state.db, m2_id, &edited)
        .await
        .unwrap();
    let snap = state.reload_snapshot().await.unwrap();

    let row = snap.candidate_alias("smart").unwrap();
    // The stored enabled set is untouched by the outside edit.
    assert!(row.capabilities_enabled.iter().any(|f| f == "vision"));

    let derived = lmgw_core::candidates::derive::derive(&state, &snap, row).await;
    assert_eq!(derived.routable, vec!["m1".to_string()]);
    assert!(
        derived
            .problems
            .iter()
            .any(|p| p.contains("m2") && p.contains("vision")),
        "{:?}",
        derived.problems
    );
}

// ---------------------------------------------------------------------------
// Name uniqueness (§4.1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_candidate_alias_cannot_take_an_existing_local_models_name() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();

    let err = create(&state, patch("create", "m1", "m1"))
        .await
        .unwrap_err();
    assert!(err.contains("m1"), "{err}");
}

#[tokio::test]
async fn a_local_model_cannot_take_an_existing_candidate_aliass_name() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    create(&state, patch("create", "smart", "m1"))
        .await
        .unwrap();

    let err = ops::local_model_set(
        &state,
        ops::LocalModelPatch {
            action: "create".to_string(),
            model_id: Some("smart".to_string()),
            gguf_path: Some("m.gguf".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(err.contains("smart"), "{err}");
}

/// Review finding X4: `candidate_alias_name_taken`'s plain-alias check used
/// to look up a lowercased key in `Snapshot.aliases`, which is keyed by the
/// alias's literal case — so it almost never matched, and a candidate alias
/// could silently shadow a mixed-case plain alias. Candidate-alias lookup
/// (`Snapshot::candidate_alias`, used by `resolve`) is case-insensitive, so
/// the collision check must be too.
#[tokio::test]
async fn a_candidate_alias_cannot_take_an_existing_plain_aliass_name_case_insensitively() {
    let (state, _dir) = fixture().await;
    let mock = MockServer::start().await;
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "cloud".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "Claude-Fast".into(),
            upstream_id: up,
            upstream_model_id: "gpt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();

    // Differs only in case from the plain alias above.
    let err = create(&state, patch("create", "claude-fast", "m1"))
        .await
        .unwrap_err();
    assert!(err.contains("plain alias"), "{err}");
}

/// Review finding X4, the other half: a plain alias whose name a candidate
/// alias already shadows (data predating this fix, or a race) must stay
/// editable — an owner cannot fix a name collision they never chose to create
/// if the very act of editing the row they own is refused over it.
#[tokio::test]
async fn a_shadowed_plain_alias_stays_editable() {
    let (state, _dir) = fixture().await;
    let mock = MockServer::start().await;
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "cloud".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "Claude-Fast".into(),
            upstream_id: up,
            upstream_model_id: "gpt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    // Inserted directly rather than through `candidate_alias_set`, which now
    // refuses this at create time (the test above) — this simulates a row
    // that predates the fix.
    store::insert_candidate_alias(
        &state.db,
        &store::NewCandidateAlias {
            alias: "claude-fast".into(),
            candidates: vec!["m1".into()],
            background: false,
            fallback_mode: HoldFallbackMode::Inherit,
            fallback: None,
            capabilities_disabled: vec![],
            capabilities_enabled: vec![],
            enabled: true,
            notes: String::new(),
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    // Disabling (Models-list menu) must not be refused...
    let out = ops::model_set(
        &state,
        ops::AliasPatch {
            action: "disable".into(),
            alias: Some("Claude-Fast".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], true, "{out}");

    // ...and neither must an unrelated update (no rename) once it is back
    // on.
    let out = ops::model_set(
        &state,
        ops::AliasPatch {
            action: "enable".into(),
            alias: Some("Claude-Fast".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], true, "{out}");
}

#[tokio::test]
async fn a_candidate_alias_cannot_take_an_existing_plain_aliass_name() {
    let (state, _dir) = fixture().await;
    let mock = MockServer::start().await;
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "cloud".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "plain".into(),
            upstream_id: up,
            upstream_model_id: "gpt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();

    let err = create(&state, patch("create", "plain", "m1"))
        .await
        .unwrap_err();
    assert!(err.contains("plain"), "{err}");
}

// ---------------------------------------------------------------------------
// Save refusals (§4.1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_empty_candidate_list_is_refused() {
    let (state, _dir) = fixture().await;
    // Passes `opt()`'s "not supplied" filter (it is non-blank after trim,
    // commas survive) but splits into zero real ids — `refuse_empty_or_
    // duplicate`'s own check, not `require_all`'s.
    let err = create(&state, patch("create", "smart", " , "))
        .await
        .unwrap_err();
    assert!(err.to_lowercase().contains("empty"), "{err}");
}

#[tokio::test]
async fn a_duplicate_candidate_is_refused() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let err = create(&state, patch("create", "smart", "m1, m1"))
        .await
        .unwrap_err();
    assert!(err.contains("m1"), "{err}");
}

#[tokio::test]
async fn a_local_fallback_is_refused() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    store::insert_local_model(&state.db, &full_row("m2"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();

    let mut p = patch("create", "smart", "m1");
    p.fallback_mode = Some(HoldFallbackMode::Alias);
    p.fallback = Some("m2".to_string());
    let err = create(&state, p).await.unwrap_err();
    assert!(err.contains("local"), "{err}");
}

#[tokio::test]
async fn a_candidate_alias_cannot_fall_back_to_another_candidate_alias() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    store::insert_local_model(&state.db, &full_row("m2"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    create(&state, patch("create", "other", "m2"))
        .await
        .unwrap();

    let mut p = patch("create", "smart", "m1");
    p.fallback_mode = Some(HoldFallbackMode::Alias);
    p.fallback = Some("other".to_string());
    let err = create(&state, p).await.unwrap_err();
    assert!(err.contains("candidate alias"), "{err}");
}

// ---------------------------------------------------------------------------
// `Snapshot::request_fallback`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_fallback_reads_the_alias_fallback_for_a_candidate_alias_name() {
    let (state, _dir) = fixture().await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "cloud".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: mock.uri(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "cloud-fb".into(),
            upstream_id: up,
            upstream_model_id: "gpt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    store::insert_local_model(&state.db, &full_row("row-fb"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let mut p = patch("create", "smart", "m1");
    p.fallback_mode = Some(HoldFallbackMode::Alias);
    p.fallback = Some("cloud-fb".to_string());
    // The mock upstream publishes no catalog, so its capabilities are
    // entirely unknown — disable every facet so the fallback's
    // every-enabled-facet check has nothing to hold it to (this test is
    // about `request_fallback`'s routing, not capability matching).
    p.capabilities_disabled = Some(
        Facet::ALL
            .iter()
            .map(|f| f.as_str())
            .collect::<Vec<_>>()
            .join(","),
    );
    create(&state, p).await.unwrap();

    // `row-fb`'s own row fallback would answer for a *direct* request to it;
    // through the candidate alias, the alias' own fallback answers instead.
    let mut row_fb = full_row("row-fb");
    row_fb.hold_fallback_mode = HoldFallbackMode::Alias;
    row_fb.hold_fallback = Some("cloud-fb".to_string());

    let snap = state.reload_snapshot().await.unwrap();
    let target = Target {
        class: Class::Chat,
        model_id: "m1".to_string(),
    };
    match snap.request_fallback("smart", &target) {
        lmgw_core::config::FallbackRoute::Usable { alias, .. } => assert_eq!(alias, "cloud-fb"),
        other => panic!("expected Usable, got {other:?}"),
    }
    // A direct model name (not a candidate alias) still reads the row's own
    // fallback via `fallback_route`.
    match snap.request_fallback("m1", &target) {
        lmgw_core::config::FallbackRoute::None => {}
        other => panic!("m1 carries no row fallback; expected None, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// `/v1/models` publishing and `ops::models`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn v1_models_shows_exactly_the_enabled_set_and_the_minimum_context() {
    let (state, _dir) = fixture().await;
    // m1: ctx 8192, max_out 1024; m2: ctx 4096, max_out 2048 — minimum wins.
    store::insert_local_model(
        &state.db,
        &local_row("m1", true, true, true, true, true, Some(8192), Some(1024)),
    )
    .await
    .unwrap();
    store::insert_local_model(
        &state.db,
        &local_row("m2", true, true, true, true, true, Some(4096), Some(2048)),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    create(&state, patch("create", "smart", "m1, m2"))
        .await
        .unwrap();
    let snap = state.reload_snapshot().await.unwrap();

    let entry = exposed::exposed_entry(&state, "smart").await.unwrap();
    assert_eq!(entry.context_length, Some(4096));
    assert_eq!(entry.max_output_tokens, Some(1024));
    let caps = entry.capabilities.unwrap();
    assert_eq!(caps.vision, Some(true));
    assert_eq!(
        caps.tool_calls.as_ref().map(|t| t.kind.as_str()),
        Some("native")
    );
    assert!(caps.reasoning.is_some());
    drop(snap);
}

#[tokio::test]
async fn v1_models_context_is_absent_when_one_candidate_is_unknown() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(
        &state.db,
        &local_row("m1", true, true, true, true, true, Some(8192), Some(1024)),
    )
    .await
    .unwrap();
    // No ctx_size at all: llama-server's `--fit` decides it at start, so
    // lmgw publishes nothing rather than a guess.
    store::insert_local_model(
        &state.db,
        &local_row("m2", true, true, true, true, true, None, Some(2048)),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    create(&state, patch("create", "smart", "m1, m2"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();

    let entry = exposed::exposed_entry(&state, "smart").await.unwrap();
    assert_eq!(
        entry.context_length, None,
        "one candidate's context is unknown"
    );
    assert_eq!(entry.max_output_tokens, Some(1024));
}

#[tokio::test]
async fn a_disabled_facet_publishes_an_explicit_negative_not_absence() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let mut p = patch("create", "smart", "m1");
    p.capabilities_disabled = Some("audio,tool_calls,structured_output,reasoning".to_string());
    create(&state, p).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let entry = exposed::exposed_entry(&state, "smart").await.unwrap();
    let caps = entry.capabilities.unwrap();
    assert_eq!(caps.vision, Some(true));
    assert_eq!(
        caps.input_modalities
            .as_ref()
            .map(|m| m.iter().any(|x| x == "audio")),
        Some(false)
    );
    assert_eq!(
        caps.tool_calls.as_ref().map(|t| t.kind.as_str()),
        Some("none")
    );
    let so = caps.structured_output.unwrap();
    assert_eq!(so.json_schema, Some(false));
    assert_eq!(so.json_object, Some(false));
    // Reasoning has no negative shape in the schema: disabled means absent.
    assert!(caps.reasoning.is_none());
}

#[tokio::test]
async fn ops_models_kind_alias_lists_candidate_aliases_alongside_plain_ones() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    create(&state, patch("create", "smart", "m1"))
        .await
        .unwrap();

    let out = ops::models(&state, Some("alias"), None).await.unwrap();
    let entries = out["models"].as_array().unwrap();
    let found = entries
        .iter()
        .find(|e| e["name"] == "smart")
        .unwrap_or_else(|| panic!("{entries:?}"));
    assert_eq!(found["kind"], "candidate_alias");
    assert_eq!(found["candidates"], json!(["m1"]));
}

// ---------------------------------------------------------------------------
// `preview` (UI phase, §6): the editor's live draft check — never writes.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preview_of_a_fresh_draft_reports_facets_and_addable_without_saving() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    // m2 lacks audio.
    store::insert_local_model(
        &state.db,
        &local_row("m2", true, false, true, true, true, Some(8192), Some(1024)),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let out = create(&state, patch("preview", "smart", "m1"))
        .await
        .unwrap();
    assert_eq!(out["ok"], true, "{out}");
    assert!(out["error"].is_null(), "{out}");
    assert!(out["enabled_facets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "audio"));
    // m2 is not a candidate yet and would drop audio if added now.
    let addable = out["addable"].as_array().unwrap();
    let m2 = addable
        .iter()
        .find(|a| a["id"] == "m2")
        .unwrap_or_else(|| panic!("{addable:?}"));
    assert_eq!(m2["missing"], json!(["audio"]));
    // m1 is already a candidate, so it is not offered again.
    assert!(!addable.iter().any(|a| a["id"] == "m1"));

    // Nothing was written: no row exists under this name.
    assert!(store::list_candidate_aliases(&state.db)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn preview_of_an_empty_draft_fails_but_still_derives() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();

    let out = create(&state, patch("preview", "smart", "")).await.unwrap();
    assert_eq!(out["ok"], false, "{out}");
    assert!(out["error"].as_str().unwrap().contains("empty"), "{out}");
    // Nothing to be common over, so every facet is vacuously common — the
    // editor still gets a coherent `addable` list to build the picker from.
    assert!(out["addable"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["id"] == "m1"));
}

#[tokio::test]
async fn preview_of_a_facet_drop_names_the_facet_but_still_derives() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    store::insert_local_model(
        &state.db,
        &local_row("m2", true, false, true, true, true, Some(8192), Some(1024)),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    create(&state, patch("create", "smart", "m1"))
        .await
        .unwrap();

    // Previewing "add m2" (not yet saved) over the real row: audio would be
    // dropped, so the preview fails, but the facet breakdown is still there
    // for the editor to grey out the toggle and explain why.
    let out = create(&state, patch("preview", "smart", "m1, m2"))
        .await
        .unwrap();
    assert_eq!(out["ok"], false, "{out}");
    assert!(out["error"].as_str().unwrap().contains("audio"), "{out}");
    assert_eq!(out["unsupported_by"]["audio"], json!(["m2"]), "{out}");

    // Still not written: the saved row is unchanged.
    let saved = create(&state, patch("preview", "smart", "m1"))
        .await
        .unwrap();
    assert!(saved["enabled_facets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "audio"));
}

#[tokio::test]
async fn preview_of_a_bad_fallback_names_it_but_still_derives() {
    let (state, _dir) = fixture().await;
    store::insert_local_model(&state.db, &full_row("m1"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();

    let mut p = patch("preview", "smart", "m1");
    p.fallback_mode = Some(HoldFallbackMode::Alias);
    // No `fallback` named at all: resolve_fallback refuses.
    let out = create(&state, p).await.unwrap();
    assert_eq!(out["ok"], false, "{out}");
    assert!(out["error"].as_str().unwrap().contains("fallback"), "{out}");
    // The candidate-side derivation still ran.
    assert!(out["enabled_facets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "vision"));
}
