//! A bound voice session and its thread's profile (personality-profiles
//! design D21; filled by WP5): the next response uses an edited profile
//! without a rebind, the stage cache invalidated by the snapshot — its
//! prompt, its TTS's voice, and the session object itself, re-shaped and
//! said with `session.updated` before that response's first audio. A
//! profile switch (the thread's `profile_id`) does the same; a voice that
//! did not change says nothing.

use serde_json::{json, Value};

use super::{of_type, say, until_type, World};
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::{Turn, Ws};

async fn create(w: &World, body: Value) -> i64 {
    let r = w.post("/chat/api/profiles", body).await;
    assert_eq!(r.status(), 200);
    r.json::<Value>().await.unwrap()["id"].as_i64().unwrap()
}

async fn edit(w: &World, id: i64, patch: Value) {
    let r = w.post(&format!("/chat/api/profiles/{id}"), patch).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
}

/// One spoken turn answering `words`: its events, up to its timing.
async fn turn(w: &World, ws: &mut Ws, words: &[&'static str]) -> Vec<Value> {
    w.chat.push(Turn::text(words));
    w.asr.push(Asr::Text("Frage."));
    say(ws).await;
    until_type(ws, "lmgw.response.timing").await
}

/// The last chat request's system message.
fn system(w: &World) -> String {
    let body = w.chat.seen.chat(w.chat.seen.chat_count() - 1);
    body["messages"][0]["content"].as_str().unwrap().to_string()
}

/// The index of the first event of type `t`.
fn at(events: &[Value], t: &str) -> usize {
    events
        .iter()
        .position(|e| e["type"] == t)
        .unwrap_or_else(|| panic!("no {t} in {events:#?}"))
}

#[tokio::test]
async fn an_edited_profile_reaches_the_next_response_and_the_session() {
    let w = super::world(|_| {}).await;
    let id = create(
        &w,
        json!({"name": "Calm", "persona": "You are calm.",
               "voice": {"voice": "alba"}}),
    )
    .await;
    let tid = w.thread("chatty", json!({})).await;
    w.set(tid, json!({"profile_id": id})).await;
    let mut ws = w.voice(tid).await;

    let events = turn(&w, &mut ws, &["Eins."]).await;
    assert!(system(&w).starts_with("You are calm."), "{}", system(&w));
    assert!(
        of_type(&events, "session.updated").is_empty(),
        "the bind shaped it already"
    );
    let spoken = w.tts.seen.count();
    assert_eq!(w.tts.seen.body(spoken - 1)["voice"], "alba");

    // Edited between two responses: persona, voice and style.
    edit(
        &w,
        id,
        json!({"persona": "You are brisk.",
               "voice": {"voice": "cosette", "speech_style": "brisk"}}),
    )
    .await;
    let events = turn(&w, &mut ws, &["Zwei."]).await;
    assert!(system(&w).starts_with("You are brisk."), "{}", system(&w));
    assert_eq!(w.tts.seen.body(w.tts.seen.count() - 1)["voice"], "cosette");

    // The session object follows, said once, before the response's audio.
    let updated = of_type(&events, "session.updated");
    assert_eq!(updated.len(), 1, "{events:#?}");
    let s = &updated[0]["session"];
    assert_eq!(s["audio"]["output"]["voice"], "cosette", "{s}");
    assert_eq!(s["lmgw"]["speech_instructions"], "brisk", "{s}");
    assert_eq!(s["lmgw"]["tts_model"], "speak", "{s}");
    assert_eq!(s["lmgw"]["resolved"]["voice"], "cosette", "{s}");
    let u = at(&events, "session.updated");
    assert!(at(&events, "response.created") < u, "{events:#?}");
    assert!(
        u < at(&events, "response.output_audio.delta"),
        "{events:#?}"
    );

    // An echo of the new session is the client's own: accepted.
    crate::support::realtime_fakes::send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
            "audio": {"output": {"voice": "cosette"}}}}),
    )
    .await;
    assert_eq!(super::next(&mut ws).await["type"], "session.updated");

    // Nothing changed since: nothing is said.
    let events = turn(&w, &mut ws, &["Drei."]).await;
    assert!(
        of_type(&events, "session.updated").is_empty(),
        "{events:#?}"
    );
}

#[tokio::test]
async fn a_profile_switch_re_shapes_the_session_from_its_next_response() {
    let w = super::world(|_| {}).await;
    let calm = create(&w, json!({"name": "Calm", "persona": "You are calm."})).await;
    let brisk = create(
        &w,
        json!({"name": "Brisk", "persona": "You are brisk.",
               "voice": {"voice": "cosette", "speech_style": "quick"}}),
    )
    .await;
    let tid = w.thread("chatty", json!({})).await;
    w.set(tid, json!({"profile_id": calm})).await;
    let (mut ws, created) = w.bind(tid).await;
    // No voice named: realtime's chain decides (`merge::DEFAULT_VOICE`
    // asked, resolved to the setting's).
    assert_eq!(created["session"]["audio"]["output"]["voice"], "marin");
    assert_eq!(created["session"]["lmgw"]["resolved"]["voice"], "alba");
    assert_eq!(
        created["session"]["lmgw"]["speech_instructions"],
        Value::Null
    );
    super::manual(&mut ws, 60_000).await;
    turn(&w, &mut ws, &["Eins."]).await;

    // What the desktop client's `personality_set` does, through the thread.
    w.set(tid, json!({"profile_id": brisk})).await;
    let events = turn(&w, &mut ws, &["Zwei."]).await;
    assert!(system(&w).starts_with("You are brisk."));
    let updated = of_type(&events, "session.updated");
    assert_eq!(updated.len(), 1, "{events:#?}");
    let s = &updated[0]["session"];
    assert_eq!(s["audio"]["output"]["voice"], "cosette", "{s}");
    assert_eq!(s["lmgw"]["speech_instructions"], "quick", "{s}");

    // Back to none: the Chat's own voice (realtime's default here).
    w.set(tid, json!({"profile_id": null})).await;
    let events = turn(&w, &mut ws, &["Drei."]).await;
    let updated = of_type(&events, "session.updated");
    assert_eq!(updated.len(), 1, "{events:#?}");
    let s = &updated[0]["session"];
    assert_eq!(s["audio"]["output"]["voice"], "marin", "{s}");
    assert_eq!(s["lmgw"]["resolved"]["voice"], "alba", "{s}");
    assert_eq!(s["lmgw"]["speech_instructions"], Value::Null, "{s}");
}

/// A thread with no profile (the profiles review's test gap): no response
/// of its bound session says `session.updated` — not the first, not after
/// a thread edit that is no voice change, not after another profile's
/// edit moved the snapshot.
#[tokio::test]
async fn a_session_without_a_profile_says_no_extra_session_updated() {
    let w = super::world(|_| {}).await;
    let other = create(&w, json!({"name": "Other", "voice": {"voice": "cosette"}})).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;

    let events = turn(&w, &mut ws, &["Eins."]).await;
    assert!(
        of_type(&events, "session.updated").is_empty(),
        "{events:#?}"
    );
    w.set(tid, json!({"temperature": 0.5})).await;
    let events = turn(&w, &mut ws, &["Zwei."]).await;
    assert!(
        of_type(&events, "session.updated").is_empty(),
        "{events:#?}"
    );
    edit(
        &w,
        other,
        json!({"voice": {"voice": "alba", "speech_style": "brisk"}}),
    )
    .await;
    let events = turn(&w, &mut ws, &["Drei."]).await;
    assert!(
        of_type(&events, "session.updated").is_empty(),
        "{events:#?}"
    );
}
