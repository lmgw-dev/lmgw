//! Encrypted reasoning carried between turns on the OpenAI platform (openai
//! egress design §7.3, decision 17, WP8): the seal of each reasoning item an
//! `openai` row answered with goes back to that row on the next turn, kept by
//! the Chat's store, carried by `/v1/messages` in the thinking block's
//! `signature` and by the `/v1/responses` loop in `encrypted_content`, and a
//! client key may send back only the seals it was given.

mod chat;
mod cut;
mod messages;
mod responses;
mod surfaces;

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::ir::{Seal, SealScope};
use lmgw_core::state::SharedState;
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::common::Gw;
use crate::e2e_proxy::setup_kind;
use crate::support::responses_wire as rw;

/// The `encrypted_content` of every reasoning item the mock answers with
/// (`support::responses_wire::reasoning`).
pub(crate) const SEALED: &str = "gAAAAB-sealed";

/// The platform row the tests' seals come from: its id and its scope as
/// configured (`SealScope`, WP8 review W-R2). Shown (`{up}`) as a
/// carrier's `<upstream_id>:<config>`.
#[derive(Debug, Clone)]
pub(crate) struct Up {
    pub id: i64,
    pub scope: SealScope,
}

impl Up {
    /// The row `id` as `state` has it configured now.
    pub(crate) fn of(state: &SharedState, id: i64) -> Self {
        Self {
            id,
            scope: SealScope::of(&state.snapshot().upstreams[&id]),
        }
    }

    /// An OpenAI seal of this row.
    pub(crate) fn seal(&self, item_id: &str, data: &str) -> Seal {
        Seal::openai(&self.scope, item_id, data)
    }
}

impl std::fmt::Display for Up {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.id, self.scope.config)
    }
}

/// A platform row in front of a mock whose `/responses` answers with
/// `respond`; the alias `my-model`, and the row.
pub(crate) async fn platform<R>(respond: R) -> (MockServer, SharedState, Gw, Up)
where
    R: Fn(&Request) -> ResponseTemplate + Send + Sync + 'static,
{
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(respond)
        .mount(&mock)
        .await;
    let (state, gw) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM upstreams WHERE name = 'test-up'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    let up = Up::of(&state, id);
    (mock, state, gw, up)
}

/// The body of each POST the mock got.
pub(crate) async fn posted(mock: &MockServer) -> Vec<Value> {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| serde_json::from_slice(&r.body).unwrap_or_default())
        .collect()
}

/// The reasoning items of a Responses body's `input`.
pub(crate) fn reasoning_items(body: &Value) -> Vec<Value> {
    body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["type"] == "reasoning")
        .cloned()
        .collect()
}

/// The answer as a stream or whole, as the send asks.
pub(crate) fn answer(req: &Request, items: &[Value]) -> ResponseTemplate {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
    match body["stream"] == true {
        true => ResponseTemplate::new(200)
            .set_body_raw(rw::stream("resp_1", items), "text/event-stream"),
        false => ResponseTemplate::new(200).set_body_json(rw::response(
            "resp_1",
            "completed",
            Value::Array(items.to_vec()),
        )),
    }
}

/// A tool turn's stored record (`chat_messages.ir_messages`) whose reasoning
/// came sealed by row `up`: the reasoning `Use the tool.`, a call and its
/// result.
pub(crate) fn sealed_record(up: &Up) -> String {
    use lmgw_core::ir::{ContentPart, Message, Role, ToolResultBlock};
    let record = vec![
        Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::sealed_reasoning("Use the tool.", up.seal("rs_1", SEALED)),
                ContentPart::ToolUse {
                    id: "c1".into(),
                    name: "stub__echo".into(),
                    args: serde_json::json!({"text": "hi"}),
                },
            ],
        },
        Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                id: "c1".into(),
                name: None,
                content: ToolResultBlock::one("hi"),
                is_error: false,
            }],
        },
    ];
    serde_json::to_string(&record).unwrap()
}

/// A client key `name`, its bearer `lmgw-<name>`; its id.
pub(crate) async fn client_key(state: &SharedState, name: &str) -> i64 {
    sqlx::query("INSERT INTO api_keys (name, key_hash, enabled) VALUES (?1, ?2, 1)")
        .bind(name)
        .bind(lmgw_core::config::hash_api_key(&format!("lmgw-{name}")))
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    sqlx::query_scalar("SELECT id FROM api_keys WHERE name = ?1")
        .bind(name)
        .fetch_one(&state.db)
        .await
        .unwrap()
}
