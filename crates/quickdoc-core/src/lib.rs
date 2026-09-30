//! quickdoc-core — versioned documentation corpora with hybrid retrieval.
//!
//! The testable heart of quickdoc (see
//! `docs/design/2026-08-29-quickdoc-design.md`; `§n` below refer to
//! it). Deliberately free of any lmgw dependency: retrieval is iterated in
//! `cargo test` against a fixture embedder, with no gateway, no container and
//! no model in the loop.
//!
//! - [`store`] — the corpus SQLite file: its own pool, its own migrations.
//! - [`vector`] — f16 embedding storage and the exact-KNN scan (§5).
//! - [`embed`] — the [`Embedder`](embed::Embedder) /
//!   [`Reranker`](embed::Reranker) boundary lmgw-core implements, plus
//!   deterministic fixtures.
//! - [`retrieve`] — BM25 + KNN + RRF (+ optional rerank) with a per-stage trace
//!   (§6).
//! - [`markdown`] — `docs__query`'s answer shape (§7).
//! - [`eval`] — golden queries, hit@k and MRR.
//! - [`golden`] — the synthetic golden-query contract (§10): what a generation
//!   run may propose, and what code checks before it becomes a candidate.
//! - [`ingest`] — the code-fenced half of ingestion (§8): source sniffing, the
//!   domain fence, the versioned prompt, and the verbatim-payload contract.
//!   lmgw-core's job executor supplies the fetcher and the tool loop.

pub mod embed;
pub mod error;
pub mod eval;
pub mod golden;
#[cfg(feature = "http")]
pub mod http;
pub mod ingest;
pub mod markdown;
pub mod retrieve;
pub mod store;
pub mod vector;

pub use error::{QuickdocError, Result};
