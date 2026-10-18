//! The self-admin tools' answers that no OpenAPI document describes
//! (`lmgw__models`, `lmgw__upstreams`, `lmgw__logs`, `lmgw__settings`): each
//! tool's text is read into its type
//! and written out again, and the two texts are equal byte for byte. The text
//! is the answer's pretty JSON with sorted keys, so a key a type adds, drops,
//! renames or turns from absent into `null` (or an integer into a float)
//! shows up here.

use lmgw_api_types as dto;
use lmgw_api_types::self_admin::{self, ModelEntry};
use lmgw_core::config::chat_profile::{create_kind, ProfileCreate};
use lmgw_core::config::{
    CatalogRevision, HoldFallbackMode, Protocol, SelfAdmin, Settings, UpstreamKind,
};
use lmgw_core::gguf::synth;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{
    self, NewAlias, NewAudioModel, NewAuxModel, NewCandidateAlias, NewImageModel, NewLocalModel,
    NewRequestLog, NewUpstream,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::self_admin::{parse, read_back, Admin};

// ---------------------------------------------------------------------------
// The fixture: every kind of model, upstreams in every state
// ---------------------------------------------------------------------------

fn upstream(name: &str, protocol: Protocol, kind: UpstreamKind, base_url: &str) -> NewUpstream {
    NewUpstream {
        name: name.into(),
        protocol,
        kind,
        base_url: base_url.into(),
        api_key: None,
        extra_headers: vec![],
        timeout_ms: 30_000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        catalog_all_modalities: true,
    }
}

fn alias(name: &str, upstream_id: i64, model: &str, enabled: bool) -> NewAlias {
    NewAlias {
        alias: name.into(),
        upstream_id,
        upstream_model_id: model.into(),
        param_overrides: Default::default(),
        enabled,
        capabilities_override: None,
    }
}

fn local(model_id: &str, gguf: &str, public: bool) -> NewLocalModel {
    NewLocalModel {
        model_id: model_id.into(),
        gguf_path: gguf.into(),
        params: Default::default(),
        args: vec![],
        idle_seconds: 300,
        enabled: true,
        public,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
        ladder: vec![],
    }
}

fn aux(
    model_id: &str,
    gguf: &str,
    kind: lmgw_core::config::AuxKind,
    pooling: Option<&str>,
) -> NewAuxModel {
    NewAuxModel {
        model_id: model_id.into(),
        gguf_path: gguf.into(),
        kind,
        pooling: pooling.map(String::from),
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
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    }
}

fn audio(model_id: &str, family: &str, task: &str) -> NewAudioModel {
    NewAudioModel {
        model_id: model_id.into(),
        family: family.into(),
        path: format!("audio-cpp/{model_id}"),
        task: task.into(),
        mode: "offline".into(),
        lazy: None,
        busy_timeout_ms: None,
        backend: None,
        threads: None,
        load_options: Default::default(),
        session_options: Default::default(),
        default_request_options: Default::default(),
        model_spec_override: None,
        config_id: None,
        weight_id: None,
        voice_presets: Default::default(),
        default_voice_preset: None,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    }
}

fn image(model_id: &str, files: Value, modes: &[&str], edit: bool) -> NewImageModel {
    NewImageModel {
        model_id: model_id.into(),
        files: files.as_object().cloned().unwrap_or_default(),
        args: Map::new(),
        modes: modes.iter().map(|m| m.to_string()).collect(),
        edit,
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

fn candidate(name: &str, candidates: &[&str], background: bool) -> NewCandidateAlias {
    NewCandidateAlias {
        alias: name.into(),
        candidates: candidates.iter().map(|c| c.to_string()).collect(),
        background,
        fallback_mode: HoldFallbackMode::Inherit,
        fallback: None,
        capabilities_disabled: vec![],
        capabilities_enabled: vec![],
        enabled: true,
        notes: String::new(),
    }
}

async fn log_deferral(state: &SharedState, alias: &str) {
    sqlx::query(
        "INSERT INTO request_logs (ts, ingress_proto, requested_alias, status, error_kind)
         VALUES (strftime('%Y-%m-%d %H:%M:%S', 'now'), 'openai', ?, 503, 'gpu_deferred')",
    )
    .bind(alias)
    .execute(&state.db)
    .await
    .unwrap();
}

/// What the models tool lists, with the weights on disk for some rows only:
/// a row whose GGUF cannot be read has no `capabilities` and says why in its
/// notes.
struct World {
    state: SharedState,
    _dirs: Vec<tempfile::TempDir>,
    _mocks: Vec<MockServer>,
}

async fn world(passthrough: bool) -> World {
    let state = AppState::init_for_tests().await.unwrap();
    let chat_dir = tempfile::tempdir().unwrap();
    let aux_dir = tempfile::tempdir().unwrap();
    synth::chat("qwen35", 32768).write_to(&chat_dir.path().join("chat.gguf"));
    synth::embedding("qwen3", 3, 32768).write_to(&aux_dir.path().join("embed.gguf"));
    let mut settings = store::load_settings(&state.db).await.unwrap();
    settings.router.models_dir = chat_dir.path().display().to_string();
    settings.aux_router.models_dir = aux_dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();

    let mut mocks = Vec::new();
    // Three llama.cpp rows: one whose server answers `/props`, one that
    // answers 404, and one nobody has asked yet.
    let mut llama = Vec::new();
    for (name, status) in [("llama-ok", 200), ("llama-old", 404)] {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(if status == 200 {
                ResponseTemplate::new(200).set_body_json(json!({
                    "default_generation_settings": {"params": {}, "n_ctx": 8192},
                    "modalities": {"vision": true, "video": false, "audio": false},
                    "build_info": "b11226-0c6a6a7",
                }))
            } else {
                ResponseTemplate::new(404)
            })
            .mount(&mock)
            .await;
        llama.push((name, format!("{}/v1", mock.uri())));
        mocks.push(mock);
    }

    // `acme` lists `acme-turbo` with the numbers a catalog gives; `plain`
    // answers 404 to everything, so its aliases carry notes and no facts.
    let catalog = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{
                "id": "acme-turbo",
                "context_length": 131072,
                "top_provider": {"max_completion_tokens": 8192},
                "architecture": {"input_modalities": ["text", "image"],
                                 "output_modalities": ["text"]},
            }],
        })))
        .mount(&catalog)
        .await;
    let plain = MockServer::start().await;
    let mut acme = upstream(
        "acme",
        Protocol::Openai,
        UpstreamKind::Generic,
        &format!("{}/v1", catalog.uri()),
    );
    acme.api_key = Some("sk-secret".into());
    acme.extra_headers = vec![
        ("X-Org".into(), "org-1".into()),
        ("X-Team".into(), "team-2".into()),
    ];
    acme.timeout_ms = 45_000;
    acme.supports_responses = true;
    let acme = store::insert_upstream(&state.db, &acme).await.unwrap();
    let bare = store::insert_upstream(
        &state.db,
        &upstream(
            "bare",
            Protocol::Anthropic,
            UpstreamKind::Generic,
            &plain.uri(),
        ),
    )
    .await
    .unwrap();
    let mut off = upstream(
        "off-gemini",
        Protocol::Gemini,
        UpstreamKind::Generic,
        &plain.uri(),
    );
    off.enabled = false;
    let off = store::insert_upstream(&state.db, &off).await.unwrap();
    store::insert_upstream(
        &state.db,
        &upstream(
            "audio-box",
            Protocol::Openai,
            UpstreamKind::AudioCpp,
            "http://127.0.0.1:1/v1",
        ),
    )
    .await
    .unwrap();
    if passthrough {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"object": "list", "data": [{"id": "gpt-x"}]})),
            )
            .mount(&mock)
            .await;
        let mut open = upstream(
            "open",
            Protocol::Openai,
            UpstreamKind::Generic,
            &format!("{}/v1", mock.uri()),
        );
        open.expose_all = true;
        open.expose_prefix = "op".into();
        store::insert_upstream(&state.db, &open).await.unwrap();
        let mut hidden = upstream(
            "open-but-off",
            Protocol::Openai,
            UpstreamKind::Generic,
            &format!("{}/v1", plain.uri()),
        );
        hidden.expose_all = true;
        hidden.enabled = false;
        store::insert_upstream(&state.db, &hidden).await.unwrap();
        mocks.push(mock);
    }
    mocks.push(catalog);
    mocks.push(plain);
    let mut llama_ids = Vec::new();
    for (name, base) in &llama {
        let mut row = upstream(name, Protocol::LlamaCpp, UpstreamKind::LlamaServer, base);
        row.api_key = Some("sk-llama".into());
        llama_ids.push(store::insert_upstream(&state.db, &row).await.unwrap());
    }
    // Never asked: `llama_facts` is there, and empty.
    store::insert_upstream(
        &state.db,
        &upstream(
            "llama-new",
            Protocol::LlamaCpp,
            UpstreamKind::LlamaServer,
            "http://127.0.0.1:1/v1",
        ),
    )
    .await
    .unwrap();

    // Aliases: a plain one, a switched-off one, one with the owner's say on
    // its capabilities, and one per llama row (to make the cache ask).
    store::insert_alias(&state.db, &alias("fast", acme, "acme-turbo", true))
        .await
        .unwrap();
    store::insert_alias(&state.db, &alias("sleeping", bare, "claude-x", false))
        .await
        .unwrap();
    // An alias on a switched-off upstream still names it.
    store::insert_alias(&state.db, &alias("dormant", off, "gone", true))
        .await
        .unwrap();
    // Not in its upstream's catalog: the notes say so, nothing else is known.
    store::insert_alias(&state.db, &alias("unlisted", acme, "acme-gone", true))
        .await
        .unwrap();
    for ((name, _), id) in llama.iter().zip(&llama_ids) {
        store::insert_alias(&state.db, &alias(&format!("m-{name}"), *id, "gguf", true))
            .await
            .unwrap();
    }

    // Local models: readable weights, unreadable weights, not public.
    store::insert_local_model(&state.db, &local("chat-cap", "chat.gguf", true))
        .await
        .unwrap();
    store::insert_local_model(&state.db, &local("chat-gone", "gone.gguf", true))
        .await
        .unwrap();
    store::insert_local_model(&state.db, &local("chat-private", "chat.gguf", false))
        .await
        .unwrap();
    // Aux: an embedding model with a pooling, a rerank one without weights.
    store::insert_aux_model(
        &state.db,
        &aux(
            "emb-cap",
            "embed.gguf",
            lmgw_core::config::AuxKind::Embed,
            Some("mean"),
        ),
    )
    .await
    .unwrap();
    store::insert_aux_model(
        &state.db,
        &aux(
            "rr-gone",
            "gone.gguf",
            lmgw_core::config::AuxKind::Rerank,
            None,
        ),
    )
    .await
    .unwrap();
    store::insert_audio_model(&state.db, &audio("voice-1", "qwen3_tts", "tts"))
        .await
        .unwrap();
    store::insert_audio_model(&state.db, &audio("ears-1", "whisper", "asr"))
        .await
        .unwrap();
    // Image: the loader under `diffusion_model`, under `model` (which wins),
    // and under neither.
    store::insert_image_model(
        &state.db,
        &image(
            "pipe-dit",
            json!({"diffusion_model": "dit.gguf"}),
            &[],
            false,
        ),
    )
    .await
    .unwrap();
    store::insert_image_model(
        &state.db,
        &image(
            "pipe-all",
            json!({"model": "all.safetensors", "diffusion_model": "dit.gguf"}),
            &["img_gen", "vid_gen"],
            true,
        ),
    )
    .await
    .unwrap();
    store::insert_image_model(&state.db, &image("pipe-none", json!({}), &[], false))
        .await
        .unwrap();

    // Candidate aliases: a healthy one with a facet enabled, one with a
    // fallback, one naming a model that is gone (a problem), and one with an
    // empty list.
    let mut smart = candidate("smart", &["chat-cap"], false);
    smart.capabilities_enabled = vec!["structured_output".into()];
    store::insert_candidate_alias(&state.db, &smart)
        .await
        .unwrap();
    store::insert_candidate_alias(
        &state.db,
        &candidate("broken", &["chat-cap", "absent"], false),
    )
    .await
    .unwrap();
    let mut fb = candidate("smart-fb", &["chat-cap", "chat-gone"], true);
    fb.fallback_mode = HoldFallbackMode::Alias;
    fb.fallback = Some("fast".into());
    store::insert_candidate_alias(&state.db, &fb).await.unwrap();
    store::insert_candidate_alias(&state.db, &candidate("empty-list", &[], false))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    log_deferral(&state, "smart").await;
    log_deferral(&state, "SMART").await;

    // Let the cache ask the two llama rows that have an alias.
    for (name, id) in llama.iter().map(|(n, _)| n).zip(&llama_ids) {
        let route = state.snapshot().resolve(&format!("m-{name}")).unwrap();
        lmgw_core::llama_facts::resolve(&state, None, &route);
        state.llama_facts.settled(*id).await;
    }
    World {
        state,
        _dirs: vec![chat_dir, aux_dir],
        _mocks: mocks,
    }
}

// ---------------------------------------------------------------------------
// lmgw__models
// ---------------------------------------------------------------------------

fn find<'a>(list: &'a self_admin::ModelList, name: &str) -> &'a ModelEntry {
    list.models
        .iter()
        .find(|m| m.name() == name)
        .unwrap_or_else(|| panic!("{name} is not listed: {:?}", list.models))
}

fn raw<'a>(list: &'a Value, name: &str) -> &'a Value {
    list["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == name)
        .unwrap_or_else(|| panic!("{name} is not listed"))
}

/// Every kind, with the enrichment present and absent: the tool's text reads
/// into `ModelList` and writes back equal.
#[tokio::test]
async fn the_models_text_reads_back_equal_for_every_kind() {
    let w = world(true).await;
    let admin = Admin::new(&w.state).await;
    let text = admin.text("lmgw__models", json!({})).await;
    let list: self_admin::ModelList = read_back(&text);
    let wire = parse(&text);

    // Sorted by name, one entry per kind's row.
    let names: Vec<&str> = list.models.iter().map(ModelEntry::name).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "{names:?}");
    let kinds: std::collections::BTreeSet<&str> = wire["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds.into_iter().collect::<Vec<_>>(),
        ["alias", "audio", "aux", "candidate_alias", "image", "local"]
    );

    // alias: what the upstream's catalog states reaches the entry as
    // integers; a switched-off alias is not published, so it has no facts at
    // all (not `null`, not empty); one the catalog does not list has notes only.
    let ModelEntry::Alias(fast) = find(&list, "fast") else {
        panic!("fast is an alias")
    };
    assert_eq!(fast.upstream.as_deref(), Some("acme"));
    assert_eq!(fast.upstream_model, "acme-turbo");
    assert!(fast.enabled);
    assert_eq!(fast.facts.context_length, Some(131_072));
    assert_eq!(fast.facts.max_output_tokens, Some(8192));
    assert!(fast.facts.capabilities.is_some(), "{fast:?}");
    assert!(raw(&wire, "fast")["context_length"].is_u64());
    assert!(raw(&wire, "fast")["max_output_tokens"].is_u64());
    let ModelEntry::Alias(sleeping) = find(&list, "sleeping") else {
        panic!("sleeping is an alias")
    };
    assert!(!sleeping.enabled);
    assert_eq!(sleeping.facts, self_admin::ModelFacts::default());
    let mut keys: Vec<&str> = raw(&wire, "sleeping")
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        ["enabled", "kind", "name", "upstream", "upstream_model"]
    );
    let ModelEntry::Alias(unlisted) = find(&list, "unlisted") else {
        panic!("unlisted is an alias")
    };
    assert_eq!(unlisted.facts.context_length, None);
    assert_eq!(unlisted.facts.capabilities, None);
    assert!(!unlisted.facts.notes.is_empty(), "{unlisted:?}");
    for key in ["context_length", "max_output_tokens", "capabilities"] {
        assert!(raw(&wire, "unlisted").get(key).is_none(), "{key}");
    }
    // An alias on a switched-off upstream still names it.
    let ModelEntry::Alias(dormant) = find(&list, "dormant") else {
        panic!("dormant is an alias")
    };
    assert_eq!(dormant.upstream.as_deref(), Some("off-gemini"));

    // candidate_alias: a count is an integer, a missing fallback `null`, the
    // routable candidates a list.
    let ModelEntry::CandidateAlias(smart) = find(&list, "smart") else {
        panic!("smart is a candidate alias")
    };
    assert_eq!(smart.candidates, ["chat-cap"]);
    assert_eq!(smart.routable_candidates, ["chat-cap"]);
    assert_eq!(smart.deferrals_24h, 2, "both spellings count");
    assert!(raw(&wire, "smart")["deferrals_24h"].is_u64());
    assert!(raw(&wire, "smart")["fallback"].is_null());
    assert!(raw(&wire, "smart").get("fallback").is_some());
    assert_eq!(raw(&wire, "smart")["fallback_mode"], "inherit");
    let ModelEntry::CandidateAlias(fb) = find(&list, "smart-fb") else {
        panic!("smart-fb is a candidate alias")
    };
    assert!(fb.background);
    assert_eq!(fb.fallback_mode, "alias");
    assert_eq!(fb.fallback.as_deref(), Some("fast"));
    assert!(fb.fallback_usable);
    assert!(fb.problems.is_empty(), "{fb:?}");
    assert_eq!(fb.routable_candidates, ["chat-cap", "chat-gone"]);
    let ModelEntry::CandidateAlias(broken) = find(&list, "broken") else {
        panic!("broken is a candidate alias")
    };
    assert_eq!(broken.problems, ["candidate 'absent' does not exist"]);
    assert_eq!(smart.enabled_facets, ["structured_output"]);
    assert_eq!(smart.common_facets, ["structured_output"]);
    let ModelEntry::CandidateAlias(empty) = find(&list, "empty-list") else {
        panic!("empty-list is a candidate alias")
    };
    assert!(empty.candidates.is_empty() && empty.routable_candidates.is_empty());
    assert_eq!(empty.deferrals_24h, 0);
    assert_eq!(raw(&wire, "empty-list")["deferrals_24h"], json!(0));

    // local: readable weights bring the facts, unreadable ones the notes only.
    let ModelEntry::Local(cap) = find(&list, "chat-cap") else {
        panic!("chat-cap is local")
    };
    assert!(cap.public && cap.enabled);
    assert_eq!(cap.gguf_path, "chat.gguf");
    assert!(cap.facts.capabilities.is_some(), "{cap:?}");
    assert!(!cap.facts.notes.is_empty(), "{cap:?}");
    assert_eq!(cap.facts.context_length, None, "no n_ctx is configured");
    assert!(raw(&wire, "chat-cap").get("context_length").is_none());
    let ModelEntry::Local(gone) = find(&list, "chat-gone") else {
        panic!("chat-gone is local")
    };
    assert_eq!(gone.facts.capabilities, None);
    assert!(!gone.facts.notes.is_empty(), "{gone:?}");
    assert!(raw(&wire, "chat-gone").get("capabilities").is_none());
    assert!(raw(&wire, "chat-gone")["notes"].is_array());
    let ModelEntry::Local(private) = find(&list, "chat-private") else {
        panic!("chat-private is local")
    };
    assert!(!private.public);
    // Not published, so nothing is joined onto it.
    assert_eq!(private.facts, self_admin::ModelFacts::default());
    assert!(raw(&wire, "chat-private").get("notes").is_none());

    // aux: `pooling` is `null` when unset, never absent.
    let ModelEntry::Aux(emb) = find(&list, "embed/emb-cap") else {
        panic!("embed/emb-cap is aux")
    };
    assert_eq!(emb.aux_kind, "embed");
    assert_eq!(emb.pooling.as_deref(), Some("mean"));
    assert_eq!(emb.facts.context_length, Some(32_768));
    assert!(raw(&wire, "embed/emb-cap")["context_length"].is_u64());
    let ModelEntry::Aux(rr) = find(&list, "embed/rr-gone") else {
        panic!("rerank/rr-gone is aux")
    };
    assert_eq!(rr.aux_kind, "rerank");
    assert_eq!(rr.pooling, None);
    assert!(raw(&wire, "embed/rr-gone").get("pooling").is_some());
    assert!(raw(&wire, "embed/rr-gone")["pooling"].is_null());

    // audio: no context window exists, so the key is not there.
    let ModelEntry::Audio(voice) = find(&list, "audio/voice-1") else {
        panic!("audio/voice-1 is audio")
    };
    assert_eq!(
        (voice.task.as_str(), voice.family.as_str()),
        ("tts", "qwen3_tts")
    );
    assert_eq!(voice.facts.context_length, None);
    assert!(raw(&wire, "audio/voice-1").get("context_length").is_none());

    // image: the loader is whatever `model` holds, else `diffusion_model`,
    // else `null`; no modes means the default.
    let image = |name: &str| match find(&list, name) {
        ModelEntry::Image(m) => m.clone(),
        other => panic!("{name} is image, not {other:?}"),
    };
    let dit = image("image/pipe-dit");
    assert_eq!(dit.loads, Some(json!("dit.gguf")));
    assert_eq!(dit.modes, ["img_gen"]);
    assert!(!dit.edit);
    let all = image("image/pipe-all");
    assert_eq!(all.loads, Some(json!("all.safetensors")));
    assert_eq!(all.modes, ["img_gen", "vid_gen"]);
    assert!(all.edit);
    let none = image("image/pipe-none");
    assert_eq!(none.loads, None);
    assert!(raw(&wire, "image/pipe-none").get("loads").is_some());
    assert!(raw(&wire, "image/pipe-none")["loads"].is_null());

    // Passthrough: the enabled `expose_all` upstream only.
    assert_eq!(wire["passthrough_upstreams"].as_array().unwrap().len(), 1);
    assert_eq!(list.passthrough_upstreams[0].upstream, "open");
    assert_eq!(list.passthrough_upstreams[0].prefix, "op");
    assert!(list.passthrough_upstreams[0]
        .note
        .starts_with("expose_all upstream"));
}

/// The `kind` and `search` arguments narrow the list, and what is left reads
/// back the same.
#[tokio::test]
async fn the_models_tool_narrows_by_kind_and_search() {
    let w = world(false).await;
    let admin = Admin::new(&w.state).await;

    let only = |text: &str| -> Vec<String> {
        let list: self_admin::ModelList = read_back(text);
        list.models.iter().map(|m| m.name().to_string()).collect()
    };
    let aliases = admin.text("lmgw__models", json!({"kind": "alias"})).await;
    assert_eq!(
        only(&aliases),
        [
            "broken",
            "dormant",
            "empty-list",
            "fast",
            "m-llama-ok",
            "m-llama-old",
            "sleeping",
            "smart",
            "smart-fb",
            "unlisted"
        ]
    );
    let aux = admin.text("lmgw__models", json!({"kind": "aux"})).await;
    assert_eq!(only(&aux), ["embed/emb-cap", "embed/rr-gone"]);
    let images = admin.text("lmgw__models", json!({"kind": "image"})).await;
    assert_eq!(
        only(&images),
        ["image/pipe-all", "image/pipe-dit", "image/pipe-none"]
    );
    let audio = admin.text("lmgw__models", json!({"kind": "audio"})).await;
    assert_eq!(only(&audio), ["audio/ears-1", "audio/voice-1"]);
    let found = admin.text("lmgw__models", json!({"search": "SMART"})).await;
    assert_eq!(only(&found), ["smart", "smart-fb"]);

    // No expose-all upstream: the key is there, and an empty list.
    let wire = parse(&found);
    assert_eq!(wire["passthrough_upstreams"], json!([]));

    let (failed, text) = admin.call("lmgw__models", json!({"kind": "nope"})).await;
    assert!(failed);
    assert_eq!(
        text,
        "unknown kind 'nope' (expected all, alias, local, aux, audio or image)"
    );
}

/// A gateway with nothing configured answers two empty lists.
#[tokio::test]
async fn the_models_text_of_an_empty_gateway_is_two_empty_lists() {
    let state = AppState::init_for_tests().await.unwrap();
    let admin = Admin::new(&state).await;
    let text = admin.text("lmgw__models", json!({})).await;
    assert_eq!(
        text,
        serde_json::to_string_pretty(&json!({"models": [], "passthrough_upstreams": []})).unwrap()
    );
    let list: self_admin::ModelList = read_back(&text);
    assert_eq!(list, self_admin::ModelList::default());
}

// ---------------------------------------------------------------------------
// lmgw__upstreams
// ---------------------------------------------------------------------------

/// Each protocol, with and without a key, headers and facts.
#[tokio::test]
async fn the_upstreams_text_reads_back_equal_in_every_state() {
    let w = world(true).await;
    let admin = Admin::new(&w.state).await;
    let text = admin.text("lmgw__upstreams", json!({})).await;
    assert!(
        !text.contains("sk-secret") && !text.contains("sk-llama"),
        "{text}"
    );
    assert!(
        !text.contains("org-1") && !text.contains("team-2"),
        "{text}"
    );
    let list: dto::UpstreamsResponse = read_back(&text);
    let wire = parse(&text);
    let by = |name: &str| list.upstreams.iter().find(|u| u.name == name).unwrap();
    let raw = |name: &str| {
        wire["upstreams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["name"] == name)
            .unwrap()
    };

    // The dashboard's `UpstreamView`: a key is a `has_api_key` flag, headers
    // are (name, "<set>") pairs; a timeout is an integer.
    let acme = by("acme");
    assert!(acme.has_api_key);
    assert_eq!(
        acme.extra_headers,
        [
            ("X-Org".to_string(), "<set>".to_string()),
            ("X-Team".to_string(), "<set>".to_string())
        ]
    );
    assert_eq!(
        raw("acme")["extra_headers"],
        json!([["X-Org", "<set>"], ["X-Team", "<set>"]])
    );
    assert_eq!(
        (acme.protocol.as_str(), acme.kind.as_str()),
        ("openai", "generic")
    );
    assert_eq!(acme.timeout_ms, 45_000);
    assert!(raw("acme")["timeout_ms"].is_u64());
    assert!(acme.llama_facts.is_empty());
    assert_eq!(raw("acme")["llama_facts"], json!([]));
    assert!(raw("acme")["supports_responses"].is_boolean());
    assert_eq!(raw("acme")["has_api_key"], json!(true));
    assert!(raw("acme").get("api_key").is_none());
    let bare = by("bare");
    assert!(!bare.has_api_key);
    assert_eq!(raw("bare")["has_api_key"], json!(false));
    assert!(bare.extra_headers.is_empty());
    assert_eq!(raw("bare")["extra_headers"], json!([]));
    assert_eq!(bare.protocol, "anthropic");
    assert!(!by("off-gemini").enabled);
    assert_eq!(by("audio-box").kind, "audio_cpp");
    let open = by("open");
    assert!(open.expose_all);
    assert_eq!(open.expose_prefix, "op");
    assert_eq!(by("bare").expose_prefix, "");

    // A llama.cpp row carries the server's facts, why they are unknown, or an
    // empty list while nobody has asked (as does every other row).
    let ok = &by("llama-ok").llama_facts;
    assert_eq!(ok.len(), 1);
    assert_eq!(ok[0].model, "");
    assert!(!ok[0].router && ok[0].cached);
    assert_eq!(
        ok[0].props.as_ref().and_then(|p| p.build_info.as_deref()),
        Some("b11226-0c6a6a7")
    );
    assert!(raw("llama-ok")["llama_facts"][0]["read_at"].is_i64());
    let old = &by("llama-old").llama_facts;
    assert_eq!(old.len(), 1);
    assert!(
        old[0].props.is_none() && old[0].unknown.is_some(),
        "{old:?}"
    );
    let old_wire = &raw("llama-old")["llama_facts"][0];
    assert!(old_wire["props"].is_null() && old_wire.get("props").is_some());
    assert!(old_wire["unknown"].is_string());
    assert_eq!(by("llama-new").llama_facts, vec![]);
    assert_eq!(raw("llama-new")["llama_facts"], json!([]));
}

/// No upstream at all is an empty list under its key.
#[tokio::test]
async fn the_upstreams_text_of_an_empty_gateway_is_an_empty_list() {
    let state = AppState::init_for_tests().await.unwrap();
    let admin = Admin::new(&state).await;
    let text = admin.text("lmgw__upstreams", json!({})).await;
    assert_eq!(
        text,
        serde_json::to_string_pretty(&json!({"upstreams": []})).unwrap()
    );
    read_back::<dto::UpstreamsResponse>(&text);
}

// ---------------------------------------------------------------------------
// lmgw__logs
// ---------------------------------------------------------------------------

async fn log_row(state: &SharedState, row: NewRequestLog) -> i64 {
    store::insert_request_log(&state.db, &row).await.unwrap()
}

/// No request yet is an empty tail: the one key, an empty list.
#[tokio::test]
async fn the_logs_text_of_an_empty_gateway_is_an_empty_tail() {
    let state = AppState::init_for_tests().await.unwrap();
    let admin = Admin::new(&state).await;
    let text = admin.text("lmgw__logs", json!({})).await;
    assert_eq!(
        text,
        serde_json::to_string_pretty(&json!({"logs": []})).unwrap()
    );
    let tail: self_admin::LogTail = read_back(&text);
    assert_eq!(tail, self_admin::LogTail::default());
}

/// A bare row, one with every optional column set, and an MCP tool call: the
/// text reads back through the type and writes out equal for the whole tail
/// and for each narrowing. A row always has all seventeen keys; a column the
/// request never had is `null`, an integer stays an integer.
#[tokio::test]
async fn the_logs_text_reads_back_equal_with_rows_of_every_shape() {
    let state = AppState::init_for_tests().await.unwrap();
    let admin = Admin::new(&state).await;
    let bare = log_row(
        &state,
        NewRequestLog {
            ingress_proto: "openai".into(),
            requested_alias: "bare".into(),
            status: 200,
            ..Default::default()
        },
    )
    .await;
    let full = log_row(
        &state,
        NewRequestLog {
            ingress_proto: "anthropic".into(),
            requested_alias: "full".into(),
            upstream_name: Some("acme".into()),
            upstream_model: Some("acme-1".into()),
            status: 502,
            ttfb_ms: Some(120),
            total_ms: Some(900),
            prompt_tokens: Some(1000),
            completion_tokens: Some(50),
            streamed: true,
            error_kind: Some("upstream_error".into()),
            error_msg: Some("boom".into()),
            fallback_reason: Some("hold".into()),
            rung: Some(2),
            ..Default::default()
        },
    )
    .await;
    let tool = log_row(
        &state,
        NewRequestLog {
            ingress_proto: "mcp".into(),
            requested_alias: "docs__query".into(),
            mcp_tool: Some("docs__query".into()),
            status: 200,
            total_ms: Some(0),
            ..Default::default()
        },
    )
    .await;

    let text = admin.text("lmgw__logs", json!({})).await;
    let tail: self_admin::LogTail = read_back(&text);
    // Newest first.
    assert_eq!(
        tail.logs.iter().map(|r| r.log_id).collect::<Vec<_>>(),
        [tool, full, bare]
    );
    let raw = parse(&text);
    let rows = raw["logs"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert!(
        raw.get("count").is_none(),
        "the length is the list's: {text}"
    );
    for row in rows {
        assert_eq!(row.as_object().unwrap().len(), 17, "{row}");
    }
    // The bare row: the six keys it has, the eleven columns it lacks as null.
    let b = &rows[2];
    assert_eq!(
        (b["ingress_proto"].as_str(), b["requested_alias"].as_str()),
        (Some("openai"), Some("bare"))
    );
    assert!(b["status"].is_i64() && b["streamed"] == json!(false));
    for key in [
        "upstream_name",
        "upstream_model",
        "mcp_tool",
        "ttfb_ms",
        "total_ms",
        "prompt_tokens",
        "completion_tokens",
        "error_kind",
        "error_msg",
        "fallback_reason",
        "rung",
    ] {
        assert!(b[key].is_null(), "{key}: {b}");
    }
    // The full row: all of them set, the numbers integers.
    let f = &rows[1];
    assert_eq!(f["upstream_name"], json!("acme"));
    assert_eq!(f["upstream_model"], json!("acme-1"));
    assert_eq!(f["fallback_reason"], json!("hold"));
    assert_eq!(f["error_kind"], json!("upstream_error"));
    assert_eq!(f["error_msg"], json!("boom"));
    assert_eq!(f["streamed"], json!(true));
    for key in [
        "log_id",
        "status",
        "ttfb_ms",
        "total_ms",
        "prompt_tokens",
        "completion_tokens",
        "rung",
    ] {
        assert!(f[key].is_i64(), "{key}: {f}");
    }
    assert_eq!(f["rung"], json!(2));
    assert!(f["mcp_tool"].is_null());
    // The tool call: `mcp_tool` and a zero that is not a missing value.
    assert_eq!(rows[0]["mcp_tool"], json!("docs__query"));
    assert_eq!(rows[0]["total_ms"], json!(0));
    assert!(rows[0]["ttfb_ms"].is_null());

    // Each narrowing is the same type and the same text through it. A tool
    // call writes its own row once it is done, so the calls above are in the
    // log by now: `before_id` keeps the tail to the three rows made here.
    let after = tool + 1;
    for (args, ids) in [
        (json!({"limit": 2, "before_id": after}), vec![tool, full]),
        (json!({"errors_only": true}), vec![full]),
        (json!({"alias": "bare"}), vec![bare]),
        (json!({"before_id": full}), vec![bare]),
        (json!({"limit": 1, "before_id": tool}), vec![full]),
    ] {
        let text = admin.text("lmgw__logs", args.clone()).await;
        let tail: self_admin::LogTail = read_back(&text);
        assert_eq!(
            tail.logs.iter().map(|r| r.log_id).collect::<Vec<_>>(),
            ids,
            "{args}"
        );
    }

    // The tool calls above are rows of their own now: an MCP call by its tool
    // name, no tokens, a latency, and the same seventeen keys.
    let text = admin
        .text("lmgw__logs", json!({"alias": "lmgw__logs"}))
        .await;
    let tail: self_admin::LogTail = read_back(&text);
    assert!(tail.logs.len() >= 6, "{text}");
    let own = &tail.logs[0];
    assert_eq!(own.ingress_proto, "mcp");
    assert_eq!(own.mcp_tool.as_deref(), Some("lmgw__logs"));
    assert!(own.total_ms.is_some() && own.prompt_tokens.is_none());
    for row in parse(&text)["logs"].as_array().unwrap() {
        assert_eq!(row.as_object().unwrap().len(), 17, "{row}");
    }
}

// ---------------------------------------------------------------------------
// lmgw__settings
// ---------------------------------------------------------------------------

async fn set_settings(state: &SharedState, change: impl FnOnce(&mut Settings)) {
    let mut settings = store::load_settings(&state.db).await.unwrap();
    change(&mut settings);
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

/// A Chat profile: `chat_profile` only keeps the id of one that exists.
async fn profile(state: &SharedState, name: &str) -> i64 {
    let body: ProfileCreate = serde_json::from_value(json!({ "name": name })).unwrap();
    store::chat_profiles::create_chat_profile(&state.db, &create_kind(&body).unwrap())
        .await
        .unwrap()
        .unwrap()
        .id
}

/// What kind of JSON value this is, down to an integer against a float.
fn shape_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The keys of an object and the kind of value each holds, against a list.
fn assert_shapes(what: &str, v: &Value, expected: &[(&str, &str)]) {
    let got: BTreeMap<&str, &str> = v
        .as_object()
        .unwrap_or_else(|| panic!("{what} is not an object: {v}"))
        .iter()
        .map(|(k, x)| (k.as_str(), shape_of(x)))
        .collect();
    let want: BTreeMap<&str, &str> = expected.iter().copied().collect();
    assert_eq!(got, want, "{what}");
}

/// The top-level keys of the settings text with every optional unset, and
/// what each holds; `changed` overrides the kind of some.
fn top_shapes(changed: &[(&str, &'static str)]) -> Vec<(&'static str, &'static str)> {
    let mut all = vec![
        ("agent_origin_suffix", "string"),
        ("agent_script_image", "string"),
        ("audio", "object"),
        ("aux_router", "object"),
        ("auth_enabled", "bool"),
        ("bind_addr", "string"),
        ("build_update_check_hours", "int"),
        ("builds_dir", "null"),
        ("builds_dir_effective", "string"),
        ("builds_dir_warning", "null"),
        ("chat_archive_days", "int"),
        ("chat_feed_keepalive_max_s", "int"),
        ("chat_feed_keepalive_s", "int"),
        ("chat_feed_live_buffer", "int"),
        ("chat_feed_page_size", "int"),
        ("chat_feed_retention_days", "int"),
        ("chat_kb_budget_tokens", "int"),
        ("chat_pdf_mode", "string"),
        ("chat_profile", "null"),
        ("chat_purge_days", "int"),
        ("chat_read_aloud", "bool"),
        ("chat_speech_style", "string"),
        ("chat_stt_alias", "string"),
        ("chat_system_prompt", "string"),
        ("chat_system_prompt_is_builtin", "bool"),
        ("chat_tts_alias", "string"),
        ("chat_turn_detection", "string"),
        ("chat_voice", "string"),
        ("chat_voice_audio_input", "string"),
        ("chat_voice_language", "string"),
        ("chat_voice_reply_language", "string"),
        ("container_prefix", "string"),
        ("docs_embed_batch", "int"),
        ("docs_fetch_delay_ms", "int"),
        ("docs_ingest_reply_tokens", "int"),
        ("docs_rerank_model", "string"),
        ("docs_search", "object"),
        ("forge_tokens", "object"),
        ("has_hf_token", "bool"),
        ("has_update_token", "bool"),
        ("hold", "object"),
        ("image", "object"),
        ("jobs_max_rows", "int"),
        ("jobs_retention_days", "int"),
        ("max_body_mb", "int"),
        ("realtime", "object"),
        ("retention_days", "int"),
        ("retention_max_rows", "int"),
        ("router", "object"),
        ("sampling_alias", "string"),
        ("self_admin", "string"),
        ("update_check_enabled", "bool"),
        ("vram", "object"),
    ];
    for (key, kind) in changed {
        let slot = all.iter_mut().find(|(k, _)| k == key).unwrap();
        slot.1 = kind;
    }
    all
}

const CLASS_KEYS: [(&str, &str); 4] = [
    ("extra_run_args", "array"),
    ("image", "string"),
    ("models_dir", "string"),
    ("public_prefix", "string"),
];

/// The aux class: the class keys and its decision rows' prefix
/// (decision-models design §3.10).
const AUX_KEYS: [(&str, &str); 5] = [
    ("decision_prefix", "string"),
    ("extra_run_args", "array"),
    ("image", "string"),
    ("models_dir", "string"),
    ("public_prefix", "string"),
];

const AUDIO_KEYS: [(&str, &str); 15] = [
    ("backend", "string"),
    ("busy_timeout_ms", "int"),
    ("catalog_revision", "string"),
    ("device", "int"),
    ("extra_run_args", "array"),
    ("idle_unload_ms", "int"),
    ("image", "string"),
    ("lazy_load", "bool"),
    ("max_request_body_mb", "int"),
    ("min_free_memory_mb", "int"),
    ("models_dir", "string"),
    ("public_prefix", "string"),
    ("threads", "int"),
    ("voice_dir", "string"),
    ("voice_transcribe_alias", "string"),
];

const DOCS_SEARCH_KEYS: [(&str, &str); 8] = [
    ("budget_tokens", "int"),
    ("eval_k", "int"),
    ("fts_weights", "object"),
    ("k_fts", "int"),
    ("k_rerank", "int"),
    ("k_vec", "int"),
    ("limit", "int"),
    ("rrf_k", "float"),
];

const FTS_WEIGHT_KEYS: [(&str, &str); 4] = [
    ("derived_summary", "float"),
    ("derived_title", "float"),
    ("heading_path", "float"),
    ("payload", "float"),
];

/// The nested objects of a settings text: the kind of each key.
fn assert_nested_shapes(v: &Value, hold_alias: &str) {
    assert_shapes("router", &v["router"], &CLASS_KEYS);
    assert_shapes("aux_router", &v["aux_router"], &AUX_KEYS);
    assert_shapes("image", &v["image"], &CLASS_KEYS);
    assert_shapes("audio", &v["audio"], &AUDIO_KEYS);
    assert_shapes("docs_search", &v["docs_search"], &DOCS_SEARCH_KEYS);
    assert_shapes(
        "fts_weights",
        &v["docs_search"]["fts_weights"],
        &FTS_WEIGHT_KEYS,
    );
    assert_shapes(
        "hold",
        &v["hold"],
        &[("active", "bool"), ("fallback_alias", hold_alias)],
    );
    assert_shapes("vram", &v["vram"], &[("fallback_on_external", "bool")]);
}

/// A gateway on its defaults: the text reads back through `SettingsRead` and
/// writes out equal, every secret and every unset optional is a `null` that is
/// there, the ratios are floats and the counts integers.
#[tokio::test]
async fn the_settings_text_of_a_default_gateway_reads_back_equal() {
    let state = AppState::init_for_tests().await.unwrap();
    let admin = Admin::new(&state).await;
    let text = admin.text("lmgw__settings", json!({})).await;
    let settings: self_admin::SettingsRead = read_back(&text);
    let raw = parse(&text);
    assert_shapes("the settings", &raw, &top_shapes(&[]));
    assert_nested_shapes(&raw, "null");
    assert_eq!(raw["forge_tokens"], json!({}));
    assert_eq!(raw["self_admin"], json!("full"));
    assert_eq!(raw["chat_system_prompt_is_builtin"], json!(true));
    assert_eq!(raw["audio"]["catalog_revision"], json!("pinned"));
    assert_eq!(
        raw["realtime"]["default_instructions_is_builtin"],
        json!(true)
    );
    assert!(raw["docs_search"]["rrf_k"].is_f64());
    assert!(!settings.has_hf_token);
    assert_eq!(settings.chat_profile, None);
    assert_eq!(settings.hold.fallback_alias, None);
    assert_eq!(settings.self_admin, lmgw_api_types::AdminLevel::Full);
    assert!(settings.vram.fallback_on_external);
    assert!(settings.forge_tokens.is_empty());
}

/// Every optional set, every list non-empty: the tokens are `"<set>"`, the run
/// arguments one text a line each, the floats keep their fraction, and the
/// text still reads back and writes out equal.
#[tokio::test]
async fn the_settings_text_of_a_configured_gateway_reads_back_equal() {
    let state = AppState::init_for_tests().await.unwrap();
    let admin = Admin::new(&state).await;
    // Under /dev/shm where there is one, so the warning about builds in RAM is
    // there too; the same text with the warning `null` otherwise.
    let builds_dir = "/dev/shm/lmgw-settings-wire/builds";
    let in_ram = lmgw_core::backends::paths::is_tmpfs(std::path::Path::new(builds_dir));
    let calm = profile(&state, "Calm").await;
    set_settings(&state, |s| {
        s.bind_addr = "0.0.0.0:9".into();
        s.auth_enabled = true;
        s.retention_days = 3;
        s.retention_max_rows = 77;
        s.jobs_retention_days = 0;
        s.jobs_max_rows = 5;
        s.max_body_mb = 0;
        s.chat_archive_days = 4;
        s.chat_purge_days = 9;
        s.docs_ingest_reply_tokens = 123;
        s.docs_embed_batch = 7;
        s.docs_fetch_delay_ms = 1500;
        s.docs_search.k_fts = 11;
        s.docs_search.k_vec = 12;
        s.docs_search.rrf_k = 0.1;
        s.docs_search.fts_weights.payload = 0.3;
        s.docs_search.fts_weights.heading_path = 2.0;
        s.docs_search.fts_weights.derived_title = 1.7;
        s.docs_search.fts_weights.derived_summary = 0.05;
        s.docs_search.k_rerank = 0;
        s.docs_search.limit = 3;
        s.docs_search.budget_tokens = 1234;
        s.docs_search.eval_k = 4;
        s.docs_rerank_model = "rr".into();
        s.sampling_alias = "samp".into();
        s.self_admin = SelfAdmin::ReadOnly;
        s.update_check_enabled = false;
        s.hf_token = "hf_secret".into();
        s.update_token = "upd_secret".into();
        s.container_prefix = "pre".into();
        s.agent_origin_suffix = "example.test".into();
        s.agent_script_image = "img:1".into();
        s.hold.active = true;
        s.hold.fallback_alias = Some("fb".into());
        s.vram.fallback_on_external = false;
        s.router.image = "r-img".into();
        s.router.models_dir = "/r".into();
        s.router.extra_run_args = vec!["--a".into(), "--b=1".into()];
        s.router.public_prefix = "rp".into();
        s.aux_router.image = "a-img".into();
        s.aux_router.models_dir = "/a".into();
        s.aux_router.extra_run_args = vec!["--x".into()];
        s.aux_router.public_prefix = "ap".into();
        s.aux_router.decision_prefix = "dp".into();
        s.audio.image = "au".into();
        s.audio.models_dir = "/au".into();
        s.audio.backend = "vulkan".into();
        s.audio.device = 2;
        s.audio.threads = 6;
        s.audio.lazy_load = !s.audio.lazy_load;
        s.audio.busy_timeout_ms = 11;
        s.audio.idle_unload_ms = 22;
        s.audio.min_free_memory_mb = 33;
        s.audio.max_request_body_mb = 44;
        s.audio.voice_dir = "/v".into();
        s.audio.extra_run_args = vec!["--c".into(), "--d".into(), "--e".into()];
        s.audio.public_prefix = "audio2".into();
        s.audio.voice_transcribe_alias = "tr".into();
        s.audio.catalog_revision = CatalogRevision::Latest;
        s.image.image = "im".into();
        s.image.models_dir = "/im".into();
        s.image.extra_run_args = vec!["--i".into(), "--j".into()];
        s.image.public_prefix = "imp".into();
        s.chat_feed_retention_days = 0;
        s.chat_feed_keepalive_s = 9;
        s.chat_feed_keepalive_max_s = 11;
        s.chat_feed_page_size = 13;
        s.chat_feed_live_buffer = 14;
        s.chat_system_prompt = Some("my prompt".into());
        s.chat_profile = Some(calm);
        s.chat_pdf_mode = "ask".into();
        s.chat_stt_alias = "stt".into();
        s.chat_tts_alias = "tts".into();
        s.chat_voice = "alba".into();
        s.chat_speech_style = "calm".into();
        s.chat_voice_language = "de".into();
        s.chat_voice_reply_language = "fr".into();
        s.chat_read_aloud = true;
        s.chat_turn_detection = "push_to_talk".into();
        s.chat_voice_audio_input = "on".into();
        s.chat_kb_budget_tokens = 999;
        s.builds_dir = Some(builds_dir.into());
        s.forge_tokens.insert("github.com".into(), "tok1".into());
        s.forge_tokens
            .insert("git.example.com".into(), "tok2".into());
        s.build_update_check_hours = 12;
        s.realtime.default_instructions = Some("be brief".into());
        s.realtime.default_model = "rt".into();
        s.realtime.threshold = 0.3;
    })
    .await;
    let text = admin.text("lmgw__settings", json!({})).await;
    assert!(
        !text.contains("hf_secret") && !text.contains("upd_secret"),
        "{text}"
    );
    assert!(!text.contains("tok1") && !text.contains("tok2"), "{text}");
    let settings: self_admin::SettingsRead = read_back(&text);
    let raw = parse(&text);
    assert_shapes(
        "the settings",
        &raw,
        &top_shapes(&[
            ("builds_dir", "string"),
            ("builds_dir_warning", if in_ram { "string" } else { "null" }),
            ("chat_profile", "int"),
        ]),
    );
    assert_nested_shapes(&raw, "string");
    // The dashboard's names: the secrets are flags, never the value.
    assert_eq!(raw["has_hf_token"], json!(true));
    assert_eq!(raw["has_update_token"], json!(true));
    assert!(raw.get("hf_token").is_none() && raw.get("update_token").is_none());
    assert!(!text.contains("hf_secret") && !text.contains("upd_secret"));
    assert_eq!(raw["chat_profile"], json!(calm));
    assert_eq!(raw["builds_dir"], json!(builds_dir));
    assert_eq!(
        raw["forge_tokens"],
        json!({"git.example.com": "<set>", "github.com": "<set>"})
    );
    assert_eq!(raw["hold"], json!({"active": true, "fallback_alias": "fb"}));
    assert_eq!(raw["vram"], json!({"fallback_on_external": false}));
    assert_eq!(raw["router"]["extra_run_args"], json!(["--a", "--b=1"]));
    assert_eq!(raw["aux_router"]["extra_run_args"], json!(["--x"]));
    assert_eq!(raw["audio"]["extra_run_args"], json!(["--c", "--d", "--e"]));
    assert_eq!(raw["image"]["extra_run_args"], json!(["--i", "--j"]));
    assert_eq!(raw["audio"]["catalog_revision"], json!("latest"));
    assert_eq!(raw["audio"]["busy_timeout_ms"], json!(11));
    assert_eq!(raw["docs_search"]["rrf_k"], json!(0.1f32));
    assert_eq!(
        raw["docs_search"]["fts_weights"]["heading_path"],
        json!(2.0)
    );
    assert_eq!(raw["docs_search"]["budget_tokens"], json!(1234));
    assert_eq!(raw["self_admin"], json!("read_only"));
    assert_eq!(raw["chat_system_prompt"], json!("my prompt"));
    assert_eq!(raw["chat_system_prompt_is_builtin"], json!(false));
    assert_eq!(raw["realtime"]["default_instructions"], json!("be brief"));
    assert_eq!(
        raw["realtime"]["default_instructions_is_builtin"],
        json!(false)
    );
    assert!(settings.has_hf_token);
    assert_eq!(settings.chat_profile, Some(calm));
    assert_eq!(settings.builds_dir_warning.is_some(), in_ram);
    assert_eq!(settings.forge_tokens.len(), 2);
    assert_eq!(settings.self_admin, lmgw_api_types::AdminLevel::ReadOnly);
}

/// The other states of the optionals: an empty custom prompt is a prompt (not
/// the built-in one), no token is `null` again, a builds directory off tmpfs
/// has no warning, and `full` is spelled `full`.
#[tokio::test]
async fn the_settings_text_follows_each_optional_to_its_other_state() {
    let state = AppState::init_for_tests().await.unwrap();
    let admin = Admin::new(&state).await;
    let other = profile(&state, "Other").await;
    set_settings(&state, |s| {
        s.update_token = "x".into();
        s.hold.active = true;
        s.chat_system_prompt = Some(String::new());
        s.chat_profile = Some(other);
        s.builds_dir = Some("/var/tmp/lmgw-settings-wire/builds".into());
        s.audio.extra_run_args = vec![];
        s.docs_search.rrf_k = 60.0;
    })
    .await;
    let text = admin.text("lmgw__settings", json!({})).await;
    read_back::<self_admin::SettingsRead>(&text);
    let raw = parse(&text);
    assert_shapes(
        "the settings",
        &raw,
        &top_shapes(&[("builds_dir", "string"), ("chat_profile", "int")]),
    );
    assert_nested_shapes(&raw, "null");
    assert_eq!(raw["has_hf_token"], json!(false));
    assert_eq!(raw["has_update_token"], json!(true));
    assert_eq!(raw["chat_profile"], json!(other));
    assert_eq!(raw["hold"], json!({"active": true, "fallback_alias": null}));
    assert_eq!(raw["chat_system_prompt"], json!(""));
    assert_eq!(raw["chat_system_prompt_is_builtin"], json!(false));
    assert_eq!(raw["audio"]["extra_run_args"], json!([]));
    // A whole number stays a float: `60.0`, not `60`.
    assert_eq!(raw["docs_search"]["rrf_k"], json!(60.0));
    assert!(raw["docs_search"]["rrf_k"].is_f64());
    assert!(text.contains("\"rrf_k\": 60.0"), "{text}");
    assert_eq!(raw["self_admin"], json!("full"));
}
