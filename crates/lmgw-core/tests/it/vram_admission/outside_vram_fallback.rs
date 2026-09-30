//! Outside-VRAM fallback (candidate-aliases design §4.7) — scheduler level

use super::*;

// These pin the verdict and `admit_or_external` itself; the HTTP half (the
// gate swapping to the fallback, its headers and log row) is the end-to-end
// section below.

const MIB: u64 = 1024 * 1024;

async fn admit_or_external(
    f: &Fixture,
    alias: &str,
    has_fallback: bool,
) -> Result<lmgw_core::vram::Admission, lmgw_core::error::GatewayError> {
    let route = f.state.snapshot().resolve(alias).unwrap();
    let fallback = has_fallback.then(lmgw_core::vram::ExternalFallback::usable);
    lmgw_core::vram::admit_or_external(&f.state, &route, alias, fallback).await
}

async fn set_fallback_on_external(f: &Fixture, on: bool) {
    let mut s = f.state.snapshot().settings.clone();
    s.vram.fallback_on_external = on;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// Poll `GET /api/vram` until `ready` holds. The status view reads a
/// container's PID in the background the first time it sees it, so the frame
/// right after a start may still say "being read".
pub(super) async fn until_vram(f: &Fixture, what: &str, ready: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..200 {
        let v = vram_status(&f.gateway).await;
        if ready(&v) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what}: {}", vram_status(&f.gateway).await);
}

/// The motivating case: a game holds VRAM lmgw cannot free. With every lmgw
/// model gone the request still would not fit, so a caller with a fallback
/// is told so at once — no queue, no eviction of the idle resident, no start.
#[tokio::test]
async fn outside_use_that_leaves_too_little_room_answers_external_at_once() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(3 * GIB);
    // The embedder is resident and idle: 3 GiB lmgw could free.
    assert_eq!(embed(&f.gateway).await.status(), 200);

    let waits = f.state.vram.waits_begun();
    let admitted = admit_or_external(&f, "chat-model", true)
        .await
        .expect("a verdict is not a refusal");
    let lmgw_core::vram::Admission::External(short) = admitted else {
        panic!("outside use should have been answered as external: {admitted:?}");
    };
    assert_eq!(
        f.state.vram.waits_begun(),
        waits,
        "it must not wait in the queue first"
    );
    // needed 6.5 GiB; free 8 - 3 (embedder) - 3 (game) = 2; + lmgw's 3 = 5.
    assert_eq!(short.model, "chat-model");
    assert_eq!(short.needed_bytes, 6 * GIB + 512 * MIB);
    assert_eq!(short.free_bytes, 2 * GIB);
    assert_eq!(short.lmgw_share_bytes, 3 * GIB);
    assert_eq!(short.outside_bytes, 3 * GIB);
    assert!(short.describe().contains("chat/chat-model"));

    assert_eq!(f.runs(), vec!["embed-model".to_string()], "nothing started");
    assert!(
        f.stops().is_empty(),
        "the idle embedder was not evicted for it"
    );
    let v = vram_status(&f.gateway).await;
    assert!(v["queue"].as_array().is_none_or(|q| q.is_empty()), "{v}");
    assert!(
        f.pid_inspects()
            .iter()
            .flatten()
            .any(|n| n.contains("embed")),
        "the embedder's share was measured under its own PID"
    );
}

/// The same shortfall with lmgw's own busy model in the way is contention
/// lmgw resolves itself: the request queues (visibly) for the busy model,
/// as it always has, and is never sent to a fallback.
#[tokio::test]
async fn busy_lmgw_models_in_the_way_queue_instead_of_falling_back() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(GIB);
    assert_eq!(chat(&f.gateway).await.status(), 200);
    set_slot_busy(&f, true);
    set_queue_timeout(&f, 1).await;

    // The embedder needs 3.5 GiB: 1 GiB is free, but the chat model's 6 GiB
    // is lmgw's to free once it finishes.
    let state = f.state.clone();
    let waiting = tokio::spawn(async move {
        let route = state.snapshot().resolve("embed/embed-model").unwrap();
        let fallback = Some(lmgw_core::vram::ExternalFallback::usable());
        lmgw_core::vram::admit_or_external(&state, &route, "embed/embed-model", fallback)
            .await
            .map(|a| matches!(a, lmgw_core::vram::Admission::External(_)))
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    let v = vram_status(&f.gateway).await;
    assert_eq!(
        v["queue"].as_array().map_or(0, |q| q.len()),
        1,
        "it waits in the queue like any other request: {v}"
    );
    match waiting.await.unwrap() {
        Err(lmgw_core::error::GatewayError::VramQueueTimeout { holding, .. }) => {
            assert!(holding.contains("chat-model"), "{holding}")
        }
        other => panic!("expected today's queue timeout, got {other:?}"),
    }
    assert!(f.stops().is_empty(), "a generating model is never stopped");
}

/// A model that cannot fit even on an empty card stays today's
/// `vram_too_large`, and needs no PID to be told so.
#[tokio::test]
async fn a_model_larger_than_the_card_stays_vram_too_large() {
    let f = fixture(4 * GIB, 8 * GIB, GIB, 512).await;
    f.attribute(GIB);
    let r = admit_or_external(&f, "chat-model", true).await;
    assert!(
        matches!(r, Err(lmgw_core::error::GatewayError::VramTooLarge { .. })),
        "{r:?}"
    );
    assert!(f.runs().is_empty());
    assert!(f.pid_inspects().is_empty(), "{:?}", f.pid_inspects());
}

/// The switch off is hold-only: the same outside shortfall queues and times
/// out exactly as before §4.7, nothing is inspected, and the status surfaces
/// say why the trigger is inactive.
#[tokio::test]
async fn with_the_switch_off_an_outside_shortfall_queues_as_before() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(4 * GIB);
    set_fallback_on_external(&f, false).await;
    set_queue_timeout(&f, 1).await;

    let r = admit_or_external(&f, "chat-model", true).await;
    assert!(
        matches!(
            r,
            Err(lmgw_core::error::GatewayError::VramQueueTimeout { .. })
        ),
        "{r:?}"
    );
    assert!(f.pid_inspects().is_empty(), "{:?}", f.pid_inspects());

    let v = vram_status(&f.gateway).await;
    assert_eq!(v["external_trigger_active"], false, "{v}");
    assert!(
        v["external_trigger_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("vram.fallback_on_external"),
        "{v}"
    );
    assert!(v["lmgw_share_bytes"].is_null() && v["outside_share_bytes"].is_null());
    let status = lmgw_core::ops::status(&f.state).await.unwrap();
    assert_eq!(status["vram"]["external_trigger_active"], false);
}

/// One lmgw container the driver does not list — a CPU-only row, say —
/// makes lmgw's share a guess, so the trigger is unavailable and the request
/// takes today's path, waiting. The status view names the model. The
/// container is generating (on its own port), so today's path cannot evict it
/// either and the trigger stays unavailable for the whole wait, however often
/// the verdict is taken again (review finding 8; once the unattributed
/// container is gone, see
/// `a_request_that_waited_on_an_unattributed_container_falls_back_once_it_is_gone`).
#[tokio::test]
async fn an_unattributed_container_leaves_admission_as_before() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(3 * GIB);
    f.world().unlisted.insert("embed-model".into());
    assert_eq!(embed(&f.gateway).await.status(), 200);
    f.world().busy.insert("embed-model".into());
    set_queue_timeout(&f, 1).await;

    let v = until_vram(&f, "the embedder is named", |v| {
        v["external_trigger_reason"]
            .as_str()
            .is_some_and(|r| r.contains("aux/embed-model") && r.contains("not attributed"))
    })
    .await;
    assert_eq!(v["external_trigger_active"], false);

    let r = admit_or_external(&f, "chat-model", true).await;
    assert!(
        matches!(
            r,
            Err(lmgw_core::error::GatewayError::VramQueueTimeout { .. })
        ),
        "today's path, not a fallback: {r:?}"
    );
    assert!(f.stops().is_empty(), "a generating model is never stopped");
}

/// Without a fallback there is nothing to answer with: today's path, the
/// verdict never computed — the same outside shortfall queues and times out,
/// and nothing asks podman for a PID.
#[tokio::test]
async fn without_a_fallback_admission_is_todays_path() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(4 * GIB);
    set_queue_timeout(&f, 1).await;

    let r = admit_or_external(&f, "chat-model", false).await;
    assert!(
        matches!(
            r,
            Err(lmgw_core::error::GatewayError::VramQueueTimeout { .. })
        ),
        "{r:?}"
    );
    assert!(f.pid_inspects().is_empty(), "{:?}", f.pid_inspects());
    assert!(f.runs().is_empty());

    // And a model with room is simply admitted, as by `vram::admit`.
    f.world().outside = 0;
    let admitted = admit_or_external(&f, "chat-model", false).await.unwrap();
    let lmgw_core::vram::Admission::Admitted(Some(hold)) = admitted else {
        panic!("expected a hold: {admitted:?}");
    };
    assert_eq!(hold.model_id(), "chat-model");
}

/// `GET /api/vram` and `lmgw__status` carry lmgw's measured share, the rest,
/// and the trigger's state; a container's PID is read once per container,
/// and a start in flight makes the share unavailable while it lasts.
#[tokio::test]
async fn the_status_view_shows_the_measured_share_and_reads_each_pid_once() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(GIB);
    assert_eq!(chat(&f.gateway).await.status(), 200);

    let v = until_vram(&f, "the chat model's share is measured", |v| {
        v["external_trigger_active"] == true
    })
    .await;
    assert_eq!(v["lmgw_share_bytes"], 6 * GIB, "{v}");
    assert_eq!(v["outside_share_bytes"], GIB, "{v}");
    assert!(v["external_trigger_reason"].is_null(), "{v}");
    let status = lmgw_core::ops::status(&f.state).await.unwrap();
    assert_eq!(status["vram"]["lmgw_share_bytes"], 6 * GIB);

    let asked = f.pid_inspects().len();
    assert_eq!(asked, 1, "{:?}", f.pid_inspects());
    for _ in 0..3 {
        vram_status(&f.gateway).await;
    }
    assert_eq!(
        f.pid_inspects().len(),
        asked,
        "a cached PID is not asked again"
    );

    // A start in flight: its memory is not on the card yet.
    let (tx, rx) = tokio::sync::watch::channel(false);
    *f.podman.gate.lock().unwrap() = Some(rx);
    let gateway = f.gateway.clone();
    let starting = tokio::spawn(async move { embed(&gateway).await.status() });
    let v = until_vram(&f, "the start is named", |v| {
        v["external_trigger_reason"]
            .as_str()
            .is_some_and(|r| r.contains("aux/embed-model") && r.contains("starting"))
    })
    .await;
    assert!(v["lmgw_share_bytes"].is_null(), "{v}");
    tx.send(true).unwrap();
    assert_eq!(starting.await.unwrap(), 200);
    let v = until_vram(&f, "both models measured", |v| {
        v["external_trigger_active"] == true
    })
    .await;
    assert_eq!(v["lmgw_share_bytes"], 9 * GIB, "{v}");
}

/// Right after an eviction the driver can still list the stopped container's
/// process. That memory is still lmgw's (a tombstone), not outside use —
/// otherwise the moment after every eviction would send requests to the
/// cloud. Once the driver stops listing it, it is gone for good.
#[tokio::test]
async fn a_just_stopped_container_still_counts_as_lmgws_while_listed() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(0);
    assert_eq!(chat(&f.gateway).await.status(), 200);
    until_vram(&f, "the chat model's PID is read", |v| {
        v["lmgw_share_bytes"] == 6 * GIB
    })
    .await;

    // Stopped, and still on the card.
    f.world().lingering.insert("chat-model".into());
    f.state
        .runtime()
        .stop(lmgw_core::runtime::Class::Chat, "chat-model", false)
        .await
        .unwrap();
    let v = vram_status(&f.gateway).await;
    assert_eq!(v["lmgw_share_bytes"], 6 * GIB, "{v}");
    assert_eq!(v["outside_share_bytes"], 0, "{v}");

    // A verdict in that moment: 2 GiB free, but the 6 GiB are lmgw's and on
    // their way out — not a reason to fall back. The driver lets go shortly
    // after, and the request is admitted on today's path.
    let world = f.world.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        world.lock().unwrap().lingering.clear();
    });
    let admitted = admit_or_external(&f, "embed/embed-model", true)
        .await
        .expect("admitted once the stopped process let go");
    assert!(
        matches!(admitted, lmgw_core::vram::Admission::Admitted(Some(_))),
        "{admitted:?}"
    );

    let v = until_vram(&f, "the tombstone is dropped", |v| {
        v["lmgw_share_bytes"] == 3 * GIB
    })
    .await;
    assert_eq!(v["outside_share_bytes"], 0, "{v}");
}

/// Entry 23's open edge, closed: a container's PID is read when it becomes
/// ready, so one evicted before any verdict or status view looked at it
/// still leaves a tombstone — its process, still on the card right after the
/// stop, counts as lmgw's rather than as outside use.
#[tokio::test]
async fn a_container_evicted_before_any_pass_still_leaves_a_tombstone() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(0);
    assert_eq!(chat(&f.gateway).await.status(), 200);
    // No view and no verdict ran: the start itself asked podman.
    for _ in 0..200 {
        if !f.pid_inspects().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        f.pid_inspects().len(),
        1,
        "one inspect, for the container that came up: {:?}",
        f.pid_inspects()
    );
    // The answer is recorded by the same task, right after podman returns.
    tokio::time::sleep(Duration::from_millis(100)).await;

    f.world().lingering.insert("chat-model".into());
    f.state
        .runtime()
        .stop(lmgw_core::runtime::Class::Chat, "chat-model", false)
        .await
        .unwrap();
    let v = vram_status(&f.gateway).await;
    assert_eq!(v["lmgw_share_bytes"], 6 * GIB, "{v}");
    assert_eq!(v["outside_share_bytes"], 0, "{v}");
}

/// With the switch off nothing reads a PID at start either — the switch off
/// is today's behaviour, podman calls included (§7 item 14).
#[tokio::test]
async fn with_the_switch_off_a_start_reads_no_pid() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(0);
    set_fallback_on_external(&f, false).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(f.pid_inspects().is_empty(), "{:?}", f.pid_inspects());
}

/// Review finding 1: boot reconciliation is spawned, not awaited, so requests
/// are served while it still reads podman. A container a previous lmgw left
/// on the card is lmgw's, but until boot adopts it the registry does not hold
/// it and its memory would read as outside use. So until boot has settled the
/// verdict is unavailable, and the request takes today's path rather than the
/// fallback.
#[tokio::test]
async fn a_container_boot_has_not_adopted_yet_is_not_outside_use() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(0);
    // The embedder survived an lmgw restart: podman runs it and the driver
    // lists its process, but nothing has adopted it yet.
    {
        let mut w = f.world();
        w.loaded.insert("embed-model".into());
        w.pids.insert("embed-model".into(), 5_999_000);
    }
    f.state.vram.set_boot_settled(false);
    set_queue_timeout(&f, 1).await;

    let v = vram_status(&f.gateway).await;
    assert_eq!(v["external_trigger_active"], false, "{v}");
    assert!(
        v["external_trigger_reason"]
            .as_str()
            .is_some_and(|r| r.contains("boot reconciliation")),
        "{v}"
    );
    // The chat model needs 6.5 GiB and 5 are free: with the embedder's 3 GiB
    // read as outside use this would be External.
    let r = admit_or_external(&f, "chat-model", true).await;
    assert!(
        matches!(
            r,
            Err(lmgw_core::error::GatewayError::VramQueueTimeout { .. })
        ),
        "today's path, not a fallback: {r:?}"
    );
    assert!(f.pid_inspects().is_empty(), "{:?}", f.pid_inspects());

    // Boot settles the flag however reconciliation went — here podman lists
    // nothing to adopt, so from now on the unadopted container really is
    // memory lmgw cannot free, and the same request is answered as such.
    lmgw_core::runtime::lifecycle::boot(&f.state).await;
    assert!(f.state.vram.boot_settled());
    let r = admit_or_external(&f, "chat-model", true).await.unwrap();
    assert!(
        matches!(r, lmgw_core::vram::Admission::External(_)),
        "{r:?}"
    );
}

/// Review finding 4: reading a container's PID is one `podman inspect`, and
/// podman can queue it behind a container lock for as long as a stop's grace
/// period or a pull takes. Verdicts that need a PID already being read join
/// that read instead of spawning their own, and the wait is bounded by
/// lmgw's control-call timeout: past it the verdict is unavailable, with a
/// reason that names the bound.
#[tokio::test]
async fn verdicts_share_one_bounded_pid_read() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(3 * GIB);
    let (_hold_inspects, rx) = tokio::sync::watch::channel(false);
    *f.podman.inspect_gate.lock().unwrap() = Some(rx);
    // The embedder comes up; the read of its PID that follows parks in podman.
    assert_eq!(embed(&f.gateway).await.status(), 200);
    for _ in 0..200 {
        if !f.pid_inspects().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(f.pid_inspects().len(), 1, "{:?}", f.pid_inspects());

    let snap = f.state.snapshot();
    let route = snap.resolve("chat-model").unwrap();
    let target = lmgw_core::vram::classify(&route).unwrap();
    let started = std::time::Instant::now();
    let verdicts = futures::future::join_all(
        (0..3).map(|_| f.state.vram.external_verdict(&f.state, &snap, &target)),
    )
    .await;
    let waited = started.elapsed();
    for v in &verdicts {
        let lmgw_core::vram::ExternalVerdict::Unavailable(why) = v else {
            panic!("a PID podman does not answer for is no verdict: {v:?}");
        };
        assert!(
            why.contains("5s") && why.contains("CONTROL_TIMEOUT"),
            "the reason names the bound: {why}"
        );
    }
    assert_eq!(
        f.pid_inspects().len(),
        1,
        "three verdicts joined the read in flight: {:?}",
        f.pid_inspects()
    );
    assert!(
        waited < Duration::from_secs(8),
        "bounded by the control timeout, not by podman: {waited:?}"
    );
}

// Outside-VRAM fallback (candidate-aliases design §4.7, §7 item 13) — end to
// end: the gate's admission swapping to the fallback, its headers, its log

/// Which client dialect a chat request goes out in.
#[derive(Debug, Clone, Copy)]
pub(super) enum Dialect {
    OpenAi,
    Anthropic,
}

/// The global chat fallback (chat rows inherit it). With the hold off it is
/// what the outside-VRAM verdict answers with.
pub(super) async fn set_global_fallback(f: &Fixture, alias: &str) {
    let mut s = f.state.snapshot().settings.clone();
    s.hold.fallback_alias = Some(alias.into());
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// The embedder row's own fallback — aux never inherits the global one.
async fn set_embed_fallback(f: &Fixture, alias: &str) {
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
            hold_fallback: Some(alias.into()),
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// A cloud chat alias that answers both shapes: streamed when the body asks
/// for it, unary otherwise (`cloud_upstream`'s own responder).
pub(super) async fn cloud_chat(f: &Fixture, alias: &str) -> MockServer {
    let cloud = cloud_upstream(f, alias).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(wiremock::matchers::body_partial_json(json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    concat!(
                        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"from the cloud\"}}]}\n\n",
                        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                        "data: [DONE]\n\n"
                    ),
                    "text/event-stream",
                ),
        )
        .with_priority(1)
        .mount(&cloud)
        .await;
    cloud
}

/// The newest request-log row for `alias`, once there are more than
/// `before` — a streamed request writes its row when the relay ends.
pub(super) async fn newest_log(f: &Fixture, alias: &str, before: usize) -> store::RequestLogRow {
    for _ in 0..200 {
        let rows = store::query_logs(
            &f.state.db,
            &store::LogFilter {
                alias: Some(alias.into()),
                limit: 1000,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if rows.len() > before {
            return rows.into_iter().next().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no new request-log row for '{alias}'");
}

pub(super) async fn log_count(f: &Fixture, alias: &str) -> usize {
    store::query_logs(
        &f.state.db,
        &store::LogFilter {
            alias: Some(alias.into()),
            limit: 1000,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .len()
}

/// Send a chat request for `requested` and assert §4.7's answer: `expected`
/// answered **at once** — no container started, nothing queued, well inside
/// the queue timeout — the response names it with `external_vram`, the body
/// still echoes the requested name, and the log row records it all.
///
/// Phase 4 adds §7 item 13's last case — a non-background candidate alias
/// answered by its alias fallback on the same verdict — as one more call:
/// `chat_falls_back_at_once(&f, Dialect::OpenAi, "<candidate alias>", false,
/// "<its fallback>")`.
async fn chat_falls_back_at_once(
    f: &Fixture,
    dialect: Dialect,
    requested: &str,
    stream: bool,
    expected: &str,
) {
    let before = log_count(f, requested).await;
    let (url, body) = match dialect {
        Dialect::OpenAi => (
            format!("{}/v1/chat/completions", f.gateway),
            json!({"model": requested, "stream": stream,
                   "messages": [{"role": "user", "content": "hi"}]}),
        ),
        Dialect::Anthropic => (
            format!("{}/v1/messages", f.gateway),
            json!({"model": requested, "stream": stream, "max_tokens": 64,
                   "messages": [{"role": "user", "content": "hi"}]}),
        ),
    };
    let waits = f.state.vram.waits_begun();
    let resp = f
        .gateway
        .client()
        .post(url)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        f.state.vram.waits_begun(),
        waits,
        "answered at once, never queued ({dialect:?}, stream {stream})"
    );
    assert_eq!(resp.status(), 200, "{dialect:?}, stream {stream}");
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some(expected),
        "{dialect:?}, stream {stream}: {:?}",
        resp.headers()
    );
    assert_eq!(fallback_reason(&resp), Some("external_vram"));
    let text = resp.text().await.unwrap();
    assert!(
        text.contains("from the cloud"),
        "answered by the cloud mock ({dialect:?}, stream {stream}): {text}"
    );
    if !stream {
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["model"], requested, "the body echoes the requested name");
    }

    let row = newest_log(f, requested, before).await;
    assert_eq!(row.status, 200);
    assert_eq!(row.upstream_name.as_deref(), Some("cloud"), "{row:?}");
    assert_eq!(
        row.fallback_reason.as_deref(),
        Some("external_vram"),
        "{row:?}"
    );
    assert_eq!(row.streamed, stream);

    assert!(f.runs().is_empty(), "nothing was started: {:?}", f.runs());
    let v = vram_status(&f.gateway).await;
    assert!(v["queue"].as_array().is_none_or(|q| q.is_empty()), "{v}");
}

/// The motivating case, over HTTP: a game holds 3 GiB of an 8 GiB card, the
/// 6.5 GiB chat model could not load even with every lmgw model gone, and
/// the chat rows inherit a cloud fallback. Both dialects, unary and
/// streamed, are answered by the fallback at once.
#[tokio::test]
async fn outside_use_sends_chat_to_its_fallback_at_once_in_both_dialects() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(3 * GIB);
    let _cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;

    for dialect in [Dialect::OpenAi, Dialect::Anthropic] {
        for stream in [false, true] {
            chat_falls_back_at_once(&f, dialect, "chat-model", stream, "cloud-chat").await;
        }
    }

    // And once the game is gone, the same request is the local model's again.
    f.world().outside = 0;
    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-lmgw-fallback").is_none());
    assert!(fallback_reason(&resp).is_none());
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
}

/// Busy lmgw models in the way are contention lmgw resolves itself: with a
/// fallback configured, the request still queues for the busy model, times
/// out as it always has, and is never sent to the fallback.
#[tokio::test]
async fn busy_lmgw_models_in_the_way_queue_even_with_a_fallback() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(GIB);
    let cloud = cloud_upstream(&f, "cloud-embed").await;
    set_embed_fallback(&f, "cloud-embed").await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    set_slot_busy(&f, true);
    set_queue_timeout(&f, 1).await;

    let resp = embed(&f.gateway).await;
    assert_eq!(resp.status(), 503);
    assert!(resp.headers().get("x-lmgw-fallback").is_none());
    assert!(fallback_reason(&resp).is_none());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "vram_queue_timeout", "{body}");
    assert_eq!(cloud.received_requests().await.unwrap().len(), 0);
    assert!(f.stops().is_empty(), "a generating model is never stopped");
}

/// A model larger than the card is a configuration error, not outside
/// pressure: `vram_too_large` as today, with a fallback configured and
/// outside use present.
#[tokio::test]
async fn a_model_larger_than_the_card_is_vram_too_large_even_with_a_fallback() {
    let f = fixture(4 * GIB, 8 * GIB, GIB, 512).await;
    f.attribute(GIB);
    let cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;

    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 507);
    assert!(resp.headers().get("x-lmgw-fallback").is_none());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "vram_too_large", "{body}");
    assert_eq!(cloud.received_requests().await.unwrap().len(), 0);
    assert!(f.runs().is_empty());
}

/// No fallback configured: the outside shortfall queues and times out
/// exactly as before §4.7 — no fallback headers, and not one `podman
/// inspect` (§7 item 14: rows without a fallback are unchanged).
#[tokio::test]
async fn without_a_fallback_an_outside_shortfall_queues_and_times_out() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(3 * GIB);
    set_queue_timeout(&f, 1).await;

    let started = std::time::Instant::now();
    let resp = chat(&f.gateway).await;
    assert!(started.elapsed() >= Duration::from_secs(1), "it waited");
    assert_eq!(resp.status(), 503);
    assert!(resp.headers().get("x-lmgw-fallback").is_none());
    assert!(fallback_reason(&resp).is_none());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "vram_queue_timeout", "{body}");
    assert!(f.pid_inspects().is_empty(), "{:?}", f.pid_inspects());
    assert!(f.runs().is_empty());
    let row = newest_log(&f, "chat-model", 0).await;
    assert!(row.fallback_reason.is_none(), "{row:?}");
}

/// Hold-only, over HTTP: with the switch off — and, separately, with one
/// lmgw container the driver does not list — the same shortfall with a
/// fallback configured takes today's path. The switch off asks podman
/// nothing at all.
#[tokio::test]
async fn with_the_switch_off_or_an_unattributed_container_the_request_queues() {
    // The switch off.
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(3 * GIB);
    let cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;
    set_fallback_on_external(&f, false).await;
    set_queue_timeout(&f, 1).await;

    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 503);
    assert!(resp.headers().get("x-lmgw-fallback").is_none());
    assert!(fallback_reason(&resp).is_none());
    assert_eq!(cloud.received_requests().await.unwrap().len(), 0);
    assert!(f.pid_inspects().is_empty(), "{:?}", f.pid_inspects());

    // Attribution missing for the resident embedder, which is generating on
    // its own port: nothing evicts it, so the trigger stays unavailable.
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(3 * GIB);
    f.world().unlisted.insert("embed-model".into());
    let cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;
    assert_eq!(embed(&f.gateway).await.status(), 200);
    f.world().busy.insert("embed-model".into());
    set_queue_timeout(&f, 1).await;

    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 503);
    assert!(resp.headers().get("x-lmgw-fallback").is_none());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "vram_queue_timeout", "{body}");
    assert_eq!(cloud.received_requests().await.unwrap().len(), 0);
    assert!(f.stops().is_empty(), "a generating model is never stopped");
}

/// A fallback this endpoint cannot use counts as no fallback. Legacy
/// `/v1/completions` only speaks to an openai-protocol upstream, so an
/// Anthropic-protocol fallback is not one it can send to: under the hold that
/// is the handler's own 400, but here the local model may still serve the
/// request, so it waits for room as it always has — and gets it once the
/// outside use ends.
#[tokio::test]
async fn a_fallback_the_endpoint_cannot_use_leaves_the_request_waiting_for_the_local_model() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(3 * GIB);
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
    set_global_fallback(&f, "claude-fallback").await;
    set_queue_timeout(&f, 5).await;
    // The chat model's container (the first to start) answers the legacy
    // route.
    Mock::given(method("POST"))
        .and(path("/v1/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "t1", "object": "text_completion", "created": 1, "model": "chat-model",
            "choices": [{"index": 0, "text": "from the card", "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        })))
        .mount(&f.first)
        .await;

    // The game quits half a second in.
    let world = f.world.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        world.lock().unwrap().outside = 0;
    });
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
        200,
        "the local model served it once there was room"
    );
    assert!(resp.headers().get("x-lmgw-fallback").is_none());
    assert!(fallback_reason(&resp).is_none());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["text"], "from the card", "{body}");
    assert_eq!(claude.received_requests().await.unwrap().len(), 0);
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
}

/// A fallback that cannot answer at all — one that does not resolve, or is
/// itself a local model — is a misconfiguration: under the hold a 503 naming
/// it, here a warning and today's path, since the local model may still
/// serve the request.
#[tokio::test]
async fn a_misconfigured_fallback_leaves_the_request_on_todays_path() {
    for fallback in ["no-such-alias", "embed/embed-model"] {
        let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
        f.attribute(3 * GIB);
        set_global_fallback(&f, fallback).await;
        set_queue_timeout(&f, 1).await;

        let resp = chat(&f.gateway).await;
        assert_eq!(resp.status(), 503, "{fallback}");
        assert!(
            resp.headers().get("x-lmgw-fallback").is_none(),
            "{fallback}"
        );
        let body: Value = resp.json().await.unwrap();
        assert_eq!(
            body["error"]["code"], "vram_queue_timeout",
            "{fallback}: {body}"
        );
        assert!(f.runs().is_empty(), "{fallback}: {:?}", f.runs());
    }
}

/// A non-chat class end to end: the embedder's own row fallback answers an
/// embedding request at once when outside use leaves no room for it.
#[tokio::test]
async fn outside_use_sends_an_embedding_to_its_row_fallback_at_once() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(6 * GIB);
    let cloud = cloud_upstream(&f, "cloud-embed").await;
    set_embed_fallback(&f, "cloud-embed").await;

    let waits = f.state.vram.waits_begun();
    let resp = embed(&f.gateway).await;
    assert_eq!(f.state.vram.waits_begun(), waits, "never queued");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("cloud-embed")
    );
    assert_eq!(fallback_reason(&resp), Some("external_vram"));
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["model"], "embed/embed-model");
    assert_eq!(body["data"][0]["embedding"], json!([0.25, 0.75]), "{body}");
    assert_eq!(cloud.received_requests().await.unwrap().len(), 1);
    assert!(f.runs().is_empty());
    let row = newest_log(&f, "embed/embed-model", 0).await;
    assert_eq!(row.fallback_reason.as_deref(), Some("external_vram"));
    assert_eq!(row.upstream_name.as_deref(), Some("cloud"));
}

/// quickdoc never takes a fallback's vectors (gpu-hold design §2), and the
/// outside-VRAM verdict is no exception: the in-process embedder is pinned,
/// so the same shortfall that sends `/v1/embeddings` to the cloud makes it
/// wait for room — and not one embedding is bought from the fallback.
#[tokio::test]
async fn quickdocs_embedder_never_falls_back_on_outside_use() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(6 * GIB);
    let cloud = cloud_upstream(&f, "cloud-embed").await;
    set_embed_fallback(&f, "cloud-embed").await;
    set_queue_timeout(&f, 1).await;

    let err = InProcessEmbedder::probe(f.state.clone(), "embed/embed-model")
        .await
        .expect_err("pinned: it waits for room, and there is none");
    assert!(
        err.to_string().contains("waited"),
        "today's queue timeout, not a fallback: {err}"
    );
    assert_eq!(
        cloud.received_requests().await.unwrap().len(),
        0,
        "not one embedding from the fallback"
    );
    assert!(f.runs().is_empty());
}

/// The image class too: its row fallback is judged by the image routes' own
/// check (a cloud route whose catalog does not rule it out) and answers a
/// generation at once.
#[tokio::test]
async fn outside_use_sends_an_image_generation_to_its_row_fallback() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(6 * GIB);
    let cloud = cloud_upstream(&f, "cloud-image").await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created": 1, "data": [{"b64_json": "aW1n"}],
        })))
        .mount(&cloud)
        .await;
    let id = add_image_model(&f, "pipeline", 3 * GIB).await;
    let image = store::get_image_model(&f.state.db, id)
        .await
        .unwrap()
        .unwrap();
    store::update_image_model(
        &f.state.db,
        id,
        &store::NewImageModel {
            model_id: image.model_id.clone(),
            files: image.files.clone(),
            enabled: true,
            idle_seconds: 0,
            hold_fallback_mode: HoldFallbackMode::Alias,
            hold_fallback: Some("cloud-image".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();

    let waits = f.state.vram.waits_begun();
    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/images/generations", f.gateway))
        .json(&json!({"model": "image/pipeline", "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(f.state.vram.waits_begun(), waits, "never queued");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("cloud-image")
    );
    assert_eq!(fallback_reason(&resp), Some("external_vram"));
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"][0]["b64_json"], "aW1n", "{body}");
    assert!(f.runs().is_empty());
    let row = newest_log(&f, "image/pipeline", 0).await;
    assert_eq!(row.fallback_reason.as_deref(), Some("external_vram"));
}

/// Review finding 6: the fallback's route check runs only once a verdict
/// says the fallback could answer. For the image routes it reads the
/// fallback upstream's catalog — a network fetch on a cold cache — which a
/// request that fits must not pay. Here the pipeline fits on the card, is
/// served locally, and the fallback's upstream is never asked a thing.
#[tokio::test]
async fn a_request_that_fits_never_runs_its_fallbacks_route_check() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(GIB);
    let cloud = cloud_upstream(&f, "cloud-image").await;
    let id = add_image_model(&f, "pipeline", 3 * GIB).await;
    let image = store::get_image_model(&f.state.db, id)
        .await
        .unwrap()
        .unwrap();
    store::update_image_model(
        &f.state.db,
        id,
        &store::NewImageModel {
            model_id: image.model_id.clone(),
            files: image.files.clone(),
            enabled: true,
            idle_seconds: 0,
            hold_fallback_mode: HoldFallbackMode::Alias,
            hold_fallback: Some("cloud-image".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    // The pipeline's container is the first to start.
    Mock::given(method("POST"))
        .and(wiremock::matchers::path_regex("images/generations$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created": 1, "data": [{"b64_json": "bG9jYWw="}],
        })))
        .mount(&f.first)
        .await;

    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/images/generations", f.gateway))
        .json(&json!({"model": "image/pipeline", "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-lmgw-fallback").is_none());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"][0]["b64_json"], "bG9jYWw=", "{body}");
    assert_eq!(f.runs(), vec!["pipeline".to_string()]);
    assert!(
        cloud.received_requests().await.unwrap().is_empty(),
        "no catalog read for a fallback that was never going to answer"
    );
}

/// A cloud alias on the fixture's `cloud` upstream whose owner override says
/// it cannot see images.
async fn blind_cloud_alias(f: &Fixture, alias: &str) {
    let upstream_id = f
        .state
        .snapshot()
        .upstreams
        .values()
        .find(|u| u.name == "cloud")
        .expect("cloud_upstream ran first")
        .id;
    store::insert_alias(
        &f.state.db,
        &store::NewAlias {
            alias: alias.into(),
            upstream_id,
            upstream_model_id: "gpt-cloud-text".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({"capabilities": {
                "task": "chat",
                "endpoints": ["/v1/chat/completions"],
                "input_modalities": ["text"],
                "vision": false,
                "source": "owner",
            }})),
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// Review finding 7: a chat request with image parts is never swapped to a
/// fallback that says it cannot see images — the provider would answer the
/// images with a 400, where today's path waits for the local model. A
/// fallback whose vision is unknown is used, and so is a text-only one for a
/// request without images.
#[tokio::test]
async fn images_are_not_swapped_to_a_fallback_that_cannot_see_them() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(3 * GIB);
    let cloud = cloud_chat(&f, "cloud-chat").await;
    blind_cloud_alias(&f, "cloud-blind").await;
    set_queue_timeout(&f, 1).await;
    let with_image = json!({"model": "chat-model", "messages": [{"role": "user", "content": [
        {"type": "text", "text": "what is this"},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}},
    ]}]});
    let send = |body: Value| {
        let gw = f.gateway.clone();
        async move {
            gw.client()
                .post(format!("{gw}/v1/chat/completions"))
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };

    // Vision unknown: absent means unknown, so it answers.
    set_global_fallback(&f, "cloud-chat").await;
    let resp = send(with_image.clone()).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(fallback_reason(&resp), Some("external_vram"));

    // A fallback that says it cannot see images: today's path.
    set_global_fallback(&f, "cloud-blind").await;
    let before = cloud.received_requests().await.unwrap().len();
    let resp = send(with_image).await;
    assert_eq!(resp.status(), 503);
    assert!(resp.headers().get("x-lmgw-fallback").is_none());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "vram_queue_timeout", "{body}");
    assert_eq!(cloud.received_requests().await.unwrap().len(), before);

    // The same fallback takes a request without images.
    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("cloud-blind")
    );
    assert!(f.runs().is_empty(), "{:?}", f.runs());
}

/// No waiter in the queue, and no reservation left behind.
async fn queue_and_ledger_clear(f: &Fixture) {
    let v = until_vram(f, "the waiter left the queue", |v| {
        v["queue"].as_array().is_none_or(|q| q.is_empty())
    })
    .await;
    assert!(
        v["resident"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["state"] != "reserved"),
        "no reservation leaked: {v}"
    );
}

/// Review finding 8: a request that queued with a fallback does not wait out
/// the whole queue timeout when the shortfall turns into outside use while
/// it waits. It queues for lmgw's own busy chat model (lmgw can make room:
/// Fits). Then the chat model finishes just as a game takes most of the
/// card: the wait evicts the idle chat model as it always has, and the
/// verdict, taken again, now says the memory that is short is not lmgw's —
/// the fallback answers long before the 30 s timeout, the waiter is gone
/// from the queue, and nothing is reserved.
#[tokio::test]
async fn a_queued_request_leaves_for_its_fallback_once_outside_use_is_the_shortfall() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(GIB);
    let cloud = cloud_upstream(&f, "cloud-embed").await;
    set_embed_fallback(&f, "cloud-embed").await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    set_slot_busy(&f, true);
    set_queue_timeout(&f, 30).await;

    let waits = f.state.vram.waits_begun();
    let started = std::time::Instant::now();
    let gw = f.gateway.clone();
    let waiting = tokio::spawn(async move { embed(&gw).await });
    until_vram(&f, "the embedding request queues", |v| {
        v["queue"].as_array().is_some_and(|q| q.len() == 1)
    })
    .await;
    assert_eq!(f.state.vram.waits_begun(), waits + 1);
    {
        let mut w = f.world();
        w.busy.clear();
        w.outside = 6 * GIB;
    }

    let resp = waiting.await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "answered while it waited, not at the timeout: {:?}",
        started.elapsed()
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(fallback_reason(&resp), Some("external_vram"));
    assert_eq!(cloud.received_requests().await.unwrap().len(), 1);
    assert_eq!(
        f.runs(),
        vec!["chat-model".to_string()],
        "the embedder was never started"
    );
    queue_and_ledger_clear(&f).await;
    let row = newest_log(&f, "embed/embed-model", 0).await;
    assert_eq!(row.fallback_reason.as_deref(), Some("external_vram"));
}

/// Review finding 8, the other way in: the first verdict is unavailable (an
/// lmgw container the driver does not list — a CPU-only row), so the request
/// takes today's path and evicts it. From then on lmgw's share is measured
/// again, the verdict taken while the request waits says the shortfall is
/// the game's, and the fallback answers.
#[tokio::test]
async fn a_request_that_waited_on_an_unattributed_container_falls_back_once_it_is_gone() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(3 * GIB);
    f.world().unlisted.insert("embed-model".into());
    let cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;
    assert_eq!(embed(&f.gateway).await.status(), 200);
    set_queue_timeout(&f, 30).await;

    let waits = f.state.vram.waits_begun();
    let started = std::time::Instant::now();
    let resp = chat(&f.gateway).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(resp.status(), 200);
    assert_eq!(fallback_reason(&resp), Some("external_vram"));
    assert_eq!(f.state.vram.waits_begun(), waits + 1, "it did queue first");
    assert_eq!(
        f.stops(),
        vec!["embed-model".to_string()],
        "today's path evicted the idle model first"
    );
    assert_eq!(cloud.received_requests().await.unwrap().len(), 1);
    queue_and_ledger_clear(&f).await;
}

/// Review finding 9: under the GPU hold the hold's own swap answers — reason
/// `hold`, however much outside use there is — and the outside-VRAM
/// machinery asks podman nothing and queues nothing.
#[tokio::test]
async fn under_the_hold_outside_use_changes_nothing_and_reads_no_pid() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(7 * GIB);
    let _cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;
    engage_hold(&f).await;

    let waits = f.state.vram.waits_begun();
    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("cloud-chat")
    );
    assert_eq!(fallback_reason(&resp), Some("hold"));
    assert_eq!(f.state.vram.waits_begun(), waits);
    assert!(f.pid_inspects().is_empty(), "{:?}", f.pid_inspects());
    let v = vram_status(&f.gateway).await;
    assert!(
        v["external_trigger_reason"]
            .as_str()
            .is_some_and(|r| r.contains("hold")),
        "{v}"
    );
    assert!(f.pid_inspects().is_empty(), "{:?}", f.pid_inspects());
}

/// Review finding 9: a model that is already up is served by it, fallback or
/// not, outside use or not — no verdict, so not one more `podman inspect`
/// than the one its start made, and nothing sent to the fallback.
#[tokio::test]
async fn a_model_already_up_is_served_locally_without_a_pid_read() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(GIB);
    let cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    // The start's own read of its PID (§12 entry 33), recorded in the
    // background.
    for _ in 0..200 {
        if !f.pid_inspects().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let asked = f.pid_inspects().len();
    assert_eq!(asked, 1, "{:?}", f.pid_inspects());
    // A game now fills the rest of the card.
    f.world().outside = 2 * GIB;

    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-lmgw-fallback").is_none());
    assert_eq!(f.pid_inspects().len(), asked, "{:?}", f.pid_inspects());
    assert_eq!(cloud.received_requests().await.unwrap().len(), 0);
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
}

/// Review finding 9: legacy `/v1/completions` is one more route the external
/// swap reaches — with an openai-protocol fallback, which is the one it can
/// use, it is answered at once by the fallback.
#[tokio::test]
async fn outside_use_sends_a_legacy_completion_to_its_fallback_at_once() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(3 * GIB);
    let cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;

    let waits = f.state.vram.waits_begun();
    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/completions", f.gateway))
        .json(&json!({"model": "chat-model", "prompt": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(f.state.vram.waits_begun(), waits, "never queued");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("cloud-chat")
    );
    assert_eq!(fallback_reason(&resp), Some("external_vram"));
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["text"], "from the cloud", "{body}");
    assert!(cloud
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|r| r.url.path() == "/v1/completions"));
    assert!(f.runs().is_empty());
    let row = newest_log(&f, "chat-model", 0).await;
    assert_eq!(row.fallback_reason.as_deref(), Some("external_vram"));
}

/// Review finding 9: the other pinned callers never swap at admission
/// either (§12 entry 30). quickdoc's in-process reranker names its model in
/// the search trace, and ingest counts tokens with the model's own
/// tokenizer (`gate::open_pinned` on the `/v1/count_tokens` check, which is
/// what `proxy::count_tokens_inner(pinned)` calls). With a fallback
/// configured and outside use leaving no room, both wait for room and time
/// out rather than use the fallback.
#[tokio::test]
async fn the_pinned_reranker_and_ingest_count_never_swap_on_outside_use() {
    use quickdoc_core::embed::Reranker;

    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(6 * GIB);
    let cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;
    set_queue_timeout(&f, 1).await;

    // A rerank row with its own fallback.
    f.world().size.insert("rank-model".into(), 3 * GIB);
    std::fs::File::create(f._models_dir.path().join("rank-model.gguf"))
        .unwrap()
        .set_len(3 * GIB)
        .unwrap();
    store::insert_aux_model(
        &f.state.db,
        &NewAuxModel {
            model_id: "rank-model".into(),
            gguf_path: "rank-model.gguf".into(),
            kind: AuxKind::Rerank,
            pooling: None,
            ctx_size: None,
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: HoldFallbackMode::Alias,
            hold_fallback: Some("cloud-chat".into()),
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();

    let waits = f.state.vram.waits_begun();
    let ranker =
        lmgw_core::quickdoc::InProcessReranker::for_alias(f.state.clone(), "embed/rank-model")
            .unwrap();
    let err = ranker
        .rerank("which", &["a".to_string(), "b".to_string()])
        .await
        .expect_err("pinned: it waits for room, and there is none");
    assert!(
        err.to_string().contains("waited"),
        "today's queue timeout, not a fallback: {err}"
    );

    let counted = lmgw_core::gate::open_pinned(
        &f.state,
        "chat-model",
        lmgw_core::gate::RouteCheck::Text("/v1/count_tokens"),
    )
    .await;
    let failed = counted.expect_err("pinned: it waits for room, and there is none");
    assert!(failed.headers.fallback().is_none());
    assert!(
        matches!(
            failed.error,
            lmgw_core::error::GatewayError::VramQueueTimeout { .. }
        ),
        "{:?}",
        failed.error
    );

    assert_eq!(f.state.vram.waits_begun(), waits + 2, "both queued");
    assert_eq!(cloud.received_requests().await.unwrap().len(), 0);
    assert!(f.runs().is_empty(), "{:?}", f.runs());
    assert!(
        f.pid_inspects().is_empty(),
        "no verdict was taken for either"
    );
}
