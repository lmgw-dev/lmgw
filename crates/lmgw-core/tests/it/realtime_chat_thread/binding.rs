//! The binding (§8.1): decided at the handshake, for the Chat capability
//! (client-apps design §1.3) only, never for an Admin Chat thread; a second
//! bind takes over; what the thread owns stays the thread's; Keep waits.

use futures::StreamExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use super::{next, world};
use crate::support::realtime_fakes::{next_event, send};

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn the_cookie_binds_and_the_session_starts_as_the_thread_chose() {
    let w = world(|s| {
        // A stale default model cannot fail a bound session (§8.1).
        s.realtime.default_model = "gone".into();
    })
    .await;
    let tid = w.thread("other", json!({})).await;
    let (_ws, created) = w.bind(tid).await;
    let s = &created["session"];
    assert_eq!(s["model"], "other", "{created}");
    let resolved = &s["lmgw"]["resolved"];
    assert_eq!(
        resolved["chat_thread"],
        json!({"id": tid, "title": "New chat", "temporary": false, "admin_tools": false})
    );
    assert_eq!(
        (&resolved["chat"], &resolved["asr"], &resolved["tts"]),
        (&json!("other"), &json!("hear"), &json!("speak"))
    );
    let input = &s["audio"]["input"];
    assert_eq!(input["transcription"]["model"], "hear");
    assert_eq!(input["transcription"]["language"], "de");
    assert_eq!(input["turn_detection"]["type"], "semantic_vad", "{s}");
    assert_eq!(s["lmgw"]["tts_model"], "speak");

    // An unbound session says nothing of a thread.
    let mut plain = w.connect("model=chatty", &[]).await.unwrap();
    let created = next_event(&mut plain).await;
    assert!(
        created["session"]["lmgw"]["resolved"]
            .get("chat_thread")
            .is_none(),
        "{created}"
    );
}

#[tokio::test]
async fn binding_is_refused_before_the_101_as_the_table_says() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    sqlx::query("INSERT INTO api_keys (name, key_hash, enabled) VALUES ('client', ?1, 1)")
        .bind(lmgw_core::config::hash_api_key("lmgw-client-key"))
        .execute(&w.state.db)
        .await
        .unwrap();
    w.state.reload_snapshot().await.unwrap();

    // A client key: an inference credential, not the dashboard's.
    let bearer = "Bearer lmgw-client-key";
    let (status, body) = w
        .connect(&format!("chat_thread={tid}"), &[("authorization", bearer)])
        .await
        .unwrap_err();
    assert_eq!(
        (status, code(&body)),
        (403, "chat_thread_not_allowed"),
        "{body}"
    );
    // Without any credential (Require API key off): the same.
    let (status, body) = w
        .connect(&format!("chat_thread={tid}"), &[])
        .await
        .unwrap_err();
    assert_eq!(
        (status, code(&body)),
        (403, "chat_thread_not_allowed"),
        "{body}"
    );

    let cookie = w.cookie();
    let with = [("cookie", cookie.as_str())];
    let (status, body) = w
        .connect(&format!("model=chatty&chat_thread={tid}"), &with)
        .await
        .unwrap_err();
    assert_eq!((status, code(&body)), (400, "owned_by_thread"), "{body}");
    let (status, body) = w.connect("chat_thread=9999", &with).await.unwrap_err();
    assert_eq!(
        (status, code(&body)),
        (404, "chat_thread_not_found"),
        "{body}"
    );

    let r = w
        .post(
            "/chat/api/threads",
            json!({"model_alias": "chatty", "kind": "admin"}),
        )
        .await;
    let admin = r.json::<Value>().await.unwrap()["id"].as_i64().unwrap();
    let (status, body) = w
        .connect(&format!("chat_thread={admin}"), &with)
        .await
        .unwrap_err();
    assert_eq!((status, code(&body)), (409, "chat_thread_admin"), "{body}");

    // The owner's key as a bearer is the dashboard's capability too.
    let owner = format!("Bearer {}", w.gw.key);
    assert!(w
        .connect(&format!("chat_thread={tid}"), &[("authorization", &owner)])
        .await
        .is_ok());
}

#[tokio::test]
async fn a_plain_thread_with_the_admin_tools_binds_and_its_json_says_so() {
    let w = world(|s| s.self_admin = lmgw_core::config::SelfAdmin::Full).await;
    let tid = w.thread("chatty", json!({})).await;
    let r = w
        .post(
            &format!("/chat/api/threads/{tid}/settings"),
            json!({"mcp_tools": [{"server_label": "lmgw"}]}),
        )
        .await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let thread = w.get(&format!("/chat/api/threads/{tid}")).await;
    let rt = &thread["thread"]["voice_resolved"]["realtime"];
    assert_eq!(
        (&rt["ok"], &rt["admin_tools"]),
        (&json!(true), &json!(true))
    );
    let (_ws, created) = w.bind(tid).await;
    let bound = &created["session"]["lmgw"]["resolved"]["chat_thread"];
    assert_eq!(
        (&bound["id"], &bound["admin_tools"]),
        (&json!(tid), &json!(true))
    );
}

/// WP8 review m10: the admin-tools flag is live in a session — a thread
/// that gains the toolset mid-session says so before the next turn runs,
/// as does a title its first spoken turn named.
#[tokio::test]
async fn the_thread_s_flag_and_title_follow_it_through_the_session() {
    use super::{say, until_type};
    use crate::support::realtime_audio::Asr;
    use crate::support::realtime_fakes::Turn;
    let w = world(|s| s.self_admin = lmgw_core::config::SelfAdmin::Full).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    // The first turn names the thread.
    w.asr.push(Asr::Text("Wie wird das Wetter?"));
    w.chat.push(Turn::text(&["Sonnig."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let named: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "lmgw.chat.thread")
        .collect();
    assert_eq!(named.len(), 1, "{events:?}");
    assert_eq!(named[0]["chat_thread"]["admin_tools"], false);
    let title = named[0]["chat_thread"]["title"].clone();
    assert_ne!(title, "New chat", "{title}");
    // Another window attaches the self-admin toolset.
    w.set(tid, json!({"mcp_tools": [{"server_label": "lmgw"}]}))
        .await;
    w.asr.push(Asr::Text("Und morgen?"));
    w.chat.push(Turn::text(&["Regen."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let at = |t: &str| events.iter().position(|e| e["type"] == t).unwrap();
    let flagged = &events[at("lmgw.chat.thread")];
    assert_eq!(
        flagged["chat_thread"],
        json!({"id": tid, "title": title, "temporary": false, "admin_tools": true})
    );
    assert!(
        at("lmgw.chat.thread") < at("lmgw.chat.frame"),
        "before the turn's frames"
    );
    // Nothing changed since: said no more.
    w.asr.push(Asr::Text("Danke."));
    w.chat.push(Turn::text(&["Gern."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    assert!(
        !events.iter().any(|e| e["type"] == "lmgw.chat.thread"),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_second_bind_takes_the_thread_over() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let (mut first, _) = w.bind(tid).await;
    let (_second, _) = w.bind(tid).await;
    let e = next(&mut first).await;
    assert_eq!(code(&e), "chat_thread_taken_over", "{e}");
    let close = loop {
        match first.next().await {
            Some(Ok(Message::Close(c))) => break c,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            other => panic!("expected the close, got {other:?}"),
        }
    };
    let close = close.expect("a close frame with a reason");
    assert_eq!(u16::from(close.code), 4000);
    // The close names who took it over (client-apps design §1.7): the
    // second bind was the dashboard's own.
    assert_eq!(close.reason.as_str(), "voice mode moved to the dashboard");
}

#[tokio::test]
async fn what_the_thread_owns_is_refused_and_an_echo_is_not() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let (mut ws, created) = w.bind(tid).await;

    // The session echoed back whole, and the client's own fields.
    let mut echo = created["session"].clone();
    echo["type"] = json!("realtime");
    send(&mut ws, json!({"type": "session.update", "session": echo})).await;
    assert_eq!(next(&mut ws).await["type"], "session.updated");
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
            "audio": {"input": {"turn_detection": null}}, "output_modalities": ["text"]}}),
    )
    .await;
    assert_eq!(next(&mut ws).await["type"], "session.updated");

    for (update, param) in [
        (json!({"model": "other"}), "session.model"),
        (
            json!({"audio": {"output": {"voice": "cosette"}}}),
            "session.audio.output.voice",
        ),
        (
            json!({"lmgw": {"tts_model": null}}),
            "session.lmgw.tts_model",
        ),
        (json!({"instructions": "Be terse."}), "session.instructions"),
    ] {
        let mut session = update.clone();
        session["type"] = json!("realtime");
        send(
            &mut ws,
            json!({"type": "session.update", "event_id": "u", "session": session}),
        )
        .await;
        let e = next(&mut ws).await;
        assert_eq!(code(&e), "owned_by_thread", "{update}: {e}");
        assert_eq!(e["error"]["param"], param, "{e}");
    }
    send(
        &mut ws,
        json!({"type": "response.create", "response": {"instructions": "x"}}),
    )
    .await;
    let e = next(&mut ws).await;
    assert_eq!(code(&e), "owned_by_thread", "{e}");
    for event in [
        json!({"type": "conversation.item.create", "item": {"type": "message",
            "role": "user", "content": [{"type": "input_text", "text": "hi"}]}}),
        json!({"type": "conversation.item.delete", "item_id": "item_x"}),
    ] {
        send(&mut ws, event).await;
        let e = next(&mut ws).await;
        assert_eq!(code(&e), "owned_by_thread", "{e}");
    }
}

#[tokio::test]
async fn keep_waits_while_a_temporary_thread_is_bound() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({"temporary": true})).await;
    assert!(tid < 0);
    let (ws, created) = w.bind(tid).await;
    assert_eq!(
        created["session"]["lmgw"]["resolved"]["chat_thread"]["temporary"],
        true
    );
    let r = w
        .post(&format!("/chat/api/threads/{tid}/persist"), json!({}))
        .await;
    assert_eq!(r.status(), 409);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["code"], "voice_session_active", "{v}");

    // Voice mode left: kept.
    drop(ws);
    super::eventually("the binding to go", || async {
        w.post(&format!("/chat/api/threads/{tid}/persist"), json!({}))
            .await
            .status()
            == 200
    })
    .await;
}

/// The dashboard's cookie replayed by a page of another origin binds
/// nothing (the principal middleware's same-origin rule, `/v1/realtime`
/// included; WP8 review, coverage).
#[tokio::test]
async fn a_cookie_from_another_origin_does_not_bind() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let cookie = w.cookie();
    let (status, body) = w
        .connect(
            &format!("chat_thread={tid}"),
            &[("cookie", &cookie), ("origin", "http://127.0.0.1:1")],
        )
        .await
        .unwrap_err();
    assert_eq!(status, 403, "{body}");
    // The same cookie from the gateway's own page binds.
    let own = format!("http://{}", w.addr());
    assert!(w
        .connect(
            &format!("chat_thread={tid}"),
            &[("cookie", &cookie), ("origin", &own)],
        )
        .await
        .is_ok());
}

/// A thread deleted mid-session fails the next response with
/// `chat_thread_not_found`, and writes nothing (§8.3; WP8 review,
/// coverage).
#[tokio::test]
async fn a_thread_deleted_mid_session_fails_the_next_response() {
    use super::{say, until_type};
    use crate::support::realtime_audio::Asr;
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    let r = w
        .post(&format!("/chat/api/threads/{tid}/delete"), json!({}))
        .await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    w.asr.push(Asr::Text("Noch da?"));
    say(&mut ws).await;
    let events = until_type(&mut ws, "response.done").await;
    let e: Vec<&Value> = events.iter().filter(|e| e["type"] == "error").collect();
    assert_eq!(e[0]["error"]["code"], "chat_thread_not_found", "{events:?}");
    // A request error, as the stock session types its refusals of this
    // kind, not a permission one (WP11 binding review NIT 2).
    assert_eq!(e[0]["error"]["type"], "invalid_request_error", "{events:?}");
    assert!(e[0]["error"]["param"].is_null(), "{events:?}");
    assert_eq!(w.chat.seen.chat_count(), 0, "no model call");
}

/// WP11 binding review NIT 1: a bound turn reads the thread's ASR when its
/// call starts, both ways — a session bound with none takes commits, whose
/// turns fail `asr_not_configured` until the thread names one, and a thread
/// whose ASR went away since the bind fails the turn instead of keeping the
/// bind's alias.
#[tokio::test]
async fn a_bound_turn_reads_the_thread_s_asr_both_ways() {
    use crate::support::realtime_audio::{append, silence, Asr};
    let w = world(|s| {
        s.chat_stt_alias = String::new();
        s.realtime.asr_alias = String::new();
    })
    .await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    let transcribed = |e: &Value| {
        let t = e["type"].as_str().unwrap_or("");
        t == "conversation.item.input_audio_transcription.failed"
            || t == "conversation.item.input_audio_transcription.completed"
            || t == "error"
    };

    // None at the bind: the commit is taken, and its turn fails.
    append(&mut ws, &silence(200)).await;
    send(&mut ws, json!({"type": "input_audio_buffer.commit"})).await;
    let ev = super::until(&mut ws, transcribed).await;
    let last = ev.last().unwrap();
    assert_eq!(
        last["type"], "conversation.item.input_audio_transcription.failed",
        "{ev:?}"
    );
    assert_eq!(last["error"]["code"], "asr_not_configured", "{last}");

    // The chip set: the next turn is transcribed.
    w.set(
        tid,
        json!({"voice": {"asr_alias": "hear", "language": "de"}}),
    )
    .await;
    w.asr.push(Asr::Text("Jetzt geht es."));
    append(&mut ws, &silence(200)).await;
    send(&mut ws, json!({"type": "input_audio_buffer.commit"})).await;
    let ev = super::until(&mut ws, transcribed).await;
    let last = ev.last().unwrap();
    assert_eq!(last["transcript"], "Jetzt geht es.", "{ev:?}");

    // Gone again: the turn fails rather than keep the alias.
    w.set(tid, json!({"voice": {"language": "de"}})).await;
    append(&mut ws, &silence(200)).await;
    send(&mut ws, json!({"type": "input_audio_buffer.commit"})).await;
    let ev = super::until(&mut ws, transcribed).await;
    assert_eq!(
        ev.last().unwrap()["error"]["code"],
        "asr_not_configured",
        "{ev:?}"
    );
    assert_eq!(
        w.asr.seen.count(),
        1,
        "only the turn with an alias was sent"
    );
}
