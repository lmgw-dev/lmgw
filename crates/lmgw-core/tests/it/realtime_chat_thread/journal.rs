//! The journal (§8.3, §8.4, §8.6): reply N saved and cut before user
//! message N+1 (review B1), a late truncate's re-cut, a reply nobody heard
//! deleted unless it ran tools, owed turns written once, a voice finalize
//! that gives way to a text turn of another window, announcements never
//! stored, and the drain at disconnect.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::Notify;

use super::{of_type, say, until, until_type, world, World};
use crate::support::realtime_audio::{fixture, silence, stream, Asr};
use crate::support::realtime_fakes::{send, Step, Turn, Ws};
use crate::support::realtime_tts::{speech, wav, Tts};

fn truncate(item: &str, ms: u64) -> Value {
    json!({"type": "conversation.item.truncate", "item_id": item, "content_index": 0,
           "audio_end_ms": ms})
}

/// The spoken reply item of `events`.
fn reply_item(events: &[Value]) -> String {
    of_type(events, "response.output_item.added")
        .iter()
        .find(|e| e["item"]["type"] == "message")
        .map(|e| e["item"]["id"].as_str().unwrap().to_string())
        .unwrap_or_else(|| panic!("no spoken item in {events:?}"))
}

/// One clause is one second of audio.
async fn long_clauses() -> World {
    let w = world(|_| {}).await;
    w.tts.set_default(wav(&speech(1000), 24_000));
    w
}

#[tokio::test]
async fn b1_reply_n_is_saved_and_cut_before_the_next_user_message() {
    let w = long_clauses().await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    let hold = Arc::new(Notify::new());
    w.asr.push(Asr::Text("Erste Frage."));
    w.chat.push(Turn::Stream(vec![
        Step::Text("Hallo Jürgen. "),
        Step::Text("Schön dich zu hören. "),
        Step::Wait(hold.clone()),
        Step::Text("Wie geht es?"),
        Step::Finish("stop"),
    ]));
    say(&mut ws).await;
    // Both clauses spoken, generation still running.
    let events = until(&mut ws, |e| {
        e["type"] == "response.output_audio_transcript.delta"
            && e["delta"].as_str().is_some_and(|d| d.contains("hören"))
    })
    .await;
    let item = reply_item(&events);
    // Push-to-talk while it speaks: stop it — the cancel, then the truncate
    // with what played — and say the next turn at once.
    send(&mut ws, json!({"type": "response.cancel"})).await;
    send(&mut ws, truncate(&item, 1500)).await;
    w.asr.push(Asr::Text("Zweite Frage."));
    w.chat.push(Turn::text(&["Gut."]));
    send(&mut ws, json!({"type": "input_audio_buffer.clear"})).await;
    say(&mut ws).await;
    let mut seen = Vec::new();
    while of_type(&seen, "lmgw.response.timing").len() < 2 {
        seen.extend(until_type(&mut ws, "lmgw.response.timing").await);
    }

    let m = w.messages(tid).await;
    let rows: Vec<(&str, &str)> = m.iter().map(|(r, c, _)| (r.as_str(), c.as_str())).collect();
    assert_eq!(
        rows,
        [
            ("user", "Erste Frage."),
            ("assistant", "Hallo Jürgen. Schön dich"),
            ("user", "Zweite Frage."),
            ("assistant", "Gut."),
        ],
        "{m:?}"
    );
    assert_eq!(m[1].2["unheard"], "zu hören.", "{}", m[1].2);
    // The next turn answered the history as it was heard.
    let body = w.chat.seen.chat(1);
    let msgs = body["messages"].as_array().unwrap();
    let n = msgs.len();
    assert_eq!(msgs[n - 2]["content"], "Hallo Jürgen. Schön dich", "{body}");
    assert_eq!(msgs[n - 1]["content"], "Zweite Frage.");
}

#[tokio::test]
async fn a_late_truncate_re_cuts_the_reply() {
    let w = long_clauses().await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Frage."));
    w.chat
        .push(Turn::text(&["Hallo Jürgen. ", "Schön dich zu hören."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    assert_eq!(
        of_type(&events, "lmgw.chat.reply")[0]["unheard"],
        Value::Null
    );
    // The page stops playback a round trip later: 1 s in, the first clause.
    send(&mut ws, truncate(&reply_item(&events), 1000)).await;
    let ev = until_type(&mut ws, "lmgw.chat.reply").await;
    let reply = ev.last().unwrap();
    assert_eq!(reply["content"], "Hallo Jürgen.", "{reply}");
    assert_eq!(reply["unheard"], "Schön dich zu hören.");
    let m = w.messages(tid).await;
    assert_eq!(m[1].1, "Hallo Jürgen.");
    assert_eq!(m[1].2["unheard"], "Schön dich zu hören.");
    assert!(m[1].2["timing"]["total_ms"].is_u64(), "the timing stays");
}

#[tokio::test]
async fn a_reply_nobody_heard_is_deleted_unless_it_ran_tools() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    let tts_hold = Arc::new(Notify::new());
    w.tts
        .push(Tts::Held(tts_hold.clone(), wav(&speech(300), 24_000)));
    w.asr.push(Asr::Text("Frage."));
    w.chat.push(Turn::text(&["Niemand hört das."]));
    say(&mut ws).await;
    // Generated, saved, nothing spoken yet: stopped.
    until(&mut ws, |e| {
        e["type"] == "lmgw.chat.frame" && e["event"] == "done"
    })
    .await;
    send(&mut ws, json!({"type": "response.cancel"})).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let reply = of_type(&events, "lmgw.chat.reply");
    assert_eq!(reply[0]["removed"], true, "{events:?}");
    tts_hold.notify_one();
    let m = w.messages(tid).await;
    assert_eq!(m.len(), 1, "only the question stays: {m:?}");

    // A tool thread: the tool ran before anything was said.
    let url = crate::chat_golden::mcp_stub(std::time::Duration::ZERO).await;
    crate::chat_golden::register_stub(&w.state, &url).await;
    let tid = w.thread("chatty", json!({})).await;
    w.set(tid, json!({"mcp_tools": [{"server_label": "stub"}]}))
        .await;
    let mut ws = w.voice(tid).await;
    let tts_hold = Arc::new(Notify::new());
    w.tts
        .push(Tts::Held(tts_hold.clone(), wav(&speech(300), 24_000)));
    w.asr.push(Asr::Text("Wie warm?"));
    w.chat.push(Turn::Stream(vec![
        Step::CallStart {
            index: 0,
            id: Some("call_t"),
            name: "stub__echo",
        },
        Step::CallArgs {
            index: 0,
            args: "{\"text\":\"21\"}",
        },
        Step::Finish("tool_calls"),
    ]));
    w.chat.push(Turn::text(&["Es sind 21 Grad."]));
    say(&mut ws).await;
    until(&mut ws, |e| {
        e["type"] == "lmgw.chat.frame" && e["event"] == "done"
    })
    .await;
    send(&mut ws, json!({"type": "response.cancel"})).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    tts_hold.notify_one();
    let reply = of_type(&events, "lmgw.chat.reply")[0];
    assert_eq!(reply["content"], "", "{reply}");
    assert_eq!(reply["unheard"], "Es sind 21 Grad.");
    let m = w.messages(tid).await;
    assert_eq!(m.len(), 2, "the reply and its record stay: {m:?}");
    assert_eq!(m[1].2["unheard"], "Es sind 21 Grad.");
}

/// Server VAD on the committed TTS-generated question, as the page's
/// automatic mode sends it.
async fn vad(ws: &mut Ws) {
    send(
        ws,
        json!({"type": "session.update", "session": {"type": "realtime",
            "audio": {"input": {"turn_detection": {"type": "server_vad",
                "silence_duration_ms": 300}}}}}),
    )
    .await;
    until_type(ws, "session.updated").await;
}

async fn speak_question(ws: &mut Ws) {
    stream(ws, &fixture("en_complete_short.wav")).await;
    stream(ws, &silence(800)).await;
}

#[tokio::test]
async fn owed_turns_are_written_once() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    vad(&mut ws).await;
    let hold = Arc::new(Notify::new());
    w.asr.push(Asr::Text("Erste Frage."));
    // The first answer is still prefilling when the user speaks again.
    w.chat.push(Turn::Held(
        hold.clone(),
        Box::new(Turn::text(&["Zu spät."])),
    ));
    w.asr.push(Asr::Text("Zweite Frage."));
    w.chat.push(Turn::text(&["Beides."]));
    speak_question(&mut ws).await;
    until(&mut ws, |e| e["type"] == "lmgw.chat.user").await;
    // Asked for, and still prefilling.
    super::eventually("the first answer to be asked for", || async {
        w.chat.seen.chat_count() == 1
    })
    .await;
    // A second question cuts the first answer before a word of it was
    // heard: both questions are owed.
    speak_question(&mut ws).await;
    let mut seen: Vec<Value> = Vec::new();
    loop {
        let Some(ev) = super::try_next(&mut ws, 20).await else {
            let types: Vec<String> = seen
                .iter()
                .map(|e| {
                    format!(
                        "{} {} {}",
                        e["type"],
                        e.get("error").cloned().unwrap_or_default(),
                        e.get("transcript").cloned().unwrap_or_default()
                    )
                })
                .collect();
            panic!("no answer to both: {types:#?}");
        };
        let hit = ev["type"] == "lmgw.chat.reply" && ev["content"] == "Beides.";
        seen.push(ev);
        if hit {
            break;
        }
    }
    hold.notify_one();
    let m = w.messages(tid).await;
    let rows: Vec<(&str, &str)> = m.iter().map(|(r, c, _)| (r.as_str(), c.as_str())).collect();
    assert_eq!(
        rows,
        [
            ("user", "Erste Frage."),
            ("user", "Zweite Frage."),
            ("assistant", "Beides."),
        ],
        "{m:?}"
    );
    let last = w.chat.seen.chat(w.chat.seen.chat_count() - 1);
    let users: Vec<&Value> = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .collect();
    assert_eq!(users.len(), 1, "{last}");
    assert_eq!(users[0]["content"], "Erste Frage.\n\nZweite Frage.");
}

#[tokio::test]
async fn a_text_turn_of_another_window_wins_over_a_voice_cut() {
    let w = long_clauses().await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Frage."));
    w.chat
        .push(Turn::text(&["Hallo Jürgen. ", "Schön dich zu hören."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let item = reply_item(&events);
    // While the page still plays it, text from another window.
    w.chat.push(Turn::text(&["Getippt."]));
    let r = w
        .post(
            &format!("/chat/api/threads/{tid}/send"),
            json!({"content": "Tippe."}),
        )
        .await;
    let _ = r.text().await;
    // The voice reply's cut comes after it: skipped, with a note.
    send(&mut ws, truncate(&item, 1000)).await;
    let ev = until_type(&mut ws, "lmgw.chat.reply").await;
    let reply = ev.last().unwrap();
    assert_eq!(reply["skipped"], "another turn started", "{reply}");
    let m = w.messages(tid).await;
    let rows: Vec<(&str, &str)> = m.iter().map(|(r, c, _)| (r.as_str(), c.as_str())).collect();
    assert_eq!(
        rows,
        [
            ("user", "Frage."),
            ("assistant", "Hallo Jürgen. Schön dich zu hören."),
            ("user", "Tippe."),
            ("assistant", "Getippt."),
        ]
    );
    assert!(m[1].2.get("unheard").is_none(), "{}", m[1].2);
}

/// WP8 review m3: text from another window while the voice reply still
/// plays — before its slot finalized — wins over the cut, and the reply
/// still gets its `voice` (how it was spoken, its timing), not `unheard`.
#[tokio::test]
async fn a_skipped_cut_still_writes_the_reply_s_voice() {
    let w = world(|_| {}).await;
    w.tts.set_default(wav(&speech(2000), 24_000));
    let tid = w.thread("chatty", json!({})).await;
    let (mut ws, _) = w.bind(tid).await;
    // Paced: the reply plays for four seconds, its slot open until then.
    super::manual(&mut ws, 200).await;
    w.asr.push(Asr::Text("Frage."));
    w.chat
        .push(Turn::text(&["Hallo Jürgen. ", "Schön dich zu hören."]));
    say(&mut ws).await;
    let mut seen = until(&mut ws, |e| {
        e["type"] == "lmgw.chat.frame" && e["event"] == "done"
    })
    .await;
    if of_type(&seen, "response.output_audio.delta").is_empty() {
        seen.extend(until(&mut ws, |e| e["type"] == "response.output_audio.delta").await);
    }
    let item = reply_item(&seen);
    w.chat.push(Turn::text(&["Getippt."]));
    let r = w
        .post(
            &format!("/chat/api/threads/{tid}/send"),
            json!({"content": "Tippe."}),
        )
        .await;
    let _ = r.text().await;
    // Stopped while it plays: the cut comes after the text turn started.
    send(&mut ws, json!({"type": "response.cancel"})).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let reply = of_type(&events, "lmgw.chat.reply")[0];
    assert_eq!(reply["skipped"], "another turn started", "{reply}");
    assert_eq!(reply["voice"]["via"], "realtime", "{reply}");
    let m = w.messages(tid).await;
    let rows: Vec<(&str, &str)> = m.iter().map(|(r, c, _)| (r.as_str(), c.as_str())).collect();
    assert_eq!(
        rows,
        [
            ("user", "Frage."),
            ("assistant", "Hallo Jürgen. Schön dich zu hören."),
            ("user", "Tippe."),
            ("assistant", "Getippt."),
        ]
    );
    let voice = &m[1].2;
    assert_eq!(voice["via"], "realtime", "{voice}");
    assert_eq!(voice["tts"], "speak", "{voice}");
    assert!(voice["timing"]["total_ms"].is_u64(), "{voice}");
    assert!(voice.get("unheard").is_none(), "{voice}");
    // A late truncate of it has nothing more to try, and says nothing: its
    // `truncated` is read however long it takes, and only then is a quiet
    // second the check that no reply follows (a 1 s window for both could
    // false-fail under load — WP11 binding review NIT 6).
    send(&mut ws, truncate(&item, 100)).await;
    let mut later = until_type(&mut ws, "conversation.item.truncated").await;
    while let Some(e) = super::try_next(&mut ws, 1).await {
        later.push(e);
    }
    assert!(
        of_type(&later, "conversation.item.truncated").len() == 1
            && of_type(&later, "lmgw.chat.reply").is_empty(),
        "{later:?}"
    );
}

#[tokio::test]
async fn an_announcement_is_never_stored_and_its_block_only_when_heard() {
    let w = long_clauses().await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    let reply = "Hier:\n```rust\nfn main() {}\n```\nFertig.";
    for (cut, content, unheard) in [
        // Inside "Codeblock, rust.": neither its words nor the block.
        (1500, "Hier:", "```rust\nfn main() {}\n```\nFertig."),
        // Exactly after it: the block, not what follows.
        (2000, "Hier:\n```rust\nfn main() {}\n```", "Fertig."),
    ] {
        w.asr.push(Asr::Text("Zeig Code."));
        w.chat.push(Turn::text(&[reply]));
        say(&mut ws).await;
        let events = until_type(&mut ws, "lmgw.response.timing").await;
        let said: Vec<&str> = of_type(&events, "response.output_audio_transcript.delta")
            .iter()
            .map(|e| e["delta"].as_str().unwrap().trim())
            .collect();
        assert_eq!(said, ["Hier:", "Codeblock, rust.", "Fertig."]);
        send(&mut ws, truncate(&reply_item(&events), cut)).await;
        let ev = until_type(&mut ws, "lmgw.chat.reply").await;
        let r = ev.last().unwrap();
        assert_eq!(r["content"], content, "{cut}: {r}");
        assert_eq!(r["unheard"], unheard, "{cut}: {r}");
        assert!(!r["content"].as_str().unwrap().contains("Codeblock"));
        // What is stored is the reply less what was heard, as written.
        let m = w.messages(tid).await;
        let (_, stored, voice) = m.last().unwrap();
        assert_eq!(stored, content);
        assert_eq!(voice["unheard"], unheard);
    }
    // The timing says the first clause was no announcement here.
    let m = w.messages(tid).await;
    assert!(m[1].2["timing"].get("first_clause").is_none());
}

#[tokio::test]
async fn a_disconnect_mid_reply_drains_the_journal() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    let hold = Arc::new(Notify::new());
    w.asr.push(Asr::Text("Erzähl."));
    w.chat.push(Turn::Stream(vec![
        Step::Text("Es war einmal ein König. "),
        Step::Wait(hold.clone()),
        Step::Text("Er war alt."),
        Step::Finish("stop"),
    ]));
    say(&mut ws).await;
    until(&mut ws, |e| e["type"] == "response.output_audio.delta").await;
    // The network goes.
    drop(ws);
    super::eventually("the reply finalized", || async {
        let m = w.messages(tid).await;
        m.len() == 2 && m[1].2["via"] == "realtime"
    })
    .await;
    let m = w.messages(tid).await;
    assert_eq!(
        m[1].1.trim(),
        "Es war einmal ein König.",
        "the partial reply: {m:?}"
    );
    assert!(m[1].2["timing"]["response_id"].is_string(), "{}", m[1].2);
    assert_eq!(
        w.chat
            .seen
            .closed_early
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the turn was stopped with the session"
    );
}

/// WP8 review m11: a user message the store refuses is
/// `chat_history_write_failed` — the page does not leave voice mode for a
/// thread that is still there — and its words lead the next user message.
#[tokio::test]
async fn a_refused_user_write_is_said_as_such_and_its_words_are_kept() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    sqlx::query(
        "CREATE TRIGGER refuse_user BEFORE INSERT ON chat_messages WHEN NEW.role = 'user' \
         BEGIN SELECT RAISE(ABORT, 'the disk is full'); END",
    )
    .execute(&w.state.db)
    .await
    .unwrap();
    w.asr.push(Asr::Text("Erste Frage."));
    say(&mut ws).await;
    let events = until_type(&mut ws, "response.done").await;
    let e = of_type(&events, "error");
    assert_eq!(
        e[0]["error"]["code"], "chat_history_write_failed",
        "{events:?}"
    );
    assert_eq!(e[0]["error"]["type"], "server_error", "{events:?}");
    assert!(w.messages(tid).await.is_empty());
    sqlx::query("DROP TRIGGER refuse_user")
        .execute(&w.state.db)
        .await
        .unwrap();
    // The store is back: the next turn writes both.
    w.asr.push(Asr::Text("Zweite Frage."));
    w.chat.push(Turn::text(&["Beide."]));
    say(&mut ws).await;
    super::until_reply(&mut ws, "Beide.").await;
    let m = w.messages(tid).await;
    let rows: Vec<(&str, &str)> = m.iter().map(|(r, c, _)| (r.as_str(), c.as_str())).collect();
    assert_eq!(
        rows,
        [
            ("user", "Erste Frage.\nZweite Frage."),
            ("assistant", "Beide.")
        ],
        "{m:?}"
    );
}
