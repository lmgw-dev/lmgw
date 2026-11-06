//! Self-admin tools over `/mcp/admin` — local models: thinking controls, kv-unified, missing ids and capability overrides.

use super::*;

/// The thinking controls that reach the chat template: a default effort level
/// (`--reasoning-effort`) and freeform template variables
/// (`--chat-template-kwargs`) — two flags feeding one variable namespace, so
/// the round trip has to prove both arrive and that an effort level written
/// into the kwargs does not end up configured twice.
#[tokio::test]
async fn reasoning_effort_and_custom_kwargs_both_reach_the_command_line() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let created = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create",
            "model_id": "qwen3",
            "gguf_path": "qwen3.gguf",
            "jinja": true,
            "reasoning_preserve": true,
            "reasoning_effort": "medium",
            "chat_template_kwargs": "{\"preserve_thinking\": true}",
            "n_predict": 4096
        }),
    )
    .await;
    assert!(!is_error(&created), "create failed: {}", text_of(&created));

    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "qwen3" }),
        )
        .await,
    );
    assert_eq!(got["params"]["reasoning_effort"], "medium");
    assert_eq!(got["params"]["reasoning_preserve"], true);
    assert_eq!(
        got["params"]["chat_template_kwargs"]["preserve_thinking"],
        true
    );
    assert_eq!(got["params"]["n_predict"], 4096);

    let cmd = got["command_line"].as_str().unwrap();
    // A bare switch, not `--reasoning-preserve true`: CLI semantics, not INI
    // ones (§3.6).
    assert!(cmd.contains("--reasoning-preserve "), "{cmd}");
    assert!(cmd.contains("--reasoning-effort medium"), "{cmd}");
    assert!(cmd.contains("--n-predict 4096"), "{cmd}");
    assert!(
        cmd.contains(r#"preserve_thinking"#),
        "kwargs missing:\n{cmd}"
    );

    // Malformed JSON fails on the way in, not minutes later at model load.
    let bad = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "qwen3", "chat_template_kwargs": "[1,2]" }),
    )
    .await;
    assert!(is_error(&bad), "a JSON array was accepted");
    assert!(text_of(&bad).contains("object"), "{}", text_of(&bad));

    // Each control clears on its own; "off" is a value, not an absence, so
    // clearing the switch has to leave the template default in charge.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "qwen3", "reasoning_preserve": false,
            "clear": "reasoning_effort, n_predict"
        }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "qwen3" }),
        )
        .await,
    );
    assert!(got["params"]["reasoning_effort"].is_null(), "{got}");
    assert_eq!(got["params"]["reasoning_preserve"], false);
    assert!(got["params"]["n_predict"].is_null(), "{got}");
    let cmd = got["command_line"].as_str().unwrap();
    // `false` on a default-on option renders the `--no-*` twin (§3.6).
    assert!(cmd.contains("--no-reasoning-preserve"), "{cmd}");
    assert!(!cmd.contains("--reasoning-effort"), "{cmd}");
    assert!(!cmd.contains("--n-predict"), "{cmd}");
    assert!(cmd.contains("preserve_thinking"), "{cmd}");

    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "qwen3",
            "clear": "reasoning_preserve,chat_template_kwargs"
        }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "qwen3" }),
        )
        .await,
    );
    let cmd = got["command_line"].as_str().unwrap();
    assert!(!cmd.contains("reasoning-preserve"), "{cmd}");
    assert!(!cmd.contains("--chat-template-kwargs"), "{cmd}");

    // An effort level typed into the kwargs folds into its own field rather
    // than being configured twice, under two flags, for one template variable.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "qwen3",
            "chat_template_kwargs": "{\"reasoning_effort\":\"none\",\"other\":1}"
        }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "qwen3" }),
        )
        .await,
    );
    assert_eq!(got["params"]["reasoning_effort"], "none");
    assert!(
        got["params"]["chat_template_kwargs"]["reasoning_effort"].is_null(),
        "{got}"
    );
    let cmd = got["command_line"].as_str().unwrap();
    assert!(cmd.contains("--reasoning-effort none"), "{cmd}");
    assert!(cmd.contains(r#"{"other":1}"#), "{cmd}");
}

/// The unified-KV toggle (unified-KV design §3.1): both fields round-trip
/// through `lmgw__local_model_get`, reach the command line as the flags the
/// design names, and `clear` unsets each independently — the same path the
/// dashboard's save form and this MCP tool share (`ops::overlay_params`).
#[tokio::test]
async fn kv_unified_round_trips_through_clear_and_the_command_line() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let created = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create",
            "model_id": "qwen3-pool",
            "gguf_path": "qwen3.gguf",
            "kv_unified": true,
            "kv_unified_per_slot": 16384,
            "parallel": 4,
            "n_predict": 4096
        }),
    )
    .await;
    assert!(!is_error(&created), "create failed: {}", text_of(&created));

    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "qwen3-pool" }),
        )
        .await,
    );
    assert_eq!(got["params"]["kv_unified"], true);
    assert_eq!(got["params"]["kv_unified_per_slot"], 16384);
    let cmd = got["command_line"].as_str().unwrap();
    assert!(cmd.contains("--kv-unified "), "{cmd}");
    assert!(!cmd.contains("--no-kv-unified"), "{cmd}");
    assert!(cmd.contains("--kv-unified-per-slot 16384"), "{cmd}");

    // Flipping the tri-state to `false` renders the `--no-*` twin, same shape
    // as `reasoning_preserve`.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "qwen3-pool", "kv_unified": false, "clear": "kv_unified_per_slot" }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "qwen3-pool" }),
        )
        .await,
    );
    assert_eq!(got["params"]["kv_unified"], false);
    assert!(got["params"]["kv_unified_per_slot"].is_null(), "{got}");
    let cmd = got["command_line"].as_str().unwrap();
    assert!(cmd.contains("--no-kv-unified"), "{cmd}");
    assert!(!cmd.contains("--kv-unified-per-slot"), "{cmd}");

    // `clear: kv_unified` returns the row to llama-server's own default —
    // silent, neither flag rendered.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "qwen3-pool", "clear": "kv_unified" }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "qwen3-pool" }),
        )
        .await,
    );
    assert!(got["params"]["kv_unified"].is_null(), "{got}");
    let cmd = got["command_line"].as_str().unwrap();
    assert!(!cmd.contains("kv-unified"), "{cmd}");
}

/// Decision D2 (unified-KV design §3.3): an explicitly shared pool with more
/// than one effective slot and no `n_predict` has no bound to guard, so the
/// save is refused rather than silently left unguarded — both with an
/// explicit `parallel` and with it left at the auto default (4 slots).
/// Setting `n_predict` is what unblocks it.
#[tokio::test]
async fn kv_unified_save_is_refused_without_max_output_on_a_shared_pool() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let explicit_parallel = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create", "model_id": "no-bound-1", "gguf_path": "m.gguf",
            "kv_unified": true, "parallel": 2
        }),
    )
    .await;
    assert!(
        is_error(&explicit_parallel),
        "{}",
        text_of(&explicit_parallel)
    );
    assert!(
        text_of(&explicit_parallel).contains("n_predict"),
        "{}",
        text_of(&explicit_parallel)
    );

    let auto_slots = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create", "model_id": "no-bound-2", "gguf_path": "m.gguf",
            "kv_unified": true
        }),
    )
    .await;
    assert!(is_error(&auto_slots), "{}", text_of(&auto_slots));

    let accepted = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create", "model_id": "bounded", "gguf_path": "m.gguf",
            // The pool ledger also needs a known pool size (review finding
            // 2) — `ctx_size` here is what makes this fixture "accepted".
            "kv_unified": true, "parallel": 2, "n_predict": 4096, "ctx_size": 16384
        }),
    )
    .await;
    assert!(!is_error(&accepted), "{}", text_of(&accepted));
}

/// A short llama-server alias (`-kvu`, `runtime/argv.rs`'s `SHORT_ALIASES`)
/// typed into freeform `extra_args` must not bypass `validate_kv_unified`
/// (review finding 3): the save has to fold it into the typed `kv_unified`
/// field and judge it exactly like spelling `"kv_unified": true` out, not
/// wait for this row's next load to notice.
#[tokio::test]
async fn a_short_kv_unified_alias_in_extra_args_is_still_validated() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let refused = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create", "model_id": "short-alias-bypass", "gguf_path": "m.gguf",
            "extra_args": ["-kvu"], "parallel": 2
        }),
    )
    .await;
    assert!(is_error(&refused), "{}", text_of(&refused));
    assert!(
        text_of(&refused).contains("n_predict"),
        "{}",
        text_of(&refused)
    );
}

/// A miss on a model id used to come back as a bare "not found". A caller with
/// no shell then has nowhere to go, so the error enumerates what does exist.
#[tokio::test]
async fn a_missing_local_model_names_the_ones_that_exist() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "create", "model_id": "real-one", "gguf_path": "a.gguf" }),
    )
    .await;

    let miss = call(
        &base,
        &sid,
        "lmgw__local_model_get",
        json!({ "model_id": "typo" }),
    )
    .await;
    assert!(is_error(&miss));
    assert!(text_of(&miss).contains("real-one"), "{}", text_of(&miss));
}

/// Owner overrides (model-capabilities design §7): `capabilities_override`
/// arrives as a JSON string (MCP arguments are flat scalars — mirrors
/// `chat_template_kwargs`), round-trips through `lmgw__local_model_get`, and
/// clears through `clear`. A malformed one — an unknown top-level key, a
/// non-object `capabilities`, invalid JSON — is refused naming the problem
/// rather than silently accepted or dropped.
#[tokio::test]
async fn capabilities_override_round_trips_and_rejects_malformed_shapes() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "create", "model_id": "owner-caps", "gguf_path": "a.gguf" }),
    )
    .await;

    let set = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "owner-caps",
            "capabilities_override": "{\"max_output_tokens\":8192,\"notes\":[\"hand-verified\"]}"
        }),
    )
    .await;
    assert!(!is_error(&set), "{}", text_of(&set));

    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "owner-caps" }),
        )
        .await,
    );
    assert_eq!(
        got["capabilities_override"],
        json!({"max_output_tokens": 8192, "notes": ["hand-verified"]})
    );

    // Clearing goes back to null, not an empty object.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "owner-caps", "clear": "capabilities_override" }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "owner-caps" }),
        )
        .await,
    );
    assert!(got["capabilities_override"].is_null(), "{got}");

    // A non-object `capabilities` value is refused, naming the field.
    let bad = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "owner-caps",
            "capabilities_override": "{\"capabilities\": \"not an object\"}"
        }),
    )
    .await;
    assert!(
        is_error(&bad),
        "a non-object capabilities value was accepted"
    );
    assert!(text_of(&bad).contains("capabilities"), "{}", text_of(&bad));

    // Invalid JSON entirely is refused too.
    let bad_json = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "owner-caps",
            "capabilities_override": "{not json"
        }),
    )
    .await;
    assert!(is_error(&bad_json), "invalid JSON was accepted");

    // An unknown top-level key names itself in the error.
    let bad_key = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "owner-caps",
            "capabilities_override": "{\"bogus_key\": 1}"
        }),
    )
    .await;
    assert!(is_error(&bad_key), "an unknown top-level key was accepted");
    assert!(
        text_of(&bad_key).contains("bogus_key"),
        "{}",
        text_of(&bad_key)
    );
}

/// The same owner-override shape, on an alias (`lmgw__model_set`) rather than
/// a local model — the MCP-facing patch carries its own `clear` (the alias
/// patch had none before this field existed).
#[tokio::test]
async fn alias_capabilities_override_round_trips_and_clears() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let up_id = store::insert_upstream(
        &state.db,
        &store::NewUpstream {
            name: "cloud".into(),
            protocol: lmgw_core::config::Protocol::OpenaiCompatible,
            kind: lmgw_core::config::UpstreamKind::Generic,
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__model_set",
            json!({
                "action": "create", "alias": "owner-caps-alias",
                "upstream": up_id.to_string(), "upstream_model": "tgt"
            }),
        )
        .await,
    );
    let id = created["id"].as_i64().unwrap();

    let set = call(
        &base,
        &sid,
        "lmgw__model_set",
        json!({
            "action": "update", "id": id,
            "capabilities_override": "{\"capabilities\":{\"task\":\"chat\",\"endpoints\":[\"/v1/chat/completions\"],\"source\":\"catalog\"}}"
        }),
    )
    .await;
    assert!(!is_error(&set), "{}", text_of(&set));

    let aliases = store::list_aliases(&state.db).await.unwrap();
    let alias = aliases.iter().find(|a| a.id == id).unwrap();
    assert_eq!(
        alias.capabilities_override,
        Some(json!({
            "capabilities": {
                "task": "chat",
                "endpoints": ["/v1/chat/completions"],
                "source": "catalog"
            }
        }))
    );

    call(
        &base,
        &sid,
        "lmgw__model_set",
        json!({ "action": "update", "id": id, "clear": "capabilities_override" }),
    )
    .await;
    let aliases = store::list_aliases(&state.db).await.unwrap();
    let alias = aliases.iter().find(|a| a.id == id).unwrap();
    assert_eq!(alias.capabilities_override, None);

    let bad = call(
        &base,
        &sid,
        "lmgw__model_set",
        json!({ "action": "update", "id": id, "clear": "nonsense_field" }),
    )
    .await;
    assert!(is_error(&bad), "an unknown clear target must be reported");
    assert!(text_of(&bad).contains("nonsense_field"));
}
