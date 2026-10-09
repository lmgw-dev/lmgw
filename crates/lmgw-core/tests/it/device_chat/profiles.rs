//! A paired device and personality profiles (personality-profiles design
//! D13, D14, §3.5): it lists and edits profiles, a TTS alias outside its
//! key's scope is a 403, a folder `defaults_patch` applies `profile_id` to
//! the current thread (the desktop client's `personality_set` path), and an
//! admin thread stays out of reach.

use serde_json::{json, Value};

use super::{admin_thread, device_world, get, pair, post};
use crate::chat_ongoing::{current_ok, ongoing_folder};
use crate::realtime_chat_thread::world;

#[tokio::test]
async fn a_device_lists_creates_edits_and_deletes_profiles() {
    let (w, d) = device_world().await;
    let (s, list) = get(&w, &d.client, "/chat/api/profiles").await;
    assert_eq!(s, 200, "{list}");
    assert_eq!(list["profiles"][0]["name"], "Concise");

    let (s, p) = post(
        &w,
        &d.client,
        "/chat/api/profiles",
        json!({"name": "Pirate", "persona": "Talk like a pirate.",
               "voice": {"tts_alias": "speak"}}),
    )
    .await;
    assert_eq!(s, 200, "{p}");
    let id = p["id"].as_i64().unwrap();
    let (s, p) = post(
        &w,
        &d.client,
        &format!("/chat/api/profiles/{id}"),
        json!({"length_rule": "Short."}),
    )
    .await;
    assert_eq!((s, &p["length_rule"]), (200, &json!("Short.")), "{p}");
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/profiles/{id}/delete"),
        json!({}),
    )
    .await;
    assert_eq!((s, &v["deleted"]), (200, &json!(id)), "{v}");
}

#[tokio::test]
async fn a_tts_alias_outside_the_key_s_scope_is_refused() {
    let w = world(|_| {}).await;
    let d = pair(
        &w,
        "phone",
        json!({ "scope_mode": "allow", "scope_patterns": "chatty" }),
    )
    .await;
    let (s, v) = post(
        &w,
        &d.client,
        "/chat/api/profiles",
        json!({"name": "Loud", "voice": {"tts_alias": "speak"}}),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (403, Some("key_scope")), "{v}");
    let (_, list) = get(&w, &d.client, "/chat/api/profiles").await;
    assert_eq!(
        list["profiles"].as_array().unwrap().len(),
        1,
        "nothing created"
    );

    // The owner's profile carries it; the device may keep it, not write it
    // anew.
    let (_, p) = post(
        &w,
        &w.gw.client(),
        "/chat/api/profiles",
        json!({"name": "Loud", "voice": {"tts_alias": "speak"}}),
    )
    .await;
    let id = p["id"].as_i64().unwrap();
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/profiles/{id}"),
        json!({"voice": {"tts_alias": "speak", "voice": "alba"}}),
    )
    .await;
    assert_eq!(s, 200, "carried: {v}");
    let (s, v) = post(
        &w,
        &d.client,
        "/chat/api/profiles",
        json!({"name": "Plain"}),
    )
    .await;
    assert_eq!(s, 200, "no alias, nothing to scope: {v}");
}

/// The desktop client's path (D14, §3.5): the name resolved against the
/// list, then the folder's `defaults_patch {profile_id}`, which reaches the
/// current thread (`applied`); `null` goes back to none.
#[tokio::test]
async fn a_folder_s_profile_reaches_its_current_thread() {
    let (w, d) = device_world().await;
    let folder = ongoing_folder(&w, &d.client, "Assistant", 30, json!({})).await;
    let (current, _, _) = current_ok(&w, &d.client, folder, false).await;
    let (_, list) = get(&w, &d.client, "/chat/api/profiles").await;
    let list: lmgw_client::requests::ProfileList = serde_json::from_value(list).unwrap();
    let concise = lmgw_client::requests::profile_named(&list, "concise")
        .unwrap()
        .unwrap();

    let req = lmgw_client::requests::set_folder_profile(folder, Some(concise));
    let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
    let (s, v) = post(&w, &d.client, &req.path, body).await;
    assert_eq!(s, 200, "{v}");
    let patched =
        lmgw_client::requests::read_folder_patched(s, &v.to_string()).expect("reads as typed");
    assert_eq!(patched.folder.defaults.profile_id, Some(concise));
    let applied = patched.applied.expect("the current thread followed");
    assert_eq!(applied.thread_id, current);
    assert_eq!(applied.fields, ["profile_id"]);
    let (_, t) = get(&w, &d.client, &format!("/chat/api/threads/{current}")).await;
    assert_eq!(t["thread"]["profile_id"], concise, "{t}");

    // A new conversation in the folder starts with it.
    let (next, rolled, _) = current_ok(&w, &d.client, folder, true).await;
    if rolled {
        let (_, t) = get(&w, &d.client, &format!("/chat/api/threads/{next}")).await;
        assert_eq!(t["thread"]["profile_id"], concise, "{t}");
    }

    // "Default": none again, for the folder and its current thread.
    let req = lmgw_client::requests::set_folder_profile(folder, None);
    let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
    let (s, v) = post(&w, &d.client, &req.path, body).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["applied"]["fields"], json!(["profile_id"]), "{v}");
    let (_, t) = get(&w, &d.client, &format!("/chat/api/threads/{next}")).await;
    assert_eq!(t["thread"]["profile_id"], Value::Null, "{t}");

    // Without apply_to_current the current thread keeps its own.
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/folders/{folder}"),
        json!({"defaults_patch": {"profile_id": concise}, "apply_to_current": false}),
    )
    .await;
    assert_eq!((s, &v["applied"]), (200, &Value::Null), "{v}");
    let (_, t) = get(&w, &d.client, &format!("/chat/api/threads/{next}")).await;
    assert_eq!(t["thread"]["profile_id"], Value::Null, "{t}");
}

/// An Admin Chat thread stays out of a device's reach: it cannot set its
/// profile, and the profile's `used_by` does not count it for the device.
#[tokio::test]
async fn an_admin_thread_stays_out_of_reach() {
    let (w, d) = device_world().await;
    let admin = admin_thread(&w).await.id;
    let (_, list) = get(&w, &d.client, "/chat/api/profiles").await;
    let concise = list["profiles"][0]["id"].as_i64().unwrap();
    let (s, v) = post(
        &w,
        &w.gw.client(),
        &format!("/chat/api/threads/{admin}/settings"),
        json!({"profile_id": concise}),
    )
    .await;
    assert_eq!(s, 200, "the owner gives it one (D22): {v}");
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/threads/{admin}/settings"),
        json!({"profile_id": null}),
    )
    .await;
    assert_eq!(s, 404, "{v}");
    let used = |list: &Value| list["profiles"][0]["used_by"]["threads"].clone();
    let (_, mine) = get(&w, &d.client, "/chat/api/profiles").await;
    let (_, owners) = get(&w, &w.gw.client(), "/chat/api/profiles").await;
    assert_eq!((used(&mine), used(&owners)), (json!(0), json!(1)));

    // A device's delete of a profile Admin Chat uses is refused unless it
    // may write with lmgw's admin tools (review fix 1, `profiles_admin`);
    // the owner's clears it.
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/profiles/{concise}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("profile_in_admin_use")),
        "{v}"
    );
    let (s, v) = post(
        &w,
        &w.gw.client(),
        &format!("/chat/api/profiles/{concise}/delete"),
        json!({}),
    )
    .await;
    assert_eq!((s, &v["threads_cleared"]), (200, &json!(1)), "{v}");
    let (_, t) = get(&w, &w.gw.client(), &format!("/chat/api/threads/{admin}")).await;
    assert_eq!(t["thread"]["profile_id"], Value::Null, "{t}");
}

/// The self-admin tools check a device's TTS alias against its key's scope
/// as the routes do (review fix 4): a device allowed the admin tools with
/// writes still writes only aliases its key may use.
#[tokio::test]
async fn the_self_admin_tools_check_a_device_s_tts_alias_scope() {
    use lmgw_core::mcp::selfadmin::{call_capped, Caller};
    let w = world(|s| s.self_admin = lmgw_core::config::SelfAdmin::Full).await;
    let d = pair(
        &w,
        "desktop",
        json!({ "self_admin": "full", "scope_mode": "allow", "scope_patterns": "chatty" }),
    )
    .await;
    let call = |args: Value, device: Option<i64>| {
        let state = w.state.clone();
        async move {
            call_capped(
                &state,
                "lmgw__profile_set",
                args.as_object().cloned(),
                Caller { device },
            )
            .await
            .unwrap()
        }
    };
    let r = call(
        json!({"action": "create", "name": "Loud", "tts_alias": "speak"}),
        Some(d.id),
    )
    .await;
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(r.to_string().contains("scope"), "{r}");

    let r = call(json!({"action": "create", "name": "Plain"}), Some(d.id)).await;
    assert_ne!(r["isError"], json!(true), "{r}");
    let (_, list) = get(&w, &w.gw.client(), "/chat/api/profiles").await;
    let plain = list["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "Plain")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let r = call(
        json!({"action": "update", "id": plain, "tts_alias": "speak"}),
        Some(d.id),
    )
    .await;
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(r.to_string().contains("scope"), "{r}");

    // The owner's tool call is not scoped.
    let r = call(
        json!({"action": "update", "id": plain, "tts_alias": "speak"}),
        None,
    )
    .await;
    assert_ne!(r["isError"], json!(true), "{r}");
    // Carried, the device's later voice edit is not checked again.
    let r = call(
        json!({"action": "update", "id": plain, "voice": "alba"}),
        Some(d.id),
    )
    .await;
    assert_ne!(r["isError"], json!(true), "{r}");
}
