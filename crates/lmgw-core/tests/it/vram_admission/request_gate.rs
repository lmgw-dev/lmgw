//! The request gate: clamp, count and the unified-KV pool ledger
//! (unified-KV design §3.3, §7 items 2 and 14)

use super::*;

/// Edit `chat-model`'s params the way `local_model_set` does: save the row,
/// reload the snapshot, then `lifecycle::stop_for_apply` — which stops an
/// idle container (the next request starts it with the new configuration)
/// and refuses a busy one, which keeps running the configuration it was
/// started with. Returns that sentence, `None` when nothing was running.
pub(super) async fn edit_chat_model(
    f: &Fixture,
    edit: impl FnOnce(&mut lmgw_core::config::LlamaParams),
) -> Option<String> {
    let row = f
        .state
        .snapshot()
        .local_models
        .iter()
        .find(|m| m.model_id == "chat-model")
        .cloned()
        .unwrap();
    let mut params = row.params.clone();
    edit(&mut params);
    store::update_local_model(
        &f.state.db,
        row.id,
        &NewLocalModel {
            model_id: row.model_id,
            gguf_path: row.gguf_path,
            params,
            args: row.args,
            idle_seconds: row.idle_seconds,
            enabled: row.enabled,
            public: row.public,
            image: row.image,
            extra_run_args: row.extra_run_args,
            warm_start: row.warm_start,
            hold_fallback_mode: row.hold_fallback_mode,
            hold_fallback: row.hold_fallback,
            capabilities_override: row.capabilities_override,
            ladder: row.ladder,
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    lmgw_core::runtime::lifecycle::stop_for_apply(
        &f.state,
        lmgw_core::runtime::Class::Chat,
        "chat-model",
    )
    .await
}

/// Turn `chat-model` into a guarded unified-KV row (design §3.3 "When it is
/// active"): unified explicitly on, two slots, a max-output ceiling, and a
/// pool of `pool` tokens. The fixture's weights are not a GGUF, so there is no
/// trained context to read — `ctx_size` is the pool, exactly.
///
/// Applied like a real edit ([`edit_chat_model`]): the guard is a fact of the
/// container's *start* (second review, finding 1), so a model that is already up is
/// restarted by its next request.
pub(super) async fn guard_chat_model(f: &Fixture, pool: i64, n_predict: i64) {
    edit_chat_model(f, |params| {
        params.kv_unified = Some(true);
        params.parallel = Some(2);
        params.ctx_size = Some(pool);
        params.n_predict = Some(n_predict);
        assert!(params.pool_guarded(), "the fixture row must be guarded");
    })
    .await;
}

/// The port `chat-model`'s container answers on right now — the registry's
/// own answer, so a test that restarts the model follows it to its new port.
pub(super) fn chat_port(f: &Fixture) -> u16 {
    f.state
        .runtime()
        .ready_port(lmgw_core::runtime::Class::Chat, "chat-model")
        .expect("chat-model is up")
}

/// Set `vram.queue_timeout_seconds` — the pool wait's bound, and the deferred
/// release's (0: no limit from it).
pub(super) async fn set_queue_timeout(f: &Fixture, secs: u64) {
    let mut s = f.state.snapshot().settings.clone();
    s.vram.queue_timeout_seconds = secs;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// What `chat-model`'s `/slots` reports: a slot processing, or all idle.
pub(super) fn set_slot_busy(f: &Fixture, busy: bool) {
    let mut w = f.world();
    if busy {
        w.busy.insert("chat-model".into());
    } else {
        w.busy.remove("chat-model");
    }
}

/// `n` distinct words starting with `tag`, so a test can size a prompt
/// exactly (the mock counts one token per word, and renders a user message as
/// `"user: <content>"` — one more word) and still tell requests apart in
/// `chat_bodies`.
pub(super) fn words(tag: &str, n: usize) -> String {
    (0..n)
        .map(|i| format!("{tag}{i}"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) async fn chat_body(base: &Gw, body: Value) -> reqwest::Response {
    base.client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// A chat whose reservation is `1 + content words + max_tokens` on the mock.
pub(super) async fn sized_chat(base: &Gw, content: &str, max_tokens: u64) -> reqwest::Response {
    chat_body(
        base,
        json!({
            "model": "chat-model",
            "messages": [{"role": "user", "content": content}],
            "max_tokens": max_tokens,
        }),
    )
    .await
}

pub(super) fn pools(v: &Value) -> Vec<Value> {
    v["kv_pools"].as_array().cloned().unwrap_or_default()
}

/// Poll the (only) pool until `ready` holds, so a test's ordering rests on
/// what the ledger says rather than on a sleep being long enough under load.
pub(super) async fn until_pool(f: &Fixture, what: &str, ready: impl Fn(&Value) -> bool) {
    let mut wait = common::patience::Wait::new(format!("the pool shows {what}"));
    loop {
        let p = pools(&vram_status(&f.gateway).await);
        if p.first().is_some_and(&ready) {
            return;
        }
        wait.again(Some(&Value::from(p))).await;
    }
}

pub(super) fn queued(p: &Value) -> usize {
    p["queue"].as_array().map_or(0, |q| q.len())
}

/// Warm the chat model and put its port into the mock's pool mode, so an
/// overflow is something the mock itself would report (fact 3). Returns the
/// port.
pub(super) async fn warm_pool(f: &Fixture, capacity: u64, delay: Duration) -> u16 {
    assert_eq!(sized_chat(&f.gateway, "hi", 8).await.status(), 200);
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
    let port = f.first.address().port();
    let mut w = f.world();
    w.pool_capacity.insert(port, capacity);
    w.pool_delay.insert(port, delay);
    port
}

/// Two requests of 40 tokens each against a 64-token pool, the second sent
/// while the first is still generating.
async fn overlapping_pair(f: &Fixture) -> (u16, u16, Duration) {
    let a = {
        let gw = f.gateway.clone();
        tokio::spawn(async move { sized_chat(&gw, &words("a", 9), 30).await.status().as_u16() })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    let started = std::time::Instant::now();
    let b = {
        let gw = f.gateway.clone();
        tokio::spawn(async move { sized_chat(&gw, &words("b", 9), 30).await.status().as_u16() })
    };
    let (a, b) = (a.await.unwrap(), b.await.unwrap());
    (a, b, started.elapsed())
}

/// Item 2, the guard's reason to exist: two requests whose reservations
/// together exceed the pool. The second waits, visibly, and starts when the
/// first ends — so the pool never overflows.
#[tokio::test]
async fn a_request_that_would_overflow_the_pool_waits_for_the_one_in_flight() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;
    let port = warm_pool(&f, 64, Duration::from_millis(700)).await;

    // Watch the second one wait, on both status surfaces.
    let a = {
        let gw = f.gateway.clone();
        tokio::spawn(async move { sized_chat(&gw, &words("a", 9), 30).await.status().as_u16() })
    };
    until_pool(&f, "A in flight", |p| p["in_flight"] == 1).await;
    let b = {
        let gw = f.gateway.clone();
        tokio::spawn(async move { sized_chat(&gw, &words("b", 9), 30).await.status().as_u16() })
    };
    until_pool(&f, "B queued", |p| queued(p) == 1).await;

    let v = vram_status(&f.gateway).await;
    let p = pools(&v);
    assert_eq!(p.len(), 1, "one guarded model with activity: {v}");
    assert_eq!(p[0]["model"], "chat-model");
    assert_eq!(p[0]["capacity_tokens"], 64);
    assert_eq!(p[0]["reserved_tokens"], 40, "1 + 9 words + 30 max output");
    assert_eq!(p[0]["in_flight"], 1);
    let queue = p[0]["queue"].as_array().cloned().unwrap_or_default();
    assert_eq!(queue.len(), 1, "the second request is waiting: {v}");
    assert_eq!(queue[0]["position"], 1);
    assert_eq!(queue[0]["alias"], "chat-model");
    assert_eq!(queue[0]["needs_tokens"], 40);
    assert!(queue[0]["waiting_ms"].as_u64().unwrap_or(0) > 0);

    // lmgw__status reads the same view.
    let status = lmgw_core::ops::status(&f.state).await.unwrap();
    assert_eq!(
        status["vram"]["kv_pools"][0]["queue"][0]["needs_tokens"], 40,
        "{status}"
    );

    let (a, b) = (a.await.unwrap(), b.await.unwrap());
    assert_eq!((a, b), (200, 200), "both are served — one after the other");
    // Second review, finding 12: the order is read off the mock's own record of when
    // each request arrived and when its window ended — not off a wall-clock
    // duration, which a loaded machine stretches either way.
    let served = f.world().pool_served[&port].clone();
    assert_eq!(served.len(), 2, "A and B, each served once: {served:?}");
    let (a_ended, b_arrived) = (served[0].1, served[1].0);
    assert!(
        b_arrived >= a_ended,
        "B reached the model only after A's window had ended"
    );
    assert_eq!(f.world().pool_overflows, 0, "the pool never overflowed");

    // Both reservations were released with their responses.
    assert!(pools(&vram_status(&f.gateway).await).is_empty());
}

/// The same pair against the same pool, unguarded: the mock's overflow fires,
/// which is what proves the test above bites.
#[tokio::test]
async fn the_same_pair_overflows_an_unguarded_pool() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    warm_pool(&f, 64, Duration::from_millis(700)).await;

    let (a, b, _) = overlapping_pair(&f).await;
    assert!(
        a == 502 || b == 502,
        "one of them died of the overflow: {a} {b}"
    );
    assert!(f.world().pool_overflows >= 1);
    assert!(
        f.world().apply_template_calls.is_empty(),
        "and nothing counted anything on the unguarded row"
    );
}

/// Item 2, FIFO: a small request that would fit now does not overtake a large
/// one that is already waiting — so a large request can never starve.
///
/// Pool 64: A (40) is generating; B (50) waits, since 40 + 50 > 64; C (20)
/// would fit beside A, but B is ahead of it. When A ends, B goes; C still
/// waits (50 + 20 > 64) and goes when B ends.
#[tokio::test]
async fn a_small_request_never_overtakes_a_waiting_large_one() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;
    let port = warm_pool(&f, 64, Duration::from_millis(500)).await;

    let send = |content: String, max: u64| {
        let gw = f.gateway.clone();
        tokio::spawn(async move { sized_chat(&gw, &content, max).await.status().as_u16() })
    };
    let a = send(words("a", 9), 30);
    until_pool(&f, "A in flight", |p| p["in_flight"] == 1).await;
    let b = send(words("b", 19), 30);
    until_pool(&f, "B queued", |p| queued(p) == 1).await;
    let c = send(words("c", 9), 10);
    until_pool(&f, "C queued", |p| queued(p) == 2).await;

    let v = vram_status(&f.gateway).await;
    let queue = pools(&v)[0]["queue"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let needs: Vec<u64> = queue
        .iter()
        .map(|w| w["needs_tokens"].as_u64().unwrap())
        .collect();
    assert_eq!(needs, vec![50, 20], "both wait, the large one first: {v}");

    assert_eq!(a.await.unwrap(), 200);
    assert_eq!(b.await.unwrap(), 200);
    assert_eq!(c.await.unwrap(), 200);

    let order: Vec<String> = f.world().chat_bodies[&port]
        .iter()
        .map(|b| {
            b["messages"][0]["content"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(1)
                .collect()
        })
        .collect();
    assert_eq!(
        order,
        vec!["h", "a", "b", "c"],
        "the mock saw them in arrival order — C never jumped B"
    );
    assert_eq!(f.world().pool_overflows, 0);
}

/// Item 2, timeout: a request that cannot get room within
/// `vram.queue_timeout_seconds` gets the same 503 and code as a VRAM queue
/// timeout, naming the pool's numbers — and leaves the queue. In both
/// dialects: Anthropic's is `overloaded_error`, its SDKs' own retry signal
/// (second review, finding 12).
///
/// The timeout is 1 s and A generates for 3 s, so the two waiters' refusals
/// land two seconds before A could free anything — the margin a loaded
/// machine has to eat before the test's premise breaks, not a wall-clock
/// assertion.
#[tokio::test]
async fn a_pool_wait_past_the_queue_timeout_is_a_named_503_in_both_dialects() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;
    warm_pool(&f, 64, Duration::from_millis(3_000)).await;
    set_queue_timeout(&f, 1).await;

    let a = {
        let gw = f.gateway.clone();
        tokio::spawn(async move { sized_chat(&gw, &words("a", 9), 30).await.status().as_u16() })
    };
    until_pool(&f, "A in flight", |p| p["in_flight"] == 1).await;
    let started = std::time::Instant::now();
    let anthropic = f
        .gateway
        .client()
        .post(format!("{}/v1/messages", f.gateway))
        .json(&json!({
            "model": "chat-model",
            "max_tokens": 30,
            "messages": [{"role": "user", "content": words("c", 9)}],
        }))
        .send();
    let b_words = words("b", 9);
    let (openai, anthropic) = tokio::join!(sized_chat(&f.gateway, &b_words, 30), anthropic);
    let anthropic = anthropic.unwrap();
    // A lower bound only: the timeout never fires early.
    assert!(started.elapsed() >= Duration::from_secs(1));

    assert_eq!(openai.status(), 503);
    let body: Value = openai.json().await.unwrap();
    assert_eq!(body["error"]["code"], "vram_queue_timeout", "{body}");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("shared KV pool") && msg.contains("40 tokens") && msg.contains("64"),
        "the refusal names the pool and its numbers: {msg}"
    );

    assert_eq!(anthropic.status(), 503);
    let body: Value = anthropic.json().await.unwrap();
    assert_eq!(body["type"], "error", "{body}");
    assert_eq!(body["error"]["type"], "overloaded_error", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("shared KV pool"),
        "{body}"
    );

    let v = vram_status(&f.gateway).await;
    assert!(
        pools(&v)
            .iter()
            .all(|p| p["queue"].as_array().is_none_or(|q| q.is_empty())),
        "the refused requests left the queue: {v}"
    );
    assert_eq!(
        a.await.unwrap(),
        200,
        "the one in flight was never disturbed"
    );
    assert!(pools(&vram_status(&f.gateway).await).is_empty());
}

/// Item 2, above the per-request limit: `400 context_length_exceeded` in both
/// dialects, before anything reaches the model — and no container beyond the
/// one the count needed.
#[tokio::test]
async fn a_request_above_the_per_request_limit_is_a_400_in_both_dialects() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;

    // 1 + 40 words + 30 = 71 > 64.
    let resp = sized_chat(&f.gateway, &words("x", 40), 30).await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "context_length_exceeded", "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("chat-model")
            && msg.contains("41")
            && msg.contains("30")
            && msg.contains("64"),
        "prompt, max output and the limit are all named: {msg}"
    );

    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/messages", f.gateway))
        .json(&json!({
            "model": "chat-model",
            "max_tokens": 30,
            "messages": [{"role": "user", "content": words("x", 40)}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error", "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");

    assert_eq!(
        f.runs(),
        vec!["chat-model".to_string()],
        "the count needed the model up; nothing else was started"
    );
    let port = f.first.address().port();
    assert!(
        f.world()
            .chat_bodies
            .get(&port)
            .is_none_or(|b| b.is_empty()),
        "nothing was sent to generate"
    );
    assert_eq!(
        f.world().apply_template_calls[&port],
        2,
        "both were counted"
    );
    assert!(pools(&vram_status(&f.gateway).await).is_empty());
}

/// The clamp (ladder design §3.2 / unified-KV design §3.3 step 1): a client
/// value above `n_predict` is lowered and says so — header and request log —
/// a value below is kept without a word, a missing one is filled, and a raw
/// `n_predict` in the body is bound the same way.
#[tokio::test]
async fn the_clamp_lowers_reports_keeps_and_binds() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 4096, 32).await;
    let clamped_header = |r: &reqwest::Response| {
        r.headers()
            .get("x-lmgw-max-tokens-clamped")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };

    let above = sized_chat(&f.gateway, "above", 100).await;
    assert_eq!(above.status(), 200);
    assert_eq!(clamped_header(&above).as_deref(), Some("32"));

    let below = sized_chat(&f.gateway, "below", 20).await;
    assert_eq!(below.status(), 200);
    assert_eq!(
        clamped_header(&below),
        None,
        "a value under the ceiling is kept"
    );

    let missing = chat_body(
        &f.gateway,
        json!({"model": "chat-model", "messages": [{"role": "user", "content": "missing"}]}),
    )
    .await;
    assert_eq!(missing.status(), 200);
    assert_eq!(clamped_header(&missing), None, "filling is not lowering");

    let raw = chat_body(
        &f.gateway,
        json!({
            "model": "chat-model",
            "messages": [{"role": "user", "content": "raw"}],
            "n_predict": 1000,
        }),
    )
    .await;
    assert_eq!(raw.status(), 200);
    assert_eq!(clamped_header(&raw).as_deref(), Some("32"));

    let port = f.first.address().port();
    let bodies = f.world().chat_bodies[&port].clone();
    let sent: Vec<(Value, Value)> = bodies
        .iter()
        .map(|b| (b["max_tokens"].clone(), b["n_predict"].clone()))
        .collect();
    assert_eq!(
        sent,
        vec![
            (json!(32), Value::Null),
            (json!(20), Value::Null),
            (json!(32), Value::Null),
            (json!(32), Value::Null),
        ],
        "what llama-server was actually sent — no raw n_predict past the clamp"
    );

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
    let mut clamped: Vec<Option<i64>> = logs.iter().map(|r| r.max_tokens_clamped).collect();
    clamped.reverse(); // newest first → request order
    assert_eq!(clamped, vec![Some(32), None, None, Some(32)]);
}

/// Review finding 7 ("clamp-then-refuse is reported"): a request whose
/// `max_tokens` the clamp lowered, but whose reservation is then still
/// refused (too big even at the clamped ceiling), must still carry
/// `x-lmgw-max-tokens-clamped` and log the same value — the clamp is a fact
/// about what the client asked for, independent of whether the request that
/// followed it was ultimately let through. Before this fix, `fit_chat`
/// returning an `Err` discarded the lease (and the clamp it carried) outright.
#[tokio::test]
async fn a_refusal_after_the_clamp_still_reports_it() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;

    // 1000 is clamped down to n_predict (32); 1 + 40 words + 32 = 73 > 64:
    // the reservation still refuses it, but only after the clamp already ran.
    let resp = sized_chat(&f.gateway, &words("x", 40), 1000).await;
    assert_eq!(resp.status(), 400);
    let clamped = resp
        .headers()
        .get("x-lmgw-max-tokens-clamped")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    assert_eq!(
        clamped.as_deref(),
        Some("32"),
        "the response must still report the clamp: {resp:?}"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "context_length_exceeded", "{body}");

    let logs = store::query_logs(
        &f.state.db,
        &store::LogFilter {
            alias: Some("chat-model".into()),
            limit: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        logs[0].max_tokens_clamped,
        Some(32),
        "the request-log row must also record it"
    );
}

/// Counting (item 2's precondition) and item 14: a guarded row is counted on
/// the running server, one `/apply-template` and one `/tokenize` per send; an
/// unguarded row makes neither call, gets exactly the body plain egress
/// renders — a raw `n_predict` included — and no clamp header.
#[tokio::test]
async fn only_a_guarded_row_is_counted_and_an_unguarded_body_is_untouched() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let request = json!({
        "model": "chat-model",
        "messages": [{"role": "user", "content": "hello there"}],
        "max_tokens": 5000,
        "n_predict": 7000,
        "temperature": 0.5,
    });

    let resp = chat_body(&f.gateway, request.clone()).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-lmgw-max-tokens-clamped").is_none());
    let port = f.first.address().port();
    {
        let w = f.world();
        assert!(
            w.apply_template_calls.is_empty(),
            "no count on an unguarded row"
        );
        assert!(w.tokenize_calls.is_empty());
        let ir = lmgw_core::ingress::openai::parse_chat_request(&request).unwrap();
        let expected = lmgw_core::egress::llama_cpp::chat_body(
            &ir,
            "chat-model",
            &ir.params,
            false,
            &lmgw_core::config::Snapshot::default().router_upstream(),
        );
        assert_eq!(
            w.chat_bodies[&port][0], expected,
            "byte for byte what egress renders without the gate"
        );
        assert_eq!(w.chat_bodies[&port][0]["n_predict"], 7000);
        assert_eq!(w.chat_bodies[&port][0]["max_tokens"], 5000);
    }

    // The edit applies like a real one: the idle container is stopped, and
    // the guard is a fact of the next start (second review, finding 1).
    guard_chat_model(&f, 100_000, 64).await;
    let resp = chat_body(&f.gateway, request).await;
    assert_eq!(resp.status(), 200);
    let restarted = chat_port(&f);
    assert_ne!(restarted, port, "the edit restarted the model");
    let w = f.world();
    assert_eq!(w.apply_template_calls[&restarted], 1);
    assert_eq!(w.tokenize_calls[&restarted], 1);
    assert_eq!(w.chat_bodies[&restarted][0]["max_tokens"], 64);
    assert!(w.chat_bodies[&restarted][0].get("n_predict").is_none());
}

/// Release path: an upstream error. The failed request's reservation goes
/// with it, so a request that needs almost the whole pool is served straight
/// after — it would wait out the timeout behind a leak.
#[tokio::test]
async fn an_upstream_error_releases_the_reservation() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;
    assert_eq!(sized_chat(&f.gateway, "hi", 8).await.status(), 200);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": {"code": 500, "message": "boom", "type": "server_error"}
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&f.first)
        .await;

    let resp = sized_chat(&f.gateway, &words("a", 9), 30).await;
    assert_eq!(resp.status(), 502);
    assert!(pools(&vram_status(&f.gateway).await).is_empty());

    // 1 + 29 + 30 = 60 of 64: served, where a leaked 40 would have left it
    // waiting out the queue timeout into a 503 — the status is the proof, no
    // wall clock needed (second review, finding 12).
    assert_eq!(
        sized_chat(&f.gateway, &words("b", 29), 30).await.status(),
        200
    );
}

/// llama-server's own context refusal (the backstop, when a count was short):
/// the client sees `context_length_exceeded` naming the model, not an empty
/// name and not a generic upstream error.
#[tokio::test]
async fn llama_servers_own_context_refusal_names_the_model() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"code": 400, "message": "the request exceeds the available context size",
                      "type": "exceed_context_size_error", "n_prompt_tokens": 8010, "n_ctx": 4096}
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&f.first)
        .await;

    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "context_length_exceeded", "{body}");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.starts_with("'chat-model': prompt 8010"), "{msg}");
}

/// A streaming chat of `1 + content words + max_tokens` whose client hangs
/// up mid-answer: the mock streams far more than the socket buffers between
/// here and the client hold, so the relay is still writing when the client
/// goes and notices on its next write. Mounted once, on the first container.
pub(super) async fn stream_and_hang_up(f: &Fixture, content: &str, max_tokens: u64) {
    assert_eq!(
        chat_port(f),
        f.first.address().port(),
        "chat-model is on the first container"
    );
    let event = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n";
    let long: String = event.repeat(200_000);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(long, "text/event-stream"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&f.first)
        .await;

    let resp = chat_body(
        &f.gateway,
        json!({
            "model": "chat-model",
            "messages": [{"role": "user", "content": content}],
            "max_tokens": max_tokens,
            "stream": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    drop(resp);
}

/// Release path: a streaming client that disconnects mid-stream. The relay
/// notices on its next write; the reservation goes once the container's
/// `/slots` shows the abandoned slot let go — here at once, since the mock's
/// slot reads idle (the busy case is the next test).
#[tokio::test]
async fn a_streaming_client_that_disconnects_releases_the_reservation() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;
    assert_eq!(sized_chat(&f.gateway, "hi", 8).await.status(), 200);

    stream_and_hang_up(&f, &words("a", 9), 30).await;

    let mut released = false;
    for _ in 0..100 {
        if pools(&vram_status(&f.gateway).await).is_empty() {
            released = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(released, "the disconnect released the reservation");

    // …and it really was the disconnect that ended it.
    let logs = store::query_logs(
        &f.state.db,
        &store::LogFilter {
            alias: Some("chat-model".into()),
            limit: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        logs[0].error_kind.as_deref(),
        Some("canceled"),
        "{:?}",
        logs[0]
    );

    // 1 + 29 + 30 = 60 of 64: served, not left behind a leak until the
    // queue timeout's 503.
    assert_eq!(
        sized_chat(&f.gateway, &words("b", 29), 30).await.status(),
        200
    );
}

/// Release path: a client that disconnects while *waiting* for room leaves the
/// queue at once and never reaches the model. The pattern of
/// `a_disconnected_client_leaves_the_queue`, for the pool.
///
/// Deterministic (second review, finding 12): the queue timeout is off, so the
/// disconnect is the only way out of the queue; and the reservation ahead is
/// held by the test itself — a hung-up stream whose slot the mock keeps
/// reporting busy stays reserved until the test flips it idle — instead of by
/// a generation that has to outlast the test's own steps.
#[tokio::test]
async fn a_client_that_disconnects_while_waiting_leaves_the_pool_queue() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;
    assert_eq!(sized_chat(&f.gateway, "hi", 8).await.status(), 200);
    let port = chat_port(&f);
    set_queue_timeout(&f, 0).await;

    set_slot_busy(&f, true);
    stream_and_hang_up(&f, &words("a", 9), 30).await;
    until_pool(&f, "A releasing", |p| p["releasing"] == 1).await;

    let waiting = {
        let gw = f.gateway.clone();
        tokio::spawn(async move { sized_chat(&gw, &words("b", 9), 30).await.status() })
    };
    // The wait must be queued before the client goes away.
    until_pool(&f, "B queued", |p| queued(p) == 1).await;

    waiting.abort();
    let _ = waiting.await;
    until_pool(&f, "B gone from the queue", |p| queued(p) == 0).await;
    let v = vram_status(&f.gateway).await;
    assert_eq!(
        pools(&v)[0]["reserved_tokens"],
        40,
        "and it took nothing with it: {v}"
    );

    set_slot_busy(&f, false);
    until_pools_empty(&f).await;
    assert_eq!(
        f.world().chat_bodies[&port].len(),
        1,
        "only the warm-up was generated — the cancelled request never reached the model"
    );
}

/// Poll until no pool has any activity left.
pub(super) async fn until_pools_empty(f: &Fixture) {
    let mut wait = common::patience::Wait::new("every reservation goes");
    loop {
        let v = vram_status(&f.gateway).await;
        if pools(&v).is_empty() {
            return;
        }
        wait.again(Some(&v)).await;
    }
}

/// An in-process loop takes its lease per turn, not per hold: Admin Chat runs
/// two turns on one admission, each counted on its own, and while the second
/// is generating only the second is in the pool.
///
/// Deterministic (second review, finding 12): what the pool held is read by the mock
/// itself, at the instant turn 2 reaches the model — not by the test racing a
/// generation window, which a loaded machine can miss either way.
#[tokio::test]
async fn an_in_process_loop_takes_a_lease_per_turn_and_releases_between_turns() {
    use lmgw_core::config::{SelfAdmin, Settings};

    let call = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",",
        "\"type\":\"function\",\"function\":{\"name\":\"lmgw__status\",\"arguments\":\"\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
        "\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let answer = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"all good\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );

    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    // Admin Chat's system prompt is long; the pool is not what is under test.
    guard_chat_model(&f, 100_000, 1_000).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(call, "text/event-stream"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&f.first)
        .await;
    // Turn 2's arrival: what the ledger, the registry and the counter say at
    // that instant. The reservation is granted before the send, so it is in
    // the view; turn 1's must already be gone.
    type AtTurnTwo = (
        Vec<lmgw_core::gate::pool::KvPoolView>,
        Vec<lmgw_core::runtime::registry::RuntimeView>,
        u64,
    );
    let at_turn_two: Arc<Mutex<Option<AtTurnTwo>>> = Arc::default();
    {
        let (state, world, seen) = (f.state.clone(), f.world.clone(), at_turn_two.clone());
        let port = f.first.address().port();
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |_: &Request| {
                let counted = world
                    .lock()
                    .unwrap()
                    .apply_template_calls
                    .get(&port)
                    .copied()
                    .unwrap_or(0);
                *seen.lock().unwrap() =
                    Some((state.kv_pools.view(), state.runtime().list(), counted));
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(answer, "text/event-stream")
            })
            .up_to_n_times(1)
            .with_priority(2)
            .mount(&f.first)
            .await;
    }
    let s = Settings {
        self_admin: SelfAdmin::ReadOnly,
        ..f.state.snapshot().settings.clone()
    };
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();

    let tid = f
        .gateway
        .client()
        .post(format!("{}/chat/api/threads", f.gateway))
        .json(&json!({"model_alias": "chat-model", "kind": "admin"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let body = f
        .gateway
        .client()
        .post(format!("{}/chat/api/threads/{tid}/send", f.gateway))
        .json(&json!({"content": "how is the gateway doing?"}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("all good"), "the loop finished: {body}");

    let (pools_then, resident_then, counted) = at_turn_two
        .lock()
        .unwrap()
        .clone()
        .expect("turn 2 reached the model");
    assert_eq!(counted, 2, "one count per turn, both done by turn 2's send");
    assert_eq!(pools_then.len(), 1, "{pools_then:?}");
    assert_eq!(
        (pools_then[0].in_flight, pools_then[0].releasing),
        (1, 0),
        "only turn 2 holds a reservation — turn 1's went with its response: {pools_then:?}"
    );
    let chat = resident_then
        .iter()
        .find(|r| r.model_id == "chat-model")
        .expect("chat-model is resident");
    assert_eq!(
        chat.in_flight, 1,
        "while the loop's one admission spans both"
    );
    assert!(
        pools(&vram_status(&f.gateway).await).is_empty(),
        "nothing is left reserved after the loop"
    );
}
