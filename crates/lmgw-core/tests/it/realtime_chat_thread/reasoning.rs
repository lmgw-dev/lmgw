//! A voice turn's reasoning (§8.5): the off it asks for is fitted to a cloud
//! model (model-capabilities design §5.6) and never ends in an error — a
//! model that refuses every form of off answers with its default reasoning
//! (the owner's ruling of 2026-10-04) — and reasoning is never spoken: the
//! TTS gets the reply's text alone, whatever the model reasons at, while the
//! reasoning is relayed to the page, stored with the reply, kept whole by a
//! cut, and said once on the status line (`done.reasoning_note`).

use serde_json::{json, Value};

use super::{frames, of_type, say, until_type, world, world_on};
use crate::chat_voice_dictation::tweak;
use crate::support::gpu_world::{Gpu, ANSWER, GIB, THOUGHT};
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::{send, Step, Turn};
use crate::support::realtime_tts::{speech, wav};

/// A refusal as the OpenAI API words it.
fn refusal(message: &str) -> Turn {
    Turn::Status(
        400,
        json!({"error": {"message": message, "type": "invalid_request_error", "param": null}}),
    )
}

/// Every text the TTS was asked to say, in order.
fn spoken(w: &super::World) -> Vec<String> {
    (0..w.tts.seen.count())
        .map(|i| w.tts.seen.body(i)["input"].as_str().unwrap().to_string())
        .collect()
}

/// The data of the turn's `lmgw.chat.frame` events named `event`.
fn frame_data<'a>(events: &'a [Value], event: &str) -> Vec<&'a Value> {
    of_type(events, "lmgw.chat.frame")
        .into_iter()
        .filter(|e| e["event"] == event)
        .map(|e| &e["data"])
        .collect()
}

/// The thread's last message as stored: `(content, reasoning, voice)`.
async fn last_stored(w: &super::World, tid: i64) -> (String, String, Value) {
    let t = w.get(&format!("/chat/api/threads/{tid}")).await;
    let m = t["messages"].as_array().unwrap().last().unwrap().clone();
    (
        m["content"].as_str().unwrap().to_string(),
        m["reasoning"].as_str().unwrap().to_string(),
        m["voice"].clone(),
    )
}

#[tokio::test]
async fn a_voice_turn_answers_on_a_model_without_reasoning() {
    let w = world(|_| {}).await;
    let tid = w.thread("other", json!({})).await;
    let mut ws = w.voice(tid).await;
    // `gpt-4.1-nano`, as probed: the off's `reasoning_effort` refused by name.
    w.asr.push(Asr::Text("Frage."));
    w.chat.push(refusal(
        "Unrecognized request argument supplied: reasoning_effort",
    ));
    w.chat.push(Turn::text(&["Antwort."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    assert_eq!(
        of_type(&events, "response.done")[0]["response"]["status"],
        "completed",
        "{events:?}"
    );
    assert_eq!(w.messages(tid).await.last().unwrap().1, "Antwort.");
    assert_eq!(w.chat.seen.chat_count(), 2);
    assert_eq!(w.chat.seen.chat(0)["reasoning_effort"], "none");
    assert!(w.chat.seen.chat(1).get("reasoning_effort").is_none());
    let rows: Vec<(i64, Option<String>)> = sqlx::query_as(
        "SELECT status, error_msg FROM request_logs WHERE class = 'chat' ORDER BY id",
    )
    .fetch_all(&w.state.db)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].0, 400);
    assert!(
        rows[0]
            .1
            .as_deref()
            .unwrap()
            .contains("retried once with no reasoning control"),
        "{rows:?}"
    );
    assert_eq!(rows[1].0, 200);
    // A model that takes no control and shows no reasoning: nothing to say.
    let done = frame_data(&events, "done");
    assert_eq!(done[0]["reasoning_note"], Value::Null, "{done:?}");

    // The next voice turn sends no control at once.
    w.asr.push(Asr::Text("Noch eine."));
    w.chat.push(Turn::text(&["Gern."]));
    say(&mut ws).await;
    until_type(&mut ws, "lmgw.response.timing").await;
    assert_eq!(w.chat.seen.chat_count(), 3);
    assert!(w.chat.seen.chat(2).get("reasoning_effort").is_none());
}

/// A model that refuses the off, then the level its refusal named, answers
/// with no reasoning control: it reasons, nothing of that is spoken, and the
/// turn says so — no error.
#[tokio::test]
async fn a_model_refusing_every_off_reasons_and_is_heard_without_it() {
    let w = world(|_| {}).await;
    let tid = w.thread("other", json!({})).await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Wie ist das Wetter?"));
    w.chat.push(refusal(
        "Unsupported value: 'reasoning_effort' does not support 'none' with this model. \
         Supported values are: 'minimal', 'low', 'medium', and 'high'.",
    ));
    w.chat.push(refusal(
        "Unsupported value: 'reasoning_effort' does not support 'minimal' with this model.",
    ));
    w.chat.push(Turn::reasoned(
        &["Kurz nachdenken: ", "Sonne laut Bericht."],
        &["Es ist sonnig. ", "Und warm."],
    ));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    assert_eq!(
        of_type(&events, "response.done")[0]["response"]["status"],
        "completed",
        "{events:?}"
    );
    assert!(of_type(&events, "error").is_empty(), "{events:?}");

    // Three sends: the off, the level the refusal named, no control.
    assert_eq!(w.chat.seen.chat_count(), 3);
    assert_eq!(w.chat.seen.chat(0)["reasoning_effort"], "none");
    assert_eq!(w.chat.seen.chat(1)["reasoning_effort"], "minimal");
    assert!(w.chat.seen.chat(2).get("reasoning_effort").is_none());

    // Relayed for the bubble, before the reply, and said once.
    let names = frames(&events);
    let first_reasoning = names.iter().position(|n| n == "reasoning").unwrap();
    let first_delta = names.iter().position(|n| n == "delta").unwrap();
    assert!(first_reasoning < first_delta, "{names:?}");
    let done = frame_data(&events, "done");
    assert_eq!(done[0]["reasoning_ignored"], json!(["enabled"]), "{done:?}");
    assert_eq!(
        done[0]["reasoning_note"],
        "other refused every way lmgw has to switch reasoning off; it reasons as it does by \
         default"
    );

    // Never spoken.
    let said = spoken(&w);
    assert_eq!(said.join(" "), "Es ist sonnig. Und warm.", "{said:?}");
    assert!(said.iter().all(|s| !s.contains("nachdenken")), "{said:?}");
    let transcript: String = of_type(&events, "response.output_audio_transcript.delta")
        .iter()
        .map(|e| e["delta"].as_str().unwrap())
        .collect();
    assert!(!transcript.contains("Bericht"), "{transcript}");

    // Stored with the reply, and timed.
    let (content, reasoning, voice) = last_stored(&w, tid).await;
    assert_eq!(content, "Es ist sonnig. Und warm.");
    assert_eq!(reasoning, "Kurz nachdenken: Sonne laut Bericht.");
    assert!(voice["timing"]["reasoning_ms"].is_u64(), "{voice}");
    let timing = of_type(&events, "lmgw.response.timing")[0];
    assert!(timing["reasoning_ms"].is_u64(), "{timing}");

    // Remembered: the next turn goes out with no control at once, and says
    // the same.
    w.asr.push(Asr::Text("Und morgen?"));
    w.chat.push(Turn::text(&["Auch."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    assert_eq!(w.chat.seen.chat_count(), 4);
    assert!(w.chat.seen.chat(3).get("reasoning_effort").is_none());
    let done = frame_data(&events, "done");
    assert!(
        done[0]["reasoning_note"]
            .as_str()
            .unwrap()
            .starts_with("other refused every way"),
        "{done:?}"
    );
}

/// The thread switches reasoning on: the request asks no off, and the TTS
/// still gets only the reply.
#[tokio::test]
async fn reasoning_switched_on_in_the_thread_is_shown_never_spoken() {
    let w = world(|_| {}).await;
    let tid = w.thread("other", json!({})).await;
    w.set(tid, json!({"reasoning_enabled": true})).await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Hallo?"));
    w.chat.push(Turn::reasoned(
        &["Erst nachdenken. ", "Dann reden."],
        &["Hallo. ", "Wie geht es dir?"],
    ));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    assert_ne!(w.chat.seen.chat(0)["reasoning_effort"], "none");
    assert_eq!(spoken(&w).join(" "), "Hallo. Wie geht es dir?");
    let reasoning: String = frame_data(&events, "reasoning")
        .iter()
        .map(|d| d["text"].as_str().unwrap())
        .collect();
    assert_eq!(reasoning, "Erst nachdenken. Dann reden.");
    // No off was asked: nothing to say about one.
    assert_eq!(
        frame_data(&events, "done")[0]["reasoning_note"],
        Value::Null
    );
    let (content, stored, _) = last_stored(&w, tid).await;
    assert_eq!(
        (content.as_str(), stored.as_str()),
        ("Hallo. Wie geht es dir?", "Erst nachdenken. Dann reden.")
    );
}

/// A cut keeps the reasoning whole — it was never spoken, so nothing of it
/// was heard or unheard — and the next turn is told the heard text as the
/// reply, the reasoning beside it as a text turn's is.
#[tokio::test]
async fn a_cut_leaves_the_reasoning_whole() {
    let w = world(|_| {}).await;
    // One clause, one second of audio.
    w.tts.set_default(wav(&speech(1000), 24_000));
    let tid = w.thread("other", json!({})).await;
    w.set(tid, json!({"reasoning_enabled": true})).await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Zähl bitte."));
    w.chat
        .push(Turn::reasoned(&["Drei Zahlen."], &["Eins. Zwei. Drei."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let item = of_type(&events, "response.output_item.added")
        .iter()
        .find(|e| e["item"]["type"] == "message")
        .map(|e| e["item"]["id"].as_str().unwrap().to_string())
        .unwrap();
    // The page stopped playback after the first clause.
    send(
        &mut ws,
        json!({"type": "conversation.item.truncate", "item_id": item, "content_index": 0,
               "audio_end_ms": 1000}),
    )
    .await;
    let ev = until_type(&mut ws, "lmgw.chat.reply").await;
    assert_eq!(ev.last().unwrap()["content"], "Eins.", "{ev:?}");
    let (content, reasoning, voice) = last_stored(&w, tid).await;
    assert_eq!(content, "Eins.");
    assert_eq!(voice["unheard"], "Zwei. Drei.");
    assert_eq!(reasoning, "Drei Zahlen.");

    w.asr.push(Asr::Text("Weiter."));
    w.chat.push(Turn::text(&["Vier."]));
    say(&mut ws).await;
    until_type(&mut ws, "lmgw.response.timing").await;
    let body = w.chat.seen.chat(1);
    let reply = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap()
        .clone();
    assert_eq!(reply["content"], "Eins.", "{reply}");
    assert_eq!(reply["reasoning_content"], "Drei Zahlen.", "{reply}");
}

/// A local row whose template reasons whatever it is told: the off reaches
/// llama-server as the template's own switch, nothing errors, the model
/// reasons anyway — stored, shown, reported, and none of it spoken.
#[tokio::test]
async fn a_local_template_that_reasons_anyway_is_heard_without_it() {
    let g = Gpu::new(10 * GIB, 2, 30).await;
    let models = g.models_dir().display().to_string();
    tweak(&g.state, |s| s.audio.models_dir = models).await;
    g.model("talk", 4 * GIB).await;
    g.world().thinking.insert("talk".into());
    let w = world_on(g.state.clone(), |_| {}).await;
    let tid = w.thread("talk", json!({})).await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Wie ist das Wetter?"));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    assert_eq!(
        of_type(&events, "response.done")[0]["response"]["status"],
        "completed",
        "{events:?}"
    );

    // The template's off, and no level beside it.
    let sent = g.world().streamed_bodies.clone();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        sent[0]["chat_template_kwargs"]["enable_thinking"], false,
        "{}",
        sent[0]
    );
    assert!(sent[0].get("reasoning_effort").is_none(), "{}", sent[0]);

    let done = frame_data(&events, "done");
    assert_eq!(done[0]["reasoning_ignored"], json!(["enabled"]), "{done:?}");
    assert_eq!(
        done[0]["reasoning_note"],
        "talk did not switch reasoning off; it reasoned anyway"
    );
    let reasoning: String = frame_data(&events, "reasoning")
        .iter()
        .map(|d| d["text"].as_str().unwrap())
        .collect();
    assert_eq!(reasoning, THOUGHT);
    assert_eq!(spoken(&w).join(" "), ANSWER);
    let (content, stored, _) = last_stored(&w, tid).await;
    assert_eq!((content.as_str(), stored.as_str()), (ANSWER, THOUGHT));
}

/// A thread with tools runs its voice turn through the tool loop: the same
/// fit there (a refused off retried, then remembered for the loop's next
/// call), reasoning before the preamble and before the answer relayed and
/// stored, and only the preamble and the answer spoken.
#[tokio::test]
async fn a_tool_turn_speaks_neither_call_s_reasoning() {
    let w = world(|_| {}).await;
    let url = crate::chat_golden::mcp_stub(std::time::Duration::ZERO).await;
    crate::chat_golden::register_stub(&w.state, &url).await;
    let tid = w.thread("chatty", json!({})).await;
    w.set(tid, json!({"mcp_tools": [{"server_label": "stub"}]}))
        .await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Sag hallo."));
    w.chat.push(refusal(
        "Unrecognized request argument supplied: reasoning_effort",
    ));
    w.chat.push(Turn::Stream(vec![
        Step::Reasoning("Ich rufe das Werkzeug. "),
        Step::Text("Moment. "),
        Step::CallStart {
            index: 0,
            id: Some("call_1"),
            name: "stub__echo",
        },
        Step::CallArgs {
            index: 0,
            args: "{\"text\":\"hallo\"}",
        },
        Step::Finish("tool_calls"),
    ]));
    w.chat
        .push(Turn::reasoned(&["Jetzt antworten."], &["Hallo!"]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    assert!(of_type(&events, "error").is_empty(), "{events:?}");
    assert_eq!(w.chat.seen.chat_count(), 3);
    assert_eq!(w.chat.seen.chat(0)["reasoning_effort"], "none");
    for n in [1, 2] {
        assert!(
            w.chat.seen.chat(n).get("reasoning_effort").is_none(),
            "{}",
            w.chat.seen.chat(n)
        );
    }
    assert_eq!(spoken(&w), ["Moment.", "Hallo!"]);
    let done = frame_data(&events, "done");
    assert_eq!(done[0]["reasoning_ignored"], json!(["enabled"]), "{done:?}");
    assert_eq!(
        done[0]["reasoning_note"],
        "chatty did not switch reasoning off; it reasoned anyway"
    );
    let (content, reasoning, _) = last_stored(&w, tid).await;
    assert_eq!(content, "Moment. Hallo!");
    assert_eq!(reasoning, "Ich rufe das Werkzeug. Jetzt antworten.");
}
