//! Delivery cues in a realtime session (WP9b): a `[laughing]` that opens a
//! sentence is sent, to a TTS that renders no tags and reads a style, as how
//! to say that sentence — after the style, in its instructions, never in
//! its text — while the transcript never shows it and the history keeps it.
//! A TTS that renders tags keeps them, a voice-design row's description
//! never takes one, and a cloud TTS or fallback gets it as its
//! instructions only when its alias declares a style. Local rows run on
//! the fake GPU world with synthetic
//! packages (`support::audiocpp_gguf`); nothing real is started.

use lmgw_api_types::realtime::cue_hint_text;
use lmgw_core::config::HoldFallbackMode;
use lmgw_core::store;
use serde_json::{json, Value};

use crate::realtime_expressive::{
    answer, create, custom, custom_voice, each, gateway, omnivoice, session, spoken, update,
    voice_design, Row,
};
use crate::support::audio_world::{tts_row, wav_bytes};
use crate::support::realtime_fakes::{events_until, send, user_text, ChatFake, Turn, Ws};
use crate::support::realtime_tts::{
    add_cloud_tts_alias, add_described_cloud_tts_alias, speech_gateway, tts_fake, TtsFake,
};

/// A sentence opened by a cue, with a comma inside it (a clause is a whole
/// sentence: nothing cuts at the comma), then one without.
const LAUGH: &[&str] = &["[laughing] Oh no, that is funny. Anyway, it rains."];

/// The text the chat model wrote for the last answer, as the next request's
/// history has it: a text response after it, and its assistant message.
async fn history(ws: &mut Ws, chat: &ChatFake) -> Value {
    chat.push(Turn::text(&["Sure."]));
    send(ws, user_text("again")).await;
    events_until(ws, "conversation.item.done").await;
    send(
        ws,
        json!({"type": "response.create", "response": {"output_modalities": ["text"]}}),
    )
    .await;
    events_until(ws, "response.done").await;
    let messages = chat.seen.chat(chat.seen.chat_count() - 1)["messages"].clone();
    messages[2]["content"].clone()
}

/// Every transcript text a response's events carry: the deltas, the done
/// event's and the item's.
fn transcripts(events: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = events
        .iter()
        .filter(|e| {
            e["type"]
                .as_str()
                .is_some_and(|t| t.starts_with("response.output_audio_transcript."))
        })
        .filter_map(|e| e["delta"].as_str().or(e["transcript"].as_str()))
        .map(str::to_string)
        .collect();
    let done = &events.last().unwrap()["response"]["output"][0]["content"][0];
    out.push(done["transcript"].as_str().unwrap().to_string());
    out
}

/// The system prompt of the chat request `n`.
fn prompt(chat: &ChatFake, n: usize) -> String {
    let system = chat.seen.chat(n)["messages"][0].clone();
    assert_eq!(system["role"], "system");
    system["content"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_cue_goes_after_the_style_for_the_sentence_it_opens() {
    let (g, addr, chat) = gateway(&[custom()], 1, |s| {
        s.realtime.speech_instructions = "calm".into();
    })
    .await;
    let (mut ws, s) = session(&addr, json!({})).await;
    assert_eq!(s["lmgw"]["resolved"]["speech"]["cues"], true);
    let n = spoken(&g);
    let events = answer(&mut ws, &chat, LAUGH, create()).await;
    // The cue covers the sentence it opens, comma and all (one request),
    // and the next sentence has the style alone.
    assert_eq!(
        each(&g, n, "instructions"),
        [json!("calm, but laughing right now"), json!("calm")]
    );
    assert_eq!(
        each(&g, n, "input"),
        [json!("Oh no, that is funny."), json!("Anyway, it rains.")]
    );
    for t in transcripts(&events) {
        assert!(!t.contains('[') && !t.contains("laughing"), "{t}");
    }
    assert_eq!(
        transcripts(&events).last().unwrap(),
        "Oh no, that is funny. Anyway, it rains."
    );
    // The model sees its own cue in its history.
    assert_eq!(history(&mut ws, &chat).await, json!(LAUGH[0]));
}

/// A row that describes itself under `instruct` and no style: the cue goes
/// after the row's own description, under both keys (R2) — a bare cue would
/// have replaced the row's style for that sentence. Clauses without one
/// send nothing, and the engine merges the row's in.
#[tokio::test]
async fn a_cue_keeps_a_row_s_own_description() {
    let row: Row = (
        "custom-row",
        "qwen3_tts",
        "tts",
        json!({"instruct": "a narrator"}),
        custom_voice,
    );
    let (g, addr, chat) = gateway(&[row], 1, |_| {}).await;
    let (mut ws, s) = session(&addr, json!({})).await;
    let speech = &s["lmgw"]["resolved"]["speech"];
    assert_eq!(
        (&speech["text"], &speech["source"], &speech["cues"]),
        (&json!("a narrator"), &json!("row"), &json!(true))
    );
    answer(&mut ws, &chat, &["[laughing] Oh no. It rains."], create()).await;
    let cued = "a narrator, but laughing right now";
    assert_eq!(each(&g, 0, "instructions"), [json!(cued), Value::Null]);
    assert_eq!(
        each(&g, 0, "options"),
        [json!({"instruct": cued}), Value::Null]
    );
    assert_eq!(each(&g, 0, "input"), [json!("Oh no."), json!("It rains.")]);
    assert!(g.world().refused_speech.is_empty());
}

/// The prompt is told about cues on a TTS that takes them, about its sounds
/// on one that renders tags, and nothing on a voice-design row — whose
/// description is the same on every clause, the tag stripped. With the
/// hint off the cue still applies.
#[tokio::test]
async fn the_hint_says_what_brackets_do_and_each_tts_gets_its_own() {
    let rows: [Row; 3] = [
        custom(),
        ("omni", "omnivoice", "tts", json!({}), omnivoice),
        ("design", "qwen3_tts", "vdes", json!({}), voice_design),
    ];
    let (g, addr, chat) = gateway(&rows, 3, |_| {}).await;
    let cues = cue_hint_text();

    let (mut ws, s) = session(&addr, json!({})).await;
    let speech = &s["lmgw"]["resolved"]["speech"];
    assert_eq!(
        (&speech["cues"], &speech["tag_hint"]),
        (&json!(true), &json!(cues))
    );
    let n = spoken(&g);
    answer(&mut ws, &chat, &["[laughing] Fine."], create()).await;
    let first = chat.seen.chat_count() - 1;
    assert!(
        prompt(&chat, first).ends_with(&format!("\n\n{cues}")),
        "a paragraph after the instructions"
    );
    assert_eq!(each(&g, n, "instructions"), [json!("laughing")]);
    // Off: the prompt says nothing, and the cue still applies.
    let s = update(&mut ws, json!({"tag_hint": false})).await;
    let speech = &s["lmgw"]["resolved"]["speech"];
    assert_eq!(
        (&speech["cues"], &speech["tag_hint"]),
        (&json!(true), &Value::Null)
    );
    let n = spoken(&g);
    answer(&mut ws, &chat, &["[whispering] Fine."], create()).await;
    assert!(!prompt(&chat, chat.seen.chat_count() - 1).contains("square brackets"));
    assert_eq!(each(&g, n, "instructions"), [json!("whispering")]);
    assert_eq!(each(&g, n, "input"), [json!("Fine.")]);

    // OmniVoice renders tags: its sounds are named, and the tag is mapped.
    let (mut ws, s) = session(&addr, json!({"tts_model": "audio/omni"})).await;
    let speech = &s["lmgw"]["resolved"]["speech"];
    assert_eq!(speech["cues"], false);
    let hint = speech["tag_hint"].as_str().unwrap().to_string();
    assert!(hint.contains("can also make these sounds"), "{hint}");
    let n = spoken(&g);
    answer(&mut ws, &chat, &["[laughing] That is funny."], create()).await;
    assert!(prompt(&chat, chat.seen.chat_count() - 1).ends_with(&format!("\n\n{hint}")));
    assert_eq!(each(&g, n, "input"), [json!("[laughter] That is funny.")]);
    assert_eq!(each(&g, n, "instructions"), [Value::Null]);

    // A designed voice: no hint, and its description on every clause.
    let sailor = "an old sailor, hoarse and slow";
    let (mut ws, s) = session(
        &addr,
        json!({"tts_model": "audio/design", "speech_instructions": sailor}),
    )
    .await;
    let speech = &s["lmgw"]["resolved"]["speech"];
    assert_eq!(
        (&speech["cues"], &speech["tag_hint"]),
        (&json!(false), &Value::Null)
    );
    let n = spoken(&g);
    answer(&mut ws, &chat, LAUGH, create()).await;
    assert!(!prompt(&chat, chat.seen.chat_count() - 1).contains("square brackets"));
    assert_eq!(each(&g, n, "instructions"), vec![json!(sailor); 2]);
    assert_eq!(
        each(&g, n, "input"),
        [json!("Oh no, that is funny."), json!("Anyway, it rains.")]
    );
    assert_eq!(g.runs(), ["custom", "omni", "design"]);
}

/// A cloud TTS nobody described gets its style but no cue: gpt-4o-mini-tts
/// ignored every phrasing of one, so the prompt is not told about them and
/// the tag is stripped, never read out.
#[tokio::test]
async fn a_cloud_tts_nobody_described_takes_no_cue() {
    let cloud = tts_fake(&[]).await;
    let (state, addr, chat, _local) = speech_gateway(false, None, |_| {}).await;
    add_cloud_tts_alias(&state, &cloud, "cloud-tts").await;
    let (mut ws, s) = session(
        &addr,
        json!({"tts_model": "cloud-tts", "speech_instructions": "calm"}),
    )
    .await;
    let speech = &s["lmgw"]["resolved"]["speech"];
    assert_eq!(
        (
            &speech["instructions"],
            &speech["cues"],
            &speech["tag_hint"]
        ),
        (&json!("passthrough"), &json!(false), &Value::Null)
    );
    answer(
        &mut ws,
        &chat,
        &["[laughing] That is funny. It rains."],
        create(),
    )
    .await;
    assert!(!prompt(&chat, chat.seen.chat_count() - 1).contains("square brackets"));
    assert_eq!(
        heard(&cloud),
        [
            (json!("calm"), json!("That is funny.")),
            (json!("calm"), json!("It rains.")),
        ]
    );
}

/// A cloud alias the owner declared a style (`capabilities.speech.
/// instructions: "style"`) takes cues: the cue is its instructions on the
/// cued sentence only, and it never gets a seed.
#[tokio::test]
async fn a_cloud_tts_declared_a_style_gets_the_cue_as_its_instructions() {
    let cloud = tts_fake(&[]).await;
    let (state, addr, chat, _local) = speech_gateway(false, None, |_| {}).await;
    add_described_cloud_tts_alias(
        &state,
        &cloud,
        "cloud-tts",
        json!({"instructions": "style"}),
    )
    .await;
    let (mut ws, s) = session(&addr, json!({"tts_model": "cloud-tts"})).await;
    assert_eq!(s["lmgw"]["resolved"]["speech"]["cues"], true);
    answer(
        &mut ws,
        &chat,
        &["[laughing] That is funny. It rains."],
        create(),
    )
    .await;
    assert_eq!(cloud.seen.count(), 2);
    let (cued, plain) = (cloud.seen.body(0), cloud.seen.body(1));
    assert_eq!(cued["instructions"], "laughing", "{cued}");
    assert_eq!(cued["input"], "That is funny.", "{cued}");
    assert!(plain.get("instructions").is_none(), "{plain}");
    assert_eq!(plain["input"], "It rains.", "{plain}");
    for body in [cued, plain] {
        assert!(body.get("seed").is_none(), "{body}");
    }
}

/// The GPU hold on, and `rows` as local TTS rows whose hold fallback is the
/// cloud alias `cloud-tts`, declared a style so that it takes cues: the
/// gateway, the chat fake, the local TTS fake that must hear nothing, the
/// cloud one, and the models directory to keep.
async fn held(
    rows: &[Row],
) -> (
    lmgw_core::state::SharedState,
    String,
    ChatFake,
    TtsFake,
    TtsFake,
    tempfile::TempDir,
) {
    let cloud = tts_fake(&[]).await;
    let models = tempfile::tempdir().unwrap();
    let dir = models.path().display().to_string();
    let (state, addr, chat, local) = speech_gateway(false, None, |s| {
        s.hold.active = true;
        s.realtime.warm_on_connect = false;
        s.audio.models_dir = dir;
    })
    .await;
    add_described_cloud_tts_alias(
        &state,
        &cloud,
        "cloud-tts",
        json!({"instructions": "style"}),
    )
    .await;
    for (id, family, task, defaults, package) in rows {
        let root = models.path().join(id);
        std::fs::create_dir_all(&root).unwrap();
        package(&root);
        let mut row = tts_row(id, family);
        row.task = (*task).into();
        row.default_request_options = defaults.as_object().cloned().unwrap();
        row.hold_fallback_mode = HoldFallbackMode::Alias;
        row.hold_fallback = Some("cloud-tts".into());
        store::insert_audio_model(&state.db, &row).await.unwrap();
    }
    // The session's voice, as `gateway` has it: a library clip.
    let voices = models.path().join("voices");
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::write(voices.join("alba.wav"), wav_bytes(24_000, 480)).unwrap();
    state.reload_snapshot().await.unwrap();
    (state, addr, chat, local, cloud, models)
}

/// `(instructions, input)` of every body the cloud fake got.
fn heard(cloud: &TtsFake) -> Vec<(Value, Value)> {
    (0..cloud.seen.count())
        .map(|n| {
            let b = cloud.seen.body(n);
            (b["instructions"].clone(), b["input"].clone())
        })
        .collect()
}

/// Under the GPU hold a cloud fallback answers, and the cue is decided on
/// it: it reaches the fallback as instructions — after a CustomVoice row's
/// style, and after a voice-design row's description too, which on the
/// row itself never takes one.
#[tokio::test]
async fn under_the_gpu_hold_a_cloud_fallback_keeps_the_cue() {
    let rows: [Row; 2] = [
        custom(),
        ("design", "qwen3_tts", "vdes", json!({}), voice_design),
    ];
    let (state, addr, chat, local, cloud, _models) = held(&rows).await;

    let words = &["[laughing] That is funny. It rains."];
    let (mut ws, s) = session(
        &addr,
        json!({"tts_model": "audio/custom", "speech_instructions": "calm"}),
    )
    .await;
    assert_eq!(s["lmgw"]["resolved"]["speech"]["cues"], true);
    answer(&mut ws, &chat, words, create()).await;
    let sailor = "an old sailor, hoarse and slow";
    let (mut ws, s) = session(
        &addr,
        json!({"tts_model": "audio/design", "speech_instructions": sailor}),
    )
    .await;
    assert_eq!(
        s["lmgw"]["resolved"]["speech"]["cues"], false,
        "the primary's"
    );
    answer(&mut ws, &chat, words, create()).await;

    assert_eq!(local.seen.count(), 0);
    assert_eq!(
        heard(&cloud),
        [
            (
                json!("calm, but laughing right now"),
                json!("That is funny.")
            ),
            (json!("calm"), json!("It rains.")),
            (
                json!(format!("{sailor}, but laughing right now")),
                json!("That is funny.")
            ),
            (json!(sailor), json!("It rains.")),
        ]
    );
    assert!(state.runtime().list().is_empty(), "nothing started");
}

/// A row's own description is not sent (the engine merges it in), so it
/// stays with the row: a cloud fallback under the GPU hold gets the bare
/// cue, after nothing — for a CustomVoice row's style and a voice-design
/// row's description alike. Taking the primary's row for the base would
/// put "a narrator, but laughing right now" on the wire to the cloud.
#[tokio::test]
async fn under_the_gpu_hold_a_row_s_own_description_stays_with_the_row() {
    let sailor = "an old sailor, hoarse and slow";
    let rows: [Row; 2] = [
        (
            "narrated",
            "qwen3_tts",
            "tts",
            json!({"instruct": "a narrator"}),
            custom_voice,
        ),
        (
            "designed",
            "qwen3_tts",
            "vdes",
            json!({"instruct": sailor}),
            voice_design,
        ),
    ];
    let (state, addr, chat, local, cloud, _models) = held(&rows).await;
    let words = &["[laughing] That is funny. It rains."];
    for (alias, description) in [("audio/narrated", "a narrator"), ("audio/designed", sailor)] {
        let (mut ws, s) = session(&addr, json!({"tts_model": alias})).await;
        let speech = &s["lmgw"]["resolved"]["speech"];
        assert_eq!(
            (&speech["text"], &speech["source"]),
            (&json!(description), &json!("row")),
            "{alias}"
        );
        answer(&mut ws, &chat, words, create()).await;
    }
    assert_eq!(local.seen.count(), 0);
    let bare = [
        (json!("laughing"), json!("That is funny.")),
        (Value::Null, json!("It rains.")),
    ];
    assert_eq!(heard(&cloud), [bare.clone(), bare].concat());
    assert!(state.runtime().list().is_empty(), "nothing started");
}

/// The owner's override `capabilities.speech.instructions: "none"` on a
/// cloud alias is C5's off switch: no cue, nothing sent as instructions,
/// and the tag stripped as for any TTS that renders none.
#[tokio::test]
async fn a_cloud_tts_that_reads_no_instructions_takes_no_cue() {
    let cloud = tts_fake(&[]).await;
    let (state, addr, chat, _local) = speech_gateway(false, None, |_| {}).await;
    add_described_cloud_tts_alias(&state, &cloud, "plain-tts", json!({"instructions": "none"}))
        .await;
    let (mut ws, s) = session(&addr, json!({"tts_model": "plain-tts"})).await;
    let speech = &s["lmgw"]["resolved"]["speech"];
    assert_eq!(
        (&speech["cues"], &speech["tag_hint"]),
        (&json!(false), &Value::Null)
    );
    answer(
        &mut ws,
        &chat,
        &["[laughing] That is funny. It rains."],
        create(),
    )
    .await;
    assert!(!prompt(&chat, chat.seen.chat_count() - 1).contains("square brackets"));
    assert_eq!(
        heard(&cloud),
        [
            (Value::Null, json!("That is funny.")),
            (Value::Null, json!("It rains.")),
        ]
    );
}

/// A local passthrough row that renders no tags — Auk, which reads only
/// `instruct` — takes cues as CustomVoice does: the cue goes where the
/// family reads instructions, and the tag is not read out.
#[tokio::test]
async fn a_local_passthrough_row_without_tags_takes_cues() {
    fn auk(r: &std::path::Path) {
        crate::support::audiocpp_gguf::with_options(r, "auk", &["instruct"], &["offline"])
    }
    let row: Row = ("auk", "auk", "tts", json!({}), auk);
    let (g, addr, chat) = gateway(&[row], 1, |_| {}).await;
    let (mut ws, s) = session(&addr, json!({})).await;
    let speech = &s["lmgw"]["resolved"]["speech"];
    assert_eq!(
        (&speech["instructions"], &speech["tags"], &speech["cues"]),
        (&json!("passthrough"), &json!("none"), &json!(true))
    );
    assert_eq!(speech["tag_hint"], json!(cue_hint_text()));
    answer(&mut ws, &chat, &["[laughing] Oh no. It rains."], create()).await;
    assert_eq!(
        each(&g, 0, "options"),
        [json!({"instruct": "laughing"}), Value::Null]
    );
    assert_eq!(each(&g, 0, "instructions"), [Value::Null, Value::Null]);
    assert_eq!(each(&g, 0, "input"), [json!("Oh no."), json!("It rains.")]);
    assert!(g.world().refused_speech.is_empty());
}

/// A clause of nothing but a cue rides the carry (WP10 D10) into the next
/// clause, whose cue it is; one at the end of the answer is not sent, and
/// the history keeps both.
///
/// Such a clause is a stage direction written as a word ("(laughs)"): a
/// bracketed tag alone is no clause — a sentence end or a line end cuts only
/// after a word, so "[whispering], it is a secret." is one clause that opens
/// with its cue, and needs no carry.
#[tokio::test]
async fn a_cue_rides_the_carry_and_one_at_the_end_is_not_sent() {
    let (g, addr, chat) = gateway(&[custom()], 1, |_| {}).await;
    let (mut ws, _) = session(&addr, json!({})).await;
    let written = "(laughs). It is a secret. Okay. [laughing]";
    let events = answer(&mut ws, &chat, &[written], create()).await;
    assert_eq!(each(&g, 0, "instructions"), [json!("laughs"), Value::Null]);
    assert_eq!(
        each(&g, 0, "input"),
        [json!("It is a secret."), json!("Okay.")]
    );
    assert_eq!(
        transcripts(&events).last().unwrap(),
        "It is a secret. Okay."
    );
    assert_eq!(history(&mut ws, &chat).await, json!(written));
}
