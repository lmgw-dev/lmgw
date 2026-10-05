//! A bound turn (§8.2, §8.5, §8.7): the user row with how it was spoken,
//! the reply through the chat engine with the thread's model, sampling and
//! prompt — the voice block after it — knowledge and tools, every frame
//! relayed and no tool call as a `function_call` item; `empty_turn`, a
//! superseded turn, a failed one merged with the next, a temporary thread,
//! and the extension events for a bound session only.

use std::sync::Arc;

use lmgw_core::config::{VOICE_FOLLOWS_USER, VOICE_FORM, VOICE_NO_DATE, VOICE_PERSONA};
use serde_json::{json, Value};
use tokio::sync::Notify;

use super::{frames, of_type, say, until, until_type, world, world_on};
use crate::common::captured_log::capture_log;
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::{next_event, send, user_text, Step, Turn};
use crate::support::realtime_tts::{add_described_cloud_tts_alias, speech, wav, Tts};

/// The thread's language in a voice turn's prompt (`w.thread` gives every
/// thread `de`).
const SPOKEN_GERMAN: &str = "The user speaks German and hears your reply in a German voice, so \
                             answer in German unless the user asks for another language.";

#[tokio::test]
async fn a_spoken_turn_is_the_thread_s_own_turn() {
    let w = world(|_| {}).await;
    // A TTS that takes delivery cues: its hint follows the voice block.
    add_described_cloud_tts_alias(
        &w.state,
        &w.tts,
        "cloud-tts",
        json!({"instructions": "style"}),
    )
    .await;
    let tid = w.thread("other", json!({})).await;
    w.set(
        tid,
        json!({"system_prompt": "Du bist ein Assistent.", "temperature": 0.3,
               "voice": {"tts_alias": "cloud-tts", "language": "de"}}),
    )
    .await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Wie spät ist es?"));
    w.chat.push(Turn::text(&["Es ist ", "drei Uhr."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;

    // The user row, and how it was spoken.
    let m = w.messages(tid).await;
    assert_eq!(m.len(), 2, "{m:?}");
    let (role, content, voice) = &m[0];
    assert_eq!(
        (role.as_str(), content.as_str()),
        ("user", "Wie spät ist es?")
    );
    assert_eq!(voice["via"], "realtime");
    assert_eq!(voice["asr"], "hear");
    assert_eq!(voice["audio_ms"], 200);
    assert!(voice["asr_ms"].is_u64(), "{voice}");
    // The reply: heard whole, annotated.
    let (role, content, voice) = &m[1];
    assert_eq!(
        (role.as_str(), content.as_str()),
        ("assistant", "Es ist drei Uhr.")
    );
    assert_eq!(voice["via"], "realtime");
    assert_eq!(voice["tts"], "cloud-tts");
    assert!(voice.get("unheard").is_none(), "{voice}");
    assert!(voice["timing"]["first_token_ms"].is_u64(), "{voice}");

    // The chat engine's turn: the thread's model and sampling, and its
    // prompt with the voice block after it — the thread's language in place
    // of "answer in the language the user speaks" — then the hint.
    let body = w.chat.seen.chat(0);
    assert_eq!(body["temperature"], 0.3, "{body}");
    let system = body["messages"][0]["content"].as_str().unwrap().to_string();
    assert!(
        system.starts_with("Du bist ein Assistent.\n\nThis reply is spoken aloud."),
        "{system}"
    );
    let style = system.find(VOICE_FORM).expect("the voice style");
    let language = system
        .find(SPOKEN_GERMAN)
        .expect("the thread's language (2026-10-04)");
    let date = system.find(VOICE_NO_DATE).expect("the date line, kept");
    let hint = system
        .find(&lmgw_api_types::realtime::cue_hint_text())
        .expect("the hint");
    assert!(
        style < language && language < date && date < hint,
        "{system}"
    );
    assert!(!system.contains(VOICE_FOLLOWS_USER), "{system}");
    assert!(!system.contains(VOICE_PERSONA), "{system}");
    assert_eq!(body["messages"][1]["content"], "Wie spät ist es?");
    // The thread sets no reasoning: a voice turn asks for none (§8.5).
    assert_eq!(body["reasoning_effort"], "none", "{body}");
    let row: (String, String) = sqlx::query_as(
        "SELECT ingress_proto, requested_alias FROM request_logs WHERE class = 'chat'",
    )
    .fetch_one(&w.state.db)
    .await
    .unwrap();
    assert_eq!(row, ("chat".to_string(), "other".to_string()));

    // The extension events: the user message, the turn's frames, the reply.
    let user = of_type(&events, "lmgw.chat.user");
    assert_eq!(user.len(), 1);
    assert_eq!(user[0]["content"], "Wie spät ist es?");
    let f = frames(&events);
    assert_eq!(f.first().map(String::as_str), Some("turn"), "{f:?}");
    assert!(f.contains(&"delta".to_string()) && f.last().map(String::as_str) == Some("done"));
    let rid = of_type(&events, "response.created")[0]["response"]["id"].clone();
    assert!(of_type(&events, "lmgw.chat.frame")
        .iter()
        .all(|e| e["response_id"] == rid));
    let reply = of_type(&events, "lmgw.chat.reply");
    assert_eq!(reply.len(), 1);
    assert_eq!(reply[0]["content"], "Es ist drei Uhr.");
    assert_eq!(reply[0]["unheard"], Value::Null);
    let timing = of_type(&events, "lmgw.response.timing")[0];
    assert_eq!(timing["response_id"], rid);
    assert_eq!(timing["message_id"], reply[0]["message_id"]);
    assert_eq!(timing["models"]["asr"]["alias"], "hear");
    assert_eq!(timing["models"]["chat"]["alias"], "other");
    assert_eq!(timing["models"]["tts"]["alias"], "cloud-tts");
    // And the conversation's own events as realtime sends them.
    assert_eq!(
        of_type(&events, "response.done")[0]["response"]["status"],
        "completed"
    );
}

/// WP8 review m8: the owner's reasoning ruling — a voice turn thinks only
/// when its thread says so, and an explicit setting wins — and a
/// text-output session's turns are plain chat turns: no voice block, the
/// thread's own reasoning; the thread's language still (2026-10-04), since
/// the user spoke.
#[tokio::test]
async fn a_voice_turn_thinks_only_when_the_thread_says_so() {
    let w = world(|_| {}).await;
    let plain = w.thread("chatty", json!({})).await;
    w.set(plain, json!({"system_prompt": "Du bist ein Assistent."}))
        .await;
    let thinking = w.thread("chatty", json!({})).await;
    w.set(
        thinking,
        json!({"reasoning_enabled": true, "reasoning_effort": "high"}),
    )
    .await;
    for (tid, effort) in [(plain, "none"), (thinking, "high")] {
        let mut ws = w.voice(tid).await;
        w.asr.push(Asr::Text("Frage."));
        w.chat.push(Turn::text(&["Antwort."]));
        say(&mut ws).await;
        until_type(&mut ws, "lmgw.response.timing").await;
        let body = w.chat.seen.chat(w.chat.seen.chat_count() - 1);
        assert_eq!(body["reasoning_effort"], effort, "{body}");
    }
    // Text output: the thread's prompt and its language, no voice block,
    // and no reasoning asked for.
    let (mut ws, _) = super::World::bind(&w, plain).await;
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
            "output_modalities": ["text"], "audio": {"input": {"turn_detection": null}}}}),
    )
    .await;
    assert_eq!(super::next(&mut ws).await["type"], "session.updated");
    w.asr.push(Asr::Text("Und als Text?"));
    w.chat.push(Turn::text(&["Als Text."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    assert!(
        of_type(&events, "response.output_audio.delta").is_empty(),
        "{events:?}"
    );
    let body = w.chat.seen.chat(w.chat.seen.chat_count() - 1);
    assert_eq!(
        body["messages"][0]["content"],
        "Du bist ein Assistent.\n\nThe user speaks German, so answer in German unless the user asks for \
         another language.",
        "{body}"
    );
    assert_eq!(
        of_type(&events, "response.done")[0]["response"]["status"],
        "completed",
        "the WP8 build stopped a text-output turn at once: {events:?}"
    );
    assert_eq!(w.messages(plain).await.last().unwrap().1, "Als Text.");
    assert!(body.get("reasoning_effort").is_none(), "{body}");
}

#[tokio::test]
async fn kb_context_lands_on_the_spoken_user_row() {
    let kb_mock = wiremock::MockServer::start().await;
    crate::knowledge::mount(&kb_mock).await;
    let state = crate::knowledge::setup(&kb_mock).await;
    let w = world_on(state, |_| {}).await;
    let taxes = crate::knowledge::ingested(
        &w.state,
        &w.gw,
        json!({"name": "Taxes", "embed_alias": "embed-model"}),
        &[("notes.md", crate::knowledge::NOTES.as_bytes().to_vec())],
    )
    .await;
    let tid = w.thread("chatty", json!({})).await;
    w.set(tid, json!({"kb_ids": [taxes]})).await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("When did the refund arrive?"));
    w.chat.push(Turn::text(&["In May."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    assert!(
        frames(&events).contains(&"retrieval".to_string()),
        "{events:?}"
    );
    let t = w.get(&format!("/chat/api/threads/{tid}")).await;
    let user = &t["messages"][0];
    assert_eq!(user["voice"]["via"], "realtime");
    assert!(
        user["context"]["excerpts"]
            .as_array()
            .is_some_and(|e| !e.is_empty()),
        "{user}"
    );
    let sent = w.chat.seen.chat(0)["messages"][1]["content"].to_string();
    assert!(sent.contains("412 EUR"), "{sent}");
}

#[tokio::test]
async fn mcp_tool_frames_are_relayed_and_never_a_function_call() {
    let w = world(|_| {}).await;
    let url = crate::chat_golden::mcp_stub(std::time::Duration::ZERO).await;
    crate::chat_golden::register_stub(&w.state, &url).await;
    let tid = w.thread("chatty", json!({})).await;
    w.set(tid, json!({"mcp_tools": [{"server_label": "stub"}]}))
        .await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Sag hallo."));
    w.chat.push(Turn::Stream(vec![
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
    w.chat.push(Turn::text(&["Hallo!"]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let tools: Vec<&str> = of_type(&events, "lmgw.chat.frame")
        .iter()
        .filter(|e| e["event"] == "tool")
        .map(|e| e["data"]["event"].as_str().unwrap())
        .collect();
    assert!(
        tools.contains(&"start") && tools.contains(&"result"),
        "{tools:?}"
    );
    for e in &events {
        assert_ne!(e["item"]["type"], "function_call", "{e}");
        assert!(
            !e["type"]
                .as_str()
                .unwrap()
                .starts_with("response.function_call"),
            "{e}"
        );
    }
    // The preamble was spoken before the tool ran; the reply keeps its
    // record.
    let first = w.tts.seen.body(0);
    assert_eq!(first["input"], "Moment.", "{first}");
    let m = w.messages(tid).await;
    assert_eq!(m[1].1, "Moment. Hallo!");
    let record: Option<String> =
        sqlx::query_scalar("SELECT ir_messages FROM chat_messages WHERE role = 'assistant'")
            .fetch_one(&w.state.db)
            .await
            .unwrap();
    assert!(record.is_some_and(|r| r.contains("stub__echo")));
    // The tool loop says its counts in the turn's `done` frame alone: they
    // are the response's usage (WP11 server review m1).
    let done = of_type(&events, "response.done");
    let usage = &done[0]["response"]["usage"];
    assert_eq!(
        (
            &usage["input_tokens"],
            &usage["output_tokens"],
            &usage["total_tokens"]
        ),
        (&json!(12), &json!(8), &json!(20)),
        "{usage}"
    );
}

#[tokio::test]
async fn a_response_that_answers_nothing_is_refused_empty_turn() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    // Push-to-talk pressed without a word.
    w.asr.push(Asr::Text(""));
    say(&mut ws).await;
    let events = until(&mut ws, |e| {
        e["type"] == "error" || e["type"] == "response.done"
    })
    .await;
    let last = events.last().unwrap();
    let e = match last["type"].as_str() {
        Some("error") => last.clone(),
        _ => of_type(&events, "error")[0].clone(),
    };
    assert_eq!(e["error"]["code"], "empty_turn", "{events:?}");
    assert_eq!(w.chat.seen.chat_count(), 0, "no model call");
    assert!(w.messages(tid).await.is_empty(), "nothing written");
    // With the transcript in already, before response.created.
    w.asr.push(Asr::Text(""));
    super::append(&mut ws, &super::silence(200)).await;
    send(&mut ws, json!({"type": "input_audio_buffer.commit"})).await;
    until_type(&mut ws, "conversation.item.done").await;
    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "c2"}),
    )
    .await;
    let e = super::next(&mut ws).await;
    assert_eq!(e["type"], "error", "{e}");
    assert_eq!(e["error"]["code"], "empty_turn");
    assert_eq!(e["error"]["event_id"], "c2");
}

#[tokio::test]
async fn a_text_turn_from_another_window_supersedes_the_voice_turn() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    let hold = Arc::new(Notify::new());
    w.asr.push(Asr::Text("Erzähl was."));
    w.chat.push(Turn::Stream(vec![
        Step::Text("Es war einmal"),
        Step::Wait(hold.clone()),
        Step::Text(" ein König."),
        Step::Finish("stop"),
    ]));
    say(&mut ws).await;
    until(&mut ws, |e| {
        e["type"] == "lmgw.chat.frame" && e["event"] == "delta"
    })
    .await;
    // The other window sends text.
    w.chat.push(Turn::text(&["Getippt."]));
    let r = w
        .post(
            &format!("/chat/api/threads/{tid}/send"),
            json!({"content": "Tippe ich."}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let _ = r.text().await;
    let events = until_type(&mut ws, "response.done").await;
    let e = of_type(&events, "error");
    assert_eq!(e[0]["error"]["code"], "superseded", "{events:?}");
    assert_eq!(
        of_type(&events, "response.done")[0]["response"]["status"],
        "failed"
    );
    // The text turn is saved; the voice reply is not.
    let m = w.messages(tid).await;
    let contents: Vec<&str> = m.iter().map(|(_, c, _)| c.as_str()).collect();
    assert_eq!(contents, ["Erzähl was.", "Tippe ich.", "Getippt."], "{m:?}");
    // The session stays open: the next spoken turn is answered.
    w.asr.push(Asr::Text("Und jetzt?"));
    w.chat.push(Turn::text(&["Jetzt."]));
    say(&mut ws).await;
    super::until_reply(&mut ws, "Jetzt.").await;
    assert_eq!(w.messages(tid).await.last().unwrap().1, "Jetzt.");
}

/// WP8 review m1: a superseded turn's reply is not saved, so what it
/// generated is not said on — the speaker stops at the turn's error, and
/// no audio follows it.
#[tokio::test]
async fn a_superseded_turn_stops_speaking_at_once() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    let (chat_hold, tts_hold) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    // The first clause is said; the second's synthesis is still running
    // when the other window sends, and two more clauses wait behind it.
    w.tts.push(Tts::Wav(wav(&speech(300), 24_000)));
    w.tts
        .push(Tts::Held(tts_hold.clone(), wav(&speech(300), 24_000)));
    w.asr.push(Asr::Text("Erzähl was."));
    w.chat.push(Turn::Stream(vec![
        Step::Text("Eins. "),
        Step::Text("Zwei. "),
        Step::Text("Drei. "),
        Step::Text("Vier. "),
        Step::Wait(chat_hold.clone()),
        Step::Text("Fünf."),
        Step::Finish("stop"),
    ]));
    say(&mut ws).await;
    until(&mut ws, |e| e["type"] == "response.output_audio.delta").await;
    super::eventually("the second clause to be synthesized", || async {
        w.tts.seen.count() == 2
    })
    .await;
    w.chat.push(Turn::text(&["Getippt."]));
    let r = w
        .post(
            &format!("/chat/api/threads/{tid}/send"),
            json!({"content": "Tippe ich."}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let _ = r.text().await;
    until(&mut ws, |e| {
        e["type"] == "lmgw.chat.frame" && e["event"] == "error"
    })
    .await;
    // The held synthesis may answer now: nobody is listening for it.
    tts_hold.notify_one();
    let after = until_type(&mut ws, "response.done").await;
    assert!(
        of_type(&after, "response.output_audio.delta").is_empty(),
        "audio after the turn was superseded: {after:?}"
    );
    assert_eq!(of_type(&after, "error")[0]["error"]["code"], "superseded");
    assert_eq!(w.tts.seen.count(), 2, "no clause synthesized after it");
    chat_hold.notify_one();
}

/// WP8 review m4: a thread bound without a usable TTS is judged per turn
/// by its speech plan, after `response.created` — so the chip fixed
/// mid-session speaks the next turn.
#[tokio::test]
async fn a_tts_fixed_mid_session_speaks_the_next_turn() {
    let w = world(|s| s.chat_tts_alias = String::new()).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Hallo?"));
    say(&mut ws).await;
    let events = until_type(&mut ws, "response.done").await;
    assert_eq!(of_type(&events, "response.created").len(), 1, "{events:?}");
    let e = of_type(&events, "error");
    assert_eq!(e[0]["error"]["code"], "tts_not_configured", "{events:?}");
    assert_eq!(w.chat.seen.chat_count(), 0, "no model call");
    // The chip: the thread gets a TTS.
    w.set(
        tid,
        json!({"voice": {"tts_alias": "speak", "language": "de"}}),
    )
    .await;
    w.asr.push(Asr::Text("Und jetzt?"));
    w.chat.push(Turn::text(&["Jetzt ja."]));
    say(&mut ws).await;
    let events = super::until_reply(&mut ws, "Jetzt ja.").await;
    assert!(
        !of_type(&events, "response.output_audio.delta").is_empty(),
        "spoken: {events:?}"
    );
    assert_eq!(w.tts.seen.body(0)["input"], "Jetzt ja.");
}

#[tokio::test]
async fn a_failed_turn_and_the_next_reach_the_model_as_one_user_message() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Erste Frage."));
    w.chat.push(Turn::Status(
        500,
        json!({"error": {"message": "the upstream fell over", "type": "server_error"}}),
    ));
    say(&mut ws).await;
    let events = until_type(&mut ws, "response.done").await;
    assert_eq!(
        of_type(&events, "response.done")[0]["response"]["status"],
        "failed"
    );
    w.asr.push(Asr::Text("Zweite Frage."));
    w.chat.push(Turn::text(&["Beide beantwortet."]));
    say(&mut ws).await;
    super::until_reply(&mut ws, "Beide beantwortet.").await;
    let body = w.chat.seen.chat(1);
    let users: Vec<&Value> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .collect();
    assert_eq!(users.len(), 1, "{body}");
    assert_eq!(users[0]["content"], "Erste Frage.\n\nZweite Frage.");
    let m = w.messages(tid).await;
    assert_eq!(m.len(), 3, "two user rows, one reply: {m:?}");
}

#[tokio::test]
async fn a_temporary_thread_gets_the_whole_turn() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({"temporary": true})).await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Nur kurz."));
    w.chat.push(Turn::text(&["Gern."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    assert_eq!(of_type(&events, "lmgw.chat.reply").len(), 1);
    let m = w.messages(tid).await;
    assert_eq!(m.len(), 2);
    assert_eq!(m[0].2["via"], "realtime");
    assert_eq!(m[1].2["via"], "realtime");
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_messages")
        .fetch_one(&w.state.db)
        .await
        .unwrap();
    assert_eq!(stored, 0, "nothing of it in the database");
}

#[tokio::test]
async fn an_unbound_session_never_sends_an_lmgw_event() {
    let w = world(|s| {
        s.realtime.tts_alias = "speak".into();
    })
    .await;
    let mut ws = w.connect("model=chatty", &[]).await.unwrap();
    next_event(&mut ws).await;
    super::manual(&mut ws, 60_000).await;
    w.chat.push(Turn::text(&["Hallo."]));
    send(&mut ws, user_text("hi")).await;
    until_type(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = until_type(&mut ws, "response.done").await;
    assert_eq!(
        of_type(&events, "response.done")[0]["response"]["status"],
        "completed"
    );
    for e in &events {
        assert!(!e["type"].as_str().unwrap().starts_with("lmgw."), "{e}");
    }
    // Nor a little later.
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "item_id": "x"}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["type"], "error", "{e}");
}

#[tokio::test]
async fn the_timing_event_says_what_the_timing_line_says() {
    let (log, _guard) = capture_log();
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Wie weit?"));
    w.chat.push(Turn::text(&["Drei ", "Kilometer."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let t = of_type(&events, "lmgw.response.timing")[0].clone();
    let rid = t["response_id"].as_str().unwrap().to_string();
    let text = log.text();
    let line = text
        .lines()
        .find(|l| l.contains(&format!("response {rid} completed")))
        .unwrap_or_else(|| panic!("no timing line for {rid} in {text}"));
    let field = |name: &str| -> Option<u64> {
        let at = line.find(name)? + name.len();
        line[at..].trim_start().split(' ').next()?.parse().ok()
    };
    assert_eq!(t["asr_ms"].as_u64(), field("ASR"), "{line}\n{t}");
    assert_eq!(
        t["first_token_ms"].as_u64(),
        field("LLM first token"),
        "{line}"
    );
    assert_eq!(
        t["first_clause_ms"].as_u64(),
        field("first clause"),
        "{line}"
    );
    assert_eq!(
        t["first_audio_ms"].as_u64(),
        field("TTS first audio"),
        "{line}"
    );
    assert_eq!(
        t["to_first_audio_ms"].as_u64(),
        field("first audio at"),
        "{line}"
    );
    assert_eq!(t["total_ms"].as_u64(), field("total"), "{line}");
    assert!(
        t["total_ms"].is_u64() && t["first_token_ms"].is_u64(),
        "{t}"
    );
}
