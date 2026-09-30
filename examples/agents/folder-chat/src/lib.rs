//! folder-chat — a first-party example agent for lmgw: chat with a folder.
//!
//! The owner binds one folder on the agent's Run tab (a `format: "directory"`,
//! `access: "rw"` config field, mounted at `/lmgw/mounts/folder`). The agent
//! keeps a vector index of every markdown, plain-text, source-code and PDF file
//! in it **inside one hidden directory in that folder**
//! ([`config::INDEX_DIR_NAME`]), keeps it in sync (new and changed files are
//! embedded, files that disappeared are removed), and answers questions with
//! retrieval through lmgw.
//!
//! The library carries the web half too, so the tests can stand it up without
//! a container: [`server`] (the UI, the JSON API, the SSE streams, and the two
//! request guards), [`hub`] (one sync at a time, its events replayed to a late
//! subscriber) and [`mcp`] (the `search` and `read` tools on `provides.mcp`).
//! `src/main.rs` is only start-up and shutdown. Underneath, four things:
//!
//! - [`app::FolderChat`] — the whole agent: [`FolderChat::from_env`] in the
//!   container, [`FolderChat::sync`] streaming [`sync::SyncEvent`]s,
//!   [`FolderChat::ask`] returning an [`chat::Answer`] (metadata with the
//!   [`budget::Budget`] and [`chat::Citation`]s, plus a stream of
//!   [`gateway::ChatChunk`]s).
//! - [`gateway::Gateway`] — the lmgw client: model info, the embedder, the
//!   reranker, streaming chat.
//! - [`index::IndexDir`] — **the only code in this crate that opens anything
//!   for writing**. Every source file is opened read-only (see [`scan`] and
//!   [`pdf`]); the index directory is the one path the agent ever writes.
//! - [`sync::run`] — the sync itself over any `Embedder`, which is what the
//!   offline tests drive with quickdoc's `FixtureEmbedder`.
//!
//! The corpus store, the f16 KNN matrix and hybrid BM25 + KNN retrieval are
//! quickdoc-core's, reused unchanged: one folder is one corpus
//! (`folder@1`), one source, one document per file.
//!
//! **No hidden limits.** Every constant that shapes what the owner sees is
//! named, documented where it is defined, and carried in the report or the
//! answer metadata that shows its effect: the chunk size and the estimator
//! that measures it, the embedding batch size, the answer reserve and the
//! context fallback, the excerpt count, the retrieval depths. The one skip
//! for size is derived, never a fixed byte count: a file larger than
//! `sync::LARGE_FILE_MEMORY_FRACTION` of the container's real memory limit is
//! skipped as `too_large_for_memory`, its reason naming the limit and how to
//! raise it — with no limit, nothing is skipped for size. A PDF that
//! `pdftotext` cannot extract within `pdf::PDF_EXTRACT_TIMEOUT` is skipped as
//! `pdf_timeout`, naming the timeout. With a vision model set ([`vision`]),
//! the rule that picks the PDF pages it reads and the resolution they are
//! rendered at are named constants carried in every sync report
//! (`vision_rule`, `vision_dpi`), and a reading is never capped: one the
//! model's own context cuts off is a failed page, not a stored fragment.

pub mod app;
pub mod budget;
pub mod chat;
pub mod chunk;
pub mod config;
pub mod gateway;
pub mod hub;
pub mod index;
pub mod mcp;
pub mod pdf;
pub mod scan;
pub mod server;
pub mod source;
pub mod sync;
pub mod vision;

pub use app::FolderChat;
