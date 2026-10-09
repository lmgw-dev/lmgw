//! A profile's "no speech style" (`""`, §1.1) through the self-admin tools
//! (the profiles review's fix 6): `speech_style_mode` says inherit, none or
//! own, as `voice_block_mode` does for the voice block, and a stored none
//! survives an edit of the other voice fields.

use lmgw_core::config::SelfAdmin;
use lmgw_core::mcp::selfadmin::{call_capped, Caller};
use serde_json::{json, Value};

use crate::realtime_chat_thread::{world, World};

async fn set(w: &World, args: Value) -> Value {
    call_capped(
        &w.state,
        "lmgw__profile_set",
        args.as_object().cloned(),
        Caller::OWNER,
    )
    .await
    .unwrap()
}

/// The tool's JSON answer, or a panic with its error text.
fn answer(r: &Value) -> Value {
    assert_ne!(r["isError"], json!(true), "{r}");
    let text = r["content"][0]["text"].as_str().expect("text content");
    serde_json::from_str(text).unwrap()
}

fn error_text(r: &Value) -> String {
    assert_eq!(r["isError"], json!(true), "{r}");
    r["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn speech_style_mode_writes_none_inherit_and_own() {
    let w = world(|s| s.self_admin = SelfAdmin::Full).await;
    let p = answer(
        &set(
            &w,
            json!({"action": "create", "name": "Flat", "speech_style_mode": "none"}),
        )
        .await,
    );
    let id = p["id"].as_i64().unwrap();
    assert_eq!(p["voice"]["speech_style"], json!(""), "{p}");

    // Another voice field changes; the stored none stays.
    let p = answer(&set(&w, json!({"action": "update", "id": id, "voice": "alba"})).await);
    assert_eq!(p["voice"]["speech_style"], json!(""), "{p}");
    assert_eq!(p["voice"]["voice"], json!("alba"), "{p}");

    let p = answer(
        &set(
            &w,
            json!({"action": "update", "id": id, "speech_style": "warm"}),
        )
        .await,
    );
    assert_eq!(
        p["voice"]["speech_style"],
        json!("warm"),
        "own by default: {p}"
    );
    let p = answer(
        &set(
            &w,
            json!({"action": "update", "id": id, "speech_style_mode": "inherit"}),
        )
        .await,
    );
    assert_eq!(p["voice"]["speech_style"], Value::Null, "{p}");
    let p = answer(
        &set(
            &w,
            json!({"action": "update", "id": id, "speech_style_mode": "none"}),
        )
        .await,
    );
    assert_eq!(p["voice"]["speech_style"], json!(""), "{p}");

    for (args, needle) in [
        (json!({"speech_style_mode": "own"}), "needs speech_style"),
        (
            json!({"speech_style_mode": "none", "speech_style": "x"}),
            "'own' only",
        ),
        (json!({"speech_style_mode": "loud"}), "not one of"),
        (
            json!({"speech_style_mode": "none", "clear": "speech_style"}),
            "not both",
        ),
    ] {
        let mut a = args.clone();
        a["action"] = json!("update");
        a["id"] = json!(id);
        let e = error_text(&set(&w, a).await);
        assert!(e.contains(needle), "{args}: {e}");
    }
}
