//! A fallback that cannot see, in the Chat (the owner's ruling, 2026-10-06;
//! `web::chat_turn::blind`): a turn with images goes to it — under the GPU
//! hold and at §4.7's outside-VRAM swap, plain and in the tool loop — its
//! images as placeholders and a PDF sent as page images as the PDF's text;
//! the reply says so in `images_note`, which is stored with it; a draft
//! says beforehand what it will become. The thread's own model decides
//! whether an image may go at all, a hold's fallback that sees lets one go
//! a blind model of the thread's would not have taken.

use super::blind_fallback::{held_with, marker_of, post, OMITTED, ONE_PLACEHOLDER};
use super::outside_vram_fallback::{cloud_alias_seeing, last_cloud_chat};
use super::*;

/// The fixture's PNG, as the Chat uploads it.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest-of-file";

/// A thread on the local model; `kind` "admin" gives it the tool loop.
async fn thread(f: &Fixture, kind: Option<&str>) -> i64 {
    let mut thread = json!({"model_alias": "chat-model"});
    if let Some(kind) = kind {
        let s = Settings {
            self_admin: lmgw_core::config::SelfAdmin::ReadOnly,
            ..f.state.snapshot().settings.clone()
        };
        store::save_settings(&f.state.db, &s).await.unwrap();
        f.state.reload_snapshot().await.unwrap();
        thread["kind"] = json!(kind);
    }
    post(f, "/chat/api/threads", thread)
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap()
}

/// Upload `bytes` as `name` onto thread `tid`: the attachment as the server
/// answered, blockers and hints included.
async fn attach(f: &Fixture, tid: i64, name: &str, bytes: Vec<u8>) -> Value {
    let r = f
        .gateway
        .client()
        .post(format!(
            "{}/chat/api/threads/{tid}/attachments?name={name}",
            f.gateway
        ))
        .body(bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{name}");
    r.json().await.unwrap()
}

/// Send `content` with attachment `att` on thread `tid`: the status, and the
/// `done` frame of a turn that ran (the refusal's body otherwise).
async fn send_turn(f: &Fixture, tid: i64, att: i64, content: &str) -> (u16, Value) {
    let r = post(
        f,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": content, "attachments": [att]}),
    )
    .await;
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    if status != 200 {
        return (status, serde_json::from_str(&text).unwrap_or(Value::Null));
    }
    let done = crate::chat_actions::sse_events(&text)
        .into_iter()
        .find(|(e, _)| e == "done")
        .map(|(_, d)| d)
        .unwrap_or_else(|| panic!("no done frame: {text}"));
    (status, done)
}

/// The thread as the page reads it, after a reload.
async fn reloaded(f: &Fixture, tid: i64) -> Value {
    f.gateway
        .client()
        .get(format!("{}/chat/api/threads/{tid}", f.gateway))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

const BLIND_NOTE: &str = "'cloud-blind' answered in place of 'chat-model' and cannot see \
                          images: the images went to it as placeholders";

/// The Chat under the hold, a fallback that cannot see: a turn with an image
/// is not refused — the fallback answers, its request carries the
/// placeholder, and `done` names who answered and says so in `images_note`,
/// which the reloaded thread still carries. The plain stream and the tool
/// loop alike.
#[tokio::test]
async fn the_chat_sends_to_a_blind_fallback_with_placeholders_and_says_so() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let cloud = held_with(&f, "cloud-blind").await;

    for (kind, said) in [(None, "plain turn"), (Some("admin"), "tool-loop turn")] {
        let tid = thread(&f, kind).await;
        let att = attach(&f, tid, "shot.png", PNG.to_vec()).await;
        let (status, done) = send_turn(&f, tid, att["id"].as_i64().unwrap(), said).await;
        assert_eq!(status, 200, "{done}");
        assert_eq!(done["answered_by"], "cloud-blind", "{done}");
        assert_eq!(done["images_note"], BLIND_NOTE, "{done}");
        assert!(done["message_id"].as_i64().unwrap_or(0) > 0, "{done}");
        let sent = last_cloud_chat(&cloud).await;
        assert!(sent.contains(said), "{sent}");
        assert!(sent.contains(OMITTED), "{sent}");
        assert!(!sent.contains("image_url"), "{sent}");
        // The turn's request row says what it lost.
        assert_eq!(
            marker_of(&f, "chat-model").await.as_deref(),
            Some(ONE_PLACEHOLDER)
        );

        // Stored with the reply (review I6): a reload shows it again.
        let t = reloaded(&f, tid).await;
        let reply = t["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(reply["role"], "assistant");
        assert_eq!(reply["answered_by"], "cloud-blind");
        assert_eq!(reply["images_note"], BLIND_NOTE, "{reply}");
        let user = &t["messages"][0];
        assert!(user.get("images_note").is_none(), "{user}");
    }
    assert!(f.runs().is_empty(), "{:?}", f.runs());
}

/// The Chat under the hold, a fallback that sees: the image goes, the turn
/// has no note, and the draft no hint.
#[tokio::test]
async fn the_chat_sends_images_to_a_fallback_that_sees() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let cloud = held_with(&f, "cloud-sees").await;
    let tid = thread(&f, None).await;
    let att = attach(&f, tid, "shot.png", PNG.to_vec()).await;
    assert!(att.get("hints").is_none(), "{att}");
    let (status, done) = send_turn(&f, tid, att["id"].as_i64().unwrap(), "look").await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["answered_by"], "cloud-sees", "{done}");
    assert!(done.get("images_note").is_none(), "{done}");
    let sent = last_cloud_chat(&cloud).await;
    assert!(sent.contains("data:image/png;base64,"), "{sent}");
    assert!(!sent.contains("omitted:"), "{sent}");
}

/// A draft says beforehand what it becomes (review I2): under the hold whose
/// fallback cannot see, an image's chip carries a hint worded as the voice
/// turn's hold hints are, and nothing blocks the send.
#[tokio::test]
async fn a_draft_says_its_image_goes_to_a_blind_fallback_as_a_placeholder() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _cloud = held_with(&f, "cloud-blind").await;
    let tid = thread(&f, None).await;
    let att = attach(&f, tid, "shot.png", PNG.to_vec()).await;
    assert_eq!(att["blockers"], json!([]), "{att}");
    assert_eq!(
        att["hints"],
        json!([
            "under the GPU hold this goes to cloud-blind, which cannot see images: it gets \
                a placeholder instead"
        ]),
        "{att}"
    );
    let t = reloaded(&f, tid).await;
    assert_eq!(t["draft_attachments"][0]["hints"], att["hints"], "{t}");
}

/// The local model's own capabilities, as the owner overrides them: it
/// sees, or not.
async fn chat_model_sees(f: &Fixture, vision: bool) {
    let row = f
        .state
        .snapshot()
        .local_models
        .iter()
        .find(|m| m.model_id == "chat-model")
        .cloned()
        .unwrap();
    store::update_local_model(
        &f.state.db,
        row.id,
        &NewLocalModel {
            model_id: row.model_id,
            gguf_path: row.gguf_path,
            params: row.params,
            args: row.args,
            idle_seconds: row.idle_seconds,
            enabled: row.enabled,
            public: row.public,
            image: row.image,
            extra_run_args: row.extra_run_args,
            warm_start: row.warm_start,
            hold_fallback_mode: row.hold_fallback_mode,
            hold_fallback: row.hold_fallback,
            capabilities_override: Some(json!({"capabilities": {
                "task": "chat", "endpoints": ["/v1/chat/completions"],
                "vision": vision, "source": "owner",
            }})),
            ladder: row.ladder,
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// Which model decides whether a Chat image may go at all (review I9):
/// under the hold, a fallback that sees lets the image of a thread whose own
/// model cannot see go to it; a fallback that cannot see does not decide it
/// — the thread's own blind model still refuses the image, by its name.
#[tokio::test]
async fn the_threads_own_model_decides_an_image_unless_the_hold_fallback_sees() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let cloud = held_with(&f, "cloud-sees").await;
    chat_model_sees(&f, false).await;

    let tid = thread(&f, None).await;
    let att = attach(&f, tid, "shot.png", PNG.to_vec()).await;
    assert_eq!(att["blockers"], json!([]), "{att}");
    let (status, done) = send_turn(&f, tid, att["id"].as_i64().unwrap(), "look").await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["answered_by"], "cloud-sees", "{done}");
    assert!(last_cloud_chat(&cloud)
        .await
        .contains("data:image/png;base64,"));

    set_global_fallback(&f, "cloud-blind").await;
    let tid = thread(&f, None).await;
    let att = attach(&f, tid, "shot.png", PNG.to_vec()).await;
    assert_eq!(
        att["blockers"],
        json!(["'chat-model' does not accept images — remove the image or switch models"]),
        "{att}"
    );
    let (status, body) = send_turn(&f, tid, att["id"].as_i64().unwrap(), "look").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "model_no_vision", "{body}");
}

/// At §4.7's outside-VRAM swap — not the hold — the Chat's turn goes to a
/// fallback that cannot see at once, with the placeholder and the note
/// (review I9).
#[tokio::test]
async fn the_chat_at_an_outside_vram_swap_says_its_blind_fallback_got_placeholders() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(3 * GIB);
    let cloud = cloud_chat(&f, "cloud-chat").await;
    cloud_alias_seeing(&f, "cloud-blind", false).await;
    set_global_fallback(&f, "cloud-blind").await;
    let tid = thread(&f, None).await;
    let att = attach(&f, tid, "shot.png", PNG.to_vec()).await;
    assert!(att.get("hints").is_none(), "no block is on: {att}");
    let (status, done) = send_turn(&f, tid, att["id"].as_i64().unwrap(), "look").await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["answered_by"], "cloud-blind", "{done}");
    assert_eq!(done["images_note"], BLIND_NOTE, "{done}");
    assert!(last_cloud_chat(&cloud).await.contains(OMITTED));
    assert!(f.runs().is_empty(), "{:?}", f.runs());
}

/// A PDF sent as page images reaches a fallback that cannot see as its text
/// (review I1), under the hold and at the outside-VRAM swap alike: the
/// `<file … kind="pdf">` block, no placeholder and no page image, and the
/// note says so; a draft hints it beforehand. A fallback that sees gets the
/// pages.
#[tokio::test]
async fn a_pdf_sent_as_pages_reaches_a_blind_fallback_as_its_text() {
    if !lmgw_core::extract::pdf::available().await {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let pdf = || crate::chat_attach_kinds::pdf_bytes(&[Some("Alpha"), Some("Beta")]);
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let cloud = held_with(&f, "cloud-blind").await;
    let note = "'cloud-blind' answered in place of 'chat-model' and cannot see images: PDF \
                pages went to it as the PDF's text";

    let paged = |f: &Fixture, tid: i64| {
        let gw = f.gateway.clone();
        async move {
            let att = attach_on(&gw, tid, "doc.pdf", pdf()).await;
            let id = att["id"].as_i64().unwrap();
            assert_eq!(
                crate::chat_attach_kinds::set_mode(&gw, id, "images")
                    .await
                    .status(),
                200
            );
            id
        }
    };

    // Under the hold.
    let tid = thread(&f, None).await;
    let id = paged(&f, tid).await;
    let t = reloaded(&f, tid).await;
    assert_eq!(
        t["draft_attachments"][0]["hints"],
        json!([
            "under the GPU hold this goes to cloud-blind, which cannot see images: it gets \
                the PDF's text instead of its page images"
        ]),
        "{t}"
    );
    let (status, done) = send_turn(&f, tid, id, "read it").await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["images_note"], note, "{done}");
    let sent = last_cloud_chat(&cloud).await;
    assert_eq!(
        marker_of(&f, "chat-model").await.as_deref(),
        Some("fallback 'cloud-blind' lacks vision: PDF pages sent as text")
    );
    assert!(sent.contains("kind=\\\"pdf\\\""), "{sent}");
    assert!(sent.contains("Alpha"), "{sent}");
    assert!(
        sent.contains("was sent as page images and the answering model does not accept images"),
        "{sent}"
    );
    assert!(!sent.contains("image_url"), "{sent}");
    assert!(!sent.contains("omitted:"), "{sent}");

    // A fallback that sees gets the pages.
    set_global_fallback(&f, "cloud-sees").await;
    let tid = thread(&f, None).await;
    let id = paged(&f, tid).await;
    let (status, done) = send_turn(&f, tid, id, "read it").await;
    assert_eq!(status, 200, "{done}");
    assert!(done.get("images_note").is_none(), "{done}");
    let sent = last_cloud_chat(&cloud).await;
    assert_eq!(
        sent.matches("\"type\":\"image_url\"").count(),
        2,
        "two pages"
    );

    // At the outside-VRAM swap: the send swaps the text in, as under the hold.
    lmgw_core::ops::hold_set(&f.state, false).await.unwrap();
    f.attribute(3 * GIB);
    set_global_fallback(&f, "cloud-blind").await;
    let tid = thread(&f, None).await;
    let id = paged(&f, tid).await;
    let (status, done) = send_turn(&f, tid, id, "read it").await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["images_note"], note, "{done}");
    let sent = last_cloud_chat(&cloud).await;
    assert!(sent.contains("Beta"), "{sent}");
    assert!(!sent.contains("image_url"), "{sent}");
}

/// [`attach`] on a gateway alone.
async fn attach_on(gw: &crate::common::Gw, tid: i64, name: &str, bytes: Vec<u8>) -> Value {
    let r = gw
        .client()
        .post(format!(
            "{gw}/chat/api/threads/{tid}/attachments?name={name}"
        ))
        .body(bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{name}");
    r.json().await.unwrap()
}

/// The thread's own model that cannot see gets a history image as a note
/// (chat-archive-pin-attachments §2): no fallback is involved, and the
/// request row says what the turn lost all the same (the owner's
/// requirement, 2026-10-06).
#[tokio::test]
async fn a_history_image_the_threads_own_model_cannot_see_marks_the_row() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let tid = thread(&f, None).await;
    let att = attach(&f, tid, "shot.png", PNG.to_vec()).await;
    let (status, done) = send_turn(&f, tid, att["id"].as_i64().unwrap(), "look").await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(marker_of(&f, "chat-model").await, None, "it saw the image");

    chat_model_sees(&f, false).await;
    let r = post(
        &f,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "and now?"}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let _ = r.text().await.unwrap();
    assert_eq!(
        marker_of(&f, "chat-model").await.as_deref(),
        Some("'chat-model' lacks vision: 1 image sent as a note")
    );
}
