//! L3 by every id: each `/chat/api` route that reaches a thread by its id, a
//! message id or an attachment id — enumerated from `CAPABILITY_TABLE`, so a
//! route added later joins the walk or fails it — answers a device 404 for
//! an Admin Chat thread, changes nothing there, and answers the same device
//! otherwise for a chat thread (the 404 is the rule, not a bad request).

use lmgw_core::principal::Cap;
use lmgw_core::server::CAPABILITY_TABLE;
use serde_json::{json, Value};

use super::{chat_thread, device_world, get, seed, Hidden};
use crate::realtime_chat_thread::World;

/// The rows the walk drives: every `Chat` row addressed by a thread or an
/// attachment id, deletes last (the chat thread's pass needs the thread).
fn addressed() -> Vec<(&'static str, &'static str)> {
    let mut rows: Vec<(&str, &str)> = CAPABILITY_TABLE
        .iter()
        .filter(|(_, p, cap)| {
            *cap == Cap::Chat
                && (p.starts_with("/chat/api/threads/{id}")
                    || p.starts_with("/chat/api/attachments/{id}"))
        })
        .map(|(m, p, _)| (*m, *p))
        .collect();
    rows.sort_by_key(|(_, p)| {
        (
            p.ends_with("/delete"),
            p.starts_with("/chat/api/threads/{id}/delete"),
        )
    });
    assert!(rows.len() >= 20, "the walk found only {} rows", rows.len());
    rows
}

/// What a route is sent: a body its extractor reads, so a 404 is the
/// handler's — every route is named, and a new one fails here until it is.
fn request(path: &str) -> (&'static str, Body) {
    let tail = path
        .trim_start_matches("/chat/api/threads/{id}")
        .trim_start_matches("/chat/api/attachments/{id}");
    let on_thread = path.starts_with("/chat/api/threads/");
    match (on_thread, tail) {
        (true, "") | (true, "/export") | (true, "/delete") | (true, "/speech/stop") => {
            ("", Body::Json(json!({})))
        }
        (true, "/settings") => ("", Body::Json(json!({ "temperature": 0.5 }))),
        (true, "/send") => ("", Body::Json(json!({ "content": "hello" }))),
        (true, "/voice/warm") => ("", Body::Json(json!({ "stages": ["chat"] }))),
        (true, "/transcribe") => ("", Body::Raw(Vec::new())),
        (true, "/pin") => ("", Body::Json(json!({ "pinned": true }))),
        (true, "/archive") => ("", Body::Json(json!({ "archived": true }))),
        (true, "/move") => ("", Body::Json(json!({ "folder_id": null }))),
        (true, "/persist") | (true, "/continue") => ("", Body::Json(json!({}))),
        (true, "/attachments") => ("?name=more.txt", Body::Raw(b"more".to_vec())),
        (true, "/messages/{mid}/delete")
        | (true, "/messages/{mid}/regenerate")
        | (true, "/messages/{mid}/speak") => ("", Body::Json(json!({}))),
        (true, "/messages/{mid}/edit") => ("", Body::Json(json!({ "content": "changed" }))),
        (false, "") | (false, "/delete") | (false, "/transcribe") | (false, "/text") => {
            ("", Body::Json(json!({})))
        }
        (false, "/mode") => ("", Body::Json(json!({ "mode": "text" }))),
        _ => panic!("the walk has no request for {path} — add one"),
    }
}

enum Body {
    Json(Value),
    Raw(Vec<u8>),
}

/// `(method, path)` with the ids of `(thread, message, attachment)` put in,
/// as `client`: the status and body text.
async fn drive(
    w: &World,
    client: &reqwest::Client,
    (method, path): (&str, &str),
    (thread, message, attachment): (i64, i64, i64),
) -> (u16, String) {
    let (query, body) = request(path);
    let id = if path.starts_with("/chat/api/attachments/") {
        attachment
    } else {
        thread
    };
    let url = format!(
        "{}{}{query}",
        w.gw,
        path.replace("{id}", &id.to_string())
            .replace("{mid}", &message.to_string())
    );
    let req = match method {
        "GET" => client.get(url),
        "POST" => client.post(url),
        other => panic!("a Chat row with method {other}"),
    };
    let req = match body {
        Body::Json(v) if method == "POST" => req.json(&v),
        Body::Json(_) => req,
        Body::Raw(b) => req.body(b),
    };
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    // Read whole: an SSE answer ends with its turn.
    let text = tokio::time::timeout(std::time::Duration::from_secs(30), resp.text())
        .await
        .unwrap_or_else(|_| panic!("{method} {path}: the answer never ended"))
        .unwrap();
    (status, text)
}

#[tokio::test]
async fn every_route_by_id_answers_a_device_404_for_an_admin_thread() {
    for hidden in Hidden::BOTH {
        let (w, d) = device_world().await;
        let admin = hidden.make(&w).await;
        walk_404(&w, &d, admin).await;
    }
}

/// Every addressed row as `d`, on the ids of `admin`: 404, and nothing
/// changed.
async fn walk_404(w: &World, d: &super::Device, admin: super::AdminThread) {
    let ids = (admin.id, admin.message, admin.attachment);
    let before = get(
        w,
        &w.gw.client(),
        &format!("/chat/api/threads/{}", admin.id),
    )
    .await;
    for row in addressed() {
        let (status, body) = drive(w, &d.client, row, ids).await;
        assert_eq!(status, 404, "{} {}: {body}", row.0, row.1);
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        assert_eq!(v["code"], "not_found", "{} {}: {body}", row.0, row.1);
    }
    // Nothing reached it: the thread is as it was, its message and draft
    // still there, its title, pin and archive state untouched.
    let after = get(
        w,
        &w.gw.client(),
        &format!("/chat/api/threads/{}", admin.id),
    )
    .await;
    assert_eq!(after.0, 200, "{after:?}");
    assert_eq!(before.1["messages"], after.1["messages"]);
    assert_eq!(before.1["draft_attachments"], after.1["draft_attachments"]);
    for field in ["title", "pinned", "archived_at", "folder_id", "temperature"] {
        assert_eq!(
            before.1["thread"][field], after.1["thread"][field],
            "{field}"
        );
    }
}

/// Review W4-2: a temporary thread the owner attached the self-admin
/// toolset to is out of a device's reach by every id, its attachments (by
/// their own, enumerable, negative ids) included.
#[tokio::test]
async fn every_route_by_id_answers_a_device_404_for_a_temporary_self_admin_thread() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let (s, v) = super::post(
        &w,
        &owner,
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "temporary": true }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let id = v["id"].as_i64().unwrap();
    assert!(id < 0);
    let resp = owner
        .post(format!(
            "{}/chat/api/threads/{id}/attachments?name=keys.txt",
            w.gw
        ))
        .body("sk-secret-zebra")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let attachment = resp.json::<Value>().await.unwrap()["id"].as_i64().unwrap();
    assert!(attachment < 0);
    // Before the toolset, the device reaches the attachment.
    let (s, _) = get(
        &w,
        &d.client,
        &format!("/chat/api/attachments/{attachment}"),
    )
    .await;
    assert_eq!(s, 200);
    let (s, v) = super::post(
        &w,
        &owner,
        &format!("/chat/api/threads/{id}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    walk_404(
        &w,
        &d,
        super::AdminThread {
            id,
            message: 0,
            attachment,
        },
    )
    .await;
}

#[tokio::test]
async fn the_same_routes_reach_a_chat_thread() {
    let (w, d) = device_world().await;
    for _ in 0..6 {
        w.chat
            .push(crate::support::realtime_fakes::Turn::text(&["fine."]));
    }
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let (message, attachment) = seed(&w, tid, "a plain question").await;
    for row in addressed() {
        let (status, body) = drive(&w, &d.client, row, (tid, message, attachment)).await;
        assert_ne!(status, 404, "{} {}: {body}", row.0, row.1);
        assert!(status < 500, "{} {}: {status} {body}", row.0, row.1);
    }
}

/// Ids in a body, a message id under another thread's id, and a message's
/// own (sent) attachment (review W3-11): none reaches a thread a device may
/// not see, for either hidden kind.
#[tokio::test]
async fn an_admin_thread_s_ids_reach_nothing_from_a_visible_thread() {
    for hidden in Hidden::BOTH {
        let (w, d) = device_world().await;
        let admin = hidden.make(&w).await;
        let mine = chat_thread(&w, &d.client, "chatty").await;
        let owner = w.gw.client();

        // A sent attachment of the admin thread: bound to a message there.
        let resp = owner
            .post(format!(
                "{}/chat/api/threads/{}/attachments?name=bound.txt",
                w.gw, admin.id
            ))
            .body("zebra bound")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bound = resp.json::<Value>().await.unwrap()["id"].as_i64().unwrap();
        let sent = lmgw_core::store::append_user_message_with_voice(
            &w.state.db,
            admin.id,
            "with a file",
            &[bound],
            &[],
            None,
        )
        .await
        .unwrap();
        assert!(matches!(
            sent,
            lmgw_core::store::SendMessageOutcome::Sent(_)
        ));
        for path in [
            format!("/chat/api/attachments/{bound}"),
            format!("/chat/api/attachments/{bound}/text"),
        ] {
            let (s, v) = get(&w, &d.client, &path).await;
            assert_eq!(s, 404, "{path}: {v}");
        }

        // The admin thread's draft in a send's body: refused as any id that
        // is not this thread's draft, and left where it was.
        let (s, v) = super::post(
            &w,
            &d.client,
            &format!("/chat/api/threads/{mine}/send"),
            json!({ "content": "look", "attachments": [admin.attachment] }),
        )
        .await;
        assert_eq!(s, 400, "{v}");
        assert!(!v.to_string().contains("zebra"), "{v}");

        // The admin thread's message under the device's own thread id.
        for tail in ["edit", "delete", "regenerate", "speak"] {
            let (s, v) = super::post(
                &w,
                &d.client,
                &format!("/chat/api/threads/{mine}/messages/{}/{tail}", admin.message),
                json!({ "content": "changed" }),
            )
            .await;
            assert_eq!(s, 404, "{tail}: {v}");
        }

        // Nothing moved there.
        let (_, after) = get(&w, &owner, &format!("/chat/api/threads/{}", admin.id)).await;
        let texts: Vec<&str> = after["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["content"].as_str().unwrap())
            .collect();
        assert!(
            texts.contains(&"the owner's provider key is sk-secret-zebra"),
            "{after}"
        );
        assert_eq!(
            after["draft_attachments"][0]["id"],
            json!(admin.attachment),
            "{after}"
        );
    }
}
