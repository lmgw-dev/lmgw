//! Chat voice WP4 (chat-voice design §6): read-aloud of a stored reply
//! (`messages/{mid}/speak`) and of a turn sent with `speak: true` (the
//! speech tee), `speech/stop`, the closure that passes the page's going
//! upstream, the thread's seed, and code and tables announced rather than
//! read. The chat upstream and the TTS are fakes (`support::realtime_fakes`,
//! `support::realtime_tts`; a voice-design row on the fake GPU world for the
//! seed); the audio is a slice of the committed speech fixture.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use lmgw_core::config::{Protocol, Settings, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};
use tokio::sync::Notify;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_actions::{openai_sse, sse_events};
use crate::chat_golden::{mcp_stub, openai_call, register_stub, tool_thread};
use crate::common::{serve, Gw};
use crate::support::realtime_fakes::{add_chat_aliases, chat_fake, ChatFake, Step, Turn};
use crate::support::realtime_tts::{
    add_tts_alias, speech, tts_fake, wav, Tts, TtsFake, TTS_ALIAS, VOICES,
};

// -- harness -------------------------------------------------------------------

/// A gateway with the chat fake (`chatty`) and the TTS fake ([`TTS_ALIAS`]),
/// the Chat speaking with it (`chat_tts_alias`) and realtime's default voice
/// `alba`, then `tweak`.
pub(crate) struct World {
    pub state: SharedState,
    pub gw: Gw,
    pub chat: ChatFake,
    pub tts: TtsFake,
}

pub(crate) async fn world(tweak: impl FnOnce(&mut Settings)) -> World {
    let chat = chat_fake().await;
    let tts = tts_fake(&VOICES).await;
    let state = AppState::init_for_tests().await.unwrap();
    add_chat_aliases(&state, &chat).await;
    add_tts_alias(&state, &tts).await;
    settings(&state, |s| {
        s.chat_tts_alias = TTS_ALIAS.into();
        s.realtime.default_voice = "alba".into();
        tweak(s);
    })
    .await;
    let gw = serve(state.clone()).await;
    World {
        state,
        gw,
        chat,
        tts,
    }
}

pub(crate) async fn settings(state: &SharedState, f: impl FnOnce(&mut Settings)) {
    let mut s = state.snapshot().settings.clone();
    f(&mut s);
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

pub(crate) async fn post(gw: &Gw, route: &str, body: Value) -> reqwest::Response {
    gw.client()
        .post(format!("{gw}{route}"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// A thread on `model` whose voice speaks German.
pub(crate) async fn thread(gw: &Gw, model: &str) -> i64 {
    let r = post(gw, "/chat/api/threads", json!({ "model_alias": model })).await;
    let tid = r.json::<Value>().await.unwrap()["id"].as_i64().unwrap();
    let r = post(
        gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "voice": { "language": "de" } }),
    )
    .await;
    assert_eq!(r.status(), 200);
    tid
}

/// Send `content` without speech and read the turn to its end: the reply's
/// id.
pub(crate) async fn stored_reply(gw: &Gw, tid: i64, content: &str) -> i64 {
    let r = post(
        gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": content }),
    )
    .await;
    let events = sse_events(&r.text().await.unwrap());
    let done = events.iter().find(|(e, _)| e == "done").expect("done");
    done.1["message_id"].as_i64().expect("a saved reply")
}

/// An SSE body read as it arrives.
pub(crate) struct Reader {
    resp: reqwest::Response,
    buf: String,
}

impl Reader {
    pub fn new(resp: reqwest::Response) -> Self {
        assert_eq!(resp.status(), 200);
        Self {
            resp,
            buf: String::new(),
        }
    }

    /// The next event; `None` at the end of the stream.
    pub async fn next(&mut self) -> Option<(String, Value)> {
        loop {
            if let Some(at) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..at + 2).collect();
                if let Some(e) = sse_events(&block).into_iter().next() {
                    return Some(e);
                }
                continue;
            }
            let chunk = tokio::time::timeout(Duration::from_secs(20), self.resp.chunk())
                .await
                .unwrap_or_else(|_| panic!("no event for 20 s after: {}", self.buf))
                .unwrap()?;
            self.buf.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    }

    /// Events up to and with the first `event` for which `f` holds.
    pub async fn until(&mut self, event: &str, f: impl Fn(&Value) -> bool) -> Vec<(String, Value)> {
        let mut got = Vec::new();
        loop {
            let e = self
                .next()
                .await
                .unwrap_or_else(|| panic!("the stream ended before '{event}': {got:?}"));
            let hit = e.0 == event && f(&e.1);
            got.push(e);
            if hit {
                return got;
            }
        }
    }

    /// Every event to the end of the stream.
    pub async fn rest(&mut self) -> Vec<(String, Value)> {
        let mut got = Vec::new();
        while let Some(e) = self.next().await {
            got.push(e);
        }
        got
    }
}

pub(crate) fn names(events: &[(String, Value)]) -> Vec<&str> {
    events.iter().map(|(e, _)| e.as_str()).collect()
}

/// The text of each `speech` frame, in order.
pub(crate) fn spoken(events: &[(String, Value)]) -> Vec<String> {
    events
        .iter()
        .filter(|(e, _)| e == "speech")
        .map(|(_, d)| d["text"].as_str().unwrap().to_string())
        .collect()
}

pub(crate) fn position(events: &[(String, Value)], f: impl Fn(&(String, Value)) -> bool) -> usize {
    events
        .iter()
        .position(f)
        .unwrap_or_else(|| panic!("not in {events:?}"))
}

/// The TTS rows: `(ingress_proto, status, error_kind)`.
pub(crate) async fn tts_rows(
    state: &SharedState,
    alias: &str,
) -> Vec<(String, i64, Option<String>)> {
    sqlx::query_as(
        "SELECT ingress_proto, status, error_kind FROM request_logs
         WHERE requested_alias = ?1 AND class = 'audio' ORDER BY id",
    )
    .bind(alias)
    .fetch_all(&state.db)
    .await
    .unwrap()
}

pub(crate) async fn until(what: &str, cond: impl AsyncFn() -> bool) {
    for _ in 0..500 {
        if cond().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// The thread's messages as `(role, content)`.
pub(crate) async fn messages(gw: &Gw, tid: i64) -> Vec<(String, String)> {
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
        .map(|m| {
            (
                m["role"].as_str().unwrap().to_string(),
                m["content"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

const CODE_AND_TABLE: [&str; 3] = [
    "Hier ist Code:\n```rust\nfn main() {}\n```\n",
    "| a | b |\n|---|---|\n| 1 | 2 |\n",
    "Fertig.",
];

// -- a stored reply ----------------------------------------------------------------

#[tokio::test]
async fn a_stored_reply_is_read_in_frame_order() {
    let w = world(|_| {}).await;
    let tid = thread(&w.gw, "chatty").await;
    w.chat.push(Turn::text(&CODE_AND_TABLE));
    let mid = stored_reply(&w.gw, tid, "zeig was").await;

    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{mid}/speak"),
        json!({}),
    )
    .await;
    let events = Reader::new(r).rest().await;
    // A remote TTS loads nothing here: no `state`. The route, then the
    // clauses, then the end.
    assert_eq!(
        names(&events),
        [
            "voice",
            "speech",
            "speech",
            "speech",
            "speech",
            "speech_done"
        ],
        "{events:?}"
    );
    assert_eq!(
        events[0].1,
        json!({"tts": TTS_ALIAS, "voice": "alba", "tts_answered_by": null})
    );
    // Code and the table are announced in the thread's language, not read.
    assert_eq!(
        spoken(&events),
        ["Hier ist Code:", "Codeblock, rust.", "Tabelle.", "Fertig."]
    );
    for (n, (_, d)) in events.iter().filter(|(e, _)| e == "speech").enumerate() {
        assert_eq!(d["seq"], n);
        let pcm = base64::engine::general_purpose::STANDARD
            .decode(d["pcm"].as_str().unwrap())
            .unwrap();
        // 300 ms of 24 kHz PCM16 per clause.
        assert_eq!(pcm.len(), 300 * 24 * 2, "{n}");
    }
    let done = &events.last().unwrap().1;
    assert_eq!(done["audio_ms"], 1200);
    assert_eq!(done["chars"], 14 + 16 + 8 + 7);
    assert_eq!(done["tts"], TTS_ALIAS);
    assert_eq!(done["tts_answered_by"], Value::Null);
    assert_eq!(done["stopped"], false);
    assert!(done["first_audio_ms"].as_u64().is_some(), "{done}");
    // What the TTS was sent, and one row for the speak, the Chat's.
    let inputs: Vec<Value> = (0..w.tts.seen.count())
        .map(|n| w.tts.seen.body(n)["input"].clone())
        .collect();
    assert_eq!(
        inputs,
        ["Hier ist Code:", "Codeblock, rust.", "Tabelle.", "Fertig."]
    );
    assert_eq!(
        tts_rows(&w.state, TTS_ALIAS).await,
        [("chat".to_string(), 200, None)]
    );

    // Not a reply, not there.
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{}/speak", mid - 1),
        json!({}),
    )
    .await;
    assert_eq!(r.status(), 400);
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/999/speak"),
        json!({}),
    )
    .await;
    assert_eq!(r.status(), 404);
}

/// An Admin Chat thread's read-aloud is labelled as its model turns are,
/// `admin`, not `chat`.
#[tokio::test]
async fn an_admin_thread_s_speech_row_says_admin() {
    let w = world(|_| {}).await;
    let r = post(
        &w.gw,
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "kind": "admin" }),
    )
    .await;
    let tid = r.json::<Value>().await.unwrap()["id"].as_i64().unwrap();
    // A reply as an earlier turn left it.
    let mid = store::append_chat_message(
        &w.state.db,
        tid,
        "assistant",
        "Erledigt.",
        "",
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{mid}/speak"),
        json!({}),
    )
    .await;
    let events = Reader::new(r).rest().await;
    assert_eq!(events.last().unwrap().0, "speech_done", "{events:?}");
    let state = w.state.clone();
    until("the speak's row is written", async || {
        !tts_rows(&state, TTS_ALIAS).await.is_empty()
    })
    .await;
    assert_eq!(
        tts_rows(&w.state, TTS_ALIAS).await,
        [("admin".to_string(), 200, None)]
    );
}

#[tokio::test]
async fn a_thread_that_cannot_speak_says_why_in_the_stream() {
    let w = world(|s| s.chat_tts_alias.clear()).await;
    let tid = thread(&w.gw, "chatty").await;
    w.chat.push(Turn::text(&["Hallo."]));
    let mid = stored_reply(&w.gw, tid, "hi").await;
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{mid}/speak"),
        json!({}),
    )
    .await;
    let events = Reader::new(r).rest().await;
    assert_eq!(names(&events), ["speech_error"]);
    assert_eq!(events[0].1["code"], "tts_not_configured");
    // A turn sent with speech says so too, and its text is unchanged.
    w.chat.push(Turn::text(&["Noch ", "mal."]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "nochmal", "speak": true}),
    )
    .await;
    let events = Reader::new(r).rest().await;
    assert_eq!(events[0].0, "turn", "the turn frame stays first");
    assert!(names(&events).contains(&"speech_error"), "{events:?}");
    assert_eq!(events.last().unwrap().0, "done");
}

// -- a streaming reply -------------------------------------------------------------

#[tokio::test]
async fn speak_true_interleaves_speech_and_text() {
    let w = world(|_| {}).await;
    let tid = thread(&w.gw, "chatty").await;
    let hold = Arc::new(Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Hallo zusammen. "),
        Step::Wait(hold.clone()),
        Step::Text("Wie geht es?"),
        Step::Finish("stop"),
        Step::Usage(5, 6),
    ]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi", "speak": true}),
    )
    .await;
    let mut sse = Reader::new(r);
    // The first clause is spoken while the text is still being written.
    let mut events = sse.until("speech", |_| true).await;
    assert_eq!(events[0].0, "turn");
    assert!(
        !events
            .iter()
            .any(|(e, d)| e == "delta" && d["text"] == "Wie geht es?"),
        "the stream is still held: {events:?}"
    );
    hold.notify_one();
    events.extend(sse.rest().await);
    let first_speech = position(&events, |(e, _)| e == "speech");
    let second_delta = position(&events, |(e, d)| {
        e == "delta" && d["text"] == "Wie geht es?"
    });
    let done = position(&events, |(e, _)| e == "done");
    assert!(
        first_speech < second_delta && second_delta < done,
        "{events:?}"
    );
    assert_eq!(spoken(&events), ["Hallo zusammen.", "Wie geht es?"]);
    assert_eq!(events.last().unwrap().0, "speech_done");
    assert_eq!(events.last().unwrap().1["stopped"], false);
    // The text and its save are a text turn's.
    assert_eq!(events[done].1["saved"], true);
    let m = messages(&w.gw, tid).await;
    assert_eq!(m.last().unwrap().1, "Hallo zusammen. Wie geht es?");
    // Its speech is the Chat's own traffic, labelled `chat` like the turn.
    until("the speech's row is written", async || {
        !tts_rows(&w.state, TTS_ALIAS).await.is_empty()
    })
    .await;
    assert_eq!(
        tts_rows(&w.state, TTS_ALIAS).await,
        [("chat".to_string(), 200, None)]
    );
    let turn: Vec<String> = sqlx::query_scalar(
        "SELECT ingress_proto FROM request_logs WHERE requested_alias = 'chatty'",
    )
    .fetch_all(&w.state.db)
    .await
    .unwrap();
    assert_eq!(turn, ["chat"]);
}

#[tokio::test]
async fn speech_stop_stops_the_speech_not_the_text() {
    let w = world(|_| {}).await;
    let tid = thread(&w.gw, "chatty").await;
    let hold = Arc::new(Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Erster Satz. "),
        Step::Wait(hold.clone()),
        Step::Text("Zweiter Satz."),
        Step::Finish("stop"),
    ]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi", "speak": true}),
    )
    .await;
    let mut sse = Reader::new(r);
    let mut events = sse.until("speech", |_| true).await;
    let stopped: Value = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/speech/stop"),
        json!({}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(stopped, json!({"ok": true, "stopped": 1}));
    hold.notify_one();
    events.extend(sse.rest().await);
    // The text goes on to its end, and is saved whole.
    assert!(
        events
            .iter()
            .any(|(e, d)| e == "delta" && d["text"] == "Zweiter Satz."),
        "{events:?}"
    );
    let done = &events[position(&events, |(e, _)| e == "done")].1;
    assert_eq!(done["saved"], true);
    assert_eq!(done["aborted"], false);
    // The speech ended where it was stopped.
    assert_eq!(spoken(&events), ["Erster Satz."]);
    let end = &events[position(&events, |(e, _)| e == "speech_done")].1;
    assert_eq!(end["stopped"], true);
    assert_eq!(w.tts.seen.count(), 1);
    let m = messages(&w.gw, tid).await;
    assert_eq!(m.last().unwrap().1, "Erster Satz. Zweiter Satz.");
    // Nothing runs any more.
    let again: Value = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/speech/stop"),
        json!({}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(again["stopped"], 0);
    let r = post(&w.gw, "/chat/api/threads/99999/speech/stop", json!({})).await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn aborting_a_speaking_send_drops_the_upstream_and_saves_the_partial_reply() {
    let w = world(|_| {}).await;
    let tid = thread(&w.gw, "chatty").await;
    // Held for good: only the page going away ends it.
    let never = Arc::new(Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Ein halber "),
        Step::Text("Satz. "),
        Step::Wait(never),
        Step::Text("nie gesagt"),
    ]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi", "speak": true}),
    )
    .await;
    let mut sse = Reader::new(r);
    sse.until("speech", |_| true).await;
    // The page goes (Stop, a reload, leaving the thread).
    drop(sse);
    tokio::time::timeout(Duration::from_secs(10), w.chat.seen.closed.notified())
        .await
        .expect("the upstream request was dropped");
    assert_eq!(
        w.chat
            .seen
            .closed_early
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let (gw, tid) = (w.gw.clone(), tid);
    until("the partial reply is saved", async || {
        messages(&gw, tid)
            .await
            .last()
            .is_some_and(|m| m.0 == "assistant" && m.1 == "Ein halber Satz. ")
    })
    .await;
    // Its speech ended with it.
    let state = w.state.clone();
    until("the speech's row is written", async || {
        !tts_rows(&state, TTS_ALIAS).await.is_empty()
    })
    .await;
    let again: Value = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/speech/stop"),
        json!({}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(again["stopped"], 0, "no speech is left running");
}

#[tokio::test]
async fn an_aborted_speak_stops_synthesis_and_writes_one_row_labelled_as_the_chat() {
    let w = world(|_| {}).await;
    let tid = thread(&w.gw, "chatty").await;
    w.chat.push(Turn::text(&[
        "Eins ist gut. ",
        "Zwei ist besser. ",
        "Drei ist am besten.",
    ]));
    let mid = stored_reply(&w.gw, tid, "zähl").await;
    // The first clause is answered, the second never.
    let held = Arc::new(Notify::new());
    w.tts.push(Tts::Wav(wav(&speech(300), 24_000)));
    w.tts.push(Tts::Held(held, wav(&speech(300), 24_000)));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{mid}/speak"),
        json!({}),
    )
    .await;
    let mut sse = Reader::new(r);
    let events = sse.until("speech", |_| true).await;
    assert_eq!(spoken(&events), ["Eins ist gut."]);
    let seen = w.tts.seen.clone();
    until("the second clause is with the TTS", async || {
        seen.count() == 2
    })
    .await;
    drop(sse);
    let state = w.state.clone();
    until("the speak's row is written", async || {
        !tts_rows(&state, TTS_ALIAS).await.is_empty()
    })
    .await;
    // One row, the Chat's (not `realtime`): a stop is a 200 `canceled`.
    assert_eq!(
        tts_rows(&w.state, TTS_ALIAS).await,
        [("chat".to_string(), 200, Some("canceled".to_string()))]
    );
    // Nothing more is synthesized.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(w.tts.seen.count(), 2);
}

// -- tools -----------------------------------------------------------------------

/// Mount `bodies` as the answers to successive `/chat/completions` calls.
async fn in_order(mock: &MockServer, bodies: &[String]) {
    for (i, body) in bodies.iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(body.clone(), "text/event-stream"),
            )
            .up_to_n_times(1)
            .with_priority((i + 1) as u8)
            .mount(mock)
            .await;
    }
}

#[tokio::test]
async fn a_tool_preamble_is_spoken_before_the_result() {
    let mock = MockServer::start().await;
    in_order(
        &mock,
        &[
            // No sentence end: only the flush at the call says it.
            openai_call("Ich schaue nach", "c1", "stub__slow", "{\"text\":\"x\"}"),
            openai_sse("Gefunden.", 20, 4),
        ],
    )
    .await;
    let (state, gw) =
        crate::chat_actions::gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tts = tts_fake(&VOICES).await;
    add_tts_alias(&state, &tts).await;
    settings(&state, |s| {
        s.chat_tts_alias = TTS_ALIAS.into();
        s.realtime.default_voice = "alba".into();
    })
    .await;
    // The tool takes a while: the preamble is heard meanwhile.
    register_stub(&state, &mcp_stub(Duration::from_secs(2)).await).await;
    let tid = tool_thread(&gw).await;
    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "schau nach", "speak": true}),
    )
    .await;
    let events = Reader::new(r).rest().await;
    let preamble = position(&events, |(e, d)| {
        e == "speech" && d["text"] == "Ich schaue nach."
    });
    let result = position(&events, |(e, d)| e == "tool" && d["event"] == "result");
    assert!(preamble < result, "{events:?}");
    assert_eq!(spoken(&events), ["Ich schaue nach.", "Gefunden."]);
    assert_eq!(events.last().unwrap().0, "speech_done");
}

// -- the seed ----------------------------------------------------------------------

/// Also the `state` frames of a cold TTS, which only a local row has.
#[tokio::test]
async fn two_speaks_of_one_thread_send_the_same_seed() {
    // A voice-design row: without one seed its voice changes per request.
    let crate::chat_voice_speak_style::Design { g, chat, gw } =
        crate::chat_voice_speak_style::design_world(Some("a deep, slow narrator"), |_| {}).await;
    let tid = thread(&gw, "chatty").await;
    chat.push(Turn::text(&["Guten Tag."]));
    let mid = stored_reply(&gw, tid, "hallo").await;
    let before: Value = gw
        .client()
        .get(format!("{gw}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["thread"]["voice_resolved"]["seed"], Value::Null);

    // Two first uses at once draw one seed between them.
    let speak = || async {
        let r = post(
            &gw,
            &format!("/chat/api/threads/{tid}/messages/{mid}/speak"),
            json!({}),
        )
        .await;
        Reader::new(r).rest().await
    };
    let (a, b) = tokio::join!(speak(), speak());
    for events in [&a, &b] {
        assert_eq!(events.last().unwrap().0, "speech_done", "{events:?}");
    }
    // The row was cold: its opening said so, `loading` then `ready` with
    // the time it took (§6.3: `state` only while the TTS is not resident).
    let states: Vec<&Value> = a
        .iter()
        .filter(|(e, _)| e == "state")
        .map(|(_, d)| d)
        .collect();
    assert_eq!(states.len(), 2, "{a:?}");
    assert_eq!(states[0]["state"], "loading");
    assert_eq!(states[0]["stage"], "tts");
    assert_eq!(states[1]["state"], "ready");
    assert!(states[1]["ms"].as_u64().is_some(), "{a:?}");
    let after: Value = gw
        .client()
        .get(format!("{gw}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let seed = after["thread"]["voice_resolved"]["seed"]
        .as_u64()
        .unwrap_or_else(|| panic!("drawn and stored on first use: {after}"));
    let bodies = g.world().speech_bodies.clone();
    // The load the warm-up may send aside, every clause carries it.
    let seeds: Vec<Value> = bodies.iter().map(|b| b["seed"].clone()).collect();
    assert!(seeds.len() >= 2, "{bodies:?}");
    assert!(
        seeds.iter().all(|s| *s == json!(seed)),
        "{seeds:?} vs {seed}"
    );
    // And a later speak keeps it.
    let n = bodies.len();
    let events = speak().await;
    assert_eq!(
        names(&events),
        ["voice", "speech", "speech_done"],
        "resident now: no state"
    );
    let later = g.world().speech_bodies.clone();
    assert!(later.len() > n);
    assert!(later[n..].iter().all(|b| b["seed"] == json!(seed)));
}
