//! A cut inside a clause the speakable pass changed (§8.4, WP8 review M1):
//! "z. B." spelled out, markdown emphasis stripped, parentheses turned into
//! commas, a tag left out of what was said. What was heard as said is then
//! no prefix of the stored reply, so the reply is cut after the clauses
//! heard whole, the clause the cut fell in goes to `unheard` whole, and a
//! cut inside the first clause is nobody-heard's rule — never a reply left
//! whole as if everything had been heard.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::Notify;

use super::{of_type, say, until, until_type, world, World};
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::{send, Step, Turn};
use crate::support::realtime_tts::{speech, wav};

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

/// What was said, clause by clause.
fn said(events: &[Value]) -> Vec<String> {
    of_type(events, "response.output_audio_transcript.delta")
        .iter()
        .map(|e| e["delta"].as_str().unwrap().trim().to_string())
        .collect()
}

/// One clause is one second of audio.
async fn long_clauses() -> World {
    let w = world(|_| {}).await;
    w.tts.set_default(wav(&speech(1000), 24_000));
    w
}

#[tokio::test]
async fn a_cut_inside_a_clause_said_otherwise_keeps_what_was_heard_whole() {
    let w = long_clauses().await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    // Each reply's second clause is said otherwise than written; the cut
    // falls inside it, past where the two part. What is kept of it is its
    // words written as said (WP11 binding review NIT 5): "Das ist" of "Das
    // ist zum Beispiel", "Der Wert" of "Der Wert,". A clause heard to its
    // last word is heard whole, as the model wrote it (m2). The last row
    // cuts before the two part, where the words heard whole are the
    // model's own (WP9 review B). One sentence to a line: each clause is
    // its own TTS request, none joins another across a line
    // (`speech/batch.rs`) — and the line end stays in what is kept and
    // what is unheard.
    for (reply, second, cut, content, unheard) in [
        (
            "Hallo.\nDas ist z. B. so.\nUnd weiter.",
            "Das ist zum Beispiel so.",
            1500,
            "Hallo.\nDas ist",
            "z. B. so.\nUnd weiter.",
        ),
        (
            "Gut.\nDas ist **sehr** wichtig.\nEnde.",
            "Das ist sehr wichtig.",
            1800,
            "Gut.\nDas ist",
            "**sehr** wichtig.\nEnde.",
        ),
        (
            "Ja.\nDer Wert (etwa zehn) passt.\nGut.",
            "Der Wert, etwa zehn, passt.",
            1500,
            "Ja.\nDer Wert",
            "(etwa zehn) passt.\nGut.",
        ),
        (
            "Oh.\nDas ist *laughs* lustig.\nFertig.",
            "Das ist lustig.",
            1950,
            "Oh.\nDas ist *laughs* lustig.",
            "Fertig.",
        ),
        // 10 of 21 characters, "Das ist se": "Das ist" heard whole, as
        // written.
        (
            "Gut.\nDas ist **sehr** wichtig.\nEnde.",
            "Das ist sehr wichtig.",
            1500,
            "Gut.\nDas ist",
            "**sehr** wichtig.\nEnde.",
        ),
    ] {
        w.asr.push(Asr::Text("Erklär."));
        w.chat.push(Turn::text(&[reply]));
        say(&mut ws).await;
        let events = until_type(&mut ws, "lmgw.response.timing").await;
        let s = said(&events);
        assert_eq!(s.len(), 3, "{reply}: {s:?}");
        assert_eq!(s[1], second, "{reply}: {s:?}");
        // The page stops playback inside the second clause.
        send(&mut ws, truncate(&reply_item(&events), cut)).await;
        let ev = until_type(&mut ws, "lmgw.chat.reply").await;
        let r = ev.last().unwrap();
        assert_eq!(r["content"], content, "{reply}: {r}");
        assert_eq!(r["unheard"], unheard, "{reply}: {r}");
        let m = w.messages(tid).await;
        let (_, stored, voice) = m.last().unwrap();
        assert_eq!(stored, content, "{reply}");
        assert_eq!(voice["unheard"], unheard, "{reply}: {voice}");
    }

    // Inside the first clause's first word: not one word was heard, and
    // nobody heard the reply — it is deleted, as one cut before it is.
    w.asr.push(Asr::Text("Und?"));
    w.chat
        .push(Turn::text(&["Das ist z. B. so. ", "Und weiter."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let id = of_type(&events, "lmgw.chat.reply")[0]["message_id"].clone();
    send(&mut ws, truncate(&reply_item(&events), 100)).await;
    let ev = until_type(&mut ws, "lmgw.chat.reply").await;
    let r = ev.last().unwrap();
    assert_eq!(
        (&r["message_id"], &r["removed"]),
        (&id, &json!(true)),
        "{r}"
    );
    let m = w.messages(tid).await;
    assert_eq!(
        m.last().unwrap().1,
        "Und?",
        "only the question stays: {m:?}"
    );
}

/// A reply still being generated, both clauses spoken, stopped inside the
/// second: the item is producing, so the truncate alone cancels the rest of
/// the response in the same step (`item_truncate`), and the cancel's cut
/// reads the table already cut. The page's order for a stop (WP9 review
/// NIT 13): the truncate, and a `response.cancel` only for a response whose
/// audio has ended or not begun. One reply, cut once — nothing can slip in
/// between (WP9 review A: the old order, cancel then truncate, raced the
/// turn's save).
#[tokio::test]
async fn a_stop_inside_a_changed_clause_cuts_the_open_reply() {
    let w = long_clauses().await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    let hold = Arc::new(Notify::new());
    w.asr.push(Asr::Text("Erklär."));
    w.chat.push(held_reply(&hold));
    say(&mut ws).await;
    let item = reply_item(&until_spoken(&mut ws, "Beispiel").await);
    send(&mut ws, truncate(&item, 1500)).await;
    let ev = until_type(&mut ws, "lmgw.response.timing").await;
    hold.notify_one();
    let done = of_type(&ev, "response.done");
    assert_eq!(
        done.last()
            .map(|d| &d["response"]["status_details"]["reason"]),
        Some(&json!("client_cancelled")),
        "the truncate cancelled the rest: {done:?}"
    );
    let replies = of_type(&ev, "lmgw.chat.reply");
    assert_eq!(replies.len(), 1, "one finalize, cut once: {replies:?}");
    assert_eq!(replies[0]["content"], "Hallo. Das ist", "{}", replies[0]);
    assert_eq!(replies[0]["unheard"], "z. B. so.", "{}", replies[0]);
    let m = w.messages(tid).await;
    assert_eq!(m[1].1, "Hallo. Das ist");
    assert_eq!(m[1].2["unheard"], "z. B. so.");
}

/// The same reply cancelled first and truncated a round trip later — what
/// a barge-in is to the server (it cancels at the speech's start, the
/// page's truncate follows) and what a stock client sends. The cancel's cut
/// is what was sent; whether the turn's save lands before the truncate or
/// after it, the reply ends cut at the truncate (§8.3's late truncate: a
/// second `lmgw.chat.reply` then follows an annotate-only one).
#[tokio::test]
async fn a_cancel_then_a_late_truncate_cuts_the_reply_at_the_truncate() {
    let w = long_clauses().await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    let hold = Arc::new(Notify::new());
    w.asr.push(Asr::Text("Erklär."));
    w.chat.push(held_reply(&hold));
    say(&mut ws).await;
    let item = reply_item(&until_spoken(&mut ws, "Beispiel").await);
    send(&mut ws, json!({"type": "response.cancel"})).await;
    send(&mut ws, truncate(&item, 1500)).await;
    let mut ev = until_type(&mut ws, "lmgw.response.timing").await;
    hold.notify_one();
    if !of_type(&ev, "lmgw.chat.reply")
        .iter()
        .any(|r| r["unheard"].is_string())
    {
        ev.extend(
            until(&mut ws, |e| {
                e["type"] == "lmgw.chat.reply" && e["unheard"].is_string()
            })
            .await,
        );
    }
    let replies = of_type(&ev, "lmgw.chat.reply");
    let (last, earlier) = replies.split_last().unwrap();
    for r in earlier {
        // The cancel's cut, before the truncate re-cut it: everything sent
        // was heard, so the reply was only annotated.
        assert!(r["unheard"].is_null(), "{r}");
        assert_eq!(r["message_id"], last["message_id"], "{r}");
    }
    assert_eq!(last["content"], "Hallo. Das ist", "{last}");
    assert_eq!(last["unheard"], "z. B. so.", "{last}");
    super::eventually("the row cut at the truncate", || async {
        let m = w.messages(tid).await;
        m[1].1 == "Hallo. Das ist" && m[1].2["unheard"] == "z. B. so."
    })
    .await;
}

/// A truncate sent just before the socket closes — the page leaving while
/// the voice speaks (WP9 review m9) — cuts the reply at what was heard,
/// not at what was sent: the frames before the close are read first.
#[tokio::test]
async fn a_truncate_just_before_the_close_cuts_the_reply() {
    let w = long_clauses().await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    let hold = Arc::new(Notify::new());
    w.asr.push(Asr::Text("Erklär."));
    w.chat.push(held_reply(&hold));
    say(&mut ws).await;
    let item = reply_item(&until_spoken(&mut ws, "Beispiel").await);
    send(&mut ws, truncate(&item, 1000)).await;
    // The held generation is never let go: the session's end stops it.
    drop(ws);
    super::eventually("the reply cut at the truncate", || async {
        let m = w.messages(tid).await;
        m.len() == 2 && m[1].2["via"] == "realtime"
    })
    .await;
    let m = w.messages(tid).await;
    assert_eq!(m[1].1, "Hallo.", "{m:?}");
    assert_eq!(m[1].2["unheard"], "Das ist z. B. so.", "{m:?}");
}

/// "Hallo. Das ist z. B. so. " spoken, then generation held until `hold`.
fn held_reply(hold: &Arc<Notify>) -> Turn {
    Turn::Stream(vec![
        Step::Text("Hallo. "),
        Step::Text("Das ist z. B. so. "),
        Step::Wait(hold.clone()),
        Step::Text("Und weiter."),
        Step::Finish("stop"),
    ])
}

/// Events up to the transcript delta that says `word`.
async fn until_spoken(ws: &mut super::Ws, word: &str) -> Vec<Value> {
    until(ws, |e| {
        e["type"] == "response.output_audio_transcript.delta"
            && e["delta"].as_str().is_some_and(|d| d.contains(word))
    })
    .await
}

/// A reply with a tool record (§8.4), at IT level: a cut inside the final
/// answer cuts only that; one inside the preamble keeps the record whole
/// and moves the final answer to `unheard` (WP8 review, coverage).
#[tokio::test]
async fn a_tool_reply_is_cut_in_its_answer_or_kept_whole_from_its_preamble() {
    let w = long_clauses().await;
    let url = crate::chat_golden::mcp_stub(std::time::Duration::ZERO).await;
    crate::chat_golden::register_stub(&w.state, &url).await;
    let tid = w.thread("chatty", json!({})).await;
    w.set(tid, json!({"mcp_tools": [{"server_label": "stub"}]}))
        .await;
    let mut ws = w.voice(tid).await;
    // One sentence to a line, so each clause is its own TTS request
    // (`speech/batch.rs`).
    for (cut, content, unheard) in [
        // "Moment." 0–1 s, "Es sind 21 Grad." 1–2 s, "Schön warm." 2–3 s.
        (1500, "Moment. Es sind", "21 Grad.\nSchön warm."),
        (500, "Moment.", "Es sind 21 Grad.\nSchön warm."),
    ] {
        w.asr.push(Asr::Text("Wie warm?"));
        w.chat.push(Turn::Stream(vec![
            Step::Text("Moment. "),
            Step::CallStart {
                index: 0,
                id: Some("call_w"),
                name: "stub__echo",
            },
            Step::CallArgs {
                index: 0,
                args: "{\"text\":\"21\"}",
            },
            Step::Finish("tool_calls"),
        ]));
        w.chat
            .push(Turn::text(&["Es sind 21 Grad.\n", "Schön warm."]));
        say(&mut ws).await;
        let events = until_type(&mut ws, "lmgw.response.timing").await;
        assert_eq!(
            said(&events),
            ["Moment.", "Es sind 21 Grad.", "Schön warm."]
        );
        send(&mut ws, truncate(&reply_item(&events), cut)).await;
        let ev = until_type(&mut ws, "lmgw.chat.reply").await;
        let r = ev.last().unwrap();
        assert_eq!(r["content"].as_str().unwrap().trim(), content, "{cut}: {r}");
        assert_eq!(r["unheard"], unheard, "{cut}: {r}");
    }
    let record: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT ir_messages FROM chat_messages WHERE role = 'assistant' ORDER BY id",
    )
    .fetch_all(&w.state.db)
    .await
    .unwrap();
    assert!(
        record
            .iter()
            .all(|r| r.as_deref().is_some_and(|r| r.contains("stub__echo"))),
        "the records stay: {record:?}"
    );
}
