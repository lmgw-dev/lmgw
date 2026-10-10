//! WP4: the remaining request-level cases (ladder design §7 items 2, 5, 6, 8,
//! 9; §12 entries 8–10, 18, 20, 23). Item 3 (climbs straight to rung 3), item
//! 7 (the backstop) and item 13 (byte-identical without a ladder) are proven
//! in `ladder_http.rs`, item 12 (the ledger) in `ladder_admission.rs`; the
//! streamed climb of "the verdict holds the answer" (also `ladder_http.rs`)
//! already shows no byte from the old rung reaching the client, and
//! `request_logs.rung`'s other three values (base, climbed, fallback) are
//! pinned by the tests in `ladder_http.rs` too — only the refused-above-the-top
//! value is new, in `a_prompt_too_big_for_the_top_rung_is_a_400_naming_it_in_both_dialects`.

use super::*;

/// §7 item 2: a client `max_tokens` below the cap is kept, and it is what the
/// fit is judged against — a prompt that would have climbed at the cap (16)
/// stays on rung 1 at the client's own, lower value, and nothing is stamped
/// clamped (only a *lowered* value gets the header, §12 entry 3).
#[tokio::test]
async fn a_client_max_tokens_below_the_cap_is_kept_and_used_for_the_fit() {
    let f = ladder_fixture(16 * GIB, 0).await;
    let before = log_count(&f, LADDER).await;
    // 51 prompt tokens + 5 (kept) = 56 ≤ 64: stays on rung 1. The cap (16)
    // would have needed 67 > 64 and climbed.
    let resp = ladder_chat(&f, Dialect::OpenAi, false, 50, 5).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-lmgw-rung"),
        Some("1/3; ctx=64; gguf=ladder-base.gguf")
    );
    assert_eq!(
        header(&resp, "x-lmgw-max-tokens-clamped"),
        None,
        "kept, not lowered"
    );
    let port = ladder_view(&f).port;
    assert_eq!(
        f.world().chat_bodies[&port].last().unwrap()["max_tokens"],
        5
    );
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf"], "no climb");
    assert_eq!(newest_log(&f, LADDER, before).await.rung, Some(1));
}

/// §7 item 4: a send in flight on rung 1 (held by `chat_delay`) when another
/// request triggers a climb finishes normally on rung 1's own port; the
/// trigger and a short request that arrives mid-climb are both served on the
/// new rung — one stop, one start (ladder design §3.1, §3.4 step 2).
#[tokio::test]
async fn a_send_in_flight_on_rung_1_finishes_there_while_the_trigger_and_a_queued_request_climb() {
    let f = ladder_fixture(16 * GIB, 0).await;
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

    // A: 4 prompt tokens + 16 = 20 ≤ 64, but its answer is held by the delay.
    let a = {
        let gw = f.gateway.clone();
        tokio::spawn(async move {
            chat_body(
                &gw,
                json!({"model": LADDER, "max_tokens": 16,
                       "messages": [{"role": "user", "content": words("a", 3)}]}),
            )
            .await
        })
    };
    for _ in 0..500 {
        if ladder_view(&f).sends >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(ladder_view(&f).sends, 1, "A is in flight on rung 1");

    // B: the trigger — 201 prompt tokens + 16 needs rung 3, past rung 2.
    let b = {
        let gw = f.gateway.clone();
        tokio::spawn(async move {
            chat_body(
                &gw,
                json!({"model": LADDER, "max_tokens": 16,
                       "messages": [{"role": "user", "content": words("b", 200)}]}),
            )
            .await
        })
    };
    for _ in 0..500 {
        if ladder_view(&f).climbing.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(ladder_view(&f).climbing.is_some(), "B marked the climb");

    // C: a short request, arriving while the mark is up.
    let c = {
        let gw = f.gateway.clone();
        tokio::spawn(async move {
            chat_body(
                &gw,
                json!({"model": LADDER, "max_tokens": 16,
                       "messages": [{"role": "user", "content": words("c", 3)}]}),
            )
            .await
        })
    };
    // Generous margin, commented: A's own 400ms delay is what keeps the
    // drain — and so the mark — open long enough for this to be reliable.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!c.is_finished(), "parked behind the climb, not raced");

    let resp_a = a.await.unwrap();
    assert_eq!(resp_a.status(), 200);
    let text_a = resp_a.text().await.unwrap();
    assert!(
        text_a.contains(&served_by(base)),
        "A finished on rung 1: {text_a}"
    );

    let resp_b = b.await.unwrap();
    assert_eq!(resp_b.status(), 200);
    let top = ladder_view(&f).port;
    assert_eq!(
        header(&resp_b, "x-lmgw-rung"),
        Some("3/3; ctx=512; gguf=ladder-top.gguf")
    );
    let text_b = resp_b.text().await.unwrap();
    assert!(text_b.contains(&served_by(top)), "{text_b}");

    let resp_c = c.await.unwrap();
    assert_eq!(resp_c.status(), 200);
    let text_c = resp_c.text().await.unwrap();
    assert!(text_c.contains(&served_by(top)), "{text_c}");

    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf", "ladder-top.gguf"]);
    assert_eq!(
        f.stops(),
        vec![LADDER.to_string()],
        "exactly one stop, of rung 1"
    );
}

/// §7 item 5: a prompt too big even for the top rung is a `400` in both
/// dialects, naming the prompt tokens, the max output and the top rung's
/// per-slot context (§3.1) — judged on the rung that was running (the base,
/// here), so `x-lmgw-rung` and `request_logs.rung` both still say `1` even on
/// the refusal (§12 entry 36), and no podman call happens past the base.
#[tokio::test]
async fn a_prompt_too_big_for_the_top_rung_is_a_400_naming_it_in_both_dialects() {
    let f = ladder_fixture(16 * GIB, 0).await;

    let before = log_count(&f, LADDER).await;
    // 601 prompt tokens + 16 = 617: past rung 3's 512, so nothing holds it.
    let resp = ladder_chat(&f, Dialect::OpenAi, false, 600, 16).await;
    assert_eq!(resp.status(), 400);
    assert_eq!(
        header(&resp, "x-lmgw-rung"),
        Some("1/3; ctx=64; gguf=ladder-base.gguf"),
        "judged on the rung that was running"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "context_length_exceeded", "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("601") && msg.contains("16") && msg.contains("512") && msg.contains("3/3"),
        "prompt, max output and the top rung's limit are all named: {msg}"
    );
    assert_eq!(newest_log(&f, LADDER, before).await.rung, Some(1), "{msg}");

    let before = log_count(&f, LADDER).await;
    let resp = ladder_chat(&f, Dialect::Anthropic, false, 600, 16).await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error", "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
    assert_eq!(newest_log(&f, LADDER, before).await.rung, Some(1));

    assert_eq!(
        ladder_runs(&f),
        vec!["ladder-base.gguf"],
        "no climb was ever attempted"
    );
}

/// §7 item 6 at request level: admission cannot fit the target rung inside a
/// short `vram.queue_timeout_seconds` — a busy other model holds the room.
/// The client's answer is the VRAM error naming the rung; rung 1 keeps
/// serving the very next request — unarbitrated, since it is already up
/// (`admit_local`'s "a model the registry already holds takes no gate, no
/// measurement and no queue").
#[tokio::test]
async fn a_request_that_cannot_climb_for_vram_gets_the_named_error_and_rung_1_serves_on() {
    let f = ladder_fixture(12 * GIB, 0).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    set_slot_busy(&f, true);
    set_queue_timeout(&f, 1).await;

    let before = log_count(&f, LADDER).await;
    // 12 − 6 (chat) − 2 (the base, freed) = 4: short of rung 3's 8.5 with
    // headroom, and chat cannot be evicted while its slot is busy.
    let resp = ladder_chat(&f, Dialect::OpenAi, false, 200, 16).await;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["code"], "vram_queue_timeout", "{body}");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("ladder-model rung 3/3 (ladder-top.gguf)"),
        "{msg}"
    );
    assert!(msg.contains("chat/chat-model"), "{msg}");
    assert_eq!(newest_log(&f, LADDER, before).await.rung, Some(1), "{msg}");

    assert!(ladder_view(&f).climbing.is_none(), "the mark is cleared");
    assert_eq!(ladder_view(&f).rung.unwrap().rung, 1);
    assert!(f.stops().is_empty());

    let resp2 = ladder_chat(&f, Dialect::OpenAi, false, 3, 16).await;
    assert_eq!(resp2.status(), 200);
    assert_eq!(
        header(&resp2, "x-lmgw-rung"),
        Some("1/3; ctx=64; gguf=ladder-base.gguf")
    );
}

/// §7 item 8 at request level: after a climb to rung 3, the reaper — or
/// `restart` through the ops path — stops it (§3.5); the very next request's
/// `podman run` is the base file at rung 1's context, and the response says
/// `1/3`.
#[tokio::test]
async fn a_request_after_the_reaper_or_restart_resets_a_climbed_ladder_to_the_base() {
    for how in ["reaper", "restart"] {
        let f = ladder_fixture(16 * GIB, 1).await;
        let hold = ladder_hold(&f).await;
        climb_to_top(&f, &hold).await.expect("it fits");
        drop(hold);
        assert_eq!(ladder_view(&f).rung.unwrap().rung, 3, "{how}");

        match how {
            "reaper" => {
                // `last_used` is compared in whole seconds.
                tokio::time::sleep(Duration::from_millis(1_200)).await;
                lmgw_core::runtime::lifecycle::reap_idle(&f.state).await;
                assert!(
                    f.state
                        .runtime()
                        .list()
                        .iter()
                        .all(|v| v.model_id != LADDER),
                    "{how}: stopped, and only the next request starts it again"
                );
            }
            _ => {
                // Unlike the reaper, `restart` stops *and* starts the base at
                // once (runtime_lifecycle.rs's own `restart_brings_a_climbed_
                // ladder_back_to_its_base` pins this) — nothing is absent to
                // assert here.
                let out = crate::common::container_wire(
                    lmgw_core::ops::container(&f.state, None, Some(LADDER), "restart", false, None)
                        .await
                        .unwrap(),
                );
                assert_eq!(out["ok"], true, "{how}: {out}");
                assert_eq!(ladder_view(&f).rung.unwrap().rung, 1, "{how}: back at once");
            }
        }

        let before = log_count(&f, LADDER).await;
        let resp = ladder_chat(&f, Dialect::OpenAi, false, 3, 16).await;
        assert_eq!(resp.status(), 200, "{how}");
        assert_eq!(
            header(&resp, "x-lmgw-rung"),
            Some("1/3; ctx=64; gguf=ladder-base.gguf"),
            "{how}"
        );
        assert_eq!(newest_log(&f, LADDER, before).await.rung, Some(1), "{how}");
        assert_eq!(
            ladder_runs(&f),
            vec!["ladder-base.gguf", "ladder-top.gguf", "ladder-base.gguf"],
            "{how}: base, the climb, base again"
        );
    }
}

/// §7 item 9 at request level: the hold is engaged before a request that
/// would otherwise climb. Unchanged from any other local model (§3.1): no
/// fallback configured is a `503 gpu_hold`; one configured answers instead
/// and names `hold`. Either way nothing is ever started — the request never
/// reaches the fit or the climb.
#[tokio::test]
async fn a_request_that_would_climb_under_the_hold_falls_back_or_is_refused() {
    let f = ladder_fixture(16 * GIB, 0).await;
    engage_hold(&f).await;

    let resp = ladder_chat(&f, Dialect::OpenAi, false, 200, 16).await;
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_hold", "{body}");
    assert!(f.runs().is_empty(), "no podman run at all under the hold");

    let _cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;
    let resp = ladder_chat(&f, Dialect::OpenAi, false, 200, 16).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lmgw-fallback"), Some("cloud-chat"));
    assert_eq!(fallback_reason(&resp), Some("hold"));
    assert_eq!(header(&resp, "x-lmgw-rung"), None);
    assert!(f.runs().is_empty(), "still no local start");
}

/// Drain timeout at request level (§12 entry 10): an in-flight send on rung 1
/// held well past the climb's own budget. The trigger gets a named `503`
/// naming the sends still busy; the mark clears, and the very next (short)
/// request is served on rung 1, untouched.
#[tokio::test]
async fn a_climb_that_times_out_draining_names_the_sends_and_rung_1_serves_next() {
    let f = ladder_fixture(16 * GIB, 0).await;
    assert_eq!(
        ladder_chat(&f, Dialect::OpenAi, false, 3, 16)
            .await
            .status(),
        200
    );
    let base = ladder_view(&f).port;
    f.world().chat_delay.insert(base, Duration::from_secs(2));
    set_queue_timeout(&f, 1).await;

    // A: in flight, held well past the 1s budget below.
    let a = {
        let gw = f.gateway.clone();
        tokio::spawn(async move {
            chat_body(
                &gw,
                json!({"model": LADDER, "max_tokens": 16,
                       "messages": [{"role": "user", "content": words("a", 3)}]}),
            )
            .await
        })
    };
    for _ in 0..500 {
        if ladder_view(&f).sends >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(ladder_view(&f).sends, 1, "A is in flight");

    // B: needs rung 3 — the drain never sees zero within the 1s budget.
    let resp = ladder_chat(&f, Dialect::OpenAi, false, 200, 16).await;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["code"], "vram_queue_timeout", "{body}");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("rung 3/3 (ladder-top.gguf)"), "{msg}");
    assert!(msg.contains("1 request(s) were still in flight"), "{msg}");
    assert!(ladder_view(&f).climbing.is_none(), "the mark cleared");
    assert_eq!(ladder_view(&f).rung.unwrap().rung, 1);

    // C: a short request, right after — rung 1 serves it, untouched.
    let resp = ladder_chat(&f, Dialect::OpenAi, false, 3, 16).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-lmgw-rung"),
        Some("1/3; ctx=64; gguf=ladder-base.gguf")
    );

    let resp_a = a.await.unwrap();
    assert_eq!(resp_a.status(), 200);
    assert!(resp_a.text().await.unwrap().contains(&served_by(base)));
    assert!(f.stops().is_empty());
    assert_eq!(
        ladder_runs(&f),
        vec!["ladder-base.gguf"],
        "no climb ever started"
    );
}

/// Two concurrent triggers needing rungs 2 and 3 (race table "two triggers at
/// once"; test map item 18). A brand-new request has no claim yet, so it
/// cannot join an in-progress mark the way an already-admitted caller's own
/// retry can (`Marked::Joined { raised }`) — a fresh `gate::open` parks in
/// `Registry::acquire` on `Phase::Climbing` instead, and only re-judges once
/// the first climb settles. Podman's gate freezes the first mid-start so the
/// ordering is guaranteed rather than raced: rung 1 → 2, then rung 2 → 3 —
/// never two `podman run`s in flight together.
#[tokio::test]
async fn two_concurrent_triggers_climb_one_rung_at_a_time_never_at_once() {
    let f = ladder_fixture(16 * GIB, 0).await;
    assert_eq!(
        ladder_chat(&f, Dialect::OpenAi, false, 3, 16)
            .await
            .status(),
        200
    );
    let (open, gate) = tokio::sync::watch::channel(false);
    *f.podman.gate.lock().unwrap() = Some(gate);

    // A: 101 prompt tokens + 16 needs rung 2 (128), not rung 1 (64).
    let a = {
        let gw = f.gateway.clone();
        tokio::spawn(async move {
            chat_body(
                &gw,
                json!({"model": LADDER, "max_tokens": 16,
                       "messages": [{"role": "user", "content": words("a", 100)}]}),
            )
            .await
        })
    };
    for _ in 0..3_000 {
        if ladder_view(&f).state.as_str() == "starting" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(ladder_view(&f).state.as_str(), "starting");
    assert_eq!(
        ladder_view(&f).climbing.as_ref().map(|c| c.to),
        Some(2),
        "climbing to rung 2"
    );

    // B: 201 prompt tokens + 16 needs rung 3 — a brand-new request, parked.
    let b = {
        let gw = f.gateway.clone();
        tokio::spawn(async move {
            chat_body(
                &gw,
                json!({"model": LADDER, "max_tokens": 16,
                       "messages": [{"role": "user", "content": words("b", 200)}]}),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !b.is_finished(),
        "parked behind A's mark, not joined into it"
    );

    open.send_replace(true);
    let resp_a = a.await.unwrap();
    assert_eq!(resp_a.status(), 200);
    assert_eq!(
        header(&resp_a, "x-lmgw-rung"),
        Some("2/3; ctx=128; gguf=ladder-mid.gguf")
    );

    let resp_b = b.await.unwrap();
    assert_eq!(resp_b.status(), 200);
    assert_eq!(
        header(&resp_b, "x-lmgw-rung"),
        Some("3/3; ctx=512; gguf=ladder-top.gguf")
    );

    assert_eq!(
        ladder_runs(&f),
        vec!["ladder-base.gguf", "ladder-mid.gguf", "ladder-top.gguf"],
        "rung 1 → 2 → 3, one run at a time"
    );
    assert_eq!(
        f.stops(),
        vec![LADDER.to_string(), LADDER.to_string()],
        "two climbs, two stops — never concurrent"
    );
}

/// The container dies while the count and the send are both in flight (§12
/// entry 20, test map item 20): the pair is dropped, the generation-safe
/// recovery restarts the base, and the request is recounted and resent there
/// — the client still gets its 200.
#[tokio::test]
async fn the_container_dying_mid_count_recovers_at_the_base_and_still_answers() {
    let mut f = ladder_fixture(16 * GIB, 0).await;
    assert_eq!(
        ladder_chat(&f, Dialect::OpenAi, false, 3, 16)
            .await
            .status(),
        200
    );
    let base = ladder_view(&f).port;
    f.world()
        .apply_template_delay
        .insert(base, Duration::from_millis(300));
    f.world()
        .chat_delay
        .insert(base, Duration::from_millis(300));

    let sent = {
        let gw = f.gateway.clone();
        tokio::spawn(async move {
            chat_body(
                &gw,
                json!({"model": LADDER, "max_tokens": 16,
                       "messages": [{"role": "user", "content": words("k", 3)}]}),
            )
            .await
        })
    };
    for _ in 0..500 {
        if ladder_view(&f).sends >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(ladder_view(&f).sends, 1, "the pair is under way");
    f.kill_container_on(base).await;

    let resp = sent.await.unwrap();
    let rung_header = header(&resp, "x-lmgw-rung").map(str::to_string);
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        rung_header.as_deref(),
        Some("1/3; ctx=64; gguf=ladder-base.gguf"),
        "answered by the recovered base"
    );
    assert_eq!(
        ladder_runs(&f),
        vec!["ladder-base.gguf", "ladder-base.gguf"],
        "the cold start, then the recovery"
    );
}

/// `/v1/count_tokens` during a climb (test map item 23): a fresh count
/// request has no claim to lose — unlike a chat send it never counts against
/// the drain — so it simply parks in admission like any acquire and answers
/// only once the new rung is up, on the container that ends up running.
#[tokio::test]
async fn count_tokens_during_a_climb_waits_then_answers_on_the_new_rung() {
    let f = ladder_fixture(16 * GIB, 0).await;
    assert_eq!(
        ladder_chat(&f, Dialect::OpenAi, false, 3, 16)
            .await
            .status(),
        200
    );
    let (open, gate) = tokio::sync::watch::channel(false);
    *f.podman.gate.lock().unwrap() = Some(gate);

    // The trigger: 201 prompt tokens + 16 needs rung 3.
    let trigger = {
        let gw = f.gateway.clone();
        tokio::spawn(async move {
            chat_body(
                &gw,
                json!({"model": LADDER, "max_tokens": 16,
                       "messages": [{"role": "user", "content": words("t", 200)}]}),
            )
            .await
        })
    };
    for _ in 0..3_000 {
        if ladder_view(&f).state.as_str() == "starting" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(ladder_view(&f).state.as_str(), "starting");

    let counted = {
        let gw = f.gateway.clone();
        tokio::spawn(async move {
            gw.client()
                .post(format!("{gw}/v1/count_tokens"))
                .json(&json!({"model": LADDER, "input": words("c", 4)}))
                .send()
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!counted.is_finished(), "parked behind the climb");

    open.send_replace(true);
    assert_eq!(trigger.await.unwrap().status(), 200);

    let resp = counted.await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["tokens"], 4, "{body}");
    assert_eq!(
        ladder_view(&f).rung.unwrap().rung,
        3,
        "answered once rung 3 was up"
    );
}
