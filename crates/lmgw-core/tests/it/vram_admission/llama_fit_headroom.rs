//! The second review of the llama-fit branch (2026-11-11), end to end, on a
//! card where other programs hold more now than their 7-day minimum and auto
//! headroom has learned a growth:
//!
//! - A: a fitted row's target is sized to what the card can give it now —
//!   other programs' use now, lmgw's own models counted as room — and the
//!   learned growth does not shrink it, so it is admitted: an operator start
//!   goes ahead, and a request with a live fallback is answered by the model
//!   (lmgw's idle model evicted), not by the fallback;
//! - B: a row that cannot adapt waits for and evicts lmgw's own models for the
//!   growth, and once none is left starts where it fits beside other
//!   programs' use now with its margin, warned with the growth; a real
//!   shortfall still waits, and says what holds the card;
//! - C: an adopted llama-server whose holding the driver reads keeps no
//!   first-prompt margin; one nothing was read of keeps it;
//! - D: a token count does not clear the first-prompt margin; a chat does.

use super::llama_fit_review::{add_row, argv, flag, now_hour, start};
use super::unheld_containers::{forget_everything, pass};
use super::*;
use lmgw_core::config::LlamaParams;
use lmgw_core::store::OutsideHour;
use lmgw_core::vram::headroom::{GROWTH_LET_GO, LLAMA_MARGIN};
use lmgw_core::vram::partial::llama::CONTEXT_FITTED;

const MIB: u64 = 1024 * 1024;

/// The card: 1 GiB.
const TOTAL: u64 = 1024 * MIB;
/// What other programs hold at rest: the 7-day minimum.
const AT_REST: u64 = 16 * MIB;
/// What they hold now.
const NOW: u64 = 100 * MIB;
/// The largest growth learned.
const GROWTH: u64 = 200 * MIB;

/// A day of readings: at rest at [`AT_REST`], one hour that rose by
/// [`GROWTH`] — auto headroom learned, with a growth.
fn learned(f: &Fixture) {
    let now = now_hour();
    f.state.vram.seed_outside_hours(
        (0..24)
            .map(|h| OutsideHour {
                hour: now - h,
                low_bytes: AT_REST,
                high_bytes: if h == 3 { AT_REST + GROWTH } else { AT_REST },
                rise_bytes: if h == 3 { GROWTH } else { 0 },
            })
            .collect(),
    );
}

/// A card with other programs holding [`NOW`] on it, per process, and the
/// growth learned.
async fn card() -> Fixture {
    let f = fixture(TOTAL, 8 * MIB, 8 * MIB, 4).await;
    f.attribute(NOW);
    learned(&f);
    let snap = f.state.snapshot();
    let h = f.state.vram.headroom_now(&snap);
    assert!(h.learned_in_use && h.bytes == GROWTH, "{h:?}");
    f
}

fn entry(f: &Fixture, model_id: &str) -> lmgw_core::runtime::registry::RuntimeView {
    f.state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.model_id == model_id)
        .unwrap_or_else(|| panic!("{model_id} is up"))
}

fn warned(f: &Fixture, model_id: &str, lead: &str) -> String {
    let w = entry(f, model_id).warnings;
    w.iter()
        .find(|w| w.contains(lead))
        .cloned()
        .unwrap_or_else(|| panic!("no {lead:?} in {w:?}"))
}

async fn chat_with(f: &Fixture, model: &str) -> reqwest::Response {
    f.gateway
        .client()
        .post(format!("{}/v1/chat/completions", f.gateway))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
}

/// A: before, the target was sized to the card less the 7-day minimum and
/// the growth, then admitted against the free memory now, where it never
/// fitted; an operator start found nothing smaller beside the growth and was
/// refused. Now it is fitted to what the card gives it now, starts with that
/// target, and says that the growth is not kept free.
#[tokio::test]
async fn a_fitted_target_is_sized_to_outside_use_now_and_admits() {
    let f = card().await;
    add_row(&f, "fit-model", LlamaParams::default()).await;
    let out = start(&f, "fit-model").await;
    assert!(out.is_ok(), "{out:?}");
    let a = argv(&f, "fit-model");
    assert_eq!(flag(&a, "--fit").as_deref(), Some("off"), "{a:?}");
    let g = entry(&f, "fit-model").grant.expect("its target");
    let ctx: u64 = flag(&a, "--ctx-size").unwrap().parse().unwrap();
    assert_eq!(g.llama.and_then(|p| p.ctx), Some(ctx));
    assert!(
        g.bytes + LLAMA_MARGIN <= TOTAL - NOW,
        "fits beside other programs' use now with its margin: {g:?}"
    );
    let w = warned(&f, "fit-model", CONTEXT_FITTED);
    assert!(
        w.contains(GROWTH_LET_GO) && w.contains("200 MiB on "),
        "the growth and when it was seen: {w}"
    );
}

/// A: a live fallback does not answer for a target fitted to the card as it
/// is now: lmgw's idle model is evicted and the model itself answers.
#[tokio::test]
async fn a_live_fallback_does_not_answer_for_a_fitted_target() {
    let f = card().await;
    let _cloud = cloud_chat(&f, "cloud").await;
    add_row(&f, "fit-model", LlamaParams::default()).await;
    set_global_fallback(&f, "cloud").await;
    assert_eq!(chat_with(&f, "chat-model").await.status(), 200);

    let resp = chat_with(&f, "fit-model").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(fallback_reason(&resp), None, "served by the model itself");
    assert_eq!(f.stops(), vec!["chat-model".to_string()], "evicted for it");
    let w = warned(&f, "fit-model", CONTEXT_FITTED);
    assert!(w.contains(GROWTH_LET_GO), "{w}");
}

/// B: a row that sets its context cannot adapt. With the growth beside it
/// it would need more than the card less what other programs hold now; it
/// evicts lmgw's idle model for it, then starts with its margin alone,
/// warned, instead of waiting out the queue. A real shortfall — other
/// programs' use now — still waits, and the refusal says so.
#[tokio::test]
async fn a_non_adapting_row_starts_once_nothing_of_lmgws_is_left() {
    let pinned = LlamaParams {
        ctx_size: Some(4096),
        n_gpu_layers: Some(999),
        ..Default::default()
    };
    let f = card().await;
    add_row(&f, "pinned", pinned.clone()).await;
    assert_eq!(chat_with(&f, "chat-model").await.status(), 200);
    let resp = chat_with(&f, "pinned").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(f.stops(), vec!["chat-model".to_string()], "evicted for it");
    let w = warned(&f, "pinned", GROWTH_LET_GO);
    assert!(
        w.contains("200 MiB on ") && w.contains("chat/pinned"),
        "{w}"
    );
    assert_eq!(
        flag(&argv(&f, "pinned"), "--ctx-size").as_deref(),
        Some("4096")
    );

    // Other programs now hold so much that it does not fit beside them
    // even with its margin alone: a wait, as before, that names it.
    let f = card().await;
    f.attribute(TOTAL - LLAMA_MARGIN);
    set_queue_timeout(&f, 1).await;
    add_row(&f, "pinned", pinned).await;
    let resp = chat_with(&f, "pinned").await;
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("another process is using the GPU"),
        "{body}"
    );
    assert!(argv(&f, "pinned").is_empty(), "not started");
}

/// The note on `model`'s row of `GET /api/vram`.
async fn note(f: &Fixture, model: &str) -> String {
    let v = vram_status(&f.gateway).await;
    v["resident"]
        .as_array()
        .and_then(|rs| rs.iter().find(|r| r["model"] == model).cloned())
        .map(|r| r["note"].as_str().unwrap_or_default().to_string())
        .unwrap_or_default()
}

const KEPT: &str = "is kept free beside it until its first request has run";

/// C: after a restart the container is adopted, with nothing known of what
/// it served. Where the driver reads what it holds, that reading is its
/// measurement and no margin is kept; with no per-process figures it keeps
/// the margin until a request has run.
#[tokio::test]
async fn an_adopted_llama_server_the_driver_reads_keeps_no_margin() {
    for measured in [true, false] {
        let f = fixture(4096 * MIB, 8 * MIB, 8 * MIB, 4).await;
        f.world().reports = true;
        if measured {
            f.attribute(NOW);
        }
        learned(&f);
        start(&f, "chat-model").await.unwrap();
        assert!(note(&f, "chat-model").await.contains(KEPT), "a fresh start");
        forget_everything(&f);
        assert!(pass(&f).await, "adopted");
        let n = note(&f, "chat-model").await;
        assert_eq!(!n.contains(KEPT), measured, "measured {measured}: {n}");
    }
}

/// D: `/tokenize` runs no prompt, so the margin stays; a chat runs one.
#[tokio::test]
async fn a_token_count_leaves_the_first_prompt_margin() {
    let f = fixture(4096 * MIB, 8 * MIB, 8 * MIB, 4).await;
    learned(&f);
    start(&f, "chat-model").await.unwrap();
    let resp = f
        .gateway
        .client()
        .post(format!("{}/tokenize", f.gateway))
        .json(&json!({"model": "chat-model", "content": "hello there"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.bytes().await.unwrap();
    assert!(
        f.world().tokenize_calls.values().sum::<u64>() > 0,
        "the count reached the container"
    );
    // The claim is let go after the answer: before the fix, the release
    // marked the container served within this window.
    let until = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < until {
        assert!(!entry(&f, "chat-model").served, "a count is no prompt");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(note(&f, "chat-model").await.contains(KEPT));

    let resp = chat_with(&f, "chat-model").await;
    assert_eq!(resp.status(), 200);
    resp.bytes().await.unwrap();
    let mut wait = crate::common::patience::Wait::new("the margin dropped after the chat");
    loop {
        if entry(&f, "chat-model").served && !note(&f, "chat-model").await.contains(KEPT) {
            break;
        }
        wait.again(None).await;
    }
}
