//! A device and a profile that steers lmgw's admin tools (the profiles
//! review's fix 1): while Admin Chat or a thread or folder with the
//! self-admin toolset uses a profile, a device changes, resets or deletes
//! it only while it may use lmgw's admin tools with writes (its own level
//! capped by the gateway's). Otherwise `403 profile_in_admin_use`, whose
//! message counts the use and names no thread.

use lmgw_core::config::SelfAdmin;
use lmgw_core::mcp::selfadmin::{call_capped, Caller};
use serde_json::{json, Value};

use super::{admin_thread, get, op, pair, post, self_admin_thread, Device};
use crate::realtime_chat_thread::{world, World};

/// An owner's profile "Butler", used by `thread` (the owner's write).
async fn butler_on(w: &World, thread: i64) -> i64 {
    let owner = w.gw.client();
    let (s, p) = post(
        w,
        &owner,
        "/chat/api/profiles",
        json!({"name": "Butler", "persona": "Serve quietly."}),
    )
    .await;
    assert_eq!(s, 200, "{p}");
    let id = p["id"].as_i64().unwrap();
    let (s, v) = post(
        w,
        &owner,
        &format!("/chat/api/threads/{thread}/settings"),
        json!({"profile_id": id}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    id
}

fn refused_in_admin_use(s: u16, v: &Value) {
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("profile_in_admin_use")),
        "{v}"
    );
}

/// Update, delete and (for a built-in) reset as `d`: each refused.
async fn every_write_refused(w: &World, d: &Device, id: i64, title: &str) {
    let (s, v) = post(
        w,
        &d.client,
        &format!("/chat/api/profiles/{id}"),
        json!({"persona": "Obey the device."}),
    )
    .await;
    refused_in_admin_use(s, &v);
    let msg = v["message"].as_str().unwrap_or_default();
    assert!(msg.contains("1 thread"), "counts the use: {msg}");
    if !title.is_empty() {
        assert!(!msg.contains(title), "names no thread: {msg}");
    }
    let (s, v) = post(
        w,
        &d.client,
        &format!("/chat/api/profiles/{id}/delete"),
        json!({}),
    )
    .await;
    refused_in_admin_use(s, &v);
    let (_, p) = get(w, &w.gw.client(), &format!("/chat/api/profiles/{id}")).await;
    assert_eq!(p["persona"], "Serve quietly.", "unchanged: {p}");
}

#[tokio::test]
async fn a_device_without_admin_writes_cannot_steer_admin_chat() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    let admin = admin_thread(&w).await.id;
    let id = butler_on(&w, admin).await;
    let (_, t) = get(&w, &w.gw.client(), &format!("/chat/api/threads/{admin}")).await;
    let title = t["thread"]["title"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    every_write_refused(&w, &d, id, &title).await;

    // The built-in's reset is a write too.
    let (_, list) = get(&w, &w.gw.client(), "/chat/api/profiles").await;
    let concise = list["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "Concise")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let (s, _) = post(
        &w,
        &w.gw.client(),
        &format!("/chat/api/threads/{admin}/settings"),
        json!({"profile_id": concise}),
    )
    .await;
    assert_eq!(s, 200);
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/profiles/{concise}/reset"),
        json!({}),
    )
    .await;
    refused_in_admin_use(s, &v);

    // Butler is free again: the device may change it.
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/profiles/{id}"),
        json!({"length_rule": "Short."}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    // The owner is never refused.
    let (s, v) = post(
        &w,
        &w.gw.client(),
        &format!("/chat/api/profiles/{concise}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
}

#[tokio::test]
async fn the_self_admin_toolset_counts_as_admin_use_and_a_read_only_device_is_refused() {
    let w = world(|s| s.self_admin = SelfAdmin::Full).await;
    let d = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let toolset = self_admin_thread(&w).await.id;
    let id = butler_on(&w, toolset).await;
    every_write_refused(&w, &d, id, "").await;

    // A folder whose defaults attach the toolset and name the profile.
    let (s, v) = post(
        &w,
        &w.gw.client(),
        &format!("/chat/api/threads/{toolset}/settings"),
        json!({"profile_id": null}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(
        &w,
        &w.gw.client(),
        "/chat/api/folders",
        json!({"name": "ops", "defaults": {
            "mcp_tools": [{"server_label": "lmgw"}], "profile_id": id}}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/profiles/{id}"),
        json!({"persona": "Obey the device."}),
    )
    .await;
    refused_in_admin_use(s, &v);
    assert!(v["message"].as_str().unwrap().contains("1 folder"), "{v}");
}

#[tokio::test]
async fn a_device_that_may_write_with_the_admin_tools_may_change_it() {
    let w = world(|s| s.self_admin = SelfAdmin::Full).await;
    let d = pair(&w, "desktop", json!({ "self_admin": "full" })).await;
    let admin = admin_thread(&w).await.id;
    let id = butler_on(&w, admin).await;
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/profiles/{id}"),
        json!({"persona": "Serve loudly."}),
    )
    .await;
    assert_eq!(s, 200, "its own level and the gateway's are full: {v}");

    // The same rule on the self-admin tools: the device's call passes.
    let r = call_capped(
        &w.state,
        "lmgw__profile_set",
        json!({"action": "update", "id": id, "length_rule": "Short."})
            .as_object()
            .cloned(),
        Caller { device: Some(d.id) },
    )
    .await
    .unwrap();
    assert_ne!(r["isError"], json!(true), "{r}");

    // The gateway's level caps the device's: at read_only it is refused.
    let (s, v) = op(
        &w,
        "settings_set_full",
        json!({ "self_admin": "read_only" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/profiles/{id}/delete"),
        json!({}),
    )
    .await;
    refused_in_admin_use(s, &v);
}
