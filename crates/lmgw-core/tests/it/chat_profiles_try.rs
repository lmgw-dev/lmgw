//! Preview, Test and Speak on an unsaved profile draft (personality-profiles
//! design §3.1's last three rows; filled by WP5), with a mock upstream and a
//! mock TTS: the preview's texts equal what a send builds, `tokens` with its
//! approximation flags, Test storing nothing but its request row, and Speak
//! answering one valid WAV, its refusals as flat JSON.

use serde_json::{json, Value};

use crate::chat_golden::{plain_thread, text_turn, voice_turn};
use crate::realtime_chat_thread::{world, World};
use crate::support::realtime_fakes::Turn;

/// A draft with every prompt part, `{{model}}` in the persona.
fn draft() -> Value {
    json!({
        "persona": "You are {{model}}, a calm voice.",
        "length_rule": "Answer in one sentence.",
        "examples": [{"user": "Is it raining?", "reply": "I can't see outside."}],
        "voice_block": "Talk in short sentences.",
        "reasoning": "off",
    })
}

async fn post(w: &World, path: &str, body: Value) -> (u16, Value) {
    let r = w.post(path, body).await;
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

/// The system message of the last request the chat fake saw.
fn last_system(w: &World) -> String {
    let body = w.chat.seen.chat(w.chat.seen.chat_count() - 1);
    body["messages"][0]["content"]
        .as_str()
        .unwrap_or_else(|| panic!("no system text: {body}"))
        .to_string()
}

/// `request_logs` rows: `(ingress_proto, requested_alias)`, in order.
async fn rows(w: &World) -> Vec<(String, String)> {
    sqlx::query_as("SELECT ingress_proto, requested_alias FROM request_logs ORDER BY id")
        .fetch_all(&w.state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_preview_is_what_a_send_with_the_profile_builds() {
    let w = world(|_| {}).await;
    let (s, p) = post(&w, "/chat/api/profiles", {
        let mut b = draft();
        b["name"] = json!("Calm");
        b
    })
    .await;
    assert_eq!(s, 200, "{p}");
    let tid = plain_thread(&w, None, Some("You are the thread's own prompt.")).await;
    w.set(
        tid,
        json!({"profile_id": p["id"], "voice": {"language": "de"}}),
    )
    .await;

    // A text send and a bound voice turn with the stored profile.
    text_turn(&w, tid, false).await;
    let text_sent = last_system(&w);
    voice_turn(&w, tid).await;
    let voice_sent = last_system(&w);

    // The draft with the same content, on that thread.
    let (s, v) = post(
        &w,
        "/chat/api/profiles/preview",
        json!({"profile": draft(), "thread_id": tid}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["text_turn"], text_sent);
    assert_eq!(v["voice_turn"], voice_sent);
    assert_eq!(v["tokens"], Value::Null, "no model, no count");
    let stat = v["static"].as_str().unwrap();
    assert!(stat.starts_with("You are chatty, a calm voice."), "{stat}");
    assert!(
        !stat.contains("thread's own prompt"),
        "the persona takes its place"
    );
    assert!(text_sent.starts_with(stat));
    assert!(
        voice_sent.contains("Talk in short sentences."),
        "{voice_sent}"
    );

    // A different draft on the same thread: the draft, not the stored one.
    let (_, v) = post(
        &w,
        "/chat/api/profiles/preview",
        json!({"profile": {"length_rule": "Two words."}, "thread_id": tid}),
    )
    .await;
    assert_eq!(
        v["static"],
        "You are the thread's own prompt.\n\nTwo words."
    );

    // No thread: an empty one with Settings' default prompt. `{{model}}`
    // names `model`, and with no model to name stays as written.
    let (s, v) = post(
        &w,
        "/chat/api/profiles/preview",
        json!({"profile": {"persona": "I am {{model}}."}}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["static"], "I am {{model}}.");
    let (_, v) = post(
        &w,
        "/chat/api/profiles/preview",
        json!({"profile": {"persona": "I am {{model}}."}, "model": "other"}),
    )
    .await;
    assert_eq!(v["static"], "I am other.");
    let (_, v) = post(
        &w,
        "/chat/api/profiles/preview",
        json!({"profile": {"length_rule": "Short."}}),
    )
    .await;
    let today = chrono::Local::now().date_naive();
    let builtin = lmgw_core::config::expand_chat_prompt(
        w.state.snapshot().settings.default_chat_prompt(),
        "{{model}}",
        today,
    );
    assert_eq!(v["static"], format!("{}\n\nShort.", builtin.trim()), "{v}");
}

#[tokio::test]
async fn the_count_is_the_universal_counter_s_with_its_flags() {
    let w = world(|_| {}).await;
    let (s, v) = post(
        &w,
        "/chat/api/profiles/preview",
        json!({"profile": draft(), "model": "chatty"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let t = &v["tokens"];
    assert_eq!(t["alias"], "chatty", "{v}");
    assert_eq!(t["answered_by"], Value::Null);
    let n = |k: &str| t[k].as_u64().unwrap();
    assert!(n("static") > 0 && n("text_turn") >= n("static"), "{t}");
    assert!(
        n("voice_turn") > n("text_turn"),
        "the voice block adds: {t}"
    );

    // What `/v1/count_tokens` says of the same text, number and flags.
    let r = w
        .post(
            "/v1/count_tokens",
            json!({"model": "chatty", "input": v["static"]}),
        )
        .await;
    let flags: Vec<String> = r
        .headers()
        .get("x-lmgw-count-approximate")
        .map(|h| h.to_str().unwrap().split(',').map(str::to_string).collect())
        .unwrap_or_default();
    let counted: Value = r.json().await.unwrap();
    assert_eq!(counted["tokens"], t["static"]);
    assert_eq!(t["approx"], json!(flags));

    // A model that is none is refused with its code.
    let (s, v) = post(
        &w,
        "/chat/api/profiles/preview",
        json!({"profile": draft(), "model": "nope"}),
    )
    .await;
    assert!(s >= 400 && v["code"].is_string(), "{s} {v}");
}

#[tokio::test]
async fn test_is_one_call_that_stores_nothing_but_its_row() {
    let w = world(|_| {}).await;
    let tid = plain_thread(&w, None, None).await;
    w.set(tid, json!({"max_tokens": 77})).await;
    let thread_before = w.get(&format!("/chat/api/threads/{tid}")).await;
    let rows_before = rows(&w).await.len();

    w.chat
        .push(Turn::reasoned(&["Let me think."], &["Hello", " there."]));
    let (s, v) = post(
        &w,
        "/chat/api/profiles/test",
        json!({"profile": draft(), "thread_id": tid, "model": "chatty",
               "text": "Hi!", "voice": false}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["reply"], "Hello there.");
    assert_eq!(v["reasoning"], "Let me think.");
    assert_eq!(
        v["usage"],
        json!({"prompt_tokens": 12, "completion_tokens": 8})
    );
    assert!(
        v["first_token_ms"].is_u64() && v["reasoning_ms"].is_u64(),
        "{v}"
    );
    assert!(v["total_ms"].as_u64().unwrap() >= v["first_token_ms"].as_u64().unwrap());
    assert_eq!(v["answered_by"], Value::Null);

    // The request: the preview's text turn, the user's text, no history and
    // no max_tokens (the thread's 77 is not sent).
    let sent = w.chat.seen.chat(w.chat.seen.chat_count() - 1);
    let msgs = sent["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "{sent}");
    assert_eq!(msgs[1]["content"], "Hi!");
    assert!(sent.get("max_tokens").is_none(), "{sent}");
    assert_eq!(msgs[0]["content"], v["system"]);
    let (_, preview) = post(
        &w,
        "/chat/api/profiles/preview",
        json!({"profile": draft(), "thread_id": tid}),
    )
    .await;
    assert_eq!(v["system"], preview["text_turn"]);

    // One request row, the Chat's; the thread is as it was.
    let after = rows(&w).await;
    assert_eq!(after.len(), rows_before + 1, "{after:?}");
    assert_eq!(
        after.last().unwrap(),
        &("chat".to_string(), "chatty".to_string())
    );
    let key: Option<String> =
        sqlx::query_scalar("SELECT client_key FROM request_logs ORDER BY id DESC LIMIT 1")
            .fetch_one(&w.state.db)
            .await
            .unwrap();
    // The owner's call is a Chat turn's: no key, named `internal:chat` by
    // its protocol.
    assert_eq!(key, None, "the owner's call");
    let thread_after = w.get(&format!("/chat/api/threads/{tid}")).await;
    assert_eq!(thread_after["messages"], thread_before["messages"]);
    assert_eq!(thread_after["updated_at"], thread_before["updated_at"]);

    // A voice test: the voice turn's system message.
    let (s, v) = post(
        &w,
        "/chat/api/profiles/test",
        json!({"profile": draft(), "thread_id": tid, "model": "chatty",
               "text": "Hi!", "voice": true}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["system"], preview["voice_turn"]);
    assert_eq!(
        v["reasoning_ms"],
        Value::Null,
        "the default answer reasons not"
    );
}

#[tokio::test]
async fn test_refusals_say_their_code() {
    let w = world(|_| {}).await;
    let cases = [
        (
            json!({"profile": draft(), "text": "Hi"}),
            400,
            "bad_request",
        ),
        (
            json!({"profile": draft(), "model": "chatty", "text": "  "}),
            400,
            "empty_message",
        ),
        (
            json!({"profile": {"examples": [{"user": "a", "reply": " "}]},
                   "model": "chatty", "text": "Hi"}),
            400,
            "bad_request",
        ),
        (
            json!({"profile": draft(), "thread_id": 9999, "model": "chatty", "text": "Hi"}),
            404,
            "not_found",
        ),
    ];
    for (body, status, code) in cases {
        let (s, v) = post(&w, "/chat/api/profiles/test", body.clone()).await;
        assert_eq!((s, v["code"].as_str()), (status, Some(code)), "{body}: {v}");
    }
    // The upstream's refusal, with its status (a draft that asks no
    // reasoning off, which a refusal would have retried in another form).
    w.chat.push(Turn::Status(
        400,
        json!({"error": {"message": "context too long", "type": "invalid_request_error"}}),
    ));
    let (s, v) = post(
        &w,
        "/chat/api/profiles/test",
        json!({"profile": {"persona": "Hi."}, "model": "chatty", "text": "Hi"}),
    )
    .await;
    assert_eq!(s, 400, "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("context too long"),
        "{v}"
    );
}

#[tokio::test]
async fn speak_answers_one_wav_in_the_draft_s_voice() {
    let w = world(|_| {}).await;
    let tts_rows = |rows: &[(String, String)]| rows.iter().filter(|r| r.1 == "speak").count();
    let before = tts_rows(&rows(&w).await);
    let r = w
        .post(
            "/chat/api/profiles/speak",
            json!({"profile": {"voice": {"voice": "cosette", "speech_style": "calm"}},
                   "text": "One sentence. And another one."}),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "audio/wav");
    let wav = r.bytes().await.unwrap();
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 24_000);
    let data = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
    assert_eq!(data, wav.len() - 44);
    assert!(data > 24_000, "the clauses' audio, joined: {data} bytes");

    // The draft's voice reached the TTS; one TTS row.
    assert!(w.tts.seen.count() >= 1);
    assert_eq!(w.tts.seen.body(0)["voice"], "cosette");
    let after = rows(&w).await;
    assert_eq!(tts_rows(&after), before + 1, "{after:?}");
}

#[tokio::test]
async fn speak_refusals_are_flat_json() {
    // No text-to-speech model anywhere.
    let w = world(|s| s.chat_tts_alias.clear()).await;
    let (s, v) = post(
        &w,
        "/chat/api/profiles/speak",
        json!({"profile": {}, "text": "Hello."}),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (400, Some("tts_not_configured")),
        "{v}"
    );
    assert!(v["message"].is_string() && v.get("error").is_none(), "{v}");

    // A voice the model lacks.
    let w = world(|_| {}).await;
    let (s, v) = post(
        &w,
        "/chat/api/profiles/speak",
        json!({"profile": {"voice": {"voice": "nobody"}}, "text": "Hello."}),
    )
    .await;
    assert!(s >= 400, "{s} {v}");
    assert!(v["code"].as_str().unwrap().starts_with("voice_"), "{v}");
    // Nothing to say.
    let (s, v) = post(
        &w,
        "/chat/api/profiles/speak",
        json!({"profile": {}, "text": " "}),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (400, Some("empty_message")), "{v}");
    assert_eq!(w.tts.seen.count(), 0);
}

#[tokio::test]
async fn a_device_tries_as_its_key() {
    let w = world(|_| {}).await;
    let d = crate::device_chat::pair(
        &w,
        "phone",
        json!({ "scope_mode": "allow", "scope_patterns": "chatty" }),
    )
    .await;
    let post_as = |path: &'static str, body: Value| {
        let (w, client) = (&w, d.client.clone());
        async move { crate::device_chat::post(w, &client, path, body).await }
    };

    // Within its scope: a test, charged to its key.
    let (s, v) = post_as(
        "/chat/api/profiles/test",
        json!({"profile": draft(), "model": "chatty", "text": "Hi"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let key: Option<String> =
        sqlx::query_scalar("SELECT client_key FROM request_logs ORDER BY id DESC LIMIT 1")
            .fetch_one(&w.state.db)
            .await
            .unwrap();
    assert_eq!(key.as_deref(), Some("device:phone"));

    // Outside it: the count, the test and the speech are its key's refusals.
    let (s, v) = post_as(
        "/chat/api/profiles/preview",
        json!({"profile": draft(), "model": "other"}),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (403, Some("key_scope")), "{v}");
    // The TTS the draft resolves to (Settings' "speak") is outside it too:
    // refused before anything looks at it (review fix 5) — the preview,
    // whose voice turn reads its speech profile, and a voice test.
    let (s, v) = post_as(
        "/chat/api/profiles/preview",
        json!({"profile": draft(), "model": "chatty"}),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (403, Some("key_scope")), "{v}");
    assert!(v["message"].as_str().unwrap().contains("speak"), "{v}");
    let (s, v) = post_as(
        "/chat/api/profiles/test",
        json!({"profile": draft(), "model": "chatty", "text": "Hi", "voice": true}),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (403, Some("key_scope")), "{v}");
    assert!(v["message"].as_str().unwrap().contains("speak"), "{v}");
    // A draft naming a voice the TTS lacks: still the scope's refusal, not
    // the voice list's (`voice_not_found`), which would have asked the TTS.
    let (s, v) = post_as(
        "/chat/api/profiles/speak",
        json!({"profile": {"voice": {"voice": "nobody"}}, "text": "Hello."}),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (403, Some("key_scope")), "{v}");
    let (s, v) = post_as(
        "/chat/api/profiles/test",
        json!({"profile": draft(), "model": "other", "text": "Hi"}),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (403, Some("key_scope")), "{v}");
    let (s, v) = post_as(
        "/chat/api/profiles/speak",
        json!({"profile": {}, "text": "Hello."}),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (403, Some("key_scope")), "{v}");
    assert_eq!(w.tts.seen.count(), 0);
}
