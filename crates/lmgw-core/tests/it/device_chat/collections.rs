//! L3 in the collections (client-apps design §1.3): a device's thread list,
//! its archived count, its folders' counts, its search and its exports carry
//! no Admin Chat thread, and its folder delete leaves them in place.

use std::io::Cursor;

use serde_json::{json, Value};

use super::{chat_thread, device_world, get, post, Hidden};
use crate::realtime_chat_thread::World;

/// Folder `F` holding the owner's `hidden` thread — Admin Chat, or the
/// self-admin toolset attached (review W3-1) — archived, and a chat thread
/// (active): `(folder, admin, chat)`.
async fn filed(w: &World, hidden: Hidden) -> (i64, i64, i64) {
    let owner = w.gw.client();
    let (_, f) = post(w, &owner, "/chat/api/folders", json!({ "name": "F" })).await;
    let folder = f["folder"]["id"].as_i64().or(f["id"].as_i64()).unwrap();
    let admin = hidden.make(w).await.id;
    let chat = chat_thread(w, &owner, "chatty").await;
    for t in [admin, chat] {
        let (s, v) = post(
            w,
            &owner,
            &format!("/chat/api/threads/{t}/move"),
            json!({ "folder_id": folder }),
        )
        .await;
        assert_eq!(s, 200, "{v}");
    }
    let (s, v) = post(
        w,
        &owner,
        &format!("/chat/api/threads/{admin}/archive"),
        json!({ "archived": true }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    (folder, admin, chat)
}

fn ids(list: &Value) -> Vec<i64> {
    list["threads"]
        .as_array()
        .unwrap_or_else(|| panic!("no threads in {list}"))
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect()
}

fn counts(list: &Value, folder: i64) -> (i64, i64) {
    let f = list["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["folder"]["id"] == json!(folder) || f["id"] == json!(folder))
        .unwrap_or_else(|| panic!("no folder {folder} in {list}"))
        .clone();
    (
        f["threads_active"].as_i64().unwrap(),
        f["threads_archived"].as_i64().unwrap(),
    )
}

#[tokio::test]
async fn the_list_its_counts_and_the_folders_leave_admin_chat_out() {
    for hidden in Hidden::BOTH {
        let (w, d) = device_world().await;
        let (folder, admin, chat) = filed(&w, hidden).await;
        let owner = w.gw.client();

        let (_, mine) = get(&w, &owner, "/chat/api/threads?archived=all").await;
        assert!(
            ids(&mine).contains(&admin) && ids(&mine).contains(&chat),
            "{mine}"
        );
        assert_eq!(mine["archived_count"], 1);
        assert_eq!(counts(&mine, folder), (1, 1));

        let (s, theirs) = get(&w, &d.client, "/chat/api/threads?archived=all").await;
        assert_eq!(s, 200, "{theirs}");
        assert!(!ids(&theirs).contains(&admin), "{theirs}");
        assert!(ids(&theirs).contains(&chat), "{theirs}");
        assert_eq!(theirs["archived_count"], 0, "{theirs}");
        assert_eq!(counts(&theirs, folder), (1, 0), "{theirs}");
        let (_, archived) = get(&w, &d.client, "/chat/api/threads?archived=1").await;
        assert_eq!(ids(&archived), Vec::<i64>::new(), "{archived}");

        let (s, folders) = get(&w, &d.client, "/chat/api/folders").await;
        assert_eq!(s, 200, "{folders}");
        assert_eq!(counts(&folders, folder), (1, 0), "{folders}");
    }
}

#[tokio::test]
async fn search_finds_nothing_of_an_admin_thread() {
    for hidden in Hidden::BOTH {
        let (w, d) = device_world().await;
        let (_, admin, _) = filed(&w, hidden).await;
        let q = "/chat/api/search?q=zebra&archived=all";
        let (_, mine) = get(&w, &w.gw.client(), q).await;
        let found = |v: &Value| {
            v["threads"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["thread_id"].as_i64().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(found(&mine), vec![admin], "{mine}");
        let (s, theirs) = get(&w, &d.client, q).await;
        assert_eq!(s, 200, "{theirs}");
        assert_eq!(found(&theirs), Vec::<i64>::new(), "{theirs}");
        assert_eq!(theirs["total_threads"], 0, "{theirs}");
    }
}

/// The JSON files of an export zip, by thread id.
async fn exported(w: &World, client: &reqwest::Client, path: &str) -> Vec<i64> {
    let resp = client.get(format!("{}{path}", w.gw)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let bytes = resp.bytes().await.unwrap();
    let mut z = zip::ArchiveZip::new(Cursor::new(bytes.as_ref()));
    z.thread_ids()
}

/// A tiny reader over the zip crate: the `thread.id` of every JSON file.
mod zip {
    use std::io::{Cursor, Read};

    pub struct ArchiveZip<'a>(::zip::ZipArchive<Cursor<&'a [u8]>>);

    impl<'a> ArchiveZip<'a> {
        pub fn new(c: Cursor<&'a [u8]>) -> Self {
            Self(::zip::ZipArchive::new(c).unwrap())
        }

        pub fn thread_ids(&mut self) -> Vec<i64> {
            let mut out = Vec::new();
            for i in 0..self.0.len() {
                let mut f = self.0.by_index(i).unwrap();
                if !f.name().ends_with(".json") {
                    continue;
                }
                let mut text = String::new();
                f.read_to_string(&mut text).unwrap();
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                out.push(v["thread"]["id"].as_i64().unwrap());
            }
            out.sort();
            out
        }
    }
}

#[tokio::test]
async fn exports_leave_admin_chat_out() {
    for hidden in Hidden::BOTH {
        let (w, d) = device_world().await;
        let (folder, admin, chat) = filed(&w, hidden).await;
        let all = "/chat/api/export?format=json&archived=all";
        let mut both = vec![admin, chat];
        both.sort();
        assert_eq!(exported(&w, &w.gw.client(), all).await, both);
        assert_eq!(exported(&w, &d.client, all).await, vec![chat]);
        let one = format!("/chat/api/folders/{folder}/export?format=json&archived=all");
        assert_eq!(exported(&w, &d.client, &one).await, vec![chat]);
    }
}

/// Review W6-1 (decided by the owner, 2026-10-07): a device's folder delete
/// takes its own threads as it asked — deleted, or moved out — and leaves
/// the folder in place holding the threads out of its reach, its retention
/// unchanged. The device's answer is a plain success, the folder is gone
/// for it (and for every device) from then on, and nothing it is told says
/// that other threads were there. The owner sees the folder with them.
#[tokio::test]
async fn a_device_s_folder_delete_leaves_the_folder_to_the_threads_it_cannot_see() {
    for hidden in Hidden::BOTH {
        for fate in ["delete", "keep"] {
            let (w, d) = device_world().await;
            let (folder, admin, chat) = filed(&w, hidden).await;
            let owner = w.gw.client();
            let (s, v) = post(
                &w,
                &d.client,
                &format!("/chat/api/folders/{folder}/delete"),
                json!({ "threads": fate }),
            )
            .await;
            assert_eq!((s, &v), (200, &json!({ "ok": true })), "{fate}: {v}");
            // The owner's thread stays in the folder, which the owner sees.
            let (s, kept) = get(&w, &owner, &format!("/chat/api/threads/{admin}")).await;
            assert_eq!(s, 200, "{fate}: the admin thread stays: {kept}");
            assert_eq!(kept["thread"]["folder_id"], json!(folder), "{fate}");
            let (_, folders) = get(&w, &owner, "/chat/api/folders").await;
            assert_eq!(counts(&folders, folder), (0, 1), "{fate}: {folders}");
            // The device's own thread went as it asked.
            let (s, mine) = get(&w, &owner, &format!("/chat/api/threads/{chat}")).await;
            match fate {
                "delete" => assert_eq!(s, 404, "{fate}: {mine}"),
                _ => assert_eq!(mine["thread"]["folder_id"], Value::Null, "{fate}: {mine}"),
            }
            // For the device the folder is gone: not listed, a 404 by id.
            let (_, theirs) = get(&w, &d.client, "/chat/api/folders").await;
            assert!(
                !theirs["folders"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|f| f["id"] == json!(folder)),
                "{fate}: {theirs}"
            );
            let (s, _) = post(
                &w,
                &d.client,
                &format!("/chat/api/folders/{folder}"),
                json!({ "name": "back" }),
            )
            .await;
            assert_eq!(s, 404, "{fate}");
        }
    }
}

/// The case W6-1 found: the owner gave a device-visible folder a year of
/// history, and it holds the owner's former current thread with the
/// self-admin toolset attached. A device's delete leaves that thread in the
/// folder, on the folder's 365 days, not the global 30; the device's feed
/// reads it as the folder's delete, after its own thread's.
#[tokio::test]
async fn a_device_s_folder_delete_keeps_the_owner_s_retention() {
    use crate::chat_feed::Feed;
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let (s, v) = post(
        &w,
        &owner,
        "/chat/api/folders",
        json!({ "name": "Desk", "defaults": { "model_alias": "chatty" },
                "ongoing": { "idle_minutes": 0 }, "purge_days": 365 }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let folder = v["id"].as_i64().unwrap();
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}/current"),
        json!({}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let owners = v["thread"]["id"].as_i64().unwrap();
    // The device's own conversation goes on in the folder ...
    let (s, v) = post(
        &w,
        &d.client,
        "/chat/api/threads",
        json!({ "folder_id": folder }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let devices = v["id"].as_i64().unwrap();
    // ... and the owner attaches the toolset to the former current thread.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{owners}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let mut feed = Feed::open(&w, &d.client, "", None).await;

    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/folders/{folder}/delete"),
        json!({ "threads": "delete" }),
    )
    .await;
    assert_eq!((s, &v), (200, &json!({ "ok": true })), "{v}");
    let stored = lmgw_core::store::get_chat_folder(&w.state.db, folder)
        .await
        .unwrap()
        .expect("the folder stays");
    assert_eq!(stored.purge_days, Some(365), "its retention unchanged");
    let t = lmgw_core::store::get_chat_thread(&w.state.db, owners)
        .await
        .unwrap()
        .expect("the owner's thread stays");
    assert_eq!(t.folder_id, Some(folder), "in the folder, on its 365 days");
    assert!(
        lmgw_core::store::get_chat_thread(&w.state.db, devices)
            .await
            .unwrap()
            .is_none(),
        "the device's thread went"
    );
    // The device's feed: its thread's delete, then the folder's.
    feed.until(10, |f| f.iter().any(|f| f.event == "folder.deleted"))
        .await;
    let stored: Vec<&str> = feed
        .frames
        .iter()
        .filter(|f| f.id.is_some())
        .map(|f| f.event.as_str())
        .collect();
    assert_eq!(
        stored,
        ["thread.deleted", "folder.deleted"],
        "{:#?}",
        feed.frames
    );
    assert_eq!(feed.named("thread.deleted")[0].data["thread_id"], devices);
    assert_eq!(feed.named("folder.deleted")[0].data["folder_id"], folder);
    assert!(
        feed.frames
            .iter()
            .all(|f| f.data["thread_id"] != owners && f.data["previous_thread_id"] != owners),
        "nothing of the owner's thread: {:#?}",
        feed.frames
    );
}

/// Review F-7: the owner sees the mark a device's delete set — the folder
/// is "hidden from devices" — and shows the folder to devices again. A
/// device's feed then receives it as created, then the threads in it it may
/// see; the threads it may not see stay out of its reach. The mark is the
/// owner's: a device cannot reach a hidden folder, and is refused for
/// writing it on one it sees.
#[tokio::test]
async fn the_owner_shows_a_folder_a_device_deleted_to_devices_again() {
    use crate::chat_feed::Feed;
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let (folder, admin, _) = filed(&w, Hidden::Admin).await;
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/folders/{folder}/delete"),
        json!({ "threads": "keep" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let hidden_flag = |v: &Value| {
        v["folders"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == folder)
            .map(|f| f["devices_hidden"].clone())
    };
    let (_, mine) = get(&w, &owner, "/chat/api/folders").await;
    assert_eq!(
        hidden_flag(&mine),
        Some(json!(true)),
        "the owner sees the mark: {mine}"
    );
    let (s, _) = post(
        &w,
        &d.client,
        &format!("/chat/api/folders/{folder}"),
        json!({ "devices_hidden": false }),
    )
    .await;
    assert_eq!(s, 404, "a hidden folder is no folder for a device");
    // A plain thread the owner files there now: in no folder for a device.
    let plain = chat_thread(&w, &owner, "chatty").await;
    let (s, _) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{plain}/move"),
        json!({ "folder_id": folder }),
    )
    .await;
    assert_eq!(s, 200);
    let (s, v) = get(&w, &d.client, &format!("/chat/api/threads/{plain}")).await;
    assert_eq!((s, &v["thread"]["folder_id"]), (200, &Value::Null), "{v}");
    let mut feed = Feed::open(&w, &d.client, "", None).await;

    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "devices_hidden": false }),
    )
    .await;
    assert_eq!((s, &v["devices_hidden"]), (200, &json!(false)), "{v}");
    feed.until(10, |f| {
        f.iter()
            .any(|f| f.event == "thread.updated" && crate::chat_feed::subject(f) == plain)
    })
    .await;
    let order: Vec<(String, i64)> = feed
        .frames
        .iter()
        .filter(|f| f.id.is_some() && f.event != ":")
        .map(|f| (f.event.clone(), crate::chat_feed::subject(f)))
        .collect();
    assert_eq!(
        order,
        [
            ("folder.created".to_string(), folder),
            ("thread.updated".to_string(), plain),
        ],
        "the folder, then its thread the device sees; never Admin Chat: {:#?}",
        feed.frames
    );
    assert!(feed.frames.iter().all(|f| f.data["thread_id"] != admin));
    let (_, theirs) = get(&w, &d.client, "/chat/api/folders").await;
    assert_eq!(hidden_flag(&theirs), Some(json!(false)), "{theirs}");
    let (_, v) = get(&w, &d.client, &format!("/chat/api/threads/{plain}")).await;
    assert_eq!(v["thread"]["folder_id"], json!(folder));
    let (s, _) = get(&w, &d.client, &format!("/chat/api/threads/{admin}")).await;
    assert_eq!(s, 404, "Admin Chat stays hidden");

    // Only showing is a request; a device writes the mark on no folder.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "devices_hidden": true }),
    )
    .await;
    assert_eq!(s, 400, "{v}");
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/folders/{folder}"),
        json!({ "devices_hidden": true }),
    )
    .await;
    assert_eq!((s, &v["code"]), (403, &json!("forbidden")), "{v}");
}

/// The mark and a device allowed lmgw's admin tools (2026-10-07): its
/// delete takes the threads with the toolset as it asked — they are within
/// its reach — and leaves the folder only to Admin Chat.
#[tokio::test]
async fn an_allowed_device_s_folder_delete_leaves_the_folder_only_to_admin_chat() {
    let (w, _) = device_world().await;
    let desk = super::pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let owner = w.gw.client();
    let (tools_folder, tools, _) = filed(&w, Hidden::SelfAdmin).await;
    let (s, v) = post(
        &w,
        &desk.client,
        &format!("/chat/api/folders/{tools_folder}/delete"),
        json!({ "threads": "delete" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, _) = get(&w, &owner, &format!("/chat/api/threads/{tools}")).await;
    assert_eq!(s, 404, "the toolset's thread was the device's to delete");
    let (_, folders) = get(&w, &owner, "/chat/api/folders").await;
    assert!(
        !ids_of(&folders).contains(&tools_folder),
        "nothing it could not see was left: the folder went: {folders}"
    );

    let (admin_folder, admin, _) = filed(&w, Hidden::Admin).await;
    let (s, v) = post(
        &w,
        &desk.client,
        &format!("/chat/api/folders/{admin_folder}/delete"),
        json!({ "threads": "delete" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = get(&w, &owner, &format!("/chat/api/threads/{admin}")).await;
    assert_eq!(
        (s, &v["thread"]["folder_id"]),
        (200, &json!(admin_folder)),
        "{v}"
    );
    let (_, folders) = get(&w, &owner, "/chat/api/folders").await;
    let kept = folders["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == admin_folder)
        .cloned()
        .expect("kept for Admin Chat");
    assert_eq!(kept["devices_hidden"], json!(true));
}

fn ids_of(folders: &Value) -> Vec<i64> {
    folders["folders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_i64().unwrap())
        .collect()
}
