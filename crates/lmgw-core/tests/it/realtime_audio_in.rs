//! Audio in, end to end (realtime design §2.3, §4.2, §4.3, §5.2, §10.3,
//! §16 "golden sequences"): the committed synthetic fixtures streamed as
//! 24 kHz appends through the real detector (Silero on ONNX Runtime), an ASR
//! fake answering fixed text, and the streaming chat fake.

use std::sync::Arc;

use lmgw_core::config::{KeyPolicy, ScopeMode};
use lmgw_core::state::SharedState;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::support::realtime_audio::{
    add_asr_alias, asr_fake, fixture, stream, Asr, AsrFake, ASR_ALIAS,
};
use crate::support::realtime_fakes::{
    chat_fake, events_until, gateway, next_event, open, send, types, ChatFake, Turn, Ws, KEY,
};

const QUESTION: &str = "Where is the nearest station?";

/// A gateway with the chat fake (`chatty`), the ASR fake (`hear`) and
/// `realtime.asr_alias` set to it.
async fn voice_gateway(
    auth: bool,
    policy: Option<KeyPolicy>,
) -> (SharedState, String, ChatFake, AsrFake) {
    let chat = chat_fake().await;
    let asr = asr_fake().await;
    let (state, addr) = gateway(&chat, auth, policy, |s| {
        s.realtime.asr_alias = ASR_ALIAS.into();
    })
    .await;
    add_asr_alias(&state, &asr).await;
    (state, addr, chat, asr)
}

/// Open a session on `chatty` with `headers` and send `session` (merged into
/// a text-output update); returns the socket and the `session.updated`.
async fn voice_session(addr: &str, headers: &[(&str, &str)], session: Value) -> (Ws, Value) {
    let mut ws = open(addr, "/v1/realtime?model=chatty", headers).await;
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    let mut update = json!({"type": "realtime", "output_modalities": ["text"]});
    for (k, v) in session.as_object().unwrap() {
        update[k] = v.clone();
    }
    send(
        &mut ws,
        json!({"type": "session.update", "session": update}),
    )
    .await;
    let updated = next_event(&mut ws).await;
    assert_eq!(updated["type"], "session.updated", "{updated}");
    (ws, updated)
}

/// Every event up to the answer to a retrieve of a missing item — the
/// client frames before it, and whatever they set off synchronously, came
/// first.
async fn until_sentinel(ws: &mut Ws) -> Vec<Value> {
    send(
        ws,
        json!({"type": "conversation.item.retrieve", "item_id": "nope", "event_id": "sentinel"}),
    )
    .await;
    let mut out = Vec::new();
    loop {
        let ev = next_event(ws).await;
        if ev["error"]["event_id"] == "sentinel" {
            return out;
        }
        out.push(ev);
    }
}

#[tokio::test]
async fn a_voice_turn_is_the_golden_sequence_with_text_out() {
    let (_s, addr, chat, asr) = voice_gateway(false, None).await;
    asr.push(Asr::Text(QUESTION));
    chat.push(Turn::text(&["Two ", "blocks ", "north."]));
    // What `@openai/agents` asks for, with text output.
    let (mut ws, updated) = voice_session(
        &addr,
        &[],
        json!({"audio": {"input": {"transcription": {"model": "gpt-4o-mini-transcribe"},
                                    "turn_detection": {"type": "server_vad"}}}}),
    )
    .await;
    let resolved = &updated["session"]["lmgw"]["resolved"];
    assert_eq!(resolved["asr"], ASR_ALIAS, "{updated}");
    assert_eq!(resolved["turn_detection"], "server_vad");

    stream(&mut ws, &fixture("en_complete_short.wav")).await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(
        types(&events),
        [
            "input_audio_buffer.speech_started",
            "input_audio_buffer.speech_stopped",
            "input_audio_buffer.committed",
            "conversation.item.added",
            "conversation.item.input_audio_transcription.completed",
            "conversation.item.done",
            "response.created",
            "response.output_item.added",
            "conversation.item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "conversation.item.done",
            "response.done",
        ]
    );
    // The transcription's usage is the committed segment's audio — the
    // field openai-python requires (§23, L3).
    let usage = &events[4]["usage"];
    assert_eq!(usage["type"], "duration", "{usage}");
    let seconds = usage["seconds"].as_f64().unwrap();
    assert!((0.5..5.0).contains(&seconds), "{seconds}");
    // One item, named by speech_started before it existed.
    let item = events[0]["item_id"].as_str().unwrap();
    assert!(item.starts_with("item_"));
    assert_eq!(events[1]["item_id"], item);
    assert_eq!(events[2]["item_id"], item);
    assert_eq!(events[2]["previous_item_id"], Value::Null);
    assert_eq!(events[3]["item"]["id"], item);
    assert_eq!(events[3]["previous_item_id"], Value::Null);
    assert_eq!(events[4]["item_id"], item);
    assert_eq!(events[4]["content_index"], 0);
    assert_eq!(events[5]["item"]["id"], item);

    // Added before the transcript exists: `transcript: null`, said so.
    let added = &events[3]["item"];
    assert_eq!(added["role"], "user");
    assert_eq!(added["content"][0]["type"], "input_audio");
    assert_eq!(added["content"][0].get("transcript"), Some(&Value::Null));
    assert_eq!(events[4]["transcript"], QUESTION);
    assert_eq!(events[5]["item"]["content"][0]["transcript"], QUESTION);

    // The times: the padded onset before the speech (300 ms in), the end
    // after it plus the 500 ms window.
    let start = events[0]["audio_start_ms"].as_u64().unwrap();
    let end = events[1]["audio_end_ms"].as_u64().unwrap();
    assert!(start <= 300, "audio_start_ms {start}");
    assert!(end >= 1738 + 500, "audio_end_ms {end}");
    // The assistant's item follows the user's.
    assert_eq!(events[8]["previous_item_id"], item);

    // The model was asked about what was said.
    let body = chat.seen.chat(0);
    let last = body["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"], QUESTION, "{body}");

    // The upload: the segment, pre-roll and window included, as 16 kHz mono.
    let (rate, channels, samples) = asr.seen.wav(0);
    assert_eq!((rate, channels), (16_000, 1));
    let want = (end - start) as usize * 16;
    assert!(
        samples.abs_diff(want) <= 16,
        "{samples} samples, want {want}"
    );
}

#[tokio::test]
async fn without_transcription_asked_for_the_events_stay_out_but_the_turn_is_answered() {
    let (_s, addr, chat, asr) = voice_gateway(false, None).await;
    asr.push(Asr::Text(QUESTION));
    chat.push(Turn::text(&["Near."]));
    let (mut ws, updated) = voice_session(&addr, &[], json!({})).await;
    // The default session: server_vad, no transcription — and the cascade
    // transcribes anyway (§5.2).
    assert_eq!(
        updated["session"]["audio"]["input"]["transcription"],
        Value::Null
    );
    assert_eq!(updated["session"]["lmgw"]["resolved"]["asr"], ASR_ALIAS);

    stream(&mut ws, &fixture("en_complete_short.wav")).await;
    let events = events_until(&mut ws, "response.done").await;
    let t = types(&events);
    assert!(
        !t.iter().any(|t| t.contains("input_audio_transcription")),
        "{t:?}"
    );
    assert_eq!(t[4], "conversation.item.done");
    assert_eq!(events[4]["item"]["content"][0]["transcript"], QUESTION);
    assert_eq!(t[5], "response.created");
}

#[tokio::test]
async fn the_asr_call_is_told_only_the_two_letter_language_code() {
    // B5 review: the session's language went up raw, and a model that
    // refuses "german" would fail every turn. Fix package B6: its ISO 639-1
    // code, or nothing — never an error.
    for (language, sent) in [("de-DE", Some("de")), ("german", None)] {
        let (_s, addr, chat, asr) = voice_gateway(false, None).await;
        asr.push(Asr::Text(QUESTION));
        chat.push(Turn::text(&["Near."]));
        let (mut ws, updated) = voice_session(
            &addr,
            &[],
            json!({"audio": {"input": {"transcription": {"model": "gpt-4o-mini-transcribe",
                                                         "language": language}}}}),
        )
        .await;
        // The client's value is echoed as it came.
        assert_eq!(
            updated["session"]["audio"]["input"]["transcription"]["language"],
            language
        );
        stream(&mut ws, &fixture("en_complete_short.wav")).await;
        let events = events_until(&mut ws, "response.done").await;
        assert!(
            types(&events).contains(&"conversation.item.input_audio_transcription.completed"),
            "{language}: the turn is transcribed"
        );
        let body = asr.seen.bodies.lock().unwrap()[0].clone();
        let field = String::from_utf8_lossy(&body)
            .split("--")
            .find(|part| part.contains("name=\"language\""))
            .map(|part| {
                part.trim_end()
                    .rsplit('\n')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string()
            });
        assert_eq!(field.as_deref(), sent, "{language}");
    }
}

#[tokio::test]
async fn a_failed_transcription_is_reported_without_nulls_and_answers_nothing() {
    let (_s, addr, chat, asr) = voice_gateway(false, None).await;
    asr.push(Asr::Status(
        500,
        json!({"error": {"message": "engine fell over"}}),
    ));
    let (mut ws, _) = voice_session(
        &addr,
        &[],
        json!({"audio": {"input": {"transcription": {"model": ASR_ALIAS}}}}),
    )
    .await;
    stream(&mut ws, &fixture("en_complete_short.wav")).await;
    let mut events = events_until(&mut ws, "conversation.item.done").await;
    events.extend(until_sentinel(&mut ws).await);
    assert_eq!(
        types(&events),
        [
            "input_audio_buffer.speech_started",
            "input_audio_buffer.speech_stopped",
            "input_audio_buffer.committed",
            "conversation.item.added",
            "conversation.item.input_audio_transcription.failed",
            "conversation.item.done",
        ]
    );
    let failed = &events[4];
    assert_eq!(failed["item_id"], events[0]["item_id"]);
    assert_eq!(failed["content_index"], 0);
    // `@openai/agents` rejects a null code or param here: absent, not null.
    let error = failed["error"].as_object().unwrap();
    assert!(error["code"].is_string(), "{failed}");
    assert!(error["type"].is_string() && error["message"].is_string());
    assert!(!error.values().any(Value::is_null), "{failed}");
    assert!(!error.contains_key("param") && !error.contains_key("event_id"));
    // The item stays, untranscribed; nothing was asked of the model.
    assert_eq!(
        events[5]["item"]["content"][0].get("transcript"),
        Some(&Value::Null)
    );
    assert_eq!(chat.seen.chat_count(), 0);
}

#[tokio::test]
async fn an_empty_transcript_starts_no_response() {
    let (_s, addr, chat, asr) = voice_gateway(false, None).await;
    asr.push(Asr::Text(""));
    let (mut ws, _) = voice_session(
        &addr,
        &[],
        json!({"audio": {"input": {"transcription": {}}}}),
    )
    .await;
    stream(&mut ws, &fixture("en_complete_short.wav")).await;
    let mut events = events_until(&mut ws, "conversation.item.done").await;
    events.extend(until_sentinel(&mut ws).await);
    let t = types(&events);
    assert_eq!(
        t[4..],
        [
            "conversation.item.input_audio_transcription.completed",
            "conversation.item.done"
        ]
    );
    assert_eq!(events[4]["transcript"], "");
    assert_eq!(events[5]["item"]["content"][0]["transcript"], "");
    assert_eq!(chat.seen.chat_count(), 0);
}

#[tokio::test]
async fn with_no_asr_configured_audio_fails_at_the_commit_and_text_still_works() {
    let chat = chat_fake().await;
    let (_s, addr) = gateway(&chat, false, None, |_| {}).await;
    let (mut ws, updated) = voice_session(&addr, &[], json!({})).await;
    assert_eq!(updated["session"]["lmgw"]["resolved"]["asr"], Value::Null);

    stream(&mut ws, &fixture("en_complete_short.wav")).await;
    let events = events_until(&mut ws, "error").await;
    assert_eq!(
        types(&events),
        [
            "input_audio_buffer.speech_started",
            "input_audio_buffer.speech_stopped",
            "error"
        ]
    );
    assert_eq!(events[2]["error"]["code"], "asr_not_configured");
    assert!(events[2]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("realtime.asr_alias"));

    // A text turn is unaffected.
    send(&mut ws, crate::support::realtime_fakes::user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let done = events_until(&mut ws, "response.done").await;
    assert_eq!(done.last().unwrap()["response"]["status"], "completed");
}

#[tokio::test]
async fn the_asr_call_is_the_session_key_s_and_its_policy_is_checked_per_call() {
    let (state, addr, chat, asr) = voice_gateway(
        true,
        Some(KeyPolicy {
            scope_mode: ScopeMode::Allow,
            scope_patterns: format!("chatty\n{ASR_ALIAS}"),
            ..Default::default()
        }),
    )
    .await;
    chat.push(Turn::text(&["Near."]));
    asr.push(Asr::Text(QUESTION));
    let bearer = format!("Bearer {KEY}");
    let auth = [("authorization", bearer.as_str())];
    let session = json!({"audio": {"input": {"transcription": {"model": ASR_ALIAS}}}});
    let (mut ws, _) = voice_session(&addr, &auth, session.clone()).await;
    stream(&mut ws, &fixture("en_complete_short.wav")).await;
    let done = events_until(&mut ws, "response.done").await;
    assert_eq!(done.last().unwrap()["response"]["status"], "completed");

    // The ASR row carries the session's key, under the audio class and the
    // realtime label (§11).
    let rows: Vec<(Option<String>, Option<String>, i64, String)> = sqlx::query_as(
        "SELECT client_key, class, status, ingress_proto FROM request_logs WHERE \
         requested_alias = ?1",
    )
    .bind(ASR_ALIAS)
    .fetch_all(&state.db)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [(
            Some("voice".into()),
            Some("audio".into()),
            200,
            "realtime".into()
        )]
    );
    assert_eq!(asr.seen.wav(0).0, 16_000);

    // The owner fences the key off the ASR alias mid-session: the next turn
    // is refused before any upload, and nothing is answered.
    sqlx::query("UPDATE api_keys SET scope_patterns = 'chatty' WHERE name = 'voice'")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    stream(&mut ws, &fixture("en_complete_short.wav")).await;
    let mut events = events_until(&mut ws, "conversation.item.done").await;
    events.extend(until_sentinel(&mut ws).await);
    let failed = events
        .iter()
        .find(|e| e["type"] == "conversation.item.input_audio_transcription.failed")
        .unwrap_or_else(|| panic!("{events:?}"));
    assert_eq!(failed["error"]["code"], "key_scope");
    assert!(!types(&events).contains(&"response.created"));
    assert_eq!(asr.seen.count(), 1, "the refused call was never made");

    // And a key that may not use the ASR alias cannot open a session at all
    // (§10.2): the alias is checked before the 101.
    let refused = tokio_tungstenite::connect_async({
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut req = format!("ws://{addr}/v1/realtime?model=chatty")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", bearer.parse().unwrap());
        req
    })
    .await;
    match refused {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), 403)
        }
        other => panic!("expected a 403 before the upgrade, got {other:?}"),
    }
}

#[tokio::test]
async fn speech_before_the_response_cancels_it_and_the_turns_are_answered_together() {
    let (_s, addr, chat, asr) = voice_gateway(false, None).await;
    let release = Arc::new(Notify::new());
    asr.push(Asr::HeldText(release.clone(), "I am tired."));
    asr.push(Asr::Text("I will rest now."));
    chat.push(Turn::text(&["Sleep ", "well."]));
    let (mut ws, _) = voice_session(
        &addr,
        &[],
        json!({"audio": {"input": {"transcription": {}}}}),
    )
    .await;
    // Two sentences 700 ms apart: two turns under the 500 ms window, the
    // second starting while the first's transcript is still being made.
    stream(&mut ws, &fixture("en_two_sentences_pause.wav")).await;
    let mut events = events_until(&mut ws, "input_audio_buffer.committed").await;
    events.extend(events_until(&mut ws, "input_audio_buffer.committed").await);
    events.push(next_event(&mut ws).await);
    release.notify_one();
    events.extend(events_until(&mut ws, "response.done").await);

    let t = types(&events);
    let (first, second) = (
        events[0]["item_id"].as_str().unwrap(),
        events[4]["item_id"].as_str().unwrap(),
    );
    assert_ne!(first, second);
    assert_eq!(
        t[..12],
        [
            "input_audio_buffer.speech_started",
            "input_audio_buffer.speech_stopped",
            "input_audio_buffer.committed",
            "conversation.item.added",
            "input_audio_buffer.speech_started",
            "input_audio_buffer.speech_stopped",
            "input_audio_buffer.committed",
            "conversation.item.added",
            "conversation.item.input_audio_transcription.completed",
            "conversation.item.done",
            "conversation.item.input_audio_transcription.completed",
            "conversation.item.done",
        ],
        "{t:?}"
    );
    assert_eq!(events[9]["item"]["id"], first);
    assert_eq!(events[11]["item"]["id"], second);
    // One response, for both turns, rendered as one user message.
    assert_eq!(t.iter().filter(|t| **t == "response.created").count(), 1);
    assert_eq!(t[12], "response.created");
    assert_eq!(chat.seen.chat_count(), 1);
    let last = chat.seen.chat(0)["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["content"], "I am tired.\nI will rest now.");
}

#[tokio::test]
async fn an_asr_call_still_running_when_the_session_ends_is_stopped_and_writes_its_row() {
    // WP1c review #4: the upstream never answers, and nothing bounds the
    // call but the session — which ends.
    let (state, addr, _chat, asr) = voice_gateway(false, None).await;
    let never = Arc::new(Notify::new());
    asr.push(Asr::HeldText(never.clone(), QUESTION));
    let (mut ws, _) = voice_session(&addr, &[], json!({})).await;
    stream(&mut ws, &fixture("en_complete_short.wav")).await;
    events_until(&mut ws, "conversation.item.added").await;
    for _ in 0..500 {
        if asr.seen.count() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(asr.seen.count(), 1, "the call is with the engine");
    drop(ws);

    // The call ended without its answer, and still wrote its one row — on
    // the route the gate had opened (package A review #4).
    // Status, error kind, upstream, model.
    type Row = (i64, Option<String>, Option<String>, Option<String>);
    let mut rows: Vec<Row> = Vec::new();
    for _ in 0..500 {
        rows = sqlx::query_as(
            "SELECT status, error_kind, upstream_name, upstream_model FROM request_logs WHERE \
             requested_alias = ?1",
        )
        .bind(ASR_ALIAS)
        .fetch_all(&state.db)
        .await
        .unwrap();
        if !rows.is_empty() && state.telemetry.stats().active_requests == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        rows,
        [(
            200,
            Some("canceled".into()),
            Some("audiocpp".into()),
            Some("nemotron-asr".into())
        )]
    );
    assert_eq!(state.telemetry.stats().active_requests, 0);
}

/// Billable units (billable-units design §4.2, §10): a turn's ASR row
/// records the length of the WAV lmgw built for the turn and sent up,
/// measured to the millisecond (whole frames × 1000 / rate) — the engine
/// reports nothing — and the chat request that answers it records no
/// audio.
#[tokio::test]
async fn a_voice_turns_asr_row_records_the_length_of_the_audio_it_sent() {
    let (state, addr, chat, asr) = voice_gateway(false, None).await;
    asr.push(Asr::Text(QUESTION));
    chat.push(Turn::text(&["Near."]));
    let (mut ws, _) = voice_session(&addr, &[], json!({})).await;
    stream(&mut ws, &fixture("en_complete_short.wav")).await;
    events_until(&mut ws, "response.done").await;

    let (rate, channels, samples) = asr.seen.wav(0);
    assert_eq!((rate, channels), (16_000, 1));
    let sent_ms = i64::try_from(samples as u64 * 1000 / u64::from(rate)).unwrap();
    // The question's 1.4 s of speech, its pre-roll and the closing window.
    assert!(sent_ms > 1_500, "{sent_ms} ms went up");
    let rows = |alias: &'static str| {
        let db = state.db.clone();
        async move {
            lmgw_core::store::query_logs(
                &db,
                &lmgw_core::store::LogFilter {
                    alias: Some(alias.into()),
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        }
    };
    crate::common::patience::until_async("the turn's ASR row and the answer's row", || async {
        !rows(ASR_ALIAS).await.is_empty() && !rows("chatty").await.is_empty()
    })
    .await;
    let heard = rows(ASR_ALIAS).await;
    assert_eq!(heard.len(), 1, "{heard:?}");
    assert_eq!(heard[0].ingress_proto, "realtime");
    assert_eq!(heard[0].audio_in_ms, Some(sent_ms), "{:?}", heard[0]);
    let answered = rows("chatty").await;
    assert_eq!(answered[0].audio_in_ms, None, "{:?}", answered[0]);
}
