//! Chat export (chat-complete design §6): Markdown and lossless JSON for one
//! thread, a `.zip` per folder and for everything, a temporary thread from
//! memory, and the download headers.

use std::io::{Cursor, Read};

use base64::Engine;
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{
    self, ChatContext, ContextExcerpt, KbMode, NewAttachment, SendMessageOutcome, ThreadDefaults,
};
use serde_json::{json, Value};
use wiremock::MockServer;

use crate::chat_actions::{gateway, mount_openai_reply, post};
use crate::common::{serve, Gw};

async fn gw() -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0, 1, 2, 254, 255, 0];

/// A stored thread with every kind of content: settings, a user turn with an
/// attachment and retrieval, an assistant turn with reasoning and a tool
/// record.
async fn rich_thread(state: &SharedState, title: &str) -> i64 {
    let db = &state.db;
    let id = store::create_chat_thread_with_prompt(db, "m-alias", "chat", "Be ``` careful.", None)
        .await
        .unwrap();
    let mut t = store::get_chat_thread(db, id).await.unwrap().unwrap();
    t.title = title.into();
    t.temperature = Some(0.3);
    t.max_tokens = Some(777);
    t.top_p = Some(0.9);
    t.top_k = Some(40);
    t.min_p = Some(0.05);
    t.repeat_penalty = Some(1.1);
    t.presence_penalty = Some(0.2);
    t.frequency_penalty = Some(0.3);
    t.seed = Some(42);
    t.stop = vec!["END".into()];
    t.reasoning_enabled = Some(true);
    t.reasoning_effort = Some("high".into());
    t.reasoning_budget = Some(2048);
    t.kb_ids = vec![7];
    t.kb_mode = KbMode::Tool;
    t.kb_budget_tokens = Some(1234);
    store::update_chat_thread_settings(db, &t, store::SeedWrite::AsGiven, None)
        .await
        .unwrap();

    let aid = store::insert_chat_attachment_new(
        db,
        id,
        &NewAttachment::plain("image", "pic.png", "image/png", PNG),
    )
    .await
    .unwrap();
    let SendMessageOutcome::Sent(uid) =
        store::append_user_message_with_kb_refs(db, id, "What is in the file?", &[aid], &[7])
            .await
            .unwrap()
    else {
        panic!("draft not bindable")
    };
    let ctx = ChatContext {
        excerpts: vec![ContextExcerpt {
            kb_id: 7,
            kb: "Manuals".into(),
            file_id: 3,
            file: "handbook.pdf".into(),
            page: Some(12),
            text: "the excerpt text".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    store::set_chat_message_knowledge(db, id, uid, &[7], Some(&ctx))
        .await
        .unwrap();
    let ir =
        r#"[{"role":"assistant","tool_calls":[{"id":"c1","name":"lmgw__status","arguments":{}}]}]"#;
    // Answered by a fallback, on a model the thread has since moved away
    // from: the transcript names who answered, not the thread's model now.
    store::append_chat_reply(
        db,
        id,
        &store::ChatReply {
            content: "It is a picture.".into(),
            reasoning: "let me think it over".into(),
            prompt_tokens: Some(11),
            completion_tokens: Some(5),
            ir_messages: Some(ir.into()),
            model: Some("m-earlier".into()),
            answered_by: Some("cloud-fb".into()),
            images_note: None,
            voice: None,
        },
    )
    .await
    .unwrap();
    id
}

async fn export(gw: &Gw, route: &str) -> reqwest::Response {
    gw.client()
        .get(format!("{gw}{route}"))
        .send()
        .await
        .unwrap()
}

fn disposition(r: &reqwest::Response) -> String {
    r.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_string()
}

fn zip_entries(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut z = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    (0..z.len())
        .map(|i| {
            let mut f = z.by_index(i).unwrap();
            let mut data = Vec::new();
            f.read_to_end(&mut data).unwrap();
            (f.name().to_string(), data)
        })
        .collect()
}

#[tokio::test]
async fn markdown_is_a_readable_transcript() {
    let (state, gw) = gw().await;
    let id = rich_thread(&state, "Bilder und Notizen").await;
    let r = export(&gw, &format!("/chat/api/threads/{id}/export?format=md")).await;
    assert_eq!(r.status(), 200);
    assert!(r.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/markdown"));
    let d = disposition(&r);
    assert!(
        d.starts_with("attachment; filename=\"lmgw-chat-") && d.ends_with(".md"),
        "{d}"
    );
    let md = r.text().await.unwrap();
    for needle in [
        "# Bilder und Notizen",
        "JSON export has them",
        "**Model:** m-alias",
        "temperature 0.3",
        "top_k 40",
        "seed 42",
        "reasoning_effort high",
        "**Knowledge bases:** #7 (tool",
        "budget 1234 tokens",
        "## You",
        "## Assistant · m-earlier (answered by cloud-fb)",
        "What is in the file?",
        "It is a picture.",
        "<details><summary>Thinking</summary>",
        "let me think it over",
        "<details><summary>Tool calls</summary>",
        "lmgw__status",
        "<details><summary>Sources (1)</summary>",
        "- [1] handbook.pdf · page 12 · Manuals",
        "- Attachment: pic.png (image, 10 B)",
    ] {
        assert!(md.contains(needle), "missing {needle:?} in:\n{md}");
    }
    assert!(md.contains("```json"), "tool calls in a json fence");
    // The system prompt contains a backtick run, so its fence is longer.
    assert!(md.contains("````\nBe ``` careful.\n````"), "{md}");
    // Default parameters are left out.
    assert!(!md.contains("presence_penalty 0\n"));
    assert!(
        !md.contains("Assistant · m-alias"),
        "not the thread's model now"
    );

    // A reply saved before the model was recorded names none.
    store::append_chat_message(&state.db, id, "assistant", "older", "", None, None, None)
        .await
        .unwrap();
    let md = export(&gw, &format!("/chat/api/threads/{id}/export?format=md"))
        .await
        .text()
        .await
        .unwrap();
    assert!(md.contains("## Assistant\n"), "{md}");
}

#[tokio::test]
async fn json_is_lossless() {
    let (state, gw) = gw().await;
    let id = rich_thread(&state, "T").await;
    let folder = store::create_chat_folder(&state.db, "Work", &ThreadDefaults::default(), None)
        .await
        .unwrap();
    store::set_chat_thread_folder(&state.db, id, Some(folder), None)
        .await
        .unwrap();
    let r = export(&gw, &format!("/chat/api/threads/{id}/export?format=json")).await;
    assert_eq!(r.status(), 200);
    assert!(disposition(&r).contains(".json"), "{}", disposition(&r));
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["format"], "lmgw.chat.v1");
    assert!(v["exported_at"].as_str().unwrap().contains('T'));
    assert_eq!(v["folder"], json!({"id": folder, "name": "Work"}));
    let t = &v["thread"];
    assert_eq!(t["model_alias"], "m-alias");
    assert_eq!(t["system_prompt"], "Be ``` careful.");
    assert_eq!(t["temperature"], 0.3);
    assert_eq!(t["max_tokens"], 777);
    assert_eq!(t["top_p"], 0.9);
    assert_eq!(t["top_k"], 40);
    assert_eq!(t["min_p"], 0.05);
    assert_eq!(t["seed"], 42);
    assert_eq!(t["stop"], json!(["END"]));
    assert_eq!(t["reasoning_enabled"], true);
    assert_eq!(t["reasoning_effort"], "high");
    assert_eq!(t["reasoning_budget"], 2048);
    assert_eq!(t["kb_ids"], json!([7]));
    assert_eq!(t["kb_mode"], "tool");
    assert_eq!(t["kb_budget_tokens"], 1234);
    assert_eq!(t["folder_id"], folder);
    assert!(t["created_at"].is_string() && t["updated_at"].is_string());

    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["kb_refs"], json!([7]));
    assert_eq!(msgs[0]["context"]["excerpts"][0]["file"], "handbook.pdf");
    assert_eq!(msgs[0]["context"]["excerpts"][0]["page"], 12);
    let a = &msgs[0]["attachments"][0];
    assert_eq!(a["name"], "pic.png");
    assert_eq!(a["kind"], "image");
    assert_eq!(a["mime"], "image/png");
    assert_eq!(a["size"], PNG.len());
    let data = base64::engine::general_purpose::STANDARD
        .decode(a["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(data, PNG, "the bytes are the upload's");
    assert!(a.get("extracted").is_some() && a.get("meta").is_some());
    assert_eq!(a["ord"], 0, "its position among the message's files");
    assert!(a["created_at"].as_str().unwrap().len() >= 19, "{a}");

    assert_eq!(msgs[1]["reasoning"], "let me think it over");
    assert_eq!(msgs[1]["prompt_tokens"], 11);
    assert_eq!(msgs[1]["completion_tokens"], 5);
    assert!(
        msgs[1]["ir_messages"].is_array(),
        "ir_messages is JSON, not a string: {}",
        msgs[1]["ir_messages"]
    );
    assert_eq!(msgs[1]["ir_messages"][0]["tool_calls"][0]["id"], "c1");
    assert!(msgs[1].get("ir_messages_unparsed").is_none());
    assert_eq!(msgs[1]["model"], "m-earlier");
    assert_eq!(msgs[1]["answered_by"], "cloud-fb");
    assert!(msgs[0]["model"].is_null(), "a user message has none");
}

/// A tool record that does not parse is exported as the string it is, and
/// says so — not dropped to `null` (review R1 item b).
#[tokio::test]
async fn an_unreadable_tool_record_is_kept_as_its_text() {
    let (state, gw) = gw().await;
    let id = store::create_chat_thread(&state.db, "m", "chat")
        .await
        .unwrap();
    store::append_chat_message(&state.db, id, "user", "q", "", None, None, None)
        .await
        .unwrap();
    store::append_chat_message(
        &state.db,
        id,
        "assistant",
        "a",
        "",
        None,
        None,
        Some("[{not json"),
    )
    .await
    .unwrap();
    let v: Value = export(&gw, &format!("/chat/api/threads/{id}/export?format=json"))
        .await
        .json()
        .await
        .unwrap();
    let m = &v["messages"][1];
    assert_eq!(m["ir_messages"], "[{not json");
    assert_eq!(m["ir_messages_unparsed"], true);
}

/// A thread that is mostly already-compressed attachment bytes goes into the
/// zip stored, not deflated a second time; a text thread is deflated (review
/// R1 item a).
#[tokio::test]
async fn compressed_payloads_are_stored_in_a_zip() {
    let (state, gw) = gw().await;
    let photo = store::create_chat_thread(&state.db, "m", "chat")
        .await
        .unwrap();
    let noise: Vec<u8> = (0..64 * 1024u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let aid = store::insert_chat_attachment_new(
        &state.db,
        photo,
        &NewAttachment::plain("image", "big.jpg", "image/jpeg", &noise),
    )
    .await
    .unwrap();
    store::append_user_message_with_kb_refs(&state.db, photo, "look", &[aid], &[])
        .await
        .unwrap();
    let text = store::create_chat_thread(&state.db, "m", "chat")
        .await
        .unwrap();
    store::append_chat_message(
        &state.db,
        text,
        "user",
        &"words and more words ".repeat(2000),
        "",
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let body = export(&gw, "/chat/api/export?format=json")
        .await
        .bytes()
        .await
        .unwrap();
    let mut z = zip::ZipArchive::new(Cursor::new(body.as_ref())).unwrap();
    let method = |z: &mut zip::ZipArchive<Cursor<&[u8]>>, id: i64| {
        let name = z
            .file_names()
            .find(|n| n.starts_with(&format!("lmgw-chat-{id}-")))
            .unwrap()
            .to_string();
        z.by_name(&name).unwrap().compression()
    };
    assert_eq!(method(&mut z, photo), zip::CompressionMethod::Stored);
    assert_eq!(method(&mut z, text), zip::CompressionMethod::Deflated);
    // And it still reads back whole.
    let entries = zip_entries(body.as_ref());
    let photo_json = entries
        .iter()
        .find(|(n, _)| n.starts_with(&format!("lmgw-chat-{photo}-")))
        .unwrap();
    let v: Value = serde_json::from_slice(&photo_json.1).unwrap();
    let data = base64::engine::general_purpose::STANDARD
        .decode(v["messages"][0]["attachments"][0]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(data, noise);
}

#[tokio::test]
async fn a_folder_zip_has_one_file_per_thread_and_honours_archived() {
    let (state, gw) = gw().await;
    let folder =
        store::create_chat_folder(&state.db, "Work stuff", &ThreadDefaults::default(), None)
            .await
            .unwrap();
    let a = rich_thread(&state, "Alpha").await;
    let b = rich_thread(&state, "Beta").await;
    let outside = rich_thread(&state, "Outside").await;
    for id in [a, b] {
        store::set_chat_thread_folder(&state.db, id, Some(folder), None)
            .await
            .unwrap();
    }
    store::archive_chat_thread(&state.db, b, None)
        .await
        .unwrap();

    let names = |bytes: &[u8]| -> Vec<String> {
        zip_entries(bytes)
            .into_iter()
            .map(|e| e.0)
            .filter(|n| n != "README.txt")
            .collect()
    };
    let r = export(&gw, &format!("/chat/api/folders/{folder}/export?format=md")).await;
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    assert_eq!(r.headers()["content-type"], "application/zip");
    let d = disposition(&r);
    assert!(
        d.contains("lmgw-folder-work-stuff-") && d.contains(".zip"),
        "{d}"
    );
    let len: usize = r.headers()["content-length"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let body = r.bytes().await.unwrap();
    assert_eq!(body.len(), len);
    let all = names(&body);
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(all.contains(&format!("lmgw-chat-{a}-alpha.md")), "{all:?}");
    assert!(all.contains(&format!("lmgw-chat-{b}-beta.md")));
    assert!(!all.iter().any(|n| n.contains(&outside.to_string())));

    let active = export(
        &gw,
        &format!("/chat/api/folders/{folder}/export?format=json&archived=0"),
    )
    .await
    .bytes()
    .await
    .unwrap();
    assert_eq!(names(&active), vec![format!("lmgw-chat-{a}-alpha.json")]);
    let archived = export(
        &gw,
        &format!("/chat/api/folders/{folder}/export?archived=1"),
    )
    .await
    .bytes()
    .await
    .unwrap();
    assert_eq!(names(&archived), vec![format!("lmgw-chat-{b}-beta.md")]);
    let bad = export(
        &gw,
        &format!("/chat/api/folders/{folder}/export?archived=x"),
    )
    .await;
    assert_eq!(bad.status(), 400);
    assert_eq!(bad.json::<Value>().await.unwrap()["code"], "bad_request");
}

#[tokio::test]
async fn the_all_zip_has_everything_stored_and_a_readme() {
    let mock = MockServer::start().await;
    mount_openai_reply(&mock, "ok", 1, 1).await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let folder = store::create_chat_folder(&state.db, "F", &ThreadDefaults::default(), None)
        .await
        .unwrap();
    let a = rich_thread(&state, "One").await;
    let b = rich_thread(&state, "Two").await;
    store::set_chat_thread_folder(&state.db, b, Some(folder), None)
        .await
        .unwrap();
    // A temporary thread is in memory and not part of the zip.
    let tmp = post(
        &gw,
        "/chat/api/threads",
        json!({"model_alias": "m", "temporary": true}),
    )
    .await
    .json::<Value>()
    .await
    .unwrap();
    assert!(tmp["id"].as_i64().unwrap() < 0);

    let r = export(&gw, "/chat/api/export?format=json").await;
    assert_eq!(r.status(), 200);
    assert!(
        disposition(&r).contains("lmgw-chats-"),
        "{}",
        disposition(&r)
    );
    let entries = zip_entries(&r.bytes().await.unwrap());
    let names: Vec<&str> = entries.iter().map(|e| e.0.as_str()).collect();
    assert!(names.contains(&"README.txt"), "{names:?}");
    assert!(names.contains(&format!("lmgw-chat-{a}-one.json").as_str()));
    assert!(names.contains(&format!("f/lmgw-chat-{b}-two.json").as_str()));
    assert_eq!(names.len(), 3, "{names:?}");
    let readme = String::from_utf8(entries[0].1.clone()).unwrap();
    assert!(readme.contains("Temporary chats") && readme.contains("2 thread(s)"));
    let one = entries.iter().find(|e| e.0.ends_with("one.json")).unwrap();
    let v: Value = serde_json::from_slice(&one.1).unwrap();
    assert_eq!(v["format"], "lmgw.chat.v1");
}

#[tokio::test]
async fn a_temporary_thread_exports_from_memory() {
    let mock = MockServer::start().await;
    mount_openai_reply(&mock, "a reply", 5, 2).await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let t: Value = post(
        &gw,
        "/chat/api/threads",
        json!({"model_alias": "m", "temporary": true}),
    )
    .await
    .json()
    .await
    .unwrap();
    let tid = t["id"].as_i64().unwrap();
    let att: Value = gw
        .client()
        .post(format!(
            "{gw}/chat/api/threads/{tid}/attachments?name=notes.txt"
        ))
        .body("some notes")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let aid = att["id"].as_i64().unwrap();
    let sent = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "read this", "attachments": [aid]}),
    )
    .await;
    sent.text().await.unwrap();

    let md = export(&gw, &format!("/chat/api/threads/{tid}/export?format=md")).await;
    assert_eq!(md.status(), 200);
    assert!(disposition(&md).contains(&format!("lmgw-chat-temp{}-", -tid)));
    let md = md.text().await.unwrap();
    assert!(md.contains("Temporary chat") && md.contains("read this") && md.contains("a reply"));
    assert!(md.contains("notes.txt"));

    let v: Value = export(&gw, &format!("/chat/api/threads/{tid}/export?format=json"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["thread"]["temporary"], true);
    let data = v["messages"][0]["attachments"][0]["data"].as_str().unwrap();
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap(),
        b"some notes"
    );
    // Still not in the DB.
    assert!(
        store::list_chat_threads(&state.db, store::ThreadListMode::All)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn download_headers_carry_a_non_ascii_title_and_unknowns_are_404() {
    let (state, gw) = gw().await;
    let id = rich_thread(&state, "Übersicht: Größe & Maße").await;
    let r = export(&gw, &format!("/chat/api/threads/{id}/export")).await;
    assert_eq!(r.status(), 200, "md is the default format");
    let d = disposition(&r);
    assert!(d.starts_with("attachment; filename=\""), "{d}");
    assert!(d.is_ascii(), "the header carries no raw UTF-8: {d}");
    assert!(
        d.contains("filename*=UTF-8''lmgw-chat-") && d.contains("%C3%BCbersicht-gr%C3%B6%C3%9Fe"),
        "{d}"
    );

    for route in [
        "/chat/api/threads/9999/export",
        "/chat/api/threads/-5/export",
        "/chat/api/folders/9999/export",
    ] {
        let r = export(&gw, route).await;
        assert_eq!(r.status(), 404, "{route}");
        assert_eq!(r.json::<Value>().await.unwrap()["code"], "not_found");
    }
    let r = export(&gw, &format!("/chat/api/threads/{id}/export?format=pdf")).await;
    assert_eq!(r.status(), 400);
}
