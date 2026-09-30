//! Ladder requests over HTTP: the count beside the send, the climb, the
//! headers and the log column (ladder design §7 items 1, 3, 7 and 13; §12
//! entries 7–11). The ladder fixture's ports play llama-server's context rules
//! (`context_rules`) and name themselves in every answer.

use super::*;

/// A chat request to the ladder model: `n` words of content — the mock counts
/// `n + 1` prompt tokens, the `user:` it renders being one more — asking for
/// `max_tokens`, which the ladder clamps to its `n_predict` of 16.
pub(super) async fn ladder_chat(
    f: &Fixture,
    dialect: Dialect,
    stream: bool,
    n: usize,
    max_tokens: u64,
) -> reqwest::Response {
    let url = match dialect {
        Dialect::OpenAi => format!("{}/v1/chat/completions", f.gateway),
        Dialect::Anthropic => format!("{}/v1/messages", f.gateway),
    };
    f.gateway
        .client()
        .post(url)
        .json(&json!({
            "model": LADDER, "stream": stream, "max_tokens": max_tokens,
            "messages": [{"role": "user", "content": words("w", n)}],
        }))
        .send()
        .await
        .unwrap()
}

pub(super) fn header<'r>(resp: &'r reqwest::Response, name: &str) -> Option<&'r str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

pub(super) fn served_by(port: u16) -> String {
    format!("served by port {port}")
}

/// §7 item 1, in both dialects, unary and streamed: a request that fits the
/// base is clamped, served on rung 1 and says so — `x-lmgw-rung` and the
/// clamp header on the response, `rung = 1` in its log row.
#[tokio::test]
async fn a_request_that_fits_the_base_is_clamped_and_served_on_rung_1() {
    let f = ladder_fixture(16 * GIB, 0).await;
    for (dialect, stream) in [
        (Dialect::OpenAi, false),
        (Dialect::OpenAi, true),
        (Dialect::Anthropic, false),
        (Dialect::Anthropic, true),
    ] {
        let case = format!("{dialect:?}, stream {stream}");
        let before = log_count(&f, LADDER).await;
        // 6 prompt tokens + 16 (100, clamped) = 22 ≤ 64.
        let resp = ladder_chat(&f, dialect, stream, 5, 100).await;
        assert_eq!(resp.status(), 200, "{case}");
        assert_eq!(
            header(&resp, "x-lmgw-rung"),
            Some("1/3; ctx=64; gguf=ladder-base.gguf"),
            "{case}"
        );
        assert_eq!(header(&resp, "x-lmgw-max-tokens-clamped"), Some("16"));
        assert_eq!(header(&resp, "x-lmgw-fallback"), None);
        let text = resp.text().await.unwrap();
        assert!(
            text.contains(&served_by(ladder_view(&f).port)),
            "{case}: {text}"
        );
        let row = newest_log(&f, LADDER, before).await;
        assert_eq!(row.rung, Some(1), "{case}: {row:?}");
        assert_eq!(row.max_tokens_clamped, Some(16), "{case}");
    }
    let port = ladder_view(&f).port;
    {
        let w = f.world();
        let sent: Vec<&Value> = w.chat_bodies[&port]
            .iter()
            .map(|b| &b["max_tokens"])
            .collect();
        assert_eq!(sent, vec![&json!(16); 4], "what llama-server was sent");
        assert_eq!(w.apply_template_calls[&port], 4, "one count per send");
        assert_eq!(w.tokenize_calls[&port], 4);
        assert_eq!(w.truncations, 0);
    }
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf"], "no climb");
    assert_eq!(ladder_view(&f).sends, 0, "every send released");
}

/// §7 item 3: too big for rung 1 and for rung 2 — the model climbs straight
/// to rung 3: one stop, one start, the answer from rung 3's port with `3/3`,
/// and nothing truncated anywhere.
#[tokio::test]
async fn a_request_past_rung_2_climbs_straight_to_rung_3() {
    let f = ladder_fixture(16 * GIB, 0).await;
    let before = log_count(&f, LADDER).await;
    // 201 prompt tokens + 16 = 217: past rung 2's 128, within rung 3's 512.
    let resp = ladder_chat(&f, Dialect::OpenAi, false, 200, 16).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-lmgw-rung"),
        Some("3/3; ctx=512; gguf=ladder-top.gguf")
    );
    let top = ladder_view(&f).port;
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], served_by(top));
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf", "ladder-top.gguf"]);
    assert_eq!(f.stops(), vec![LADDER.to_string()], "one stop, of rung 1");
    assert_eq!(f.world().truncations, 0);
    assert_eq!(newest_log(&f, LADDER, before).await.rung, Some(3));
}

/// §7 item 7, the backstop: the count undercounts, so only llama-server's own
/// refusal catches the request. One climb — to the rung that fits the
/// refusal's own prompt count plus the max output — one retry, and a 200.
#[tokio::test]
async fn an_undercount_is_caught_by_the_backstop_with_one_climb_and_one_retry() {
    let f = ladder_fixture(16 * GIB, 0).await;
    f.world().tokenize_undercount = 30;
    // 71 prompt tokens, counted as 41: 41 + 16 fits the base by the count,
    // but llama-server refuses 71 > 64. 71 + 16 = 87 needs rung 2.
    let resp = ladder_chat(&f, Dialect::OpenAi, false, 70, 16).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-lmgw-rung"),
        Some("2/3; ctx=128; gguf=ladder-mid.gguf")
    );
    let mid = ladder_view(&f).port;
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], served_by(mid));
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf", "ladder-mid.gguf"]);
    let w = f.world();
    assert_eq!(w.exceeded, 1, "refused once, on the base");
    let sends: usize = w.chat_bodies.values().map(Vec::len).sum();
    assert_eq!(sends, 2, "the refused send and its one retry");
}

/// The verdict holds the answer (§12 entry 7): the count takes longer than
/// the send, the running rung answers first — truncated, since the prompt
/// fits its slot but the max output does not — and that answer is dropped
/// when the verdict says "does not fit". The client's answer comes from the
/// rung the model climbed to, never from the one that answered first. Once
/// unary (rung 1 → 2), once streamed (rung 2 → 3).
#[tokio::test]
async fn the_verdict_holds_the_answer_until_the_count_is_in() {
    let f = ladder_fixture(16 * GIB, 0).await;
    assert_eq!(
        ladder_chat(&f, Dialect::OpenAi, false, 3, 16)
            .await
            .status(),
        200
    );
    let base = ladder_view(&f).port;
    f.world()
        .apply_template_delay
        .insert(base, Duration::from_millis(400));

    // 56 prompt tokens ≤ 64: the base takes them and answers at once; 56 + 16
    // = 72 > 64, which only the (slow) count can tell.
    let resp = ladder_chat(&f, Dialect::OpenAi, false, 55, 16).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-lmgw-rung"),
        Some("2/3; ctx=128; gguf=ladder-mid.gguf")
    );
    let mid = ladder_view(&f).port;
    let text = resp.text().await.unwrap();
    assert!(text.contains(&served_by(mid)), "{text}");
    assert!(!text.contains(&served_by(base)), "{text}");
    {
        let w = f.world();
        assert_eq!(w.chat_bodies[&base].len(), 2, "the base did answer it");
        assert_eq!(w.truncations, 1, "…truncated, and that answer was dropped");
    }

    // Streamed: 121 prompt tokens ≤ 128 on rung 2, but 121 + 16 > 128.
    f.world()
        .apply_template_delay
        .insert(mid, Duration::from_millis(400));
    let resp = ladder_chat(&f, Dialect::OpenAi, true, 120, 16).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-lmgw-rung"),
        Some("3/3; ctx=512; gguf=ladder-top.gguf")
    );
    let top = ladder_view(&f).port;
    let text = resp.text().await.unwrap();
    assert!(text.contains(&served_by(top)), "{text}");
    assert!(!text.contains(&served_by(mid)), "{text}");
    let w = f.world();
    assert_eq!(
        w.chat_bodies[&mid].len(),
        2,
        "the first request's retry, then the second's dropped send"
    );
    assert_eq!(w.truncations, 2);
    drop(w);
    assert_eq!(
        ladder_runs(&f),
        vec!["ladder-base.gguf", "ladder-mid.gguf", "ladder-top.gguf"]
    );
}

/// §12 entry 8 over HTTP: a request that needs a higher rung, while VRAM
/// outside lmgw's control leaves no room for it, is answered by the request's
/// fallback with `external_vram` — unary and streamed — without a rung header
/// and with no rung in its log row. The running rung is not touched.
#[tokio::test]
async fn a_climb_short_of_outside_vram_is_answered_by_the_fallback() {
    let f = ladder_fixture(12 * GIB, 0).await;
    f.attribute(6 * GIB);
    let _cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;
    for stream in [false, true] {
        let before = log_count(&f, LADDER).await;
        // Rung 3 needs 8.5 GiB: 12 − 6 outside − 2 (the base) = 4 free, 6
        // with every lmgw model gone.
        let resp = ladder_chat(&f, Dialect::OpenAi, stream, 200, 16).await;
        assert_eq!(resp.status(), 200, "stream {stream}");
        assert_eq!(header(&resp, "x-lmgw-fallback"), Some("cloud-chat"));
        assert_eq!(fallback_reason(&resp), Some("external_vram"));
        assert_eq!(header(&resp, "x-lmgw-rung"), None, "a fallback answered");
        let text = resp.text().await.unwrap();
        assert!(text.contains("from the cloud"), "stream {stream}: {text}");
        let row = newest_log(&f, LADDER, before).await;
        assert_eq!(row.upstream_name.as_deref(), Some("cloud"), "{row:?}");
        assert_eq!(row.fallback_reason.as_deref(), Some("external_vram"));
        assert_eq!(row.rung, None);
        assert_eq!(log_count(&f, LADDER).await, before + 1, "one row");
    }
    assert!(f.stops().is_empty(), "{:?}", f.stops());
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf"]);
    assert!(ladder_view(&f).climbing.is_none());
}

/// Legacy `/v1/completions` on a ladder: its largest prompt is counted with
/// `/tokenize` beside the send, and it climbs like a chat — to rung 2 here —
/// with the rung and the clamp on the response and the log row.
#[tokio::test]
async fn a_legacy_completion_climbs_and_names_the_rung_that_served() {
    let f = ladder_fixture(16 * GIB, 0).await;
    let before = log_count(&f, LADDER).await;
    // 100 prompt tokens + 16 (50, clamped) = 116: rung 2.
    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/completions", f.gateway))
        .json(&json!({"model": LADDER, "prompt": words("p", 100), "max_tokens": 50}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-lmgw-rung"),
        Some("2/3; ctx=128; gguf=ladder-mid.gguf")
    );
    assert_eq!(header(&resp, "x-lmgw-max-tokens-clamped"), Some("16"));
    let mid = ladder_view(&f).port;
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["text"], served_by(mid));
    assert_eq!(f.world().chat_bodies[&mid][0]["max_tokens"], 16);
    let row = newest_log(&f, LADDER, before).await;
    assert_eq!((row.rung, row.max_tokens_clamped), (Some(2), Some(16)));
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf", "ladder-mid.gguf"]);
}

/// The same fallback on the legacy path gets the body as the client sent it:
/// the ladder's clamp was a fact about the local rung, not about the request.
#[tokio::test]
async fn a_legacy_completion_that_falls_back_sends_the_clients_own_body() {
    let f = ladder_fixture(12 * GIB, 0).await;
    f.attribute(6 * GIB);
    let cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;
    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/completions", f.gateway))
        .json(
            &json!({"model": LADDER, "prompt": words("p", 200), "max_tokens": 50,
                      "n_predict": 70}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(fallback_reason(&resp), Some("external_vram"));
    assert_eq!(header(&resp, "x-lmgw-rung"), None);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["text"], "from the cloud");
    let sent: Vec<Value> = cloud
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["model"], "gpt-cloud");
    assert_eq!(sent[0]["max_tokens"], 50, "{}", sent[0]);
    assert_eq!(sent[0]["n_predict"], 70, "{}", sent[0]);
}

/// §7 item 13: a row without a ladder, on the same gateway as one with it,
/// is sent byte for byte as egress renders it — no `/apply-template` or
/// `/tokenize` call, no clamp, no `x-lmgw-rung`, no rung in the log, and no
/// send counted for a drain. The ladder row's send is, for contrast.
#[tokio::test]
async fn a_row_without_a_ladder_is_sent_exactly_as_before() {
    let f = ladder_fixture(16 * GIB, 0).await;
    let chat_port = f.first.address().port();
    f.world()
        .chat_delay
        .insert(chat_port, Duration::from_millis(400));
    let request = json!({
        "model": "chat-model",
        "messages": [{"role": "user", "content": "hello there"}],
        "max_tokens": 5000,
        "n_predict": 7000,
    });
    let in_flight = {
        let gw = f.gateway.clone();
        let request = request.clone();
        tokio::spawn(async move { chat_body(&gw, request).await })
    };
    for _ in 0..500 {
        if f.world().chat_bodies.contains_key(&chat_port) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let view = f
        .state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.model_id == "chat-model")
        .unwrap();
    assert_eq!((view.in_flight, view.sends), (1, 0), "no send counted");
    let resp = in_flight.await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lmgw-rung"), None);
    assert_eq!(header(&resp, "x-lmgw-max-tokens-clamped"), None);
    {
        let w = f.world();
        assert!(w.apply_template_calls.is_empty(), "no count");
        assert!(w.tokenize_calls.is_empty());
        let ir = lmgw_core::ingress::openai::parse_chat_request(&request).unwrap();
        let expected = lmgw_core::egress::openai::chat_body(
            &ir,
            "chat-model",
            &ir.params,
            false,
            lmgw_core::config::UpstreamKind::LlamaServer,
        );
        assert_eq!(w.chat_bodies[&chat_port], vec![expected], "byte for byte");
    }
    assert_eq!(newest_log(&f, "chat-model", 0).await.rung, None);

    // The ladder row's send, in flight, is counted.
    assert_eq!(
        ladder_chat(&f, Dialect::OpenAi, false, 3, 16)
            .await
            .status(),
        200
    );
    let base = ladder_view(&f).port;
    f.world()
        .chat_delay
        .insert(base, Duration::from_millis(400));
    let in_flight = {
        let gw = f.gateway.clone();
        tokio::spawn(async move {
            gw.client()
                .post(format!("{gw}/v1/chat/completions"))
                .json(&json!({"model": LADDER,
                              "messages": [{"role": "user", "content": "hi"}]}))
                .send()
                .await
                .unwrap()
        })
    };
    for _ in 0..500 {
        if f.world().chat_bodies[&base].len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(ladder_view(&f).sends, 1, "a ladder send is counted");
    assert_eq!(in_flight.await.unwrap().status(), 200);
    assert_eq!(ladder_view(&f).sends, 0);
}
