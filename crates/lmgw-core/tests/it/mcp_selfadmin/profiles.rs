//! Self-admin tools over `/mcp/admin` — personality profiles and the old embed spelling of aux.

use super::*;

// ---------------------------------------------------------------------------
// Personality profiles (personality-profiles design §3.4, §6)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn profile_tools_are_gated_like_the_other_readers_and_writers() {
    let base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let sid = initialize(&base).await;
    let names = list_tool_names(&base, &sid).await;
    assert!(names.iter().any(|n| n == "lmgw__profiles"));
    for w in ["lmgw__profile_set", "lmgw__profile_delete"] {
        assert!(!names.iter().any(|n| n == w), "{w} listed at read_only");
    }
    let r = call(&base, &sid, "lmgw__profile_delete", json!({ "id": 1 })).await;
    assert!(is_error(&r));
    assert!(text_of(&r).contains("read_only"), "{}", text_of(&r));

    let full = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&full).await;
    let tools = list_tools(&full, &sid).await;
    for t in [
        "lmgw__profiles",
        "lmgw__profile_set",
        "lmgw__profile_delete",
    ] {
        assert!(tool(&tools, t)["inputSchema"]["additionalProperties"] == json!(false));
    }
}

#[tokio::test]
async fn a_profile_is_created_changed_cleared_and_deleted_through_the_tools() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__profile_set",
            json!({
                "action": "create", "name": "Terse", "persona": "You are curt.",
                "length_rule": "One sentence.",
                "examples": "User: Hi\nReply: Hello.\n\nUser: Two\n  lines\nReply: Ok.",
                "voice_block_mode": "none", "reasoning": "off",
                "tts_alias": "", "voice": "anna",
            }),
        )
        .await,
    );
    let id = created["id"].as_i64().unwrap();
    assert_eq!(created["voice_block"], json!(""));
    assert_eq!(created["reasoning"], json!("off"));
    assert_eq!(created["voice"]["voice"], json!("anna"));
    assert_eq!(
        created["examples"],
        json!("User: Hi\nReply: Hello.\n\nUser: Two\n  lines\nReply: Ok.")
    );

    // Update: only what is given changes; the other voice fields stay.
    let updated = payload(
        &call(
            &base,
            &sid,
            "lmgw__profile_set",
            json!({
                "action": "update", "id": id, "speech_style": "calm",
                "persona": "", "voice_block_mode": "generic", "reasoning": "inherit",
            }),
        )
        .await,
    );
    assert_eq!(updated["persona"], json!("You are curt."));
    assert_eq!(updated["voice_block"], json!(null));
    assert_eq!(updated["reasoning"], json!(null));
    assert_eq!(updated["voice"]["voice"], json!("anna"));
    assert_eq!(updated["voice"]["speech_style"], json!("calm"));

    let cleared = payload(
        &call(
            &base,
            &sid,
            "lmgw__profile_set",
            json!({ "action": "update", "id": id, "clear": "persona, voice" }),
        )
        .await,
    );
    assert_eq!(cleared["persona"], json!(""));
    assert_eq!(cleared["voice"]["voice"], json!(null));
    assert_eq!(cleared["voice"]["speech_style"], json!("calm"));

    // The read tool lists it with the examples as text, and the feed heard of it.
    let list = payload(&call(&base, &sid, "lmgw__profiles", json!({})).await);
    let row = list["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .expect("listed");
    assert!(row["examples"].as_str().unwrap().starts_with("User: Hi"));
    let kinds: Vec<String> = sqlx::query_scalar("SELECT type FROM chat_feed ORDER BY seq")
        .fetch_all(&state.db)
        .await
        .unwrap();
    assert!(kinds.iter().any(|k| k == "profile.created"), "{kinds:?}");
    assert!(kinds.iter().any(|k| k == "profile.updated"), "{kinds:?}");

    // A thread using it goes back to none, and the answer says so.
    sqlx::query("INSERT INTO chat_threads (id, profile_id) VALUES (9001, ?1)")
        .bind(id)
        .execute(&state.db)
        .await
        .unwrap();
    let d = payload(&call(&base, &sid, "lmgw__profile_delete", json!({ "id": id })).await);
    assert_eq!(d["deleted"], json!(id));
    assert_eq!(d["threads_cleared"], json!(1));
    assert!(d["message"].as_str().unwrap().contains("1 thread"));
    let left: Option<i64> =
        sqlx::query_scalar("SELECT profile_id FROM chat_threads WHERE id = 9001")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(left, None);
    let gone = call(&base, &sid, "lmgw__profile_delete", json!({ "id": id })).await;
    assert!(is_error(&gone));
}

#[tokio::test]
async fn profile_set_refuses_what_the_routes_refuse_in_words() {
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;
    for (args, needle) in [
        (json!({ "action": "create" }), "name"),
        (json!({ "action": "create", "name": "Default" }), "default"),
        (json!({ "action": "update" }), "id"),
        (
            json!({ "action": "update", "id": 424242, "persona": "x" }),
            "424242",
        ),
        (
            json!({ "action": "create", "name": "A", "examples": "nonsense" }),
            "examples",
        ),
        (
            json!({ "action": "create", "name": "A", "voice_block_mode": "own" }),
            "voice_block",
        ),
        (
            json!({ "action": "create", "name": "A", "reasoning": "maybe" }),
            "reasoning",
        ),
        (
            json!({ "action": "create", "name": "A", "clear": "name" }),
            "clear",
        ),
        (
            json!({ "action": "create", "name": "A", "persnoa": "x" }),
            "persnoa",
        ),
    ] {
        let r = call(&base, &sid, "lmgw__profile_set", args.clone()).await;
        assert!(is_error(&r), "{args}");
        assert!(
            text_of(&r).to_lowercase().contains(needle),
            "{args}: {}",
            text_of(&r)
        );
    }
}

/// The pre-rename `embed` for the aux class is not accepted anywhere a tool
/// takes a target or a models kind (alpha: no compat): each refusal names the
/// values that are valid.
#[tokio::test]
async fn the_old_embed_spelling_of_aux_is_refused_naming_the_valid_values() {
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;
    for (tool, args, valid) in [
        (
            "lmgw__container",
            json!({ "target": "embed", "action": "status" }),
            "aux",
        ),
        (
            "lmgw__hf_add",
            json!({ "repo": "o/r", "file": "m.gguf", "target": "embed" }),
            "chat, aux, audio, image",
        ),
        (
            "lmgw__gguf_files",
            json!({ "target": "embed" }),
            "chat|aux|audio|image",
        ),
        ("lmgw__models", json!({ "kind": "embed" }), "aux"),
    ] {
        let result = call(&base, &sid, tool, args).await;
        assert!(is_error(&result), "{tool} accepted embed: {result}");
        let msg = text_of(&result);
        assert!(
            msg.contains("embed") && msg.contains(valid),
            "{tool}: {msg}"
        );
    }
}
