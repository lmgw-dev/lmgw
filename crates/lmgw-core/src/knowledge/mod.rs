//! Knowledge bases (chat-complete design §9): the owner's own documents in
//! named collections, retrieved into Chat threads and served as the `kb__*`
//! built-in toolset on `/mcp` ([`crate::mcp::kb`]).
//!
//! - [`store`] — `knowledge.db`: its own file, 0600, its own migrations.
//! - [`originals`] — the uploaded files under `<data_dir>/knowledge/`, by
//!   sha256.
//! - [`chunk`] — the deterministic, markdown-aware chunker.
//! - [`sections`] — a file's bytes → its text, per page / slide / sheet, with
//!   text-less PDF pages read by the base's vision model.
//! - [`limit`] — what one embedding input may hold, and the tokenizer that
//!   counts it.
//! - [`ingest`] — the `kb_ingest` job ([`ingest_split`]: a chunk the embedder
//!   refuses as too large, split in halves); [`reembed`] — `kb_reembed`.
//! - [`retrieve`] — hybrid search over a set of bases, through quickdoc-core's
//!   one pipeline ([`quickdoc_core::retrieve::hybrid_search`]);
//!   [`retrieve_guard`] keeps its query and rerank pairs inside the models'
//!   per-input limits.
//! - [`read`] — reading on in a document (`kb__read`, the source viewer).
//! - [`ops`] — create / edit / delete a base, upload, re-ingest, resume,
//!   cancel, and the views the dashboard and the tools show.
//!
//! **What the Chat calls** (WP10): [`retrieve::retrieve`] for auto mode,
//! [`crate::mcp::exec::KbExecutor`] restricted to a thread's bases for tool
//! mode, and [`read::source`] for the source viewer.

pub mod chunk;
pub mod fit;
pub mod ingest;
pub mod ingest_split;
pub mod limit;
pub mod ops;
pub mod originals;
pub mod read;
pub mod reembed;
pub mod retrieve;
pub mod retrieve_guard;
pub mod sections;
pub mod store;
pub mod wire;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use quickdoc_core::vector::VectorMatrix;
use sqlx::SqlitePool;

/// The knowledge DB file, next to the main one in the data dir (§9.1).
pub const KNOWLEDGE_DB_FILE: &str = "knowledge.db";

/// The job key both knowledge jobs use: one live job of each kind per base,
/// and [`ops`] keeps the two kinds from running on one base at once.
pub fn job_key(kb_id: i64) -> String {
    format!("kb:{kb_id}")
}

/// The gateway's handle on `knowledge.db`: the pool, and each base's vectors
/// resident once loaded.
pub struct Knowledge {
    pub pool: SqlitePool,
    /// `kb id → (vectors_rev, matrix)`. `kb.vectors_rev` moves on every chunk
    /// write (a trigger), so a matrix is reloaded exactly when it went stale
    /// and never served stale. What it costs is visible: the base view
    /// reports resident bytes.
    matrices: Mutex<HashMap<i64, (i64, Arc<VectorMatrix>)>>,
    /// `model identity → tokenizer ratio to tiktoken` ([`limit::cached_counter`]).
    ratios: Mutex<HashMap<String, f64>>,
}

impl Knowledge {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            matrices: Mutex::new(HashMap::new()),
            ratios: Mutex::new(HashMap::new()),
        }
    }

    /// The remembered tokenizer ratio of a model identity.
    pub fn ratio(&self, key: &str) -> Option<f64> {
        self.ratios
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .copied()
    }

    pub fn set_ratio(&self, key: &str, ratio: f64) {
        self.ratios
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string(), ratio);
    }

    /// A base's vectors, loaded once per revision.
    pub async fn matrix(&self, kb: &store::Kb) -> Result<Arc<VectorMatrix>, String> {
        if let Some((rev, m)) = self.cached(kb.id) {
            if rev == kb.vectors_rev && m.dims() == kb.dims() {
                return Ok(m);
            }
        }
        let m = Arc::new(store::load_matrix(&self.pool, kb).await?);
        self.matrices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(kb.id, (kb.vectors_rev, m.clone()));
        Ok(m)
    }

    fn cached(&self, id: i64) -> Option<(i64, Arc<VectorMatrix>)> {
        self.matrices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
    }

    /// Drop a deleted base's matrix.
    pub fn forget(&self, kb_id: i64) {
        self.matrices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&kb_id);
    }
}
