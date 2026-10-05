//! The realtime settings surface (realtime design §12, WP8): the section's
//! save and read-back on both planes, the refusals a save makes, the
//! `/v1/realtime` entry chat models get on `/v1/models`, and the cascade's
//! VRAM budget.

use lmgw_api_types::realtime::{RealtimeBudget, RealtimeBudgetStage};
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewAudioModel, NewUpstream};
use serde_json::{json, Value};

use crate::common::{serve, Gw};
use crate::support::gpu_world::{Gpu, GIB};

/// A gateway with aliases on an audio.cpp-kind upstream that is never
/// called — `my-asr` (task asr), `my-tts` (task tts), `my-design` (task
/// vdes, a voice designed from a description) — and `my-chat`, a chat alias
/// on a generic upstream of its own.
async fn setup() -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let upstream = |name: &str, kind| NewUpstream {
        name: name.into(),
        protocol: Protocol::Openai,
        kind,
        base_url: "https://never-called.invalid/v1".into(),
        api_key: None,
        extra_headers: vec![],
        timeout_ms: 5_000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
    };
    let audio = store::insert_upstream(&state.db, &upstream("voice", UpstreamKind::AudioCpp))
        .await
        .unwrap();
    let cloud = store::insert_upstream(&state.db, &upstream("cloud", UpstreamKind::Generic))
        .await
        .unwrap();
    for (alias, up, task, endpoint) in [
        ("my-asr", audio, "asr", "/v1/audio/transcriptions"),
        ("my-tts", audio, "tts", "/v1/audio/speech"),
        ("my-design", audio, "vdes", "/v1/audio/speech"),
        ("my-chat", cloud, "chat", "/v1/chat/completions"),
    ] {
        store::insert_alias(
            &state.db,
            &NewAlias {
                alias: alias.into(),
                upstream_id: up,
                upstream_model_id: alias.into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: Some(json!({ "capabilities": {
                    "task": task, "endpoints": [endpoint], "source": "owner"
                } })),
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

/// `POST /api/op/<name>`: the status and the body.
async fn op(gw: &Gw, name: &str, args: Value) -> (u16, Value) {
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

async fn get(gw: &Gw, path: &str) -> Value {
    gw.client()
        .get(format!("{gw}{path}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn realtime(gw: &Gw) -> lmgw_api_types::realtime::RealtimeSettings {
    let full: lmgw_api_types::SettingsFull =
        serde_json::from_value(get(gw, "/api/settings-full").await).unwrap();
    full.realtime
}

#[tokio::test]
async fn the_realtime_section_round_trips_through_the_dashboard_save() {
    let (_state, gw) = setup().await;
    let before = realtime(&gw).await;
    assert_eq!(before.max_message_mb, 64);
    assert_eq!(before.semantic_vad_engine, "smart_turn");
    assert!(before.default_instructions_is_builtin);
    assert_eq!(
        before.default_instructions,
        before.default_instructions_builtin
    );

    let (status, body) = op(
        &gw,
        "settings_set_full",
        json!({"realtime": {
            "default_model": "my-chat",
            "model_map": {"gpt-realtime": "my-chat", "whisper-1": "my-asr"},
            "asr_alias": "my-asr",
            "tts_alias": "my-tts",
            "default_voice": "alba",
            "voice_map": {"alloy": "alba"},
            "default_instructions": "",
            "semantic_vad": {"low": {"floor": 0.5}},
            "barge_in_check": "duration",
            "barge_in_check_alias": "my-asr",
            "barge_in_check_scripts": ["Latin", "Cyrillic"],
            "warm_on_connect": false,
            "max_message_mb": 0,
            "max_frame_mb": 8,
            "ping_interval_s": 0,
        }}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let after = realtime(&gw).await;
    assert_eq!(after.default_model, "my-chat");
    assert_eq!(after.model_map["whisper-1"], "my-asr");
    assert_eq!(
        (after.asr_alias.as_str(), after.tts_alias.as_str()),
        ("my-asr", "my-tts")
    );
    assert_eq!(after.voice_map["alloy"], "alba");
    assert_eq!(
        after.default_instructions, "",
        "empty is none, kept as that"
    );
    assert!(!after.default_instructions_is_builtin);
    assert_eq!(after.semantic_vad.low.floor, 0.5);
    assert_eq!(
        after.semantic_vad.low.threshold, 0.95,
        "the rest of the row stays"
    );
    assert_eq!(after.semantic_vad.high, before.semantic_vad.high);
    assert_eq!(after.barge_in_check, "duration");
    assert_eq!(after.barge_in_check_scripts, ["Latin", "Cyrillic"]);
    assert!(!after.warm_on_connect);
    assert_eq!((after.max_message_mb, after.max_frame_mb), (0, 8));
    assert_eq!(after.ping_interval_s, 0);

    // The built-in text puts the setting back to unset.
    let builtin = after.default_instructions_builtin.clone();
    let (status, _) = op(
        &gw,
        "settings_set_full",
        json!({"realtime": {"default_instructions": builtin}}),
    )
    .await;
    assert_eq!(status, 200);
    assert!(realtime(&gw).await.default_instructions_is_builtin);
}

#[tokio::test]
async fn a_save_refuses_what_no_session_could_run_and_names_the_field() {
    let (_state, gw) = setup().await;
    let before = realtime(&gw).await;
    for (patch, field) in [
        (json!({"tts_alias": "my-chat"}), "realtime.tts_alias"),
        (json!({"asr_alias": "my-tts"}), "realtime.asr_alias"),
        (
            json!({"barge_in_check_alias": "my-chat"}),
            "realtime.barge_in_check_alias",
        ),
        (
            json!({"barge_in_check_alias": "nope"}),
            "realtime.barge_in_check_alias",
        ),
        (json!({"default_model": "my-tts"}), "realtime.default_model"),
        (json!({"default_model": "nope"}), "realtime.default_model"),
        (
            json!({"model_map": {"gpt-realtime": "my-tts"}}),
            "realtime.model_map",
        ),
        (
            json!({"semantic_vad": {"medium": {"floor": 0.8}}}),
            "realtime.semantic_vad.medium",
        ),
        (
            json!({"semantic_floor_window_ms": 5000}),
            "realtime.semantic_vad.high",
        ),
        (
            json!({"max_message_mb": 0, "max_frame_mb": 0}),
            "realtime.max_message_mb",
        ),
        (json!({"barge_in_check_scripts": ["Elvish"]}), "Elvish"),
        (json!({"threshold": 2.0}), "realtime.threshold"),
    ] {
        // Beside a valid field, so the refusal is seen to refuse all of it.
        let mut p = patch.clone();
        p["warm_on_connect"] = json!(false);
        let (status, body) = op(&gw, "settings_set_full", json!({ "realtime": p })).await;
        assert_eq!(status, 400, "{patch} was accepted: {body}");
        assert_eq!(body["code"], "op_failed");
        let msg = body["message"].as_str().unwrap();
        assert!(msg.contains(field), "{patch}: {msg}");
    }
    assert_eq!(
        realtime(&gw).await,
        before,
        "a refused save changes nothing"
    );
    // Empty is each alias's "none", never refused.
    let (status, body) = op(
        &gw,
        "settings_set_full",
        json!({"realtime": {
            "default_model": "", "asr_alias": "", "tts_alias": "", "barge_in_check_alias": ""
        }}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn the_tool_plane_saves_the_same_patch() {
    let (state, gw) = setup().await;
    let p: lmgw_core::ops::SettingsPatch = serde_json::from_value(json!({
        "realtime": {"tts_alias": "my-tts", "echo_tail_ms": 400}
    }))
    .unwrap();
    let out = lmgw_core::ops::settings_set(&state, p).await.unwrap();
    assert_eq!(
        out["changed"],
        json!(["realtime.tts_alias", "realtime.echo_tail_ms"])
    );
    assert_eq!(realtime(&gw).await.echo_tail_ms, 400);
    let p: lmgw_core::ops::SettingsPatch =
        serde_json::from_value(json!({"realtime": {"tts_alias": "my-asr"}})).unwrap();
    let e = lmgw_core::ops::settings_set(&state, p).await.unwrap_err();
    assert!(e.contains("realtime.tts_alias"), "{e}");
    // The read the tool plane has carries the section too.
    let read = lmgw_core::ops::settings(&state).await.unwrap();
    assert_eq!(read["realtime"]["tts_alias"], "my-tts");
    assert_eq!(
        read["realtime"]["semantic_vad"]["medium"]["max_wait_ms"],
        4000
    );
}

/// Every chat model's `capabilities.endpoints` on `/v1/models`.
fn endpoints(list: &Value, id: &str) -> Vec<String> {
    let m = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("{id} is not listed: {list}"));
    serde_json::from_value(m["capabilities"]["endpoints"].clone()).unwrap()
}

#[tokio::test]
async fn chat_models_list_the_realtime_route_once_a_session_could_speak() {
    let (_state, gw) = setup().await;
    let list = get(&gw, "/v1/models").await;
    assert!(!endpoints(&list, "my-chat").contains(&"/v1/realtime".to_string()));
    // The list-level block names the route either way: it exists.
    let openai = list["lmgw"]["endpoints"]["openai"].as_array().unwrap();
    assert!(openai.iter().any(|p| p == "/v1/realtime"), "{openai:?}");

    // A TTS alias alone is not enough: a session could not hear.
    let (status, _) = op(
        &gw,
        "settings_set_full",
        json!({"realtime": {"tts_alias": "my-tts"}}),
    )
    .await;
    assert_eq!(status, 200);
    let list = get(&gw, "/v1/models").await;
    assert!(!endpoints(&list, "my-chat").contains(&"/v1/realtime".to_string()));

    // The Chat's transcription model counts, as a session takes it.
    let (status, _) = op(
        &gw,
        "settings_set_full",
        json!({"chat_stt_alias": "my-asr"}),
    )
    .await;
    assert_eq!(status, 200);
    let list = get(&gw, "/v1/models").await;
    assert_eq!(
        endpoints(&list, "my-chat"),
        ["/v1/chat/completions", "/v1/realtime"]
    );
    assert!(!endpoints(&list, "my-tts").contains(&"/v1/realtime".to_string()));
    let one = get(&gw, "/v1/models/my-chat").await;
    assert!(one["capabilities"]["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p == "/v1/realtime"));
}

/// A voice-design model answers the speech route too, and a session speaks
/// with one (its description is the session's speech instructions, WP10):
/// the setting takes it, and chat models list the realtime route on it — one
/// predicate for both.
#[tokio::test]
async fn a_voice_design_alias_is_a_text_to_speech_alias_for_realtime() {
    let (_state, gw) = setup().await;
    for patch in [
        json!({"realtime": {"tts_alias": "my-design"}}),
        json!({"chat_stt_alias": "my-asr"}),
    ] {
        let (status, body) = op(&gw, "settings_set_full", patch).await;
        assert_eq!(status, 200, "{body}");
    }
    assert_eq!(realtime(&gw).await.tts_alias, "my-design");
    let list = get(&gw, "/v1/models").await;
    assert_eq!(
        endpoints(&list, "my-chat"),
        ["/v1/chat/completions", "/v1/realtime"]
    );
}

async fn audio_row(g: &Gpu, id: &str, task: &str, on_disk: u64) -> i64 {
    let dir = g.models_dir().join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::File::create(dir.join("model.gguf"))
        .unwrap()
        .set_len(on_disk)
        .unwrap();
    let id = store::insert_audio_model(
        &g.state.db,
        &NewAudioModel {
            model_id: id.into(),
            family: "pocket_tts".into(),
            path: id.into(),
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
        },
    )
    .await
    .unwrap();
    g.state.reload_snapshot().await.unwrap();
    id
}

fn stage<'a>(b: &'a RealtimeBudget, id: &str) -> &'a RealtimeBudgetStage {
    b.stages
        .iter()
        .find(|s| s.stage == id)
        .unwrap_or_else(|| panic!("no {id} stage: {b:?}"))
}

async fn budget(gw: &Gw, args: Value) -> RealtimeBudget {
    let (status, body) = op(gw, "realtime_budget", args).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_value(body).unwrap()
}

#[tokio::test]
async fn the_budget_sizes_each_stage_with_the_figures_admission_charges() {
    let g = Gpu::new(16 * GIB, 0, 5).await;
    g.model("brain", 8 * GIB).await;
    // Learned: 3 GiB resident on 1 GiB of files.
    let ears = audio_row(&g, "ears", "asr", GIB).await;
    // Not learned yet: charged at its 1 GiB on disk.
    audio_row(&g, "voice", "tts", GIB).await;
    audio_row(&g, "check", "asr", 5 * GIB).await;
    let models = g.models_dir().display().to_string();
    let mut s = g.state.snapshot().settings.clone();
    s.audio.models_dir = models;
    s.realtime.default_model = "brain".into();
    s.realtime.asr_alias = "audio/ears".into();
    s.realtime.tts_alias = "audio/voice".into();
    store::save_settings(&g.state.db, &s).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
    let row = g
        .state
        .snapshot()
        .audio_models
        .iter()
        .find(|m| m.model_id == "ears")
        .cloned()
        .unwrap();
    let key = lmgw_core::vram::residency::resident_key(&row, &g.state.snapshot().settings.audio);
    store::set_audio_model_residency(&g.state.db, ears, Some((3 * GIB, &key)))
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();
    // A provider that is never asked anything here.
    let up = store::insert_upstream(
        &g.state.db,
        &NewUpstream {
            name: "provider".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: "https://api.provider.invalid/v1".into(),
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
        &g.state.db,
        &NewAlias {
            alias: "cloud-brain".into(),
            upstream_id: up,
            upstream_model_id: "big".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    g.state.reload_snapshot().await.unwrap();
    let gw = serve(g.state.clone()).await;

    // The saved cascade: 8 + 3 + 1 GiB of a 16 GiB card.
    let b = budget(&gw, json!({})).await;
    let ids: Vec<&str> = b.stages.iter().map(|s| s.stage.as_str()).collect();
    assert_eq!(
        ids,
        ["turn", "chat", "asr", "tts"],
        "no word check of its own"
    );
    assert_eq!(stage(&b, "turn").placement, "cpu");
    assert_eq!(stage(&b, "turn").bytes, Some(0));
    let chat = stage(&b, "chat");
    assert_eq!(
        (chat.placement.as_str(), chat.bytes),
        ("local", Some(8 * GIB))
    );
    assert_eq!(chat.model.as_deref(), Some("chat/brain"));
    let asr = stage(&b, "asr");
    assert_eq!(asr.bytes, Some(3 * GIB), "the learned residency");
    assert!(asr.note.contains("learned"), "{}", asr.note);
    let tts = stage(&b, "tts");
    assert_eq!(tts.bytes, Some(GIB), "the on-disk size, unlearned");
    assert!(tts.note.contains("not learned"), "{}", tts.note);
    assert_eq!(b.total_bytes, 12 * GIB);
    assert_eq!(b.headroom_bytes, 0);
    assert_eq!(b.capacity_bytes, Some(16 * GIB));
    assert_eq!(b.verdict, "fits", "{}", b.summary);
    assert!(b.unknown.is_empty());

    // A draft with a word check on its own model: 17 GiB will not fit.
    let b = budget(&gw, json!({"barge_in_check_alias": "audio/check"})).await;
    assert_eq!(stage(&b, "check").bytes, Some(5 * GIB));
    assert_eq!(b.verdict, "too_large", "{}", b.summary);
    // ... nor be sized when the check is by duration.
    let b = budget(
        &gw,
        json!({"barge_in_check_alias": "audio/check", "barge_in_check": "duration"}),
    )
    .await;
    assert!(b.stages.iter().all(|s| s.stage != "check"));
    // A model two stages name is held once.
    let b = budget(&gw, json!({"tts_alias": "audio/ears"})).await;
    assert_eq!(stage(&b, "tts").placement, "shared");
    assert_eq!(stage(&b, "tts").bytes, Some(0));
    assert_eq!(b.total_bytes, 11 * GIB);

    // The LLM in the cloud: nothing on the card for it.
    let b = budget(&gw, json!({"default_model": "cloud-brain"})).await;
    let chat = stage(&b, "chat");
    assert_eq!((chat.placement.as_str(), chat.bytes), ("cloud", Some(0)));
    assert_eq!(b.total_bytes, 4 * GIB);

    // What cannot be sized is said, and the verdict does not guess.
    let b = budget(&gw, json!({"tts_alias": "nope"})).await;
    assert_eq!(stage(&b, "tts").placement, "unresolved");
    assert_eq!(stage(&b, "tts").bytes, None);
    assert_eq!(b.unknown, ["tts"]);
    assert_eq!(b.verdict, "unknown", "{}", b.summary);
    // Unset is nothing to hold, not unknown.
    let b = budget(&gw, json!({"tts_alias": "", "default_model": ""})).await;
    assert_eq!(stage(&b, "tts").placement, "unset");
    assert_eq!(b.verdict, "fits");
    let (status, body) = op(&gw, "realtime_budget", json!({"barge_in_check": "both"})).await;
    assert_eq!(status, 400, "{body}");

    // The word check's model on the CPU (the per-row CPU switch): nothing
    // on this card, and the 17 GiB draft fits again.
    sqlx::query("UPDATE audio_models SET backend = 'cpu', threads = 8 WHERE model_id = 'check'")
        .execute(&g.state.db)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();
    let b = budget(&gw, json!({"barge_in_check_alias": "audio/check"})).await;
    let check = stage(&b, "check");
    assert_eq!((check.placement.as_str(), check.bytes), ("cpu", Some(0)));
    assert!(
        check
            .note
            .contains("runs on this machine's CPU (8 threads, set on the row)")
            && check.note.contains("host RAM is not measured"),
        "{}",
        check.note
    );
    assert_eq!(b.total_bytes, 12 * GIB);
    assert_eq!(b.verdict, "fits", "{}", b.summary);
}
