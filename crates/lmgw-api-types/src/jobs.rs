//! Background jobs (§9c)

use serde::{Deserialize, Serialize};

/// `GET /api/jobs`. The `jobs` SSE frame on `/api/events` carries the
/// `Vec<JobRow>` of *running* jobs directly (same row shape).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct JobsView {
    pub jobs: Vec<JobRow>,
    /// Number of jobs currently running, of any kind.
    pub active: i64,
}

/// One background job. `done`/`total` are in the kind's own unit (bytes for
/// `hf_download`, documents for an ingest, …) and `stage` names what is
/// happening; `total` is absent when the total genuinely isn't known yet.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct JobRow {
    pub id: i64,
    /// `hf_download | ingest | re_embed | eval_run | golden_gen`.
    pub kind: String,
    /// Dedup key scoped to the kind (`hf:<download row id>`).
    pub key: Option<String>,
    pub label: String,
    /// `queued | running | done | failed | canceled`.
    pub status: String,
    pub done: u64,
    pub total: Option<u64>,
    pub percent: Option<u64>,
    pub stage: String,
    /// Kind-specific payload; the generic feed does not interpret it.
    pub detail: serde_json::Value,
    pub error: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}
