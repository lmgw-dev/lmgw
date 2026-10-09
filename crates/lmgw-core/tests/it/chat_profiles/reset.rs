//! `POST /chat/api/profiles/{id}/reset` (the editor's "Reset to built-in"):
//! a built-in profile's fields follow the built-in texts again — the texts
//! themselves, not empty ones — its voice is unset and its name kept, in
//! one transaction with its `profile.updated` feed event; one of the
//! owner's own is refused with `profile_not_builtin`. A device may reset.

use serde_json::{json, Value};

use crate::device_chat::{device_world, get, post};

/// The newest feed event's type and detail.
async fn last_event(w: &crate::realtime_chat_thread::World) -> (String, Value) {
    let (kind, detail): (String, Option<String>) =
        sqlx::query_as("SELECT type, detail FROM chat_feed ORDER BY seq DESC LIMIT 1")
            .fetch_one(&w.state.db)
            .await
            .unwrap();
    let detail = detail.and_then(|d| serde_json::from_str(&d).ok());
    (kind, detail.unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_built_in_follows_its_texts_again_and_its_voice_is_unset() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let (_, list) = get(&w, &owner, "/chat/api/profiles").await;
    let concise = list["profiles"][0].clone();
    assert_eq!(concise["builtin"], "concise", "{list}");
    let id = concise["id"].as_i64().unwrap();

    // Edited away from the built-in: other texts, a voice, a new name.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/profiles/{id}"),
        json!({"name": "Mine", "persona": "You are terse.", "examples": [],
               "reasoning": null, "voice": {"voice": "alba", "speech_style": "calm"}}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["follows_builtin"], json!(["length_rule", "voice_block"]));

    // A device resets it: the built-in's texts, not empty ones.
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/profiles/{id}/reset"),
        json!({}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["name"], "Mine", "the name stays");
    for field in ["persona", "length_rule", "examples", "reasoning"] {
        assert_eq!(v[field], concise[field], "{field}: {v}");
    }
    assert!(!v["persona"].as_str().unwrap().is_empty());
    assert_eq!(v["reasoning"], "off");
    assert_eq!(
        v["follows_builtin"],
        json!([
            "persona",
            "length_rule",
            "examples",
            "voice_block",
            "reasoning"
        ])
    );
    assert_eq!(
        v["voice"],
        json!({"tts_alias": null, "voice": null, "speech_style": null})
    );
    // In the snapshot, and in the feed as the device's update.
    let snap = w.state.snapshot();
    let p = snap.chat_profile(id).unwrap();
    assert_eq!(p.persona, concise["persona"].as_str().unwrap());
    assert!(p.voice.voice.is_none());
    let (kind, detail) = last_event(&w).await;
    assert_eq!(kind, "profile.updated");
    assert_eq!(detail["id"], id, "{detail}");
    assert_eq!(detail["name"], "Mine", "{detail}");
}

#[tokio::test]
async fn an_own_profile_or_none_is_refused_and_nothing_is_recorded() {
    let (w, _) = device_world().await;
    let owner = w.gw.client();
    let (s, own) = post(
        &w,
        &owner,
        "/chat/api/profiles",
        json!({"name": "Calm", "persona": "You are calm."}),
    )
    .await;
    assert_eq!(s, 200, "{own}");
    let own_id = own["id"].as_i64().unwrap();
    let before = last_event(&w).await;

    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/profiles/{own_id}/reset"),
        json!({}),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (400, Some("profile_not_builtin")),
        "{v}"
    );
    let (s, v) = post(&w, &owner, "/chat/api/profiles/9999/reset", json!({})).await;
    assert_eq!((s, v["code"].as_str()), (404, Some("not_found")), "{v}");

    assert_eq!(last_event(&w).await, before, "a refusal records no event");
    let (_, v) = get(&w, &owner, &format!("/chat/api/profiles/{own_id}")).await;
    assert_eq!(v["persona"], "You are calm.", "unchanged");
}
