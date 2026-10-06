//! The realtime protocol types and `session.update` merging, without a
//! socket (realtime design §2, §16 "Protocol").
//!
//! The client-event fixtures follow the GA reference shapes; the session
//! updates mirror what the stock clients were captured sending (2026-10-01):
//! `@openai/agents`' first full update (an OpenAI model name, `semantic_vad`
//! with no eagerness, no voice, a tool whose parameters carry `$schema` and a
//! nullable `anyOf`), its follow-up `{type, tracing}`, and Hugging Face's
//! session-level `extensions` array.

use lmgw_core::config::RealtimeSettings;
use lmgw_core::realtime::merge::{apply_update, deep_merge, initial_session, DEFAULT_VOICE};
use lmgw_core::realtime::protocol::{
    ClientEvent, ErrorObject, Item, ServerEvent, ServerFrame, Session, TurnDetection,
};
use lmgw_core::realtime::resolve::is_openai_realtime_name;
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// Client events
// ---------------------------------------------------------------------------

/// One fixture per GA client event (§2.3), each with and without `event_id`.
fn client_fixtures() -> Vec<Value> {
    vec![
        json!({"type": "session.update", "session": {"type": "realtime", "instructions": "be brief"}}),
        json!({"type": "input_audio_buffer.append", "audio": "AAAA"}),
        json!({"type": "input_audio_buffer.commit"}),
        json!({"type": "input_audio_buffer.clear"}),
        json!({"type": "conversation.item.create", "previous_item_id": "item_1",
               "item": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "hi"}]}}),
        json!({"type": "conversation.item.retrieve", "item_id": "item_1"}),
        json!({"type": "conversation.item.truncate", "item_id": "item_1",
               "content_index": 0, "audio_end_ms": 1500}),
        json!({"type": "conversation.item.delete", "item_id": "item_1"}),
        json!({"type": "response.create", "response": {
               "instructions": "shorter", "output_modalities": ["text"],
               "max_output_tokens": "inf", "metadata": {"turn": 3},
               "tools": [{"type": "function", "name": "f", "parameters": {"type": "object"}}],
               "tool_choice": {"type": "function", "name": "f"}}}),
        json!({"type": "response.cancel", "response_id": "resp_1"}),
        json!({"type": "output_audio_buffer.clear"}),
    ]
}

#[test]
fn every_client_event_round_trips() {
    let fixtures = client_fixtures();
    assert_eq!(fixtures.len(), 11, "the GA client union is eleven events");
    for mut fixture in fixtures {
        for with_id in [false, true] {
            if with_id {
                fixture["event_id"] = json!("agents_js_response_create_7");
            }
            let ev: ClientEvent = serde_json::from_value(fixture.clone())
                .unwrap_or_else(|e| panic!("{fixture}: {e}"));
            assert_eq!(ev.type_name(), fixture["type"].as_str().unwrap());
            assert_eq!(ev.event_id(), fixture["event_id"].as_str());
            let back = serde_json::to_value(&ev).unwrap();
            assert_eq!(back, fixture, "round trip of {}", ev.type_name());
        }
    }
}

#[test]
fn an_item_without_an_id_parses_and_takes_one() {
    let mut item: Item = serde_json::from_value(json!({
        "type": "function_call_output", "call_id": "call_1", "output": "{\"ok\":true}"
    }))
    .unwrap();
    assert_eq!(item.id(), None);
    item.set_id_if_missing(|| "item_x".into());
    assert_eq!(item.id(), Some("item_x"));
    item.set_id_if_missing(|| "item_y".into());
    assert_eq!(item.id(), Some("item_x"), "an id the item has is kept");
}

#[test]
fn server_side_tools_parse_and_unknown_events_do_not() {
    // `mcp` tools are realtime-server-tools §1: a tool like a function's.
    let mcp = json!({"type": "response.create", "response": {
        "tools": [{"type": "mcp", "server_label": "x"}]}});
    let ev = serde_json::from_value::<ClientEvent>(mcp.clone()).unwrap();
    assert_eq!(serde_json::to_value(&ev).unwrap(), mcp);
    // A hosted tool OpenAI runs is not one lmgw has.
    let hosted = json!({"type": "response.create", "response": {
        "tools": [{"type": "web_search"}]}});
    assert!(serde_json::from_value::<ClientEvent>(hosted).is_err());
    // A beta name is not a GA event.
    let beta = json!({"type": "conversation.item.created"});
    assert!(serde_json::from_value::<ClientEvent>(beta).is_err());
}

// ---------------------------------------------------------------------------
// Server events
// ---------------------------------------------------------------------------

fn part_ref() -> Value {
    json!({"response_id": "resp_1", "item_id": "item_2", "output_index": 0, "content_index": 0})
}

fn with(mut base: Value, extra: Value) -> Value {
    for (k, v) in extra.as_object().unwrap() {
        base[k] = v.clone();
    }
    base
}

/// A representative set: one of every family, every delta kind.
fn server_fixtures() -> Vec<Value> {
    let item = json!({"type": "message", "id": "item_2", "object": "realtime.item",
                      "status": "in_progress", "role": "assistant", "content": []});
    let response = json!({"id": "resp_1", "object": "realtime.response", "status": "cancelled",
        "status_details": {"type": "cancelled", "reason": "turn_detected"},
        "output": [item.clone()], "output_modalities": ["audio"], "max_output_tokens": "inf",
        "usage": {"total_tokens": 7, "input_tokens": 5, "output_tokens": 2,
                  "input_token_details": {"text_tokens": 5, "audio_tokens": 0},
                  "output_token_details": {"text_tokens": 2, "audio_tokens": 0}},
        "metadata": {"turn": 3}});
    vec![
        json!({"type": "error", "error": {"type": "invalid_request_error",
               "code": "invalid_event", "message": "m", "param": null, "event_id": "e1"}}),
        json!({"type": "input_audio_buffer.speech_started", "audio_start_ms": 1200,
               "item_id": "item_3"}),
        json!({"type": "input_audio_buffer.speech_stopped", "audio_end_ms": 2500,
               "item_id": "item_3"}),
        json!({"type": "input_audio_buffer.committed", "previous_item_id": null,
               "item_id": "item_3"}),
        json!({"type": "input_audio_buffer.cleared"}),
        json!({"type": "conversation.item.added", "previous_item_id": "item_1", "item": item}),
        json!({"type": "conversation.item.done", "previous_item_id": "item_1", "item": item}),
        json!({"type": "conversation.item.truncated", "item_id": "item_2", "content_index": 0,
               "audio_end_ms": 900}),
        json!({"type": "conversation.item.deleted", "item_id": "item_2"}),
        // `usage` always: openai-python declares it required (§23, L3).
        json!({"type": "conversation.item.input_audio_transcription.completed",
               "item_id": "item_3", "content_index": 0, "transcript": "hello",
               "usage": {"type": "duration", "seconds": 1.5}}),
        // No `param`, and never a null one: `@openai/agents` rejects it here.
        json!({"type": "conversation.item.input_audio_transcription.failed",
               "item_id": "item_3", "content_index": 0,
               "error": {"type": "api_error", "code": "upstream", "message": "m"}}),
        // A committed turn's item before its ASR call returns.
        json!({"type": "conversation.item.added", "previous_item_id": null,
               "item": {"type": "message", "id": "item_3", "object": "realtime.item",
                        "status": "completed", "role": "user",
                        "content": [{"type": "input_audio", "transcript": null}]}}),
        json!({"type": "response.created", "response": response}),
        json!({"type": "response.output_item.added", "response_id": "resp_1",
               "output_index": 0, "item": item}),
        with(
            part_ref(),
            json!({"type": "response.content_part.added",
               "part": {"type": "audio", "transcript": ""}}),
        ),
        with(
            part_ref(),
            json!({"type": "response.output_audio.delta", "delta": "AAAA"}),
        ),
        with(part_ref(), json!({"type": "response.output_audio.done"})),
        with(
            part_ref(),
            json!({"type": "response.output_audio_transcript.delta", "delta": "Hel"}),
        ),
        with(
            part_ref(),
            json!({"type": "response.output_audio_transcript.done",
               "transcript": "Hello."}),
        ),
        with(
            part_ref(),
            json!({"type": "response.output_text.delta", "delta": "Hel"}),
        ),
        with(
            part_ref(),
            json!({"type": "response.output_text.done", "text": "Hello."}),
        ),
        json!({"type": "response.function_call_arguments.delta", "response_id": "resp_1",
               "item_id": "item_4", "output_index": 0, "call_id": "call_1", "delta": "{\"a\""}),
        json!({"type": "response.function_call_arguments.done", "response_id": "resp_1",
               "item_id": "item_4", "output_index": 0, "call_id": "call_1", "name": "f",
               "arguments": "{\"a\":1}"}),
        with(
            part_ref(),
            json!({"type": "response.content_part.done",
               "part": {"type": "text", "text": "Hello."}}),
        ),
        json!({"type": "response.output_item.done", "response_id": "resp_1",
               "output_index": 0, "item": item}),
        json!({"type": "response.done", "response": response}),
    ]
}

#[test]
fn server_events_round_trip_with_an_event_id() {
    for mut fixture in server_fixtures() {
        fixture["event_id"] = json!("event_abc1");
        let frame: ServerFrame =
            serde_json::from_value(fixture.clone()).unwrap_or_else(|e| panic!("{fixture}: {e}"));
        assert_eq!(serde_json::to_value(&frame).unwrap(), fixture);
    }
}

#[test]
fn every_delta_event_carries_its_four_ids() {
    for fixture in server_fixtures() {
        let t = fixture["type"].as_str().unwrap();
        if !t.ends_with(".delta") || t.contains("function_call") {
            continue;
        }
        let ev: ServerEvent = serde_json::from_value(fixture.clone()).unwrap();
        let out = serde_json::to_value(ServerFrame {
            event_id: "event_x".into(),
            event: ev,
        })
        .unwrap();
        for key in [
            "event_id",
            "response_id",
            "item_id",
            "output_index",
            "content_index",
        ] {
            assert!(out.get(key).is_some(), "{t} lacks {key}: {out}");
        }
    }
}

#[test]
fn the_error_object_always_writes_its_five_keys() {
    let e = ErrorObject::invalid("invalid_event", "nope");
    let v = serde_json::to_value(&e).unwrap();
    for key in ["type", "code", "message", "param", "event_id"] {
        assert!(v.get(key).is_some(), "error lacks {key}: {v}");
    }
    assert_eq!(v["type"], "invalid_request_error");
    assert_eq!(v["param"], Value::Null);
}

// ---------------------------------------------------------------------------
// The session and session.update
// ---------------------------------------------------------------------------

fn settings() -> RealtimeSettings {
    RealtimeSettings {
        default_instructions: Some("You are lmgw.".into()),
        ..Default::default()
    }
}

fn fresh() -> Session {
    initial_session(&settings(), "sess_test1".into(), Some("chatty".into()))
}

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

fn echo(s: &Session) -> Value {
    serde_json::to_value(s).unwrap()
}

#[test]
fn a_new_session_carries_every_default() {
    let s = echo(&fresh());
    assert_eq!(s["type"], "realtime");
    assert_eq!(s["object"], "realtime.session");
    assert_eq!(s["id"], "sess_test1");
    assert_eq!(s["instructions"], "You are lmgw.");
    assert_eq!(s["output_modalities"], json!(["audio"]));
    assert_eq!(s["tools"], json!([]));
    assert_eq!(s["tool_choice"], "auto");
    assert_eq!(s["max_output_tokens"], "inf");
    let input = &s["audio"]["input"];
    assert_eq!(input["format"], json!({"type": "audio/pcm", "rate": 24000}));
    assert_eq!(input["transcription"], Value::Null);
    assert_eq!(
        input["turn_detection"],
        json!({"type": "server_vad", "threshold": 0.5, "prefix_padding_ms": 300,
               "silence_duration_ms": 500, "create_response": true, "interrupt_response": true})
    );
    let output = &s["audio"]["output"];
    assert_eq!(
        output["format"],
        json!({"type": "audio/pcm", "rate": 24000})
    );
    assert_eq!(output["voice"], DEFAULT_VOICE);
    assert_eq!(s["lmgw"]["resolved"]["turn_detection"], "server_vad");
    assert_eq!(s["lmgw"]["resolved"]["semantic_vad"], Value::Null);
    assert_eq!(s["lmgw"]["resolved"]["truncation"], "disabled");
}

/// `@openai/agents`' first full `session.update`, as captured.
fn agents_first_update() -> Value {
    json!({
        "type": "realtime",
        "model": "gpt-realtime-2.1",
        "instructions": "You are a helpful assistant.",
        "output_modalities": ["audio"],
        "audio": {
            "input": {
                "format": {"type": "audio/pcm", "rate": 24000},
                "noise_reduction": null,
                "transcription": {"model": "gpt-4o-mini-transcribe"},
                "turn_detection": {"type": "semantic_vad"}
            },
            "output": {"format": {"type": "audio/pcm", "rate": 24000}, "speed": 1}
        },
        "tools": [{
            "type": "function",
            "name": "get_weather",
            "description": "Weather for a city",
            "parameters": {
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {
                    "city": {"type": "string"},
                    "unit": {"anyOf": [{"type": "string", "enum": ["c", "f"]}, {"type": "null"}]}
                },
                "required": ["city", "unit"],
                "additionalProperties": false
            }
        }]
    })
}

#[test]
fn the_agents_sdk_first_update_merges_and_echoes_what_it_needs() {
    let update = agents_first_update();
    let ev: ClientEvent = serde_json::from_value(json!({
        "type": "session.update", "session": update.clone()
    }))
    .unwrap();
    let ClientEvent::SessionUpdate { session, .. } = ev else {
        unreachable!()
    };
    let next = apply_update(&fresh(), &session, &settings()).unwrap();
    let s = echo(&next);

    assert_eq!(s["model"], "gpt-realtime-2.1");
    assert_eq!(s["instructions"], "You are a helpful assistant.");
    let td = &s["audio"]["input"]["turn_detection"];
    // The type changed, so server_vad's fields are gone — and the two flags
    // the SDK reads are there although the client did not send them.
    assert_eq!(
        td,
        &json!({"type": "semantic_vad", "eagerness": "auto",
                "create_response": true, "interrupt_response": true})
    );
    // Smart Turn serves it, and the echo carries the rule of its eagerness
    // (auto is medium, §6.3).
    assert_eq!(s["lmgw"]["resolved"]["turn_detection"], "semantic_vad");
    assert_eq!(
        s["lmgw"]["resolved"]["semantic_vad"],
        json!({"threshold": 0.5, "floor": 0.2, "floor_window_ms": 500, "max_wait_ms": 4000,
               "silence_duration_ms": 500})
    );
    assert_eq!(
        s["audio"]["input"]["transcription"],
        json!({"model": "gpt-4o-mini-transcribe"})
    );
    // No voice was sent: the placeholder stands, the format is kept.
    assert_eq!(s["audio"]["output"]["voice"], DEFAULT_VOICE);
    assert_eq!(s["audio"]["output"]["speed"], 1.0);
    assert_eq!(
        s["audio"]["output"]["format"],
        json!({"type": "audio/pcm", "rate": 24000})
    );
    // The tool's schema is kept verbatim, `$schema` and the nullable union
    // included.
    assert_eq!(
        s["tools"][0]["parameters"],
        update["tools"][0]["parameters"]
    );

    // Its follow-up carries `tracing` alone, and changes nothing else.
    let again = apply_update(
        &next,
        &obj(json!({"type": "realtime", "tracing": "auto"})),
        &settings(),
    )
    .unwrap();
    let a = echo(&again);
    assert_eq!(a["tracing"], "auto");
    assert_eq!(a["tools"], s["tools"]);
    assert_eq!(a["audio"], s["audio"]);
}

#[test]
fn unknown_session_fields_are_ignored() {
    // Hugging Face's speech-to-speech client.
    let next = apply_update(
        &fresh(),
        &obj(json!({"type": "realtime",
                    "extensions": ["speech_to_speech.input_audio_transcription.snapshot"],
                    "instructions": "hi"})),
        &settings(),
    )
    .unwrap();
    let s = echo(&next);
    assert_eq!(s["instructions"], "hi");
    assert!(s.get("extensions").is_none());
}

#[test]
fn an_update_must_carry_type_realtime() {
    let e = apply_update(&fresh(), &obj(json!({"instructions": "x"})), &settings()).unwrap_err();
    assert_eq!(e.code.as_deref(), Some("missing_required_parameter"));
    assert_eq!(e.param.as_deref(), Some("session.type"));
    let e = apply_update(
        &fresh(),
        &obj(json!({"type": "transcription"})),
        &settings(),
    )
    .unwrap_err();
    assert_eq!(e.param.as_deref(), Some("session.type"));
}

#[test]
fn a_partial_update_merges_deeply() {
    let next = apply_update(
        &fresh(),
        &obj(json!({"type": "realtime",
                    "audio": {"input": {"turn_detection": {"type": "server_vad",
                                                           "silence_duration_ms": 800}}}})),
        &settings(),
    )
    .unwrap();
    let td = &echo(&next)["audio"]["input"]["turn_detection"];
    assert_eq!(td["silence_duration_ms"], 800);
    assert_eq!(td["threshold"], 0.5, "untouched fields stay");
    assert_eq!(td["prefix_padding_ms"], 300);
    assert_eq!(
        echo(&next)["audio"]["output"]["voice"],
        DEFAULT_VOICE,
        "a sibling branch is untouched"
    );
}

#[test]
fn null_turn_detection_is_manual_and_empty_tools_clear() {
    let with_tool = apply_update(
        &fresh(),
        &obj(json!({"type": "realtime",
        "tools": [{"type": "function", "name": "f"}]})),
        &settings(),
    )
    .unwrap();
    assert_eq!(echo(&with_tool)["tools"].as_array().unwrap().len(), 1);

    // An absent `tools` leaves them; an explicit `[]` clears them (§2.2).
    let kept = apply_update(&with_tool, &obj(json!({"type": "realtime"})), &settings()).unwrap();
    assert_eq!(echo(&kept)["tools"].as_array().unwrap().len(), 1);
    let cleared = apply_update(
        &with_tool,
        &obj(json!({"type": "realtime", "tools": []})),
        &settings(),
    )
    .unwrap();
    assert_eq!(echo(&cleared)["tools"], json!([]));

    let manual = apply_update(
        &fresh(),
        &obj(json!({"type": "realtime", "audio": {"input": {"turn_detection": null}}})),
        &settings(),
    )
    .unwrap();
    let s = echo(&manual);
    assert_eq!(s["audio"]["input"]["turn_detection"], Value::Null);
    assert_eq!(s["lmgw"]["resolved"]["turn_detection"], Value::Null);
    let input = manual.audio.as_ref().unwrap().input.as_ref().unwrap();
    assert!(input.turn_detection.is_none());

    // And back: a fresh server_vad gets every default again.
    let auto = apply_update(
        &manual,
        &obj(json!({"type": "realtime",
                    "audio": {"input": {"turn_detection": {"type": "server_vad"}}}})),
        &settings(),
    )
    .unwrap();
    let input = auto.audio.as_ref().unwrap().input.as_ref().unwrap();
    assert!(matches!(
        input.turn_detection,
        Some(TurnDetection::ServerVad {
            threshold: Some(_),
            create_response: Some(true),
            interrupt_response: Some(true),
            ..
        })
    ));
}

#[test]
fn unions_parse_in_every_spelling() {
    for (choice, ok) in [
        (json!("auto"), true),
        (json!("none"), true),
        (json!("required"), true),
        (json!({"type": "function", "name": "f"}), true),
        (json!("sometimes"), false),
        (json!({"type": "mcp", "server_label": "x"}), true),
        (
            json!({"type": "mcp", "server_label": "x", "name": "t"}),
            true,
        ),
        (json!({"type": "mcp"}), false),
    ] {
        let r = apply_update(
            &fresh(),
            &obj(json!({"type": "realtime", "tool_choice": choice})),
            &settings(),
        );
        assert_eq!(r.is_ok(), ok, "tool_choice {choice}");
    }
    for (max, ok) in [
        (json!(1), true),
        (json!(100000), true),
        (json!("inf"), true),
        (json!("lots"), false),
        (json!(-1), false),
    ] {
        let r = apply_update(
            &fresh(),
            &obj(json!({"type": "realtime", "max_output_tokens": max})),
            &settings(),
        );
        assert_eq!(r.is_ok(), ok, "max_output_tokens {max}");
    }
    let named = apply_update(
        &fresh(),
        &obj(json!({"type": "realtime",
        "audio": {"output": {"voice": {"id": "clip_7"}}}})),
        &settings(),
    )
    .unwrap();
    assert_eq!(
        echo(&named)["audio"]["output"]["voice"],
        json!({"id": "clip_7"})
    );
}

#[test]
fn what_the_cascade_cannot_serve_is_an_error_and_changes_nothing() {
    let current = fresh();
    for (update, param) in [
        (
            json!({"type": "realtime", "audio": {"input": {"format": {"type": "audio/pcmu"}}}}),
            "session.audio.input.format",
        ),
        (
            json!({"type": "realtime", "audio": {"output": {"format": {"type": "audio/pcm",
                                                                    "rate": 16000}}}}),
            "session.audio.output.format",
        ),
        (
            json!({"type": "realtime", "output_modalities": ["audio", "text"]}),
            "session.output_modalities",
        ),
        (
            json!({"type": "realtime", "audio": {"input": {"turn_detection":
            {"type": "server_vad", "threshold": 1.5}}}}),
            "session.audio.input.turn_detection.threshold",
        ),
    ] {
        let e = apply_update(&current, &obj(update.clone()), &settings()).unwrap_err();
        assert_eq!(e.param.as_deref(), Some(param), "{update}");
    }
    // A shape error names the session.
    let e = apply_update(
        &current,
        &obj(json!({"type": "realtime", "instructions": 5})),
        &settings(),
    )
    .unwrap_err();
    assert_eq!(e.code.as_deref(), Some("invalid_value"));
}

#[test]
fn the_lmgw_object_is_strict_and_resolved_is_the_server_s() {
    let current = fresh();
    let e = apply_update(
        &current,
        &obj(json!({"type": "realtime",
        "lmgw": {"tts_model": "audio/pocket", "barge_in_min": 300}})),
        &settings(),
    )
    .unwrap_err();
    assert_eq!(e.param.as_deref(), Some("session.lmgw"));
    assert!(e.message.contains("barge_in_min"), "{}", e.message);

    let next = apply_update(
        &current,
        &obj(json!({"type": "realtime",
        "lmgw": {"tts_model": "audio/pocket", "barge_in_min_ms": 250,
                 "resolved": {"chat": "forged", "truncation": "auto"}}})),
        &settings(),
    )
    .unwrap();
    let s = echo(&next);
    assert_eq!(s["lmgw"]["tts_model"], "audio/pocket");
    assert_eq!(s["lmgw"]["barge_in_min_ms"], 250);
    assert_eq!(
        s["lmgw"]["resolved"]["chat"],
        Value::Null,
        "the client cannot write it"
    );
    assert_eq!(s["lmgw"]["resolved"]["truncation"], "disabled");

    // `null` resets the knobs.
    let reset = apply_update(
        &next,
        &obj(json!({"type": "realtime", "lmgw": null})),
        &settings(),
    )
    .unwrap();
    assert!(echo(&reset)["lmgw"].get("tts_model").is_none());
    assert_eq!(echo(&reset)["lmgw"]["resolved"]["truncation"], "disabled");
}

#[test]
fn server_owned_keys_are_not_the_client_s() {
    let next = apply_update(
        &fresh(),
        &obj(json!({"type": "realtime", "id": "sess_forged",
        "object": "something"})),
        &settings(),
    )
    .unwrap();
    assert_eq!(next.id.as_deref(), Some("sess_test1"));
    assert_eq!(next.object.as_deref(), Some("realtime.session"));
}

#[test]
fn a_retyped_object_is_replaced_whole() {
    let mut base = json!({"a": {"type": "x", "keep": 1}, "b": {"type": "y", "n": 1}});
    deep_merge(
        &mut base,
        &json!({"a": {"type": "z"}, "b": {"n": 2}, "c": [1]}),
    );
    assert_eq!(
        base,
        json!({"a": {"type": "z"}, "b": {"type": "y", "n": 2}, "c": [1]})
    );
}

#[test]
fn openai_realtime_names_are_recognised() {
    for n in [
        "gpt-realtime",
        "gpt-realtime-2.1",
        "gpt-realtime-mini",
        "gpt-4o-realtime-preview",
        "gpt-4o-mini-realtime-preview-2024-12-17",
    ] {
        assert!(is_openai_realtime_name(n), "{n}");
    }
    for n in ["gpt-4o", "gpt-4o-mini-transcribe", "qwen3-8b", "realtime"] {
        assert!(!is_openai_realtime_name(n), "{n}");
    }
}

#[test]
fn a_missing_type_takes_the_ga_default() {
    use lmgw_core::realtime::protocol::{AudioFormat, Tool};
    // `type` is optional in the GA params: `{rate}` is PCM, and a tool
    // without one is a function tool.
    let f: AudioFormat = serde_json::from_value(json!({"rate": 24000})).unwrap();
    assert_eq!(f, AudioFormat::Pcm { rate: 24000 });
    let f: AudioFormat = serde_json::from_value(json!({})).unwrap();
    assert_eq!(f, AudioFormat::Pcm { rate: 24000 });
    let f: AudioFormat = serde_json::from_value(json!({"type": "audio/pcmu"})).unwrap();
    assert_eq!(f, AudioFormat::Pcmu);
    // The echo always writes it.
    assert_eq!(
        serde_json::to_value(AudioFormat::Pcm { rate: 24000 }).unwrap(),
        json!({"type": "audio/pcm", "rate": 24000})
    );

    let t: Tool =
        serde_json::from_value(json!({"name": "f", "parameters": {"type": "object"}})).unwrap();
    assert_eq!(
        serde_json::to_value(&t).unwrap(),
        json!({"type": "function", "name": "f", "parameters": {"type": "object"}})
    );
    // A server-side tool parses as one; a type lmgw has no tool for does
    // not, and says which variant.
    let t: Tool = serde_json::from_value(json!({"type": "mcp", "server_label": "x"})).unwrap();
    assert_eq!(t.as_mcp().map(|m| m.server_label.as_str()), Some("x"));
    let e = serde_json::from_value::<Tool>(json!({"type": "web_search"}))
        .unwrap_err()
        .to_string();
    assert!(e.contains("web_search"), "{e}");

    // And in a session.update: `{rate}` alone merges as a format.
    let next = apply_update(
        &fresh(),
        &obj(
            json!({"type": "realtime", "audio": {"output": {"format": {"rate": 24000}}},
                    "tools": [{"name": "lookup"}]}),
        ),
        &settings(),
    )
    .unwrap();
    let e = echo(&next);
    assert_eq!(
        e["audio"]["output"]["format"],
        json!({"type": "audio/pcm", "rate": 24000})
    );
    assert_eq!(e["tools"][0]["type"], "function");
}
