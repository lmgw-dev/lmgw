use serde_json::{json, Value};

use super::*;

#[test]
fn the_session_says_its_turn_detection_and_thread() {
    let ev = json!({"type": "session.created", "event_id": "e1", "session": {
        "type": "realtime",
        "audio": {"input": {"turn_detection": {"type": "server_vad", "prefix_padding_ms": 300}}},
        "lmgw": {"half_duplex": false, "resolved": {"chat": "gemma", "asr": "parakeet",
            "tts": "supertonic", "voice": "F2",
            "chat_thread": {"id": -3, "title": "Plan", "temporary": true, "admin_tools": true}}}
    }});
    let Some(ServerEvent::SessionCreated(f)) = parse(&ev.to_string()) else {
        panic!("a session")
    };
    assert_eq!(f.turn_detection.as_deref(), Some("server_vad"));
    assert_eq!(f.prefix_padding_ms, Some(300));
    assert_eq!(f.half_duplex, Some(false));
    assert_eq!(f.asr.as_deref(), Some("parakeet"));
    assert_eq!(f.voice.as_deref(), Some("F2"));
    assert_eq!(
        f.thread,
        Some(ThreadFacts {
            id: -3,
            title: "Plan".into(),
            admin_tools: true
        })
    );
    // Manual turns: a null turn detection; `session.updated` is an echo.
    let ev = json!({"type": "session.updated", "session": {"audio": {"input": {"turn_detection": null}}}});
    let Some(ServerEvent::SessionUpdated(f)) = parse(&ev.to_string()) else {
        panic!("a session")
    };
    assert_eq!(f.turn_detection, None);
    assert_eq!(f.thread, None);
    assert_eq!(
        parse(r#"{"type":"input_audio_buffer.cleared","event_id":"e"}"#),
        Some(ServerEvent::Cleared)
    );
}

#[test]
fn only_the_echo_of_the_last_update_is_the_session_s_word() {
    let mut u = Updates::default();
    u.sent();
    u.sent();
    assert!(!u.echo(), "an older update's answer");
    assert!(u.echo(), "the last one's");
    assert!(u.echo(), "one nobody here sent");
}

#[test]
fn audio_is_decoded_and_tagged_with_its_response() {
    let ev = json!({"type": "response.output_audio.delta", "event_id": "e", "response_id": "resp_1",
        "item_id": "item_9", "output_index": 0, "content_index": 0, "delta": "AAEC"});
    assert_eq!(
        parse(&ev.to_string()),
        Some(ServerEvent::AudioDelta {
            response_id: "resp_1".into(),
            item_id: "item_9".into(),
            pcm: vec![0, 1, 2],
        })
    );
}

#[test]
fn a_done_response_says_why_it_ended() {
    let ev = json!({"type": "response.done", "response": {"id": "r", "status": "cancelled",
        "status_details": {"type": "cancelled", "reason": "turn_detected"}}});
    assert_eq!(
        parse(&ev.to_string()),
        Some(ServerEvent::ResponseDone {
            response_id: "r".into(),
            status: "cancelled".into(),
            reason: Some("turn_detected".into())
        })
    );
    let ev = json!({"type": "response.done", "response": {"id": "r", "status": "failed",
        "status_details": {"type": "failed", "error": {"code": "empty_turn"}}}});
    let Some(ServerEvent::ResponseDone { reason, .. }) = parse(&ev.to_string()) else {
        panic!()
    };
    assert_eq!(reason.as_deref(), Some("empty_turn"));
    // Words before a code.
    let ev = json!({"type": "response.done", "response": {"id": "r", "status": "failed",
        "status_details": {"type": "failed",
                           "error": {"code": "gpu_hold", "message": "held"}}}});
    let Some(ServerEvent::ResponseDone { reason, .. }) = parse(&ev.to_string()) else {
        panic!()
    };
    assert_eq!(reason.as_deref(), Some("held"));
}

#[test]
fn a_failed_transcription_keeps_its_code() {
    let ev = json!({"type": "conversation.item.input_audio_transcription.failed",
        "item_id": "i", "content_index": 0,
        "error": {"type": "invalid_request_error", "code": "asr_not_configured",
                  "message": "this turn cannot be transcribed"}});
    assert_eq!(
        parse(&ev.to_string()),
        Some(ServerEvent::TranscriptFailed(ErrorFacts {
            kind: "invalid_request_error".into(),
            code: Some("asr_not_configured".into()),
            message: "this turn cannot be transcribed".into(),
        }))
    );
    // The code is left out when there is none.
    let ev = json!({"type": "conversation.item.input_audio_transcription.failed",
        "item_id": "i", "content_index": 0, "error": {"type": "server_error", "message": "m"}});
    assert!(matches!(
        parse(&ev.to_string()),
        Some(ServerEvent::TranscriptFailed(ErrorFacts { code: None, .. }))
    ));
}

#[test]
fn the_extension_events_are_read() {
    let ev = json!({"type": "lmgw.chat.reply", "event_id": "e", "message_id": 812,
        "content": "Heard", "unheard": "rest", "voice": {"via": "realtime"}});
    assert_eq!(
        parse(&ev.to_string()),
        Some(ServerEvent::ChatReply(ChatReply {
            message_id: 812,
            content: Some("Heard".into()),
            unheard: Some("rest".into()),
            voice: json!({"via": "realtime"}),
            removed: false,
            skipped: None,
        }))
    );
    let ev = json!({"type": "lmgw.chat.reply", "message_id": 9, "removed": true});
    let Some(ServerEvent::ChatReply(r)) = parse(&ev.to_string()) else {
        panic!()
    };
    assert!(r.removed);
    assert_eq!((r.content, r.voice), (None, Value::Null));
    let ev = json!({"type": "lmgw.chat.reply", "message_id": 9, "skipped": "no audio heard"});
    let Some(ServerEvent::ChatReply(r)) = parse(&ev.to_string()) else {
        panic!()
    };
    assert_eq!(r.skipped.as_deref(), Some("no audio heard"));
    let ev = json!({"type": "lmgw.model.state", "event_id": "e", "stage": "asr", "alias": "a",
        "state": "fallback", "ms": null, "answered_by": "cloud/asr"});
    assert_eq!(
        parse(&ev.to_string()),
        Some(ServerEvent::ModelState(ModelState {
            stage: "asr".into(),
            alias: "a".into(),
            state: "fallback".into(),
            answered_by: Some("cloud/asr".into()),
            ..Default::default()
        }))
    );
    let ev = json!({"type": "lmgw.response.timing", "event_id": "e", "response_id": "r",
        "models": {}});
    assert_eq!(
        parse(&ev.to_string()),
        Some(ServerEvent::Timing(
            json!({"response_id": "r", "models": {}})
        ))
    );
    let ev = json!({"type": "lmgw.chat.thread", "chat_thread": {"id": 4, "title": "T",
        "temporary": false, "admin_tools": false}});
    assert_eq!(
        parse(&ev.to_string()),
        Some(ServerEvent::Thread(ThreadFacts {
            id: 4,
            title: "T".into(),
            admin_tools: false
        }))
    );
    let ev = json!({"type": "lmgw.chat.user", "message_id": 7, "content": "Hallo",
        "voice": {"via": "realtime"}, "response_id": "r1"});
    assert_eq!(
        parse(&ev.to_string()),
        Some(ServerEvent::ChatUser {
            message_id: 7,
            content: "Hallo".into(),
            voice: json!({"via": "realtime"}),
            response_id: Some("r1".into()),
        })
    );
    let ev = json!({"type": "lmgw.chat.input", "response_id": "r1", "input": "transcript",
        "why": "the model takes no audio"});
    assert_eq!(
        parse(&ev.to_string()),
        Some(ServerEvent::ChatInput {
            response_id: "r1".into(),
            input: "transcript".into(),
            why: Some("the model takes no audio".into()),
        })
    );
    let ev = json!({"type": "lmgw.chat.frame", "response_id": "r1", "event": "delta",
        "data": {"text": "Hi"}});
    assert_eq!(
        parse(&ev.to_string()),
        Some(ServerEvent::ChatFrame {
            response_id: "r1".into(),
            event: "delta".into(),
            data: json!({"text": "Hi"}),
        })
    );
    let ev = json!({"type": "error", "error": {"type": "invalid_request_error",
        "code": "chat_thread_taken_over", "message": "voice mode moved", "param": null}});
    assert_eq!(
        parse(&ev.to_string()),
        Some(ServerEvent::Error(ErrorFacts {
            kind: "invalid_request_error".into(),
            code: Some("chat_thread_taken_over".into()),
            message: "voice mode moved".into()
        }))
    );
    let ev = json!({"type": "conversation.item.truncated", "item_id": "item_1",
        "content_index": 0, "audio_end_ms": 1500});
    assert_eq!(
        parse(&ev.to_string()),
        Some(ServerEvent::Truncated {
            item_id: "item_1".into(),
            audio_end_ms: 1500
        })
    );
    assert_eq!(
        parse(r#"{"type":"rate_limits.updated","rate_limits":[]}"#),
        Some(ServerEvent::Unknown {
            kind: "rate_limits.updated".into(),
            data: json!({"type": "rate_limits.updated", "rate_limits": []})
        })
    );
    // A typed event whose fields do not read is Unknown too, kept whole.
    let bad = json!({"type": "lmgw.model.state", "stage": 3});
    assert_eq!(
        parse(&bad.to_string()),
        Some(ServerEvent::Unknown {
            kind: "lmgw.model.state".into(),
            data: bad
        })
    );
    assert_eq!(parse("not json"), None);
}

#[test]
fn the_client_events_are_what_the_server_takes() {
    let up = ClientEvent::SessionUpdate {
        turn_detection: TurnDetection::requested(true, Some("x")),
        half_duplex: true,
    };
    let v: Value = serde_json::from_str(&up.to_json()).unwrap();
    assert_eq!(v["session"]["type"], "realtime");
    assert!(v["session"]["audio"]["input"]["turn_detection"].is_null());
    assert_eq!(v["session"]["lmgw"]["half_duplex"], true);
    assert_eq!(
        TurnDetection::requested(false, Some("server_vad")),
        Some(TurnDetection::ServerVad)
    );
    // A thread resolved to push-to-talk, switched to automatic.
    assert_eq!(
        TurnDetection::requested(false, Some("push_to_talk")),
        Some(TurnDetection::SemanticVad)
    );
    assert_eq!(
        TurnDetection::for_automatic(None),
        TurnDetection::SemanticVad
    );
}

/// The frames as the dashboard's voice panel sent them before this crate:
/// byte for byte, so the panel on the crate sends exactly what it did.
#[test]
fn the_client_events_are_byte_for_byte_the_panel_s_frames() {
    let cases: Vec<(ClientEvent, &str)> = vec![
        (
            ClientEvent::SessionUpdate {
                turn_detection: None,
                half_duplex: true,
            },
            r#"{"session":{"audio":{"input":{"turn_detection":null}},"lmgw":{"half_duplex":true},"type":"realtime"},"type":"session.update"}"#,
        ),
        (
            ClientEvent::SessionUpdate {
                turn_detection: Some(TurnDetection::ServerVad),
                half_duplex: false,
            },
            r#"{"session":{"audio":{"input":{"turn_detection":{"type":"server_vad"}}},"lmgw":{"half_duplex":false},"type":"realtime"},"type":"session.update"}"#,
        ),
        (
            ClientEvent::SessionUpdate {
                turn_detection: Some(TurnDetection::SemanticVad),
                half_duplex: false,
            },
            r#"{"session":{"audio":{"input":{"turn_detection":{"type":"semantic_vad"}}},"lmgw":{"half_duplex":false},"type":"realtime"},"type":"session.update"}"#,
        ),
        (
            ClientEvent::append_pcm16(&[1, -1]),
            r#"{"audio":"AQD//w==","type":"input_audio_buffer.append"}"#,
        ),
        (
            ClientEvent::Commit,
            r#"{"type":"input_audio_buffer.commit"}"#,
        ),
        (ClientEvent::Clear, r#"{"type":"input_audio_buffer.clear"}"#),
        (ClientEvent::ResponseCreate, r#"{"type":"response.create"}"#),
        (
            ClientEvent::ResponseCancel {
                response_id: Some("r1".into()),
            },
            r#"{"response_id":"r1","type":"response.cancel"}"#,
        ),
        (
            ClientEvent::ResponseCancel { response_id: None },
            r#"{"type":"response.cancel"}"#,
        ),
        (
            ClientEvent::Truncate {
                item_id: "item_1".into(),
                content_index: 0,
                audio_end_ms: 1234,
            },
            r#"{"audio_end_ms":1234,"content_index":0,"item_id":"item_1","type":"conversation.item.truncate"}"#,
        ),
    ];
    for (ev, want) in cases {
        assert_eq!(ev.to_json(), want, "{ev:?}");
    }
}

#[test]
fn an_approval_request_and_a_decision_elsewhere_are_typed() {
    let ev = parse(
        r#"{"type": "conversation.item.done", "event_id": "e1", "previous_item_id": null,
            "item": {"id": "mcpr_1", "type": "mcp_approval_request",
                     "server_label": "desktop", "name": "notify",
                     "arguments": "{\"text\":\"hi\"}"}}"#,
    )
    .unwrap();
    assert_eq!(
        ev,
        ServerEvent::ApprovalRequest(ApprovalRequest {
            approval_request_id: "mcpr_1".into(),
            server_label: "desktop".into(),
            name: "notify".into(),
            arguments: r#"{"text":"hi"}"#.into(),
            call_id: None,
        })
    );
    // Any other item's done stays untyped.
    let other = parse(r#"{"type": "conversation.item.done", "item": {"type": "message"}}"#);
    assert!(matches!(other, Some(ServerEvent::Unknown { .. })));
    let ev = parse(
        r#"{"type": "lmgw.approval.decided", "event_id": "e2",
            "approval_request_id": "mcpr_1", "approve": false, "by": "the dashboard"}"#,
    )
    .unwrap();
    assert_eq!(
        ev,
        ServerEvent::ApprovalDecided(ApprovalDecidedEvent {
            approval_request_id: "mcpr_1".into(),
            approve: false,
            by: Some("the dashboard".into()),
        })
    );
    let answer = ClientEvent::ApprovalResponse {
        approval_request_id: "mcpr_1".into(),
        approve: true,
        reason: None,
    };
    let v: Value = serde_json::from_str(&answer.to_json()).unwrap();
    assert_eq!(v["type"], "conversation.item.create");
    assert_eq!(v["item"]["type"], "mcp_approval_response");
    assert_eq!(v["item"]["approve"], true);
    assert!(v["item"].get("reason").is_none());
}

#[test]
fn a_late_task_result_is_typed() {
    let ev = parse(
        r#"{"type": "lmgw.task.done", "event_id": "e3", "thread_id": 7, "message_id": 41,
            "id": 3, "task_id": "t1", "server_label": "desktop", "tool": "desktop__build",
            "status": "completed", "by": null}"#,
    )
    .unwrap();
    assert_eq!(
        ev,
        ServerEvent::TaskDone(TaskDone {
            thread_id: 7,
            message_id: 41,
            id: 3,
            task_id: "t1".into(),
            server_label: "desktop".into(),
            tool: "desktop__build".into(),
            status: "completed".into(),
            by: None,
        })
    );
    // One whose fields do not read stays untyped.
    let odd = parse(r#"{"type": "lmgw.task.done", "message_id": "x"}"#);
    assert!(matches!(odd, Some(ServerEvent::Unknown { .. })), "{odd:?}");
    // The continuation is a plain `response.create`.
    assert_eq!(
        ClientEvent::ResponseCreate.to_json(),
        r#"{"type":"response.create"}"#
    );
}
