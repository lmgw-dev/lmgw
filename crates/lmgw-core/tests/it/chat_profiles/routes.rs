//! `/chat/api/profiles*`'s CRUD rows over HTTP (personality-profiles
//! design §3.1, §3.3; WP4): the list with `default_profile_id`, create
//! (`{name, …}` and `{builtin}`), read, patch (absent unchanged, `null`
//! unset), delete with its answer, the refusals' status and code, and the
//! snapshot reloaded after each write; a thread's and a folder's
//! `profile_id`, a new thread's profile, the `chat_profile` setting, and an
//! export's `profile`.

use serde_json::{json, Value};

use crate::device_chat::{get, post};
use crate::realtime_chat_thread::{world, World};

async fn owner_get(w: &World, path: &str) -> (u16, Value) {
    get(w, &w.gw.client(), path).await
}

async fn owner_post(w: &World, path: &str, body: Value) -> (u16, Value) {
    post(w, &w.gw.client(), path, body).await
}

/// Create a profile as the owner: its JSON.
async fn create(w: &World, body: Value) -> Value {
    let (s, v) = owner_post(w, "/chat/api/profiles", body).await;
    assert_eq!(s, 200, "{v}");
    v
}

fn names(list: &Value) -> Vec<String> {
    list["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn crud_with_its_refusals_and_the_snapshot_after_each_write() {
    let w = world(|_| {}).await;

    // The seeded built-in, and no default.
    let (s, list) = owner_get(&w, "/chat/api/profiles").await;
    assert_eq!(s, 200, "{list}");
    assert_eq!(names(&list), ["Concise"]);
    assert_eq!(list["default_profile_id"], Value::Null);
    let concise = &list["profiles"][0];
    assert_eq!(concise["builtin"], "concise");
    assert_eq!(concise["reasoning"], "off");
    assert_eq!(concise["used_by"], json!({"threads": 0, "folders": []}));
    let concise_id = concise["id"].as_i64().unwrap();

    // Create: trimmed, unset fields empty; in the snapshot at once.
    let calm = create(
        &w,
        json!({
            "name": "  Calm ",
            "persona": "You are calm.",
            "examples": [{"user": "Hi", "reply": "Hello."}],
            "voice": {"tts_alias": "speak", "voice": "alba"},
        }),
    )
    .await;
    let id = calm["id"].as_i64().unwrap();
    assert_eq!(calm["name"], "Calm");
    assert_eq!(calm["length_rule"], "");
    assert_eq!(calm["voice_block"], Value::Null);
    assert_eq!(calm["voice"]["tts_alias"], "speak");
    assert_eq!(calm["follows_builtin"], json!([]));
    let snap = w.state.snapshot();
    assert_eq!(snap.chat_profile(id).map(|p| p.name.as_str()), Some("Calm"));

    // The name rules.
    for (body, status, code) in [
        (json!({"name": "CONCISE"}), 409, "profile_name_taken"),
        (json!({"name": " default "}), 400, "profile_name_reserved"),
        (json!({"name": ""}), 400, "bad_request"),
        (
            json!({"name": "X", "examples": [{"user": "a", "reply": " "}]}),
            400,
            "bad_request",
        ),
        // A text-to-speech alias names a text-to-speech model.
        (
            json!({"name": "X", "voice": {"tts_alias": "chatty"}}),
            400,
            "bad_request",
        ),
        (json!({"builtin": "concise"}), 409, "profile_builtin_exists"),
        (json!({"builtin": "pirate"}), 400, "unknown_builtin"),
    ] {
        let (s, v) = owner_post(&w, "/chat/api/profiles", body.clone()).await;
        assert_eq!((s, v["code"].as_str()), (status, Some(code)), "{body}: {v}");
    }
    let (s, v) = owner_post(
        &w,
        "/chat/api/profiles",
        json!({"name": "X", "persnoa": "y"}),
    )
    .await;
    assert!(
        s >= 400 && v["message"].as_str().unwrap().contains("persnoa"),
        "{v}"
    );
    assert_eq!(
        names(&owner_get(&w, "/chat/api/profiles").await.1),
        ["Calm", "Concise"]
    );

    // Read; a missing one is a 404.
    let (s, v) = owner_get(&w, &format!("/chat/api/profiles/{id}")).await;
    assert_eq!((s, &v["name"]), (200, &json!("Calm")), "{v}");
    let (s, v) = owner_get(&w, "/chat/api/profiles/9999").await;
    assert_eq!((s, v["code"].as_str()), (404, Some("not_found")), "{v}");

    // Patch: an absent field stays, null unsets, a taken name is refused.
    let (s, v) = owner_post(
        &w,
        &format!("/chat/api/profiles/{id}"),
        json!({"length_rule": "One sentence.", "persona": null, "reasoning": "on"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["persona"], "");
    assert_eq!(v["length_rule"], "One sentence.");
    assert_eq!(v["reasoning"], "on");
    assert_eq!(v["examples"], json!([{"user": "Hi", "reply": "Hello."}]));
    assert_eq!(v["voice"]["voice"], "alba", "the voice is unchanged");
    let snap = w.state.snapshot();
    assert_eq!(snap.chat_profile(id).unwrap().length_rule, "One sentence.");
    let (s, v) = owner_post(
        &w,
        &format!("/chat/api/profiles/{id}"),
        json!({"name": "concise"}),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (409, Some("profile_name_taken")),
        "{v}"
    );
    let (s, v) = owner_post(&w, "/chat/api/profiles/9999", json!({"name": "Y"})).await;
    assert_eq!((s, v["code"].as_str()), (404, Some("not_found")), "{v}");

    // A built-in written back with its own text follows it again.
    let (_, v) = owner_post(
        &w,
        &format!("/chat/api/profiles/{concise_id}"),
        json!({"length_rule": "Short."}),
    )
    .await;
    assert!(!v["follows_builtin"]
        .as_array()
        .unwrap()
        .contains(&json!("length_rule")));
    let builtin = concise["length_rule"].clone();
    let (_, v) = owner_post(
        &w,
        &format!("/chat/api/profiles/{concise_id}"),
        json!({"length_rule": builtin}),
    )
    .await;
    assert!(v["follows_builtin"]
        .as_array()
        .unwrap()
        .contains(&json!("length_rule")));

    // Delete, then the built-in again.
    let (s, v) = owner_post(
        &w,
        &format!("/chat/api/profiles/{concise_id}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["deleted"], concise_id);
    assert!(w.state.snapshot().chat_profile(concise_id).is_none());
    let (s, v) = owner_post(
        &w,
        &format!("/chat/api/profiles/{concise_id}/delete"),
        json!({}),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (404, Some("not_found")), "{v}");
    let again = create(&w, json!({"builtin": "concise"})).await;
    assert_eq!(again["name"], "Concise");
    assert_eq!(again["follows_builtin"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn threads_folders_and_the_setting_take_a_profile_and_a_delete_clears_them() {
    let w = world(|_| {}).await;
    let calm = create(&w, json!({"name": "Calm", "persona": "You are calm."})).await;
    let id = calm["id"].as_i64().unwrap();

    // A thread's settings: an id, an unknown one refused, null for none.
    let (_, t) = owner_post(&w, "/chat/api/threads", json!({"model_alias": "chatty"})).await;
    let tid = t["id"].as_i64().unwrap();
    assert_eq!(t["profile_id"], Value::Null, "no default: none");
    let settings = format!("/chat/api/threads/{tid}/settings");
    let (s, v) = owner_post(&w, &settings, json!({"profile_id": 9999})).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (400, Some("unknown_profile")),
        "{v}"
    );
    let (s, v) = owner_post(&w, &settings, json!({"profile_id": id})).await;
    assert_eq!(s, 200, "{v}");
    let (_, t) = owner_get(&w, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(t["thread"]["profile_id"], id, "{t}");
    let (_, list) = owner_get(&w, "/chat/api/threads").await;
    assert_eq!(list["threads"][0]["profile_id"], id, "{list}");
    // A settings patch that does not name it leaves it.
    owner_post(&w, &settings, json!({"temperature": 0.5})).await;
    let (_, t) = owner_get(&w, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(t["thread"]["profile_id"], id);

    // The export names it.
    let (s, export) = owner_get(&w, &format!("/chat/api/threads/{tid}/export?format=json")).await;
    assert_eq!(s, 200, "{export}");
    assert_eq!(export["profile"], json!({"id": id, "name": "Calm"}));

    // A folder's defaults: an unknown id refused, a known one taken by its
    // new threads.
    let (s, v) = owner_post(
        &w,
        "/chat/api/folders",
        json!({"name": "F", "defaults": {"profile_id": 9999}}),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (400, Some("unknown_profile")),
        "{v}"
    );
    let (s, f) = owner_post(
        &w,
        "/chat/api/folders",
        json!({"name": "F", "defaults": {"profile_id": id}}),
    )
    .await;
    assert_eq!(s, 200, "{f}");
    let fid = f["id"].as_i64().unwrap();
    let (s, v) = owner_post(
        &w,
        &format!("/chat/api/folders/{fid}"),
        json!({"defaults_patch": {"profile_id": 9999}}),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (400, Some("unknown_profile")),
        "{v}"
    );
    let (_, in_folder) = owner_post(
        &w,
        "/chat/api/threads",
        json!({"model_alias": "chatty", "folder_id": fid}),
    )
    .await;
    assert_eq!(in_folder["profile_id"], id, "{in_folder}");

    // Settings → Chat's profile for new threads: refused when unknown,
    // taken by a new thread (a temporary one too), not by an Admin Chat
    // thread.
    let (s, v) = owner_post(
        &w,
        "/api/op/settings_set_full",
        json!({"chat_profile": 9999}),
    )
    .await;
    assert_ne!(s, 200, "{v}");
    let (s, v) = owner_post(&w, "/api/op/settings_set_full", json!({"chat_profile": id})).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(w.state.snapshot().settings.chat_profile, Some(id));
    let (_, full) = owner_get(&w, "/api/settings-full").await;
    assert_eq!(full["chat_profile"], id);
    let (_, list) = owner_get(&w, "/chat/api/profiles").await;
    assert_eq!(list["default_profile_id"], id);
    let (_, fresh) = owner_post(&w, "/chat/api/threads", json!({"model_alias": "chatty"})).await;
    assert_eq!(fresh["profile_id"], id, "{fresh}");
    let (_, temp) = owner_post(
        &w,
        "/chat/api/threads",
        json!({"model_alias": "chatty", "temporary": true}),
    )
    .await;
    assert_eq!(temp["profile_id"], id, "{temp}");
    let (_, admin) = owner_post(
        &w,
        "/chat/api/threads",
        json!({"model_alias": "chatty", "kind": "admin"}),
    )
    .await;
    assert_eq!(admin["profile_id"], Value::Null, "{admin}");

    // The list counts what uses it.
    let (_, p) = owner_get(&w, &format!("/chat/api/profiles/{id}")).await;
    assert_eq!(p["used_by"]["threads"], 3, "{p}");
    assert_eq!(p["used_by"]["folders"], json!([{"id": fid, "name": "F"}]));

    // The delete clears every use in one go, and says so.
    let (s, d) = owner_post(&w, &format!("/chat/api/profiles/{id}/delete"), json!({})).await;
    assert_eq!(s, 200, "{d}");
    assert_eq!(
        d,
        json!({"deleted": id, "threads_cleared": 3, "folders_cleared": [{"id": fid, "name": "F"}],
               "default_cleared": true})
    );
    let (_, t) = owner_get(&w, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(t["thread"]["profile_id"], Value::Null);
    let (_, f) = owner_get(&w, "/chat/api/folders").await;
    let folder = f["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == fid)
        .unwrap();
    assert_eq!(folder["defaults"]["profile_id"], Value::Null, "{folder}");
    assert_eq!(w.state.snapshot().settings.chat_profile, None);
    let (_, list) = owner_get(&w, "/chat/api/profiles").await;
    assert_eq!(list["default_profile_id"], Value::Null);
    let (_, export) = owner_get(&w, &format!("/chat/api/threads/{tid}/export?format=json")).await;
    assert_eq!(export["profile"], Value::Null);
}
