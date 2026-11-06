//! Self-admin tools over `/mcp/admin` — the GPU hold tool, the settings tools and an unknown built-in.

use super::*;

// ---------------------------------------------------------------------------
// GPU hold (gpu-hold design §6/§7.10)
// ---------------------------------------------------------------------------

/// `lmgw__hold_set` writes, so it follows every other mutation through the
/// mode gate: listed at `full`, absent at `read_only` (with an actionable
/// refusal if called anyway), and absent along with everything else at `off`.
#[tokio::test]
async fn hold_set_is_gated_like_every_other_mutation() {
    let full_base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let full_sid = initialize(&full_base).await;
    assert!(
        list_tool_names(&full_base, &full_sid)
            .await
            .iter()
            .any(|n| n == "lmgw__hold_set"),
        "lmgw__hold_set must be listed at full"
    );

    let off_base = serve_with(state_with_mode(SelfAdmin::Off).await).await;
    let off_sid = initialize(&off_base).await;
    assert!(
        list_tool_names(&off_base, &off_sid).await.is_empty(),
        "self_admin=off must expose no tools at all"
    );

    let ro_base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let ro_sid = initialize(&ro_base).await;
    assert!(
        !list_tool_names(&ro_base, &ro_sid)
            .await
            .iter()
            .any(|n| n == "lmgw__hold_set"),
        "lmgw__hold_set must not be listed at read_only — it mutates"
    );
    let refused = call(&ro_base, &ro_sid, "lmgw__hold_set", json!({"active": true})).await;
    assert!(is_error(&refused), "{}", text_of(&refused));
    assert!(
        text_of(&refused).contains("read_only"),
        "unhelpful refusal: {}",
        text_of(&refused)
    );
}

/// Calling `lmgw__hold_set` under `full` really flips the switch: the next
/// `lmgw__status` sees `vram.hold_active`, and `lmgw__settings` carries the
/// `hold` block with both `active` and the `fallback_alias` that
/// `lmgw__settings_set` just wrote.
///
/// Both halves are asserted through the *tools*, because that is the whole
/// contract an agent has here: the switch is settable only through
/// `lmgw__hold_set` (engaging it stops containers, a side effect the generic
/// settings call must not grow) while the fallback is settable only through
/// `lmgw__settings_set` — and neither is any use if the read tools do not show
/// what the other one did.
#[tokio::test]
async fn hold_set_flips_the_hold_and_every_read_tool_sees_it() {
    let state = state_with_mode(SelfAdmin::Full).await;
    // A real cloud alias, which `lmgw__settings_set` no longer takes as a
    // global fallback: each model has its own (openai egress design §14.2).
    let upstream = store::insert_upstream(
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
    store::insert_alias(
        &state.db,
        &store::NewAlias {
            alias: "cloud-chat".into(),
            upstream_id: upstream,
            upstream_model_id: "gpt-cloud".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let before = payload(&call(&base, &sid, "lmgw__status", json!({})).await);
    assert_eq!(before["vram"]["hold_active"], false, "{before}");

    let set = call(
        &base,
        &sid,
        "lmgw__settings_set",
        json!({"hold_fallback_alias": "cloud-chat"}),
    )
    .await;
    assert!(
        is_error(&set),
        "there is no global fallback: {}",
        text_of(&set)
    );

    let out = payload(&call(&base, &sid, "lmgw__hold_set", json!({"active": true})).await);
    assert_eq!(out["active"], true, "{out}");
    assert!(out.get("fallback_alias").is_none(), "{out}");

    let after = payload(&call(&base, &sid, "lmgw__status", json!({})).await);
    assert_eq!(after["vram"]["hold_active"], true, "{after}");

    let settings = payload(&call(&base, &sid, "lmgw__settings", json!({})).await);
    assert_eq!(settings["hold"], json!({"active": true}), "{settings}");

    // And back off again — the switch is a switch, not a latch.
    let out = payload(&call(&base, &sid, "lmgw__hold_set", json!({"active": false})).await);
    assert_eq!(out["active"], false, "{out}");
    let after = payload(&call(&base, &sid, "lmgw__status", json!({})).await);
    assert_eq!(after["vram"]["hold_active"], false, "{after}");
}

/// `vram.fallback_on_external` (candidate-aliases design §4.7, §12.25) reads
/// as on by default next to the hold block, and `lmgw__settings_set` writes
/// it back — the same round trip as the hold's fallback alias above, for the
/// switch that decides when *that* fallback (or a per-model one) answers
/// instead of queueing.
#[tokio::test]
async fn settings_set_fallback_on_external_round_trips() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let before = payload(&call(&base, &sid, "lmgw__settings", json!({})).await);
    assert_eq!(before["vram"]["fallback_on_external"], true, "{before}");

    let set = call(
        &base,
        &sid,
        "lmgw__settings_set",
        json!({"fallback_on_external": false}),
    )
    .await;
    assert!(!is_error(&set), "{}", text_of(&set));

    let after = payload(&call(&base, &sid, "lmgw__settings", json!({})).await);
    assert_eq!(after["vram"]["fallback_on_external"], false, "{after}");
}

/// `realtime` is one JSON-encoded string on the flat tool schema (realtime
/// design §12), hoisted into the same patch the dashboard saves — with the
/// same refusals, and `""` leaving the section alone.
#[tokio::test]
async fn settings_set_realtime_takes_a_json_string() {
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;

    let set = call(
        &base,
        &sid,
        "lmgw__settings_set",
        json!({"realtime": r#"{"warm_on_connect": false, "semantic_vad": {"high": {"max_wait_ms": 2500}}}"#}),
    )
    .await;
    let out = payload(&set);
    assert_eq!(
        out["changed"],
        json!(["realtime.semantic_vad", "realtime.warm_on_connect"]),
        "{out}"
    );
    let after = payload(&call(&base, &sid, "lmgw__settings", json!({})).await);
    assert_eq!(after["realtime"]["warm_on_connect"], false, "{after}");
    assert_eq!(
        after["realtime"]["semantic_vad"]["high"]["max_wait_ms"],
        2500
    );
    assert_eq!(after["realtime"]["semantic_vad"]["high"]["floor"], 0.1);

    let refused = call(
        &base,
        &sid,
        "lmgw__settings_set",
        json!({"realtime": r#"{"max_message_mb": 0, "max_frame_mb": 0}"#}),
    )
    .await;
    assert!(is_error(&refused));
    assert!(
        text_of(&refused).contains("realtime.max_frame_mb"),
        "{}",
        text_of(&refused)
    );
    let bad_json = call(
        &base,
        &sid,
        "lmgw__settings_set",
        json!({"realtime": "{nope"}),
    )
    .await;
    assert!(is_error(&bad_json));
    assert!(text_of(&bad_json).contains("realtime: invalid JSON"));
    let empty = call(
        &base,
        &sid,
        "lmgw__settings_set",
        json!({"realtime": "", "retention_days": 7}),
    )
    .await;
    assert_eq!(payload(&empty)["changed"], json!(["retention_days"]));
}

/// An unknown name inside the reserved namespace is a genuine protocol error,
/// not a tool error — the distinction the dispatcher draws in §14.
#[tokio::test]
async fn unknown_builtin_is_a_json_rpc_method_not_found() {
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;
    let (status, _, body) = post(
        &base,
        Some(&sid),
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "lmgw__does_not_exist", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["error"]["code"], -32601, "{body}");
}
