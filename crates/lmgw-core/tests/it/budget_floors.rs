//! Budget lower bounds for the unpriced rows a client can cause (openai
//! egress design §13, decision 22). The row stays unpriced; only the
//! budgets count the bound (`request_logs.budget_floor_micro`, summed into
//! `usage_hourly` and read back by the reseed).
//!
//! - [`tier`]: an `openai` answer served at a tier that costs at least
//!   Standard, with no row at that tier, floors at the standard price.
//! - [`flex`]: flex asked for by a budgeted caller on a scope with no flex
//!   price is refused before the send.
//! - [`cut_chat`]: a platform chat stream cut before its usage chunk floors
//!   from a local count (the native Responses one is in
//!   `responses_native::budget_floor`).
//! - [`failed`]: a 2xx whole Response with `status: "failed"` counts what it
//!   reported, or its prompt locally.
//!
//! The hosted-tool floor of WP5b is in `responses_native::hosted_fees`.

use lmgw_core::config::{PriceScope, PriceTier, PriceUnit, Protocol, UpstreamKind};
use lmgw_core::pricing::{PriceSource, Prices};
use lmgw_core::state::SharedState;
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::Gw;
use crate::e2e_proxy::setup_kind;
use crate::stream_usage::row_after;

mod compat;
mod cut_chat;
mod embed;
mod failed;
mod flex;
mod hangup;
mod stopped;
mod tier;
mod units;

/// The alias `setup_kind` creates, on the upstream model `tgt-model`.
const ALIAS: &str = "my-model";

/// 1000 in, 100 out, as the Responses wire reports it (the platform's chat
/// sends take it, openai egress design §7.2). Per 1M tokens: 2 in and 8 out
/// at Standard, 1 and 4 at Flex; micro-units are tokens × rate.
fn usage() -> Value {
    json!({"input_tokens": 1000, "output_tokens": 100, "total_tokens": 1100})
}

const STANDARD_MICRO: i64 = 2000 + 800;
const FLEX_MICRO: i64 = 1000 + 400;

/// A whole Response object saying "pong", served at `tier` (`None`: it
/// names none).
fn answer(tier: Option<&str>) -> Value {
    let mut v = json!({
        "id": "resp_up", "object": "response", "status": "completed", "model": "tgt-model",
        "output": [crate::support::responses_wire::message("msg_up", "pong")],
        "usage": usage(),
    });
    if let Some(t) = tier {
        v["service_tier"] = json!(t);
    }
    v
}

/// A whole chat completion saying "pong" with [`usage`]'s counts, as an
/// OpenAI-compatible provider answers.
fn chat_answer() -> Value {
    json!({
        "id": "chatcmpl-up", "object": "chat.completion", "model": "tgt-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "pong"},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1000, "completion_tokens": 100, "total_tokens": 1100},
    })
}

/// An `openai` upstream answering every chat send (on the Responses wire)
/// with `body`, and a gateway with [`ALIAS`] on it.
async fn platform(body: ResponseTemplate) -> (MockServer, SharedState, Gw) {
    let m = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(body)
        .mount(&m)
        .await;
    let (state, gw) = setup_kind(&m.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    (m, state, gw)
}

/// Per 1M tokens on [`ALIAS`], at `tier`.
async fn price(state: &SharedState, tier: PriceTier, p_in: f64, p_out: f64) {
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        ALIAS,
        PriceUnit::PerMtok,
        tier,
        &Prices {
            price_in: Some(p_in),
            price_out: Some(p_out),
            source: PriceSource::Manual,
            ..Default::default()
        },
        None,
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// `POST /v1/chat/completions` with `body` merged over a plain ask, as
/// `bearer` when given.
async fn chat(gw: &Gw, extra: Value, bearer: Option<&str>) -> (u16, Value) {
    let mut body = json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]});
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        body[k] = v;
    }
    let rb = gw.anon().post(format!("{gw}/v1/chat/completions"));
    let rb = match bearer {
        Some(b) => rb.bearer_auth(b),
        None => rb,
    };
    let resp = rb.json(&body).send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// The row's `budget_floor_micro`, which no row view reads.
async fn floor_of(state: &SharedState, id: i64) -> Option<i64> {
    sqlx::query_scalar("SELECT budget_floor_micro FROM request_logs WHERE id = ?1")
        .bind(id)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

/// What the reseed reads for the global budget: every bucket's cost plus
/// its floor.
async fn spend(state: &SharedState) -> i64 {
    store::total_spend_micro(&state.db, "0000-01-01T00")
        .await
        .unwrap()
}

/// A key `name` with a monthly budget of `budget_micro`, its bearer
/// `lmgw-<name>`.
async fn budgeted_key(state: &SharedState, name: &str, budget_micro: i64) {
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, enabled, budget_micro, budget_period)
         VALUES (?1, ?2, 1, ?3, 'month')",
    )
    .bind(name)
    .bind(lmgw_core::config::hash_api_key(&format!("lmgw-{name}")))
    .bind(budget_micro)
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}
