//! `lmgw__docs_corpora`, `lmgw__docs_corpus_set`, `lmgw__docs_ingest`,
//! `lmgw__docs_requests` and `lmgw__docs_request_set`.
//!
//! The tools drive the dashboard's own handlers (`/api/docs/*`), so most of
//! what they answer is the dashboard's types: a corpus is a [`CorpusView`], a
//! request queue a [`DocRequestsResponse`]. What the tools add is on top of
//! those: the job behind a corpus, a `hint` or a `next_step`.
//!
//! `lmgw__docs_corpora`, `lmgw__docs_corpus_set` and `lmgw__docs_ingest` each
//! answer two different things by their arguments, so each is an
//! `#[serde(untagged)]` wrapper of the two (`Serialize` and `JsonSchema` only:
//! a reader knows the call it made and reads that call's own type). The
//! refusal of any call is an error result instead of one of these.

use serde::{Deserialize, Serialize};

use crate::{CorpusCreated, CorpusView, DocRequestsResponse, MessageAck, WithNextStep};

/// `lmgw__docs_corpora`: one corpus when `corpus` is named, else every corpus.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DocsCorpora {
    /// `corpus` named: that corpus.
    One(Box<DocsCorpusOne>),
    /// No `corpus`: the list.
    List(DocsCorpusList),
}

/// `lmgw__docs_corpora corpus=<id or library@version>`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocsCorpusOne {
    pub corpus: CorpusWithJob,
}

/// `lmgw__docs_corpora` with no `corpus`: `GET /api/docs/corpora`'s answer
/// with the job behind each corpus, and a `hint` on how to read it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocsCorpusList {
    pub corpora: Vec<CorpusWithJob>,
    /// Pending `docs__request` rows.
    pub pending_requests: u64,
    /// Alias the rerank stage would call; `null` when there is none.
    pub rerank_model: Option<String>,
    pub schema_version: i64,
    /// How to read `status`, `job`, `flags` and `warnings`.
    pub hint: String,
}

/// One corpus as the dashboard shows it, and the most recent ingest or
/// re-embed job for it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CorpusWithJob {
    #[serde(flatten)]
    pub corpus: CorpusView,
    /// The latest ingest or re-embed job, running or finished (a start's
    /// follow-up asks how it went, so a finished one is kept). Absent when the
    /// corpus has never had one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub job: Option<CorpusJob>,
}

/// A corpus's job: the columns of a job row that say how far it got.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CorpusJob {
    pub id: i64,
    /// `ingest | re_embed`.
    pub kind: String,
    /// `queued | running | done | failed | canceled`.
    pub status: String,
    /// What the job is doing right now.
    pub stage: String,
    /// Documents (an ingest) or chunks (a re-embed) done.
    pub done: u64,
    /// `null` while the total is not known.
    pub total: Option<u64>,
    /// `null` while the total is not known.
    pub percent: Option<u64>,
    /// Why it failed; `null` otherwise.
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// `lmgw__docs_corpus_set`: `create` answers the corpus and its job with a
/// `next_step`, `delete` a [`MessageAck`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DocsCorpusSetAnswer {
    /// `action=create`.
    Created(Box<WithNextStep<CorpusCreated>>),
    /// `action=delete`.
    Deleted(MessageAck),
}

/// `lmgw__docs_ingest`: `start` and `re_embed` answer the job they queued (or
/// the one already running), `cancel` a [`MessageAck`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DocsIngestAnswer {
    /// `action=start` or `re_embed`.
    Started(DocsIngestStarted),
    /// `action=cancel`.
    Canceled(MessageAck),
}

/// The job `lmgw__docs_ingest` `start` or `re_embed` queued.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocsIngestStarted {
    /// Always `true`; a refusal is an error result instead.
    pub ok: bool,
    pub job_id: i64,
    /// `true` when a job for the corpus was already in flight: `job_id` is
    /// that one's.
    pub already_running: bool,
    /// What was queued, for a person to read.
    pub message: String,
    /// What to call to follow the job.
    pub next_step: String,
}

/// `lmgw__docs_requests`: `GET /api/docs/requests`' answer and a `hint`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocsRequestQueue {
    #[serde(flatten)]
    pub response: DocRequestsResponse,
    /// How to fulfil or dismiss a request.
    pub hint: String,
}
