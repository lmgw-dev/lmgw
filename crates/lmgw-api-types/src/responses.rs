//! Stored /v1/responses conversations

use serde::{Deserialize, Serialize};

/// `GET /api/responses`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResponsesIndex {
    pub chains: Vec<ChainRow>,
    pub total_chains: i64,
    pub total_responses: i64,
    pub store_enabled: bool,
    pub retention_hours: i64,
    pub max_chains: i64,
    /// The eviction rules in one sentence.
    pub rules: String,
    /// True when more chains exist than the 200 shown.
    pub capped: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChainRow {
    pub chain_id: String,
    /// What a client passes as `previous_response_id` to continue.
    pub head_id: String,
    pub model: String,
    pub status: String,
    pub responses: i64,
    pub first_at: String,
    pub last_at: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub size: String,
    pub awaiting_approval: bool,
}

/// `GET /api/responses/chain?id=`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChainDetail {
    pub chain_id: String,
    pub responses: Vec<ResponseItem>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResponseItem {
    pub id: String,
    pub created_at: String,
    pub status: String,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    /// Output items at a glance, e.g. `2 mcp_call, message`.
    pub outline: String,
    /// Rendered assistant text.
    pub text: String,
    /// Tool calls awaiting approval, `{name} ({approval_id})`.
    pub pending: Vec<String>,
    /// Full response JSON, pretty-printed.
    pub body: String,
}
