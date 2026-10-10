//! The Chat API's thread routes against their documented types
//! (`lmgw-api-types::chat_threads`): each answer, read into its type and
//! written back, is the answer again, key by key. A field the gateway adds
//! to an answer without adding it to the type is dropped by the read and
//! fails here.

use lmgw_api_types::chat::{Ack, Thread};
use lmgw_api_types::chat_folders::FolderPatched;
use lmgw_api_types::chat_threads::{
    ReplyEdited, SearchPage, SettingsAck, SpeechStopped, ThreadDetail, ThreadKept,
};
use lmgw_core::agent::PendingCall;
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::store::{
    self, ChatContext, ChatReply, ContextExcerpt, InputPath, MessageVoice, PendingApprovals,
    SendMessageOutcome, ServedModel, VoiceModels, VoiceTiming,
};
use serde_json::{json, Value};
use wiremock::MockServer;

use crate::chat_actions::{gateway, get_json, post};
use crate::common::round_trips;

async fn json_of(r: reqwest::Response, what: &str) -> Value {
    let status = r.status();
    let body: Value = r.json().await.unwrap();
    assert_eq!(status, 200, "{what}: {body}");
    body
}

/// A spoken reply's voice with every field set.
fn full_voice() -> MessageVoice {
    let served = |alias: &str| ServedModel {
        alias: alias.into(),
        answered_by: Some("fallback".into()),
        voice: Some("v1".into()),
    };
    MessageVoice {
        via: "realtime".into(),
        asr: Some("asr".into()),
        asr_answered_by: Some("asr2".into()),
        asr_ms: Some(11),
        audio_ms: Some(2200),
        tts: Some("tts".into()),
        tts_answered_by: Some("tts2".into()),
        voice: Some("v1".into()),
        unheard: Some("the rest".into()),
        input: Some(InputPath::Audio),
        transcript_error: Some("none heard".into()),
        timing: Some(VoiceTiming {
            response_id: Some("resp_1".into()),
            message_id: Some(3),
            end_of_turn_ms: Some(1),
            asr_ms: Some(2),
            first_token_ms: Some(3),
            reasoning_ms: Some(4),
            first_clause_ms: Some(5),
            first_audio_ms: Some(6),
            total_ms: Some(7),
            to_first_audio_ms: Some(8),
            cold: vec!["tts".into()],
            first_clause: Some("announcement".into()),
            models: VoiceModels {
                asr: Some(served("asr")),
                chat: Some(served("m")),
                tts: Some(served("tts")),
            },
            input: Some(InputPath::Transcript),
            input_why: Some("why".into()),
            transcript_wait_ms: Some(9),
        }),
    }
}

#[tokio::test]
async fn a_thread_reads_into_its_type_whole() {
    let mock = MockServer::start().await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = post(&gw, "/chat/api/threads", json!({"model_alias": "m"}))
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    // A sent attachment, a draft, a retrieval, a spoken turn.
    let sent =
        store::insert_chat_attachment(&state.db, tid, "text", "a.txt", "text/plain", 3, b"abc")
            .await
            .unwrap();
    let draft =
        store::insert_chat_attachment(&state.db, tid, "text", "b.txt", "text/plain", 3, b"abc")
            .await
            .unwrap();
    let _ = draft;
    let SendMessageOutcome::Sent(user) = store::append_user_message_with_voice(
        &state.db,
        tid,
        "hello",
        &[sent],
        &[4],
        Some(&MessageVoice {
            via: "dictation".into(),
            asr: Some("asr".into()),
            ..Default::default()
        }),
    )
    .await
    .unwrap() else {
        panic!("the draft binds")
    };
    store::set_chat_message_knowledge(
        &state.db,
        tid,
        user,
        &[4],
        Some(&ChatContext {
            excerpts: vec![ContextExcerpt {
                kb_id: 4,
                kb: "kb".into(),
                file_id: 5,
                file: "f.md".into(),
                page: Some(2),
                chunk_id: "c1".into(),
                heading_path: "A > B".into(),
                text: "excerpt".into(),
                score: 0.3,
                tokens: 12,
                span_start: 0,
                span_end: 7,
                file_sha: "abc".into(),
            }],
            tokens: 12,
            dropped: 1,
            budget_tokens: 100,
            notes: vec!["a note".into()],
            searched: vec!["kb".into()],
            kb_ids: vec![4],
            query: "q".into(),
            ms: 1.5,
        }),
    )
    .await
    .unwrap();
    store::append_chat_reply(
        &state.db,
        tid,
        &ChatReply {
            content: "answer".into(),
            reasoning: "thinking".into(),
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            ir_messages: Some("[]".into()),
            model: Some("m".into()),
            answered_by: Some("other".into()),
            images_note: Some("blind".into()),
            voice: Some(full_voice()),
            pending_approvals: Some(PendingApprovals {
                calls: vec![PendingCall {
                    approval_id: "mcpr_1".into(),
                    call_id: "call_1".into(),
                    name: "srv__tool".into(),
                    args: json!("{\"a\":1}"),
                    server_label: "srv".into(),
                    needs_approval: true,
                }],
                ..Default::default()
            }),
        },
    )
    .await
    .unwrap();

    let live = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    let detail: ThreadDetail = round_trips("GET thread", &live);
    assert_eq!(detail.messages.len(), 2);
    let reply = &detail.messages[1];
    let pending = reply.pending_approvals.as_ref().expect("the call waits");
    assert_eq!(pending[0].approval_request_id, "mcpr_1");
    assert!(reply.voice.as_ref().unwrap().timing.is_some());
    assert_eq!(detail.messages[0].attachments.len(), 1);
    assert_eq!(
        detail.messages[0].context.as_ref().unwrap().excerpts.len(),
        1
    );
    assert_eq!(detail.draft_attachments.len(), 1);
    assert!(detail.thread.continue_state.is_some());
}

#[tokio::test]
async fn the_thread_routes_answer_their_types() {
    let mock = MockServer::start().await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let created = json_of(
        post(&gw, "/chat/api/threads", json!({"model_alias": "m"})).await,
        "create",
    )
    .await;
    round_trips::<Thread>("create", &created);
    let tid = created["id"].as_i64().unwrap();
    let route = |tail: &str| format!("/chat/api/threads/{tid}/{tail}");

    let settings = json_of(
        post(
            &gw,
            &route("settings"),
            json!({"temperature": 0.5, "top_k": null, "voice": {"read_aloud": true}}),
        )
        .await,
        "settings",
    )
    .await;
    let ack: SettingsAck = round_trips("settings", &settings);
    assert!(ack.ok && ack.voice.read_aloud == Some(true));

    let stopped = json_of(post(&gw, &route("speech/stop"), json!({})).await, "stop").await;
    assert_eq!(
        round_trips::<SpeechStopped>("speech stop", &stopped).stopped,
        0
    );

    for (tail, body) in [
        ("pin", json!({"pinned": true})),
        ("archive", json!({"archived": true})),
        ("archive", json!({"archived": false})),
        ("move", json!({"folder_id": null})),
    ] {
        let v = json_of(post(&gw, &route(tail), body).await, tail).await;
        round_trips::<Thread>(tail, &v);
    }

    // A reply edit answers JSON; a delete answers {ok}.
    let reply = store::append_chat_message(&state.db, tid, "assistant", "x", "", None, None, None)
        .await
        .unwrap();
    let edited = json_of(
        post(
            &gw,
            &route(&format!("messages/{reply}/edit")),
            json!({"content": "y"}),
        )
        .await,
        "edit",
    )
    .await;
    assert_eq!(
        round_trips::<ReplyEdited>("edit", &edited).message.content,
        "y"
    );
    let gone = json_of(
        post(&gw, &route(&format!("messages/{reply}/delete")), json!({})).await,
        "message delete",
    )
    .await;
    round_trips::<Ack>("message delete", &gone);

    // The search.
    store::append_chat_message(&state.db, tid, "user", "needlework", "", None, None, None)
        .await
        .unwrap();
    let found = get_json(&gw, "/chat/api/search?q=needlework").await;
    let page: SearchPage = round_trips("search", &found);
    assert_eq!(page.threads.len(), 1);

    // A folder patch and delete.
    let folder = json_of(
        post(&gw, "/chat/api/folders", json!({"name": "F"})).await,
        "folder create",
    )
    .await;
    let fid = folder["id"].as_i64().unwrap();
    let patched = json_of(
        post(
            &gw,
            &format!("/chat/api/folders/{fid}"),
            json!({"name": "G", "defaults_patch": {"temperature": 0.2}}),
        )
        .await,
        "folder patch",
    )
    .await;
    assert_eq!(
        round_trips::<FolderPatched>("folder patch", &patched)
            .folder
            .name,
        "G"
    );
    let deleted = json_of(
        post(
            &gw,
            &format!("/chat/api/folders/{fid}/delete"),
            json!({"threads": "keep"}),
        )
        .await,
        "folder delete",
    )
    .await;
    round_trips::<Ack>("folder delete", &deleted);

    // Keep, then delete.
    let temp = json_of(
        post(
            &gw,
            "/chat/api/threads",
            json!({"model_alias": "m", "temporary": true}),
        )
        .await,
        "temporary",
    )
    .await;
    let kept = json_of(
        post(
            &gw,
            &format!("/chat/api/threads/{}/persist", temp["id"]),
            json!({}),
        )
        .await,
        "persist",
    )
    .await;
    let kept: ThreadKept = round_trips("persist", &kept);
    assert!(kept.id > 0);
    let del = json_of(post(&gw, &route("delete"), json!({})).await, "delete").await;
    round_trips::<Ack>("thread delete", &del);
}
