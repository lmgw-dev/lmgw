//! Golden system messages (personality-profiles design §2.2, §6): what a
//! Chat turn sends as its system message, and the reasoning it asks for,
//! byte for byte, for a thread with **no profile** — a text turn, a
//! `speak: true` turn, a text-output bound session's turn, a voice turn and
//! their language and voice-instruction variants, and an Admin Chat turn.
//! Captured before profiles reached the prompt, so a thread without one is
//! proven to send exactly what it sent before.
//!
//! The file is `tests/fixtures/chat_system/no_profile.json`; today's date
//! is written as `<today>`. `LMGW_BLESS=1` rewrites it.
//!
//! The turn drivers are shared with `chat_profiles_prompt`.

use lmgw_core::config::SelfAdmin;
use serde_json::{json, Map, Value};

use crate::realtime_chat_thread::{self as rt, say, settings, until_type, World};
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::send as ws_send;
use crate::support::realtime_tts::add_described_cloud_tts_alias;

/// Today as `{{date}}` expands to it.
fn today() -> String {
    chrono::Local::now()
        .date_naive()
        .format("%A, %-d %B %Y")
        .to_string()
}

/// What a request carried, as the goldens compare it: its system message
/// (`null` when there is none) with today's date as `<today>`, and its
/// `reasoning_effort` and `chat_template_kwargs` (`null` when absent).
pub(crate) fn sent(body: &Value) -> Value {
    let first = &body["messages"][0];
    let system = (first["role"] == "system").then(|| {
        first["content"]
            .as_str()
            .unwrap_or_else(|| panic!("a system message that is not a text: {body}"))
            .replace(&today(), "<today>")
    });
    json!({
        "system": system,
        "reasoning_effort": body.get("reasoning_effort").cloned().unwrap_or(Value::Null),
        "chat_template_kwargs": body.get("chat_template_kwargs").cloned().unwrap_or(Value::Null),
    })
}

/// The last request the chat fake saw, as [`sent`] puts it.
pub(crate) fn last_sent(w: &World) -> Value {
    sent(&w.chat.seen.chat(w.chat.seen.chat_count() - 1))
}

/// A thread on `chatty` of `kind` (`None`: a plain chat), with no voice
/// language, and `system_prompt` written into its settings (`None`: the
/// default it starts with).
pub(crate) async fn plain_thread(w: &World, kind: Option<&str>, prompt: Option<&str>) -> i64 {
    let mut b = json!({"model_alias": "chatty"});
    if let Some(k) = kind {
        b["kind"] = json!(k);
    }
    let r = w.post("/chat/api/threads", b).await;
    assert_eq!(r.status(), 200);
    let tid = r.json::<Value>().await.unwrap()["id"].as_i64().unwrap();
    if let Some(p) = prompt {
        w.set(tid, json!({ "system_prompt": p })).await;
    }
    tid
}

/// A text turn on `tid` (`speak`: read aloud): what it sent.
pub(crate) async fn text_turn(w: &World, tid: i64, speak: bool) -> Value {
    let r = w
        .post(
            &format!("/chat/api/threads/{tid}/send"),
            json!({"content": "Hallo.", "speak": speak}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(body.contains("event: done"), "{body}");
    last_sent(w)
}

/// A spoken turn of a session bound to `tid` with audio output: what it
/// sent.
pub(crate) async fn voice_turn(w: &World, tid: i64) -> Value {
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Frage."));
    say(&mut ws).await;
    until_type(&mut ws, "lmgw.response.timing").await;
    last_sent(w)
}

/// A spoken turn of a session bound to `tid` with text output: what it
/// sent.
pub(crate) async fn text_output_turn(w: &World, tid: i64) -> Value {
    let (mut ws, _) = w.bind(tid).await;
    ws_send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
            "output_modalities": ["text"], "audio": {"input": {"turn_detection": null}}}}),
    )
    .await;
    assert_eq!(rt::next(&mut ws).await["type"], "session.updated");
    w.asr.push(Asr::Text("Frage."));
    say(&mut ws).await;
    until_type(&mut ws, "lmgw.response.timing").await;
    last_sent(w)
}

/// Compare `got` with the golden file `name` (or write it under
/// `LMGW_BLESS`).
pub(crate) fn golden(name: &str, got: &Map<String, Value>) {
    let file = format!(
        "{}/tests/fixtures/chat_system/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let got = serde_json::to_string_pretty(got).unwrap() + "\n";
    if std::env::var_os("LMGW_BLESS").is_some() {
        std::fs::create_dir_all(std::path::Path::new(&file).parent().unwrap()).unwrap();
        std::fs::write(&file, &got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&file)
        .unwrap_or_else(|e| panic!("{file}: {e} — run the suite with LMGW_BLESS=1 to capture it"));
    let (want_v, got_v): (Value, Value) = (
        serde_json::from_str(&want).unwrap(),
        serde_json::from_str(&got).unwrap(),
    );
    if want_v != got_v {
        for (k, v) in got_v.as_object().unwrap() {
            assert_eq!(
                Some(v),
                want_v.get(k),
                "the system message of '{k}' changed ({file})"
            );
        }
        panic!("the cases of {file} changed\n--- want\n{want}\n--- got\n{got}");
    }
}

/// Every kind of turn of a thread with no profile, against the goldens.
#[tokio::test]
async fn a_thread_without_a_profile_sends_what_it_always_sent() {
    let w = rt::world(|_| {}).await;
    let mut cases = Map::new();

    // Text turns: the built-in default prompt, an own one, none.
    let builtin = plain_thread(&w, None, None).await;
    cases.insert("text_builtin".into(), text_turn(&w, builtin, false).await);
    let own = plain_thread(&w, None, Some("Du bist ein Assistent.")).await;
    cases.insert("text_own".into(), text_turn(&w, own, false).await);
    let empty = plain_thread(&w, None, Some("")).await;
    cases.insert("text_empty".into(), text_turn(&w, empty, false).await);
    // Explicit reasoning on a text turn.
    let thinking = plain_thread(&w, None, Some("P")).await;
    w.set(
        thinking,
        json!({"reasoning_enabled": true, "reasoning_effort": "high"}),
    )
    .await;
    cases.insert(
        "text_reasoning_high".into(),
        text_turn(&w, thinking, false).await,
    );

    // Read aloud, without and with a reply language, and two languages.
    cases.insert("speak_builtin".into(), text_turn(&w, builtin, true).await);
    w.set(own, json!({"voice": {"language": "de"}})).await;
    cases.insert("speak_own_de".into(), text_turn(&w, own, true).await);
    let split = plain_thread(&w, None, Some("Du bist ein Assistent.")).await;
    w.set(
        split,
        json!({"voice": {"language": "de", "reply_language": "en"}}),
    )
    .await;
    cases.insert("speak_split".into(), text_turn(&w, split, true).await);

    // A text-output bound session.
    cases.insert("text_output_own_de".into(), text_output_turn(&w, own).await);
    cases.insert(
        "text_output_builtin".into(),
        text_output_turn(&w, builtin).await,
    );

    // Voice turns: the built-in prompt (it says the date), an own one
    // (it does not), none, with and without a language.
    cases.insert("voice_builtin".into(), voice_turn(&w, builtin).await);
    cases.insert("voice_own_de".into(), voice_turn(&w, own).await);
    let own_plain = plain_thread(&w, None, Some("Du bist ein Assistent.")).await;
    cases.insert("voice_own".into(), voice_turn(&w, own_plain).await);
    cases.insert("voice_empty".into(), voice_turn(&w, empty).await);
    cases.insert("voice_split".into(), voice_turn(&w, split).await);
    cases.insert(
        "voice_reasoning_high".into(),
        voice_turn(&w, thinking).await,
    );
    let dated = plain_thread(&w, None, Some("Heute ist {{date}}.")).await;
    cases.insert("voice_dated".into(), voice_turn(&w, dated).await);

    // A TTS that takes cues: its hint closes the block.
    add_described_cloud_tts_alias(
        &w.state,
        &w.tts,
        "cloud-tts",
        json!({"instructions": "style"}),
    )
    .await;
    let cued = plain_thread(&w, None, Some("Du bist ein Assistent.")).await;
    w.set(
        cued,
        json!({"voice": {"tts_alias": "cloud-tts", "language": "de"}}),
    )
    .await;
    cases.insert("voice_hint_de".into(), voice_turn(&w, cued).await);

    // realtime.default_instructions: the owner's own text, then none.
    settings(&w.state, |s| {
        s.realtime.default_instructions = Some("Sprich kurz.".into())
    })
    .await;
    cases.insert("voice_own_instructions".into(), voice_turn(&w, own).await);
    cases.insert(
        "voice_own_instructions_hint".into(),
        voice_turn(&w, cued).await,
    );
    settings(&w.state, |s| {
        s.realtime.default_instructions = Some(String::new())
    })
    .await;
    cases.insert("voice_no_instructions".into(), voice_turn(&w, own).await);
    cases.insert(
        "voice_no_instructions_plain".into(),
        voice_turn(&w, own_plain).await,
    );
    settings(&w.state, |s| s.realtime.default_instructions = None).await;

    // Admin Chat: the wrapper around the thread's prompt.
    settings(&w.state, |s| s.self_admin = SelfAdmin::ReadOnly).await;
    let admin = plain_thread(&w, Some("admin"), Some("Du bist ein Assistent.")).await;
    cases.insert("admin_text".into(), text_turn(&w, admin, false).await);
    let admin_empty = plain_thread(&w, Some("admin"), Some("")).await;
    cases.insert(
        "admin_text_empty".into(),
        text_turn(&w, admin_empty, false).await,
    );

    golden("no_profile", &cases);
}
