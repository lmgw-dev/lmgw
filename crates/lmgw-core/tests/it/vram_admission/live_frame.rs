//! The live frame (second review, finding 11)

use super::*;

/// Wait for a `vram` frame whose `kv_pools` satisfies `ready`.
async fn until_frame(
    rx: &mut tokio::sync::broadcast::Receiver<lmgw_core::telemetry::Event>,
    what: &str,
    ready: impl Fn(&[lmgw_core::gate::pool::KvPoolView]) -> bool,
) {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
            Ok(Ok(lmgw_core::telemetry::Event::Vram(v))) if ready(&v.kv_pools) => return,
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(e)) => panic!("the live bus closed waiting for {what}: {e}"),
            Err(_) => panic!("no vram frame ever showed {what}"),
        }
    }
}

/// The live `vram` frame follows every ledger change — a grant and a release
/// with no queue anywhere, a deferral — not only a queue that forms.
#[tokio::test]
async fn the_live_frame_follows_every_grant_release_and_deferral() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;
    warm_pool(&f, 64, Duration::from_millis(2_000)).await;
    let mut rx = f.state.telemetry.subscribe();

    // A grant with nobody waiting, then its release.
    let a = {
        let gw = f.gateway.clone();
        tokio::spawn(async move { sized_chat(&gw, &words("a", 9), 30).await.status().as_u16() })
    };
    until_frame(&mut rx, "the grant", |p| {
        p.len() == 1 && p[0].in_flight == 1 && p[0].queue.is_empty()
    })
    .await;
    assert_eq!(a.await.unwrap(), 200);
    until_frame(&mut rx, "the release", |p| p.is_empty()).await;

    // A deferral, then its release.
    set_queue_timeout(&f, 0).await;
    set_slot_busy(&f, true);
    stream_and_hang_up(&f, &words("b", 9), 30).await;
    until_frame(&mut rx, "the deferral", |p| {
        p.len() == 1 && p[0].releasing == 1 && p[0].in_flight == 0
    })
    .await;
    set_slot_busy(&f, false);
    until_frame(&mut rx, "the deferred release", |p| p.is_empty()).await;
}

/// Under the GPU hold a guarded row falls back exactly as before: the cloud
/// answers, and nothing was counted — no container is up to count on.
#[tokio::test]
async fn a_held_guarded_row_falls_back_without_counting() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;
    let _cloud = cloud_upstream(&f, "cloud-chat").await;
    let mut s = f.state.snapshot().settings.clone();
    s.hold.fallback_alias = Some("cloud-chat".into());
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    engage_hold(&f).await;

    let resp = sized_chat(&f.gateway, &words("x", 40), 100).await;
    assert_eq!(
        resp.status(),
        200,
        "no per-request limit applies to the fallback"
    );
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("cloud-chat")
    );
    assert_eq!(fallback_reason(&resp), Some("hold"));
    assert!(resp.headers().get("x-lmgw-max-tokens-clamped").is_none());
    assert!(f.world().apply_template_calls.is_empty());
    assert!(f.world().tokenize_calls.is_empty());
    assert!(f.runs().is_empty());
}

/// Legacy `/v1/completions` on a guarded row: `/tokenize` alone (no template),
/// every spelling of the limit folded into one clamped `max_tokens`, and the
/// clamp header on the response.
#[tokio::test]
async fn legacy_completions_on_a_guarded_row_are_counted_and_clamped() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 4096, 32).await;
    assert_eq!(sized_chat(&f.gateway, "hi", 8).await.status(), 200);
    let received: Arc<Mutex<Vec<Value>>> = Arc::default();
    let log = received.clone();
    Mock::given(method("POST"))
        .and(path("/v1/completions"))
        .respond_with(move |req: &Request| {
            log.lock()
                .unwrap()
                .push(serde_json::from_slice(&req.body).unwrap_or_default());
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "t1", "object": "text_completion", "created": 1, "model": "chat-model",
                "choices": [{"index": 0, "text": "ok", "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4},
            }))
        })
        .mount(&f.first)
        .await;
    let port = f.first.address().port();
    let tokenized_before = f.world().tokenize_calls[&port];
    let templated_before = f.world().apply_template_calls[&port];

    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/completions", f.gateway))
        .json(
            &json!({"model": "chat-model", "prompt": "one two three", "n_predict": 999,
                      "max_tokens": 5}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-max-tokens-clamped")
            .and_then(|v| v.to_str().ok()),
        Some("32"),
        "n_predict is what llama-server would have honoured, so it is what was lowered"
    );
    let sent = received.lock().unwrap()[0].clone();
    assert_eq!(sent["max_tokens"], 32, "{sent}");
    assert!(sent.get("n_predict").is_none(), "{sent}");
    assert_eq!(f.world().tokenize_calls[&port], tokenized_before + 1);
    assert_eq!(
        f.world().apply_template_calls[&port],
        templated_before,
        "a raw prompt has no template to render"
    );
    assert!(pools(&vram_status(&f.gateway).await).is_empty());
}

/// `/v1/responses` on a guarded row: its headers leave before the first turn,
/// so the clamp header is the plan every turn follows — and the turn itself
/// really is clamped, and logs it.
#[tokio::test]
async fn responses_on_a_guarded_row_stamp_and_log_the_clamp() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 4096, 32).await;

    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/responses", f.gateway))
        .json(&json!({"model": "chat-model", "input": "hi there", "max_output_tokens": 100}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-max-tokens-clamped")
            .and_then(|v| v.to_str().ok()),
        Some("32")
    );
    let port = f.first.address().port();
    assert_eq!(f.world().chat_bodies[&port][0]["max_tokens"], 32);
    assert_eq!(f.world().apply_template_calls[&port], 1);

    let logs = store::query_logs(
        &f.state.db,
        &store::LogFilter {
            alias: Some("chat-model".into()),
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        logs.iter().any(|r| r.max_tokens_clamped == Some(32)),
        "the turn's own log row carries the clamp: {logs:?}"
    );
    assert!(pools(&vram_status(&f.gateway).await).is_empty());
}
