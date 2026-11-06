//! Self-admin tools over `/mcp/admin` — mutations reaching the database and the live snapshot.

use super::*;

// ---------------------------------------------------------------------------
// Mutations reach the database *and* the live snapshot
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_upstream_and_alias_makes_a_routable_model() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let inspect = state.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__upstream_set",
            json!({
                "action": "create", "name": "acme", "protocol": "openai_compatible",
                "base_url": "https://api.acme.test/v1",
            }),
        )
        .await,
    );
    assert_eq!(created["ok"], true);

    // The alias references the upstream by *name* — what a caller reading
    // lmgw__upstreams already has, without needing the numeric id.
    payload(
        &call(
            &base,
            &sid,
            "lmgw__model_set",
            json!({
                "action": "create", "alias": "fast",
                "upstream": "acme", "upstream_model": "acme-turbo",
            }),
        )
        .await,
    );

    // Visible to the tool plane...
    let models = payload(&call(&base, &sid, "lmgw__models", json!({ "kind": "alias" })).await);
    let names: Vec<&str> = models["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["fast"]);

    // ...and, crucially, to the live snapshot the request hot path reads, which
    // only holds if each mutation reloaded it.
    let route = inspect
        .snapshot()
        .resolve("fast")
        .expect("alias must route");
    assert_eq!(route.upstream.name, "acme");
    assert_eq!(route.upstream_model, "acme-turbo");
}

/// `lmgw__models` joins the same builder `GET /v1/models` renders from
/// (model-capabilities design §8 item 9), by name, rather than re-deriving:
/// a local chat row gets `capabilities.task == "chat"` and its configured
/// `n_predict` back as `max_output_tokens`, an aux row gets
/// `capabilities.task == "embedding"`.
#[tokio::test]
async fn models_carries_the_same_capabilities_v1_models_publishes() {
    let state = state_with_mode(SelfAdmin::Full).await;

    // A real (synthetic) GGUF on disk in each class's own models dir — the
    // capability builder returns `capabilities: None` for a row whose
    // weights it could not read, so a fake path would not exercise the field
    // this test is about.
    let chat_dir = tempfile::tempdir().unwrap();
    let aux_dir = tempfile::tempdir().unwrap();
    synth::chat("qwen35", 32768).write_to(&chat_dir.path().join("chat.gguf"));
    synth::embedding("qwen3", 3, 32768).write_to(&aux_dir.path().join("embed.gguf"));

    let mut settings = store::load_settings(&state.db).await.unwrap();
    settings.router.models_dir = chat_dir.path().display().to_string();
    settings.aux_router.models_dir = aux_dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "chat-cap", "gguf_path": "chat.gguf",
                "n_predict": 512,
            }),
        )
        .await,
    );
    payload(
        &call(
            &base,
            &sid,
            "lmgw__aux_model_set",
            json!({
                "action": "create", "model_id": "emb-cap", "gguf_path": "embed.gguf",
                "kind": "embed",
            }),
        )
        .await,
    );

    let models = payload(&call(&base, &sid, "lmgw__models", json!({})).await);
    let list = models["models"].as_array().unwrap();

    let chat = list
        .iter()
        .find(|m| m["name"] == "chat-cap")
        .expect("chat row listed");
    assert_eq!(chat["capabilities"]["task"], "chat", "{chat}");
    assert_eq!(chat["max_output_tokens"], 512, "{chat}");

    let aux = list
        .iter()
        .find(|m| m["name"] == "embed/emb-cap")
        .expect("aux row listed");
    assert_eq!(aux["capabilities"]["task"], "embedding", "{aux}");
}

#[tokio::test]
async fn update_is_partial_and_preserves_the_stored_key() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let inspect = state.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__upstream_set",
            json!({
                "action": "create", "name": "acme", "protocol": "openai_compatible",
                "base_url": "https://api.acme.test/v1", "api_key": "sk-keep-me",
                "timeout_ms": 45000,
            }),
        )
        .await,
    );
    let id = created["id"].as_i64().unwrap();

    // Change one field only. Everything unmentioned — including the secret that
    // reads back as `<set>` — must survive untouched.
    payload(
        &call(
            &base,
            &sid,
            "lmgw__upstream_set",
            json!({ "action": "update", "id": id, "timeout_ms": 30000 }),
        )
        .await,
    );
    let stored = store::get_upstream(&inspect.db, id).await.unwrap().unwrap();
    assert_eq!(stored.base_url, "https://api.acme.test/v1");
    assert_eq!(stored.name, "acme");
    assert_eq!(stored.timeout_ms, 30000);
    assert_eq!(stored.api_key.as_deref(), Some("sk-keep-me"));

    // Moving it to another host without the key for that host is refused
    // (review G-3): the stored key is never sent where it was not given.
    let refused = call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "update", "id": id, "base_url": "https://new.acme.test/v1" }),
    )
    .await;
    assert!(is_error(&refused), "{refused}");
    assert!(
        text_of(&refused).contains("needs the key for that address in the same call"),
        "{}",
        text_of(&refused)
    );
    let stored = store::get_upstream(&inspect.db, id).await.unwrap().unwrap();
    assert_eq!(
        stored.base_url, "https://api.acme.test/v1",
        "nothing changed"
    );
    // Every action that applies base_url is checked, not only update (the
    // review's verification V-1): enable and disable move the row too.
    for action in ["enable", "disable"] {
        let refused = call(
            &base,
            &sid,
            "lmgw__upstream_set",
            json!({ "action": action, "id": id, "base_url": "https://new.acme.test/v1" }),
        )
        .await;
        assert!(is_error(&refused), "{action}: {refused}");
        assert!(
            text_of(&refused).contains("needs the key for that address in the same call"),
            "{action}: {}",
            text_of(&refused)
        );
        let stored = store::get_upstream(&inspect.db, id).await.unwrap().unwrap();
        assert_eq!(stored.base_url, "https://api.acme.test/v1", "{action}");
        assert!(
            stored.enabled,
            "{action}: nothing changed, enabled included"
        );
        assert_eq!(stored.api_key.as_deref(), Some("sk-keep-me"), "{action}");
    }
    // The same address written differently is no move.
    payload(
        &call(
            &base,
            &sid,
            "lmgw__upstream_set",
            json!({ "action": "update", "id": id, "base_url": "https://API.acme.test/v1/" }),
        )
        .await,
    );
    // With the key for the new host, it moves.
    payload(
        &call(
            &base,
            &sid,
            "lmgw__upstream_set",
            json!({ "action": "update", "id": id, "base_url": "https://new.acme.test/v1",
                    "api_key": "sk-new-host" }),
        )
        .await,
    );
    let stored = store::get_upstream(&inspect.db, id).await.unwrap().unwrap();
    assert_eq!(stored.base_url, "https://new.acme.test/v1");
    assert_eq!(stored.api_key.as_deref(), Some("sk-new-host"));
}

/// An MCP server's stored headers follow its `url` the same way (review
/// G-3): a move without them is refused, whichever action carries it and
/// with `null` as no restatement, and a move that restates them — `""`
/// included, which sends none — goes through.
#[tokio::test]
async fn moving_an_mcp_server_needs_its_headers_restated() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let inspect = state.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;
    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__mcp_server_set",
            json!({
                "action": "create", "name": "tickets", "transport": "http",
                "url": "https://mcp.tickets.test/mcp",
                "headers": "Authorization: Bearer secret-token",
                "autostart": false, "enabled": false,
            }),
        )
        .await,
    );
    let id = created["id"].as_i64().unwrap();
    let refused = call(
        &base,
        &sid,
        "lmgw__mcp_server_set",
        json!({ "action": "update", "id": id, "url": "https://x.example/mcp" }),
    )
    .await;
    assert!(is_error(&refused), "{refused}");
    assert!(
        text_of(&refused).contains("needs the headers for that address"),
        "{}",
        text_of(&refused)
    );
    let row = store::get_mcp_server(&inspect.db, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.url.as_deref(), Some("https://mcp.tickets.test/mcp"));
    // `null` keeps the stored headers, so it restates nothing (V-2), and
    // enable and disable apply url as update does (V-1).
    for args in [
        json!({ "action": "update", "id": id, "url": "https://x.example/mcp", "headers": null }),
        json!({ "action": "enable", "id": id, "url": "https://x.example/mcp" }),
        json!({ "action": "disable", "id": id, "url": "https://x.example/mcp" }),
    ] {
        let refused = call(&base, &sid, "lmgw__mcp_server_set", args.clone()).await;
        assert!(is_error(&refused), "{args}: {refused}");
        assert!(
            text_of(&refused).contains("needs the headers for that address"),
            "{args}: {}",
            text_of(&refused)
        );
        let row = store::get_mcp_server(&inspect.db, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.url.as_deref(),
            Some("https://mcp.tickets.test/mcp"),
            "{args}"
        );
        assert!(!row.enabled, "{args}: nothing changed, enabled included");
        assert_eq!(row.headers.len(), 1, "{args}");
    }
    payload(
        &call(
            &base,
            &sid,
            "lmgw__mcp_server_set",
            json!({ "action": "update", "id": id, "url": "https://x.example/mcp", "headers": "" }),
        )
        .await,
    );
    let row = store::get_mcp_server(&inspect.db, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.url.as_deref(), Some("https://x.example/mcp"));
    assert!(row.headers.is_empty(), "the old credential did not travel");
}

/// The access settings are refused to every tool caller, the owner's own
/// plane included (review G-1): they change on Settings.
#[tokio::test]
async fn the_access_settings_are_no_tool_s_to_change() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let inspect = state.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;
    for args in [
        json!({ "auth_enabled": true }),
        json!({ "self_admin": "off" }),
        json!({ "bind_addr": "0.0.0.0:8787" }),
        json!({ "agent_origin_suffix": "lmgw.example" }),
        json!({ "devices": { "token_minutes": 60 } }),
    ] {
        let key = args.as_object().unwrap().keys().next().unwrap().clone();
        let refused = call(&base, &sid, "lmgw__settings_set", args).await;
        assert!(is_error(&refused), "{key}: {refused}");
        assert!(
            text_of(&refused).starts_with(&format!(
                "{key} cannot be changed through lmgw's admin tools"
            )),
            "{}",
            text_of(&refused)
        );
    }
    let s = inspect.snapshot().settings.clone();
    assert!(!s.auth_enabled);
    assert_eq!(s.self_admin, SelfAdmin::Full);
    assert_eq!(s.devices.token_minutes, 10);
}

#[tokio::test]
async fn enable_disable_round_trips() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let inspect = state.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let id = payload(
        &call(
            &base,
            &sid,
            "lmgw__upstream_set",
            json!({ "action": "create", "name": "acme", "protocol": "openai_compatible",
                    "base_url": "https://api.acme.test/v1" }),
        )
        .await,
    )["id"]
        .as_i64()
        .unwrap();

    call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "disable", "id": id }),
    )
    .await;
    assert!(
        !store::get_upstream(&inspect.db, id)
            .await
            .unwrap()
            .unwrap()
            .enabled
    );

    call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "enable", "id": id }),
    )
    .await;
    assert!(
        store::get_upstream(&inspect.db, id)
            .await
            .unwrap()
            .unwrap()
            .enabled
    );
}

#[tokio::test]
async fn settings_set_cannot_widen_its_own_gate() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let inspect = state.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    // `self_admin` is not a declared property, and the schema is closed — the
    // attempt is rejected outright rather than quietly ignored.
    let result = call(
        &base,
        &sid,
        "lmgw__settings_set",
        json!({ "self_admin": "full", "retention_days": 7 }),
    )
    .await;
    assert!(is_error(&result), "unknown argument must be rejected");

    // A legitimate change still works, and the gate is untouched.
    payload(
        &call(
            &base,
            &sid,
            "lmgw__settings_set",
            json!({ "retention_days": 7 }),
        )
        .await,
    );
    let s = inspect.snapshot();
    assert_eq!(s.settings.retention_days, 7);
    assert_eq!(s.settings.self_admin, SelfAdmin::Full);
}

#[tokio::test]
async fn a_bad_argument_is_a_readable_tool_error() {
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;

    // Missing required fields are reported together, so a caller needs one
    // correction rather than one round trip per field.
    let result = call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "create", "name": "incomplete" }),
    )
    .await;
    assert!(is_error(&result));
    let msg = text_of(&result);
    assert!(msg.contains("base_url"), "{msg}");
    assert!(msg.contains("protocol"), "{msg}");

    // An action that doesn't exist lists the ones that do.
    let result = call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "obliterate", "id": 1 }),
    )
    .await;
    assert!(is_error(&result));
    assert!(text_of(&result).contains("create"), "{}", text_of(&result));
}
