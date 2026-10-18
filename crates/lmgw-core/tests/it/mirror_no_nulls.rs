//! The gateway writes some answers through a struct of its own (a "mirror")
//! or a patched `Value`, not the `lmgw-api-types` type the document
//! describes. The document says the mirrored fields below are never `null`
//! (`openapi_ext::non_null`); each test here builds a mirror with everything
//! optional unset, writes it the way the gateway does, and finds no `null`
//! at those keys, so the document's word is the wire's.

use lmgw_api_types::chat_threads::{ServedModel as ApiServed, VoiceModels as ApiModels};
use lmgw_core::mcp::McpStatusView;
use lmgw_core::runtime::registry::{RuntimeState, RuntimeView};
use lmgw_core::runtime::Class;
use lmgw_core::store::{
    self, ChatAttachmentMeta, ChatReply, MessageVoice, NewAttachment, ServedModel, VoiceModels,
    VoiceTiming,
};
use serde_json::{json, Value};

use crate::common::serve;

/// The path of every `null` under `v`, `a.b[2].c` style.
fn null_paths(v: &Value) -> Vec<String> {
    fn walk(v: &Value, at: String, out: &mut Vec<String>) {
        match v {
            Value::Null => out.push(at),
            Value::Object(m) => m
                .iter()
                .for_each(|(k, v)| walk(v, format!("{at}.{k}"), out)),
            Value::Array(a) => a
                .iter()
                .enumerate()
                .for_each(|(i, v)| walk(v, format!("{at}[{i}]"), out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, String::new(), &mut out);
    out
}

/// No `null` under `v` sits at a path ending in one of `never` (`voice.asr`,
/// so `timing.models.asr` is not taken for it).
fn assert_never_null_at(what: &str, v: &Value, never: &[&str]) {
    for path in null_paths(v) {
        assert!(
            !never
                .iter()
                .any(|n| path == *n || path.ends_with(&format!(".{n}"))),
            "{what}: `{path}` is written as null, the document says it never is: {v}"
        );
    }
}

fn view() -> RuntimeView {
    RuntimeView {
        class: Class::Chat,
        model_id: "m".into(),
        container_name: "lmgw-chat-m".into(),
        generation: 1,
        port: 1,
        state: RuntimeState::Ready,
        in_flight: 0,
        started_at_age_seconds: 0,
        last_used_age_seconds: 0,
        warnings: vec![],
        image_capabilities: None,
        llama_props: None,
        rung: None,
        climbing: None,
        sends: 0,
        charge: None,
        resident_key: None,
        placement: Default::default(),
        owner: Default::default(),
        draining_for_owner: false,
    }
}

#[test]
fn a_runtime_view_with_nothing_optional_set_writes_no_null_and_none_of_the_six_keys() {
    let v = serde_json::to_value(view()).unwrap();
    assert_eq!(null_paths(&v), Vec::<String>::new(), "{v}");
    for key in [
        "image_capabilities",
        "llama_props",
        "rung",
        "climbing",
        "owner",
        "placement",
    ] {
        assert!(v.get(key).is_none(), "{key} is written unset: {v}");
    }
}

#[test]
fn a_message_voice_with_nothing_optional_set_writes_no_null() {
    let v = serde_json::to_value(MessageVoice {
        via: "realtime".into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(v, json!({"via": "realtime"}));
}

#[test]
fn a_realtime_chat_reply_with_nothing_optional_set_writes_no_null_and_no_key() {
    let reply = lmgw_api_types::chat_voice::ChatReply {
        message_id: 7,
        ..Default::default()
    };
    let v = serde_json::to_value(&reply).unwrap();
    assert_eq!(v, json!({"message_id": 7}));
    // And as the event carries it, flat beside `type`.
    let ev = lmgw_core::realtime::protocol::ServerEvent::LmgwChatReply(Box::new(reply));
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v, json!({"type": "lmgw.chat.reply", "message_id": 7}));
}

#[test]
fn a_voice_timing_and_its_served_models_write_no_null_at_the_documented_keys() {
    let voice = MessageVoice {
        via: "realtime".into(),
        timing: Some(VoiceTiming {
            models: VoiceModels {
                chat: Some(ServedModel {
                    alias: "chat".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        }),
        ..Default::default()
    };
    let v = json!({ "voice": voice });
    // The timing's own measurements and `ServedModel.answered_by` are sent as
    // `null` when unknown and are documented so; these are the ones that are not.
    assert_never_null_at(
        "MessageVoice",
        &v,
        &[
            "voice.asr",
            "voice.asr_answered_by",
            "voice.asr_ms",
            "voice.audio_ms",
            "voice.tts",
            "voice.tts_answered_by",
            "voice.voice",
            "voice.unheard",
            "voice.input",
            "voice.transcript_error",
            "voice.timing",
            "timing.reasoning_ms",
            "timing.first_clause",
            "timing.input",
            "timing.input_why",
            "timing.transcript_wait_ms",
            "models.chat.voice",
        ],
    );
    // And what the mirror writes reads as the documented type.
    let read: lmgw_api_types::chat_threads::MessageVoice =
        serde_json::from_value(v["voice"].clone()).unwrap();
    let timing = read.timing.unwrap();
    assert_eq!(
        timing.models,
        ApiModels {
            chat: Some(ApiServed {
                alias: "chat".into(),
                answered_by: None,
                voice: None
            }),
            ..Default::default()
        }
    );
}

#[test]
fn a_chat_attachment_meta_with_no_blockers_or_hints_writes_neither_key() {
    let v = serde_json::to_value(ChatAttachmentMeta {
        id: 1,
        thread_id: 1,
        message_id: None,
        kind: "image".into(),
        name: "a.png".into(),
        mime: "image/png".into(),
        size: 3,
        mode: None,
        meta: json!({}),
        extracted_tokens: None,
        blockers: None,
        hints: None,
    })
    .unwrap();
    assert!(
        v.get("blockers").is_none() && v.get("hints").is_none(),
        "{v}"
    );
}

#[test]
fn an_mcp_status_view_always_writes_a_whole_number_tool_count() {
    let v = serde_json::to_value(McpStatusView {
        id: 1,
        name: "s".into(),
        status: "stopped",
        tool_count: 0,
        detail: None,
    })
    .unwrap();
    assert_eq!(v["tool_count"], 0, "{v}");
    let read: lmgw_api_types::McpStatus = serde_json::from_value(v).unwrap();
    assert_eq!(read.tool_count, 0);
}

/// The Chat export of a thread with no folder, plain messages, a voice reply
/// with only `via` and a draft attachment: the `lmgw.chat.v1` file leaves
/// every documented-never-null key out.
#[tokio::test]
async fn a_chat_export_without_the_optional_facts_writes_none_of_their_keys() {
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let db = &state.db;
    let id = store::create_chat_thread(db, "m", "chat").await.unwrap();
    store::append_chat_message(db, id, "user", "q", "", None, None, None)
        .await
        .unwrap();
    store::append_chat_reply(
        db,
        id,
        &ChatReply {
            content: "a".into(),
            voice: Some(MessageVoice {
                via: "realtime".into(),
                timing: Some(VoiceTiming::default()),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    store::insert_chat_attachment_new(
        db,
        id,
        &NewAttachment::plain("image", "pic.png", "image/png", &[1, 2, 3]),
    )
    .await
    .unwrap();
    let v: Value = gw
        .client()
        .get(format!("{gw}/chat/api/threads/{id}/export?format=json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(v.get("folder").is_none(), "a thread in no folder: {v}");
    assert_never_null_at(
        "ChatExport",
        &v,
        &[
            "folder",
            "ir_messages_unparsed",
            "images_note",
            "task",
            "blockers",
            "hints",
            "voice.asr",
            "voice.asr_answered_by",
            "voice.asr_ms",
            "voice.audio_ms",
            "voice.tts",
            "voice.tts_answered_by",
            "voice.voice",
            "voice.unheard",
            "voice.input",
            "voice.transcript_error",
            "voice.timing",
            "timing.reasoning_ms",
            "timing.first_clause",
            "timing.input",
            "timing.input_why",
            "timing.transcript_wait_ms",
            "models.chat.voice",
        ],
    );
    let m = &v["messages"][1];
    for key in ["ir_messages_unparsed", "images_note", "task"] {
        assert!(m.get(key).is_none(), "{key} is written unset: {m}");
    }
    let att = &v["draft_attachments"][0];
    assert!(
        att.get("blockers").is_none() && att.get("hints").is_none(),
        "{att}"
    );
}
