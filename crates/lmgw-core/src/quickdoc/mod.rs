//! quickdoc's gateway half (quickdoc design §3).
//!
//! `quickdoc-core` is deliberately free of any lmgw dependency: it owns the
//! corpus store, retrieval, and the code-fenced logic of ingestion, and it is
//! iterated in `cargo test` with fixture models. This module is the other side
//! of that boundary — everything quickdoc needs that only the gateway has:
//!
//! - [`embed`] — the in-process [`Embedder`](quickdoc_core::embed::Embedder)
//!   over lmgw's own embedding path, enforcing the corpus's pinned identity and
//!   the reranker gate.
//! - [`rerank`] — the in-process [`Reranker`](quickdoc_core::embed::Reranker)
//!   over `/v1/rerank`, with the mirror-image model-kind gate.
//! - [`query`] — assembling one retrieval: pinned embedder + rerank stage +
//!   token counter + the owner's stage defaults, shared by the MCP tools, the
//!   debug endpoint and the eval job.
//! - [`fetch`] — the fence/robots/rate-limited fetcher. The model never crawls.
//! - [`ingest`] — the `ingest` job: the tool loop that drives extraction, plus
//!   chunk writes and embedding.
//! - [`reembed`] — the `re_embed` job: the same embedding stage on its own.
//! - [`eval`] — the `eval_run` job: golden queries → hit@k / MRR → the badge.
//! - [`golden`] — the `golden_gen` job: sampled chunks → *candidate* golden
//!   queries, which only the owner's curation turns into real ones (§10, §11).
//! - [`portability`] — corpus export and import (§10).
//!
//! The `docs__*` tool surface itself lives in [`crate::mcp::docs`] and the web
//! routes in `web::api_docs`; both call into this module.

pub mod embed;
pub mod eval;
pub mod fetch;
pub mod golden;
pub mod ingest;
pub mod portability;
pub mod query;
pub mod reembed;
pub mod rerank;

pub use embed::{alias_for_identity, InProcessEmbedder};
pub use rerank::InProcessReranker;

/// The corpus DB file, next to the main one in the data dir (§3). A separate
/// file with its own migrations: nuke-and-reingest never touches gateway
/// config, and `rsync` of this one path is a full corpus backup.
pub const CORPUS_DB_FILE: &str = "quickdoc.db";
