//! `GET /api/models/full` over the real router: the live answer is read into
//! the document's `ModelsFull` and written back equal (`common::round_trips`),
//! on an empty gateway and on rows of every class with every field set, so a
//! key the server sends without the type (or the type without the server) fails
//! here. The key sets are pinned where the type alone cannot say: a key a row
//! leaves out stays out.

use std::collections::BTreeSet;

use lmgw_api_types as dto;
use lmgw_core::config::{AuxKind, HoldFallbackMode, LlamaParams, Protocol, UpstreamKind};
use lmgw_core::ir::{Params, ReasoningControl};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{
    self, NewAlias, NewAudioModel, NewAuxModel, NewCandidateAlias, NewImageModel, NewLocalModel,
    NewUpstream,
};
use serde_json::{json, Map, Value};

use crate::common::{self, Gw};

pub(crate) async fn get(gw: &Gw, path: &str) -> (u16, Value) {
    let r = gw.client().get(format!("{gw}{path}")).send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

pub(crate) async fn read(gw: &Gw, path: &str) -> Value {
    let (status, live) = get(gw, path).await;
    assert_eq!(status, 200, "{path}: {live}");
    live
}

pub(crate) fn keys(v: &Value) -> BTreeSet<&str> {
    v.as_object().unwrap().keys().map(String::as_str).collect()
}

pub(crate) fn set<'a>(names: &[&'a str]) -> BTreeSet<&'a str> {
    names.iter().copied().collect()
}

fn map(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

fn upstream(name: &str, expose_all: bool, enabled: bool, prefix: &str) -> NewUpstream {
    NewUpstream {
        name: name.into(),
        protocol: Protocol::Openai,
        kind: UpstreamKind::Generic,
        base_url: "http://127.0.0.1:1/v1".into(),
        api_key: None,
        extra_headers: vec![],
        timeout_ms: 5_000,
        enabled,
        expose_all,
        expose_prefix: prefix.into(),
        supports_responses: false,
        catalog_all_modalities: true,
    }
}

fn local(model_id: &str) -> NewLocalModel {
    NewLocalModel {
        model_id: model_id.into(),
        gguf_path: format!("{model_id}.gguf"),
        params: LlamaParams::default(),
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
    }
}

fn aux(model_id: &str, kind: AuxKind) -> NewAuxModel {
    NewAuxModel {
        model_id: model_id.into(),
        gguf_path: format!("{model_id}.gguf"),
        kind,
        pooling: None,
        ctx_size: None,
        ubatch_size: None,
        batch_size: None,
        mmproj_path: None,
        backend: None,
        threads: None,
        args: vec![],
        idle_seconds: 120,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: HoldFallbackMode::Inherit,
        hold_fallback: None,
    }
}

fn audio(model_id: &str) -> NewAudioModel {
    NewAudioModel {
        model_id: model_id.into(),
        family: "pocket_tts".into(),
        path: model_id.into(),
        task: "tts".into(),
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
        hold_fallback_mode: HoldFallbackMode::Inherit,
        hold_fallback: None,
    }
}

fn image(model_id: &str) -> NewImageModel {
    NewImageModel {
        model_id: model_id.into(),
        files: map(json!({"diffusion_model": format!("{model_id}/w.gguf")})),
        args: Default::default(),
        modes: vec![],
        edit: false,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        idle_seconds: 0,
        hold_fallback_mode: HoldFallbackMode::Inherit,
        hold_fallback: None,
        capabilities_override: None,
    }
}

/// The stored overrides of `z-full`: every field of `ir::Params` set, with no
/// `..Default::default()`, so a field added to the type fails to compile here.
fn z_full_params() -> Params {
    Params {
        temperature: Some(0.7),
        top_p: Some(0.95),
        top_k: Some(40),
        min_p: Some(0.05),
        repeat_penalty: Some(1.1),
        max_tokens: Some(4096),
        presence_penalty: Some(0.1),
        frequency_penalty: Some(-0.3),
        seed: Some(-7),
        stop: vec!["END".into()],
        reasoning: Some(ReasoningControl {
            enabled: Some(true),
            effort: Some("high".into()),
            budget_tokens: Some(2048),
        }),
    }
}

/// Every class twice: a row that sets nothing optional, and one that sets
/// every field, so the mirror of each row type is held to the whole row.
async fn fill(state: &SharedState) {
    let db = &state.db;
    let kilo = store::insert_upstream(db, &upstream("kilo", true, true, "kilo"))
        .await
        .unwrap();
    let bare = store::insert_upstream(db, &upstream("bare", true, true, "/"))
        .await
        .unwrap();
    store::insert_upstream(db, &upstream("off", true, false, "off"))
        .await
        .unwrap();
    let plain = store::insert_upstream(db, &upstream("plain", false, true, ""))
        .await
        .unwrap();
    let hide = |id: i64, names: &[&str]| {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        async move {
            store::set_passthrough_hidden(&state.db, id, &names, true)
                .await
                .unwrap()
        }
    };
    hide(kilo, &["z/model", "a/model", "m"]).await;
    hide(bare, &["only"]).await;

    store::insert_alias(
        db,
        &NewAlias {
            alias: "z-full".into(),
            upstream_id: kilo,
            upstream_model_id: "vendor/model".into(),
            param_overrides: z_full_params(),
            enabled: true,
            capabilities_override: Some(json!({"max_output_tokens": 100, "notes": ["n"]})),
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        db,
        &NewAlias {
            alias: "a-bare".into(),
            upstream_id: plain,
            upstream_model_id: "x".into(),
            param_overrides: Params::default(),
            enabled: false,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();

    store::insert_local_model(db, &local("c1")).await.unwrap();
    let mut full = local("c2-full");
    full.params = LlamaParams {
        ctx_size: Some(131072),
        n_predict: Some(8192),
        n_gpu_layers: Some(99),
        threads: Some(8),
        batch_size: Some(2048),
        ubatch_size: Some(512),
        parallel: Some(4),
        kv_unified: Some(true),
        kv_unified_per_slot: Some(32768),
        flash_attn: Some("on".into()),
        cache_type_k: Some("q8_0".into()),
        cache_type_v: Some("q8_0".into()),
        cache_ram: Some(4096),
        jinja: true,
        chat_template_file: Some("t.jinja".into()),
        reasoning_format: Some("deepseek".into()),
        reasoning: Some("on".into()),
        reasoning_budget: Some(1024),
        reasoning_preserve: Some(true),
        reasoning_effort: Some("medium".into()),
        chat_template_kwargs: map(json!({"enable_thinking": true, "n": 1.5})),
        temp: Some(0.6),
        top_p: Some(0.95),
        top_k: Some(20),
        min_p: Some(0.05),
        repeat_penalty: Some(1.05),
        presence_penalty: Some(0.35),
        seed: Some(42),
        mmproj_path: Some("mm.gguf".into()),
        no_mmproj: false,
        draft_gguf_path: Some("d.gguf".into()),
        spec_type: Some("draft".into()),
        spec_draft_n_max: Some(16),
        spec_draft_n_min: Some(2),
        spec_draft_ngl: Some("99".into()),
        fit: Some("on".into()),
        fit_ctx: Some(65536),
    };
    full.args = vec!["--no-warmup".into()];
    full.image = Some("quay.io/x/llama:1".into());
    full.extra_run_args = Some(vec!["--cpus=4".into()]);
    full.warm_start = true;
    full.public = false;
    full.enabled = false;
    full.hold_fallback_mode = HoldFallbackMode::Alias;
    full.hold_fallback = Some("z-full".into());
    full.capabilities_override = Some(json!({"max_output_tokens": 77}));
    full.ladder = vec![lmgw_core::ladder::Rung {
        gguf_path: "r1.gguf".into(),
        ctx_size: 262144,
    }];
    store::insert_local_model(db, &full).await.unwrap();

    store::insert_aux_model(db, &aux("rr", AuxKind::Rerank))
        .await
        .unwrap();
    let mut emb = aux("emb", AuxKind::Embed);
    emb.pooling = Some("mean".into());
    emb.ctx_size = Some(8192);
    emb.ubatch_size = Some(8192);
    emb.batch_size = Some(8192);
    emb.mmproj_path = Some("mm.gguf".into());
    emb.backend = Some(lmgw_core::config::AuxBackend::Cpu);
    emb.threads = Some(6);
    emb.args = vec!["--x".into()];
    emb.image = Some("img".into());
    emb.extra_run_args = Some(vec!["--cpus=1".into()]);
    emb.warm_start = true;
    emb.hold_fallback_mode = HoldFallbackMode::Alias;
    emb.hold_fallback = Some("z-full".into());
    store::insert_aux_model(db, &emb).await.unwrap();

    let bare_audio = store::insert_audio_model(db, &audio("tts-bare"))
        .await
        .unwrap();
    let mut tts = audio("tts-full");
    tts.lazy = Some(false);
    tts.busy_timeout_ms = Some(15000);
    tts.backend = Some("cpu".into());
    tts.threads = Some(6);
    tts.load_options = map(json!({"precision": "fp16", "n": 0.25}));
    tts.session_options = map(json!({"seed": 3}));
    tts.default_request_options = map(json!({"speed": 1.1}));
    tts.model_spec_override = Some("spec.json".into());
    tts.config_id = Some("cfg".into());
    tts.weight_id = Some("w".into());
    tts.voice_presets = map(json!({"alba": {"file": "a.wav"}}));
    tts.default_voice_preset = Some(json!("alba"));
    tts.image = Some("audio-img".into());
    tts.extra_run_args = Some(vec!["--a".into()]);
    tts.warm_start = true;
    tts.hold_fallback_mode = HoldFallbackMode::Alias;
    tts.hold_fallback = Some("z-full".into());
    let full_audio = store::insert_audio_model(db, &tts).await.unwrap();
    let mut cpu = audio("tts-cpu");
    cpu.backend = Some("cpu".into());
    store::insert_audio_model(db, &cpu).await.unwrap();
    // A figure learned for another configuration is kept but not charged; the
    // current configuration's is.
    store::set_audio_model_residency(db, bare_audio, Some((123_456_789, "stale-key")))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let snap = state.snapshot();
    let row = snap
        .audio_models
        .iter()
        .find(|m| m.id == full_audio)
        .unwrap();
    let key = lmgw_core::vram::residency::resident_key(row, &snap.settings.audio);
    store::set_audio_model_residency(db, full_audio, Some((987_654_321, &key)))
        .await
        .unwrap();

    let first = store::insert_image_model(db, &image("img-a"))
        .await
        .unwrap();
    let mut pipeline = image("img-b");
    pipeline.files = map(json!({"diffusion_model": "b/w.gguf", "vae": "b/ae.safetensors"}));
    pipeline.args = map(json!({"steps": 8, "cfg": 1.5, "diffusion_fa": true}));
    pipeline.modes = vec!["img_gen".into(), "vid_gen".into()];
    pipeline.edit = true;
    pipeline.enabled = false;
    pipeline.image = Some("sd-img".into());
    pipeline.extra_run_args = Some(vec!["--x".into()]);
    pipeline.warm_start = true;
    pipeline.idle_seconds = 600;
    pipeline.hold_fallback_mode = HoldFallbackMode::Alias;
    pipeline.hold_fallback = Some("z-full".into());
    pipeline.capabilities_override = Some(json!({"notes": ["i"]}));
    store::insert_image_model(db, &pipeline).await.unwrap();
    store::set_image_model_peak(db, first, Some(2_000_000_000))
        .await
        .unwrap();

    for (alias, background, fallback) in [("pick", false, None), ("bg-job", true, Some("z-full"))] {
        store::insert_candidate_alias(
            db,
            &NewCandidateAlias {
                alias: alias.into(),
                candidates: vec!["c1".into(), "c2-full".into(), "ghost".into()],
                background,
                fallback_mode: if fallback.is_some() {
                    HoldFallbackMode::Alias
                } else {
                    HoldFallbackMode::Inherit
                },
                fallback: fallback.map(str::to_string),
                capabilities_disabled: vec!["vision".into()],
                capabilities_enabled: vec![],
                enabled: true,
                notes: "n".into(),
            },
        )
        .await
        .unwrap();
    }
    for alias in ["bg-job", "bg-job", "pick"] {
        sqlx::query(
            "INSERT INTO request_logs (ts, ingress_proto, requested_alias, status, error_kind)
             VALUES (datetime('now'), 'openai', ?, 503, 'gpu_deferred')",
        )
        .bind(alias)
        .execute(db)
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
}

#[tokio::test]
async fn an_empty_gateway_lists_no_model_of_any_class() {
    let gw = common::serve(AppState::init_for_tests().await.unwrap()).await;
    let live = read(&gw, "/api/models/full").await;
    let full: dto::ModelsFull = common::round_trips("models-full, empty", &live);
    assert_eq!(full, dto::ModelsFull::default());
    assert_eq!(
        keys(&live),
        set(&[
            "aliases",
            "aux",
            "audio",
            "candidate_aliases",
            "image",
            "local",
            "passthrough",
        ])
    );
}

/// Every class with every field set reads back as the document's type, in
/// the order each list has always had, and each row is the row whole.
#[tokio::test]
async fn every_class_with_every_field_set_is_the_documented_type() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state.clone()).await;
    fill(&state).await;
    let live = read(&gw, "/api/models/full").await;
    let full: dto::ModelsFull = common::round_trips("models-full", &live);

    let names = |v: Vec<&str>| v.into_iter().map(String::from).collect::<Vec<_>>();
    assert_eq!(
        full.aliases
            .iter()
            .map(|a| a.alias.as_str())
            .collect::<Vec<_>>(),
        ["a-bare", "z-full"]
    );
    let z = &full.aliases[1];
    assert_eq!(z.upstream_name.as_deref(), Some("kilo"));
    assert_eq!(z.param_overrides.temperature, Some(0.7));
    assert_eq!(z.param_overrides.min_p, Some(0.05));
    assert_eq!(z.param_overrides.repeat_penalty, Some(1.1));
    // Tripwire: nothing the row stores is dropped on the way out, nothing is invented.
    assert_eq!(
        live["aliases"][1]["param_overrides"],
        serde_json::to_value(z_full_params()).unwrap()
    );
    assert_eq!(z.param_overrides.stop, names(vec!["END"]));
    assert_eq!(
        z.param_overrides
            .reasoning
            .as_ref()
            .unwrap()
            .effort
            .as_deref(),
        Some("high")
    );
    // A row with no override leaves the key out.
    assert!(live["aliases"][0].get("capabilities_override").is_none());
    assert!(live["aliases"][1].get("capabilities_override").is_some());

    // Chat rows, in id order of their model ids.
    assert_eq!(
        full.local
            .iter()
            .map(|m| m.public_name.as_str())
            .collect::<Vec<_>>(),
        ["c1", "c2-full"]
    );
    let c2 = &full.local[1].model;
    assert_eq!(c2.params.ctx_size, Some(131072));
    assert_eq!(c2.params.chat_template_kwargs["n"], json!(1.5));
    assert_eq!(c2.hold_fallback_mode, "alias");
    assert_eq!(c2.ladder.len(), 1);
    assert_eq!(c2.extra_run_args, Some(vec!["--cpus=4".to_string()]));
    assert_eq!(full.local[0].model.hold_fallback_mode, "inherit");
    assert!(live["local"][0]["model"]
        .get("capabilities_override")
        .is_none());

    assert_eq!(
        full.aux
            .iter()
            .map(|m| (m.model.kind.as_str(), m.public_name.as_str()))
            .collect::<Vec<_>>(),
        [("embed", "embed/emb"), ("rerank", "embed/rr")]
    );
    assert_eq!(full.aux[0].model.pooling.as_deref(), Some("mean"));

    // What each audio row is charged, and where its threads come from.
    let by_name = |n: &str| {
        full.audio
            .iter()
            .find(|v| v.public_name == format!("audio/{n}"))
            .unwrap()
    };
    let bare = by_name("tts-bare");
    assert_eq!(bare.residency_charged_bytes, None);
    assert_eq!(bare.model.residency.as_ref().unwrap().bytes, 123_456_789);
    assert_eq!(
        (bare.runs_on.as_str(), bare.threads_source.as_str()),
        ("gpu", "class")
    );
    let tts = by_name("tts-full");
    assert_eq!(tts.residency_charged_bytes, Some(987_654_321));
    assert_eq!(
        (
            tts.runs_on.as_str(),
            tts.threads_in_effect,
            tts.threads_source.as_str()
        ),
        ("cpu", 6, "row")
    );
    assert_eq!(by_name("tts-cpu").threads_source, "cores");
    assert!(full.audio.iter().all(|v| v.residency_note.is_some()));
    assert_eq!(
        keys(&live["audio"][0]),
        set(&[
            "model",
            "public_name",
            "residency_charged_bytes",
            "residency_note",
            "runs_on",
            "threads_in_effect",
            "threads_source",
        ])
    );

    assert_eq!(
        full.image
            .iter()
            .map(|m| m.public_name.as_str())
            .collect::<Vec<_>>(),
        ["image/img-a", "image/img-b"]
    );
    assert_eq!(full.image[0].model.peak_extra_bytes, Some(2_000_000_000));
    assert_eq!(full.image[1].model.modes, names(vec!["img_gen", "vid_gen"]));
    assert_eq!(full.image[1].model.args["cfg"], json!(1.5));

    // Candidate aliases, sorted, with the deferrals of the last day.
    let candidates: Vec<_> = full
        .candidate_aliases
        .iter()
        .map(|c| (c.alias.as_str(), c.deferrals_24h))
        .collect();
    assert_eq!(candidates, [("bg-job", Some(2)), ("pick", Some(1))]);

    // The exposed, enabled upstreams only, by name; each one's hidden models
    // by id.
    let passthrough: Vec<_> = full
        .passthrough
        .iter()
        .map(|p| (p.upstream.as_str(), p.prefix.as_str(), p.hidden.as_slice()))
        .collect();
    assert_eq!(
        passthrough,
        [
            ("bare", "", &["only".to_string()][..]),
            (
                "kilo",
                "kilo",
                &["a/model", "m", "z/model"].map(String::from)[..]
            ),
        ]
    );
    assert_eq!(
        keys(&live["passthrough"][0]),
        set(&["hidden", "id", "prefix", "upstream"])
    );
}

/// A class's public prefix is trimmed of its slashes, and an empty one leaves
/// the bare model id.
#[tokio::test]
async fn the_public_names_follow_each_class_prefix() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state.clone()).await;
    store::insert_local_model(&state.db, &local("c1"))
        .await
        .unwrap();
    store::insert_aux_model(&state.db, &aux("emb", AuxKind::Embed))
        .await
        .unwrap();
    store::insert_audio_model(&state.db, &audio("tts"))
        .await
        .unwrap();
    store::insert_image_model(&state.db, &image("img"))
        .await
        .unwrap();
    for (local, aux, audio, image, want) in [
        (
            "local",
            "embed/",
            "/audio/",
            "img",
            ["local/c1", "embed/emb", "audio/tts", "img/img"],
        ),
        ("", "", "", "", ["c1", "emb", "tts", "img"]),
        (
            "//a//b//",
            "x",
            "y/",
            "/",
            ["a//b/c1", "x/emb", "y/tts", "img"],
        ),
    ] {
        let mut s = state.snapshot().settings.clone();
        s.router.public_prefix = local.into();
        s.aux_router.public_prefix = aux.into();
        s.audio.public_prefix = audio.into();
        s.image.public_prefix = image.into();
        store::save_settings(&state.db, &s).await.unwrap();
        state.reload_snapshot().await.unwrap();
        let live = read(&gw, "/api/models/full").await;
        let full: dto::ModelsFull = common::round_trips("models-full, prefixes", &live);
        let got = [
            full.local[0].public_name.as_str(),
            full.aux[0].public_name.as_str(),
            full.audio[0].public_name.as_str(),
            full.image[0].public_name.as_str(),
        ];
        assert_eq!(got, want, "prefixes {local:?} {aux:?} {audio:?} {image:?}");
    }
}

/// Every aux row says the thread count its container starts with and where it
/// comes from (decision-models WP6), the same in the models list and in the
/// row's own read: its own count, a count its extra args name, this machine's
/// cores on the CPU, or llama-server's choice on the GPU. Both answers are the
/// document's types, and the rendered command line carries what they say.
#[tokio::test]
async fn aux_rows_say_their_thread_count_and_where_it_comes_from() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state.clone()).await;
    let cpu = Some(lmgw_core::config::AuxBackend::Cpu);
    let mut own = aux("own", AuxKind::Decision);
    own.backend = cpu;
    own.threads = Some(6);
    own.args = vec!["--threads".into(), "2".into()];
    let mut written = aux("written", AuxKind::Embed);
    written.args = vec!["-t".into(), "3".into()];
    let mut cores = aux("cores", AuxKind::Decision);
    cores.backend = cpu;
    let mut odd = aux("odd", AuxKind::Embed);
    odd.backend = cpu;
    odd.args = vec!["--threads=many".into()];
    let gpu = aux("gpu", AuxKind::Rerank);
    for row in [&own, &written, &cores, &odd, &gpu] {
        store::insert_aux_model(&state.db, row).await.unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let host = lmgw_core::host::cpu();
    let host_source = match host.source {
        lmgw_core::host::CoreSource::Topology => "cores",
        lmgw_core::host::CoreSource::Logical => "logical_cpus",
    };
    let host_cores = i64::try_from(host.physical_cores).unwrap();

    let live = read(&gw, "/api/models/full").await;
    let full: dto::ModelsFull = common::round_trips("models-full, aux threads", &live);
    assert_eq!(
        keys(&live["aux"][0]),
        set(&[
            "licence",
            "model",
            "public_name",
            "threads_in_effect",
            "threads_source"
        ])
    );
    let want: [(&str, Option<i64>, &str); 5] = [
        ("own", Some(6), "row"),
        ("written", Some(3), "args"),
        ("cores", Some(host_cores), host_source),
        ("odd", None, "args"),
        ("gpu", None, "llama_server"),
    ];
    for (model_id, threads, source) in want {
        let v = full
            .aux
            .iter()
            .find(|v| v.model.model_id == model_id)
            .unwrap();
        let wire = serde_json::to_value(v.threads_source).unwrap();
        assert_eq!(
            (v.threads_in_effect, wire.as_str().unwrap()),
            (threads, source),
            "{model_id}"
        );

        let live = read(
            &gw,
            &format!("/api/local-model?target=aux&model_id={model_id}"),
        )
        .await;
        let got: dto::LocalModelRead = common::round_trips("local-model, aux threads", &live);
        let dto::LocalModelRead::Aux(d) = got else {
            panic!("{model_id}: not an aux read: {live}");
        };
        assert_eq!(
            (d.threads_in_effect, d.threads_source),
            (v.threads_in_effect, v.threads_source),
            "{model_id}: the list and the row's read agree"
        );
        // A count from a field (the row's or the host's) is rendered and
        // claimed, so an args copy is dropped; an args count stays as
        // written; llama-server's own needs no flag.
        let words = d.command_line.split_whitespace().collect::<Vec<_>>();
        let counts: Vec<&str> = words
            .windows(2)
            .filter(|w| matches!(w[0], "--threads" | "-t"))
            .map(|w| w[1])
            .collect();
        let want: Vec<String> = threads.iter().map(i64::to_string).collect();
        assert_eq!(counts, want, "{model_id}: {}", d.command_line);
    }
    let odd = read(&gw, "/api/local-model?target=aux&model_id=odd").await;
    assert!(
        odd["command_line"]
            .as_str()
            .unwrap()
            .contains("--threads=many"),
        "an unreadable args count still renders as written: {odd}"
    );
}
