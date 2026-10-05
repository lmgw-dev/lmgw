//! audio.cpp backend tests: server.json generation, Podman command
//! construction, the spec-catalog parser, store CRUD, and the /v1/audio/*
//! proxy — both as a passthrough onto a remote upstream and, since per-model
//! containers §5, as a local route that reaches its own container through
//! admission.

use std::path::Path;
use std::sync::Arc;

use lmgw_core::config::{AudioModel, AudioSettings};
use lmgw_core::runtime::audio::render_server_config;
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewAudioModel};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common;
use common::{serve, Gw};

fn model(id: &str, family: &str, path: &str, task: &str, enabled: bool) -> AudioModel {
    AudioModel {
        id: 0,
        model_id: id.into(),
        family: family.into(),
        path: path.into(),
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
        enabled,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        residency: None,
    }
}

#[test]
fn renders_server_config_for_enabled_models() {
    let mut tts = model(
        "pocket-tts",
        "pocket_tts",
        "audio-cpp/audio.cpp-gguf/PocketTTS-GGUF/english",
        "tts",
        true,
    );
    tts.load_options
        .insert("language".into(), Value::String("english".into()));
    tts.session_options
        .insert("voice_state_cache_slots".into(), json!(8));
    let disabled = model("off", "x", "y", "tts", false);
    let asr = model(
        "qwen3-asr",
        "qwen3_asr",
        "audio-cpp/audio.cpp-gguf/Qwen3-ASR-0.6B-GGUF",
        "asr",
        true,
    );
    let rendered = render_server_config(&AudioSettings::default(), &[tts, disabled, asr]);
    let v: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(v["host"], "0.0.0.0");
    assert_eq!(v["port"], 8080);
    assert_eq!(v["backend"], "cuda");
    assert_eq!(v["device"], 0);
    assert_eq!(v["threads"], 1);
    assert_eq!(v["lazy_load"], true);
    let models = v["models"].as_array().unwrap();
    assert_eq!(models.len(), 2, "disabled model must be omitted");
    assert_eq!(models[0]["id"], "pocket-tts");
    assert_eq!(models[0]["family"], "pocket_tts");
    assert_eq!(
        models[0]["path"],
        "/models/audio-cpp/audio.cpp-gguf/PocketTTS-GGUF/english"
    );
    assert_eq!(models[0]["task"], "tts");
    assert_eq!(models[0]["mode"], "offline");
    assert_eq!(models[0]["load_options"]["language"], "english");
    assert_eq!(models[0]["session_options"]["voice_state_cache_slots"], 8);
    // No options → keys omitted so audiocpp's defaults apply.
    assert!(models[1].get("load_options").is_none());
    assert!(models[1].get("session_options").is_none());
    assert!(models[1].get("voice_presets").is_none());
    assert!(models[1].get("default_voice_preset").is_none());
    // Deterministic output: same input → same bytes.
    let again = render_server_config(
        &AudioSettings::default(),
        &[
            model("a", "f", "p", "tts", true),
            model("b", "f", "p", "asr", true),
        ],
    );
    assert_eq!(
        again,
        render_server_config(
            &AudioSettings::default(),
            &[
                model("a", "f", "p", "tts", true),
                model("b", "f", "p", "asr", true),
            ],
        )
    );
}

/// The class-wide engine bounds are *stated* in the file rather than left to
/// whatever default the image happens to carry, so reading a container's
/// config answers "why did that request get a 503" without knowing which
/// build is inside it. `0` is the engine's own "no bound" on each.
#[test]
fn renders_the_class_bounds_and_the_voice_library() {
    let v: Value = serde_json::from_str(&render_server_config(
        &AudioSettings::default(),
        &[model("a", "f", "p", "tts", true)],
    ))
    .unwrap();
    assert_eq!(v["busy_timeout_ms"], 300_000, "audio.cpp's own default");
    assert_eq!(v["idle_unload_ms"], 0, "0 = never unload an idle model");
    assert_eq!(v["min_free_memory_mb"], 0, "0 = no memory guard");
    assert_eq!(
        v["voice_dir"], "/models/voices",
        "the Audio lab's own library, as the container sees it"
    );
    // 0 MiB leaves the key out entirely, so audiocpp_server's 2 GiB applies.
    assert!(v.get("max_request_body_bytes").is_none());

    let mut s = AudioSettings {
        idle_unload_ms: 900_000,
        min_free_memory_mb: 512,
        max_request_body_mb: 64,
        ..AudioSettings::default()
    };
    s.voice_dir = String::new();
    let v: Value = serde_json::from_str(&render_server_config(
        &s,
        &[model("a", "f", "p", "tts", true)],
    ))
    .unwrap();
    assert_eq!(v["idle_unload_ms"], 900_000);
    assert_eq!(v["min_free_memory_mb"], 512);
    assert_eq!(v["max_request_body_bytes"], 64 * 1024 * 1024);
    assert!(
        v.get("voice_dir").is_none(),
        "an empty library is no library, not an empty path"
    );
}

/// The per-model keys audio.cpp grew: each is omitted when the row says
/// nothing, so a row that configures none of them renders the bytes it
/// rendered before the columns existed — which is what keeps container
/// adoption from evicting every audio container on the first boot after the
/// upgrade.
#[test]
fn renders_per_model_engine_keys_only_when_set() {
    let plain = model("plain", "pocket_tts", "p", "tts", true);
    let before = render_server_config(&AudioSettings::default(), std::slice::from_ref(&plain));
    let v: Value = serde_json::from_str(&before).unwrap();
    for key in [
        "lazy",
        "busy_timeout_ms",
        "model_spec_override",
        "config",
        "weight",
        "default_request_options",
    ] {
        assert!(
            v["models"][0].get(key).is_none(),
            "{key} must stay out of a row that does not set it"
        );
    }

    let mut loaded = plain;
    loaded.lazy = Some(false);
    loaded.busy_timeout_ms = Some(900_000);
    loaded.model_spec_override = Some("specs/yue2.json".into());
    loaded.config_id = Some("base".into());
    loaded.weight_id = Some("q8_0".into());
    loaded
        .default_request_options
        .insert("speed".into(), json!(1.1));
    let v: Value =
        serde_json::from_str(&render_server_config(&AudioSettings::default(), &[loaded])).unwrap();
    let m = &v["models"][0];
    assert_eq!(m["lazy"], false, "eager, against a lazy class");
    assert_eq!(m["busy_timeout_ms"], 900_000);
    assert_eq!(
        m["model_spec_override"], "/models/specs/yue2.json",
        "a path under the models dir, as the container sees it"
    );
    // The server reads the short spelling; the columns carry the long one.
    assert_eq!(m["config"], "base");
    assert_eq!(m["weight"], "q8_0");
    assert!(m.get("config_id").is_none() && m.get("weight_id").is_none());
    assert_eq!(m["default_request_options"]["speed"], 1.1);
}

/// Voice config is what makes a TTS model's speaker repeatable — a request
/// that names no voice gets the default preset injected upstream — so it has
/// to reach server.json in audio.cpp's shape.
#[test]
fn renders_voice_presets_and_default() {
    let mut named = model("fish", "fish_audio", "Fish-Audio-S2-Pro-GGUF", "tts", true);
    named.voice_presets.insert(
        "narrator".into(),
        json!({"voice_ref": "/models/voices/narrator.wav", "reference_text": "hello there"}),
    );
    named.default_voice_preset = Some(Value::String("narrator".into()));

    let mut inline = model(
        "pocket",
        "pocket_tts",
        "PocketTTS-GGUF/english",
        "tts",
        true,
    );
    inline.default_voice_preset = Some(json!({"voice_id": "alba"}));

    let v: Value = serde_json::from_str(&render_server_config(
        &AudioSettings::default(),
        &[named, inline],
    ))
    .unwrap();
    let models = v["models"].as_array().unwrap();
    assert_eq!(
        models[0]["voice_presets"]["narrator"]["voice_ref"],
        "/models/voices/narrator.wav"
    );
    assert_eq!(models[0]["default_voice_preset"], "narrator");
    // An inline preset needs no `voice_presets` entry.
    assert!(models[1].get("voice_presets").is_none());
    assert_eq!(models[1]["default_voice_preset"]["voice_id"], "alba");
}

// ---------------------------------------------------------------------------
// Spec catalog parsing
// ---------------------------------------------------------------------------

#[test]
fn parses_model_spec_with_recommended_and_download_overrides() {
    let v = json!({
        "family": "pocket_tts",
        "display_name": "PocketTTS",
        "description": "Small TTS.",
        "category": "tts",
        "tasks": ["tts", "clone"],
        "modes": ["offline"],
        "languages": ["en", "de"],
        "ui": { "recommended_package": "pocket_tts_english_q8_0" },
        "package_defaults": {
            "download": { "kind": "huggingface_snapshot", "repo": "audio-cpp/audio.cpp-gguf" }
        },
        "packages": [
            {
                "id": "pocket_tts_english_q8_0",
                "display_name": "PocketTTS English Q8_0 GGUF",
                "default": true,
                "format": "gguf",
                "precision": "q8_0",
                "target_directory": "PocketTTS-GGUF/english",
                "files": ["PocketTTS-GGUF/english/pocket-tts-english-q8_0.gguf"]
            },
            {
                "id": "pocket_tts_english_safetensors",
                "display_name": "PocketTTS English Safetensors",
                "format": "safetensors",
                "precision": "native",
                "target_directory": "pocket-tts",
                "files": ["languages/english/model.safetensors"],
                "download": { "kind": "huggingface_snapshot", "repo": "kyutai/pocket-tts" }
            },
            { "id": "", "files": [] }
        ]
    });
    let spec = lmgw_core::audio::parse_spec(&v);
    assert_eq!(spec.family, "pocket_tts");
    assert_eq!(spec.display_name, "PocketTTS");
    assert_eq!(spec.tasks, vec!["tts", "clone"]);
    assert_eq!(spec.languages, vec!["en", "de"]);
    // Garbage entry (no id, no files) is dropped.
    assert_eq!(spec.packages.len(), 2);
    // The ui recommendation wins.
    let rec = spec.recommended().unwrap();
    assert_eq!(rec.id, "pocket_tts_english_q8_0");
    // Repo resolution: family default vs per-package override.
    assert_eq!(
        spec.package_repo(&spec.packages[0]),
        Some("audio-cpp/audio.cpp-gguf")
    );
    assert_eq!(
        spec.package_repo(&spec.packages[1]),
        Some("kyutai/pocket-tts")
    );
}

/// The spec grew a typed option schema, a maturity status and a gated flag
/// after lmgw's parser was written, and a parser that drops them leaves the
/// operator guessing what a family's `load_options` may hold.
#[test]
fn parses_the_specs_status_options_and_gated_download() {
    let spec = lmgw_core::audio::parse_spec(&json!({
        "family": "yue2",
        "display_name": "YuE2",
        "description": "Song generation.",
        "category": "audio_generation",
        "status": "supported",
        "tasks": ["music"],
        "modes": ["offline"],
        "languages": ["en"],
        "capabilities": { "gen": ["lyrics", "style"] },
        "ui": {
            "recommended_package": "yue2_q8_0",
            "tags": ["Music", "GGUF"],
            "docs": ["docs/models/yue2.md"],
            "summary": "Lyrics to song.",
            "builtin_voices": ["alba", "cosette"],
            "default_voice": "alba"
        },
        "options": {
            "request": [
                { "name": "style", "type": "string", "description": "Song style.",
                  "required": true },
                { "name": "cot", "type": "enum", "values": ["off", "melody", "full"],
                  "description": "Planning route.", "required": false, "default": "full" },
                { "name": "guidance_scale", "type": "float", "description": "CFG.",
                  "required": false, "min": 1.0, "max": 10.0, "default": 1.5 }
            ],
            "session": [
                { "name": "weight_type", "type": "enum", "preset": "weight_type_full",
                  "description": "Weight storage.", "required": false, "default": "native" }
            ],
            "load": []
        },
        "package_defaults": {
            "download": { "kind": "huggingface_snapshot", "repo": "audio-cpp/yue2",
                          "revision": "main", "gated": true }
        },
        "packages": [
            { "id": "yue2_q8_0", "display_name": "YuE2 Q8_0", "default": true,
              "format": "gguf", "precision": "q8_0", "target_directory": "YuE2-GGUF",
              "strip_prefix": "YuE2-GGUF", "description": "Fits a 24 GB card.",
              "files": ["YuE2-GGUF/yue2-q8_0.gguf"] }
        ]
    }));
    assert_eq!(spec.status, "supported");
    assert_eq!(spec.tags, ["Music", "GGUF"]);
    assert_eq!(spec.docs, ["docs/models/yue2.md"]);
    assert_eq!(spec.summary, "Lyrics to song.");
    assert_eq!(spec.builtin_voices, ["alba", "cosette"]);
    assert_eq!(spec.default_voice.as_deref(), Some("alba"));
    assert_eq!(spec.capabilities["gen"][0], "lyrics");

    let req = &spec.options.request;
    assert_eq!(req.len(), 3);
    assert!(req[0].required, "the spec says style is not optional");
    assert_eq!(req[1].values, ["off", "melody", "full"]);
    assert_eq!(req[1].default.as_ref().unwrap(), "full");
    assert_eq!(req[2].min, Some(1.0));
    assert_eq!(req[2].max, Some(10.0));
    // A named preset is expanded, so a dropdown can be drawn from the spec
    // alone — audio.cpp keeps the values in a C++ table, not in the JSON.
    assert_eq!(
        spec.options.session[0].values,
        ["native", "f32", "f16", "bf16", "q8_0"]
    );
    assert!(spec.options.load.is_empty());

    let pkg = &spec.packages[0];
    assert_eq!(pkg.strip_prefix, "YuE2-GGUF");
    assert_eq!(pkg.description, "Fits a 24 GB card.");
    let dl = spec.default_download.as_ref().unwrap();
    assert!(
        dl.gated,
        "a gated repo needs a token before anything queues"
    );
    assert_eq!(dl.revision.as_deref(), Some("main"));
}

// ---------------------------------------------------------------------------
// Store CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn audio_model_crud_round_trips_json_options() {
    let db = store::open_in_memory().await.unwrap();
    let new = NewAudioModel {
        model_id: "qwen3-tts".into(),
        family: "qwen3_tts".into(),
        path: "audio-cpp/audio.cpp-gguf/Qwen3-TTS-12Hz-0.6B-Base-GGUF".into(),
        task: "tts".into(),
        mode: "offline".into(),
        lazy: None,
        busy_timeout_ms: None,
        backend: None,
        threads: None,
        load_options: serde_json::Map::from_iter([(
            "language".into(),
            Value::String("english".into()),
        )]),
        session_options: Default::default(),
        default_request_options: Default::default(),
        model_spec_override: None,
        config_id: None,
        weight_id: None,
        voice_presets: serde_json::Map::from_iter([(
            "narrator".into(),
            json!({"voice_ref": "/models/voices/narrator.wav"}),
        )]),
        default_voice_preset: Some(Value::String("narrator".into())),
        enabled: true,
        // NULL = inherit the class settings (per-model-containers design §6);
        // this row starts out unset, like every row a migration adds the
        // column to.
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    };
    let id = store::insert_audio_model(&db, &new).await.unwrap();
    let m = store::get_audio_model(&db, id).await.unwrap().unwrap();
    assert_eq!(m.model_id, "qwen3-tts");
    assert_eq!(m.load_options["language"], "english");
    assert!(m.session_options.is_empty());
    assert_eq!(
        m.voice_presets["narrator"]["voice_ref"],
        "/models/voices/narrator.wav"
    );
    assert_eq!(
        m.default_voice_preset,
        Some(Value::String("narrator".into()))
    );
    assert!(m.enabled);
    assert_eq!(m.image, None);
    assert_eq!(m.extra_run_args, None);
    assert!(!m.warm_start);

    let updated = NewAudioModel {
        mode: "streaming".into(),
        enabled: false,
        // Clearing the default must read back as unset, not as a JSON null.
        default_voice_preset: None,
        image: Some("my/own-audio-image".into()),
        extra_run_args: Some(vec!["--device".into(), "cpu".into()]),
        warm_start: true,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        ..new
    };
    store::update_audio_model(&db, id, &updated).await.unwrap();
    let m = store::get_audio_model(&db, id).await.unwrap().unwrap();
    assert_eq!(m.mode, "streaming");
    assert!(m.default_voice_preset.is_none());
    assert!(!m.enabled);
    assert_eq!(m.image.as_deref(), Some("my/own-audio-image"));
    assert_eq!(
        m.extra_run_args,
        Some(vec!["--device".to_string(), "cpu".to_string()])
    );
    assert!(m.warm_start);

    store::delete_audio_model(&db, id).await.unwrap();
    assert!(store::get_audio_model(&db, id).await.unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Managed upstream provisioning + passthrough routing
// ---------------------------------------------------------------------------

/// The audio settings + model ops persist through the `/api` plane and refuse
/// a task audio.cpp does not have, with a surfaced 400 rather than a 500.
/// (Provisioning the managed upstream is `web::audio`'s own unit test — it
/// happens on container apply, which needs Podman.)
#[tokio::test]
async fn audio_settings_and_model_ops_validate_and_persist() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    let client = base.client();

    let op = |name: &'static str, args: Value| {
        let client = client.clone();
        let url = format!("{base}/api/op/{name}");
        async move {
            let resp = client.post(url).json(&args).send().await.unwrap();
            let status = resp.status().as_u16();
            let body: Value = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
            (status, body)
        }
    };

    let (status, body) = op(
        "settings_set_full",
        json!({"audio": {
            "image": "ghcr.io/0xshug0/audio.cpp:full-cuda12",
            "models_dir": "/tmp/audio",
            "backend": "cuda",
            "device": 0,
            "threads": 1,
            "public_prefix": "audio",
        }}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let s = state.snapshot().settings.audio.clone();
    assert_eq!(s.models_dir, "/tmp/audio");
    assert_eq!(s.public_prefix, "audio");
    assert_eq!(s.backend, "cuda");

    // The router-mode keys are a contract change, not a compat shim (§6): a
    // caller still sending them is told, rather than having them silently
    // ignored while it believes it configured a port.
    let (status, body) = op("settings_set_full", json!({"audio": {"listen_port": 9294}})).await;
    assert_eq!(status, 400, "{body}");

    let (status, body) = op(
        "audio_model_set",
        json!({
            "action": "create",
            "model_id": "qwen3-asr",
            "family": "qwen3_asr",
            "path": "audio-cpp/audio.cpp-gguf/Qwen3-ASR-0.6B-GGUF",
            "task": "asr",
            "mode": "offline",
            "enabled": true,
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // The Models page reads it back under the exposed (prefixed) name.
    let listed: Value = serde_json::from_str(
        &base
            .client()
            .get(format!("{base}/api/models/full"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    let row = listed["audio"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["model"]["model_id"] == json!("qwen3-asr"))
        .unwrap_or_else(|| panic!("audio model missing: {listed}"));
    assert_eq!(row["public_name"], json!("audio/qwen3-asr"));

    // An invalid task is refused with a surfaced error, not a 500.
    let (status, body) = op(
        "audio_model_set",
        json!({"action": "create", "model_id": "broken", "family": "f", "path": "p", "task": "bogus"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown task"),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// /v1/audio/* passthrough proxy
// ---------------------------------------------------------------------------

async fn setup_audio_proxy(upstream_base: &str) -> (lmgw_core::state::SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &store::NewUpstream {
            name: "audiocpp".into(),
            protocol: lmgw_core::config::Protocol::Openai,
            kind: lmgw_core::config::UpstreamKind::AudioCpp,
            base_url: format!("{}/v1", upstream_base.trim_end_matches('/')),
            api_key: Some("sk-up".into()),
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
        &store::NewAlias {
            alias: "my-tts".into(),
            upstream_id: up_id,
            upstream_model_id: "pocket-tts".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &store::NewAlias {
            alias: "my-asr".into(),
            upstream_id: up_id,
            upstream_model_id: "qwen3-asr".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve(state.clone()).await;
    (state, base)
}

/// TTS: JSON in with the alias, audio bytes out — the upstream sees the
/// concrete model id and the response streams back with its content type.
#[tokio::test]
async fn audio_speech_passthrough_rewrites_model_and_streams_audio() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .and(wiremock::matchers::body_partial_json(json!({
            "model": "pocket-tts"
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/wav")
                .set_body_bytes(b"RIFFfake-wav-bytes".to_vec()),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let (state, base) = setup_audio_proxy(&mock.uri()).await;
    let client = base.client();
    let resp = client
        .post(format!("{base}/v1/audio/speech"))
        .json(&json!({"model": "my-tts", "input": "hello world"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("audio/wav")
    );
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(&bytes[..], b"RIFFfake-wav-bytes");

    // A request_logs row ties the alias to the upstream + concrete model.
    let logs = store::query_logs(&state.db, &store::LogFilter::default())
        .await
        .unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].requested_alias, "my-tts");
    assert_eq!(logs[0].upstream_model.as_deref(), Some("pocket-tts"));
    assert_eq!(logs[0].upstream_name.as_deref(), Some("audiocpp"));
    assert_eq!(logs[0].status, 200);
}

/// wiremock matcher for multipart bodies (boundary makes exact matching
/// impossible): assert the re-encoded form carries the rewritten model id.
struct BodyContains(&'static str);
impl wiremock::Match for BodyContains {
    fn matches(&self, request: &wiremock::Request) -> bool {
        String::from_utf8_lossy(&request.body).contains(self.0)
    }
}

/// Multipart transcription: the upload is re-encoded with `model` rewritten
/// and the JSON transcript passed back.
#[tokio::test]
async fn audio_transcription_multipart_passthrough_rewrites_model() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .and(BodyContains("name=\"model\""))
        .and(BodyContains("qwen3-asr"))
        .and(BodyContains("name=\"file\""))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({"text": "transcribed hello"})),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let (_state, base) = setup_audio_proxy(&mock.uri()).await;
    let client = base.client();
    let form = reqwest::multipart::Form::new()
        .text("model", "my-asr")
        .text("language", "en")
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"RIFFaudio".to_vec()).file_name("in.wav"),
        );
    let resp = client
        .post(format!("{base}/v1/audio/transcriptions"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["text"], "transcribed hello");
}

/// The details route is the same upload sent one path further along, and the
/// arrays the plain route drops — words, segments, speaker turns — come back
/// untouched. Nothing here parses the transcript: the relay has to be
/// lossless or the route is pointless.
#[tokio::test]
async fn transcription_details_passthrough_relays_the_detail_arrays() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions/details"))
        .and(BodyContains("qwen3-asr"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "text": "the task has completed successfully",
                    "language": "en",
                    "sample_rate": 16000,
                    "words": [
                        {"word": "the", "start_sample": 3200, "end_sample": 6400,
                         "confidence": 0.98}
                    ],
                    "speaker_turns": [
                        {"start_sample": 0, "end_sample": 6400, "speaker_id": "spk_0"}
                    ],
                    "timing": {"wall_ms": 412.7, "rtf": 0.17}
                })),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let (_state, base) = setup_audio_proxy(&mock.uri()).await;
    let form = reqwest::multipart::Form::new()
        .text("model", "my-asr")
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"RIFFaudio".to_vec()).file_name("in.wav"),
        );
    let resp = base
        .client()
        .post(format!("{base}/v1/audio/transcriptions/details"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["words"][0]["word"], "the");
    assert_eq!(v["speaker_turns"][0]["speaker_id"], "spk_0");
    assert_eq!(v["sample_rate"], 16000);
}

/// The JSON variant of the details route reaches the same path — a caller
/// with a server-local clip should not have to upload it again to get word
/// timings.
#[tokio::test]
async fn transcription_details_also_takes_the_json_variant() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions/details"))
        .and(wiremock::matchers::body_partial_json(
            json!({"model": "qwen3-asr", "audio": "/models/clip.wav"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({"text": "hi", "timing": {"wall_ms": 1.0}})),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let (_state, base) = setup_audio_proxy(&mock.uri()).await;
    let resp = base
        .client()
        .post(format!("{base}/v1/audio/transcriptions/details"))
        .json(&json!({"model": "my-asr", "audio": "/models/clip.wav"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

/// Forced alignment: the upload carries the transcript beside the clip, and
/// `model` is rewritten to the concrete id like every other audio route.
#[tokio::test]
async fn alignment_multipart_passthrough_rewrites_model() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/alignments"))
        .and(BodyContains("qwen3-asr"))
        .and(BodyContains("name=\"text\""))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "words": [{"word": "the", "start": 0.2, "end": 0.4}],
                    "sample_rate": 16000
                })),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let (_state, base) = setup_audio_proxy(&mock.uri()).await;
    let form = reqwest::multipart::Form::new()
        .text("model", "my-asr")
        .text("text", "The task has completed successfully.")
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"RIFFaudio".to_vec()).file_name("in.wav"),
        );
    let resp = base
        .client()
        .post(format!("{base}/v1/audio/alignments"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["words"][0]["word"], "the");
}

/// audio.cpp answers a JSON alignment request with a 400; lmgw says so
/// itself, so a local row's container is not started to be told the same
/// thing — and the message names the fields the route does take.
#[tokio::test]
async fn alignment_refuses_a_json_body_before_reaching_the_upstream() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/alignments"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&mock)
        .await;

    let (_state, base) = setup_audio_proxy(&mock.uri()).await;
    let resp = base
        .client()
        .post(format!("{base}/v1/audio/alignments"))
        .json(&json!({"model": "my-asr", "audio": "/models/clip.wav", "text": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let v: Value = resp.json().await.unwrap();
    let msg = v["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("multipart/form-data"), "{msg}");
}

/// An image older than the route answers `404 unknown endpoint`, and relayed
/// verbatim that reads as a gateway bug. Measured against the image on this
/// box (built 2026-08-02): both new routes 404 there, and the container is
/// pinned by tag, so this is the likely first encounter with them.
#[tokio::test]
async fn an_image_older_than_the_route_is_told_to_be_pulled() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/alignments"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": {
                "message": "unknown endpoint: /v1/audio/alignments",
                "type": "not_found",
            }
        })))
        .mount(&mock)
        .await;

    let (_state, base) = setup_audio_proxy(&mock.uri()).await;
    let form = reqwest::multipart::Form::new()
        .text("model", "my-asr")
        .text("text", "hello")
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"RIFF".to_vec()).file_name("in.wav"),
        );
    let resp = base
        .client()
        .post(format!("{base}/v1/audio/alignments"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    let v: Value = resp.json().await.unwrap();
    let msg = v["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("image"), "{msg}");
    assert!(
        msg.contains("2026-09-02"),
        "it says when the route landed: {msg}"
    );
    assert!(
        msg.contains("unknown endpoint"),
        "the upstream's own words survive: {msg}"
    );
}

/// Unknown audio alias → OpenAI-shaped 404 from the gateway, no upstream hit.
#[tokio::test]
async fn audio_speech_unknown_alias_is_a_gateway_error() {
    let mock = MockServer::start().await;
    let (_state, base) = setup_audio_proxy(&mock.uri()).await;
    let client = base.client();
    let resp = client
        .post(format!("{base}/v1/audio/speech"))
        .json(&json!({"model": "nope", "input": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let v: Value = resp.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().contains("nope"));
    assert_eq!(mock.received_requests().await.unwrap().len(), 0);
}

/// An alias onto a non-openai upstream is rejected before any request goes
/// out: `/v1/audio/*` is an OpenAI-shaped passthrough, and an anthropic
/// upstream has neither the path nor the auth header — it would otherwise get
/// `https://api.anthropic.com/audio/speech` with a bearer token.
#[tokio::test]
async fn audio_speech_rejects_non_openai_upstream() {
    let mock = MockServer::start().await;
    let (state, base) = setup_audio_proxy(&mock.uri()).await;
    let up_id = store::insert_upstream(
        &state.db,
        &store::NewUpstream {
            name: "claude".into(),
            protocol: lmgw_core::config::Protocol::Anthropic,
            kind: lmgw_core::config::UpstreamKind::Generic,
            base_url: mock.uri(),
            api_key: Some("sk-ant".into()),
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
        &store::NewAlias {
            alias: "not-audio".into(),
            upstream_id: up_id,
            upstream_model_id: "claude-sonnet-5".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let resp = base
        .client()
        .post(format!("{base}/v1/audio/speech"))
        .json(&json!({"model": "not-audio", "input": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let v: Value = resp.json().await.unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("openai-protocol"), "unexpected message: {msg}");
    assert_eq!(mock.received_requests().await.unwrap().len(), 0);

    let logs = store::query_logs(&state.db, &store::LogFilter::default())
        .await
        .unwrap();
    assert_eq!(logs[0].error_kind.as_deref(), Some("unsupported"));
}

/// An upstream failure is normalized into our own error shape and — the point
/// of buffering it — the provider's message reaches `request_logs` instead of
/// a bare "HTTP 500".
#[tokio::test]
async fn audio_speech_upstream_error_is_normalized_and_logged() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": {
                "message": "PocketTTS session prepare() requires a session voice",
                "type": "server_error",
            }
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let (state, base) = setup_audio_proxy(&mock.uri()).await;
    let resp = base
        .client()
        .post(format!("{base}/v1/audio/speech"))
        .json(&json!({"model": "my-tts", "input": "hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
    let v: Value = resp.json().await.unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("session voice"), "detail lost: {msg}");
    assert_eq!(v["error"]["code"], "upstream");

    let logs = store::query_logs(&state.db, &store::LogFilter::default())
        .await
        .unwrap();
    assert_eq!(logs[0].error_kind.as_deref(), Some("upstream"));
    assert!(
        logs[0]
            .error_msg
            .as_deref()
            .unwrap_or_default()
            .contains("session voice"),
        "log row lost the upstream message: {:?}",
        logs[0].error_msg
    );
}

/// The response carries the upstream's `content-length` through, so clients
/// get a real size for a WAV instead of a chunked body of unknown length.
#[tokio::test]
async fn audio_speech_forwards_content_length() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/wav")
                .set_body_bytes(b"RIFFfake-wav-bytes".to_vec()),
        )
        .mount(&mock)
        .await;

    let (_state, base) = setup_audio_proxy(&mock.uri()).await;
    let resp = base
        .client()
        .post(format!("{base}/v1/audio/speech"))
        .json(&json!({"model": "my-tts", "input": "hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok()),
        Some("18")
    );
    assert_eq!(&resp.bytes().await.unwrap()[..], b"RIFFfake-wav-bytes");
}

// ---------------------------------------------------------------------------
// Audio lab (web/audio_lab.rs)
// ---------------------------------------------------------------------------

/// The lab's dispatch route is a wrapper over the *public* `/v1/audio/speech`
/// handler, not a second implementation: the upstream must see the rewritten
/// model id and the call must land in `request_logs` exactly like an API client's.
#[tokio::test]
async fn audio_lab_speech_goes_through_the_public_audio_handler() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .and(wiremock::matchers::body_partial_json(json!({
            "model": "pocket-tts",
            "voice_ref": "/models/voices/ref.wav"
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/wav")
                .set_body_bytes(b"RIFFlab".to_vec()),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let (state, base) = setup_audio_proxy(&mock.uri()).await;
    let resp = base
        .client()
        .post(format!("{base}/audio-lab/api/speech"))
        .json(&json!({
            "model": "my-tts",
            "input": "hello from the lab",
            "voice_ref": "/models/voices/ref.wav"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(&resp.bytes().await.unwrap()[..], b"RIFFlab");

    let logs = store::query_logs(&state.db, &store::LogFilter::default())
        .await
        .unwrap();
    assert_eq!(logs.len(), 1, "lab call must be logged like any other");
    assert_eq!(logs[0].requested_alias, "my-tts");
    assert_eq!(logs[0].upstream_model.as_deref(), Some("pocket-tts"));
}

/// The same for transcriptions, over the multipart shape a Whisper client uses.
#[tokio::test]
async fn audio_lab_transcription_reencodes_multipart_with_the_upstream_model() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .and(BodyContains("qwen3-asr"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "hello"})))
        .expect(1)
        .mount(&mock)
        .await;

    let (_state, base) = setup_audio_proxy(&mock.uri()).await;
    let form = reqwest::multipart::Form::new()
        .text("model", "my-asr")
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"RIFFaudio".to_vec()).file_name("clip.wav"),
        );
    let resp = base
        .client()
        .post(format!("{base}/audio-lab/api/transcriptions"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["text"].as_str(), Some("hello"));
}

/// Boot a gateway whose audio models dir points at `dir` — the voice library
/// writes there, and the container would see it mounted at `/models`.
async fn setup_audio_lab(dir: &Path) -> (lmgw_core::state::SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.audio.models_dir = dir.display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve(state.clone()).await;
    (state, base)
}

/// The voice library round-trips through the audio models dir, and the path it
/// reports is the container's view (`/models/voices/...`) — the value that has
/// to go into `voice_ref`, since audio.cpp resolves it server-side.
#[tokio::test]
async fn audio_lab_voice_library_round_trips_into_the_models_dir() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, base) = setup_audio_lab(dir.path()).await;
    let client = base.client();

    let form = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(b"RIFFvoice".to_vec()).file_name("my voice.wav"),
    );
    let v: Value = client
        .post(format!("{base}/audio-lab/api/refs"))
        .multipart(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["clips"][0]["name"].as_str(), Some("my voice.wav"));
    assert_eq!(
        v["clips"][0]["server_path"].as_str(),
        Some("/models/voices/my voice.wav")
    );
    assert!(dir.path().join("voices").join("my voice.wav").exists());

    // Served back for in-page preview, then deleted off disk.
    let resp = client
        .get(format!("{base}/audio-lab/api/refs/my%20voice.wav"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(&resp.bytes().await.unwrap()[..], b"RIFFvoice");

    client
        .post(format!("{base}/audio-lab/api/refs/my%20voice.wav/delete"))
        .send()
        .await
        .unwrap();
    assert!(!dir.path().join("voices").join("my voice.wav").exists());
}

/// The library is audio.cpp's `voice_dir`, so its transcript index has to be
/// the file audiocpp_server reads: `prompt_text`, one `<voice>|<text>` line
/// per clip. With the class pointing `voice_dir` here, that line is what a
/// request `"voice": "narrator"` clones *with* its reference text instead of
/// against an unconditioned one.
#[tokio::test]
async fn audio_lab_voice_transcripts_are_the_prompt_text_file_audio_cpp_reads() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, base) = setup_audio_lab(dir.path()).await;
    let client = base.client();
    let prompt_text = dir.path().join("voices").join("prompt_text");

    // A transcript may ride along with the upload …
    let form = reqwest::multipart::Form::new()
        .text(
            "transcript",
            "okay, I'm Cemo and this is not a human voice.",
        )
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"RIFFa".to_vec()).file_name("narrator.wav"),
        );
    let v: Value = client
        .post(format!("{base}/audio-lab/api/refs"))
        .multipart(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["clips"][0]["voice"].as_str(), Some("narrator"));
    assert!(v["clips"][0]["transcript"]
        .as_str()
        .unwrap_or_default()
        .starts_with("okay, I'm Cemo"));
    let written = std::fs::read_to_string(&prompt_text).unwrap();
    assert_eq!(
        written,
        "narrator|okay, I'm Cemo and this is not a human voice.\n"
    );

    // … or be set afterwards. A newline would split one clip's line in two,
    // so it is folded to a space.
    let v: Value = client
        .post(format!("{base}/audio-lab/api/refs/narrator.wav/text"))
        .json(&json!({"transcript": "line one\nline two"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["clips"][0]["transcript"], "line one line two");
    assert_eq!(
        std::fs::read_to_string(&prompt_text).unwrap(),
        "narrator|line one line two\n"
    );

    // The index is not a clip: it lives in the same directory and must not
    // show up as one.
    assert_eq!(v["clips"].as_array().unwrap().len(), 1);

    // Clearing drops the line; with nothing left the file goes too, because
    // "no transcripts" is an absent index to audiocpp_server.
    client
        .post(format!("{base}/audio-lab/api/refs/narrator.wav/text"))
        .json(&json!({"transcript": "  "}))
        .send()
        .await
        .unwrap();
    assert!(!prompt_text.exists());

    // And a deleted clip takes its transcript with it, or the next upload
    // under the same name would inherit a stranger's line.
    client
        .post(format!("{base}/audio-lab/api/refs/narrator.wav/text"))
        .json(&json!({"transcript": "still here"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base}/audio-lab/api/refs/narrator.wav/delete"))
        .send()
        .await
        .unwrap();
    assert!(!prompt_text.exists());
}

/// A clip name that could climb out of the voices dir is refused rather than
/// sanitized — the name ends up inside a container-side path.
#[tokio::test]
async fn audio_lab_rejects_traversing_clip_names() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, base) = setup_audio_lab(dir.path()).await;

    let form = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(b"x".to_vec()).file_name("sub/../../escape.wav"),
    );
    let resp = base
        .client()
        .post(format!("{base}/audio-lab/api/refs"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    assert!(!dir.path().parent().unwrap().join("escape.wav").exists());
}

// ---------------------------------------------------------------------------
// Generic task routes (§6): /v1/tasks/run, /v1/tasks/stream, /v1/audio/voices
// ---------------------------------------------------------------------------

/// `/v1/tasks/run` carries the model at the top level and the task payload in a
/// nested `request`: the alias must be rewritten, the payload relayed verbatim
/// (the gateway has no business modelling CLI request fields), and the call
/// logged like any other.
#[tokio::test]
async fn task_run_rewrites_model_and_relays_the_request_untouched() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/tasks/run"))
        .and(wiremock::matchers::body_partial_json(json!({
            "model": "pocket-tts",
            "request": { "audio": "/models/voices/in.wav", "return_timestamps": true }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "speaker_turns": [
                {"start_sample": 0, "end_sample": 16000, "speaker_id": "spk0", "confidence": 0.9}
            ],
            "timing": {"wall_ms": 42.0}
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let (state, base) = setup_audio_proxy(&mock.uri()).await;
    let resp = base
        .client()
        .post(format!("{base}/v1/tasks/run"))
        .json(&json!({
            "model": "my-tts",
            "request": { "audio": "/models/voices/in.wav", "return_timestamps": true }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["speaker_turns"][0]["speaker_id"].as_str(), Some("spk0"));

    let logs = store::query_logs(&state.db, &store::LogFilter::default())
        .await
        .unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].requested_alias, "my-tts");
    assert_eq!(logs[0].upstream_model.as_deref(), Some("pocket-tts"));
    assert_eq!(logs[0].status, 200);
}

/// `/v1/tasks/stream` is the same passthrough against a different upstream path
/// — audio.cpp buffers the events and answers one `{events, result}` document,
/// so nothing here is SSE.
#[tokio::test]
async fn task_stream_targets_the_stream_endpoint() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/tasks/stream"))
        .and(wiremock::matchers::body_partial_json(
            json!({"model": "pocket-tts"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "events": [{"partial_text": {"text": "a"}}],
            "result": {"text": "ab", "timing": {"wall_ms": 5.0}}
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let (_state, base) = setup_audio_proxy(&mock.uri()).await;
    let v: Value = base
        .client()
        .post(format!("{base}/v1/tasks/stream"))
        .json(&json!({"model": "my-tts", "request": {"text": "hi"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["result"]["text"].as_str(), Some("ab"));
    assert_eq!(v["events"].as_array().map(Vec::len), Some(1));
}

/// A task run against a model the upstream rejects keeps the provider's message
/// and the gateway's error shape, and is logged as a failure.
#[tokio::test]
async fn task_run_upstream_error_is_normalized_and_logged() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/tasks/run"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": {"message": "configured model does not provide streaming execution"}
        })))
        .mount(&mock)
        .await;

    let (state, base) = setup_audio_proxy(&mock.uri()).await;
    let resp = base
        .client()
        .post(format!("{base}/v1/tasks/run"))
        .json(&json!({"model": "my-tts", "request": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
    let v: Value = resp.json().await.unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("does not provide streaming execution"),
        "provider message lost: {v}"
    );

    let logs = store::query_logs(&state.db, &store::LogFilter::default())
        .await
        .unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].status, 502);
}

/// A task run without `model` fails at the gateway, before any upstream call.
#[tokio::test]
async fn task_run_without_model_is_a_bad_request() {
    let mock = MockServer::start().await;
    let (_state, base) = setup_audio_proxy(&mock.uri()).await;
    let resp = base
        .client()
        .post(format!("{base}/v1/tasks/run"))
        .json(&json!({"request": {"text": "hi"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    // wiremock has no mounted route: reaching it at all would have 404'd here.
}

/// The voice list resolves the alias and asks the upstream about the *concrete*
/// model id, so a client can populate a picker using gateway names throughout.
#[tokio::test]
async fn audio_voices_rewrites_the_model_query_param() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/audio/voices"))
        .and(wiremock::matchers::query_param("model", "pocket-tts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "voices": ["alba", "cosette"]
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let (state, base) = setup_audio_proxy(&mock.uri()).await;
    let v: Value = base
        .client()
        .get(format!("{base}/v1/audio/voices?model=my-tts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["voices"][0].as_str(), Some("alba"));

    // Metadata, not inference: no request_logs row, same as `GET /v1/models`.
    let logs = store::query_logs(&state.db, &store::LogFilter::default())
        .await
        .unwrap();
    assert!(
        logs.is_empty(),
        "voice lookup should not be logged: {logs:?}"
    );
}

/// `?model=` is required — without it the upstream has nothing to look up and
/// audio.cpp would silently answer for its single configured model.
#[tokio::test]
async fn audio_voices_requires_a_model() {
    let mock = MockServer::start().await;
    let (_state, base) = setup_audio_proxy(&mock.uri()).await;
    let resp = base
        .client()
        .get(format!("{base}/v1/audio/voices"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

/// The lab's wrapper picks the endpoint from `?stream=`, so the panel's
/// checkbox maps onto the two real routes rather than a third code path.
#[tokio::test]
async fn audio_lab_task_wrapper_selects_run_or_stream() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/tasks/run"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "run"})))
        .expect(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/tasks/stream"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"result": {"text": "stream"}})),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let (_state, base) = setup_audio_proxy(&mock.uri()).await;
    let client = base.client();
    let body = json!({"model": "my-tts", "request": {"text": "hi"}});

    let v: Value = client
        .post(format!("{base}/audio-lab/api/tasks/run"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["text"].as_str(), Some("run"));

    let v: Value = client
        .post(format!("{base}/audio-lab/api/tasks/run?stream=1"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["result"]["text"].as_str(), Some("stream"));
}

// ---------------------------------------------------------------------------
// Local audio models: admission, not a class-wide port (per-model containers §5)
// ---------------------------------------------------------------------------

/// A `podman` that agrees to everything and starts nothing — the "container"
/// is the wiremock the port allocator points at.
struct FakePodman;

#[async_trait::async_trait]
impl lmgw_core::runtime::registry::CommandRunner for FakePodman {
    async fn run(
        &self,
        _program: &str,
        _args: &[String],
    ) -> std::io::Result<lmgw_core::runtime::registry::CmdOutput> {
        Ok(lmgw_core::runtime::registry::CmdOutput {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

/// `GET /v1/audio/voices?probe=engine` (the default read before audio-class
/// gap 6) was one of the paths that free-rode on the always-on audio.cpp
/// port: it resolved a route and read its `base_url` without ever asking
/// whether the model was loaded. §5 names it, and this is the fix — it admits
/// like every other local touch, so the answer comes from the container
/// admission just started, on the port that container came up on.
///
/// It also proves the resolution half: no `upstreams` row exists here, and
/// `audio/<id>` still routes, straight out of the `audio_models` table.
#[tokio::test]
async fn audio_voices_admits_the_local_model_and_reads_its_container() {
    let mock = MockServer::start().await;
    // audio.cpp has no `/health`; `GET /v1/models` is its readiness probe.
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/audio/voices"))
        .and(wiremock::matchers::query_param("model", "pocket-tts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"voices": ["alba"]})))
        .expect(1)
        .mount(&mock)
        .await;

    let state = AppState::init_for_tests().await.unwrap();
    let port = mock.address().port();
    state.set_runtime_for_tests(Arc::new(
        lmgw_core::runtime::registry::Registry::with_ports(
            Arc::new(FakePodman),
            reqwest::Client::new(),
            Arc::new(move || Ok(port)),
        ),
    ));
    let mut s = lmgw_core::config::Settings::default();
    s.vram.load_timeout_seconds = 2;
    // The start sequence refuses a class with no models dir before it renders
    // any argv; the fake podman mounts nothing, so its contents are irrelevant.
    s.audio.models_dir = std::env::temp_dir().display().to_string();
    store::save_settings(&state.db, &s).await.unwrap();
    store::insert_audio_model(
        &state.db,
        &NewAudioModel {
            model_id: "pocket-tts".into(),
            family: "pocket_tts".into(),
            path: "PocketTTS/english".into(),
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
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve(state.clone()).await;

    // `probe=engine`: since audio-class gap 6 the default answer for a local
    // row comes from lmgw's own catalog and starts nothing; asking the model
    // itself is the explicit probe.
    let resp = base
        .client()
        .get(format!(
            "{base}/v1/audio/voices?model=audio/pocket-tts&probe=engine"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["x-lmgw-voices-source"], "engine");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["voices"][0].as_str(), Some("alba"));

    // The container really was started for it — the read did not go to a
    // class-wide port that no longer exists.
    let up = state
        .runtime()
        .list()
        .iter()
        .any(|e| e.model_id == "pocket-tts" && e.port == port);
    assert!(up, "admission should have started the model's container");
}

/// A `podman` that records what it was asked to do and starts nothing.
#[derive(Default)]
struct RecordingPodman {
    calls: std::sync::Mutex<Vec<Vec<String>>>,
}

impl RecordingPodman {
    fn runs(&self) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.first().map(String::as_str) == Some("run"))
            .count()
    }
}

#[async_trait::async_trait]
impl lmgw_core::runtime::registry::CommandRunner for RecordingPodman {
    async fn run(
        &self,
        _program: &str,
        args: &[String],
    ) -> std::io::Result<lmgw_core::runtime::registry::CmdOutput> {
        self.calls.lock().unwrap().push(args.to_vec());
        Ok(lmgw_core::runtime::registry::CmdOutput {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

/// One local audio model (`audio/pocket-tts`) behind a registry that runs
/// nothing and always hands out `port`, so the container "comes up" on
/// whatever answers there. The lab's voice route, from its URL suffix.
async fn lab_with_one_audio_model(
    port: u16,
) -> (
    lmgw_core::state::SharedState,
    Arc<RecordingPodman>,
    impl Fn(&'static str) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value>>>,
) {
    let state = AppState::init_for_tests().await.unwrap();
    let podman = Arc::new(RecordingPodman::default());
    state.set_runtime_for_tests(Arc::new(
        lmgw_core::runtime::registry::Registry::with_ports(
            podman.clone(),
            reqwest::Client::new(),
            Arc::new(move || Ok(port)),
        ),
    ));
    let mut s = lmgw_core::config::Settings::default();
    s.vram.load_timeout_seconds = 2;
    s.audio.models_dir = std::env::temp_dir().display().to_string();
    store::save_settings(&state.db, &s).await.unwrap();
    store::insert_audio_model(
        &state.db,
        &NewAudioModel {
            model_id: "pocket-tts".into(),
            family: "pocket_tts".into(),
            path: "PocketTTS/english".into(),
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
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;
    let voices = move |q: &'static str| {
        let client = base.client();
        let url = format!("{base}/audio-lab/api/voices?model=audio/pocket-tts{q}");
        Box::pin(async move {
            let resp = client.get(url).send().await.unwrap();
            assert_eq!(resp.status().as_u16(), 200);
            resp.json::<Value>().await.unwrap()
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = Value>>>
    };
    (state, podman, voices)
}

/// A pocket-tts container stand-in: answers audio.cpp's readiness probe
/// (`GET /v1/models`; it has no `/health`) and its voice list.
async fn mount_pocket_tts(mock: &MockServer, voice_reads: u64) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/audio/voices"))
        .and(wiremock::matchers::query_param("model", "pocket-tts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"voices": ["alba"]})))
        .expect(voice_reads)
        .mount(mock)
        .await;
}

/// Opening the Audio lab asks for the selected model's voices, and that list
/// comes out of the model's own container. Through admission, the page load
/// itself put a model on the GPU. The dashboard route now reads only a
/// container that is already up: a stopped model answers an empty list with
/// `running: false` and no `podman run` is issued, until the owner asks for it
/// with `start=1`. Once the model is up, the plain read reaches it.
#[tokio::test]
async fn the_lab_voice_list_does_not_start_a_stopped_model() {
    let mock = MockServer::start().await;
    mount_pocket_tts(&mock, 2).await;
    let (state, podman, voices) = lab_with_one_audio_model(mock.address().port()).await;

    // Browsing: nothing is started, and the answer says why the list is empty.
    let v = voices("").await;
    assert_eq!(v, json!({"voices": [], "running": false}));
    assert_eq!(podman.runs(), 0, "a browse must not run a container");
    assert!(state.runtime().list().is_empty());

    // "Load voices": the explicit start admits the model like any request.
    let v = voices("&start=1").await;
    assert_eq!(v["voices"][0].as_str(), Some("alba"));
    assert_eq!(podman.runs(), 1);
    assert!(state
        .runtime()
        .list()
        .iter()
        .any(|e| e.model_id == "pocket-tts" && e.port == mock.address().port()));

    // Up now: the plain read reaches the container and starts nothing more.
    let v = voices("").await;
    assert_eq!(v["voices"][0].as_str(), Some("alba"));
    assert_eq!(podman.runs(), 1);
}

/// The container died outside lmgw (an OOM kill, audio.cpp crashing, a
/// `podman stop` from a shell), so the registry still holds it as `ready` on a
/// port where nothing listens. Through admission, the refused connection ran
/// the dead-container recovery — stop it, admit it afresh, `podman run` — and
/// opening the lab put the model back on the GPU. The browse read now takes no
/// claim and recovers nothing: it answers "not running" and runs no podman
/// command at all.
#[tokio::test]
async fn the_lab_voice_list_does_not_restart_a_dead_ready_model() {
    // Not pooled: dropping a bare server closes its port, which is the dead
    // container this test needs (a pooled one keeps listening for the next test).
    let mock = MockServer::builder().start().await;
    mount_pocket_tts(&mock, 1).await;
    let port = mock.address().port();
    let (state, podman, voices) = lab_with_one_audio_model(port).await;

    let v = voices("&start=1").await;
    assert_eq!(v["voices"][0].as_str(), Some("alba"));
    assert_eq!(podman.runs(), 1);

    drop(mock);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "the mock never closed its port"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let calls_before = podman.calls.lock().unwrap().len();
    let ready = |state: &lmgw_core::state::SharedState| {
        state.runtime().list().into_iter().any(|e| {
            e.model_id == "pocket-tts"
                && e.state == lmgw_core::runtime::registry::RuntimeState::Ready
                && e.in_flight == 0
        })
    };
    assert!(ready(&state), "the registry should still believe it is up");

    let v = voices("").await;
    assert_eq!(v, json!({"voices": [], "running": false}));
    assert_eq!(
        podman.runs(),
        1,
        "a browse must not restart a dead container"
    );
    assert_eq!(
        podman.calls.lock().unwrap().len(),
        calls_before,
        "no stop, no rm, no run: the browse read touches no container"
    );
    assert!(ready(&state), "and it leaves the entry as it found it");
}

/// A chat model resolves on the voice routes too — its llama-server speaks
/// the OpenAI protocol — and admitting it started a chat container for a
/// question it answers with a 404. Both voice routes refuse it before
/// admission, the lab's "Load voices" (`start=1`) included.
#[tokio::test]
async fn a_chat_model_asked_for_voices_is_refused_and_starts_nothing() {
    let state = AppState::init_for_tests().await.unwrap();
    let podman = Arc::new(RecordingPodman::default());
    state.set_runtime_for_tests(Arc::new(
        lmgw_core::runtime::registry::Registry::with_ports(
            podman.clone(),
            reqwest::Client::new(),
            Arc::new(|| Ok(9)),
        ),
    ));
    store::insert_local_model(
        &state.db,
        &store::NewLocalModel {
            model_id: "chatty".into(),
            gguf_path: "chatty.gguf".into(),
            params: Default::default(),
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
            ladder: vec![],
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;
    for url in [
        format!("{base}/v1/audio/voices?model=chatty"),
        format!("{base}/audio-lab/api/voices?model=chatty&start=1"),
        format!("{base}/audio-lab/api/voices?model=chatty"),
    ] {
        let resp = base.client().get(&url).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 400, "{url}");
        let v: Value = resp.json().await.unwrap();
        assert!(
            v.to_string().contains("lists the voices of an audio model"),
            "{url}: {v}"
        );
    }
    assert_eq!(
        podman.runs(),
        0,
        "no container for a question it cannot answer"
    );
    assert!(state.runtime().list().is_empty());
}

/// A remote audio upstream has no container to start, so the lab's voice list
/// is read from it directly — the guard only holds back local models.
#[tokio::test]
async fn the_lab_voice_list_reads_a_remote_upstream_directly() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/audio/voices"))
        .and(wiremock::matchers::query_param("model", "pocket-tts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"voices": ["alba"]})))
        .expect(1)
        .mount(&mock)
        .await;
    let (_state, base) = setup_audio_proxy(&mock.uri()).await;
    let v: Value = base
        .client()
        .get(format!("{base}/audio-lab/api/voices?model=my-tts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["voices"][0].as_str(), Some("alba"));
}

/// The reverse-route gate, the image class's twin: an `audio/<id>` sent to a
/// **text** route is refused before admission, so audio.cpp is never started
/// for a request it has no handler for. The registry is empty afterwards,
/// which is the whole point — resolution alone used to be enough to start it.
#[tokio::test]
async fn a_chat_completion_against_an_audio_model_is_refused_before_admission() {
    let mock = MockServer::start().await;
    // audio.cpp has no `/health`; `GET /v1/models` is its readiness probe. It
    // is mounted so that a start which *did* happen would look successful —
    // the assertions below would then fail on the container, not on a timeout.
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&mock)
        .await;

    let state = AppState::init_for_tests().await.unwrap();
    let port = mock.address().port();
    state.set_runtime_for_tests(Arc::new(
        lmgw_core::runtime::registry::Registry::with_ports(
            Arc::new(FakePodman),
            reqwest::Client::new(),
            Arc::new(move || Ok(port)),
        ),
    ));
    let mut s = lmgw_core::config::Settings::default();
    s.vram.load_timeout_seconds = 2;
    s.audio.models_dir = std::env::temp_dir().display().to_string();
    store::save_settings(&state.db, &s).await.unwrap();
    store::insert_audio_model(
        &state.db,
        &NewAudioModel {
            model_id: "pocket-tts".into(),
            family: "pocket_tts".into(),
            path: "PocketTTS/english".into(),
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
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve(state.clone()).await;

    for (route, body) in [
        (
            "/v1/chat/completions",
            json!({"model": "audio/pocket-tts",
                   "messages": [{"role": "user", "content": "hi"}]}),
        ),
        (
            "/v1/completions",
            json!({"model": "audio/pocket-tts", "prompt": "hi"}),
        ),
        (
            "/v1/embeddings",
            json!({"model": "audio/pocket-tts", "input": "hi"}),
        ),
        (
            "/v1/rerank",
            json!({"model": "audio/pocket-tts", "query": "hi", "documents": ["a"]}),
        ),
        (
            "/v1/count_tokens",
            json!({"model": "audio/pocket-tts", "input": "hi"}),
        ),
        (
            "/v1/responses",
            json!({"model": "audio/pocket-tts", "input": "hi"}),
        ),
    ] {
        let resp = base
            .client()
            .post(format!("{base}{route}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400, "{route}");
        let v: Value = resp.json().await.unwrap();
        let msg = v["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("is an audio model"), "{route}: {msg}");
        assert!(msg.contains("/v1/audio/speech"), "{route}: {msg}");
        assert!(msg.contains("/v1/tasks/run"), "{route}: {msg}");
    }

    assert_eq!(
        mock.received_requests().await.unwrap().len(),
        0,
        "no text route may contact the container"
    );
    assert!(
        state.runtime().list().is_empty(),
        "no text route may start an audio container"
    );
}
