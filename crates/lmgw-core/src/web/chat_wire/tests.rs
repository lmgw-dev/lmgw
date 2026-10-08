//! The wire, pinned: each DTO's bytes against what the `json!` builders
//! wrote before the types existed, on representative rows.

use lmgw_api_types::chat_feed::ThreadChanged;
use lmgw_api_types::chat_folders::{CurrentReason, CurrentThread, FolderOngoing};
use serde_json::{json, Value};

use super::*;
use crate::store::{
    AudioInputMode, ChatFolder, KbMode, ThreadDefaults, ThreadMcp, ThreadVoice, TurnDetection,
};

/// The thread list's row as `web::chat::thread_row_json` built it until
/// the DTOs replaced it (2026-10-07), kept as the reference.
/// The old builder's row, with the one field added since (2026-10-07:
/// `last_message_at`, here `null`).
fn old_thread_row_json(t: &ChatThread, purge: &PurgeDays) -> Value {
    let purge_days = purge.of(t);
    let mut v = serde_json::to_value(t).expect("ChatThread always serializes");
    v["last_message_at"] = Value::Null;
    let purge_at = (!t.pinned && purge_days > 0)
        .then_some(t.archived_at.as_deref())
        .flatten()
        .and_then(|a| store::chat_thread_purge_at(a, purge_days));
    v["purge_at"] = json!(purge_at);
    v["temporary"] = json!(ChatRepo::of(t.id).is_temp());
    v
}

fn plain() -> ChatThread {
    ChatThread {
        id: 1,
        title: "New chat".into(),
        model_alias: "gemma".into(),
        system_prompt: "Be brief.".into(),
        kind: "chat".into(),
        created_at: "2026-10-07 08:00:00".into(),
        updated_at: "2026-10-07 08:01:00".into(),
        ..Default::default()
    }
}

fn everything() -> ChatThread {
    ChatThread {
        id: 812,
        title: "Plan the week".into(),
        model_alias: "qwen".into(),
        system_prompt: String::new(),
        temperature: Some(0.7),
        max_tokens: Some(2048),
        top_p: Some(0.95),
        top_k: Some(40),
        min_p: Some(0.05),
        repeat_penalty: Some(1.1),
        presence_penalty: Some(0.0),
        frequency_penalty: Some(-0.5),
        seed: Some(42),
        stop: vec!["</s>".into(), "\n\n".into()],
        kind: "chat".into(),
        mcp_tools: vec![
            ThreadMcp {
                server_label: "kb".into(),
                allowed_tools: None,
            },
            ThreadMcp {
                server_label: "desktop".into(),
                allowed_tools: Some(vec!["desktop__notify".into()]),
            },
        ],
        reasoning_enabled: Some(true),
        reasoning_effort: Some("high".into()),
        reasoning_budget: Some(4096),
        agent_id: Some("folder-chat".into()),
        pinned: false,
        archived_at: Some("2026-10-01 12:00:00".into()),
        folder_id: Some(3),
        kb_ids: vec![2, 5],
        kb_mode: KbMode::Tool,
        kb_budget_tokens: Some(3000),
        voice: ThreadVoice {
            asr_alias: Some("parakeet".into()),
            tts_alias: Some("supertonic".into()),
            voice: Some("F2".into()),
            language: Some("de".into()),
            reply_language: Some("auto".into()),
            read_aloud: Some(false),
            turn_detection: Some(TurnDetection::PushToTalk),
            audio_input: Some(AudioInputMode::On),
            speech_style: Some(String::new()),
            seed: Some(7),
        },
        created_at: "2026-09-20 07:00:00".into(),
        updated_at: "2026-09-21 07:00:00".into(),
    }
}

fn rows() -> Vec<(&'static str, ChatThread)> {
    let mut pinned = everything();
    pinned.pinned = true;
    pinned.folder_id = None;
    let mut temporary = plain();
    temporary.id = -3;
    let mut admin = plain();
    admin.id = 9;
    admin.kind = "admin".into();
    admin.system_prompt = String::new();
    vec![
        ("plain", plain()),
        ("everything", everything()),
        ("pinned", pinned),
        ("temporary", temporary),
        ("admin", admin),
    ]
}

fn purge() -> PurgeDays {
    PurgeDays::fixed(30, &[(3, 365)])
}

fn listed() -> Vec<(&'static str, ChatFolderListed)> {
    let full = ChatFolderListed {
        folder: ChatFolder {
            id: 3,
            name: "Assistant".into(),
            sort: 2,
            defaults: ThreadDefaults {
                model_alias: Some("gemma".into()),
                system_prompt: Some("You are an assistant.".into()),
                temperature: Some(0.3),
                max_tokens: None,
                top_p: None,
                top_k: Some(20),
                min_p: None,
                repeat_penalty: None,
                presence_penalty: None,
                frequency_penalty: None,
                seed: None,
                stop: Some(vec!["END".into()]),
                reasoning_enabled: Some(false),
                reasoning_effort: None,
                reasoning_budget: None,
                mcp_tools: Some(vec![ThreadMcp {
                    server_label: "desktop".into(),
                    allowed_tools: None,
                }]),
                kb_ids: Some(vec![1]),
                kb_mode: Some(KbMode::Auto),
                kb_budget_tokens: None,
                voice: Some(ThreadVoice {
                    tts_alias: Some("supertonic".into()),
                    turn_detection: Some(TurnDetection::ServerVad),
                    ..Default::default()
                }),
            },
            ongoing: Some(FolderOngoing {
                idle_minutes: 30,
                current_thread_id: Some(812),
            }),
            archive_days: Some(0),
            purge_days: Some(365),
            created_at: "2026-10-06 09:00:00".into(),
            updated_at: "2026-10-07 09:00:00".into(),
            devices_hidden: false,
        },
        threads_active: 4,
        threads_archived: 11,
    };
    let bare = ChatFolderListed {
        folder: ChatFolder {
            id: 4,
            name: "Misc".into(),
            sort: 0,
            defaults: ThreadDefaults::default(),
            ongoing: None,
            archive_days: None,
            purge_days: None,
            created_at: "2026-10-06 09:00:00".into(),
            updated_at: "2026-10-06 09:00:00".into(),
            devices_hidden: false,
        },
        threads_active: 0,
        threads_archived: 0,
    };
    let mut ongoing_without_current = full.clone();
    ongoing_without_current.folder.ongoing = Some(FolderOngoing {
        idle_minutes: 0,
        current_thread_id: None,
    });
    vec![
        ("full", full),
        ("bare", bare),
        ("ongoing without a current thread", ongoing_without_current),
    ]
}

/// The bytes a `Json(value)` answer carries.
fn bytes(v: &Value) -> String {
    serde_json::to_string(v).unwrap()
}

#[test]
fn a_thread_row_is_byte_for_byte_the_old_one() {
    for (name, t) in rows() {
        let old = bytes(&old_thread_row_json(&t, &purge()));
        let new = bytes(&wire(&thread_row(&t, &purge(), None)));
        assert_eq!(new, old, "{name}");
    }
}

#[test]
fn a_folder_is_byte_for_byte_the_old_one() {
    for (name, f) in listed() {
        let old = bytes(&serde_json::to_value(&f).unwrap());
        let new = bytes(&wire(&folder(&f)));
        assert_eq!(new, old, "{name}");
    }
}

#[test]
fn an_open_thread_is_byte_for_byte_the_old_one() {
    let resolved = json!({"asr": {"alias": "parakeet", "source": "thread"}, "seed": null});
    let continue_state = json!({"ok": false, "reason": "there is no reply to continue yet"});
    for (name, t) in rows() {
        // `thread_json`, then `get_thread`'s and `current`'s `continue`.
        let mut old = old_thread_row_json(&t, &purge());
        old["voice_resolved"] = resolved.clone();
        let mut dto = api::Thread {
            row: thread_row(&t, &purge(), None),
            voice_resolved: resolved.clone(),
            continue_state: None,
        };
        assert_eq!(bytes(&wire(&dto)), bytes(&old), "{name}");
        old["continue"] = continue_state.clone();
        dto.continue_state = Some(serde_json::from_value(continue_state.clone()).unwrap());
        assert_eq!(bytes(&wire(&dto)), bytes(&old), "{name}, read whole");
    }
}

#[test]
fn the_lists_are_byte_for_byte_the_old_ones() {
    let threads: Vec<ChatThread> = rows().into_iter().map(|(_, t)| t).collect();
    let (stored, temporary): (Vec<_>, Vec<_>) = threads.iter().partition(|t| t.id > 0);
    let folders: Vec<ChatFolderListed> = listed().into_iter().map(|(_, f)| f).collect();
    let old = json!({
        "threads": stored.iter().map(|t| old_thread_row_json(t, &purge())).collect::<Vec<_>>(),
        "archived_count": 11,
        "temporary": temporary.iter().map(|t| old_thread_row_json(t, &purge())).collect::<Vec<_>>(),
        "folders": folders,
    });
    let new = api::ThreadList {
        threads: stored
            .iter()
            .map(|t| thread_row(t, &purge(), None))
            .collect(),
        archived_count: 11,
        temporary: temporary
            .iter()
            .map(|t| thread_row(t, &purge(), None))
            .collect(),
        folders: folders.iter().map(folder).collect(),
    };
    assert_eq!(bytes(&wire(&new)), bytes(&old));
    let old = json!({ "folders": folders });
    let new = api::FolderList {
        folders: folders.iter().map(folder).collect(),
    };
    assert_eq!(bytes(&wire(&new)), bytes(&old));
}

#[test]
fn a_feed_row_is_byte_for_byte_the_old_one() {
    for by in [None, Some("device 'phone'".to_string())] {
        for (name, t) in rows() {
            let mut old = old_thread_row_json(&t, &purge());
            old["by"] = json!(by);
            let new = ThreadChanged {
                thread: thread_row(&t, &purge(), None),
                by: by.clone(),
            };
            assert_eq!(bytes(&wire(&new)), bytes(&old), "{name}");
        }
        for (name, f) in listed() {
            let mut old = serde_json::to_value(&f).unwrap();
            old["by"] = json!(by);
            let new = lmgw_api_types::chat_feed::FolderChanged {
                folder: folder(&f),
                by: by.clone(),
            };
            assert_eq!(bytes(&wire(&new)), bytes(&old), "{name}");
        }
    }
}

/// `current`'s answer was a typed struct (fields in their declared order)
/// holding the thread as a value (keys sorted): both stay so.
#[test]
fn current_s_answer_is_byte_for_byte_the_old_one() {
    #[derive(serde::Serialize)]
    struct OldCurrent {
        thread: Value,
        rolled_over: bool,
        reason: Option<CurrentReason>,
        note: Option<String>,
    }
    let resolved = json!({"tts": {"alias": "supertonic"}});
    for (name, t) in rows() {
        let mut thread = old_thread_row_json(&t, &purge());
        thread["voice_resolved"] = resolved.clone();
        thread["continue"] = json!({"ok": true, "reason": null});
        let old = OldCurrent {
            thread,
            rolled_over: true,
            reason: Some(CurrentReason::Idle),
            note: Some("idle".into()),
        };
        let new = CurrentThread {
            thread: api::Thread {
                row: thread_row(&t, &purge(), None),
                voice_resolved: resolved.clone(),
                continue_state: Some(api::ContinueState {
                    ok: true,
                    reason: None,
                }),
            },
            rolled_over: true,
            reason: Some(CurrentReason::Idle),
            note: Some("idle".into()),
        };
        assert_eq!(
            serde_json::to_string(&new).unwrap(),
            serde_json::to_string(&old).unwrap(),
            "{name}"
        );
    }
}

/// The bytes themselves, as the `json!` builders wrote them on
/// 2026-10-07: a change here is a change of the wire, and needs a reason.
/// Since: a folder says `devices_hidden` (review F-7: the owner sees the
/// mark a device's delete set; a device never sees such a folder).
#[test]
fn the_wire_is_pinned() {
    let rows: std::collections::HashMap<_, _> = rows().into_iter().collect();
    let folders: std::collections::HashMap<_, _> = listed().into_iter().collect();
    let row = |name, last| bytes(&wire(&thread_row(&rows[name], &purge(), last)));
    let folder_of = |name| bytes(&wire(&folder(&folders[name])));
    assert_eq!(
        row("plain", None),
        r#"{"agent_id":null,"archived_at":null,"created_at":"2026-10-07 08:00:00","folder_id":null,"frequency_penalty":null,"id":1,"kb_budget_tokens":null,"kb_ids":[],"kb_mode":"auto","kind":"chat","last_message_at":null,"max_tokens":null,"mcp_tools":[],"min_p":null,"model_alias":"gemma","pinned":false,"presence_penalty":null,"purge_at":null,"reasoning_budget":null,"reasoning_effort":null,"reasoning_enabled":null,"repeat_penalty":null,"seed":null,"stop":[],"system_prompt":"Be brief.","temperature":null,"temporary":false,"title":"New chat","top_k":null,"top_p":null,"updated_at":"2026-10-07 08:01:00","voice":{}}"#
    );
    assert_eq!(
        row("everything", Some(1_758_438_000)),
        r#"{"agent_id":"folder-chat","archived_at":"2026-10-01 12:00:00","created_at":"2026-09-20 07:00:00","folder_id":3,"frequency_penalty":-0.5,"id":812,"kb_budget_tokens":3000,"kb_ids":[2,5],"kb_mode":"tool","kind":"chat","last_message_at":1758438000,"max_tokens":2048,"mcp_tools":[{"allowed_tools":null,"server_label":"kb"},{"allowed_tools":["desktop__notify"],"server_label":"desktop"}],"min_p":0.05,"model_alias":"qwen","pinned":false,"presence_penalty":0.0,"purge_at":"2027-10-01 12:00:00","reasoning_budget":4096,"reasoning_effort":"high","reasoning_enabled":true,"repeat_penalty":1.1,"seed":42,"stop":["</s>","\n\n"],"system_prompt":"","temperature":0.7,"temporary":false,"title":"Plan the week","top_k":40,"top_p":0.95,"updated_at":"2026-09-21 07:00:00","voice":{"asr_alias":"parakeet","audio_input":"on","language":"de","read_aloud":false,"reply_language":"auto","seed":7,"speech_style":"","tts_alias":"supertonic","turn_detection":"push_to_talk","voice":"F2"}}"#
    );
    assert_eq!(
        folder_of("full"),
        r#"{"archive_days":0,"created_at":"2026-10-06 09:00:00","defaults":{"frequency_penalty":null,"kb_budget_tokens":null,"kb_ids":[1],"kb_mode":"auto","max_tokens":null,"mcp_tools":[{"allowed_tools":null,"server_label":"desktop"}],"min_p":null,"model_alias":"gemma","presence_penalty":null,"reasoning_budget":null,"reasoning_effort":null,"reasoning_enabled":false,"repeat_penalty":null,"seed":null,"stop":["END"],"system_prompt":"You are an assistant.","temperature":0.3,"top_k":20,"top_p":null,"voice":{"tts_alias":"supertonic","turn_detection":"server_vad"}},"devices_hidden":false,"id":3,"name":"Assistant","ongoing":{"current_thread_id":812,"idle_minutes":30},"purge_days":365,"sort":2,"threads_active":4,"threads_archived":11,"updated_at":"2026-10-07 09:00:00"}"#
    );
    assert_eq!(
        folder_of("bare"),
        r#"{"archive_days":null,"created_at":"2026-10-06 09:00:00","defaults":{"frequency_penalty":null,"kb_budget_tokens":null,"kb_ids":null,"kb_mode":null,"max_tokens":null,"mcp_tools":null,"min_p":null,"model_alias":null,"presence_penalty":null,"reasoning_budget":null,"reasoning_effort":null,"reasoning_enabled":null,"repeat_penalty":null,"seed":null,"stop":null,"system_prompt":null,"temperature":null,"top_k":null,"top_p":null,"voice":null},"devices_hidden":false,"id":4,"name":"Misc","ongoing":null,"purge_days":null,"sort":0,"threads_active":0,"threads_archived":0,"updated_at":"2026-10-06 09:00:00"}"#
    );
}
