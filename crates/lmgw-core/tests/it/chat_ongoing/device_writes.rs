//! What a device may write to an ongoing folder and its threads (review
//! W5): a folder's own retention is the owner's (W5-1); a device's
//! defaults change reaches the current thread only within its scopes, and
//! never a current thread out of its reach (W5-6); a stale settings write
//! cannot undo the owner's attach of the self-admin toolset (W5-2).

use serde_json::json;

use super::{current_ok, folder_of, ongoing_folder};
use crate::device_chat::{device_world, post};

#[tokio::test]
async fn a_folder_s_own_retention_is_the_owner_s_to_set() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    // Creating with retention: refused, naming the fields.
    let (s, v) = post(
        &w,
        &d.client,
        "/chat/api/folders",
        json!({ "name": "Mine", "archive_days": 1, "purge_days": 1 }),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");
    let msg = v["message"].as_str().unwrap();
    assert!(msg.contains("archive_days, purge_days"), "{msg}");
    // An ongoing folder of its own it may make, and mark or unmark.
    let folder = ongoing_folder(&w, &d.client, "Assistant", 30, json!({})).await;
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/folders/{folder}"),
        json!({ "ongoing": { "idle_minutes": 5 } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    // The owner sets the folder's retention; the device cannot change it,
    // nor send it back to the global setting.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "purge_days": 365 }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    for patch in [
        json!({ "purge_days": 1 }),
        json!({ "purge_days": null }),
        json!({ "archive_days": 0 }),
    ] {
        let (s, v) = post(
            &w,
            &d.client,
            &format!("/chat/api/folders/{folder}"),
            patch.clone(),
        )
        .await;
        assert_eq!(
            (s, v["code"].as_str()),
            (403, Some("forbidden")),
            "{patch}: {v}"
        );
    }
    // The value it already has is no write: a client sending the folder
    // back whole is not refused for it.
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/folders/{folder}"),
        json!({ "name": "Assistant", "purge_days": 365, "archive_days": null }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let f = folder_of(&w, &owner, folder).await.unwrap();
    assert_eq!(
        (f["purge_days"].clone(), f["archive_days"].clone()),
        (json!(365), json!(null))
    );
    // The current thread is unaffected by any of it.
    current_ok(&w, &d.client, folder, false).await;
}

/// The thread `id` as stored.
async fn stored(w: &crate::realtime_chat_thread::World, id: i64) -> lmgw_core::store::ChatThread {
    lmgw_core::store::get_chat_thread(&w.state.db, id)
        .await
        .unwrap()
        .unwrap()
}

fn carries_lmgw(t: &lmgw_core::store::ChatThread) -> bool {
    t.mcp_tools.iter().any(|m| m.server_label == "lmgw")
}

/// The race W5-2 found, played deterministically: a device's settings read
/// before the owner attaches the self-admin toolset, its whole-row write
/// after. The route reads under the thread's lock now; the store refuses
/// the stale write whatever was read.
#[tokio::test]
async fn a_device_s_stale_settings_write_cannot_undo_the_attach() {
    use lmgw_core::store::{self, AdminThreads, SeedWrite};
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let tid = crate::device_chat::chat_thread(&w, &d.client, "chatty").await;
    // The device's read: the thread is plain.
    let mut stale = stored(&w, tid).await;
    stale.temperature = Some(0.7);
    // The owner attaches the toolset.
    let path = format!("/chat/api/threads/{tid}/settings");
    let (s, v) = post(
        &w,
        &owner,
        &path,
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    // The device's write lands: nothing is written, the attach stays.
    let written = store::write_settings_of(
        &w.state.db,
        &stale,
        SeedWrite::Keep,
        Some("device 'phone'"),
        AdminThreads::Hidden,
    )
    .await
    .unwrap();
    assert!(written.is_none(), "the stale write is refused");
    let now = stored(&w, tid).await;
    assert!(carries_lmgw(&now));
    assert_eq!(now.temperature, None);
    // Through the route it is the 404 of a thread out of reach.
    let (s, v) = post(&w, &d.client, &path, json!({ "temperature": 0.7 })).await;
    assert_eq!(s, 404, "{v}");
    assert!(carries_lmgw(&stored(&w, tid).await));
    // The owner's own write of the same copy is no device's: it lands.
    let mut mine = stored(&w, tid).await;
    mine.temperature = Some(0.2);
    let written = store::write_settings_of(
        &w.state.db,
        &mine,
        SeedWrite::Keep,
        None,
        AdminThreads::Shown,
    )
    .await
    .unwrap();
    assert!(written.is_some());
}

/// The same for a folder patch's write onto its current thread (L9): a
/// device's patch never writes a current thread out of its reach, nor one
/// that is no longer the folder's current thread (W5-12).
#[tokio::test]
async fn a_folder_patch_writes_only_the_current_thread_within_reach() {
    use lmgw_core::store::{self, AdminThreads, ChatFolderPatch, CurrentSettings, SeedWrite};
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (tid, _, _) = current_ok(&w, &d.client, folder, false).await;
    let stale = stored(&w, tid).await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let patch = ChatFolderPatch {
        sort: Some(3),
        ..Default::default()
    };
    let current = CurrentSettings {
        thread: &stale,
        seed: SeedWrite::Keep,
        admin: AdminThreads::Hidden,
    };
    let out = store::update_chat_folder(
        &w.state.db,
        folder,
        &patch,
        Some(current),
        Some("device 'phone'"),
    )
    .await
    .unwrap();
    assert!(out.found);
    assert!(out.current.is_none(), "the hidden thread is not written");
    assert!(carries_lmgw(&stored(&w, tid).await));

    // A thread moved out of the folder is its current thread no more: the
    // owner's patch does not write it either.
    let other = ongoing_folder(&w, &owner, "Other", 30, json!({})).await;
    let (moved, _, _) = current_ok(&w, &owner, other, false).await;
    let copy = stored(&w, moved).await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{moved}/move"),
        json!({ "folder_id": null }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let current = CurrentSettings {
        thread: &copy,
        seed: SeedWrite::Keep,
        admin: AdminThreads::Shown,
    };
    let out = store::update_chat_folder(&w.state.db, other, &patch, Some(current), None)
        .await
        .unwrap();
    assert!(out.current.is_none(), "no longer the current thread");
}

/// A device's defaults change reaches the current thread only within its
/// scope, and a refusal there fails the whole patch; a current thread out
/// of its reach is left alone (W5-6).
#[tokio::test]
async fn a_device_s_defaults_reach_the_current_thread_within_its_scope() {
    let w = crate::realtime_chat_thread::world(|_| {}).await;
    let owner = w.gw.client();
    let d = crate::device_chat::pair(
        &w,
        "phone",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "docs__*" }),
    )
    .await;
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (tid, _, _) = current_ok(&w, &d.client, folder, false).await;
    // The owner adds a label out of the device's scope to the defaults,
    // not to the current thread.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults": { "model_alias": "chatty", "mcp_tools": [{ "server_label": "kb" }] },
                "apply_to_current": false }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    // The device adds one in its scope: the folder's list passes (kb is
    // the folder's already), the current thread's does not (kb is new to
    // it) — the whole patch fails.
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults": { "model_alias": "chatty",
                "mcp_tools": [{ "server_label": "kb" }, { "server_label": "docs" }] } }),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("tool_label_out_of_scope")),
        "{v}"
    );
    let f = folder_of(&w, &owner, folder).await.unwrap();
    assert_eq!(
        f["defaults"]["mcp_tools"],
        json!([{ "server_label": "kb", "allowed_tools": null }])
    );
    assert!(stored(&w, tid).await.mcp_tools.is_empty());

    // The owner attaches the toolset to the current thread: a device's
    // defaults change no longer touches it.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let prompt = stored(&w, tid).await.system_prompt;
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults": { "model_alias": "chatty", "system_prompt": "Be brief.",
                "mcp_tools": [{ "server_label": "kb" }] } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["applied"], json!(null));
    assert_eq!(stored(&w, tid).await.system_prompt, prompt, "left alone");
}

/// Reviews W6-6, W6-13 and W6-12: the race played through the routes. A
/// device's settings write, move, pin, archive and delete each check the
/// thread's reach, then wait on the thread's lock — held here where the
/// owner's attach would hold it — and the attach lands meanwhile. Each
/// re-checks under the lock: the 404 of a thread out of reach, and nothing
/// written. Without the hold, the same request goes through.
#[tokio::test]
async fn a_device_s_write_waiting_on_an_attach_rechecks_its_reach() {
    use std::time::Duration;

    use lmgw_core::store::{self, SeedWrite};
    let (w, d) = device_world().await;
    let (_, v) = post(
        &w,
        &d.client,
        "/chat/api/folders",
        json!({ "name": "Elsewhere" }),
    )
    .await;
    let elsewhere = v["id"].as_i64().unwrap();
    let writes: [(&str, serde_json::Value); 5] = [
        ("settings", json!({ "system_prompt": "from the device" })),
        ("move", json!({ "folder_id": elsewhere })),
        ("pin", json!({ "pinned": true })),
        ("archive", json!({ "archived": true })),
        ("delete", json!({})),
    ];
    for (route, body) in writes {
        let tid = crate::device_chat::chat_thread(&w, &d.client, "chatty").await;
        let held = w.state.hold_chat_thread_for_tests(tid).await;
        let path = format!("/chat/api/threads/{tid}/{route}");
        let (client, gw, b) = (d.client.clone(), w.gw.to_string(), body.clone());
        let request = tokio::spawn(async move {
            let r = client
                .post(format!("{gw}{path}"))
                .json(&b)
                .send()
                .await
                .unwrap();
            let status = r.status().as_u16();
            (
                status,
                r.json::<serde_json::Value>().await.unwrap_or_default(),
            )
        });
        // The request has read the thread and waits on its lock.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!request.is_finished(), "{route}: it waits on the lock");
        // The owner's attach, landing while the lock is held.
        let mut t = stored(&w, tid).await;
        t.mcp_tools = vec![store::ThreadMcp {
            server_label: "lmgw".into(),
            allowed_tools: None,
            require_approval: None,
        }];
        store::update_chat_thread_settings(&w.state.db, &t, SeedWrite::Keep, Some("the dashboard"))
            .await
            .unwrap();
        drop(held);
        let (s, v) = request.await.unwrap();
        assert_eq!((s, &v["code"]), (404, &json!("not_found")), "{route}: {v}");
        let now = stored(&w, tid).await;
        assert!(carries_lmgw(&now), "{route}: the attach stands");
        assert_ne!(now.system_prompt, "from the device", "{route}");
        assert_eq!(now.folder_id, None, "{route}: not moved");
        assert!(!now.pinned && now.archived_at.is_none(), "{route}");

        // A plain thread, no hold: the device's write goes through.
        let plain = crate::device_chat::chat_thread(&w, &d.client, "chatty").await;
        let (s, v) = post(
            &w,
            &d.client,
            &format!("/chat/api/threads/{plain}/{route}"),
            body,
        )
        .await;
        assert_eq!(s, 200, "{route}: {v}");
    }
}

/// Review W6-12 (W5-12): a thread a device creates by hand in a folder
/// takes the folder's model, or else its own, which must then be within
/// its alias scope — decided under the folder's lock from the folder as it
/// is then. The owner takes the model out of the folder's defaults while
/// the device's create waits on the lock: the create is refused, as the
/// folder now says.
#[tokio::test]
async fn a_thread_made_by_hand_is_checked_against_the_folder_as_it_is() {
    use std::time::Duration;

    let w = crate::realtime_chat_thread::world(|_| {}).await;
    let d = crate::device_chat::pair(
        &w,
        "phone",
        json!({ "scope_mode": "allow", "scope_patterns": "chatty" }),
    )
    .await;
    let owner = w.gw.client();
    let (s, v) = post(
        &w,
        &owner,
        "/chat/api/folders",
        json!({ "name": "Shared", "defaults": { "model_alias": "chatty" } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let folder = v["id"].as_i64().unwrap();
    // The folder names the model: the device's own is not asked about.
    let body = json!({ "folder_id": folder, "model_alias": "other" });
    let (s, v) = post(&w, &d.client, "/chat/api/threads", body.clone()).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["model_alias"], "chatty", "{v}");

    let held = w.state.hold_chat_folder_for_tests(folder).await;
    let (client, gw, b) = (d.client.clone(), w.gw.to_string(), body);
    let request = tokio::spawn(async move {
        let r = client
            .post(format!("{gw}/chat/api/threads"))
            .json(&b)
            .send()
            .await
            .unwrap();
        (
            r.status().as_u16(),
            r.json::<serde_json::Value>().await.unwrap_or_default(),
        )
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!request.is_finished(), "it waits on the folder's lock");
    sqlx::query("UPDATE chat_folders SET defaults = '{}' WHERE id = ?1")
        .bind(folder)
        .execute(&w.state.db)
        .await
        .unwrap();
    drop(held);
    let (s, v) = request.await.unwrap();
    assert_eq!((s, &v["code"]), (403, &json!("key_scope")), "{v}");
    let made =
        lmgw_core::store::list_chat_threads(&w.state.db, lmgw_core::store::ThreadListMode::Active)
            .await
            .unwrap();
    assert!(
        made.iter().all(|t| t.model_alias != "other"),
        "no thread on the alias out of scope"
    );
}

/// Review W6-10: the dashboard's folder form sends only the defaults
/// fields it changed (`defaults_patch`). A device changes the folder's
/// voice while the owner's form is open; the owner saves a temperature: the
/// device's voice stays in the defaults, and only the temperature reaches
/// the current thread (L9) — no stale value is written back or re-applied.
#[tokio::test]
async fn a_folder_save_changes_only_the_fields_it_names() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let folder = super::ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (tid, _, _) = current_ok(&w, &owner, folder, false).await;
    // The device changes the voice (its patch replaces the defaults whole).
    let mut defaults = folder_of(&w, &owner, folder).await.unwrap()["defaults"].clone();
    defaults["voice"] = json!({ "language": "fr" });
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults": defaults, "apply_to_current": false }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    // The owner's form, opened before that, saves the one field it changed.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults_patch": { "temperature": 0.4 } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["applied"]["fields"], json!(["temperature"]), "{v}");
    assert_eq!(v["defaults"]["voice"], json!({ "language": "fr" }), "{v}");
    assert_eq!(v["defaults"]["temperature"], json!(0.4), "{v}");
    let t = stored(&w, tid).await;
    assert_eq!(t.temperature, Some(0.4));
    assert_eq!(t.voice.language, None, "the device's voice was not applied");
    // A voice field by field, and both shapes at once refused.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults_patch": { "voice": { "language": null, "voice": "F2" } } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["defaults"]["voice"], json!({ "voice": "F2" }), "{v}");
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults": {}, "defaults_patch": {} }),
    )
    .await;
    assert_eq!((s, &v["code"]), (400, &json!("bad_request")), "{v}");
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults_patch": { "temprature": 1 } }),
    )
    .await;
    assert_eq!(s, 400, "an unknown field is refused: {v}");
}
