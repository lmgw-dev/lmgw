//! Expressive speech in a realtime session (WP10): the speech instructions
//! every clause is sent with and where they come from, what a TTS that reads
//! none gets, a designed voice's description and seed, the prompt's hint
//! about the sounds the voice makes, a cloud TTS and a fallback under the
//! GPU hold. Local rows run on the fake GPU world with synthetic packages
//! (`support::audiocpp_gguf`); nothing real is started.

use std::path::Path;

use lmgw_api_types::realtime::cue_hint_text;
use lmgw_core::config::{HoldFallbackMode, Settings};
use lmgw_core::store;
use serde_json::{json, Value};

use crate::support::audio_world::{tts_row, wav_bytes};
use crate::support::audiocpp_gguf;
use crate::support::gpu_world::{Gpu, GIB};
use crate::support::realtime_fakes::{
    add_chat_aliases, chat_fake, events_until, gpu_gateway, next_event, send, user_text, ChatFake,
    Turn, Ws,
};
use crate::support::realtime_tts::{add_cloud_tts_alias, speech_gateway, spoken_session, tts_fake};

/// A long lead: nothing waits for the paced send.
const LEAD: u32 = 60_000;

/// An audio row: `(model id, family, task, default request options, its
/// package)`.
pub(crate) type Row = (&'static str, &'static str, &'static str, Value, fn(&Path));

pub(crate) fn custom_voice(r: &Path) {
    audiocpp_gguf::qwen3(r, "custom_voice")
}

pub(crate) fn voice_design(r: &Path) {
    audiocpp_gguf::qwen3(r, "voice_design")
}

fn kokoro(r: &Path) {
    audiocpp_gguf::with_options(r, "kokoro_tts", &["language", "speed"], &["offline"])
}

pub(crate) fn omnivoice(r: &Path) {
    audiocpp_gguf::with_options(r, "omnivoice", &[], &["offline"])
}

fn cosy(r: &Path) {
    audiocpp_gguf::with_options(r, "cosyvoice3", &["instruction"], &["offline"])
}

/// The rows in `models` (with their packages), a voice-library clip `alba`
/// for the session's voice, the chat fake, and the settings `tweak` makes
/// on top of `realtime.tts_alias` = `audio/<first row>`. `starts` is how
/// many containers the world may start.
pub(crate) async fn gateway(
    rows: &[Row],
    starts: usize,
    tweak: impl FnOnce(&mut Settings),
) -> (Gpu, String, ChatFake) {
    let g = Gpu::new(24 * GIB, starts, 5).await;
    let chat = chat_fake().await;
    add_chat_aliases(&g.state, &chat).await;
    for (id, family, task, defaults, package) in rows {
        let root = g.models_dir().join(id);
        std::fs::create_dir_all(&root).unwrap();
        package(&root);
        let mut row = tts_row(id, family);
        row.task = (*task).into();
        row.default_request_options = defaults.as_object().cloned().unwrap();
        store::insert_audio_model(&g.state.db, &row).await.unwrap();
    }
    let voices = g.models_dir().join("voices");
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::write(voices.join("alba.wav"), wav_bytes(24_000, 480)).unwrap();
    // With its transcript: OmniVoice cannot clone a clip without one
    // (`voice_needs_transcript`).
    std::fs::write(voices.join("prompt_text"), "alba|Guten Tag.\n").unwrap();
    let models = g.models_dir().display().to_string();
    let first = rows[0].0;
    let addr = gpu_gateway(&g, |s| {
        s.audio.models_dir = models;
        s.realtime.tts_alias = format!("audio/{first}");
        s.realtime.default_voice = "alba".into();
        s.realtime.warm_on_connect = false;
        tweak(s);
    })
    .await;
    (g, addr, chat)
}

/// A session with audio output; `lmgw` is merged into its `session.lmgw`.
pub(crate) async fn session(addr: &str, lmgw: Value) -> (Ws, Value) {
    let mut ext = json!({"output_lead_ms": LEAD});
    for (k, v) in lmgw.as_object().unwrap() {
        ext[k] = v.clone();
    }
    let (ws, updated) = spoken_session(addr, &[], LEAD, json!({ "lmgw": ext })).await;
    (ws, updated["session"].clone())
}

/// A `session.update` of `lmgw` alone; the updated session.
pub(crate) async fn update(ws: &mut Ws, lmgw: Value) -> Value {
    send(
        ws,
        json!({"type": "session.update", "session": {"type": "realtime", "lmgw": lmgw}}),
    )
    .await;
    let u = next_event(ws).await;
    assert_eq!(u["type"], "session.updated", "{u}");
    u["session"].clone()
}

/// A user turn, then `create`, answered with `words`; the response's events,
/// which must complete.
pub(crate) async fn answer(
    ws: &mut Ws,
    chat: &ChatFake,
    words: &[&'static str],
    create: Value,
) -> Vec<Value> {
    chat.push(Turn::text(words));
    send(ws, user_text("hi")).await;
    events_until(ws, "conversation.item.done").await;
    send(ws, create).await;
    let events = events_until(ws, "response.done").await;
    assert_eq!(
        events.last().unwrap()["response"]["status"],
        "completed",
        "{events:?}"
    );
    events
}

pub(crate) fn create() -> Value {
    json!({"type": "response.create"})
}

/// Two clauses.
const TWO: &[&str] = &["Hello there. How are you today?"];

/// The speech bodies the world's audio containers got from `from` on.
fn bodies(g: &Gpu, from: usize) -> Vec<Value> {
    g.world().speech_bodies[from..].to_vec()
}

pub(crate) fn spoken(g: &Gpu) -> usize {
    g.world().speech_bodies.len()
}

/// `field` of every body from `from` on.
pub(crate) fn each(g: &Gpu, from: usize, field: &str) -> Vec<Value> {
    bodies(g, from)
        .iter()
        .map(|b| b.get(field).cloned().unwrap_or(Value::Null))
        .collect()
}

pub(crate) fn custom() -> Row {
    ("custom", "qwen3_tts", "tts", json!({}), custom_voice)
}

#[tokio::test]
async fn a_style_goes_with_every_clause_from_the_level_that_sets_it() {
    let (g, addr, chat) = gateway(&[custom()], 1, |s| {
        s.realtime.speech_instructions = "calm".into();
    })
    .await;
    let (mut ws, s) = session(&addr, json!({"speech_instructions": "excited, laughing"})).await;
    assert_eq!(s["lmgw"]["speech_instructions"], "excited, laughing");
    assert_eq!(s["lmgw"]["tag_hint"], true, "the setting's");
    assert!(
        s["lmgw"].get("speech_seed").is_none(),
        "only as the client set it"
    );
    // CustomVoice renders no tags and takes delivery cues (WP9b): the
    // prompt is told about those.
    assert_eq!(
        s["lmgw"]["resolved"]["speech"],
        json!({"instructions": "style", "text": "excited, laughing", "source": "session",
               "dropped": false, "tags": "none", "cues": true,
               "tag_hint": cue_hint_text(), "seed": null})
    );

    let n = spoken(&g);
    answer(&mut ws, &chat, TWO, create()).await;
    assert_eq!(
        each(&g, n, "instructions"),
        [json!("excited, laughing"), json!("excited, laughing")]
    );
    // A response's own, for that response only.
    let n = spoken(&g);
    answer(
        &mut ws,
        &chat,
        TWO,
        json!({"type": "response.create",
               "response": {"lmgw": {"speech_instructions": "whispering"}}}),
    )
    .await;
    assert_eq!(
        each(&g, n, "instructions"),
        [json!("whispering"), json!("whispering")]
    );
    let n = spoken(&g);
    answer(&mut ws, &chat, &["Back again."], create()).await;
    assert_eq!(each(&g, n, "instructions"), [json!("excited, laughing")]);

    // `""` is none for the session — not the owner's either.
    let s = update(&mut ws, json!({"speech_instructions": ""})).await;
    assert_eq!(s["lmgw"]["resolved"]["speech"]["text"], Value::Null);
    let n = spoken(&g);
    answer(&mut ws, &chat, &["Plain."], create()).await;
    assert_eq!(each(&g, n, "instructions"), [Value::Null]);
    // `null` is the owner's setting.
    let s = update(&mut ws, json!({"speech_instructions": null})).await;
    assert!(s["lmgw"].get("speech_instructions").is_none());
    let speech = &s["lmgw"]["resolved"]["speech"];
    assert_eq!(
        (&speech["text"], &speech["source"]),
        (&json!("calm"), &json!("setting"))
    );
    let n = spoken(&g);
    answer(&mut ws, &chat, &["Calm now."], create()).await;
    assert_eq!(each(&g, n, "instructions"), [json!("calm")]);

    // `response.lmgw` is lmgw's own, and strict.
    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "bad",
               "response": {"lmgw": {"speech_style": "x"}}}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["type"], "error");
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .contains("speech_style"),
        "{e}"
    );
}

#[tokio::test]
async fn a_tts_that_reads_no_instructions_is_sent_none_and_the_echo_says_so() {
    let (g, addr, chat) = gateway(
        &[("kokoro", "kokoro_tts", "tts", json!({}), kokoro)],
        1,
        |_| {},
    )
    .await;
    let (mut ws, s) = session(&addr, json!({"speech_instructions": "cheerful"})).await;
    assert_eq!(
        s["lmgw"]["resolved"]["speech"],
        json!({"instructions": "none", "text": "cheerful", "source": "session",
               "dropped": true, "tags": "none", "cues": false, "tag_hint": null,
               "seed": null})
    );
    answer(&mut ws, &chat, TWO, create()).await;
    assert_eq!(each(&g, 0, "instructions"), [Value::Null, Value::Null]);
}

#[tokio::test]
async fn a_designed_voice_keeps_its_description_and_its_seed() {
    let (g, addr, chat) = gateway(
        &[
            (
                "design-row",
                "qwen3_tts",
                "vdes",
                json!({"instruct": "a deep, slow narrator"}),
                voice_design,
            ),
            ("design", "qwen3_tts", "vdes", json!({}), voice_design),
            (
                "design-pinned",
                "qwen3_tts",
                "vdes",
                json!({"instruct": "a deep, slow narrator", "seed": 5}),
                voice_design,
            ),
        ],
        3,
        |s| s.realtime.speech_instructions = "cheerful".into(),
    )
    .await;

    // The row describes its voice, and the owner's style stands back: lmgw
    // sends no instructions — the engine merges the row's — and its seed.
    let (mut ws, s) = session(&addr, json!({})).await;
    let speech = s["lmgw"]["resolved"]["speech"].clone();
    assert_eq!(speech["instructions"], "voice_design");
    assert_eq!(speech["text"], "a deep, slow narrator");
    assert_eq!(speech["source"], "row");
    let seed = speech["seed"]
        .as_u64()
        .expect("a seed of the session's own");
    let n = spoken(&g);
    answer(&mut ws, &chat, TWO, create()).await;
    assert_eq!(each(&g, n, "instructions"), [Value::Null, Value::Null]);
    assert_eq!(each(&g, n, "seed"), [json!(seed), json!(seed)]);

    // A description of the session's own: the same description and the
    // same seed on every clause of every response.
    let sailor = "an old sailor, hoarse and slow";
    let (mut ws, s) = session(
        &addr,
        json!({"tts_model": "audio/design", "speech_instructions": sailor}),
    )
    .await;
    let seed = s["lmgw"]["resolved"]["speech"]["seed"].as_u64().unwrap();
    let n = spoken(&g);
    answer(&mut ws, &chat, TWO, create()).await;
    answer(&mut ws, &chat, &["And once more."], create()).await;
    assert_eq!(each(&g, n, "instructions"), vec![json!(sailor); 3]);
    assert_eq!(each(&g, n, "seed"), vec![json!(seed); 3]);
    // Another session draws another one, unless the client pins it.
    let (_ws, s) = session(
        &addr,
        json!({"tts_model": "audio/design", "speech_instructions": sailor}),
    )
    .await;
    let other = s["lmgw"]["resolved"]["speech"]["seed"].as_u64().unwrap();
    assert_ne!(other, seed, "a new session's own seed");
    let (mut ws, s) = session(
        &addr,
        json!({"tts_model": "audio/design", "speech_instructions": sailor,
               "speech_seed": 1234}),
    )
    .await;
    assert_eq!(s["lmgw"]["speech_seed"], 1234);
    assert_eq!(s["lmgw"]["resolved"]["speech"]["seed"], 1234);
    let n = spoken(&g);
    answer(&mut ws, &chat, &["Pinned."], create()).await;
    assert_eq!(each(&g, n, "seed"), [json!(1234)]);

    // A row that pins its own seed keeps it: lmgw sends none.
    let (mut ws, s) = session(&addr, json!({"tts_model": "audio/design-pinned"})).await;
    assert_eq!(s["lmgw"]["resolved"]["speech"]["seed"], Value::Null);
    let n = spoken(&g);
    answer(&mut ws, &chat, &["Kept."], create()).await;
    assert_eq!(each(&g, n, "seed"), [Value::Null]);
    assert_eq!(g.runs(), ["design-row", "design", "design-pinned"]);
}

/// A row that describes itself under `instruct` — the key lmgw's own
/// `instructions_required` messages recommend — and a text from the session
/// or the owner: audio.cpp merges the row's in beside the request's
/// `instruction`, and Qwen3 refuses two that differ, every clause after
/// admission (the world's containers answer as it would,
/// `support::audiocpp_options`). The text goes under both keys, so it
/// replaces the row's (R2): a voice-design row's description by the
/// session's, a CustomVoice row's style by the owner's.
#[tokio::test]
async fn a_text_over_a_row_s_own_description_replaces_it_under_both_keys() {
    let (g, addr, chat) = gateway(
        &[
            (
                "design-row",
                "qwen3_tts",
                "vdes",
                json!({"instruct": "a deep, slow narrator"}),
                voice_design,
            ),
            (
                "custom-row",
                "qwen3_tts",
                "tts",
                json!({"instruct": "calm"}),
                custom_voice,
            ),
        ],
        2,
        |s| s.realtime.speech_instructions = "cheerful".into(),
    )
    .await;
    let sailor = "an old sailor, hoarse and slow";
    let (mut ws, s) = session(&addr, json!({"speech_instructions": sailor})).await;
    let speech = &s["lmgw"]["resolved"]["speech"];
    assert_eq!(
        (&speech["text"], &speech["source"]),
        (&json!(sailor), &json!("session"))
    );
    let n = spoken(&g);
    answer(&mut ws, &chat, TWO, create()).await;
    assert_eq!(each(&g, n, "instructions"), vec![json!(sailor); 2]);
    assert_eq!(each(&g, n, "options"), vec![json!({"instruct": sailor}); 2]);

    let (mut ws, s) = session(&addr, json!({"tts_model": "audio/custom-row"})).await;
    let speech = &s["lmgw"]["resolved"]["speech"];
    assert_eq!(
        (&speech["text"], &speech["source"]),
        (&json!("cheerful"), &json!("setting"))
    );
    let n = spoken(&g);
    answer(&mut ws, &chat, TWO, create()).await;
    assert_eq!(each(&g, n, "instructions"), vec![json!("cheerful"); 2]);
    assert_eq!(
        each(&g, n, "options"),
        vec![json!({"instruct": "cheerful"}); 2]
    );
    assert!(g.world().refused_speech.is_empty());
    assert_eq!(g.runs(), ["design-row", "custom-row"]);
}

#[tokio::test]
async fn a_designed_voice_with_no_description_is_refused_before_the_response() {
    let (g, addr, _chat) = gateway(
        &[("design", "qwen3_tts", "vdes", json!({}), voice_design)],
        1,
        |_| {},
    )
    .await;
    let (mut ws, s) = session(&addr, json!({})).await;
    assert_eq!(s["lmgw"]["resolved"]["speech"]["text"], Value::Null);
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "r1"}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["type"], "error", "before response.created: {e}");
    assert_eq!(e["error"]["code"], "instructions_required");
    assert_eq!(e["error"]["param"], "session.lmgw.speech_instructions");
    assert_eq!(e["error"]["event_id"], "r1");
    assert!(g.runs().is_empty(), "started: {:?}", g.runs());
    // A text response is not the voice's business.
    send(
        &mut ws,
        json!({"type": "response.create", "response": {"output_modalities": ["text"]}}),
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "response.created");
}

/// A voice-design TTS designs its voice from the description and reads no
/// speaker (Qwen3 VoiceDesign, MOSS-VoiceGen). On default settings — no
/// `realtime.default_voice`, no default preset on the row, no voice from
/// the client — the session's built-in `marin` had nothing to stand for
/// it, and every audio response was refused `voice_not_configured`, the
/// description notwithstanding. It is sent no voice now, and the echo says
/// `designed` (R2). A voice the client names is still sent; so is the
/// owner's (`default_voice`, the tests above).
#[tokio::test]
async fn a_designed_voice_speaks_on_default_settings_with_no_voice_sent() {
    let (g, addr, chat) = gateway(
        &[
            (
                "design-row",
                "qwen3_tts",
                "vdes",
                json!({"instruct": "a deep, slow narrator"}),
                voice_design,
            ),
            ("design", "qwen3_tts", "vdes", json!({}), voice_design),
        ],
        2,
        |s| s.realtime.default_voice = String::new(),
    )
    .await;
    // The row's own description.
    let (mut ws, s) = session(&addr, json!({})).await;
    assert_eq!(s["audio"]["output"]["voice"], "marin");
    assert_eq!(s["lmgw"]["resolved"]["voice"], "designed");
    let n = spoken(&g);
    let events = answer(&mut ws, &chat, TWO, create()).await;
    assert_eq!(
        events.last().unwrap()["response"]["audio"]["output"]["voice"],
        "marin"
    );
    assert_eq!(each(&g, n, "voice"), [Value::Null, Value::Null]);

    // The session's own, on a row without one.
    let sailor = "an old sailor, hoarse and slow";
    let (mut ws, s) = session(
        &addr,
        json!({"tts_model": "audio/design", "speech_instructions": sailor}),
    )
    .await;
    assert_eq!(s["lmgw"]["resolved"]["voice"], "designed");
    let n = spoken(&g);
    answer(&mut ws, &chat, &["Ahoy."], create()).await;
    assert_eq!(each(&g, n, "voice"), [Value::Null]);
    assert_eq!(each(&g, n, "instructions"), [json!(sailor)]);

    // A voice the client names is sent.
    send(
        &mut ws,
        json!({"type": "session.update",
               "session": {"type": "realtime", "audio": {"output": {"voice": "alba"}}}}),
    )
    .await;
    let u = next_event(&mut ws).await;
    assert_eq!(u["session"]["lmgw"]["resolved"]["voice"], "alba", "{u}");
    let n = spoken(&g);
    answer(&mut ws, &chat, &["Ahoy again."], create()).await;
    assert_eq!(each(&g, n, "voice"), [json!("alba")]);

    // Nothing to design from: refused for that, not for the voice.
    let (mut ws, _) = session(&addr, json!({"tts_model": "audio/design"})).await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, create()).await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["error"]["code"], "instructions_required", "{e}");
    assert_eq!(g.runs(), ["design-row", "design"]);
}

/// Live run 3, N2: the owner's `realtime.default_voice` names a voice of
/// another model (`cosette`: neither a preset of the row nor a clip of the
/// library). A session on a voice-design TTS was refused
/// `voice_not_configured` for it; the designing TTS passes it over and
/// designs its voice. (The library clip `alba` it has is still sent — the
/// harness's default, the test above.)
#[tokio::test]
async fn a_designed_voice_passes_over_a_default_voice_it_does_not_have() {
    let (g, addr, chat) = gateway(
        &[(
            "design-row",
            "qwen3_tts",
            "vdes",
            json!({"instruct": "a deep, slow narrator"}),
            voice_design,
        )],
        1,
        |s| s.realtime.default_voice = "cosette".into(),
    )
    .await;
    let (mut ws, s) = session(&addr, json!({})).await;
    assert_eq!(s["lmgw"]["resolved"]["voice"], "designed", "{s}");
    let n = spoken(&g);
    answer(&mut ws, &chat, TWO, create()).await;
    assert_eq!(each(&g, n, "voice"), [Value::Null, Value::Null]);
    assert!(g.world().refused_speech.is_empty());
}

/// R3 (live run 3, 4d) sent OmniVoice, which speaks when named no voice
/// (`audio::families::unvoiced`), no voice, echoed `engine_default`; R4 M1
/// sent every clause the session's seed to hold the speaker it draws. Live
/// run 3c heard the seed fix each clause's draw but not the speaker: the
/// clause's text picks it, and one answer spoke at 222, 118 and 222 Hz.
///
/// R5 F1: with nothing configured, or a `default_voice` it does not know (a
/// voice of another model), its voice is missing — echoed `null`, and an
/// audio response is refused `voice_not_configured` before
/// `response.created`, saying why, with nothing started. With a voice — a
/// library clip as the default here — it speaks that, and every clause
/// still carries the session's seed, echoed in `resolved.speech.seed`;
/// another session draws another.
#[tokio::test]
async fn a_tts_that_draws_its_speaker_needs_a_voice_configured() {
    let row: Row = ("omni", "omnivoice", "tts", json!({}), omnivoice);
    for default_voice in ["", "cosette"] {
        let (g, addr, _chat) = gateway(std::slice::from_ref(&row), 1, |s| {
            s.realtime.default_voice = default_voice.into()
        })
        .await;
        let (mut ws, s) = session(&addr, json!({})).await;
        assert_eq!(
            s["lmgw"]["resolved"]["voice"],
            Value::Null,
            "{default_voice:?}: {s}"
        );
        send(&mut ws, user_text("hi")).await;
        events_until(&mut ws, "conversation.item.done").await;
        send(&mut ws, create()).await;
        let e = next_event(&mut ws).await;
        assert_eq!(e["type"], "error", "before response.created: {e}");
        assert_eq!(e["error"]["code"], "voice_not_configured");
        let message = e["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("draws a new speaker for every request")
                && message.contains("configure a voice clip or preset"),
            "{e}"
        );
        assert_eq!(message.contains("cosette"), !default_voice.is_empty());
        assert!(g.runs().is_empty(), "started: {:?}", g.runs());
    }

    // The harness's default voice, the library clip `alba`.
    let (g, addr, chat) = gateway(&[row], 1, |_| {}).await;
    let (mut ws, s) = session(&addr, json!({})).await;
    assert_eq!(s["lmgw"]["resolved"]["voice"], "alba", "{s}");
    let seed = s["lmgw"]["resolved"]["speech"]["seed"]
        .as_u64()
        .expect("the session's seed, still sent");
    let n = spoken(&g);
    answer(&mut ws, &chat, TWO, create()).await;
    assert_eq!(each(&g, n, "voice"), [json!("alba"), json!("alba")]);
    assert_eq!(each(&g, n, "seed"), [json!(seed), json!(seed)]);
    let n = spoken(&g);
    answer(&mut ws, &chat, &["Again."], create()).await;
    assert_eq!(each(&g, n, "seed"), [json!(seed)], "every response's");
    assert!(g.world().refused_speech.is_empty());

    let (_ws, other) = session(&addr, json!({})).await;
    let other = other["lmgw"]["resolved"]["speech"]["seed"].as_u64();
    assert_ne!(other, Some(seed), "a new session's own seed");
    assert!(other.is_some());
}

/// R3 N4: CosyVoice3 clones from reference audio, and its engine refuses a
/// request without a clip. A session whose only voice for it is the row's
/// inline default preset without one is told before the response starts,
/// like any voice it cannot speak with — nothing is started for it.
#[tokio::test]
async fn a_cloning_tts_without_a_clip_is_refused_before_the_response() {
    let (g, addr, _chat) = gateway(&[("cosy", "cosyvoice3", "tts", json!({}), cosy)], 1, |s| {
        s.realtime.default_voice = String::new()
    })
    .await;
    sqlx::query("UPDATE audio_models SET default_voice_preset = ?1 WHERE model_id = 'cosy'")
        .bind(json!({"voice_id": "anna"}).to_string())
        .execute(&g.state.db)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();
    let (mut ws, s) = session(&addr, json!({})).await;
    assert_eq!(s["lmgw"]["resolved"]["voice"], Value::Null, "{s}");
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, create()).await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["type"], "error", "before response.created: {e}");
    assert_eq!(e["error"]["code"], "voice_not_configured");
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .contains("reference audio"),
        "{e}"
    );
    assert!(g.runs().is_empty(), "started: {:?}", g.runs());
}

/// R4 D5 (live run 3b, 6): with the owner's `default_voice` naming another
/// model's voice (Pocket's `alba` there, `cosette` here: no clip of the
/// library), a session on CosyVoice3 was admitted with it provisionally —
/// `response.created`, its container started, and only then
/// `voice_not_configured`. A cloning TTS takes only one of its clips as the
/// default: the refusal comes before `response.created`, nothing started. A
/// library clip as the default (the harness's `alba`) is cloned from.
#[tokio::test]
async fn a_cloning_tts_passes_over_a_default_voice_that_is_no_clip() {
    let row: Row = ("cosy", "cosyvoice3", "tts", json!({}), cosy);
    let (g, addr, _chat) = gateway(std::slice::from_ref(&row), 1, |s| {
        s.realtime.default_voice = "cosette".into()
    })
    .await;
    let (mut ws, s) = session(&addr, json!({})).await;
    assert_eq!(s["lmgw"]["resolved"]["voice"], Value::Null, "{s}");
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, create()).await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["type"], "error", "before response.created: {e}");
    assert_eq!(e["error"]["code"], "voice_not_configured");
    let message = e["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("realtime.default_voice") && message.contains("cosette"),
        "{e}"
    );
    assert!(g.runs().is_empty(), "started: {:?}", g.runs());

    let (g, addr, chat) = gateway(&[row], 1, |_| {}).await;
    let (mut ws, s) = session(&addr, json!({})).await;
    assert_eq!(s["lmgw"]["resolved"]["voice"], "alba", "{s}");
    answer(&mut ws, &chat, TWO, create()).await;
    assert_eq!(each(&g, 0, "voice"), [json!("alba"), json!("alba")]);
}

/// The owner sets `realtime.default_voice` to `voice`.
async fn default_voice(g: &Gpu, voice: &str) {
    let mut s = g.state.snapshot().settings.clone();
    s.realtime.default_voice = voice.into();
    store::save_settings(&g.state.db, &s).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
}

/// A user turn and a `response.create`; the first event after it.
async fn refused(ws: &mut Ws) -> Value {
    send(ws, user_text("hi")).await;
    events_until(ws, "conversation.item.done").await;
    send(ws, create()).await;
    next_event(ws).await
}

/// A user turn and a response that fails at its first clause with
/// `voice_not_configured`, after its TTS was admitted.
async fn failed_after_admission(ws: &mut Ws, chat: &ChatFake) {
    chat.push(Turn::text(TWO));
    send(ws, user_text("hi")).await;
    events_until(ws, "conversation.item.done").await;
    send(ws, create()).await;
    let events = events_until(ws, "response.done").await;
    assert!(
        events.iter().any(|e| e["type"] == "response.created"),
        "{events:?}"
    );
    assert_eq!(events.last().unwrap()["response"]["status"], "failed");
    let e = events.iter().find(|e| e["type"] == "error").unwrap();
    assert_eq!(e["error"]["code"], "voice_not_configured", "{e}");
}

/// R5 review (R6): with the class's `voice_dir` mounted for the engine
/// alone, lmgw cannot list its clips, so a cloning TTS is sent a
/// `default_voice` that names none of them (`cosette`) provisionally, and
/// the first clause's list decides. The session then forgot that list
/// before each response, so it would be read afresh — and with it lmgw
/// could not see the voices again: every audio response was created, its
/// TTS admitted, and only then refused. The list seen still decides: the
/// first response fails after admission, the next before
/// `response.created`, nothing started — a snapshot published for anything
/// else changes nothing. The owner's change reads it afresh, and a clip the
/// list shows (`alba`) is spoken.
#[tokio::test]
async fn a_default_voice_the_list_ruled_out_is_refused_before_the_next_response() {
    let (g, addr, chat) = gateway(&[("cosy", "cosyvoice3", "tts", json!({}), cosy)], 1, |s| {
        s.audio.voice_dir = "/voices".into();
        s.realtime.default_voice = "cosette".into();
    })
    .await;
    let (mut ws, s) = session(&addr, json!({})).await;
    assert_eq!(
        s["lmgw"]["resolved"]["voice"], "cosette",
        "provisional: {s}"
    );
    failed_after_admission(&mut ws, &chat).await;
    assert_eq!(g.runs(), ["cosy"]);

    g.state.reload_snapshot().await.unwrap();
    let e = refused(&mut ws).await;
    assert_eq!(e["type"], "error", "before response.created: {e}");
    assert_eq!(e["error"]["code"], "voice_not_configured");
    let message = e["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("('cosette')") && message.contains("in its voice list"),
        "{e}"
    );
    assert_eq!(g.runs(), ["cosy"], "nothing started");

    // The owner's change: read afresh, though the list seen lacks the name.
    default_voice(&g, "zora").await;
    failed_after_admission(&mut ws, &chat).await;
    default_voice(&g, "alba").await;
    answer(&mut ws, &chat, TWO, create()).await;
    assert_eq!(each(&g, 0, "voice"), [json!("alba"), json!("alba")]);
    assert_eq!(g.runs(), ["cosy"]);
}

/// R5 review (R6): with no `voice_dir` at all the engine answers to no
/// clip — `alba.wav` in the library dir included — and lmgw's own list is
/// the model's whole one. lmgw treated that as voices it cannot see, so a
/// cloning or speaker-drawing TTS was sent `default_voice` provisionally,
/// admitted and refused on every response. It is refused before
/// `response.created` now, nothing started.
#[tokio::test]
async fn with_no_voice_dir_a_default_voice_that_is_no_clip_is_refused_at_once() {
    let rows: [Row; 2] = [
        ("cosy", "cosyvoice3", "tts", json!({}), cosy),
        ("omni", "omnivoice", "tts", json!({}), omnivoice),
    ];
    for row in rows {
        let (g, addr, _chat) = gateway(&[row], 1, |s| s.audio.voice_dir = String::new()).await;
        let (mut ws, s) = session(&addr, json!({})).await;
        assert_eq!(s["lmgw"]["resolved"]["voice"], Value::Null, "{s}");
        let e = refused(&mut ws).await;
        assert_eq!(e["type"], "error", "before response.created: {e}");
        assert_eq!(e["error"]["code"], "voice_not_configured");
        let message = e["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("('alba')") && message.contains("voice_dir is empty"),
            "{e}"
        );
        assert!(g.runs().is_empty(), "started: {:?}", g.runs());
    }
}

#[tokio::test]
async fn the_prompt_names_the_voice_s_sounds_and_the_transcript_leaves_them_out() {
    let (g, addr, chat) = gateway(
        &[
            ("omni", "omnivoice", "tts", json!({}), omnivoice),
            ("kokoro", "kokoro_tts", "tts", json!({}), kokoro),
        ],
        2,
        |_| {},
    )
    .await;
    let (mut ws, s) = session(&addr, json!({})).await;
    let speech = &s["lmgw"]["resolved"]["speech"];
    assert_eq!(speech["tags"], "fixed");
    let hint = speech["tag_hint"].as_str().unwrap().to_string();
    assert!(
        hint.contains("can also make these sounds: [laughter] [sigh]."),
        "{hint}"
    );
    assert!(
        !s["instructions"].as_str().unwrap().contains(&hint),
        "never put into the session's instructions"
    );

    let events = answer(
        &mut ws,
        &chat,
        &["Ha [laughs] that is funny.", " [sighs]"],
        create(),
    )
    .await;
    let system = chat.seen.chat(0)["messages"][0].clone();
    assert_eq!(system["role"], "system");
    assert!(
        system["content"]
            .as_str()
            .unwrap()
            .ends_with(&format!("\n\n{hint}")),
        "a paragraph after the instructions: {system}"
    );
    assert_eq!(
        each(&g, 0, "input"),
        [json!("Ha [laughter] that is funny.")]
    );
    let done = &events.last().unwrap()["response"];
    assert_eq!(
        done["output"][0]["content"][0]["transcript"],
        "Ha that is funny."
    );

    // A text response gets no hint, and its history keeps both tags.
    chat.push(Turn::text(&["Sure."]));
    send(&mut ws, user_text("again")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(
        &mut ws,
        json!({"type": "response.create", "response": {"output_modalities": ["text"]}}),
    )
    .await;
    events_until(&mut ws, "response.done").await;
    let messages = chat.seen.chat(1)["messages"].clone();
    assert!(!messages[0]["content"].as_str().unwrap().contains(&hint));
    assert_eq!(messages[2]["content"], "Ha [laughs] that is funny. [sighs]");

    // Off, the prompt has none.
    let s = update(&mut ws, json!({"tag_hint": false})).await;
    assert_eq!(s["lmgw"]["tag_hint"], false);
    assert_eq!(s["lmgw"]["resolved"]["speech"]["tag_hint"], Value::Null);
    answer(&mut ws, &chat, &["Fine."], create()).await;
    assert!(!chat.seen.chat(2)["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains(&hint));
    // Nor for a TTS that renders no tags.
    let s = update(
        &mut ws,
        json!({"tag_hint": true, "tts_model": "audio/kokoro"}),
    )
    .await;
    assert_eq!(s["lmgw"]["resolved"]["speech"]["tag_hint"], Value::Null);
    answer(&mut ws, &chat, &["Fine."], create()).await;
    assert!(!chat.seen.chat(3)["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("square brackets"));
}

#[tokio::test]
async fn a_cloud_tts_gets_the_style_on_every_clause_and_never_a_seed() {
    let cloud = tts_fake(&[]).await;
    let (state, addr, chat, _local) = speech_gateway(false, None, |_| {}).await;
    add_cloud_tts_alias(&state, &cloud, "cloud-tts").await;
    let (mut ws, s) = session(
        &addr,
        json!({"tts_model": "cloud-tts", "speech_instructions": "a cheerful pirate",
               "speech_seed": 7}),
    )
    .await;
    assert_eq!(
        s["lmgw"]["resolved"]["speech"],
        json!({"instructions": "passthrough", "text": "a cheerful pirate", "source": "session",
               "dropped": false, "tags": "none", "cues": false,
               "tag_hint": null, "seed": null})
    );
    answer(&mut ws, &chat, TWO, create()).await;
    assert_eq!(cloud.seen.count(), 2);
    for n in 0..2 {
        let body = cloud.seen.body(n);
        assert_eq!(body["instructions"], "a cheerful pirate", "{body}");
        assert!(body.get("seed").is_none(), "{body}");
    }
}

#[tokio::test]
async fn under_the_gpu_hold_a_cloud_fallback_takes_the_description_as_its_style() {
    let cloud = tts_fake(&[]).await;
    let models = tempfile::tempdir().unwrap();
    let dir = models.path().display().to_string();
    let (state, addr, chat, local) = speech_gateway(false, None, |s| {
        s.hold.active = true;
        s.realtime.warm_on_connect = false;
        s.audio.models_dir = dir;
    })
    .await;
    add_cloud_tts_alias(&state, &cloud, "cloud-tts").await;
    let root = models.path().join("design");
    std::fs::create_dir_all(&root).unwrap();
    voice_design(&root);
    let mut row = tts_row("design", "qwen3_tts");
    row.task = "vdes".into();
    row.hold_fallback_mode = HoldFallbackMode::Alias;
    row.hold_fallback = Some("cloud-tts".into());
    store::insert_audio_model(&state.db, &row).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let sailor = "an old sailor, hoarse and slow";
    let (mut ws, s) = session(
        &addr,
        json!({"tts_model": "audio/design", "speech_instructions": sailor}),
    )
    .await;
    // The echo is the primary's: a designed voice, with a seed.
    let speech = &s["lmgw"]["resolved"]["speech"];
    assert_eq!(speech["instructions"], "voice_design");
    assert!(speech["seed"].is_u64(), "{speech}");
    answer(&mut ws, &chat, TWO, create()).await;
    assert_eq!(local.seen.count(), 0);
    assert_eq!(cloud.seen.count(), 2);
    for n in 0..2 {
        let body = cloud.seen.body(n);
        assert_eq!(body["instructions"], sailor, "{body}");
        assert!(body.get("seed").is_none(), "{body}");
    }
    assert!(state.runtime().list().is_empty(), "nothing started");
}
