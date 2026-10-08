//! A bound session's end (§8.6, §8.1): the turns committed and not yet
//! answered are written once transcribed (WP8 review m9), and a takeover's
//! two journals write one history one after the other — the newer session
//! writes only once the older one drained (m7).

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::Notify;

use super::{eventually, manual, say, until, until_reply, world};
use crate::support::realtime_audio::{append, silence, Asr};
use crate::support::realtime_fakes::{send, Step, Turn};

/// `(role, content)` of the thread's messages, trimmed (a partial reply
/// keeps its trailing space).
async fn rows(w: &super::World, tid: i64) -> Vec<(String, String)> {
    w.messages(tid)
        .await
        .into_iter()
        .map(|(r, c, _)| (r, c.trim().to_string()))
        .collect()
}

#[tokio::test]
async fn a_turn_still_being_transcribed_at_disconnect_is_written() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    // "Thanks, bye", then Esc while it is still being transcribed.
    let hold = Arc::new(Notify::new());
    w.asr.push(Asr::HeldText(hold.clone(), "Danke, tschüss."));
    say(&mut ws).await;
    until(&mut ws, |e| e["type"] == "response.created").await;
    drop(ws);
    hold.notify_one();
    eventually("the last words written", || async {
        rows(&w, tid).await == [("user".to_string(), "Danke, tschüss.".to_string())]
    })
    .await;
    let m = w.messages(tid).await;
    assert_eq!(m[0].2["via"], "realtime", "{}", m[0].2);
    assert_eq!(w.chat.seen.chat_count(), 0, "nothing answers it now");
}

/// The page leaves with a close of its own and reads the thread again when
/// the browser says the socket closed (WP9 review m5): the server's close
/// answer must come only once the journal drained — here, after the last
/// turn's transcript, held, was written.
#[tokio::test]
async fn the_server_s_close_comes_once_the_journal_drained() {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    let hold = Arc::new(Notify::new());
    w.asr.push(Asr::HeldText(hold.clone(), "Danke, tschüss."));
    say(&mut ws).await;
    until(&mut ws, |e| e["type"] == "response.created").await;
    ws.close(None).await.unwrap();
    // Nothing closes while the transcript is still being made.
    let early = tokio::time::timeout(Duration::from_millis(400), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_))) => continue,
                other => return format!("{other:?}"),
            }
        }
    })
    .await;
    assert!(early.is_err(), "closed before the drain: {early:?}");
    hold.notify_one();
    while let Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_))) = ws.next().await {}
    assert_eq!(
        rows(&w, tid).await,
        [("user".to_string(), "Danke, tschüss.".to_string())],
        "written before the close came"
    );
}

#[tokio::test]
async fn a_takeover_writes_the_older_session_s_history_first() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut first = w.voice(tid).await;
    // The older session answers, and a second turn is committed (no
    // response asked for) while its transcript is still being made.
    let chat_hold = Arc::new(Notify::new());
    w.asr.push(Asr::Text("Erste Frage."));
    w.chat.push(Turn::Stream(vec![
        Step::Text("Es war einmal ein König. "),
        Step::Wait(chat_hold.clone()),
        Step::Text("Er war alt."),
        Step::Finish("stop"),
    ]));
    say(&mut first).await;
    until(&mut first, |e| e["type"] == "response.output_audio.delta").await;
    let asr_hold = Arc::new(Notify::new());
    w.asr.push(Asr::HeldText(asr_hold.clone(), "Noch was."));
    append(&mut first, &silence(200)).await;
    send(&mut first, json!({"type": "input_audio_buffer.commit"})).await;
    until(&mut first, |e| e["type"] == "input_audio_buffer.committed").await;

    // Another window takes the thread over and asks at once.
    let (mut second, _) = w.bind(tid).await;
    manual(&mut second, 60_000).await;
    w.asr.push(Asr::Text("Neue Frage."));
    w.chat.push(Turn::text(&["Neue Antwort."]));
    say(&mut second).await;
    until(&mut second, |e| e["type"] == "response.created").await;
    // The older session still waits for its last transcript: the newer
    // one writes nothing and asks nothing meanwhile.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(w.chat.seen.chat_count(), 1, "only the older turn asked");
    asr_hold.notify_one();
    until_reply(&mut second, "Neue Antwort.").await;
    chat_hold.notify_one();
    drop(first);

    let rows = rows(&w, tid).await;
    let rows: Vec<(&str, &str)> = rows.iter().map(|(r, c)| (r.as_str(), c.as_str())).collect();
    assert_eq!(
        rows,
        [
            ("user", "Erste Frage."),
            ("assistant", "Es war einmal ein König."),
            ("user", "Noch was."),
            ("user", "Neue Frage."),
            ("assistant", "Neue Antwort."),
        ],
        "the older session's history, then the newer one's"
    );
    // The newer turn answered both of the older session's last words.
    let body = w.chat.seen.chat(1);
    let last = body["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["content"], "Noch was.\n\nNeue Frage.", "{body}");
}

/// WP11 binding review M1: the page leaves — truncate, then close — while
/// a reply is still paced out, so the writer has output after the client's
/// close. The server's answer is a close frame (not a dropped connection,
/// which the browser reports at once as 1006), and it comes only once the
/// journal drained: the reply cut at what was heard and the last turn,
/// still being transcribed at the leave, are written by then. Keep right
/// after it is not refused.
#[tokio::test]
async fn leaving_mid_reply_gets_the_close_after_the_drain() {
    use futures::StreamExt;
    use serde_json::Value;
    use tokio_tungstenite::tungstenite::Message;

    use crate::support::realtime_tts::{speech, wav};
    let w = world(|_| {}).await;
    // One clause a second, and 300 ms of lead: the rest is paced.
    w.tts.set_default(wav(&speech(1000), 24_000));
    let tid = w.thread("chatty", json!({"temporary": true})).await;
    let (mut ws, _) = w.bind(tid).await;
    manual(&mut ws, 300).await;
    w.asr.push(Asr::Text("Erzähl was."));
    w.chat.push(Turn::text(&[
        "Es war einmal ein König. ",
        "Er war alt. ",
        "Und sehr müde.",
    ]));
    say(&mut ws).await;
    let events = until(&mut ws, |e| e["type"] == "response.output_audio.delta").await;
    let item = events
        .iter()
        .find(|e| e["type"] == "response.output_item.added" && e["item"]["type"] == "message")
        .map(|e| e["item"]["id"].as_str().unwrap().to_string())
        .unwrap();
    // A last turn, committed while the reply plays, still being transcribed
    // when the page leaves.
    let hold = Arc::new(Notify::new());
    w.asr.push(Asr::HeldText(hold.clone(), "Noch was."));
    append(&mut ws, &silence(200)).await;
    send(&mut ws, json!({"type": "input_audio_buffer.commit"})).await;
    until(&mut ws, |e| e["type"] == "input_audio_buffer.committed").await;
    send(
        &mut ws,
        json!({"type": "conversation.item.truncate", "item_id": item, "content_index": 0,
               "audio_end_ms": 200}),
    )
    .await;
    ws.close(None).await.unwrap();
    // The paced audio's next chunk is due meanwhile: the socket stays.
    let early = tokio::time::timeout(Duration::from_millis(600), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_))) => continue,
                other => return format!("{other:?}"),
            }
        }
    })
    .await;
    assert!(early.is_err(), "closed before the drain: {early:?}");
    hold.notify_one();
    let end = loop {
        match ws.next().await {
            Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_))) => continue,
            other => break other,
        }
    };
    assert!(
        matches!(end, Some(Ok(Message::Close(_)))),
        "a close frame, not a dropped connection: {end:?}"
    );
    // Everything was written before the close came.
    let m = w.messages(tid).await;
    let roles: Vec<&str> = m.iter().map(|(r, _, _)| r.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user"], "{m:?}");
    assert_eq!(m[0].1, "Erzähl was.");
    assert_eq!(m[2].1, "Noch was.");
    let (_, reply, voice) = &m[1];
    assert!(
        voice["unheard"].as_str().is_some_and(|u| !u.is_empty()),
        "the reply is cut at what was heard: {reply:?} {voice}"
    );
    // Keep, right after the close: the binding is gone.
    let r = w
        .post(&format!("/chat/api/threads/{tid}/persist"), json!({}))
        .await;
    let status = r.status();
    let body: Value = r.json().await.unwrap_or_default();
    assert_eq!(status, 200, "{body}");
}

/// WP11 binding review m1: a last transcript that never comes holds the
/// session's end for `realtime.ping_interval_s` and no longer — its call is
/// stopped, and its words are lost with a WARN — so a window that
/// re-entered the thread meanwhile writes and answers once the older
/// session let go, instead of waiting behind its fence for good.
#[tokio::test]
async fn a_last_transcript_that_never_comes_holds_the_end_for_one_ping_interval() {
    let w = world(|s| s.realtime.ping_interval_s = 1).await;
    let tid = w.thread("chatty", json!({})).await;
    let mut first = w.voice(tid).await;
    let hold = Arc::new(Notify::new());
    w.asr.push(Asr::HeldText(hold.clone(), "Verloren."));
    say(&mut first).await;
    until(&mut first, |e| e["type"] == "response.created").await;
    // The window re-enters voice mode: it takes the thread over.
    let (mut second, _) = w.bind(tid).await;
    manual(&mut second, 60_000).await;
    w.asr.push(Asr::Text("Neue Frage."));
    w.chat.push(Turn::text(&["Neue Antwort."]));
    let started = std::time::Instant::now();
    say(&mut second).await;
    tokio::time::timeout(
        Duration::from_secs(10),
        until_reply(&mut second, "Neue Antwort."),
    )
    .await
    .expect("the newer window answers once the older session let go");
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "it waited for the older session's bound"
    );
    drop(first);
    assert_eq!(
        rows(&w, tid).await,
        [
            ("user".to_string(), "Neue Frage.".to_string()),
            ("assistant".to_string(), "Neue Antwort.".to_string()),
        ]
    );
    hold.notify_one();
}

/// RFC 6455 §5.5.1: the endpoint that receives a close answers with a close
/// of its own, echoing the status code. A client's 1000 is answered 1000,
/// not with an empty close a client reads as 1005.
#[tokio::test]
async fn a_client_s_close_is_answered_with_its_code() {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    use tokio_tungstenite::tungstenite::Message;

    use crate::support::realtime_tts::{speech, wav};
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    for (code, reason, mid_reply) in [
        (CloseCode::Normal, "bye", false),
        (CloseCode::Away, "", false),
        (CloseCode::Normal, "bye", true),
    ] {
        let (mut ws, _) = w.bind(tid).await;
        if mid_reply {
            // A reply still paced out, and audio streaming in, at the close.
            w.tts.set_default(wav(&speech(1000), 24_000));
            manual(&mut ws, 300).await;
            w.asr.push(Asr::Text("Erzähl was."));
            w.chat
                .push(Turn::text(&["Es war einmal ein König. ", "Er war alt."]));
            say(&mut ws).await;
            until(&mut ws, |e| e["type"] == "response.output_audio.delta").await;
            for _ in 0..20 {
                append(&mut ws, &silence(100)).await;
            }
        }
        ws.close(Some(CloseFrame {
            code,
            reason: reason.into(),
        }))
        .await
        .unwrap();
        let end = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match ws.next().await {
                    Some(Ok(Message::Close(c))) => break c,
                    Some(Ok(_)) => continue,
                    other => panic!("expected the close's answer, got {other:?}"),
                }
            }
        })
        .await
        .expect("the close is answered");
        assert_eq!(end.map(|c| c.code), Some(code), "the client's code, echoed");
    }
}
