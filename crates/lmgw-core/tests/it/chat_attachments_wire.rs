//! The Chat API's attachment and export routes against their documented types
//! (`lmgw-api-types::chat_attachments`, `chat_export`): each answer, read
//! into its type and written back, is the answer again, key by key. A field
//! the gateway adds to an answer without adding it to the type is dropped by
//! the read and fails here.

use std::io::{Cursor, Read};

use lmgw_api_types::chat::Ack;
use lmgw_api_types::chat_attachments::{ModeSet, Transcribed};
use lmgw_api_types::chat_export::{ChatExport, EXPORT_FORMAT};
use lmgw_api_types::chat_threads::{AttachmentFacts, AttachmentMeta};
use lmgw_core::ops::settings_set;
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_attach_kinds::{chat_mock, new_thread, patch, pdf_bytes, setup, upload_ok, wav};
use crate::chat_export::rich_thread;
use crate::common::{round_trips, Gw};

async fn post_json(gw: &Gw, route: &str, body: Value) -> (u16, Value) {
    let r = gw
        .client()
        .post(format!("{gw}{route}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

async fn get(gw: &Gw, route: &str) -> reqwest::Response {
    gw.client()
        .get(format!("{gw}{route}"))
        .send()
        .await
        .unwrap()
}

fn header(r: &reqwest::Response, name: &str) -> String {
    r.headers()[name].to_str().unwrap().to_string()
}

#[tokio::test]
async fn the_attachment_routes_answer_their_types() {
    let chat = chat_mock().await;
    let stt = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "text": "heard it" })))
        .mount(&stt)
        .await;
    let (state, gw) = setup(&chat, &stt).await;
    let tid = new_thread(&gw, "plain", false).await;

    // Upload: a text file and a text-class PDF, both as AttachmentMeta.
    let text = upload_ok(&gw, tid, "a.txt", b"hello there".to_vec()).await;
    round_trips::<AttachmentMeta>("upload text", &text);
    let pdf = upload_ok(&gw, tid, "p.pdf", pdf_bytes(&[Some("page one text")])).await;
    let pdf_meta = round_trips::<AttachmentMeta>("upload pdf", &pdf);
    assert_eq!(pdf_meta.meta["class"], "text");
    // The extraction facts are the documented ones, none left over.
    let facts = round_trips::<AttachmentFacts>("pdf facts", &pdf_meta.meta);
    assert!(
        facts.other.is_empty() && facts.pages == Some(1),
        "{facts:?}"
    );
    round_trips::<AttachmentFacts>(
        "text facts",
        &round_trips::<AttachmentMeta>("t", &text).meta,
    );
    let pid = pdf_meta.id;

    // Mode.
    let (status, v) = post_json(
        &gw,
        &format!("/chat/api/attachments/{pid}/mode"),
        json!({"mode": "images"}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let set = round_trips::<ModeSet>("mode", &v);
    assert_eq!((set.id, set.mode.as_str()), (pid, "images"));

    // Text and bytes: the types' content types and the sandbox headers.
    let r = get(&gw, &format!("/chat/api/attachments/{pid}/text")).await;
    assert_eq!(header(&r, "content-type"), "text/plain; charset=utf-8");
    assert_eq!(header(&r, "x-content-type-options"), "nosniff");
    assert_eq!(header(&r, "content-security-policy"), "sandbox");
    let r = get(&gw, &format!("/chat/api/attachments/{pid}")).await;
    assert_eq!(header(&r, "content-type"), "application/pdf");
    assert_eq!(header(&r, "x-content-type-options"), "nosniff");
    assert_eq!(header(&r, "content-security-policy"), "sandbox");
    let tid_text = text["id"].as_i64().unwrap();
    let r = get(&gw, &format!("/chat/api/attachments/{tid_text}")).await;
    assert_eq!(header(&r, "content-type"), "text/plain; charset=utf-8");

    // Transcribe: an audio draft uploaded before a speech-to-text model was set.
    let audio = upload_ok(&gw, tid, "memo.wav", wav()).await;
    let aid = audio["id"].as_i64().unwrap();
    settings_set(&state, patch(json!({ "chat_stt_alias": "my-asr" })))
        .await
        .unwrap();
    let (status, v) = post_json(
        &gw,
        &format!("/chat/api/attachments/{aid}/transcribe"),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let done = round_trips::<Transcribed>("transcribe", &v);
    assert_eq!(done.id, aid);
    assert_eq!(done.meta["transcript_alias"], "my-asr");
    let facts = round_trips::<AttachmentFacts>("transcript facts", &done.meta);
    assert!(facts.other.is_empty(), "{facts:?}");
    assert_eq!(facts.transcript_alias.as_deref(), Some("my-asr"));

    // Delete.
    let (status, v) = post_json(
        &gw,
        &format!("/chat/api/attachments/{tid_text}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(round_trips::<Ack>("delete", &v), Ack::ok());
}

/// The JSON file of a thread, read into the bundle type and written back.
fn bundle(what: &str, bytes: &[u8]) -> ChatExport {
    let live: Value = serde_json::from_slice(bytes).unwrap();
    let typed = round_trips::<ChatExport>(what, &live);
    assert_eq!(typed.format, EXPORT_FORMAT);
    typed
}

#[tokio::test]
async fn an_export_has_its_documented_content_type_disposition_and_bundle() {
    let chat = chat_mock().await;
    let stt = MockServer::start().await;
    let (state, gw) = setup(&chat, &stt).await;
    let id = rich_thread(&state, "Bilder und Notizen").await;
    // A draft too, and a record that does not parse.
    store::insert_chat_attachment(&state.db, id, "text", "draft.txt", "text/plain", 3, b"abc")
        .await
        .unwrap();
    store::append_chat_message(
        &state.db,
        id,
        "assistant",
        "odd",
        "",
        None,
        None,
        Some("not json"),
    )
    .await
    .unwrap();

    let r = get(&gw, &format!("/chat/api/threads/{id}/export?format=md")).await;
    assert_eq!(header(&r, "content-type"), "text/markdown; charset=utf-8");
    assert!(header(&r, "content-disposition").starts_with("attachment; filename=\"lmgw-chat-"));

    let r = get(&gw, &format!("/chat/api/threads/{id}/export?format=json")).await;
    assert_eq!(header(&r, "content-type"), "application/json");
    assert!(header(&r, "content-disposition").contains(".json"));
    let typed = bundle("thread json", &r.bytes().await.unwrap());
    assert_eq!(typed.thread.id, id);
    assert!(typed
        .messages
        .iter()
        .any(|m| m.ir_messages_unparsed == Some(true)));
    assert!(typed.messages.iter().any(|m| !m.attachments.is_empty()));
    assert_eq!(typed.draft_attachments.len(), 1);
    assert!(typed.folder.is_none());

    for route in [
        "/chat/api/export?format=json".to_string(),
        "/chat/api/export?format=md&archived=0".to_string(),
    ] {
        let r = get(&gw, &route).await;
        assert_eq!(header(&r, "content-type"), "application/zip", "{route}");
        assert!(header(&r, "content-disposition").contains(".zip"));
        let bytes = r.bytes().await.unwrap();
        let mut z = zip::ZipArchive::new(Cursor::new(bytes.to_vec())).unwrap();
        for i in 0..z.len() {
            let mut f = z.by_index(i).unwrap();
            let name = f.name().to_string();
            let mut data = Vec::new();
            f.read_to_end(&mut data).unwrap();
            if name.ends_with(".json") {
                bundle(&name, &data);
            }
        }
    }

    // A thread in a folder: the folder is named in the bundle, and the
    // folder's zip answers the same type.
    let (status, f) = post_json(&gw, "/chat/api/folders", json!({"name": "Notes"})).await;
    assert_eq!(status, 200, "{f}");
    let folder = f["id"].as_i64().unwrap();
    let (status, v) = post_json(
        &gw,
        &format!("/chat/api/threads/{id}/move"),
        json!({"folder_id": folder}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let r = get(&gw, &format!("/chat/api/threads/{id}/export?format=json")).await;
    let typed = bundle("folder json", &r.bytes().await.unwrap());
    assert_eq!(typed.folder.unwrap().name, "Notes");
    let r = get(
        &gw,
        &format!("/chat/api/folders/{folder}/export?format=json"),
    )
    .await;
    assert_eq!(header(&r, "content-type"), "application/zip");
    assert!(header(&r, "content-disposition").starts_with("attachment; filename=\"lmgw-folder-"));
}
