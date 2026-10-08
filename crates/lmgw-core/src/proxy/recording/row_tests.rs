//! The row writers price every unit (billable-units design §3.2, §4.5, §10):
//! the `requests` default, a caller's measured quantities, the local zero
//! (Q1) at the row level, the snapshot columns, and the live frame.

use std::time::Instant;

use super::*;
use crate::config::{PriceScope, PriceUnit, Protocol, Upstream, UpstreamKind, AUDIO_UPSTREAM_ID};
use crate::ingress::ClientProto;
use crate::pricing::{PriceSource, Prices};
use crate::state::AppState;
use crate::store::RequestLogRow;

const CLOUD: i64 = 5;

fn route(id: i64) -> Route {
    Route {
        upstream: Upstream {
            id,
            name: "up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            llama: None,
        },
        upstream_model: "m".into(),
        param_defaults: Default::default(),
        fallback: None,
    }
}

async fn price_unit(state: &SharedState, alias: &str, unit: PriceUnit, price: f64) {
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        alias,
        unit,
        &Prices {
            source: PriceSource::Manual,
            ..Default::default()
        },
        Some(price),
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

async fn price_tokens(state: &SharedState, alias: &str) {
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        alias,
        PriceUnit::PerMtok,
        &Prices {
            price_in: Some(3.0),
            price_out: Some(15.0),
            source: PriceSource::Catalog,
            ..Default::default()
        },
        None,
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

fn tokens(prompt: u64, completion: u64) -> Usage {
    Usage {
        prompt_tokens: Some(prompt),
        completion_tokens: Some(completion),
        ..Default::default()
    }
}

async fn newest(state: &SharedState, alias: &str) -> RequestLogRow {
    store::query_logs(
        &state.db,
        &store::LogFilter {
            alias: Some(alias.into()),
            limit: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .into_iter()
    .next()
    .expect("a row")
}

/// One row through [`record`], read back.
#[allow(clippy::too_many_arguments)]
async fn write(
    state: &SharedState,
    alias: &str,
    route: Option<&Route>,
    status: u16,
    usage: Usage,
    error: Option<(&str, String)>,
    quantities: Quantities,
) -> RequestLogRow {
    let ctx = RequestCtx::default();
    state.telemetry.request_started();
    record(
        LogParams {
            state,
            proto: ClientProto::OpenaiChat,
            ctx: &ctx,
            alias: alias.into(),
            route,
            started: Instant::now(),
            streamed: false,
            class: RequestClass::Chat,
            timings: None,
            max_tokens_clamped: None,
            fallback: None,
            rung: None,
            degraded: None,
            quantities,
        },
        status,
        None,
        usage,
        error,
    )
    .await;
    newest(state, alias).await
}

async fn unknown_requests(state: &SharedState, alias: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COALESCE(SUM(cost_unknown_requests), 0) FROM usage_hourly WHERE alias = ?1",
    )
    .bind(alias)
    .fetch_one(&state.db)
    .await
    .unwrap()
}

/// §3.3's mixed golden at the row level: tokens plus a per-request fee, each
/// part and each rate on the row.
#[tokio::test]
async fn an_answered_row_pays_its_request_fee_beside_its_tokens() {
    let state = AppState::init_for_tests().await.unwrap();
    price_tokens(&state, "mixed").await;
    price_unit(&state, "mixed", PriceUnit::PerRequest, 0.005).await;
    let r = route(CLOUD);

    let row = write(
        &state,
        "mixed",
        Some(&r),
        200,
        tokens(1_000_000, 100_000),
        None,
        Quantities::default(),
    )
    .await;
    assert_eq!(row.cost_in_micro, Some(3_000_000));
    assert_eq!(row.cost_out_micro, Some(1_500_000));
    assert_eq!(row.cost_units_micro, Some(5_000));
    assert_eq!(row.cost_micro, Some(4_505_000));
    assert_eq!(row.price_in, Some(3.0));
    assert_eq!(row.price_per_request, Some(0.005));
    assert_eq!(row.price_per_audio_minute, None);
    assert_eq!(row.price_source.as_deref(), Some("manual"), "manual if any");
}

/// The `requests` default (§4.5): only a routed, answered, not-canceled row
/// counts its request on its own; a caller that knows better says so.
#[tokio::test]
async fn a_row_pays_a_fee_only_for_a_request_it_knows_was_answered() {
    let state = AppState::init_for_tests().await.unwrap();
    price_unit(&state, "fee", PriceUnit::PerRequest, 0.005).await;
    let r = route(CLOUD);
    let canceled = || Some(("canceled", "client disconnected".to_string()));

    let row = write(
        &state,
        "fee",
        Some(&r),
        200,
        tokens(10, 0),
        None,
        Default::default(),
    )
    .await;
    assert_eq!(row.cost_micro, Some(5_000), "answered: {row:?}");
    assert_eq!(row.cost_units_micro, Some(5_000));

    // A stop may precede the answer: unknown, the rate still on the row,
    // and the row in the remainder, since it spent tokens.
    let before = unknown_requests(&state, "fee").await;
    let row = write(
        &state,
        "fee",
        Some(&r),
        200,
        tokens(10, 0),
        canceled(),
        Default::default(),
    )
    .await;
    assert_eq!(row.cost_micro, None, "{row:?}");
    assert_eq!(row.cost_units_micro, None);
    assert_eq!(row.price_per_request, Some(0.005));
    assert_eq!(row.price_source.as_deref(), Some("unknown"));
    assert_eq!(unknown_requests(&state, "fee").await, before + 1);

    // …unless the relay knows its upstream answered.
    let row = write(
        &state,
        "fee",
        Some(&r),
        200,
        tokens(10, 0),
        canceled(),
        Quantities::answered(),
    )
    .await;
    assert_eq!(row.cost_micro, Some(5_000), "{row:?}");

    let upstream = || Some(("upstream", "boom".to_string()));
    let row = write(
        &state,
        "fee",
        Some(&r),
        502,
        Usage::default(),
        upstream(),
        Default::default(),
    )
    .await;
    assert_eq!(row.cost_micro, None, "a failed call: {row:?}");

    let refused = || Some(("key_scope", "no".to_string()));
    let row = write(
        &state,
        "fee",
        None,
        403,
        Usage::default(),
        refused(),
        Default::default(),
    )
    .await;
    assert_eq!(row.cost_micro, None, "no route, no upstream: {row:?}");

    // A caller's own count stands, a synthesis row's clauses for one.
    let three = Quantities {
        requests: Some(3),
        ..Default::default()
    };
    let row = write(
        &state,
        "fee",
        Some(&r),
        200,
        Usage::default(),
        canceled(),
        three,
    )
    .await;
    assert_eq!(row.cost_micro, Some(15_000), "{row:?}");
}

/// A route's measurement reaches the row and its price; one the scope does
/// not price is recorded all the same.
#[tokio::test]
async fn measured_quantities_reach_the_row_and_its_price() {
    let state = AppState::init_for_tests().await.unwrap();
    price_unit(&state, "tts-1", PriceUnit::PerMchar, 15.0).await;
    let r = route(CLOUD);
    let q = Quantities {
        chars_in: Some(1_234),
        audio_in_ms: Some(900),
        ..Default::default()
    };
    let row = write(&state, "tts-1", Some(&r), 200, Usage::default(), None, q).await;
    assert_eq!(row.chars_in, Some(1_234));
    assert_eq!(row.audio_in_ms, Some(900), "recorded though not priced");
    assert_eq!(row.images_out, None, "not measured: NULL, not 0");
    assert_eq!(row.cost_micro, Some(18_510));
    assert_eq!(row.price_per_mchar, Some(15.0));

    // The same text with no count is unknown on a per-character scope.
    let row = write(
        &state,
        "tts-1",
        Some(&r),
        200,
        Usage::default(),
        None,
        Default::default(),
    )
    .await;
    assert_eq!(row.chars_in, None);
    assert_eq!(row.cost_micro, None);
    assert_eq!(row.price_per_mchar, Some(15.0));
}

/// Q1 at the row level (extends `local_routes_are_free_not_unpriced`): a
/// local row with no token count is a real 0, not unpriced, and stays out
/// of the remainder; so is one that failed.
#[tokio::test]
async fn a_local_row_is_a_real_zero_without_tokens() {
    let state = AppState::init_for_tests().await.unwrap();
    // A price row under the local name changes nothing.
    price_unit(&state, "local-asr", PriceUnit::PerAudioMinute, 0.006).await;
    let r = route(AUDIO_UPSTREAM_ID);
    let q = Quantities {
        audio_in_ms: Some(27_000),
        ..Default::default()
    };
    let row = write(
        &state,
        "local-asr",
        Some(&r),
        200,
        Usage::default(),
        None,
        q,
    )
    .await;
    assert_eq!(row.cost_micro, Some(0), "{row:?}");
    assert_eq!(row.price_source.as_deref(), Some("free_local"));
    assert_eq!(row.audio_in_ms, Some(27_000), "measured on local rows too");
    assert_eq!(row.cost_units_micro, None);
    assert_eq!(row.price_per_audio_minute, None);
    assert_eq!(unknown_requests(&state, "local-asr").await, 0);

    let failed = Some(("upstream", "container exited".to_string()));
    let row = write(
        &state,
        "local-asr",
        Some(&r),
        502,
        Usage::default(),
        failed,
        Default::default(),
    )
    .await;
    assert_eq!(row.cost_micro, Some(0), "{row:?}");
}

/// The in-process writer applies the same default.
#[tokio::test]
async fn the_in_process_writer_counts_an_answered_request() {
    let state = AppState::init_for_tests().await.unwrap();
    price_unit(&state, "agent-model", PriceUnit::PerRequest, 0.01).await;
    let r = route(CLOUD);
    for (status, error, want) in [
        (200, None, Some(10_000)),
        (200, Some(("canceled", "stopped".to_string())), None),
        (502, Some(("upstream", "boom".to_string())), None),
    ] {
        state.telemetry.request_started();
        crate::proxy::record_in_process(
            crate::proxy::InProcessLog {
                key: KeyRef::default(),
                ingress_proto: "agent",
                alias: "agent-model",
                route: &r,
                started: Instant::now(),
                streamed: true,
                class: RequestClass::Chat,
                timings: None,
                max_tokens_clamped: None,
                fallback: None,
                rung: None,
                degraded: None,
                quantities: Default::default(),
            },
            status,
            None,
            tokens(5, 5),
            error.as_ref().map(|(k, m)| (*k, m.clone())),
            &state,
        )
        .await;
        let row = newest(&state, "agent-model").await;
        assert_eq!(row.cost_micro, want, "{status} {error:?}");
    }
}

/// The free-form writer prices its rows too: an answered passthrough pays
/// its fee, a failure on a local route is a real 0, and one on a cloud
/// route stays unknown.
#[tokio::test]
async fn the_free_form_writer_prices_its_rows() {
    let state = AppState::init_for_tests().await.unwrap();
    price_unit(&state, "native", PriceUnit::PerRequest, 0.002).await;
    let ctx = RequestCtx::default();
    let cloud = route(CLOUD);
    let local = route(AUDIO_UPSTREAM_ID);

    record_passthrough(
        &state,
        &ctx,
        "responses",
        "native",
        &cloud,
        None,
        Instant::now(),
        Some(3),
        false,
        200,
        None,
        None,
    )
    .await;
    let row = newest(&state, "native").await;
    assert_eq!(row.cost_micro, Some(2_000), "{row:?}");
    assert_eq!(row.price_per_request, Some(0.002));

    let e = GatewayError::Upstream {
        status: 502,
        message: "boom".into(),
        provider_type: None,
    };
    record_request_failure(
        &state,
        &ctx,
        "responses",
        "native",
        Some(&cloud),
        None,
        Instant::now(),
        &e,
    )
    .await;
    assert_eq!(newest(&state, "native").await.cost_micro, None);

    record_request_failure(
        &state,
        &ctx,
        "responses",
        "on-gpu",
        Some(&local),
        None,
        Instant::now(),
        &e,
    )
    .await;
    let row = newest(&state, "on-gpu").await;
    assert_eq!(row.cost_micro, Some(0), "{row:?}");
    assert_eq!(row.price_source.as_deref(), Some("free_local"));
}

/// The live `request` frame carries what the stored row carries.
#[tokio::test]
async fn the_live_frame_carries_the_quantities() {
    let state = AppState::init_for_tests().await.unwrap();
    let mut rx = state.telemetry.subscribe();
    let r = route(CLOUD);
    let q = Quantities {
        audio_in_ms: Some(27_400),
        chars_in: Some(12),
        images_out: Some(2),
        requests: None,
    };
    write(&state, "frame", Some(&r), 200, Usage::default(), None, q).await;
    let summary = loop {
        match rx.recv().await.unwrap() {
            crate::telemetry::Event::Request(s) if s.requested_alias == "frame" => break s,
            _ => {}
        }
    };
    assert_eq!(summary.audio_in_ms, Some(27_400));
    assert_eq!(summary.chars_in, Some(12));
    assert_eq!(summary.images_out, Some(2));
    let wire = serde_json::to_value(&*summary).unwrap();
    assert_eq!(wire["audio_in_ms"], 27_400);
    let dto: lmgw_api_types::RequestRow = serde_json::from_value(wire).unwrap();
    assert_eq!(dto.images_out, Some(2), "the frame decodes as the DTO");
}
