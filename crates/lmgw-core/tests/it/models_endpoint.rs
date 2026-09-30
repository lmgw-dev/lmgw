//! `GET /v1/models/{id}` and the list-level `lmgw` block — the model
//! capabilities design's §2.1/§2.2 wire shape, over the real router.
//!
//! What this file pins that `web_pages.rs` cannot: the single-model route
//! exists at all, it captures ids containing `/` (a passthrough id carries its
//! upstream prefix *and* the provider's own slashes), it answers and fails in
//! whichever dialect the caller speaks, `created` is stable per process rather
//! than `now()`, and an unreadable model file degrades to "listed, with a note
//! naming it" instead of a 500 or a silent hole.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::ir::{Params, ReasoningControl};
use lmgw_core::ladder::Rung;
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewLocalModel, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const QWEN38_TEMPLATE: &str = include_str!("../fixtures/chat_templates/qwen3.8.jinja");
const MEDGEMMA_TEMPLATE: &str = include_str!("../fixtures/chat_templates/medgemma.jinja");

/// The passthrough id under test: an upstream prefix plus a provider id that
/// already contains a slash. `GET /v1/models/{id}` has to capture all of it.
const NESTED_ID: &str = "kilo/anthropic/claude-sonnet-5";

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A live gateway with one local chat row (real template + projector on disk),
/// one row whose weights are missing, and one expose-all cloud upstream whose
/// catalog entry id contains a slash.
struct Gateway {
    base: String,
    // Kept alive for the duration of a test: the models dir the handler reads
    // and the upstreams it fetches from.
    _dir: tempfile::TempDir,
    _mock: MockServer,
    _gemini: MockServer,
    _state: SharedState,
}

/// A header-only GGUF with exactly the given keys (the header reader needs no
/// tensors). String, u32 and bool are the three value types the capability
/// derivation reads.
enum Kv<'a> {
    Str(&'a str, &'a str),
    U32(&'a str, u32),
    Bool(&'a str, bool),
}

fn write_gguf(path: &std::path::Path, kvs: &[Kv<'_>]) {
    fn put_str(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
        buf.extend_from_slice(s.as_bytes());
    }
    const VT_U32: u32 = 4;
    const VT_BOOL: u32 = 7;
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
            Kv::Bool(k, v) => {
                put_str(&mut kv, k);
                kv.extend_from_slice(&VT_BOOL.to_le_bytes());
                kv.push(u8::from(*v));
            }
        }
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"GGUF");
    out.extend_from_slice(&3u32.to_le_bytes()); // version
    out.extend_from_slice(&0u64.to_le_bytes()); // tensor count
    out.extend_from_slice(&(kvs.len() as u64).to_le_bytes());
    out.extend_from_slice(&kv);
    std::fs::write(path, out).unwrap();
}

fn image_row(model_id: &str) -> store::NewImageModel {
    store::NewImageModel {
        model_id: model_id.into(),
        files: serde_json::Map::new(),
        args: serde_json::Map::new(),
        modes: vec![],
        edit: false,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        idle_seconds: 0,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
    }
}

fn local(model_id: &str, gguf: &str) -> NewLocalModel {
    NewLocalModel {
        model_id: model_id.into(),
        gguf_path: gguf.into(),
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
        capabilities_override: None,
        ladder: vec![],
    }
}

async fn gateway() -> Gateway {
    let dir = tempfile::tempdir().unwrap();
    write_gguf(
        &dir.path().join("qwen3.8-27b.gguf"),
        &[
            Kv::Str("general.architecture", "qwen3"),
            Kv::U32("qwen3.context_length", 262_144),
            Kv::Str("tokenizer.chat_template", QWEN38_TEMPLATE),
        ],
    );
    write_gguf(
        &dir.path().join("mmproj-qwen3.8.gguf"),
        &[
            Kv::Str("general.architecture", "clip"),
            Kv::Str("general.type", "mmproj"),
            Kv::Bool("clip.has_vision_encoder", true),
            Kv::Bool("clip.has_audio_encoder", true),
        ],
    );

    // A small trained context, for the unified-KV published-context tests
    // below: proof that the new formula never publishes more than the model
    // actually supports, even when nothing else configured on the row would
    // catch it.
    std::fs::create_dir(dir.path().join("smallctx")).unwrap();
    write_gguf(
        &dir.path().join("smallctx").join("weights.gguf"),
        &[
            Kv::Str("general.architecture", "qwen3"),
            Kv::U32("qwen3.context_length", 4_096),
            Kv::Str("tokenizer.chat_template", QWEN38_TEMPLATE),
        ],
    );

    // Each of the projector/template cases gets its own subdirectory, so the
    // sibling-projector scan of one is not the sibling-projector scan of
    // another.
    for sub in ["twin", "broken", "tmpl"] {
        std::fs::create_dir(dir.path().join(sub)).unwrap();
        write_gguf(
            &dir.path().join(sub).join("weights.gguf"),
            &[
                Kv::Str("general.architecture", "qwen3"),
                Kv::U32("qwen3.context_length", 262_144),
                Kv::Str("tokenizer.chat_template", QWEN38_TEMPLATE),
            ],
        );
    }
    // A projector nobody configured, next to the weights.
    write_gguf(
        &dir.path().join("twin").join("mmproj-twin.gguf"),
        &[
            Kv::Str("general.type", "mmproj"),
            Kv::Bool("clip.has_vision_encoder", true),
        ],
    );
    // A configured projector that is not a GGUF at all.
    std::fs::write(
        dir.path().join("broken").join("mmproj-broken.gguf"),
        b"nope",
    )
    .unwrap();
    // A chat-template override file: a *different* model's template, so the
    // published facts visibly come from the file rather than the GGUF.
    std::fs::write(
        dir.path().join("tmpl").join("medgemma.jinja"),
        MEDGEMMA_TEMPLATE,
    )
    .unwrap();

    let state = AppState::init_for_tests().await.unwrap();

    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": "anthropic/claude-sonnet-5",
                "context_length": 200_000,
                "created": 1_700_000_000i64,
                "architecture": {
                    "input_modalities": ["text", "image"],
                    "output_modalities": ["text"],
                },
                "supported_parameters": ["tools", "reasoning"],
                "top_provider": {"max_completion_tokens": 64_000},
            }]
        })))
        .mount(&mock)
        .await;
    store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "kilo".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: true,
            expose_prefix: "kilo".into(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();

    let mut settings = state.snapshot().settings.clone();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();

    let mut chat = local("qwen3.8-27b", "qwen3.8-27b.gguf");
    chat.params.ctx_size = Some(131_072);
    chat.params.n_predict = Some(32_768);
    chat.params.reasoning = Some("on".into());
    chat.params.mmproj_path = Some("mmproj-qwen3.8.gguf".into());
    store::insert_local_model(&state.db, &chat).await.unwrap();

    // Unified-KV published-context rows (design §3.2 — `LlamaParams::per_request_ctx`).
    // Split: unchanged from before the toggle existed.
    let mut kv_split = local("kv-split", "qwen3.8-27b.gguf");
    kv_split.params.kv_unified = Some(false);
    kv_split.params.ctx_size = Some(32_768);
    kv_split.params.parallel = Some(4);
    kv_split.params.n_predict = Some(4_096);
    store::insert_local_model(&state.db, &kv_split)
        .await
        .unwrap();
    // Explicit unified: no `ctx_size`, so the pool is sized from the per-slot
    // cap (`np * N`), and the cap itself binds the per-request number.
    let mut kv_unified_cap = local("kv-unified-cap", "qwen3.8-27b.gguf");
    kv_unified_cap.params.kv_unified = Some(true);
    kv_unified_cap.params.parallel = Some(4);
    kv_unified_cap.params.kv_unified_per_slot = Some(4_096);
    kv_unified_cap.params.n_predict = Some(4_096);
    store::insert_local_model(&state.db, &kv_unified_cap)
        .await
        .unwrap();
    // Auto default (today's shape) whose `ctx_size` is larger than the
    // model's trained context: the new formula must still cap it, unlike
    // dividing `ctx_size` by `parallel.unwrap_or(1)` alone.
    let mut kv_small_trained = local("kv-small-trained", "smallctx/weights.gguf");
    kv_small_trained.params.ctx_size = Some(8_192);
    store::insert_local_model(&state.db, &kv_small_trained)
        .await
        .unwrap();
    // Auto default with *no* `ctx_size` at all: before review finding 2 the
    // trained context (4096) would have been published here, borrowed as a
    // stand-in pool size llama-server never actually promised (`--fit` can
    // shrink an unset `ctx_size` to whatever memory allows). Now the pool
    // itself is unknown, so nothing is — the exact behaviour from before this
    // toggle existed.
    store::insert_local_model(&state.db, &local("kv-auto-no-ctx", "smallctx/weights.gguf"))
        .await
        .unwrap();

    // A ladder row (ladder design §4.4): `/v1/models` must publish the
    // **top** rung's per-slot context, not the base's — every rung here
    // shares the same GGUF (only `ctx_size` differs), which is all this
    // capabilities-publishing test needs; the tokenizer-identity rule is
    // `ops::validate_ladder`'s job, exercised in
    // `crates/lmgw-core/src/ops/tests.rs`'s own unit tests, not this direct
    // `store::insert_local_model` path.
    let mut ladder_model = local("ladder-model", "qwen3.8-27b.gguf");
    ladder_model.params.ctx_size = Some(32_768);
    ladder_model.params.parallel = Some(2);
    ladder_model.params.n_predict = Some(4_096);
    ladder_model.ladder = vec![
        Rung {
            gguf_path: "qwen3.8-27b.gguf".into(),
            ctx_size: 65_536,
        },
        Rung {
            gguf_path: "qwen3.8-27b.gguf".into(),
            ctx_size: 131_072,
        },
    ];
    store::insert_local_model(&state.db, &ladder_model)
        .await
        .unwrap();
    // A ladder saved before §4.3's trained-context rule (review finding 1):
    // its top rung is configured at 8192 per slot on weights trained at 4096,
    // and llama-server caps the slot there.
    let mut capped_ladder = local("capped-ladder", "smallctx/weights.gguf");
    capped_ladder.params.ctx_size = Some(4_096);
    capped_ladder.params.parallel = Some(2);
    capped_ladder.params.n_predict = Some(512);
    capped_ladder.ladder = vec![Rung {
        gguf_path: "smallctx/weights.gguf".into(),
        ctx_size: 16_384,
    }];
    store::insert_local_model(&state.db, &capped_ladder)
        .await
        .unwrap();

    // A row whose weights are not on disk: the listing must still carry it.
    store::insert_local_model(&state.db, &local("gone", "vanished-weights.gguf"))
        .await
        .unwrap();

    // …a projector sitting unconfigured next to the weights, once as an
    // oversight and once as a decision (`--no-mmproj`).
    store::insert_local_model(&state.db, &local("twin", "twin/weights.gguf"))
        .await
        .unwrap();
    let mut on_purpose = local("twin-text-only", "twin/weights.gguf");
    on_purpose.params.no_mmproj = true;
    store::insert_local_model(&state.db, &on_purpose)
        .await
        .unwrap();

    // …a configured projector whose header cannot be read.
    let mut broken_proj = local("broken-projector", "broken/weights.gguf");
    broken_proj.params.mmproj_path = Some("broken/mmproj-broken.gguf".into());
    store::insert_local_model(&state.db, &broken_proj)
        .await
        .unwrap();

    // …a chat-template override that exists, and one that does not.
    let mut overridden = local("template-override", "tmpl/weights.gguf");
    overridden.params.chat_template_file = Some("tmpl/medgemma.jinja".into());
    store::insert_local_model(&state.db, &overridden)
        .await
        .unwrap();
    let mut missing_tmpl = local("template-missing", "tmpl/weights.gguf");
    missing_tmpl.params.chat_template_file = Some("tmpl/nope.jinja".into());
    store::insert_local_model(&state.db, &missing_tmpl)
        .await
        .unwrap();

    // …and a row the owner has corrected by hand (§7).
    let mut owned = local("owner-corrected", "tmpl/weights.gguf");
    owned.capabilities_override = Some(json!({
        "capabilities": {"input_modalities": ["text", "image"], "vision": true},
        "max_output_tokens": 4096,
        "notes": ["Verified against a live probe on 2026-09-17."]
    }));
    store::insert_local_model(&state.db, &owned).await.unwrap();

    // …and one whose stored override is not JSON at all (hand-edited column,
    // or written by an older build): it must survive the read as *something*
    // that gets rejected out loud, not vanish into `None`.
    store::insert_local_model(&state.db, &local("owner-broken", "tmpl/weights.gguf"))
        .await
        .unwrap();
    sqlx::query("UPDATE local_models SET capabilities_override = ?1 WHERE model_id = ?2")
        .bind("{not json at all")
        .bind("owner-broken")
        .execute(&state.db)
        .await
        .unwrap();

    // Three image rows (image-generation design §5): a plain generator with
    // its own default flags, an edit pipeline, and a video-only one — plus a
    // disabled row that must not appear anywhere.
    let mut z = image_row("z-image-turbo");
    z.args.insert("width".into(), json!(1024));
    z.args.insert("height".into(), json!(1024));
    z.args.insert("steps".into(), json!(8));
    store::insert_image_model(&state.db, &z).await.unwrap();

    let mut kontext = image_row("flux-kontext");
    kontext.edit = true;
    store::insert_image_model(&state.db, &kontext)
        .await
        .unwrap();

    let mut wan = image_row("wan-2.2");
    wan.modes = vec!["vid_gen".into()];
    wan.capabilities_override = Some(json!({
        "capabilities": {"endpoints": ["/v1/images/generations"], "source": "owner"},
        "notes": ["Video comes back through the native job API only; see the spec."]
    }));
    store::insert_image_model(&state.db, &wan).await.unwrap();

    let mut off = image_row("not-installed");
    off.enabled = false;
    store::insert_image_model(&state.db, &off).await.unwrap();

    // An alias onto the local row, with its own reasoning default (§3.6): its
    // capabilities must come from the backing row, not from the catalog of the
    // llama-server upstream it nominally points at.
    let router_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "local-router".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::LlamaServer,
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
            alias: "qwen-thorough".into(),
            upstream_id: router_id,
            upstream_model_id: "qwen3.8-27b".into(),
            param_overrides: Params {
                reasoning: Some(ReasoningControl {
                    effort: Some("xhigh".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();

    // A Gemini-protocol upstream, exposed whole: a second wire shape through
    // the same handler.
    let gemini = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{
                "name": "models/gemini-3.0-pro",
                "inputTokenLimit": 1_048_576,
                "outputTokenLimit": 65_536,
                "supportedGenerationMethods": ["generateContent", "countTokens"],
                "thinking": true
            }]
        })))
        .mount(&gemini)
        .await;
    store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "gem".into(),
            protocol: Protocol::Gemini,
            kind: UpstreamKind::Generic,
            base_url: gemini.uri(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: true,
            expose_prefix: "gem".into(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();

    state.reload_snapshot().await.unwrap();

    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    Gateway {
        base,
        _dir: dir,
        _mock: mock,
        _gemini: gemini,
        _state: state,
    }
}

/// The `notes` array of a model object, as plain strings.
fn notes(v: &Value) -> Vec<String> {
    v["notes"]
        .as_array()
        .unwrap_or_else(|| panic!("no notes on {v}"))
        .iter()
        .map(|n| n.as_str().unwrap_or_default().to_string())
        .collect()
}

#[track_caller]
fn assert_some_note(v: &Value, needle: &str) {
    let all = notes(v);
    assert!(
        all.iter().any(|n| n.contains(needle)),
        "no note containing {needle:?}: {all:#?}"
    );
}

/// `(status, body)` of a GET, optionally in the Anthropic dialect.
async fn get(base: &str, path: &str, anthropic: bool) -> (u16, Value) {
    let mut rb = reqwest::Client::new().get(format!("{base}{path}"));
    if anthropic {
        rb = rb.header("anthropic-version", "2023-06-01");
    }
    let resp = rb.send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let v = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("GET {path} is not JSON ({e}): {text}"));
    (status, v)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A local id, on its own route, in both dialects: the same capability object,
/// under each SDK's own top-level names (design §2.1, §2.2).
#[tokio::test]
async fn a_local_model_is_served_by_id_in_both_dialects() {
    let gw = gateway().await;

    let (status, openai) = get(&gw.base, "/v1/models/qwen3.8-27b", false).await;
    assert_eq!(status, 200, "{openai}");
    assert_eq!(openai["id"].as_str(), Some("qwen3.8-27b"));
    assert_eq!(openai["object"].as_str(), Some("model"));
    assert_eq!(openai["owned_by"].as_str(), Some("llama-server"));
    assert_eq!(openai["context_length"].as_u64(), Some(131_072));
    assert_eq!(openai["max_output_tokens"].as_u64(), Some(32_768));
    assert_eq!(
        openai["capabilities"]["source"].as_str(),
        Some("gguf+config"),
        "{openai}"
    );
    assert_eq!(
        openai["capabilities"]["reasoning"]["kind"].as_str(),
        Some("levels"),
        "{openai}"
    );
    assert!(!openai["notes"].as_array().unwrap().is_empty(), "{openai}");

    let (status, anthropic) = get(&gw.base, "/v1/models/qwen3.8-27b", true).await;
    assert_eq!(status, 200, "{anthropic}");
    assert_eq!(anthropic["type"].as_str(), Some("model"));
    assert_eq!(anthropic["id"].as_str(), Some("qwen3.8-27b"));
    assert_eq!(anthropic["display_name"].as_str(), Some("qwen3.8-27b"));
    assert!(anthropic["created_at"].as_str().is_some(), "{anthropic}");
    // The Anthropic SDK's typed names sit beside the ones the OpenAI shape
    // uses; both name the same numbers (§2.2).
    assert_eq!(anthropic["context_window"].as_u64(), Some(131_072));
    assert_eq!(anthropic["max_input_tokens"].as_u64(), Some(131_072));
    assert_eq!(anthropic["max_tokens"].as_u64(), Some(32_768));
    assert_eq!(anthropic["max_output_tokens"].as_u64(), Some(32_768));
    // One capability schema, whichever SDK the caller holds.
    assert_eq!(anthropic["capabilities"], openai["capabilities"]);
    assert_eq!(anthropic["notes"], openai["notes"]);
}

/// Published per-request context, split vs unified (unified-KV design §3.2,
/// `LlamaParams::per_request_ctx`): a split row keeps `ctx_size / parallel`;
/// an explicitly unified row with no `ctx_size` sizes its pool from the
/// per-slot cap and is itself bound by that same cap; and an auto row whose
/// `ctx_size` outruns the model's trained context is capped to the smaller
/// number instead of publishing more than the model can actually do.
#[tokio::test]
async fn local_context_reflects_split_vs_unified_and_the_trained_ceiling() {
    let gw = gateway().await;

    let (status, split) = get(&gw.base, "/v1/models/kv-split", false).await;
    assert_eq!(status, 200, "{split}");
    assert_eq!(split["context_length"].as_u64(), Some(8_192), "{split}");

    let (status, unified) = get(&gw.base, "/v1/models/kv-unified-cap", false).await;
    assert_eq!(status, 200, "{unified}");
    assert_eq!(unified["context_length"].as_u64(), Some(4_096), "{unified}");

    let (status, small) = get(&gw.base, "/v1/models/kv-small-trained", false).await;
    assert_eq!(status, 200, "{small}");
    assert_eq!(
        small["context_length"].as_u64(),
        Some(4_096),
        "the trained context (4096) must win over the configured ctx_size \
         (8192): {small}"
    );

    // Review finding 2: an auto row with no `ctx_size` at all publishes
    // nothing — the trained context (4096, same GGUF as `kv-small-trained`)
    // is no longer a stand-in for a pool size llama-server never promised.
    let (status, auto_no_ctx) = get(&gw.base, "/v1/models/kv-auto-no-ctx", false).await;
    assert_eq!(status, 200, "{auto_no_ctx}");
    assert!(auto_no_ctx.get("context_length").is_none(), "{auto_no_ctx}");
}

/// A ladder row publishes the **top** rung's per-slot context (ladder design
/// §4.4), not the base's `ctx_size / parallel` — the opposite of every other
/// case above, which is exactly why it needs its own row rather than being
/// folded into the split/unified test. `max_output_tokens` is unchanged
/// (`n_predict`), and the notes name the rung count.
#[tokio::test]
async fn a_ladder_row_publishes_the_top_rungs_per_slot_context() {
    let gw = gateway().await;

    let (status, v) = get(&gw.base, "/v1/models/ladder-model", false).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        v["context_length"].as_u64(),
        Some(65_536),
        "top rung's ctx_size (131072) / parallel (2), not the base's: {v}"
    );
    assert_eq!(v["max_output_tokens"].as_u64(), Some(4_096), "{v}");
    assert_some_note(&v, "ladder, 3 rungs");
}

/// Review finding 1: llama-server caps every slot at the weights' trained
/// context, so a ladder whose top rung is configured past it publishes the
/// slot it really has — 4096, not 16384 / 2 — the same number the gate
/// judges requests by.
#[tokio::test]
async fn a_ladder_publishes_its_top_slot_capped_at_the_trained_context() {
    let gw = gateway().await;
    let (status, v) = get(&gw.base, "/v1/models/capped-ladder", false).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["context_length"].as_u64(), Some(4_096), "{v}");
}

/// A passthrough id keeps the upstream prefix *and* the provider's own slash,
/// so the route captures everything after `/v1/models/` (§2.2).
#[tokio::test]
async fn a_passthrough_id_containing_slashes_is_addressable() {
    let gw = gateway().await;

    let (status, openai) = get(&gw.base, &format!("/v1/models/{NESTED_ID}"), false).await;
    assert_eq!(status, 200, "{openai}");
    assert_eq!(openai["id"].as_str(), Some(NESTED_ID));
    assert_eq!(openai["owned_by"].as_str(), Some("kilo"));
    assert_eq!(openai["context_length"].as_u64(), Some(200_000));
    assert_eq!(openai["max_output_tokens"].as_u64(), Some(64_000));
    assert_eq!(
        openai["capabilities"]["source"].as_str(),
        Some("catalog"),
        "{openai}"
    );
    // The catalog published a timestamp of its own, so that is the `created`
    // — not the gateway's start time.
    assert_eq!(openai["created"].as_i64(), Some(1_700_000_000));

    let (status, anthropic) = get(&gw.base, &format!("/v1/models/{NESTED_ID}"), true).await;
    assert_eq!(status, 200, "{anthropic}");
    assert_eq!(anthropic["type"].as_str(), Some("model"));
    assert_eq!(anthropic["max_input_tokens"].as_u64(), Some(200_000));
    assert_eq!(anthropic["max_tokens"].as_u64(), Some(64_000));
    assert_eq!(anthropic["capabilities"], openai["capabilities"]);
}

/// An id nobody exposes is a 404 in the caller's *own* error dialect — a
/// client never has to parse the other SDK's error body to find out.
#[tokio::test]
async fn an_unknown_id_is_a_404_in_the_callers_dialect() {
    let gw = gateway().await;

    let (status, openai) = get(&gw.base, "/v1/models/no-such-model", false).await;
    assert_eq!(status, 404, "{openai}");
    assert_eq!(
        openai["error"]["type"].as_str(),
        Some("invalid_request_error"),
        "{openai}"
    );
    assert_eq!(openai["error"]["code"].as_str(), Some("not_found"));
    assert!(
        openai["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no-such-model"),
        "the message must name the id: {openai}"
    );

    let (status, anthropic) = get(&gw.base, "/v1/models/no-such-model", true).await;
    assert_eq!(status, 404, "{anthropic}");
    assert_eq!(anthropic["type"].as_str(), Some("error"));
    assert_eq!(
        anthropic["error"]["type"].as_str(),
        Some("not_found_error"),
        "{anthropic}"
    );
    assert!(
        anthropic["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no-such-model"),
        "{anthropic}"
    );
    // The OpenAI envelope must not leak into the Anthropic body.
    assert!(anthropic["error"].get("code").is_none(), "{anthropic}");
}

/// The list carries the gateway-wide `lmgw` block in both dialects: version,
/// the route inventory, the control headers and the notes the per-model notes
/// deliberately leave out (§2.1).
#[tokio::test]
async fn the_list_carries_the_gateway_block_in_both_dialects() {
    let gw = gateway().await;

    for anthropic in [false, true] {
        let (status, v) = get(&gw.base, "/v1/models", anthropic).await;
        assert_eq!(status, 200, "{v}");
        let block = &v["lmgw"];
        assert_eq!(
            block["version"].as_str(),
            Some(env!("CARGO_PKG_VERSION")),
            "{block}"
        );
        for header in [
            "x-lmgw-reasoning",
            "x-lmgw-reasoning-effort",
            "x-lmgw-reasoning-budget",
            "x-lmgw-fallback",
            "x-lmgw-fallback-reason",
            "x-lmgw-reasoning-ignored",
            "x-lmgw-max-tokens-defaulted",
            "x-lmgw-max-tokens-clamped",
            "x-lmgw-rung",
            // The new counter-approximation header (api-docs design §5.1):
            // documented from WP2 on, even though nothing stamps it until
            // WP3 wires `count.rs` up to it.
            "x-lmgw-count-approximate",
        ] {
            assert!(
                block["headers"][header].as_str().is_some(),
                "{header} must be documented: {block}"
            );
        }
        // The three control headers say where they work, so an agent does not
        // have to guess which routes honour them — all four now that
        // `/v1/messages/count_tokens` reads them too (api-docs design §4.9).
        for header in [
            "x-lmgw-reasoning",
            "x-lmgw-reasoning-effort",
            "x-lmgw-reasoning-budget",
        ] {
            let doc = block["headers"][header].as_str().unwrap();
            for route in [
                "/v1/chat/completions",
                "/v1/messages",
                "/v1/messages/count_tokens",
                "/v1/responses",
            ] {
                assert!(doc.contains(route), "{header} must name {route}: {doc}");
            }
        }
        let notes = block["notes"].as_array().unwrap();
        assert!(
            notes
                .iter()
                .any(|n| n.as_str().unwrap().contains("gpu_hold")),
            "{block}"
        );
        assert!(
            notes
                .iter()
                .any(|n| n.as_str().unwrap().contains("capabilities.source")),
            "{block}"
        );
        for route in ["/v1/models/{id}", "/v1/audio/voices"] {
            assert!(
                block["endpoints"]["openai"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e.as_str() == Some(route)),
                "{route} must be listed: {block}"
            );
        }
        // Local tool calling only works because --jinja is the default; a
        // client reading tool_calls.kind has to know that is an assumption.
        assert!(
            notes
                .iter()
                .any(|n| n.as_str().unwrap().contains("--no-jinja")),
            "{block}"
        );
        assert!(
            block["endpoints"]["anthropic"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e.as_str() == Some("/v1/messages")),
            "{block}"
        );
        // WP6: `lmgw.endpoints` is generated from the route registry
        // (api-docs design §4.11), so `/tokenize` and
        // `/v1/messages/count_tokens` — advertised since WP2/before WP3
        // existed, and real routes since WP3 — and the brand new
        // `/v1/openapi.json` all have to actually be there now.
        assert!(
            block["endpoints"]["anthropic"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e.as_str() == Some("/v1/messages/count_tokens")),
            "{block}"
        );
        for route in ["/tokenize", "/v1/openapi.json"] {
            assert!(
                block["endpoints"]["other"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e.as_str() == Some(route)),
                "{route} must be listed: {block}"
            );
        }
    }
}

/// `created` is the gateway's start time for a row with no timestamp of its
/// own, so two calls agree — the previous handler stamped `now()` and told
/// every polling client that every model had just been recreated (§2.1).
#[tokio::test]
async fn created_is_stable_across_consecutive_calls() {
    let gw = gateway().await;

    let (_, first) = get(&gw.base, "/v1/models/qwen3.8-27b", false).await;
    let (_, second) = get(&gw.base, "/v1/models/qwen3.8-27b", false).await;
    let created = first["created"].as_i64().unwrap();
    assert_eq!(second["created"].as_i64(), Some(created));
    assert!(created > 0, "{first}");

    // …and the list agrees with the single-model route.
    let (_, list) = get(&gw.base, "/v1/models", false).await;
    let listed = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"].as_str() == Some("qwen3.8-27b"))
        .unwrap();
    assert_eq!(listed["created"].as_i64(), Some(created));
}

/// A row whose GGUF cannot be read is still routable, so it is still listed —
/// with no `capabilities` (guessing would be worse than a hole) and a note
/// naming the file, which is the diagnosis the owner needs.
#[tokio::test]
async fn an_unreadable_gguf_is_listed_without_capabilities_and_says_why() {
    let gw = gateway().await;

    let (status, v) = get(&gw.base, "/v1/models/gone", false).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["id"].as_str(), Some("gone"));
    assert!(
        v.get("capabilities").is_none(),
        "unreadable weights must publish no capabilities: {v}"
    );
    let notes = v["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap().contains("vanished-weights.gguf")),
        "a note must name the unreadable file: {v}"
    );

    // And the row is really in the list, not only reachable by id.
    let (_, list) = get(&gw.base, "/v1/models", false).await;
    assert!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"].as_str() == Some("gone")),
        "{list}"
    );
}

/// An alias onto a local llama-server row publishes the **row's** facts, not
/// the upstream catalog's (design §3.6) — with the alias' own reasoning
/// default folded over them, so the two ids can honestly differ.
#[tokio::test]
async fn an_alias_onto_a_local_row_derives_from_that_row() {
    let gw = gateway().await;

    let (status, v) = get(&gw.base, "/v1/models/qwen-thorough", false).await;
    assert_eq!(status, 200, "{v}");
    let caps = &v["capabilities"];
    assert_eq!(caps["source"].as_str(), Some("gguf+config"), "{v}");
    assert_eq!(caps["reasoning"]["kind"].as_str(), Some("levels"));
    assert_eq!(
        caps["reasoning"]["default"].as_str(),
        Some("xhigh"),
        "the alias' param_overrides.reasoning wins over the row's default: {v}"
    );
    assert_eq!(
        v["max_output_tokens"].as_u64(),
        Some(32_768),
        "the cap is still the backing row's --n-predict: {v}"
    );
    // The alias points at a llama-server upstream, so it is free like the row.
    assert_eq!(v["pricing"]["prompt"].as_str(), Some("0"), "{v}");

    // …and the row itself still publishes its own default.
    let (_, row) = get(&gw.base, "/v1/models/qwen3.8-27b", false).await;
    assert_eq!(
        row["capabilities"]["reasoning"]["default"].as_str(),
        Some("xhigh"),
        "this row sets no --reasoning-effort, so the template's default stands: {row}"
    );
}

/// A Gemini-protocol passthrough entry: the chat routes minus
/// `/v1/completions`, reasoning expressed the way that egress expresses it,
/// and a note saying the catalog published no modalities at all.
#[tokio::test]
async fn a_gemini_passthrough_entry_publishes_its_protocols_shape() {
    let gw = gateway().await;

    let (status, v) = get(&gw.base, "/v1/models/gem/gemini-3.0-pro", false).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["owned_by"].as_str(), Some("gem"));
    assert_eq!(v["context_length"].as_u64(), Some(1_048_576));
    assert_eq!(v["max_output_tokens"].as_u64(), Some(65_536));

    let caps = &v["capabilities"];
    assert_eq!(caps["source"].as_str(), Some("catalog"));
    let endpoints: Vec<&str> = caps["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap())
        .collect();
    assert_eq!(
        endpoints,
        ["/v1/chat/completions", "/v1/messages", "/v1/responses"],
        "/v1/completions refuses a non-OpenAI-protocol route: {v}"
    );
    assert_eq!(caps["reasoning"]["kind"].as_str(), Some("toggle"));
    assert!(
        caps.get("input_modalities").is_none(),
        "Gemini's catalog states none, and unknown is not text-only: {v}"
    );
    assert!(caps.get("vision").is_none(), "{v}");
    assert_some_note(&v, "does not state this model's input modalities");
}

/// A projector sitting next to the weights that nobody configured: text-only,
/// plus the note that explains why a multimodal repo answers text-only. With
/// `--no-mmproj` the same situation is a decision, and the note says so.
#[tokio::test]
async fn an_unconfigured_sibling_projector_is_named() {
    let gw = gateway().await;

    let (status, v) = get(&gw.base, "/v1/models/twin", false).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        v["capabilities"]["input_modalities"],
        json!(["text"]),
        "{v}"
    );
    assert_eq!(v["capabilities"]["vision"].as_bool(), Some(false));
    assert_some_note(&v, "mmproj-twin.gguf");
    assert_some_note(&v, "text-only until you set it");

    let (_, on_purpose) = get(&gw.base, "/v1/models/twin-text-only", false).await;
    assert_some_note(&on_purpose, "--no-mmproj is set on this row");
    assert_some_note(&on_purpose, "text-only on purpose");
}

/// A configured projector whose header will not parse: the modalities are
/// withheld (they are exactly what that file would have said) and the note
/// names the file and the error.
#[tokio::test]
async fn an_unreadable_projector_withholds_the_modalities() {
    let gw = gateway().await;

    let (status, v) = get(&gw.base, "/v1/models/broken-projector", false).await;
    assert_eq!(status, 200, "{v}");
    let caps = &v["capabilities"];
    assert!(caps.get("input_modalities").is_none(), "{v}");
    assert!(caps.get("vision").is_none(), "{v}");
    // The rest of the row is still derived — one unreadable companion does not
    // sink the entry.
    assert_eq!(caps["reasoning"]["kind"].as_str(), Some("levels"), "{v}");
    assert_some_note(&v, "mmproj-broken.gguf");
}

/// `--chat-template-file` is what llama-server renders, so the published facts
/// come from that file — and when it cannot be read, from the GGUF plus a note
/// saying the two may disagree.
#[tokio::test]
async fn a_chat_template_override_file_decides_the_facts() {
    let gw = gateway().await;

    // The override is medgemma's template: no thinking, no tools — while the
    // weights carry Qwen3.8's.
    let (status, v) = get(&gw.base, "/v1/models/template-override", false).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        v["capabilities"]["reasoning"]["kind"].as_str(),
        Some("fixed"),
        "{v}"
    );
    assert_eq!(
        v["capabilities"]["tool_calls"]["kind"].as_str(),
        Some("none"),
        "{v}"
    );

    let (status, missing) = get(&gw.base, "/v1/models/template-missing", false).await;
    assert_eq!(status, 200, "{missing}");
    assert_eq!(
        missing["capabilities"]["reasoning"]["kind"].as_str(),
        Some("levels"),
        "falls back to the GGUF's own template: {missing}"
    );
    assert_some_note(&missing, "tmpl/nope.jinja");
    assert_some_note(&missing, "not what llama-server renders");
}

/// An owner override reaches `/v1/models`: the capabilities object it touched
/// is `source: owner`, its notes are appended to the derived ones, and a
/// hand-set `max_output_tokens` says it was hand-set (§7).
#[tokio::test]
async fn an_owner_override_is_applied_and_attributed() {
    let gw = gateway().await;

    let (status, v) = get(&gw.base, "/v1/models/owner-corrected", false).await;
    assert_eq!(status, 200, "{v}");
    let caps = &v["capabilities"];
    assert_eq!(caps["source"].as_str(), Some("owner"), "{v}");
    assert_eq!(caps["input_modalities"], json!(["text", "image"]), "{v}");
    assert_eq!(caps["vision"].as_bool(), Some(true));
    // Untouched keys still come from the derivation.
    assert_eq!(caps["reasoning"]["kind"].as_str(), Some("levels"), "{v}");
    assert_eq!(v["max_output_tokens"].as_u64(), Some(4096));
    assert_some_note(&v, "Verified against a live probe");
    assert_some_note(&v, "max_output_tokens was set by the owner");
}

/// A `capabilities_override` column that does not parse as JSON must not be
/// read as "no override": the row keeps its derived facts and says the
/// override was rejected, which is the only way the owner finds out.
#[tokio::test]
async fn an_unparseable_stored_override_is_rejected_out_loud() {
    let gw = gateway().await;

    let (status, v) = get(&gw.base, "/v1/models/owner-broken", false).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        v["capabilities"]["source"].as_str(),
        Some("gguf+config"),
        "a rejected override must not claim to be the owner's word: {v}"
    );
    assert_eq!(
        v["capabilities"]["reasoning"]["kind"].as_str(),
        Some("levels"),
        "the derived facts stand: {v}"
    );
    assert_some_note(&v, "capabilities override on this model was rejected");
}

/// An enabled image row is on the list under the class prefix, with the task,
/// routes and modalities its own columns imply — and with none of the fields a
/// diffusion pipeline has no answer for (image-generation design §5).
#[tokio::test]
async fn an_image_row_is_exposed_under_its_prefix() {
    let gw = gateway().await;
    let (status, v) = get(&gw.base, "/v1/models/image/z-image-turbo", false).await;
    assert_eq!(status, 200, "{v}");
    let caps = &v["capabilities"];
    assert_eq!(caps["task"], "image_generation");
    assert_eq!(caps["endpoints"], json!(["/v1/images/generations"]));
    assert_eq!(caps["input_modalities"], json!(["text"]));
    assert_eq!(caps["output_modalities"], json!(["image"]));
    assert_eq!(caps["vision"], json!(false));
    assert_eq!(caps["source"], "config");
    for absent in ["reasoning", "tool_calls", "structured_output"] {
        assert!(caps.get(absent).is_none(), "{absent} invented: {caps}");
    }
    // Nothing invented at the top level either: no context window, no token
    // cap, and the price of our own hardware.
    assert!(v.get("context_length").is_none(), "{v}");
    assert!(v.get("max_output_tokens").is_none(), "{v}");
    assert_eq!(v["pricing"], json!({"prompt": "0", "completion": "0"}));
    assert_eq!(v["owned_by"], "sdcpp");
    // The notes say what this row's own flags make the defaults.
    assert_some_note(
        &v,
        "width 1024, height 1024, steps 8, cfg-scale server default",
    );
    assert_some_note(&v, "<sd_cpp_extra_args>");
    assert_some_note(&v, "b64_json only");

    // It is on the list too, and the disabled row is not.
    let (_, list) = get(&gw.base, "/v1/models", false).await;
    let ids: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(ids.contains(&"image/z-image-turbo"), "{ids:?}");
    assert!(!ids.contains(&"image/not-installed"), "{ids:?}");
    // …and the list-level notes name the prefix, so a client knows what an
    // `image/…` id is without asking.
    let notes = list["lmgw"]["notes"].as_array().unwrap();
    assert!(
        notes.iter().any(|n| n
            .as_str()
            .unwrap()
            .contains("'image/…' local stable-diffusion.cpp")),
        "{notes:#?}"
    );
    // The route inventory lists both image routes.
    let endpoints = list["lmgw"]["endpoints"]["openai"].as_array().unwrap();
    for route in ["/v1/images/generations", "/v1/images/edits"] {
        assert!(
            endpoints.iter().any(|e| e.as_str() == Some(route)),
            "{route} must be listed: {endpoints:#?}"
        );
    }
}

/// The `edit` column is what adds the second route and the image input; a
/// video-only row publishes `video` out — and an owner override merges over
/// either exactly as it does for a chat row.
#[tokio::test]
async fn image_edit_and_video_rows_publish_what_they_are() {
    let gw = gateway().await;

    let (_, edit) = get(&gw.base, "/v1/models/image/flux-kontext", false).await;
    let caps = &edit["capabilities"];
    assert_eq!(caps["task"], "image_edit");
    assert_eq!(
        caps["endpoints"],
        json!(["/v1/images/generations", "/v1/images/edits"])
    );
    assert_eq!(caps["input_modalities"], json!(["text", "image"]));
    assert_eq!(caps["vision"], json!(true));
    assert_some_note(
        &edit,
        "Editing: POST /v1/images/edits as multipart/form-data",
    );

    let (_, video) = get(&gw.base, "/v1/models/image/wan-2.2", false).await;
    let caps = &video["capabilities"];
    assert_eq!(caps["task"], "video_generation");
    assert_eq!(caps["output_modalities"], json!(["video"]));
    // The owner's override merged over the derived object rather than
    // replacing it, and is attributed to them.
    assert_eq!(caps["source"], "owner");
    assert_eq!(caps["input_modalities"], json!(["text"]));
    assert_some_note(&video, "native job API only");
}
