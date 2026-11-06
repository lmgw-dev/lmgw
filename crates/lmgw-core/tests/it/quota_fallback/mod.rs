//! The fallback reasons a cloud hop brings (openai egress design §14.3–
//! §14.8).
//!
//! - [`disabled`]: a disabled plain alias, an alias on a disabled upstream
//!   and a disabled local row answered by their fallbacks (reason
//!   `disabled`), today's answer without one, a disabled hop along a
//!   chain, and a disabled name that shadows nothing keeping its old
//!   resolution.
//! - [`memory`]: an upstream remembered out of quota skipped to the
//!   fallback at resolve, the re-check, Check now, an edit, the views.
//! - [`send`]: an exhausted quota and an unreachable upstream at the chat
//!   send (§14.4) — unary, streamed and `/v1/messages`, each answered by the
//!   fallback with the attempt's own row; a rate limit untouched; no
//!   fallback; a chain; a local fallback under the hold; `Retry-After`; a
//!   `2xx` clearing what was remembered.
//! - [`native`]: the same on a native `/v1/responses` forward, the fallback
//!   served as if the client had named it, and a refused parameter or off
//!   retried before the hop hands on.
//! - [`priced`]: the priced check on the route a non-chat request is
//!   served on (§14.9).
//! - [`left`]: a client that leaves while its request is handed on keeps
//!   the attempt's row.
//! - [`runs`]: a synthesized `/v1/responses` run's turns handed on as the
//!   run's request — a guest's as a guest.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{self, Gw};
use crate::support::fallbacks::set_fallback;
use crate::support::gpu_world::{Gpu, GIB};

mod disabled;
mod left;
mod memory;
mod native;
mod priced;
mod runs;
mod send;

async fn world() -> (Gpu, Gw) {
    let g = Gpu::new(24 * GIB, 2, 2).await;
    let gw = common::serve(g.state.clone()).await;
    (g, gw)
}

/// A chat request for `model`.
async fn ask(gw: &Gw, model: &str) -> reqwest::Response {
    gw.client()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
}

fn h(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

/// A 200 for `requested` that the last hop of `chain` answered with
/// `content`, the three fallback headers saying so.
async fn answered_along(resp: reqwest::Response, requested: &str, chain: &str, content: &str) {
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    let last = chain.rsplit(", ").next().unwrap();
    let (name, reason) = last.split_once(";reason=").unwrap();
    assert_eq!(
        h(&resp, "x-lmgw-fallback").as_deref(),
        Some(name.trim_matches('"'))
    );
    assert_eq!(h(&resp, "x-lmgw-fallback-reason").as_deref(), Some(reason));
    assert_eq!(h(&resp, "x-lmgw-fallback-chain").as_deref(), Some(chain));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], content, "{v}");
    assert_eq!(v["model"], requested, "the body echoes the requested name");
}

/// Run `sql` on the world's database and reload the snapshot.
async fn exec(g: &Gpu, sql: &'static str) {
    sqlx::query(sql).execute(&g.state.db).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
}

/// OpenAI's answer for an exhausted quota.
fn quota_body() -> Value {
    json!({"error": {"message": "You exceeded your current quota, please check your plan and \
                                 billing details.",
                     "type": "insufficient_quota", "param": null,
                     "code": "insufficient_quota"}})
}

/// A plain alias `alias` on an `openai` platform row of its own
/// (`platform-<alias>`), whose mock answers every chat send with `status`, `body`
/// and the `headers`; the mock.
async fn platform(
    g: &Gpu,
    alias: &str,
    status: u16,
    body: Value,
    headers: &[(&str, &str)],
) -> MockServer {
    // Not the pooled server: a pooled one is shared with whatever else in
    // this process asked for one, and a reset of it races the mounts.
    let mock = MockServer::builder().start().await;
    let mut answer = ResponseTemplate::new(status).set_body_json(body);
    for (k, v) in headers {
        answer = answer.insert_header(*k, *v);
    }
    // The platform's chat sends take the Responses wire (openai egress
    // design §7.2); its error bodies are the same on both wires.
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(answer)
        .mount(&mock)
        .await;
    upstream_alias(g, alias, &mock.uri(), Protocol::Openai).await;
    mock
}

/// A plain alias `alias` on a row of its own (`platform-<alias>`) at
/// `base_url`; the row's id.
async fn upstream_alias(g: &Gpu, alias: &str, base_url: &str, protocol: Protocol) -> i64 {
    let up = store::insert_upstream(
        &g.state.db,
        &store::NewUpstream {
            name: format!("platform-{alias}"),
            protocol,
            kind: UpstreamKind::Generic,
            base_url: base_url.into(),
            api_key: Some("sk-test".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &g.state.db,
        &store::NewAlias {
            alias: alias.into(),
            upstream_id: up,
            upstream_model_id: "gpt-test".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    g.state.reload_snapshot().await.unwrap();
    up
}

/// The request rows for `alias`, oldest first, once there are `n`.
async fn rows(g: &Gpu, alias: &str, n: usize) -> Vec<store::RequestLogRow> {
    for _ in 0..300 {
        let mut rows = store::query_logs(
            &g.state.db,
            &store::LogFilter {
                alias: Some(alias.into()),
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if rows.len() >= n {
            rows.reverse();
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("fewer than {n} rows for '{alias}'");
}
