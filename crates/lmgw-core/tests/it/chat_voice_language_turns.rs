//! The conversation language in the turn's prompt and the bound session's
//! transcription (chat-voice design §2.1, §8.5; review of 2026-10-04): a
//! send, a regenerate and a continue with `speak: true` are told the
//! reply is heard in the thread's language, a typed send is told nothing;
//! a `speak: true` turn whose speech plan is refused gets the text-output
//! sentence; a thread's `auto` overrides Settings' language with none; and
//! a bound session's transcription follows a language changed since the
//! bind. The chat upstream, the TTS and the ASR are fakes.

use serde_json::{json, Value};

use crate::chat_voice_speak::{post, stored_reply, thread, world, Reader};
use crate::realtime_chat_thread::{say, until_type};
use crate::support::realtime_audio::AsrSeen;
use crate::support::realtime_fakes::Turn;

const HEARD: &str = "The user speaks German and hears your reply in a German voice";
const READ: &str = "The user speaks German, so answer in German";

/// The system message of the chat fake's `n`th request.
pub(crate) fn system(chat: &crate::support::realtime_fakes::ChatFake, n: usize) -> String {
    let req = chat.seen.chat(n);
    let first = &req["messages"][0];
    if first["role"] == "system" {
        first["content"].to_string()
    } else {
        String::new()
    }
}

/// Read a `speak: true` turn's SSE to its end.
pub(crate) async fn read_out(r: reqwest::Response) -> Vec<(String, Value)> {
    Reader::new(r).rest().await
}

#[tokio::test]
async fn a_spoken_send_regenerate_and_continue_say_the_language_and_a_typed_send_does_not() {
    let w = world(|_| {}).await;
    // A route that takes a prefill, for the continue: llama-server.
    sqlx::query(
        "UPDATE upstreams SET protocol = 'llama_cpp', kind = 'llama_server' WHERE name = 'fake'",
    )
    .execute(&w.state.db)
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let tid = thread(&w.gw, "chatty").await;

    // Typed: nothing is heard, nothing is said about the language.
    w.chat.push(Turn::text(&["Erste Antwort."]));
    stored_reply(&w.gw, tid, "frag").await;
    let typed = system(&w.chat, 0);
    assert!(!typed.contains("The user speaks"), "{typed}");

    // `speak: true` send.
    w.chat.push(Turn::text(&["Zweite Antwort."]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "und jetzt", "speak": true }),
    )
    .await;
    read_out(r).await;
    let sent = system(&w.chat, 1);
    assert!(sent.contains(HEARD), "{sent}");

    // `speak: true` regenerate of the latest reply.
    let v: Value =
        w.gw.client()
            .get(format!("{}/chat/api/threads/{tid}", w.gw))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    let last = v["messages"].as_array().unwrap().last().unwrap()["id"]
        .as_i64()
        .unwrap();
    w.chat
        .push(Turn::text(&["Dritte Antwort, abgeschnitten mitten im Sa"]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{last}/regenerate"),
        json!({ "speak": true }),
    )
    .await;
    read_out(r).await;
    let regenerated = system(&w.chat, 2);
    assert!(regenerated.contains(HEARD), "{regenerated}");

    // `speak: true` continue.
    w.chat.push(Turn::text(&["tz."]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/continue"),
        json!({ "speak": true }),
    )
    .await;
    read_out(r).await;
    let continued = system(&w.chat, 3);
    assert!(continued.contains(HEARD), "{continued}");

    // And typed again: nothing.
    w.chat.push(Turn::text(&["Vierte Antwort."]));
    stored_reply(&w.gw, tid, "noch eins").await;
    let typed = system(&w.chat, 4);
    assert!(!typed.contains("The user speaks"), "{typed}");
}

#[tokio::test]
async fn a_spoken_turn_whose_speech_is_refused_is_told_the_text_output_sentence() {
    // No text-to-speech model anywhere: the read-aloud is refused.
    let w = world(|s| {
        s.chat_tts_alias = String::new();
        s.realtime.tts_alias = String::new();
    })
    .await;
    let tid = thread(&w.gw, "chatty").await;
    w.chat.push(Turn::text(&["Antwort."]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "frag", "speak": true }),
    )
    .await;
    let events = read_out(r).await;
    assert!(
        events
            .iter()
            .any(|(e, d)| e == "speech_error" && d["code"] == "tts_not_configured"),
        "{events:?}"
    );
    let sys = system(&w.chat, 0);
    assert!(sys.contains(READ), "{sys}");
    assert!(!sys.contains("hears your reply"), "nothing is heard: {sys}");
}

#[tokio::test]
async fn a_thread_s_auto_overrides_the_chat_s_language_with_none() {
    let w = world(|s| s.chat_voice_language = "de".into()).await;
    let r = post(
        &w.gw,
        "/chat/api/threads",
        json!({ "model_alias": "chatty" }),
    )
    .await;
    let tid = r.json::<Value>().await.unwrap()["id"].as_i64().unwrap();

    // Settings' German, until the thread says `auto`.
    w.chat.push(Turn::text(&["Antwort."]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "frag", "speak": true }),
    )
    .await;
    read_out(r).await;
    assert!(system(&w.chat, 0).contains(HEARD));

    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "voice": { "language": "Auto" } }),
    )
    .await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["voice"]["language"], "auto", "{v}");
    assert_eq!(
        v["voice_resolved"]["language"],
        json!({"value": null, "source": "thread"}),
        "{v}"
    );
    assert_eq!(v["voice_resolved"]["language_notes"], json!([]));

    w.chat.push(Turn::text(&["Antwort."]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "frag", "speak": true }),
    )
    .await;
    read_out(r).await;
    let sys = system(&w.chat, 1);
    assert!(
        !sys.contains("The user speaks"),
        "the reply follows the user: {sys}"
    );

    // A typo is still refused.
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "voice": { "language": "automatic" } }),
    )
    .await;
    assert_eq!(r.status(), 400);
}

/// The `language` field of upload `n`, as the ASR fake received it.
pub(crate) fn asr_language(seen: &AsrSeen, n: usize) -> Option<String> {
    let body = seen.bodies.lock().unwrap()[n].clone();
    let text = String::from_utf8_lossy(&body);
    let at = text.find("name=\"language\"")?;
    let rest = &text[at..];
    let value = &rest[rest.find("\r\n\r\n")? + 4..];
    Some(value[..value.find("\r\n")?].to_string())
}

#[tokio::test]
async fn a_bound_session_s_transcription_follows_a_language_changed_since_the_bind() {
    let w = crate::realtime_chat_thread::world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    w.chat.push(Turn::text(&["Es ist drei Uhr."]));
    say(&mut ws).await;
    until_type(&mut ws, "lmgw.response.timing").await;
    assert_eq!(asr_language(&w.asr.seen, 0).as_deref(), Some("de"));

    // French from the next turn on: the transcription, as the reply.
    let r = w
        .post(
            &format!("/chat/api/threads/{tid}/settings"),
            json!({ "voice": { "language": "fr" } }),
        )
        .await;
    assert_eq!(r.status(), 200);
    w.chat.push(Turn::text(&["Il est trois heures."]));
    say(&mut ws).await;
    until_type(&mut ws, "lmgw.response.timing").await;
    assert_eq!(asr_language(&w.asr.seen, 1).as_deref(), Some("fr"));
    let sys = w.chat.seen.chat(1)["messages"][0]["content"].to_string();
    assert!(sys.contains("The user speaks French"), "{sys}");

    // `auto`: none — the ASR detects.
    let r = w
        .post(
            &format!("/chat/api/threads/{tid}/settings"),
            json!({ "voice": { "language": "auto" } }),
        )
        .await;
    assert_eq!(r.status(), 200);
    w.chat.push(Turn::text(&["Drei."]));
    say(&mut ws).await;
    until_type(&mut ws, "lmgw.response.timing").await;
    assert_eq!(asr_language(&w.asr.seen, 2), None);
}
