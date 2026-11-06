//! Service-tier pricing on the chat route (openai egress design §5, WP4):
//! an `openai` row's answer says which tier served it, whole and streamed;
//! the row records it and is priced with that tier's rows only; a tier with
//! no row is unpriced with a price note naming it, never priced at Standard
//! (decision 15); the OpenAI wire relays the tier as OpenAI's own does
//! (§5.7). An `openai_compatible` row has no tier dimension.

use lmgw_core::config::{PriceScope, PriceTier, PriceUnit, Protocol, UpstreamKind};
use lmgw_core::pricing::{PriceSource, Prices};
use lmgw_core::state::SharedState;
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::e2e_proxy::setup_kind;
use crate::stream_usage::row_after;

/// 1000 in, 100 out; per 1M tokens 2 in and 8 out at Standard, 1 and 4 at
/// Flex, 4 and 16 at Fast: micro-units are tokens × rate. On the Chat wire.
fn usage() -> Value {
    json!({"prompt_tokens": 1000, "completion_tokens": 100, "total_tokens": 1100})
}

/// [`usage`] on the Responses wire, which the platform's chat sends take
/// (openai egress design §7.2).
fn responses_usage() -> Value {
    json!({"input_tokens": 1000, "output_tokens": 100, "total_tokens": 1100})
}

const STANDARD_MICRO: i64 = 2000 + 800;
const FLEX_MICRO: i64 = 1000 + 400;
const FAST_MICRO: i64 = 4000 + 1600;

/// The platform's whole answer saying "pong", served at `tier`.
fn answer(tier: Option<&str>) -> Value {
    let mut v = json!({
        "id": "resp_up", "object": "response", "status": "completed", "model": "tgt-model",
        "output": [crate::support::responses_wire::message("msg_up", "pong")],
        "usage": responses_usage(),
    });
    if let Some(t) = tier {
        v["service_tier"] = json!(t);
    }
    v
}

/// A chat completion saying "pong", served at `tier`.
fn chat_answer(tier: Option<&str>) -> Value {
    let mut v = json!({
        "id": "chatcmpl-up", "object": "chat.completion", "model": "tgt-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "pong"},
                     "finish_reason": "stop"}],
        "usage": usage(),
    });
    if let Some(t) = tier {
        v["service_tier"] = json!(t);
    }
    v
}

/// The platform's Responses stream: `response.created` echoes the
/// requested tier, the terminal event names the one that served it, with
/// the usage.
fn stream(tier: &str) -> String {
    let mut done = answer(Some(tier));
    done["service_tier"] = json!(tier);
    crate::support::responses_wire::sse(&[
        json!({"type": "response.created", "response": {"service_tier": "auto"}}),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "pong"}),
        json!({"type": "response.completed", "response": done}),
    ])
}

/// OpenAI's own Chat Completions stream: the tier on every chunk, usage in
/// the last.
fn chat_stream(tier: &str) -> String {
    [
        json!({"object": "chat.completion.chunk", "service_tier": tier,
               "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]}),
        json!({"object": "chat.completion.chunk", "service_tier": tier,
               "choices": [{"index": 0, "delta": {"content": "pong"}, "finish_reason": "stop"}]}),
        json!({"object": "chat.completion.chunk", "service_tier": tier, "choices": [],
               "usage": usage()}),
    ]
    .iter()
    .map(|c| format!("data: {c}\n\n"))
    .chain(["data: [DONE]\n\n".to_string()])
    .collect()
}

/// The platform: its chat sends go to `/responses` unless they need the
/// Chat wire.
async fn mock(body: ResponseTemplate) -> MockServer {
    mock_at("/responses", body).await
}

async fn mock_at(at: &str, body: ResponseTemplate) -> MockServer {
    let m = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(at))
        .respond_with(body)
        .mount(&m)
        .await;
    m
}

async fn price(state: &SharedState, tier: PriceTier, p_in: f64, p_out: f64) {
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        "my-model",
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

async fn chat(base: &impl std::fmt::Display, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

fn ask() -> Value {
    json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]})
}

#[tokio::test]
async fn a_flex_answer_is_priced_with_the_flex_rows() {
    let m = mock(ResponseTemplate::new(200).set_body_json(answer(Some("flex")))).await;
    let (state, base) = setup_kind(&m.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    price(&state, PriceTier::Flex, 1.0, 4.0).await;

    let v: Value = chat(&base, ask()).await.json().await.unwrap();
    assert_eq!(
        v["service_tier"], "flex",
        "relayed as OpenAI's body has it: {v}"
    );
    let row = row_after(&state, 0).await;
    assert_eq!(row.service_tier.as_deref(), Some("flex"));
    assert_eq!(row.cost_micro, Some(FLEX_MICRO), "{row:?}");
    assert_eq!(row.price_note, None);
}

/// `default` is Standard (§5.4).
#[tokio::test]
async fn a_default_answer_is_priced_at_standard() {
    let m = mock(ResponseTemplate::new(200).set_body_json(answer(Some("default")))).await;
    let (state, base) = setup_kind(&m.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    price(&state, PriceTier::Flex, 1.0, 4.0).await;
    assert_eq!(chat(&base, ask()).await.status(), 200);
    let row = row_after(&state, 0).await;
    assert_eq!(row.cost_micro, Some(STANDARD_MICRO), "{row:?}");
}

/// Decision 15: a Fast answer on a scope with a Standard row only is
/// unpriced and says so; nothing falls back to Standard, which would
/// undercharge it 2×. Budgets count it at the standard price as a lower
/// bound (decision 22, `budget_floors::tier`), and the note says that too.
#[tokio::test]
async fn a_tier_with_no_row_is_unpriced_with_a_note() {
    let m = mock(ResponseTemplate::new(200).set_body_json(answer(Some("priority")))).await;
    let (state, base) = setup_kind(&m.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;

    let body = json!({"model": "my-model", "service_tier": "priority",
                      "messages": [{"role": "user", "content": "hi"}]});
    assert_eq!(chat(&base, body).await.status(), 200);
    let row = row_after(&state, 0).await;
    assert_eq!(row.service_tier.as_deref(), Some("priority"));
    assert_eq!(row.cost_micro, None, "{row:?}");
    assert_eq!(row.price_source.as_deref(), Some("unknown"));
    assert_eq!(
        row.price_note.as_deref(),
        Some(
            "served at service tier priority (requested: priority); no fast price on file \
             for 'tgt-model', a standard one is; budgets count it at the standard price, \
             0.002800 USD, a lower bound"
        )
    );
    // The Prices page's list; a Fast row fixes what comes next.
    let gaps = lmgw_core::ops::unpriced_tiers(&state).await.unwrap();
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert_eq!(gaps[0].service_tier, "fast");
    assert_eq!(gaps[0].scope_key, format!("{}:tgt-model", row_up(&state)));
    price(&state, PriceTier::Fast, 4.0, 16.0).await;
    assert_eq!(chat(&base, ask()).await.status(), 200);
    let row = row_after(&state, 1).await;
    assert_eq!(row.cost_micro, Some(FAST_MICRO), "{row:?}");
}

/// The test upstream's id.
fn row_up(state: &SharedState) -> i64 {
    state.snapshot().resolve("my-model").unwrap().upstream.id
}

/// An `openai` answer that names no tier is an unknown tier (§5.2): unpriced
/// with a note, and nothing relayed.
#[tokio::test]
async fn an_answer_without_a_tier_is_not_standard() {
    let m = mock(ResponseTemplate::new(200).set_body_json(answer(None))).await;
    let (state, base) = setup_kind(&m.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    let v: Value = chat(&base, ask()).await.json().await.unwrap();
    assert!(v.get("service_tier").is_none(), "{v}");
    let row = row_after(&state, 0).await;
    assert_eq!(row.cost_micro, None);
    assert!(row
        .price_note
        .unwrap()
        .starts_with("the upstream reported no service tier"));
}

/// A streamed chat's client chunks, `[DONE]` aside.
fn chunks_of(text: &str) -> Vec<Value> {
    text.split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

/// Streamed on the Responses wire: the tier is named by the terminal event
/// only, so the chunks from then on carry it (the finish and the usage
/// chunk), the text before it none; the row is priced at it.
#[tokio::test]
async fn a_streamed_answer_relays_and_prices_its_tier() {
    let m = mock(
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(stream("flex")),
    )
    .await;
    let (state, base) = setup_kind(&m.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    price(&state, PriceTier::Flex, 1.0, 4.0).await;
    let body = json!({"model": "my-model", "stream": true,
                      "stream_options": {"include_usage": true},
                      "messages": [{"role": "user", "content": "hi"}]});
    let text = chat(&base, body).await.text().await.unwrap();
    let chunks = chunks_of(&text);
    // The role chunk and the text leave before the upstream named a tier.
    assert!(
        chunks[..2].iter().all(|c| c.get("service_tier").is_none()),
        "{text}"
    );
    assert!(
        chunks[2..].iter().all(|c| c["service_tier"] == "flex"),
        "{text}"
    );
    assert!(chunks.iter().any(|c| c["usage"]["prompt_tokens"] == 1000));
    let row = row_after(&state, 0).await;
    assert_eq!(row.service_tier.as_deref(), Some("flex"));
    assert_eq!(row.cost_micro, Some(FLEX_MICRO), "{row:?}");
}

/// Streamed on the Chat wire (a send with `seed`, which only Chat
/// Completions has): every chunk after the upstream named the tier carries
/// it, the usage chunk included.
#[tokio::test]
async fn a_chat_wire_stream_relays_its_tier_on_every_chunk() {
    let m = mock_at(
        "/chat/completions",
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(chat_stream("flex")),
    )
    .await;
    let (state, base) = setup_kind(&m.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    price(&state, PriceTier::Flex, 1.0, 4.0).await;
    let body = json!({"model": "my-model", "stream": true, "seed": 7,
                      "stream_options": {"include_usage": true},
                      "messages": [{"role": "user", "content": "hi"}]});
    let text = chat(&base, body).await.text().await.unwrap();
    let chunks = chunks_of(&text);
    assert!(chunks[0].get("service_tier").is_none(), "{text}");
    assert!(
        chunks[1..].iter().all(|c| c["service_tier"] == "flex"),
        "{text}"
    );
    let row = row_after(&state, 0).await;
    assert_eq!(row.cost_micro, Some(FLEX_MICRO), "{row:?}");
}

/// A compatible row has no tier dimension: a provider's `service_tier` is
/// neither read, nor relayed, nor priced by (§5.4).
#[tokio::test]
async fn a_compatible_row_has_no_tier() {
    let m = mock_at(
        "/chat/completions",
        ResponseTemplate::new(200).set_body_json(chat_answer(Some("flex"))),
    )
    .await;
    let (state, base) =
        setup_kind(&m.uri(), Protocol::OpenaiCompatible, UpstreamKind::Generic).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    price(&state, PriceTier::Flex, 1.0, 4.0).await;
    let v: Value = chat(&base, ask()).await.json().await.unwrap();
    assert!(v.get("service_tier").is_none(), "{v}");
    let row = row_after(&state, 0).await;
    assert_eq!(row.service_tier, None);
    assert_eq!(row.cost_micro, Some(STANDARD_MICRO), "{row:?}");
}

/// `/v1/messages` on the platform: priced at the tier, but Anthropic's
/// `service_tier` has its own vocabulary, so nothing is mapped onto it.
#[tokio::test]
async fn the_anthropic_wire_relays_no_openai_tier() {
    let m = mock(ResponseTemplate::new(200).set_body_json(answer(Some("flex")))).await;
    let (state, base) = setup_kind(&m.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    price(&state, PriceTier::Flex, 1.0, 4.0).await;
    let v: Value = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&json!({"model": "my-model", "max_tokens": 16,
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(v.get("service_tier").is_none(), "{v}");
    assert!(v["usage"].get("service_tier").is_none(), "{v}");
    let row = row_after(&state, 0).await;
    assert_eq!(row.cost_micro, Some(FLEX_MICRO), "{row:?}");
}

/// An unpriced tier-note row, as the openai egress writes one.
async fn noted(
    state: &SharedState,
    tier: Option<&str>,
    note: &str,
    priced_alias: Option<&str>,
) -> i64 {
    let route = state.snapshot().resolve("my-model").unwrap();
    store::insert_request_log(
        &state.db,
        &store::NewRequestLog {
            ingress_proto: "openai".into(),
            requested_alias: "my-model".into(),
            upstream_id: Some(route.upstream.id),
            upstream_model: Some(route.upstream_model.clone()),
            egress_proto: Some("openai".into()),
            status: 200,
            service_tier: tier.map(String::from),
            price_note: Some(note.into()),
            priced_alias: priced_alias.map(String::from),
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

/// Rows answered `auto` or at no tier are unpriced too and name no row to
/// enter: they stay findable, listed without a tier or an action.
#[tokio::test]
async fn an_auto_or_absent_tier_is_listed_without_a_price_action() {
    let m = mock(ResponseTemplate::new(200).set_body_json(answer(Some("auto")))).await;
    let (state, base) = setup_kind(&m.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    assert_eq!(chat(&base, ask()).await.status(), 200);
    row_after(&state, 0).await;
    noted(
        &state,
        None,
        &lmgw_core::pricing::gap_note(
            &lmgw_core::pricing::ServedTier::of(Protocol::Openai, None, None),
            &Default::default(),
            "tgt-model",
        ),
        None,
    )
    .await;
    // An incomplete stream's note is no tier note.
    noted(
        &state,
        None,
        "stream ended before the upstream reported usage",
        None,
    )
    .await;

    let gaps = lmgw_core::ops::unpriced_tiers(&state).await.unwrap();
    assert_eq!(gaps.len(), 2, "{gaps:?}");
    let auto = gaps.iter().find(|g| g.reported == "auto").expect("auto");
    assert_eq!(
        (auto.service_tier.as_str(), auto.requests),
        ("", 1),
        "{auto:?}"
    );
    let none = gaps.iter().find(|g| g.reported.is_empty()).expect("none");
    assert_eq!(
        (none.service_tier.as_str(), none.requests),
        ("", 1),
        "{none:?}"
    );
    assert!(none.missing_units.is_empty());
}

/// The entry names the unit that is missing, and a group priced under a
/// fallback alias is re-checked against that alias's rows.
#[tokio::test]
async fn an_entry_names_its_missing_unit_and_the_alias_it_was_priced_under() {
    let m = mock(ResponseTemplate::new(200).set_body_json(answer(Some("flex")))).await;
    let (state, _base) = setup_kind(&m.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    let per_request = |alias: &'static str, tier| {
        let state = state.clone();
        async move {
            store::upsert_price(
                &state.db,
                PriceScope::Alias,
                alias,
                PriceUnit::PerRequest,
                tier,
                &Prices {
                    source: PriceSource::Manual,
                    ..Default::default()
                },
                Some(0.01),
                None,
            )
            .await
            .unwrap();
            state.reload_snapshot().await.unwrap();
        }
    };
    per_request("my-model", PriceTier::Standard).await;
    price(&state, PriceTier::Flex, 1.0, 4.0).await;
    let note = "served at service tier flex; no flex price on file";
    noted(&state, Some("flex"), note, None).await;
    let gaps = lmgw_core::ops::unpriced_tiers(&state).await.unwrap();
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert_eq!(gaps[0].missing_units, ["per_request"], "{gaps:?}");

    // The same traffic, answered by a fallback alias whose own flex rows
    // price it in full: nothing left to enter.
    let (state, _base) = setup_kind(&m.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        "fb",
        PriceUnit::PerMtok,
        PriceTier::Flex,
        &Prices {
            price_in: Some(1.0),
            price_out: Some(4.0),
            source: PriceSource::Manual,
            ..Default::default()
        },
        None,
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    noted(&state, Some("flex"), note, Some("fb")).await;
    assert!(lmgw_core::ops::unpriced_tiers(&state)
        .await
        .unwrap()
        .is_empty());
    noted(&state, Some("flex"), note, None).await;
    assert_eq!(
        lmgw_core::ops::unpriced_tiers(&state).await.unwrap().len(),
        1
    );
}

/// The list reads the index of noted rows, not the whole log.
#[tokio::test]
async fn the_unpriced_tiers_query_uses_the_price_note_index() {
    let m = mock(ResponseTemplate::new(200).set_body_json(answer(None))).await;
    let (state, _base) = setup_kind(&m.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    let plan: Vec<String> = sqlx::query(sqlx::AssertSqlSafe(format!(
        "EXPLAIN QUERY PLAN {}",
        store::UNPRICED_TIER_TRAFFIC_SQL
    )))
    .bind("x")
    .fetch_all(&state.db)
    .await
    .unwrap()
    .iter()
    .map(|r| sqlx::Row::get::<String, _>(r, "detail"))
    .collect();
    assert!(
        plan.iter()
            .any(|d| d.contains("idx_request_logs_price_note")),
        "{plan:?}"
    );
}
