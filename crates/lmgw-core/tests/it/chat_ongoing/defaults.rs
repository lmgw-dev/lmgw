//! A defaults change on an ongoing folder reaches its current thread
//! (client-apps design L9, §3.4): the changed fields only, through the
//! thread settings route's checks, unless the save says
//! `apply_to_current: false`.

use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use super::{current_ok, ongoing_folder};
use crate::device_chat::{get, pair, post};
use crate::realtime_chat_thread::world;

async fn thread(w: &crate::realtime_chat_thread::World, id: i64) -> Value {
    let (s, v) = get(w, &w.gw.client(), &format!("/chat/api/threads/{id}")).await;
    assert_eq!(s, 200, "{v}");
    v["thread"].clone()
}

#[tokio::test]
async fn changed_defaults_reach_the_current_thread_and_the_opt_out_stops_them() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(
        &w,
        &owner,
        "Assistant",
        30,
        json!({ "temperature": 0.3, "top_p": 0.8, "voice": { "tts_alias": "speak", "read_aloud": true } }),
    )
    .await;
    let (id, _, _) = current_ok(&w, &owner, folder, false).await;
    // A change made on the thread by hand, to a field the defaults keep.
    let (s, _) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{id}/settings"),
        json!({ "top_p": 0.5 }),
    )
    .await;
    assert_eq!(s, 200);

    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults": {
            "model_alias": "other", "temperature": 0.3, "top_p": 0.8,
            "voice": { "tts_alias": "speak", "read_aloud": false, "language": "de" },
        } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["applied"],
        json!({ "thread_id": id, "fields": ["model_alias", "voice.language", "voice.read_aloud"] })
    );
    let t = thread(&w, id).await;
    assert_eq!(
        (&t["model_alias"], &t["temperature"], &t["top_p"]),
        (&json!("other"), &json!(0.3), &json!(0.5)),
        "only the changed fields; the hand-made top_p stays: {t}"
    );
    assert_eq!(
        t["voice"],
        json!({ "tts_alias": "speak", "read_aloud": false, "language": "de" })
    );

    // A field set back to unset takes what a new thread starts with: none.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults": { "model_alias": "other", "top_p": 0.8 } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["applied"]["fields"],
        json!([
            "temperature",
            "voice.language",
            "voice.read_aloud",
            "voice.tts_alias"
        ])
    );
    let t = thread(&w, id).await;
    assert_eq!(t["temperature"], json!(null));
    assert_eq!(t["voice"], json!({}));

    // The opt-out: the folder changes, the thread does not.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults": { "model_alias": "chatty" }, "apply_to_current": false }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["applied"], json!(null));
    assert_eq!(v["defaults"]["model_alias"], "chatty");
    assert_eq!(thread(&w, id).await["model_alias"], "other");
}

#[tokio::test]
async fn a_field_the_current_thread_refuses_fails_the_whole_patch() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (id, _, _) = current_ok(&w, &owner, folder, false).await;
    // Reasoning switched off on the thread by hand: an effort the folder's
    // defaults now name contradicts it there, though not in the folder.
    let (s, _) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{id}/settings"),
        json!({ "reasoning_enabled": false }),
    )
    .await;
    assert_eq!(s, 200);
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "name": "Renamed", "defaults": { "model_alias": "chatty", "reasoning_effort": "high" } }),
    )
    .await;
    assert_eq!((s, &v["code"]), (400, &json!("bad_request")), "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("reasoning 'off' contradicts"),
        "{v}"
    );
    let f = super::folder_of(&w, &owner, folder).await.unwrap();
    assert_eq!(
        (&f["name"], &f["defaults"]["reasoning_effort"]),
        (&json!("Assistant"), &json!(null)),
        "nothing written: {f}"
    );
    assert_eq!(thread(&w, id).await["reasoning_effort"], json!(null));

    // Without the thread, the same patch is the folder's alone.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults": { "model_alias": "chatty", "reasoning_effort": "high" }, "apply_to_current": false }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
}

#[tokio::test]
async fn attaching_the_self_admin_toolset_through_the_defaults_takes_the_thread_from_devices_at_once(
) {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let d = pair(&w, "phone", json!({})).await;
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (id, _, _) = current_ok(&w, &d.client, folder, false).await;
    let bearer = format!("Bearer {}", d.key);
    let mut ws = w
        .connect(
            &format!("chat_thread={id}"),
            &[("authorization", bearer.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));

    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults": { "model_alias": "chatty", "mcp_tools": [{ "server_label": "lmgw" }] } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["applied"]["fields"], json!(["mcp_tools"]));

    // The device's session on the thread closes as a revocation of it: the
    // live flip followed the store's write.
    let close = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(c))) => break c,
                Some(Ok(_)) => continue,
                other => panic!("expected the close, got {other:?}"),
            }
        }
    })
    .await
    .expect("the session closes")
    .expect("a close frame with a reason");
    assert_eq!(u16::from(close.code), 4004, "out of reach, not revoked");
    let (s, _) = get(&w, &d.client, &format!("/chat/api/threads/{id}")).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn every_default_reaches_the_current_thread_as_a_new_thread_would_take_it() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (id, _, _) = current_ok(&w, &owner, folder, false).await;
    // Every field a folder default has, set.
    let every = json!({
        "model_alias": "other", "system_prompt": "be brief", "temperature": 0.4,
        "max_tokens": 300, "top_p": 0.9, "top_k": 40, "min_p": 0.05,
        "repeat_penalty": 1.1, "presence_penalty": 0.2, "frequency_penalty": 0.3,
        "seed": 7, "stop": ["END"], "reasoning_enabled": true, "reasoning_effort": "low",
        "reasoning_budget": 512, "mcp_tools": null, "kb_ids": null, "kb_mode": "tool",
        "kb_budget_tokens": 900,
        "voice": { "tts_alias": "speak", "asr_alias": "hear", "language": "de", "read_aloud": true },
    });
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults": every }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    // A thread started from the same defaults now.
    let (s, v) = post(
        &w,
        &owner,
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "folder_id": folder }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let fresh = v["id"].as_i64().unwrap();
    let settings = |t: Value| {
        let mut t = t;
        for k in [
            "id",
            "title",
            "created_at",
            "updated_at",
            "voice_resolved",
            "continue",
            "purge_at",
            "archived_at",
            "pinned",
        ] {
            t.as_object_mut().unwrap().remove(k);
        }
        t
    };
    assert_eq!(
        settings(thread(&w, id).await),
        settings(thread(&w, fresh).await),
        "the current thread took every default"
    );
}
