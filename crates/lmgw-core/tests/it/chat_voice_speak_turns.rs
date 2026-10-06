//! A turn read aloud as it streams, whichever route starts it (chat-voice
//! design §6.4; WP4 review m8, m5): an edited user message, a regenerate and
//! a continue take `speak: true` as a send does; a continue is read from the
//! clause it finishes, not from mid-word; and the optional bodies of
//! regenerate and continue take an empty request and refuse one that is not
//! JSON. The chat upstream and the TTS are fakes.

use lmgw_core::store;
use serde_json::{json, Value};

use crate::chat_actions::sse_events;
use crate::chat_voice_speak::{messages, names, post, spoken, stored_reply, thread, world, Reader};
use crate::common::Gw;
use crate::support::realtime_fakes::Turn;

/// The thread's message ids, oldest first.
async fn ids(gw: &Gw, tid: i64) -> Vec<i64> {
    let v: Value = gw
        .client()
        .get(format!("{gw}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_i64().unwrap())
        .collect()
}

/// POST `body` raw (no `Content-Type`) to `route`.
async fn raw(gw: &Gw, route: &str, body: &'static str) -> reqwest::Response {
    gw.client()
        .post(format!("{gw}{route}"))
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn an_edit_and_a_regenerate_read_their_answer_aloud() {
    let w = world(|_| {}).await;
    let tid = thread(&w.gw, "chatty").await;
    w.chat.push(Turn::text(&["Erste Antwort."]));
    stored_reply(&w.gw, tid, "frag").await;
    let [user, _] = ids(&w.gw, tid).await[..] else {
        panic!("a question and its answer")
    };

    // The user message, edited: its new answer is read as it streams.
    w.chat.push(Turn::text(&["Zweite ", "Antwort."]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{user}/edit"),
        json!({ "content": "frag anders", "speak": true }),
    )
    .await;
    let events = Reader::new(r).rest().await;
    assert_eq!(events[0].0, "turn", "{events:?}");
    assert_eq!(spoken(&events), ["Zweite Antwort."]);
    assert_eq!(events.last().unwrap().0, "speech_done", "{events:?}");

    // The answer, regenerated.
    let reply = *ids(&w.gw, tid).await.last().unwrap();
    w.chat.push(Turn::text(&["Dritte Antwort."]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{reply}/regenerate"),
        json!({ "speak": true }),
    )
    .await;
    let events = Reader::new(r).rest().await;
    assert_eq!(spoken(&events), ["Dritte Antwort."]);
    assert_eq!(events.last().unwrap().0, "speech_done", "{events:?}");
    // The user message, regenerated.
    w.chat.push(Turn::text(&["Vierte Antwort."]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{user}/regenerate"),
        json!({ "speak": true }),
    )
    .await;
    let events = Reader::new(r).rest().await;
    assert_eq!(events[0].0, "turn", "{events:?}");
    assert_eq!(spoken(&events), ["Vierte Antwort."]);
    let m = messages(&w.gw, tid).await;
    assert_eq!(m.last().unwrap().1, "Vierte Antwort.");
    assert_eq!(w.tts.seen.count(), 3);
}

#[tokio::test]
async fn a_continue_is_read_from_the_clause_it_finishes() {
    let w = world(|_| {}).await;
    // A route that takes a prefill: llama-server.
    sqlx::query(
        "UPDATE upstreams SET protocol = 'llama_cpp', kind = 'llama_server' WHERE name = 'fake'",
    )
    .execute(&w.state.db)
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let tid = thread(&w.gw, "chatty").await;
    store::append_chat_message(&w.state.db, tid, "user", "Was nun?", "", None, None, None)
        .await
        .unwrap();
    // A reply cut off mid-word by the output limit.
    store::append_chat_message(
        &w.state.db,
        tid,
        "assistant",
        "Erst das. Dann heizt der Ofen scho",
        "",
        None,
        None,
        None,
    )
    .await
    .unwrap();

    w.chat.push(Turn::text(&["n vor. ", "Danach essen wir."]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/continue"),
        json!({ "speak": true }),
    )
    .await;
    let events = Reader::new(r).rest().await;
    // The text is the continuation only; the voice says the clause it
    // finishes whole, and nothing said before again.
    let deltas: String = events
        .iter()
        .filter(|(e, _)| e == "delta")
        .map(|(_, d)| d["text"].as_str().unwrap())
        .collect();
    assert_eq!(deltas, "n vor. Danach essen wir.");
    assert_eq!(
        spoken(&events),
        ["Dann heizt der Ofen schon vor.", "Danach essen wir."]
    );
    assert_eq!(events.last().unwrap().0, "speech_done", "{events:?}");
    let m = messages(&w.gw, tid).await;
    assert_eq!(
        m.last().unwrap().1,
        "Erst das. Dann heizt der Ofen schon vor. Danach essen wir."
    );
}

#[tokio::test]
async fn regenerate_and_continue_take_an_empty_body_and_refuse_one_that_is_no_json() {
    let w = world(|_| {}).await;
    sqlx::query(
        "UPDATE upstreams SET protocol = 'llama_cpp', kind = 'llama_server' WHERE name = 'fake'",
    )
    .execute(&w.state.db)
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let tid = thread(&w.gw, "chatty").await;
    w.chat.push(Turn::text(&["Antwort."]));
    stored_reply(&w.gw, tid, "frag").await;
    let user = ids(&w.gw, tid).await[0];
    let regenerate = format!("/chat/api/threads/{tid}/messages/{user}/regenerate");

    // Refused before anything is cut: `null`, text, a wrong type.
    for body in ["null", "not json", "{\"speak\": \"yes\"}"] {
        let r = raw(&w.gw, &regenerate, body).await;
        assert_eq!(r.status(), 400, "{body:?}");
        let v: Value = r.json().await.unwrap();
        assert!(v["message"].as_str().is_some(), "{body:?}: {v}");
    }
    assert_eq!(ids(&w.gw, tid).await.len(), 2, "nothing was cut");

    // No body at all (no Content-Type either), and only whitespace: as
    // before the optional body — answered, and not read aloud.
    for body in ["", " \n\t"] {
        w.chat.push(Turn::text(&["Nochmal."]));
        let r = raw(&w.gw, &regenerate, body).await;
        assert_eq!(r.status(), 200, "{body:?}");
        let events = sse_events(&r.text().await.unwrap());
        assert_eq!(events.last().unwrap().0, "done", "{body:?}: {events:?}");
        assert!(!names(&events).contains(&"speech"), "{events:?}");
    }
    w.chat.push(Turn::text(&[" Weiter."]));
    let r = raw(&w.gw, &format!("/chat/api/threads/{tid}/continue"), "").await;
    assert_eq!(r.status(), 200);
    let events = sse_events(&r.text().await.unwrap());
    assert_eq!(events.last().unwrap().0, "done", "{events:?}");
    let r = raw(&w.gw, &format!("/chat/api/threads/{tid}/continue"), "null").await;
    assert_eq!(r.status(), 400);
    assert_eq!(w.tts.seen.count(), 0, "nothing was read aloud");
}
