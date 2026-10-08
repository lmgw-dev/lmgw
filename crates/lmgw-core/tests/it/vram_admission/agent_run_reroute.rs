//! An in-process agent run whose classify call the gate re-routes before
//! anything was sent (billable-units design §7): the run is admitted on the
//! local ladder model, one item's prompt needs a climb that outside VRAM
//! leaves no room for, and that call is answered by the cloud fallback. The
//! run's cost is the sum of its calls' rows, each priced on the route that
//! answered it, never the summed usage priced on the local route the run
//! started on, which would read the cloud call as a free 0.

use lmgw_core::config::{PriceScope, PriceUnit};
use lmgw_core::pricing::{PriceSource, Prices};

use super::*;
use crate::support::mcp_stub::{answer, answering, register, McpStub};

const FALLBACK: &str = "cloud-chat";

/// A batch agent on the ladder model: one classify call per item, the
/// item's subject as the whole prompt.
fn doc() -> String {
    json!({
        "schema_version": 1,
        "id": "rerouted",
        "name": "Rerouted",
        "model": { "alias": LADDER },
        "config": { "schema": { "type": "object", "properties": {
            "categories": { "type": "array", "items": { "type": "string" },
                            "default": ["Work", "Finance"] }
        } } },
        "tools": [ { "label": "gws" } ],
        "run": { "kind": "batch",
            "source": { "tool": "gws__search", "args": {} },
            "items_path": "/messages",
            "item": {
                "id": "{{item.id}}",
                "columns": { "subject": "{{item.subject}}" },
                "user": "{{item.subject}}",
                "output": { "field": "category", "enum_from": "config.categories",
                            "fallback": "Other" }
            },
            "review": { "editable": ["category"] }
        }
    })
    .to_string()
}

/// The source's items: a short subject the base rung serves, and one of 200
/// words, which needs rung 3.
async fn mailbox(f: &Fixture) -> McpStub {
    let long = words("w", 200);
    let stub = answering(
        json!([{
            "name": "search",
            "description": "search",
            "inputSchema": {"type": "object", "properties": {}},
        }]),
        false,
        answer(move |_name, _args| {
            let long = long.clone();
            async move {
                json!({
                    "content": [],
                    "structuredContent": { "messages": [
                        { "id": "short", "subject": "hello" },
                        { "id": "long", "subject": long },
                    ] },
                    "isError": false,
                })
            }
        }),
    )
    .await;
    register(&f.state, "gws", "gws", &stub.url, true, None).await;
    stub
}

async fn price_fallback(f: &Fixture, unit: PriceUnit, p: Prices, price: Option<f64>) {
    store::upsert_price(
        &f.state.db,
        PriceScope::Alias,
        FALLBACK,
        unit,
        &p,
        price,
        None,
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// Install the agent, run its classify phase to the end, and return the
/// run's result and its calls' rows.
async fn classify(f: &Fixture) -> (Value, Vec<store::RequestLogRow>) {
    let resp = f
        .gateway
        .client()
        .post(format!("{}/api/agents/import?replace=1", f.gateway))
        .header("content-type", "application/json")
        .body(doc())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "import: {}", resp.text().await.unwrap());
    let resp = f
        .gateway
        .client()
        .post(format!("{}/api/op/agent_run", f.gateway))
        .json(&json!({ "id": "rerouted", "phase": "classify" }))
        .send()
        .await
        .unwrap();
    let res: Value = resp.json().await.unwrap();
    let job_id = res["job_id"]
        .as_i64()
        .unwrap_or_else(|| panic!("no job: {res}"));
    let mut job = store::get_job(&f.state.db, job_id).await.unwrap().unwrap();
    for _ in 0..1000 {
        if !matches!(job.status.as_str(), "queued" | "running") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        job = store::get_job(&f.state.db, job_id).await.unwrap().unwrap();
    }
    assert_eq!(job.status, "done", "{:?}", job.error);
    let result: Value = serde_json::from_str(job.result.as_deref().unwrap()).unwrap();
    let rows: Vec<store::RequestLogRow> = store::query_logs(
        &f.state.db,
        &store::LogFilter {
            alias: Some(LADDER.into()),
            limit: 1000,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .into_iter()
    .filter(|r| r.ingress_proto == "agent")
    .collect();
    (result, rows)
}

/// The ladder model on a card that outside VRAM leaves 6 of 12 GiB on, the
/// cloud fallback configured: the base rung fits, rung 3 does not.
async fn ladder_with_fallback() -> (Fixture, MockServer, McpStub) {
    let f = ladder_fixture(12 * GIB, 0).await;
    f.attribute(6 * GIB);
    let cloud = cloud_chat(&f, FALLBACK).await;
    set_global_fallback(&f, FALLBACK).await;
    let stub = mailbox(&f).await;
    (f, cloud, stub)
}

/// The re-routed call's row is the cloud's and priced there; the local
/// call's is a real 0. The run's cost is their sum — exactly what its rows
/// say, `sum_micro` over their `cost_micro` — not the local route's 0.
#[tokio::test]
async fn a_rerouted_call_is_priced_on_the_route_that_answered_it() {
    let (f, cloud, _stub) = ladder_with_fallback().await;
    price_fallback(
        &f,
        PriceUnit::PerRequest,
        Prices {
            source: PriceSource::Manual,
            ..Default::default()
        },
        Some(0.005),
    )
    .await;

    let (result, rows) = classify(&f).await;
    assert_eq!(rows.len(), 2, "{rows:#?}");
    assert!(
        !cloud.received_requests().await.unwrap().is_empty(),
        "the cloud answered"
    );
    let (rerouted, local): (Vec<_>, Vec<_>) = rows
        .iter()
        .partition(|r| r.upstream_name.as_deref() == Some("cloud"));
    let (rerouted, local) = (rerouted[0], local[0]);
    assert_eq!(
        rerouted.fallback_reason.as_deref(),
        Some("external_vram"),
        "{rerouted:?}"
    );
    assert_eq!(rerouted.cost_micro, Some(5_000), "{rerouted:?}");
    assert_eq!(local.cost_micro, Some(0), "{local:?}");
    assert_eq!(local.price_source.as_deref(), Some("free_local"));

    assert_eq!(result["model_calls"], json!(2), "{result}");
    assert_eq!(
        result["cost_micro"],
        json!(5_000),
        "the cloud call's fee, not the local route's 0: {result}"
    );
    let summed = rows.iter().skip(1).fold(rows[0].cost_micro, |sum, r| {
        lmgw_core::pricing::sum_micro(sum, r.cost_micro)
    });
    assert_eq!(result["cost_micro"], json!(summed), "the run is its rows");
}

/// The fallback prices tokens, and its stream reports none: its row is
/// unknown, so the run is unknown — NULL, not the local call's 0 and not a
/// 0 for the whole run.
#[tokio::test]
async fn a_rerouted_call_its_route_cannot_price_leaves_the_run_unknown() {
    let (f, _cloud, _stub) = ladder_with_fallback().await;
    price_fallback(
        &f,
        PriceUnit::PerMtok,
        Prices {
            price_in: Some(3.0),
            price_out: Some(15.0),
            source: PriceSource::Manual,
            ..Default::default()
        },
        None,
    )
    .await;

    let (result, rows) = classify(&f).await;
    assert_eq!(rows.len(), 2, "{rows:#?}");
    let rerouted = rows
        .iter()
        .find(|r| r.upstream_name.as_deref() == Some("cloud"))
        .expect("a call was re-routed");
    assert_eq!(rerouted.cost_micro, None, "{rerouted:?}");
    assert!(
        rows.iter().any(|r| r.cost_micro == Some(0)),
        "the local call is free: {rows:#?}"
    );
    assert_eq!(result["cost_micro"], Value::Null, "{result}");
}
