//! A row a fallback answered is priced under the alias that answered
//! (billable-units design §12 Q2): that alias's own price rows apply, looked
//! up with its upstream and upstream model, while `requested_alias` stays the
//! name the client asked for, which Logs, Usage and the fallback marker read.
//!
//! Driven over HTTP on `support/gpu_world.rs`: a local chat row under the GPU
//! hold, whose hold fallback is a cloud alias or a passthrough name. Every
//! upstream answers 1 prompt and 1 completion token, so a sheet of 1000 in
//! and 2000 out per 1M tokens costs 3000 micro.

use std::time::Duration;

use lmgw_core::config::{upstream_scope_key, PriceScope, Protocol, UpstreamKind};
use lmgw_core::pricing::{PriceSource, Prices};
use lmgw_core::store::{self, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{self, Gw};
use crate::support::gpu_world::{Gpu, GIB};

/// The local chat row every request here names.
const LOCAL: &str = "chat-model";

fn sheet(price_in: f64, price_out: f64) -> Prices {
    Prices {
        price_in: Some(price_in),
        price_out: Some(price_out),
        source: PriceSource::Manual,
        ..Default::default()
    }
}

async fn price(g: &Gpu, scope: PriceScope, key: &str, p: Prices) {
    store::upsert_price(
        &g.state.db,
        scope,
        key,
        lmgw_core::config::PriceUnit::PerMtok,
        &p,
        None,
        None,
    )
    .await
    .unwrap();
    g.state.reload_snapshot().await.unwrap();
}

/// The local row, the cloud alias `cloud-chat` beside it, and `fallback` as
/// the hold's fallback.
async fn world(fallback: &str) -> Gpu {
    let g = Gpu::new(8 * GIB, 2, 5).await;
    g.model(LOCAL, GIB).await;
    g.cloud("cloud-chat", None).await;
    let mut s = g.state.snapshot().settings.clone();
    s.hold.fallback_alias = Some(fallback.into());
    store::save_settings(&g.state.db, &s).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
    g
}

async fn chat(gw: &Gw, model: &str) -> reqwest::Response {
    let resp = gw
        .client()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    resp
}

fn header<'r>(resp: &'r reqwest::Response, name: &str) -> Option<&'r str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

/// The newest request-log row for `alias`, waited for: the row is written
/// as the response goes out.
async fn newest_log(g: &Gpu, alias: &str) -> store::RequestLogRow {
    for _ in 0..200 {
        let rows = store::query_logs(
            &g.state.db,
            &store::LogFilter {
                alias: Some(alias.into()),
                limit: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if let Some(row) = rows.into_iter().next() {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no request-log row for '{alias}'");
}

/// The case §12 Q2 names: a local alias whose fallback is a cloud alias
/// priced under its own name. Before the fix the lookup went to the local
/// alias's name, found nothing, and the row stayed unpriced however the
/// cloud alias was priced.
#[tokio::test]
async fn a_fallback_row_takes_the_answering_alias_manual_price() {
    let g = world("cloud-chat").await;
    price(&g, PriceScope::Alias, "cloud-chat", sheet(1000.0, 2000.0)).await;
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();
    let gw = common::serve(g.state.clone()).await;

    let resp = chat(&gw, LOCAL).await;
    assert_eq!(header(&resp, "x-lmgw-fallback"), Some("cloud-chat"));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["model"], LOCAL, "the body echoes the requested name");

    let row = newest_log(&g, LOCAL).await;
    assert_eq!(
        row.requested_alias, LOCAL,
        "the row keeps the name asked for"
    );
    assert_eq!(row.fallback_reason.as_deref(), Some("hold"), "{row:?}");
    assert_eq!(row.upstream_model.as_deref(), Some("gpt"));
    assert_eq!(row.price_source.as_deref(), Some("manual"), "{row:?}");
    assert_eq!(row.price_in, Some(1000.0));
    assert_eq!(row.price_out, Some(2000.0));
    assert_eq!(row.cost_micro, Some(3000), "1 × 1000 + 1 × 2000");
}

/// A fallback named by its passthrough name, priced on its upstream model,
/// as the catalog prices a model without an alias. A manual row under the
/// requested local name is not the answering alias's and stays unused: the
/// alias tier is the fallback's, then the upstream model beside it.
#[tokio::test]
async fn a_fallback_to_a_passthrough_name_takes_its_upstream_model_price() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "created": 0, "model": "gpt-pass",
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "from the passthrough"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        })))
        .mount(&mock)
        .await;
    let g = world("pt/gpt-pass").await;
    let up = store::insert_upstream(
        &g.state.db,
        &NewUpstream {
            name: "passthrough".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: true,
            expose_prefix: "pt".into(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    price(
        &g,
        PriceScope::UpstreamModel,
        &upstream_scope_key(up, "gpt-pass"),
        sheet(1000.0, 2000.0),
    )
    .await;
    price(&g, PriceScope::Alias, LOCAL, sheet(7.0, 7.0)).await;
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();
    let gw = common::serve(g.state.clone()).await;

    let resp = chat(&gw, LOCAL).await;
    assert_eq!(header(&resp, "x-lmgw-fallback"), Some("pt/gpt-pass"));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(
        v["choices"][0]["message"]["content"],
        "from the passthrough"
    );

    let row = newest_log(&g, LOCAL).await;
    assert_eq!(row.requested_alias, LOCAL);
    assert_eq!(row.fallback_reason.as_deref(), Some("hold"), "{row:?}");
    assert_eq!(row.upstream_name.as_deref(), Some("passthrough"));
    assert_eq!(row.upstream_model.as_deref(), Some("gpt-pass"));
    assert_eq!(row.price_source.as_deref(), Some("manual"), "{row:?}");
    assert_eq!(
        row.price_in,
        Some(1000.0),
        "the upstream model's row, not 7"
    );
    assert_eq!(row.cost_micro, Some(3000));
}

/// Without a fallback nothing changes: a cloud alias named directly is
/// priced under its own name, and the local row it would stand in for is
/// free while the hold is off.
#[tokio::test]
async fn a_request_no_fallback_answered_is_priced_as_before() {
    let g = world("cloud-chat").await;
    price(&g, PriceScope::Alias, "cloud-chat", sheet(1000.0, 2000.0)).await;
    let gw = common::serve(g.state.clone()).await;

    let resp = chat(&gw, "cloud-chat").await;
    assert_eq!(header(&resp, "x-lmgw-fallback"), None);
    let row = newest_log(&g, "cloud-chat").await;
    assert_eq!(row.requested_alias, "cloud-chat");
    assert_eq!(row.fallback_reason, None);
    assert_eq!(row.price_source.as_deref(), Some("manual"), "{row:?}");
    assert_eq!(row.cost_micro, Some(3000));

    let resp = chat(&gw, LOCAL).await;
    assert_eq!(header(&resp, "x-lmgw-fallback"), None);
    let row = newest_log(&g, LOCAL).await;
    assert_eq!(row.fallback_reason, None);
    assert_eq!(row.upstream_model.as_deref(), Some(LOCAL));
    assert_eq!(row.price_source.as_deref(), Some("free_local"), "{row:?}");
    assert_eq!(row.cost_micro, Some(0));
}
