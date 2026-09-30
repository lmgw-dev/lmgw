//! GPU hold (gpu-hold design §4, §5, §6)

use super::*;

// The full §7 matrix is its own work package; these three prove the core path
// end to end — refuse, fall back, and free the GPU — because each of them is a
// different mechanism (`resolve_for_request`, the response header, the sweep)
// and a regression in any one of them is silent.

/// A cloud upstream with an alias row pointing at it, for the fallback cases.
/// A real second server rather than a bare `expose_all` upstream, so an unknown
/// alias in these tests really is unknown — with `expose_all` every typo would
/// resolve and the refusal paths could not be told apart from the served ones.
pub(super) async fn cloud_upstream(f: &Fixture, alias: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "cloud-1", "object": "chat.completion", "created": 1,
            "model": "gpt-cloud",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "from the cloud"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3},
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"object": "embedding", "index": 0, "embedding": [0.25, 0.75]}],
            "model": "gpt-cloud",
            "usage": {"prompt_tokens": 2, "total_tokens": 2},
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "cloud-2", "object": "text_completion", "created": 1, "model": "gpt-cloud",
            "choices": [{"index": 0, "text": "from the cloud", "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3},
        })))
        .mount(&server)
        .await;

    let upstream_id = store::insert_upstream(
        &f.state.db,
        &store::NewUpstream {
            name: "cloud".into(),
            protocol: lmgw_core::config::Protocol::Openai,
            kind: lmgw_core::config::UpstreamKind::Generic,
            base_url: format!("{}/v1", server.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &f.state.db,
        &store::NewAlias {
            alias: alias.into(),
            upstream_id,
            upstream_model_id: "gpt-cloud".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    server
}

/// `x-lmgw-fallback-reason`, which always travels with `x-lmgw-fallback`
/// (candidate-aliases design §4.7).
pub(super) fn fallback_reason(resp: &reqwest::Response) -> Option<&str> {
    resp.headers()
        .get("x-lmgw-fallback-reason")
        .and_then(|v| v.to_str().ok())
}

/// Engage the hold the way every surface does — through the op, so the sweep
/// and the snapshot reload are part of what is under test.
pub(super) async fn engage_hold(f: &Fixture) -> Value {
    lmgw_core::ops::hold_set(&f.state, true).await.unwrap()
}

/// Hold on, no fallback anywhere: the request is refused at resolve time with
/// the named error, and **no container is started**. The last part is the whole
/// point — a 503 that still loaded the model would leave the GPU exactly as
/// occupied as before.
#[tokio::test]
async fn a_held_chat_request_without_a_fallback_is_refused_and_starts_nothing() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    engage_hold(&f).await;

    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 503, "a hold is a 503, not a 500 or a hang");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_hold", "{body}");
    assert_eq!(
        body["error"]["type"], "api_error",
        "not overloaded_error — that is the SDKs' retry signal: {body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("chat-model"),
        "the refusal names the model: {body}"
    );
    assert!(
        f.runs().is_empty(),
        "a held refusal must not start a container: {:?}",
        f.runs()
    );
}

/// Hold on with a global fallback: the same request is served by the cloud
/// upstream, the response says so in `x-lmgw-fallback`, and the body still
/// echoes the alias the client asked for — a client matching the echoed name
/// against its own request must not break because lmgw re-routed.
#[tokio::test]
async fn a_held_chat_request_falls_back_to_the_cloud_alias_and_says_so() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _cloud = cloud_upstream(&f, "cloud-chat").await;

    let mut s = f.state.snapshot().settings.clone();
    s.hold.fallback_alias = Some("cloud-chat".into());
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    engage_hold(&f).await;

    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("cloud-chat"),
        "the substitution is only visible in this header"
    );
    assert_eq!(fallback_reason(&resp), Some("hold"));
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["model"], "chat-model",
        "the body echoes the requested alias, not the fallback: {body}"
    );
    assert_eq!(
        body["choices"][0]["message"]["content"], "from the cloud",
        "it really was answered by the cloud mock: {body}"
    );
    assert!(
        f.runs().is_empty(),
        "a fallback must not touch the GPU: {:?}",
        f.runs()
    );

    // The request log keeps the whole story: what the client asked for,
    // where it actually went — `record` is handed the swapped route and the
    // original alias — and why (`fallback_reason`, candidate-aliases §4.7).
    let logs = store::query_logs(
        &f.state.db,
        &store::LogFilter {
            alias: Some("chat-model".into()),
            upstream_name: None,
            errors_only: false,
            limit: 10,
            before_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let row = logs.first().expect("the fallback call was logged");
    assert_eq!(row.upstream_name.as_deref(), Some("cloud"));
    assert_eq!(row.upstream_model.as_deref(), Some("gpt-cloud"));
    assert_eq!(row.fallback_reason.as_deref(), Some("hold"));
}

/// Engaging the hold with one model resident and idle stops it: the op names
/// it in `stopped`, and podman really was told to stop it. Stop, never remove —
/// the stopped container is what the next start's `--replace` collects.
#[tokio::test]
async fn engaging_the_hold_stops_an_idle_resident_model() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);

    let out = engage_hold(&f).await;
    assert_eq!(out["active"], true);
    assert_eq!(
        out["stopped"],
        json!(["chat/chat-model"]),
        "the op response is what the tray and the dashboard report: {out}"
    );
    assert_eq!(out["draining"], json!([]), "{out}");
    assert_eq!(
        f.stops(),
        vec!["chat-model".to_string()],
        "podman was actually told to stop it"
    );

    // And the state is visible where every surface reads it from.
    let v = vram_status(&f.gateway).await;
    assert_eq!(v["hold_active"], true, "{v}");
    assert_eq!(v["resident"], json!([]), "the card is handed back: {v}");
}

/// Aux never inherits the global chat fallback (§2: a different embedding
/// model silently corrupts a vector index) — only a row's own `alias` mode
/// gives it one. With only the global set, an embed request is refused exactly
/// like a chat one with no fallback at all.
#[tokio::test]
async fn a_held_embed_request_does_not_inherit_the_global_fallback() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _cloud = cloud_upstream(&f, "cloud-chat").await;

    let mut s = f.state.snapshot().settings.clone();
    s.hold.fallback_alias = Some("cloud-chat".into());
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    engage_hold(&f).await;

    let resp = embed(&f.gateway).await;
    assert_eq!(
        resp.status(),
        503,
        "aux must not inherit the chat-only global"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_hold", "{body}");
    assert!(f.runs().is_empty(), "{:?}", f.runs());
}

/// A row override is the only way an aux model gets a fallback: with
/// `hold_fallback_mode = alias` pointing at a cloud alias, the embed request is
/// served by that upstream and says so — and never touches the GPU.
#[tokio::test]
async fn a_held_embed_request_falls_back_through_its_own_row_override() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _cloud = cloud_upstream(&f, "cloud-embed").await;

    let aux = f.state.snapshot().aux_models[0].clone();
    store::update_aux_model(
        &f.state.db,
        aux.id,
        &NewAuxModel {
            model_id: aux.model_id.clone(),
            gguf_path: aux.gguf_path.clone(),
            kind: aux.kind,
            pooling: aux.pooling.clone(),
            ctx_size: aux.ctx_size,
            args: aux.args.clone(),
            idle_seconds: aux.idle_seconds,
            enabled: aux.enabled,
            image: aux.image.clone(),
            extra_run_args: aux.extra_run_args.clone(),
            warm_start: aux.warm_start,
            hold_fallback_mode: HoldFallbackMode::Alias,
            hold_fallback: Some("cloud-embed".into()),
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    engage_hold(&f).await;

    let resp = embed(&f.gateway).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("cloud-embed"),
        "the row's own override is what routed this"
    );
    assert_eq!(fallback_reason(&resp), Some("hold"));
    assert!(f.runs().is_empty(), "{:?}", f.runs());
}

/// A row explicitly set to `none` wins over a global fallback that would
/// otherwise apply — the row is asking to be refused, not routed.
#[tokio::test]
async fn a_chat_row_with_mode_none_refuses_the_global_fallback() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _cloud = cloud_upstream(&f, "cloud-chat").await;

    let mut s = f.state.snapshot().settings.clone();
    s.hold.fallback_alias = Some("cloud-chat".into());
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();

    let chat_row = f.state.snapshot().local_models[0].clone();
    store::update_local_model(
        &f.state.db,
        chat_row.id,
        &NewLocalModel {
            model_id: chat_row.model_id.clone(),
            gguf_path: chat_row.gguf_path.clone(),
            params: chat_row.params.clone(),
            args: chat_row.args.clone(),
            idle_seconds: chat_row.idle_seconds,
            enabled: chat_row.enabled,
            public: chat_row.public,
            image: chat_row.image.clone(),
            extra_run_args: chat_row.extra_run_args.clone(),
            warm_start: chat_row.warm_start,
            hold_fallback_mode: HoldFallbackMode::None,
            hold_fallback: None,
            capabilities_override: chat_row.capabilities_override.clone(),
            ladder: chat_row.ladder.clone(),
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    engage_hold(&f).await;

    let resp = chat(&f.gateway).await;
    assert_eq!(
        resp.status(),
        503,
        "the row's own 'none' wins over the global fallback"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_hold", "{body}");
    assert!(f.runs().is_empty(), "{:?}", f.runs());
}

/// The set-time validator (`ops::validate_fallback_alias`) is bypassed here on
/// purpose — writing `hold.fallback_alias` straight through `save_settings`,
/// not through `ops::settings_set` — because rows and aliases change after
/// they are validated (§2). This is the request-time re-check:
/// `resolve_for_request` re-resolves the fallback on every held request and
/// refuses by name rather than silently starting the container the hold
/// exists to prevent.
#[tokio::test]
async fn a_global_fallback_naming_a_local_model_is_refused_at_request_time() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;

    let mut s = f.state.snapshot().settings.clone();
    s.hold.fallback_alias = Some("embed/embed-model".into());
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    engage_hold(&f).await;

    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_hold", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("is itself a local model"),
        "the misconfiguration is named, not silently downgraded to a bare refusal: {body}"
    );
    assert!(f.runs().is_empty(), "{:?}", f.runs());
}

/// Same request-time re-check, the other named failure: a fallback alias that
/// no longer resolves at all (deleted, typo'd — set-time validation cannot
/// catch either once the row has since changed).
#[tokio::test]
async fn a_global_fallback_that_does_not_resolve_is_refused_at_request_time() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;

    let mut s = f.state.snapshot().settings.clone();
    s.hold.fallback_alias = Some("no-such-alias".into());
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    engage_hold(&f).await;

    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_hold", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("does not resolve"),
        "{body}"
    );
    assert!(f.runs().is_empty(), "{:?}", f.runs());
}

/// A model that is mid-request when the hold engages is not a victim of the
/// sweep — it is still working, so it is listed as `draining` (both in the op
/// response and continuously in `GET /api/vram`), never `stopped`. Once the
/// request ends its claim drops, and the *next* `reap_idle` tick — the one
/// that runs the hold sweep ahead of the ordinary idle logic — takes it, even
/// though its own `idle_seconds` is 0 ("never reap"): under a hold that promise
/// cannot be kept and lmgw still has to get off the card (§5).
#[tokio::test]
async fn engaging_the_hold_lists_a_busy_resident_as_draining_and_reap_idle_stops_it_once_it_finishes(
) {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;

    // Ahead of the container's own default-priority chat responder, and slow
    // enough that the request is reliably still in flight when hold engages.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "id": "c1", "object": "chat.completion", "created": 1, "model": "chat-model",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"},
                                 "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                }))
                .set_delay(Duration::from_millis(800)),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&f.first)
        .await;

    let gateway = f.gateway.clone();
    let chatting = tokio::spawn(async move { chat(&gateway).await.status() });

    for _ in 0..3_000 {
        if f.state
            .runtime()
            .list()
            .iter()
            .any(|v| v.model_id == "chat-model" && v.in_flight > 0)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        f.state
            .runtime()
            .list()
            .iter()
            .any(|v| v.model_id == "chat-model" && v.in_flight > 0),
        "the request never reached the container"
    );

    let out = engage_hold(&f).await;
    assert_eq!(out["stopped"], json!([]), "still working: {out}");
    assert_eq!(out["draining"], json!(["chat/chat-model"]), "{out}");
    assert!(f.stops().is_empty(), "{:?}", f.stops());

    let v = vram_status(&f.gateway).await;
    assert_eq!(v["hold_active"], true, "{v}");
    assert!(
        v["draining"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "chat/chat-model"),
        "one spelling for one thing — the live frame names a draining model \
         exactly as the op response does: {v}"
    );

    assert_eq!(chatting.await.unwrap(), 200, "hold never kills work");

    lmgw_core::runtime::lifecycle::reap_idle(&f.state).await;
    assert_eq!(
        f.stops(),
        vec!["chat-model".to_string()],
        "idle_seconds = 0 does not save it from the hold sweep"
    );

    let v = vram_status(&f.gateway).await;
    assert!(v["draining"].as_array().unwrap().is_empty(), "{v}");
    assert_eq!(v["resident"], json!([]), "{v}");
}

/// Hold and admission are orthogonal switches (§1): a disabled scheduler still
/// refuses a held local request, because the gate lives at resolve time, not
/// in `admit`.
#[tokio::test]
async fn hold_refuses_even_when_vram_admission_is_disabled() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let mut s = f.state.snapshot().settings.clone();
    s.vram.enabled = false;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    engage_hold(&f).await;

    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 503, "independence: hold wins regardless");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_hold", "{body}");
    assert!(f.runs().is_empty(), "{:?}", f.runs());
}

/// `container start`/`restart` are refused before either verb's own stop step
/// (§2) — a restart under a hold would otherwise stop a resident model and
/// then fail to start it, doing exactly the destructive half of what the
/// caller asked for. The hold is flipped directly through `save_settings`
/// here, bypassing `ops::hold_set`'s sweep on purpose, so the chat model is
/// still resident when `restart` is attempted — otherwise the sweep would
/// already have stopped it and "no stop issued" would be true for the wrong
/// reason.
#[tokio::test]
async fn container_start_and_restart_are_refused_before_anything_is_stopped_while_held() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);

    let mut s = f.state.snapshot().settings.clone();
    s.hold.active = true;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();

    let err = lmgw_core::ops::container(&f.state, None, Some("embed-model"), "start", false, None)
        .await
        .expect_err("start must be refused while held");
    assert!(err.contains("hold"), "the refusal names the hold: {err}");
    assert!(
        f.runs().iter().all(|m| m != "embed-model"),
        "{:?}",
        f.runs()
    );
    assert!(
        f.stops().is_empty(),
        "start must not stop anything: {:?}",
        f.stops()
    );

    let err = lmgw_core::ops::container(&f.state, None, Some("chat-model"), "restart", false, None)
        .await
        .expect_err("restart must be refused while held");
    assert!(err.contains("hold"), "the refusal names the hold: {err}");
    assert!(
        f.stops().is_empty(),
        "restart is refused before its own stop step: {:?}",
        f.stops()
    );
    assert!(
        f.world().loaded.contains("chat-model"),
        "the resident model must still be running"
    );
}

/// `apply` on a running model is not refused — the hold wants it stopped
/// anyway — so it does its useful half (stop) and says plainly that the new
/// configuration lands at the next start, after release.
#[tokio::test]
async fn container_apply_stops_a_running_model_under_hold_and_says_when_it_restarts() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);

    let mut s = f.state.snapshot().settings.clone();
    s.hold.active = true;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();

    let out = lmgw_core::ops::container(&f.state, None, Some("chat-model"), "apply", false, None)
        .await
        .expect("apply stops on purpose — it does not refuse");
    assert_eq!(out["held"], true, "{out}");
    assert_eq!(out["running"], false, "{out}");
    let msg = out["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("takes effect") && msg.contains("released"),
        "{out}"
    );
    assert_eq!(f.stops(), vec!["chat-model".to_string()]);
    assert!(!f.world().loaded.contains("chat-model"));
}

/// `lmgw__local_model_test` resolves through `chat_local_route`, which does no
/// row check and no fallback lookup of its own — so it is refused explicitly,
/// by name, before admission is ever reached; otherwise a test would load the
/// model for real on a card the owner just took back (§2).
#[tokio::test]
async fn the_local_model_test_tool_is_refused_under_hold_and_starts_nothing() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    engage_hold(&f).await;

    let err = lmgw_core::modelinfo::local_model_test(&f.state, "chat-model", None)
        .await
        .expect_err("a test loads the model for real and must be refused under hold");
    assert!(
        err.contains("holding the GPU"),
        "the refusal names the hold: {err}"
    );
    assert!(f.runs().is_empty(), "{:?}", f.runs());
}

/// Releasing the hold puts everything back: the very next request against the
/// same model starts its container again, exactly as if the hold had never
/// engaged.
#[tokio::test]
async fn releasing_the_hold_lets_the_same_request_start_a_container() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    engage_hold(&f).await;
    assert_eq!(chat(&f.gateway).await.status(), 503);
    assert!(f.runs().is_empty());

    lmgw_core::ops::hold_set(&f.state, false).await.unwrap();

    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
}

/// The resolve-time swap (§4) means every downstream check sees the fallback's
/// *real* route — legacy `/v1/completions` requires an openai-protocol
/// upstream, checked against the route the bytes would actually go to. An
/// Anthropic-protocol fallback therefore fails with this handler's own
/// protocol error, exactly as it would if the client had named that alias
/// directly, never a silent misroute to a cloud mock that cannot speak the
/// dialect the handler is about to use.
#[tokio::test]
async fn legacy_completions_under_hold_with_an_anthropic_fallback_is_the_handlers_own_protocol_error(
) {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let claude = MockServer::start().await;
    let up_id = store::insert_upstream(
        &f.state.db,
        &store::NewUpstream {
            name: "claude".into(),
            protocol: lmgw_core::config::Protocol::Anthropic,
            kind: lmgw_core::config::UpstreamKind::Generic,
            base_url: claude.uri(),
            api_key: Some("sk-ant".into()),
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &f.state.db,
        &store::NewAlias {
            alias: "claude-fallback".into(),
            upstream_id: up_id,
            upstream_model_id: "claude-sonnet-5".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();

    let mut s = f.state.snapshot().settings.clone();
    s.hold.fallback_alias = Some("claude-fallback".into());
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    engage_hold(&f).await;

    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/completions", f.gateway))
        .json(&json!({"model": "chat-model", "prompt": "hi"}))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        400,
        "the handler's own protocol refusal, not a 200 and not a misroute"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unsupported", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("openai-protocol"),
        "{body}"
    );
    assert_eq!(
        claude.received_requests().await.unwrap().len(),
        0,
        "never reached the cloud mock"
    );
    assert!(f.runs().is_empty());
}

/// Bonus (§7): a streaming chat response under hold still carries the
/// fallback header — the helper mutates `headers_mut()` on every handler's
/// return, streams included.
#[tokio::test]
async fn a_held_streaming_chat_request_carries_the_fallback_header() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let cloud = cloud_upstream(&f, "cloud-chat").await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                        "data: [DONE]\n\n"
                    ),
                    "text/event-stream",
                ),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&cloud)
        .await;

    let mut s = f.state.snapshot().settings.clone();
    s.hold.fallback_alias = Some("cloud-chat".into());
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    engage_hold(&f).await;

    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/chat/completions", f.gateway))
        .json(&json!({
            "model": "chat-model",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("cloud-chat"),
        "the header survives on the streamed response too"
    );
    assert_eq!(fallback_reason(&resp), Some("hold"));
    assert!(f.runs().is_empty());
}

/// Bonus (§7): `/v1/count_tokens` under hold is served by the fallback and
/// says so — `count_tokens_inner` is one of the fourteen sites that swapped to
/// `resolve_for_request`.
#[tokio::test]
async fn a_held_count_tokens_request_is_served_by_the_fallback_and_says_so() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _cloud = cloud_upstream(&f, "cloud-chat").await;

    let mut s = f.state.snapshot().settings.clone();
    s.hold.fallback_alias = Some("cloud-chat".into());
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    engage_hold(&f).await;

    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/count_tokens", f.gateway))
        .json(&json!({"model": "chat-model", "input": "hello there"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("cloud-chat"),
        "{:?}",
        resp.headers()
    );
    assert_eq!(fallback_reason(&resp), Some("hold"));
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["model"], "chat-model");
    assert!(body["tokens"].as_u64().unwrap_or(0) > 0, "{body}");
    assert!(f.runs().is_empty());
}

/// The Anthropic dialect refuses a held model in *its own* error shape (§2):
/// `{"type":"error","error":{"type":"api_error",...}}` with a 503 — never
/// `overloaded_error`, which is the Anthropic SDKs' retry-with-backoff signal
/// and would have a client hammering a card the owner took away for the
/// evening. Same gate, same status, different envelope: the refusal happens at
/// resolve time, before either ingress has done anything protocol-specific.
#[tokio::test]
async fn a_held_anthropic_messages_request_is_refused_in_the_anthropic_error_shape() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    engage_hold(&f).await;

    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/messages", f.gateway))
        .json(&json!({
            "model": "chat-model",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503, "a hold is a 503 on every ingress");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error", "the Anthropic envelope: {body}");
    assert_eq!(
        body["error"]["type"], "api_error",
        "not overloaded_error — that is the SDKs' retry signal and a hold can last an hour: \
         {body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("holding the GPU"),
        "the message names the hold: {body}"
    );
    assert!(
        f.runs().is_empty(),
        "a held refusal must not start a container: {:?}",
        f.runs()
    );
}

/// A fallback that fails is still a fallback. The hold re-routes the embed
/// request to the cloud alias, the cloud upstream answers 500, and the error
/// response *still* carries `x-lmgw-fallback` — because the body's `model`
/// stays the alias the client asked for, so without the header the owner
/// reading "embed/embed-model returned 502" has nothing at all pointing at the
/// substitution that actually produced it (gpu-hold design §4).
#[tokio::test]
async fn a_fallback_served_request_that_fails_upstream_still_names_the_fallback() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let cloud = cloud_upstream(&f, "cloud-embed").await;
    // Ahead of `cloud_upstream`'s own 200 responder: this is the provider
    // having a bad day *after* the hold already committed to it.
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": {"message": "cloud embeddings are down", "type": "server_error"},
        })))
        .with_priority(1)
        .mount(&cloud)
        .await;

    let aux = f.state.snapshot().aux_models[0].clone();
    store::update_aux_model(
        &f.state.db,
        aux.id,
        &NewAuxModel {
            model_id: aux.model_id.clone(),
            gguf_path: aux.gguf_path.clone(),
            kind: aux.kind,
            pooling: aux.pooling.clone(),
            ctx_size: aux.ctx_size,
            args: aux.args.clone(),
            idle_seconds: aux.idle_seconds,
            enabled: aux.enabled,
            image: aux.image.clone(),
            extra_run_args: aux.extra_run_args.clone(),
            warm_start: aux.warm_start,
            hold_fallback_mode: HoldFallbackMode::Alias,
            hold_fallback: Some("cloud-embed".into()),
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    engage_hold(&f).await;

    let resp = embed(&f.gateway).await;
    assert_eq!(
        resp.status(),
        502,
        "the provider's failure, normalized — not a 200 and not a hold refusal"
    );
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("cloud-embed"),
        "the attribution survives the error path: {:?}",
        resp.headers()
    );
    assert_eq!(fallback_reason(&resp), Some("hold"));
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("cloud embeddings are down"),
        "the provider's own message survives: {body}"
    );
    assert!(f.runs().is_empty(), "{:?}", f.runs());
}

/// A `podman stop` that fails is the one thing a hold sweep cannot swallow.
/// The registry drops the entry regardless of the outcome, so a failed stop
/// leaves a container on the card that lmgw no longer knows about and no later
/// tick will retry — and before this it was reported as nothing at all:
/// `stopped: []`, `draining: []`, "0 container(s) stopped". The owner asked for
/// their GPU back, so they are told exactly which model still has it.
#[tokio::test]
async fn a_stop_that_fails_during_the_hold_sweep_is_reported_as_failed_not_as_nothing() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);

    *f.podman.fail_stop.lock().unwrap() = true;

    let out = engage_hold(&f).await;
    assert_eq!(out["active"], true, "the hold still engages: {out}");
    assert_eq!(
        out["stopped"],
        json!([]),
        "nothing was actually stopped: {out}"
    );
    assert_eq!(
        out["draining"],
        json!([]),
        "a failed stop is not something that drains — nothing retries it: {out}"
    );
    let failed = out["failed"].as_array().expect("a failed bucket").clone();
    assert_eq!(failed.len(), 1, "{out}");
    let entry = failed[0].as_str().unwrap_or_default();
    assert!(
        entry.starts_with("chat/chat-model: "),
        "named the way every other bucket names a model: {out}"
    );
    assert!(
        entry.contains("not stopping"),
        "and carrying podman's own reason: {out}"
    );
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains("could NOT be stopped"),
        "the one-line message a tray click sees must say it too: {out}"
    );

    // The world agrees: the container really is still holding its memory.
    assert!(
        f.world().loaded.contains("chat-model"),
        "the fixture's own model of the GPU still has it resident"
    );
}

/// quickdoc's in-process embedder never takes a fallback's vectors, and never
/// even asks for them (§2: batch and index work is refused, never re-routed).
///
/// This is the corpus-poisoning case in miniature: the local aux model and the
/// cloud fallback here answer with vectors of the *same width*, so every check
/// downstream — count, dims, non-zero — would pass, and a re-embed under hold
/// would leave half a corpus embedded with one model and half with another,
/// unsearchable with no symptom at all. The refusal lands before the call, so
/// the cloud provider is not paid for vectors that are about to be discarded
/// either. Binding to the corpus still works: a hold does not move a pin.
#[tokio::test]
async fn quickdoc_refuses_to_embed_through_a_hold_fallback_and_never_calls_it() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let cloud = cloud_upstream(&f, "cloud-embed").await;

    // The row override is the *only* way an aux model gets a fallback at all,
    // so setting it is what makes this test able to fail: without it the
    // refusal would be the ordinary no-fallback one and would prove nothing
    // about quickdoc.
    let aux = f.state.snapshot().aux_models[0].clone();
    store::update_aux_model(
        &f.state.db,
        aux.id,
        &NewAuxModel {
            model_id: aux.model_id.clone(),
            gguf_path: aux.gguf_path.clone(),
            kind: aux.kind,
            pooling: aux.pooling.clone(),
            ctx_size: aux.ctx_size,
            args: aux.args.clone(),
            idle_seconds: aux.idle_seconds,
            enabled: aux.enabled,
            image: aux.image.clone(),
            extra_run_args: aux.extra_run_args.clone(),
            warm_start: aux.warm_start,
            hold_fallback_mode: HoldFallbackMode::Alias,
            hold_fallback: Some("cloud-embed".into()),
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();

    // Pinned to the local model, at the width the fake container answers with.
    let pinned = EmbedIdentity::new(lmgw_core::config::AUX_UPSTREAM_NAME, "embed-model", 2);
    let corpus_id = qstore::insert_corpus(
        &f.state.corpus,
        &NewCorpus {
            library: "axum".into(),
            version: "0.8".into(),
            status: "ready".into(),
            embed: pinned.clone(),
            ingest_model: "chat-model".into(),
            ingest_prompt_version: "v1".into(),
            crawl_date: String::new(),
            source_kind: "markdown".into(),
        },
    )
    .await
    .unwrap();
    let corpus = qstore::get_corpus(&f.state.corpus, corpus_id)
        .await
        .unwrap()
        .unwrap();

    engage_hold(&f).await;

    let embedder = InProcessEmbedder::for_corpus(f.state.clone(), &corpus)
        .await
        .expect("a hold does not move a corpus's pin — binding still resolves it");
    assert_eq!(
        embedder.identity(),
        pinned,
        "still bound to the local model, not to the fallback"
    );

    let err = embedder
        .embed(&["hello".to_string()])
        .await
        .expect_err("a held embed must be refused, never served by another model");
    let msg = err.to_string();
    assert!(
        msg.contains("holding the GPU"),
        "the refusal names the hold: {msg}"
    );
    assert!(
        msg.contains("pinned"),
        "and why a configured fallback does not save it: {msg}"
    );

    assert_eq!(
        cloud.received_requests().await.unwrap().len(),
        0,
        "not one embedding was bought from the fallback to then be thrown away"
    );
    assert!(
        f.runs().is_empty(),
        "and nothing was started either: {:?}",
        f.runs()
    );

    // The corpus is exactly as it was: still pinned, still readable.
    let after = qstore::get_corpus(&f.state.corpus, corpus_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.embed_identity(), pinned, "the pin is untouched");
}

/// `apply` under a hold reports a *failed* stop as a failure, never as the
/// ordinary "stopped, and starts again at release" (gpu-hold design §2).
///
/// Without a hold the `start_model` that follows a failed stop is the recovery
/// — the next start replaces the container by name — which is why the failure
/// is not fatal there. Under a hold nothing starts, nothing retries, and the
/// container is still holding the GPU the owner asked for back; rounding that
/// up to "was stopped" is the one report that would leave them believing the
/// card is free when it is not. Both shapes of the verb are asserted: the
/// per-model one and the group one, which is the same defect one function
/// over.
#[tokio::test]
async fn apply_under_hold_reports_a_failed_stop_as_a_failure_not_as_held() {
    // Per-model `apply`.
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    // Straight through `save_settings`, bypassing `hold_set`'s sweep on
    // purpose, so the model is still resident when `apply` reaches it.
    let mut s = f.state.snapshot().settings.clone();
    s.hold.active = true;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    *f.podman.fail_stop.lock().unwrap() = true;

    let out = lmgw_core::ops::container(&f.state, None, Some("chat-model"), "apply", false, None)
        .await
        .expect("apply reports, it does not error out");
    assert_eq!(out["ok"], false, "a failed stop is not a success: {out}");
    let msg = out["message"].as_str().unwrap_or_default();
    assert!(msg.contains("could NOT be stopped"), "{out}");
    assert!(
        msg.contains("still be holding GPU memory") && msg.contains("nothing will retry"),
        "and says what that leaves behind: {out}"
    );

    // Group `apply` — same defect, same verdict: the model belongs in
    // `errors`, not in the `held` list that means "stopped, starts at release".
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    let mut s = f.state.snapshot().settings.clone();
    s.hold.active = true;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    *f.podman.fail_stop.lock().unwrap() = true;

    let out = lmgw_core::ops::container(&f.state, Some("all"), None, "apply", false, None)
        .await
        .expect("group apply reports too");
    assert_eq!(
        out["held"],
        json!([]),
        "nothing was actually held down: {out}"
    );
    let errors = out["errors"].as_array().expect("an errors list").clone();
    assert_eq!(errors.len(), 1, "{out}");
    assert_eq!(errors[0]["model_id"], "chat-model", "{out}");
    assert!(
        errors[0]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("stopping it failed"),
        "{out}"
    );
}
