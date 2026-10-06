//! What a bound session's thread owns (chat-voice design §8.1): a
//! `session.update` that changes one of its fields, or sets it to `null`, is
//! refused with `owned_by_thread`, and so are the overrides a
//! `response.create` carries for them and `conversation.item.create` /
//! `.delete` — the thread's history is the conversation. The page changes
//! them through the thread's settings route; the session reads the thread
//! again before each turn, so a change applies from the next one.
//!
//! **An equal value is accepted**: clients echo the whole session back
//! through realtime's deep merge. So the merged session is compared with
//! the current one, field by field — except the instructions, which the
//! merge itself rewrites when the output modality changes (a client-owned
//! field): those are refused only when the update names a different value.
//!
//! Beyond §8.1's list, `reasoning`, `max_output_tokens` and
//! `parallel_tool_calls` are the thread's too: the turn is the Chat's, with
//! the thread's reasoning and output settings, so a session's would be
//! accepted and silently not applied.

use serde_json::{Map, Value};

use super::super::protocol::{ErrorObject, ResponseCreateParams, Session};

/// The session fields the thread owns, as JSON pointers into the session,
/// and the parameter an error names.
const OWNED: &[(&str, &str)] = &[
    ("/model", "session.model"),
    ("/tools", "session.tools"),
    ("/tool_choice", "session.tool_choice"),
    ("/parallel_tool_calls", "session.parallel_tool_calls"),
    ("/max_output_tokens", "session.max_output_tokens"),
    ("/reasoning", "session.reasoning"),
    (
        "/audio/input/transcription/model",
        "session.audio.input.transcription.model",
    ),
    (
        "/audio/input/transcription/language",
        "session.audio.input.transcription.language",
    ),
    ("/audio/output/voice", "session.audio.output.voice"),
    ("/lmgw/tts_model", "session.lmgw.tts_model"),
    (
        "/lmgw/speech_instructions",
        "session.lmgw.speech_instructions",
    ),
    ("/lmgw/speech_seed", "session.lmgw.speech_seed"),
    ("/lmgw/tag_hint", "session.lmgw.tag_hint"),
];

fn owned(param: &str, what: &str) -> ErrorObject {
    ErrorObject::invalid(
        "owned_by_thread",
        format!(
            "{param} belongs to the chat thread this session is bound to: {what} — change it in \
             the thread's settings, which apply from the next turn"
        ),
    )
    .with_param(param)
}

/// A `session.update` of a bound session (module doc): `update` as the
/// client sent it, `next` the session it merges into.
pub(in crate::realtime) fn check_update(
    current: &Session,
    next: &Session,
    update: &Map<String, Value>,
) -> Result<(), ErrorObject> {
    let (Ok(before), Ok(after)) = (serde_json::to_value(current), serde_json::to_value(next))
    else {
        return Ok(());
    };
    for (pointer, param) in OWNED {
        if before.pointer(pointer) != after.pointer(pointer) {
            return Err(owned(param, "the update changes it"));
        }
    }
    if let Some(asked) = update.get("instructions") {
        let now = current.instructions.as_deref().unwrap_or_default();
        if asked.as_str() != Some(now) {
            return Err(owned(
                "session.instructions",
                "a voice turn's system prompt is the thread's prompt with the spoken-style \
                 instructions after it",
            ));
        }
    }
    Ok(())
}

/// A `response.create` of a bound session: its overrides of what the
/// thread owns are refused (module doc).
pub(in crate::realtime) fn check_create(
    p: Option<&ResponseCreateParams>,
) -> Result<(), ErrorObject> {
    let Some(p) = p else {
        return Ok(());
    };
    let voice = p
        .audio
        .as_ref()
        .and_then(|a| a.pointer("/output/voice"))
        .is_some_and(|v| !v.is_null());
    let style = p
        .lmgw
        .as_ref()
        .is_some_and(|l| l.speech_instructions.is_some());
    let asked = [
        ("response.instructions", p.instructions.is_some()),
        ("response.tools", p.tools.is_some()),
        ("response.tool_choice", p.tool_choice.is_some()),
        ("response.max_output_tokens", p.max_output_tokens.is_some()),
        ("response.audio.output.voice", voice),
        ("response.lmgw.speech_instructions", style),
    ];
    match asked.into_iter().find(|(_, set)| *set) {
        Some((param, _)) => Err(owned(param, "a response cannot override it")),
        None => Ok(()),
    }
}

/// `conversation.item.create` or `.delete` in a bound session: the
/// thread's history is the conversation.
pub(in crate::realtime) fn refuse_item(event: &str) -> ErrorObject {
    ErrorObject::invalid(
        "owned_by_thread",
        format!(
            "{event} is not available in a session bound to a chat thread: the thread's history \
             is the conversation — speak a turn, or edit the thread in the Chat"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RealtimeSettings;
    use crate::realtime::merge::{apply_update, initial_session};
    use serde_json::json;

    fn session() -> Session {
        initial_session(
            &RealtimeSettings::default(),
            "sess_1".into(),
            Some("m".into()),
        )
    }

    fn update(current: &Session, patch: Value) -> Result<(), ErrorObject> {
        let patch = patch.as_object().unwrap().clone();
        let next = apply_update(current, &patch, &RealtimeSettings::default()).unwrap();
        check_update(current, &next, &patch)
    }

    #[test]
    fn an_echo_and_the_client_s_own_fields_pass_a_change_of_the_thread_s_does_not() {
        let s = session();
        // The whole session echoed back.
        let mut echo = serde_json::to_value(&s).unwrap();
        echo["type"] = json!("realtime");
        assert!(update(&s, echo).is_ok());
        // The client's own: turn detection, the modality, the barge knobs.
        assert!(update(
            &s,
            json!({"type": "realtime", "output_modalities": ["text"],
                   "audio": {"input": {"turn_detection": null}},
                   "lmgw": {"half_duplex": true, "echo_tail_ms": 80}})
        )
        .is_ok());
        for (patch, param) in [
            (json!({"model": "other"}), "session.model"),
            (
                json!({"audio": {"output": {"voice": "cosette"}}}),
                "session.audio.output.voice",
            ),
            (
                json!({"lmgw": {"tts_model": "x"}}),
                "session.lmgw.tts_model",
            ),
            (json!({"instructions": "be brief"}), "session.instructions"),
            (json!({"tools": []}), "session.tools"),
        ] {
            let mut p = patch.clone();
            p["type"] = json!("realtime");
            if param == "session.tools" {
                // An equal (empty) list is no change.
                assert!(update(&s, p).is_ok());
                continue;
            }
            let e = update(&s, p).unwrap_err();
            assert_eq!(e.code.as_deref(), Some("owned_by_thread"), "{patch}");
            assert_eq!(e.param.as_deref(), Some(param), "{patch}");
        }
        // A server-side tool is a change of the thread's tools: its MCP
        // tools run through the Chat's own turn (realtime-server-tools).
        let e = update(
            &s,
            json!({"type": "realtime", "tools": [{"type": "mcp", "server_label": "docs"}]}),
        )
        .unwrap_err();
        assert_eq!(e.code.as_deref(), Some("owned_by_thread"));
        assert_eq!(e.param.as_deref(), Some("session.tools"));
        // `null` is a change too.
        let e = update(
            &s,
            json!({"type": "realtime", "audio": {"output": {"voice": null}},
                   "model": null}),
        )
        .unwrap_err();
        assert_eq!(e.code.as_deref(), Some("owned_by_thread"));
    }

    #[test]
    fn a_response_create_may_not_override_what_the_thread_owns() {
        let p = |v: Value| serde_json::from_value::<ResponseCreateParams>(v).unwrap();
        assert!(check_create(None).is_ok());
        assert!(check_create(Some(&p(json!({"output_modalities": ["text"]})))).is_ok());
        let e = check_create(Some(&p(json!({"instructions": "x"})))).unwrap_err();
        assert_eq!(e.param.as_deref(), Some("response.instructions"));
        let e = check_create(Some(&p(json!({"audio": {"output": {"voice": "x"}}})))).unwrap_err();
        assert_eq!(e.param.as_deref(), Some("response.audio.output.voice"));
        let e = check_create(Some(&p(json!({"lmgw": {"speech_instructions": ""}})))).unwrap_err();
        assert_eq!(
            e.param.as_deref(),
            Some("response.lmgw.speech_instructions")
        );
    }
}
