//! Corpus persistence (§4, §5): its own SQLite file, its own pool, its own
//! migrations directory. Nothing here is shared with the gateway's main DB, so
//! nuking and re-ingesting a corpus can never touch gateway config.
//!
//! Unlike `lmgw-core::store` this file is **not** clamped to 0600: a corpus
//! holds no secrets (§3), and the file being ordinarily readable is what makes
//! export/import a plain copy.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};

use crate::embed::EmbedIdentity;
use crate::error::{QuickdocError, Result};
use crate::vector::{self, VectorMatrix};

/// Open (creating if needed) a corpus DB and run its migrations.
pub async fn open(path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .map_err(QuickdocError::Db)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

/// Open a corpus DB with the caller's own connect and pool options, and run
/// its migrations. [`open`] picks both itself (and creates the parent
/// directory and the file); a caller that has to control how the file is
/// opened — no `create_if_missing`, a fixed set of connections that are never
/// recycled and so never re-resolve the path — passes its own. Nothing is
/// created here beyond what `connect` allows.
pub async fn open_with(
    connect: SqliteConnectOptions,
    pool: SqlitePoolOptions,
) -> Result<SqlitePool> {
    let pool = pool.connect_with(connect).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

/// In-memory corpus DB for tests.
pub async fn open_in_memory() -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .map_err(QuickdocError::Db)?
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

/// Flush the WAL into the main file — what export does before streaming it
/// (§10), so the copy is complete on its own.
pub async fn checkpoint(pool: &SqlitePool) -> Result<()> {
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(pool)
        .await?;
    Ok(())
}

fn to_json<T: Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| QuickdocError::Invalid(e.to_string()))
}

fn parse_json_or<T: serde::de::DeserializeOwned + Default>(s: &str) -> T {
    serde_json::from_str(s).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Corpus {
    pub id: i64,
    pub library: String,
    pub version: String,
    pub status: String,
    pub embed_upstream: String,
    pub embed_model: String,
    pub embed_dims: i64,
    pub ingest_model: String,
    pub ingest_prompt_version: String,
    pub crawl_date: String,
    pub source_kind: String,
    /// Latest measured hit@k. `None` = never evaluated, which is a different
    /// statement from "scored zero" and is reported as such.
    pub eval_score: Option<f64>,
    /// Best hit@k ever measured — what [`eval_regression`](Self::eval_regression)
    /// is measured against.
    pub eval_best: Option<f64>,
    /// The §7 badge: the latest run scored below this corpus's best. Surfaced,
    /// never enforced — `docs__query` still answers.
    pub eval_regression: bool,
    /// Depth the score was measured at.
    pub eval_k: i64,
    pub eval_at: String,
    pub chunk_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

impl Corpus {
    /// The client-facing id (`axum@0.8`), as `docs__resolve` reports it.
    pub fn corpus_id(&self) -> String {
        format!("{}@{}", self.library, self.version)
    }

    /// The embedding identity this corpus is pinned to.
    pub fn embed_identity(&self) -> EmbedIdentity {
        EmbedIdentity::new(
            self.embed_upstream.clone(),
            self.embed_model.clone(),
            self.embed_dims.max(0) as usize,
        )
    }

    pub fn dims(&self) -> usize {
        self.embed_dims.max(0) as usize
    }
}

/// Split `library@version`. On the *last* `@`, so an npm-style scoped name
/// (`@scope/pkg@1.2`) resolves the way it reads.
pub fn parse_corpus_id(id: &str) -> Result<(&str, &str)> {
    match id.rsplit_once('@') {
        Some((lib, ver)) if !lib.is_empty() && !ver.is_empty() => Ok((lib, ver)),
        _ => Err(QuickdocError::BadCorpusId(id.to_string())),
    }
}

#[derive(Debug, Clone)]
pub struct NewCorpus {
    pub library: String,
    pub version: String,
    pub status: String,
    pub embed: EmbedIdentity,
    pub ingest_model: String,
    pub ingest_prompt_version: String,
    pub crawl_date: String,
    pub source_kind: String,
}

impl NewCorpus {
    pub fn new(
        library: impl Into<String>,
        version: impl Into<String>,
        embed: EmbedIdentity,
    ) -> Self {
        Self {
            library: library.into(),
            version: version.into(),
            status: "ingesting".into(),
            embed,
            ingest_model: String::new(),
            ingest_prompt_version: String::new(),
            crawl_date: String::new(),
            source_kind: String::new(),
        }
    }
}

fn corpus_from_row(row: &sqlx::sqlite::SqliteRow) -> Corpus {
    Corpus {
        id: row.get("id"),
        library: row.get("library"),
        version: row.get("version"),
        status: row.get("status"),
        embed_upstream: row.get("embed_upstream"),
        embed_model: row.get("embed_model"),
        embed_dims: row.get("embed_dims"),
        ingest_model: row.get("ingest_model"),
        ingest_prompt_version: row.get("ingest_prompt_version"),
        crawl_date: row.get("crawl_date"),
        source_kind: row.get("source_kind"),
        eval_score: row.get("eval_score"),
        eval_best: row.get("eval_best"),
        eval_regression: row.get::<i64, _>("eval_regression") != 0,
        eval_k: row.get("eval_k"),
        eval_at: row.get("eval_at"),
        chunk_count: row.get("chunk_count"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

pub async fn insert_corpus(pool: &SqlitePool, c: &NewCorpus) -> Result<i64> {
    let res = sqlx::query(
        "INSERT INTO corpus (library, version, status, embed_upstream, embed_model, embed_dims,
            ingest_model, ingest_prompt_version, crawl_date, source_kind)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
    )
    .bind(&c.library)
    .bind(&c.version)
    .bind(&c.status)
    .bind(&c.embed.upstream)
    .bind(&c.embed.model)
    .bind(c.embed.dims as i64)
    .bind(&c.ingest_model)
    .bind(&c.ingest_prompt_version)
    .bind(&c.crawl_date)
    .bind(&c.source_kind)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn list_corpora(pool: &SqlitePool) -> Result<Vec<Corpus>> {
    let rows = sqlx::query("SELECT * FROM corpus ORDER BY library, version")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(corpus_from_row).collect())
}

pub async fn get_corpus(pool: &SqlitePool, id: i64) -> Result<Option<Corpus>> {
    let row = sqlx::query("SELECT * FROM corpus WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(corpus_from_row))
}

/// Look up by the client-facing `library@version`.
pub async fn get_corpus_by_id(pool: &SqlitePool, corpus_id: &str) -> Result<Option<Corpus>> {
    let (library, version) = parse_corpus_id(corpus_id)?;
    let row = sqlx::query("SELECT * FROM corpus WHERE library = ?1 AND version = ?2")
        .bind(library)
        .bind(version)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(corpus_from_row))
}

/// Every corpus for one library, newest row first — what `docs__resolve`
/// answers a bare library name with.
pub async fn find_corpora(pool: &SqlitePool, library: &str) -> Result<Vec<Corpus>> {
    let rows = sqlx::query("SELECT * FROM corpus WHERE library = ?1 ORDER BY version DESC")
        .bind(library)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(corpus_from_row).collect())
}

pub async fn set_corpus_status(pool: &SqlitePool, id: i64, status: &str) -> Result<()> {
    sqlx::query("UPDATE corpus SET status=?2, updated_at=datetime('now') WHERE id=?1")
        .bind(id)
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record a score without the surrounding run — the shortcut retrieval tests
/// use. [`record_eval_run`] is what an `eval_run` job calls.
pub async fn set_corpus_eval_score(pool: &SqlitePool, id: i64, score: f64) -> Result<()> {
    sqlx::query("UPDATE corpus SET eval_score=?2, updated_at=datetime('now') WHERE id=?1")
        .bind(id)
        .bind(score)
        .execute(pool)
        .await?;
    Ok(())
}

/// Re-pin the embedding identity — the tail of a re-embed job.
pub async fn set_corpus_embed(pool: &SqlitePool, id: i64, embed: &EmbedIdentity) -> Result<()> {
    sqlx::query(
        "UPDATE corpus SET embed_upstream=?2, embed_model=?3, embed_dims=?4,
         updated_at=datetime('now') WHERE id=?1",
    )
    .bind(id)
    .bind(&embed.upstream)
    .bind(&embed.model)
    .bind(embed.dims as i64)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record what an ingest run actually crawled — the date it ran and the kind
/// the fetcher settled on. Both show in `docs__resolve`, so they are written
/// from the run rather than from what the wizard guessed beforehand.
pub async fn set_corpus_crawl(
    pool: &SqlitePool,
    id: i64,
    crawl_date: &str,
    source_kind: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE corpus SET crawl_date=?2, source_kind=?3, updated_at=datetime('now') WHERE id=?1",
    )
    .bind(id)
    .bind(crawl_date)
    .bind(source_kind)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_corpus(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM corpus WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Recount `chunk_count` from the chunk table. Called at the end of an ingest
/// so the cached number can never drift from what retrieval actually sees.
pub async fn refresh_chunk_count(pool: &SqlitePool, corpus_id: i64) -> Result<i64> {
    let row = sqlx::query(
        "UPDATE corpus SET chunk_count = (SELECT COUNT(*) FROM chunk WHERE corpus_id = ?1),
         updated_at = datetime('now') WHERE id = ?1 RETURNING chunk_count",
    )
    .bind(corpus_id)
    .fetch_one(pool)
    .await?;
    Ok(row.get("chunk_count"))
}

/// How many chunks in this corpus still have no vector — the re-embed badge's
/// input, and a guard an ingest can assert on before flipping to `ready`.
pub async fn count_unembedded(pool: &SqlitePool, corpus_id: i64) -> Result<i64> {
    let row =
        sqlx::query("SELECT COUNT(*) AS n FROM chunk WHERE corpus_id = ?1 AND embedding IS NULL")
            .bind(corpus_id)
            .fetch_one(pool)
            .await?;
    Ok(row.get("n"))
}

// ---------------------------------------------------------------------------
// Sources and documents
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Source {
    pub id: i64,
    pub corpus_id: i64,
    pub root: String,
    pub kind: String,
    /// Domains a fetch may touch. Everything else fails (§8).
    pub fence: Vec<String>,
    pub created_at: String,
}

fn source_from_row(row: &sqlx::sqlite::SqliteRow) -> Source {
    Source {
        id: row.get("id"),
        corpus_id: row.get("corpus_id"),
        root: row.get("root"),
        kind: row.get("kind"),
        fence: parse_json_or(row.get::<String, _>("fence").as_str()),
        created_at: row.get("created_at"),
    }
}

pub async fn insert_source(
    pool: &SqlitePool,
    corpus_id: i64,
    root: &str,
    kind: &str,
    fence: &[String],
) -> Result<i64> {
    let res = sqlx::query(
        "INSERT INTO source (corpus_id, root, kind, fence) VALUES (?1,?2,?3,?4)
         ON CONFLICT(corpus_id, root) DO UPDATE SET kind=?3, fence=?4",
    )
    .bind(corpus_id)
    .bind(root)
    .bind(kind)
    .bind(to_json(&fence)?)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn list_sources(pool: &SqlitePool, corpus_id: i64) -> Result<Vec<Source>> {
    let rows = sqlx::query("SELECT * FROM source WHERE corpus_id = ?1 ORDER BY id")
        .bind(corpus_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(source_from_row).collect())
}

#[derive(Debug, Clone, Serialize)]
pub struct Document {
    pub id: i64,
    pub source_id: i64,
    pub url: String,
    pub content_hash: String,
    pub fetched_at: String,
}

fn document_from_row(row: &sqlx::sqlite::SqliteRow) -> Document {
    Document {
        id: row.get("id"),
        source_id: row.get("source_id"),
        url: row.get("url"),
        content_hash: row.get("content_hash"),
        fetched_at: row.get("fetched_at"),
    }
}

/// hex(sha256) of a document body — the incremental-re-ingest gate.
pub fn content_hash(body: &str) -> String {
    hex::encode(Sha256::digest(body.as_bytes()))
}

/// Record a fetched page. Returns `(document id, changed)`; `changed` is false
/// when the hash is the one already stored, which is the signal to skip the
/// model entirely (§8).
pub async fn upsert_document(
    pool: &SqlitePool,
    source_id: i64,
    url: &str,
    content_hash: &str,
) -> Result<(i64, bool)> {
    let existing =
        sqlx::query("SELECT id, content_hash FROM document WHERE source_id=?1 AND url=?2")
            .bind(source_id)
            .bind(url)
            .fetch_optional(pool)
            .await?;
    if let Some(row) = existing {
        let id: i64 = row.get("id");
        let old: String = row.get("content_hash");
        if old == content_hash {
            return Ok((id, false));
        }
        sqlx::query("UPDATE document SET content_hash=?2, fetched_at=datetime('now') WHERE id=?1")
            .bind(id)
            .bind(content_hash)
            .execute(pool)
            .await?;
        return Ok((id, true));
    }
    let res = sqlx::query("INSERT INTO document (source_id, url, content_hash) VALUES (?1,?2,?3)")
        .bind(source_id)
        .bind(url)
        .bind(content_hash)
        .execute(pool)
        .await?;
    Ok((res.last_insert_rowid(), true))
}

/// Forget a document's content hash, so the next ingest re-processes it.
///
/// The row has to exist before its chunks can reference it, which means the
/// hash is written before extraction has succeeded. If extraction then fails,
/// the gate in [`upsert_document`] would skip that document forever — this is
/// the undo, called on every failure path.
pub async fn mark_document_stale(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("UPDATE document SET content_hash = '' WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_document(pool: &SqlitePool, id: i64) -> Result<Option<Document>> {
    let row = sqlx::query("SELECT * FROM document WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(document_from_row))
}

pub async fn list_documents(pool: &SqlitePool, corpus_id: i64) -> Result<Vec<Document>> {
    let rows = sqlx::query(
        "SELECT d.* FROM document d JOIN source s ON s.id = d.source_id
         WHERE s.corpus_id = ?1 ORDER BY d.url",
    )
    .bind(corpus_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(document_from_row).collect())
}

// ---------------------------------------------------------------------------
// Chunks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: String,
    pub document_id: i64,
    pub corpus_id: i64,
    pub heading_path: String,
    pub span_start: i64,
    pub span_end: i64,
    /// Verbatim slice of the source document.
    pub payload: String,
    /// LLM-derived. Surfaced as a label, never as payload.
    pub derived_title: String,
    pub derived_summary: String,
}

fn chunk_from_row(row: &sqlx::sqlite::SqliteRow) -> Chunk {
    Chunk {
        id: row.get("id"),
        document_id: row.get("document_id"),
        corpus_id: row.get("corpus_id"),
        heading_path: row.get("heading_path"),
        span_start: row.get("span_start"),
        span_end: row.get("span_end"),
        payload: row.get("payload"),
        derived_title: row.get("derived_title"),
        derived_summary: row.get("derived_summary"),
    }
}

/// Stable chunk identity: hex(sha256(document url ‖ 0x00 ‖ span text)).
///
/// Over the *text*, not the byte offsets — a chunk whose content is unchanged
/// keeps its id when an earlier section of the page grows, which is what makes
/// citations and golden queries survive a re-ingest (§4). Two byte-identical
/// spans of the same document therefore collapse to one row, which is the right
/// answer for repeated boilerplate.
pub fn chunk_id(document_url: &str, payload: &str) -> String {
    let mut h = Sha256::new();
    h.update(document_url.as_bytes());
    h.update([0u8]);
    h.update(payload.as_bytes());
    hex::encode(h.finalize())
}

#[derive(Debug, Clone)]
pub struct NewChunk {
    pub corpus_id: i64,
    pub document_id: i64,
    pub heading_path: String,
    pub span_start: i64,
    pub span_end: i64,
    pub payload: String,
    pub derived_title: String,
    pub derived_summary: String,
    /// Unnormalised is fine — this is normalised before it is narrowed to f16.
    pub embedding: Option<Vec<f32>>,
}

impl NewChunk {
    pub fn new(
        corpus_id: i64,
        document_id: i64,
        span: (i64, i64),
        payload: impl Into<String>,
    ) -> Self {
        Self {
            corpus_id,
            document_id,
            heading_path: String::new(),
            span_start: span.0,
            span_end: span.1,
            payload: payload.into(),
            derived_title: String::new(),
            derived_summary: String::new(),
            embedding: None,
        }
    }
}

/// f32 vector → the stored blob: normalise, then narrow. Cosine is a bare dot
/// product afterwards, which is what the KNN kernel assumes.
fn embedding_blob(c: &NewChunk, dims: usize, corpus: &str) -> Result<Option<Vec<u8>>> {
    let Some(v) = &c.embedding else {
        return Ok(None);
    };
    if v.len() != dims {
        return Err(QuickdocError::DimsMismatch {
            corpus: corpus.to_string(),
            expected: dims,
            got: v.len(),
        });
    }
    let mut owned = v.clone();
    vector::l2_normalize(&mut owned);
    Ok(Some(vector::encode_f16(&owned)))
}

/// Insert or replace chunks for one document, in a single transaction, and
/// refresh the corpus's chunk count. Returns the ids in input order.
///
/// `dims` is the corpus's pinned width; a vector of any other width is an error
/// here rather than a silently unsearchable row.
pub async fn insert_chunks(
    pool: &SqlitePool,
    document_url: &str,
    dims: usize,
    chunks: &[NewChunk],
) -> Result<Vec<String>> {
    if chunks.is_empty() {
        return Ok(Vec::new());
    }
    let corpus_id = chunks[0].corpus_id;
    let corpus_label = corpus_id.to_string();
    let mut ids = Vec::with_capacity(chunks.len());
    let mut tx = pool.begin().await?;
    for c in chunks {
        let id = chunk_id(document_url, &c.payload);
        let blob = embedding_blob(c, dims, &corpus_label)?;
        sqlx::query(
            "INSERT INTO chunk (id, document_id, corpus_id, heading_path, span_start, span_end,
                payload, embedding, derived_title, derived_summary)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(id) DO UPDATE SET
                document_id=excluded.document_id, corpus_id=excluded.corpus_id,
                heading_path=excluded.heading_path, span_start=excluded.span_start,
                span_end=excluded.span_end, payload=excluded.payload,
                embedding=excluded.embedding, derived_title=excluded.derived_title,
                derived_summary=excluded.derived_summary",
        )
        .bind(&id)
        .bind(c.document_id)
        .bind(c.corpus_id)
        .bind(&c.heading_path)
        .bind(c.span_start)
        .bind(c.span_end)
        .bind(&c.payload)
        .bind(blob)
        .bind(&c.derived_title)
        .bind(&c.derived_summary)
        .execute(&mut *tx)
        .await?;
        ids.push(id);
    }
    tx.commit().await?;
    refresh_chunk_count(pool, corpus_id).await?;
    Ok(ids)
}

pub async fn get_chunk(pool: &SqlitePool, id: &str) -> Result<Option<Chunk>> {
    let row = sqlx::query("SELECT * FROM chunk WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(chunk_from_row))
}

/// Fetch a set of chunks by id in one round trip. Ids the corpus no longer has
/// are simply absent from the map — the caller decides what a miss means.
pub async fn get_chunks(pool: &SqlitePool, ids: &[String]) -> Result<HashMap<String, Chunk>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    // Runtime query rather than a macro: sqlx 0.9's macros can't take a
    // dynamically sized `IN` list.
    let mut qb = QueryBuilder::<Sqlite>::new("SELECT * FROM chunk WHERE id IN (");
    let mut sep = qb.separated(", ");
    for id in ids {
        sep.push_bind(id.clone());
    }
    qb.push(")");
    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows
        .iter()
        .map(|r| {
            let c = chunk_from_row(r);
            (c.id.clone(), c)
        })
        .collect())
}

/// `chunk id → the URL of the document it was sliced from`, for the deep links
/// `docs__query`'s markdown carries (§7). One round trip; ids the corpus no
/// longer has are simply absent.
pub async fn document_urls(pool: &SqlitePool, ids: &[String]) -> Result<HashMap<String, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut qb = QueryBuilder::<Sqlite>::new(
        "SELECT c.id AS id, d.url AS url FROM chunk c
         JOIN document d ON d.id = c.document_id WHERE c.id IN (",
    );
    let mut sep = qb.separated(", ");
    for id in ids {
        sep.push_bind(id.clone());
    }
    qb.push(")");
    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<String, _>("id"), r.get::<String, _>("url")))
        .collect())
}

pub async fn list_chunks(pool: &SqlitePool, document_id: i64) -> Result<Vec<Chunk>> {
    let rows = sqlx::query("SELECT * FROM chunk WHERE document_id = ?1 ORDER BY span_start")
        .bind(document_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(chunk_from_row).collect())
}

/// A random sample of a corpus's chunks — the input of a synthetic golden-query
/// run (§10). `limit` is how many the caller asked for; `0` (or more than the
/// corpus holds) returns every chunk, in id order.
///
/// Random rather than "the first N": the first N chunks of a corpus are the
/// first page of its documentation, and a set of golden queries about one page
/// measures one page.
pub async fn sample_chunks(pool: &SqlitePool, corpus_id: i64, limit: i64) -> Result<Vec<Chunk>> {
    let mut qb = QueryBuilder::<Sqlite>::new("SELECT * FROM chunk WHERE corpus_id = ");
    qb.push_bind(corpus_id);
    if limit > 0 {
        qb.push(" ORDER BY RANDOM() LIMIT ").push_bind(limit);
    } else {
        qb.push(" ORDER BY id");
    }
    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows.iter().map(chunk_from_row).collect())
}

/// Attach (or replace) a chunk's vector. Normalises and narrows to f16 first.
pub async fn set_chunk_embedding(
    pool: &SqlitePool,
    id: &str,
    dims: usize,
    v: &[f32],
) -> Result<()> {
    if v.len() != dims {
        return Err(QuickdocError::DimsMismatch {
            corpus: id.to_string(),
            expected: dims,
            got: v.len(),
        });
    }
    let mut owned = v.to_vec();
    vector::l2_normalize(&mut owned);
    sqlx::query("UPDATE chunk SET embedding = ?2 WHERE id = ?1")
        .bind(id)
        .bind(vector::encode_f16(&owned))
        .execute(pool)
        .await?;
    Ok(())
}

/// Drop every vector in a corpus, leaving the chunks (and their ids, so
/// citations and golden queries survive). The head of a re-embed onto a
/// different model: the old vectors are not merely stale, they are a different
/// space, and keeping any of them would make the corpus half-searchable in a way
/// nothing downstream could detect.
pub async fn clear_corpus_embeddings(pool: &SqlitePool, corpus_id: i64) -> Result<u64> {
    let res = sqlx::query("UPDATE chunk SET embedding = NULL WHERE corpus_id = ?1")
        .bind(corpus_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Chunks with no vector yet, oldest first — the re-embed job's work queue.
/// `limit` is the caller's batch size; `0` means every one of them.
pub async fn list_unembedded(pool: &SqlitePool, corpus_id: i64, limit: i64) -> Result<Vec<Chunk>> {
    let mut qb = QueryBuilder::<Sqlite>::new("SELECT * FROM chunk WHERE corpus_id = ");
    qb.push_bind(corpus_id);
    qb.push(" AND embedding IS NULL ORDER BY created_at, id");
    if limit > 0 {
        qb.push(" LIMIT ").push_bind(limit);
    }
    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows.iter().map(chunk_from_row).collect())
}

pub async fn delete_document_chunks(pool: &SqlitePool, document_id: i64) -> Result<u64> {
    let res = sqlx::query("DELETE FROM chunk WHERE document_id = ?1")
        .bind(document_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Forget one document entirely — the row, and through `chunk.document_id`'s
/// `ON DELETE CASCADE` every chunk sliced from it; `chunk_fts_ad` fires for
/// each cascaded chunk, so the BM25 index loses the text too. What a folder
/// sync calls for a file that disappeared: [`delete_document_chunks`] alone
/// would leave the row behind, and a later file at the same path would find a
/// stale hash waiting for it.
///
/// Returns the rows deleted (0 or 1). Like `delete_document_chunks` it does not
/// touch the corpus's cached `chunk_count`; the caller refreshes that once at
/// the end of its batch with [`refresh_chunk_count`].
pub async fn delete_document(pool: &SqlitePool, id: i64) -> Result<u64> {
    let res = sqlx::query("DELETE FROM document WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Slurp a corpus's vectors into the resident matrix (§5). One pass, ordered by
/// id so the matrix is reproducible.
pub async fn load_matrix(pool: &SqlitePool, corpus: &Corpus) -> Result<VectorMatrix> {
    let mut m = VectorMatrix::new(corpus.dims());
    let label = corpus.corpus_id();
    let rows = sqlx::query(
        "SELECT id, embedding FROM chunk
         WHERE corpus_id = ?1 AND embedding IS NOT NULL ORDER BY id",
    )
    .bind(corpus.id)
    .fetch_all(pool)
    .await?;
    for r in &rows {
        let blob: Vec<u8> = r.get("embedding");
        m.push_blob(&label, r.get::<String, _>("id"), &blob)?;
    }
    Ok(m)
}

// ---------------------------------------------------------------------------
// Golden queries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct GoldenQuery {
    pub id: i64,
    pub corpus_id: i64,
    pub query: String,
    pub expected_chunk_ids: Vec<String>,
    pub origin: String,
}

fn golden_from_row(row: &sqlx::sqlite::SqliteRow) -> GoldenQuery {
    GoldenQuery {
        id: row.get("id"),
        corpus_id: row.get("corpus_id"),
        query: row.get("query"),
        expected_chunk_ids: parse_json_or(row.get::<String, _>("expected_chunk_ids").as_str()),
        origin: row.get("origin"),
    }
}

pub async fn insert_golden_query(
    pool: &SqlitePool,
    corpus_id: i64,
    query: &str,
    expected_chunk_ids: &[String],
    origin: &str,
) -> Result<i64> {
    let res = sqlx::query(
        "INSERT INTO golden_query (corpus_id, query, expected_chunk_ids, origin)
         VALUES (?1,?2,?3,?4)",
    )
    .bind(corpus_id)
    .bind(query)
    .bind(to_json(&expected_chunk_ids)?)
    .bind(origin)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn list_golden_queries(pool: &SqlitePool, corpus_id: i64) -> Result<Vec<GoldenQuery>> {
    let rows = sqlx::query("SELECT * FROM golden_query WHERE corpus_id = ?1 ORDER BY id")
        .bind(corpus_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(golden_from_row).collect())
}

/// Edit one golden query. The expected ids are replaced wholesale — curating a
/// query is deciding what the right answer is, not appending to a list.
pub async fn update_golden_query(
    pool: &SqlitePool,
    id: i64,
    query: &str,
    expected_chunk_ids: &[String],
    origin: &str,
) -> Result<()> {
    sqlx::query("UPDATE golden_query SET query=?2, expected_chunk_ids=?3, origin=?4 WHERE id=?1")
        .bind(id)
        .bind(query)
        .bind(to_json(&expected_chunk_ids)?)
        .bind(origin)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_golden_query(pool: &SqlitePool, id: i64) -> Result<Option<GoldenQuery>> {
    let row = sqlx::query("SELECT * FROM golden_query WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(golden_from_row))
}

pub async fn delete_golden_query(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM golden_query WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Synthetic candidates (§10, §11) — proposals, until the owner says otherwise
// ---------------------------------------------------------------------------

/// One generated query awaiting curation. It is not a golden query and is never
/// scored: `eval` reads `golden_query` and nothing else.
#[derive(Debug, Clone, Serialize)]
pub struct GoldenCandidate {
    pub id: i64,
    pub corpus_id: i64,
    pub query: String,
    /// The chunk it was written from — attached by code, never by the model.
    pub expected_chunk_ids: Vec<String>,
    /// The model's one sentence about why that chunk answers it.
    pub rationale: String,
    /// The model that wrote it.
    pub model: String,
    /// `pending | accepted | rejected`.
    pub status: String,
    /// The golden query it became, when it was accepted.
    pub golden_query_id: Option<i64>,
    pub created_at: String,
    pub decided_at: String,
}

fn candidate_from_row(row: &sqlx::sqlite::SqliteRow) -> GoldenCandidate {
    GoldenCandidate {
        id: row.get("id"),
        corpus_id: row.get("corpus_id"),
        query: row.get("query"),
        expected_chunk_ids: parse_json_or(row.get::<String, _>("expected_chunk_ids").as_str()),
        rationale: row.get("rationale"),
        model: row.get("model"),
        status: row.get("status"),
        golden_query_id: row.get("golden_query_id"),
        created_at: row.get("created_at"),
        decided_at: row.get("decided_at"),
    }
}

#[derive(Debug, Clone)]
pub struct NewGoldenCandidate {
    pub corpus_id: i64,
    pub query: String,
    pub expected_chunk_ids: Vec<String>,
    pub rationale: String,
    pub model: String,
}

/// File a candidate. `None` means this corpus already has one with that query —
/// including one that was rejected, which is how a second run avoids proposing
/// something the owner has already turned down.
pub async fn insert_golden_candidate(
    pool: &SqlitePool,
    c: &NewGoldenCandidate,
) -> Result<Option<i64>> {
    let res = sqlx::query(
        "INSERT INTO golden_candidate (corpus_id, query, expected_chunk_ids, rationale, model)
         VALUES (?1,?2,?3,?4,?5) ON CONFLICT (corpus_id, query) DO NOTHING",
    )
    .bind(c.corpus_id)
    .bind(&c.query)
    .bind(to_json(&c.expected_chunk_ids)?)
    .bind(&c.rationale)
    .bind(&c.model)
    .execute(pool)
    .await?;
    Ok((res.rows_affected() > 0).then(|| res.last_insert_rowid()))
}

/// A corpus's candidates. Empty `status` lists every one of them; the curation
/// queue asks for `pending`.
pub async fn list_golden_candidates(
    pool: &SqlitePool,
    corpus_id: i64,
    status: &str,
) -> Result<Vec<GoldenCandidate>> {
    let mut qb = QueryBuilder::<Sqlite>::new("SELECT * FROM golden_candidate WHERE corpus_id = ");
    qb.push_bind(corpus_id);
    if !status.is_empty() {
        qb.push(" AND status = ").push_bind(status.to_string());
    }
    qb.push(" ORDER BY id");
    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows.iter().map(candidate_from_row).collect())
}

pub async fn get_golden_candidate(pool: &SqlitePool, id: i64) -> Result<Option<GoldenCandidate>> {
    let row = sqlx::query("SELECT * FROM golden_candidate WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(candidate_from_row))
}

/// Record the owner's decision. `golden_query_id` is the row an acceptance
/// created, so an accepted candidate stays traceable to what it became.
pub async fn decide_golden_candidate(
    pool: &SqlitePool,
    id: i64,
    status: &str,
    golden_query_id: Option<i64>,
) -> Result<()> {
    sqlx::query(
        "UPDATE golden_candidate
         SET status=?2, golden_query_id=?3, decided_at=datetime('now') WHERE id=?1",
    )
    .bind(id)
    .bind(status)
    .bind(golden_query_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Every query text this corpus already asks — its golden queries and its
/// candidates, whatever became of them. The dedup set a generation run starts
/// from, so it spends its turns on questions the corpus does not have yet.
pub async fn existing_query_texts(pool: &SqlitePool, corpus_id: i64) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT query FROM golden_query WHERE corpus_id = ?1
         UNION SELECT query FROM golden_candidate WHERE corpus_id = ?1",
    )
    .bind(corpus_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(|r| r.get::<String, _>("query")).collect())
}

// ---------------------------------------------------------------------------
// Eval runs (§10) — the score's history, and the regression badge
// ---------------------------------------------------------------------------

/// Float slack when comparing two hit rates. hit@k over N queries is a ratio of
/// small integers, so anything this close is the same measurement re-run, not a
/// regression.
const EVAL_EPSILON: f64 = 1e-9;

#[derive(Debug, Clone, Serialize)]
pub struct EvalRun {
    pub id: i64,
    pub corpus_id: i64,
    pub k: i64,
    pub queries: i64,
    pub hit_at_k: f64,
    pub mrr: f64,
    pub orphaned_queries: i64,
    /// Whether this run is the one that put the corpus below its best.
    pub regression: bool,
    /// The §6 stage parameters it was measured under, as JSON — two runs under
    /// different parameters are not comparable, so they are stored with it.
    pub params: Value,
    /// The full per-query breakdown.
    pub report: Value,
    pub created_at: String,
}

fn eval_run_from_row(row: &sqlx::sqlite::SqliteRow) -> EvalRun {
    EvalRun {
        id: row.get("id"),
        corpus_id: row.get("corpus_id"),
        k: row.get("k"),
        queries: row.get("queries"),
        hit_at_k: row.get("hit_at_k"),
        mrr: row.get("mrr"),
        orphaned_queries: row.get("orphaned_queries"),
        regression: row.get::<i64, _>("regression") != 0,
        params: parse_json_or(row.get::<String, _>("params").as_str()),
        report: parse_json_or(row.get::<String, _>("report").as_str()),
        created_at: row.get("created_at"),
    }
}

#[derive(Debug, Clone)]
pub struct NewEvalRun {
    pub corpus_id: i64,
    pub k: i64,
    pub queries: i64,
    pub hit_at_k: f64,
    pub mrr: f64,
    pub orphaned_queries: i64,
    pub params: Value,
    pub report: Value,
}

/// Store one eval run and re-decide the corpus's badge.
///
/// The rule, in one place because the tool surface and the UI both read its
/// result: a run **regresses** when it scores below the best this corpus has
/// measured *under the same conditions*. The badge clears by itself as soon as
/// a later comparable run recovers, and a corpus with no comparable prior run
/// can never carry it.
///
/// "Same conditions" is `(k, params)`, and it is the whole point. hit@1 and
/// hit@10 measure different things — a hit@1 run scoring 0.6 is not a
/// regression against a hit@10 best of 0.9, it is a different measurement —
/// and a run with the rerank stage off is not comparable to one with it on.
/// Comparing across those lights the badge for a change the owner made
/// deliberately, which trains everyone to ignore it. The class is narrow on
/// purpose: an incomparable run is not evidence of anything, so it is not
/// evidence of a regression either.
///
/// `eval_best` follows the same class, so the number the badge quotes is a
/// score this corpus really did reach under the settings it was just measured
/// with, not one it reached at a different `k`.
pub async fn record_eval_run(pool: &SqlitePool, run: &NewEvalRun) -> Result<EvalRun> {
    if get_corpus(pool, run.corpus_id).await?.is_none() {
        return Err(QuickdocError::CorpusNotFound(run.corpus_id.to_string()));
    }
    let params = to_json(&run.params)?;
    // `params` is compared as stored text: serde_json orders object keys, so
    // two runs under the same settings serialize identically.
    let best: Option<f64> = sqlx::query_scalar(
        "SELECT MAX(hit_at_k) FROM eval_run WHERE corpus_id = ?1 AND k = ?2 AND params = ?3",
    )
    .bind(run.corpus_id)
    .bind(run.k)
    .bind(&params)
    .fetch_one(pool)
    .await?;
    let regression = best.is_some_and(|b| run.hit_at_k + EVAL_EPSILON < b);
    let new_best = best.map_or(run.hit_at_k, |b| b.max(run.hit_at_k));

    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE corpus SET eval_score=?2, eval_best=?3, eval_regression=?4, eval_k=?5,
         eval_at=datetime('now'), updated_at=datetime('now') WHERE id=?1",
    )
    .bind(run.corpus_id)
    .bind(run.hit_at_k)
    .bind(new_best)
    .bind(i64::from(regression))
    .bind(run.k)
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query(
        "INSERT INTO eval_run (corpus_id, k, queries, hit_at_k, mrr, orphaned_queries,
            regression, params, report)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) RETURNING *",
    )
    .bind(run.corpus_id)
    .bind(run.k)
    .bind(run.queries)
    .bind(run.hit_at_k)
    .bind(run.mrr)
    .bind(run.orphaned_queries)
    .bind(i64::from(regression))
    .bind(&params)
    .bind(to_json(&run.report)?)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(eval_run_from_row(&row))
}

/// Newest first. `limit` of `0` returns the whole history.
pub async fn list_eval_runs(pool: &SqlitePool, corpus_id: i64, limit: i64) -> Result<Vec<EvalRun>> {
    let mut qb = QueryBuilder::<Sqlite>::new("SELECT * FROM eval_run WHERE corpus_id = ");
    qb.push_bind(corpus_id);
    qb.push(" ORDER BY id DESC");
    if limit > 0 {
        qb.push(" LIMIT ").push_bind(limit);
    }
    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows.iter().map(eval_run_from_row).collect())
}

// ---------------------------------------------------------------------------
// Doc requests (§7) — the queue the Docs tab shows
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct DocRequest {
    pub id: i64,
    pub library: String,
    /// Empty when the requester did not pin a version.
    pub version: String,
    pub reason: Option<String>,
    pub client_name: Option<String>,
    pub count: i64,
    pub first_requested_at: String,
    pub last_requested_at: String,
    pub status: String,
}

fn doc_request_from_row(row: &sqlx::sqlite::SqliteRow) -> DocRequest {
    DocRequest {
        id: row.get("id"),
        library: row.get("library"),
        version: row.get("version"),
        reason: row.get("reason"),
        client_name: row.get("client_name"),
        count: row.get("count"),
        first_requested_at: row.get("first_requested_at"),
        last_requested_at: row.get("last_requested_at"),
        status: row.get("status"),
    }
}

/// File a request, or bump the counter on the one already filed.
///
/// **A dismissed request stays dismissed.** Dismissing is the owner saying no,
/// and an agent asking again is not a vote that overrides it — a request that
/// reopened itself would come back to the top of the queue every time any agent
/// asked, which is exactly the pestering the dismiss button exists to stop. The
/// counter and `last_requested_at` still move, so renewed demand is visible in
/// the queue's dismissed filter and the owner can change their mind on evidence
/// rather than on nagging.
///
/// **A fulfilled request does reopen.** `fulfilled` is only ever set by an
/// ingest finishing, so a new request against it means the corpus that answered
/// it is gone or unusable — the ready-corpus short-circuit in `docs__request`
/// would have fired otherwise, and this call would never have happened.
///
/// `library` is lower-cased on the way in. `UNIQUE (library, version)` is a
/// BINARY comparison, so without this `Axum` and `axum` are two rows: §7's
/// counter never engages, and only one of them is ever closed by the ingest of
/// a corpus (which has one spelling of the name). Agents send both spellings.
pub async fn file_doc_request(
    pool: &SqlitePool,
    library: &str,
    version: &str,
    reason: Option<&str>,
    client_name: Option<&str>,
) -> Result<DocRequest> {
    let library = library.to_lowercase();
    let row = sqlx::query(
        "INSERT INTO doc_request (library, version, reason, client_name)
         VALUES (?1,?2,?3,?4)
         ON CONFLICT(library, version) DO UPDATE SET
            count = count + 1,
            last_requested_at = datetime('now'),
            reason = COALESCE(excluded.reason, reason),
            client_name = COALESCE(excluded.client_name, client_name),
            status = CASE status WHEN 'dismissed' THEN 'dismissed' ELSE 'pending' END
         RETURNING *",
    )
    .bind(library)
    .bind(version)
    .bind(reason)
    .bind(client_name)
    .fetch_one(pool)
    .await?;
    Ok(doc_request_from_row(&row))
}

/// `status` empty lists every request; otherwise filters (`pending` is the
/// queue the tab badge counts).
pub async fn list_doc_requests(pool: &SqlitePool, status: &str) -> Result<Vec<DocRequest>> {
    let mut qb = QueryBuilder::<Sqlite>::new("SELECT * FROM doc_request WHERE 1=1");
    if !status.is_empty() {
        qb.push(" AND status = ").push_bind(status.to_string());
    }
    qb.push(" ORDER BY count DESC, last_requested_at DESC");
    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows.iter().map(doc_request_from_row).collect())
}

pub async fn set_doc_request_status(pool: &SqlitePool, id: i64, status: &str) -> Result<()> {
    sqlx::query("UPDATE doc_request SET status = ?2 WHERE id = ?1")
        .bind(id)
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}

/// Close out every pending request a finished corpus answers — both the
/// version-pinned ones and the bare-library ones (§10).
///
/// `COLLATE NOCASE` because the corpus carries whatever spelling the owner
/// typed in the wizard while requests are lower-cased ([`file_doc_request`]);
/// matching on BINARY would leave a request pending forever behind the corpus
/// that answers it, with nothing but a manual dismiss to clear the tab badge.
pub async fn fulfill_doc_requests(pool: &SqlitePool, library: &str, version: &str) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE doc_request SET status='fulfilled'
         WHERE status='pending' AND library=?1 COLLATE NOCASE
           AND (version=?2 COLLATE NOCASE OR version='')",
    )
    .bind(library)
    .bind(version)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

// ---------------------------------------------------------------------------
// Portability (§10): export is a copy of the file, import is a copy back in
// ---------------------------------------------------------------------------

/// Migration version this build's schema is at. An export carries it; an import
/// checks it.
pub fn schema_version() -> i64 {
    sqlx::migrate!("./migrations")
        .migrations
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap_or(0)
}

/// One corpus as an export manifest describes it — enough to decide, before
/// downloading or accepting a file, whether it is the one you want and whether
/// this gateway can serve it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusSummary {
    pub corpus_id: String,
    pub library: String,
    pub version: String,
    pub status: String,
    pub embed_upstream: String,
    pub embed_model: String,
    pub embed_dims: i64,
    pub ingest_model: String,
    pub ingest_prompt_version: String,
    pub source_kind: String,
    pub crawl_date: String,
    pub chunk_count: i64,
    pub eval_score: Option<f64>,
    pub eval_regression: bool,
}

impl From<&Corpus> for CorpusSummary {
    fn from(c: &Corpus) -> Self {
        Self {
            corpus_id: c.corpus_id(),
            library: c.library.clone(),
            version: c.version.clone(),
            status: c.status.clone(),
            embed_upstream: c.embed_upstream.clone(),
            embed_model: c.embed_model.clone(),
            embed_dims: c.embed_dims,
            ingest_model: c.ingest_model.clone(),
            ingest_prompt_version: c.ingest_prompt_version.clone(),
            source_kind: c.source_kind.clone(),
            crawl_date: c.crawl_date.clone(),
            chunk_count: c.chunk_count,
            eval_score: c.eval_score,
            eval_regression: c.eval_regression,
        }
    }
}

/// What an export contains. Travels beside the file rather than inside it: the
/// file has to stay a plain SQLite database so that `rsync`-ing it is still a
/// complete backup (§3), which a container format would break.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: i64,
    pub exported_at: String,
    pub byte_size: u64,
    pub corpora: Vec<CorpusSummary>,
}

pub async fn manifest_for(pool: &SqlitePool, byte_size: u64, at: &str) -> Result<Manifest> {
    Ok(Manifest {
        schema_version: schema_version(),
        exported_at: at.to_string(),
        byte_size,
        corpora: list_corpora(pool)
            .await?
            .iter()
            .map(CorpusSummary::from)
            .collect(),
    })
}

/// Open a corpus file that came from somewhere else, upgrading it if it is
/// older than this build.
///
/// The caller passes a *copy* — the upgrade is a write. A file from a newer
/// build is refused by version rather than by the first missing column, and a
/// file that is not a corpus database at all is refused by name.
pub async fn open_import(path: &Path) -> Result<(SqlitePool, i64)> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .map_err(QuickdocError::Db)?
        .create_if_missing(false)
        .foreign_keys(true);
    let probe = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    let found: Option<i64> = sqlx::query("SELECT MAX(version) AS v FROM _sqlx_migrations")
        .fetch_optional(&probe)
        .await
        .map_err(|_| {
            QuickdocError::Invalid(
                "this file is not a quickdoc corpus database (no migration table)".into(),
            )
        })?
        .and_then(|r| r.get("v"));
    probe.close().await;
    let file_version =
        found.ok_or_else(|| QuickdocError::Invalid("corpus database has no schema".into()))?;
    let current = schema_version();
    if file_version > current {
        return Err(QuickdocError::Invalid(format!(
            "corpus file is at schema v{file_version}; this lmgw understands up to v{current} — \
             upgrade lmgw before importing it"
        )));
    }
    let pool = open(path).await?;
    Ok((pool, file_version))
}

/// Result of copying one corpus between two corpus databases.
#[derive(Debug, Clone, Serialize)]
pub struct CopyOutcome {
    pub corpus_id: String,
    pub dest_id: i64,
    pub documents: i64,
    pub chunks: i64,
    /// A corpus of the same `library@version` was dropped to make room.
    pub replaced: bool,
}

/// Copy one corpus — metadata, sources, documents, chunks (vectors included)
/// and golden queries — from `src` into `dest`.
///
/// The unit of both export-one-corpus and import, so the two directions cannot
/// drift. Chunk ids are content hashes, so they survive the trip and citations
/// and golden queries keep pointing at the same text. Undecided **candidates**
/// (§11) stay behind: what travels is what the owner accepted, and an
/// unreviewed proposal belongs to the queue it was proposed into. `replace` is required to
/// overwrite an existing `library@version`: silently merging two corpora that
/// claim the same id would produce one that matches neither's metadata.
pub async fn copy_corpus(
    src: &SqlitePool,
    dest: &SqlitePool,
    src_corpus_id: i64,
    replace: bool,
) -> Result<CopyOutcome> {
    let c = get_corpus(src, src_corpus_id)
        .await?
        .ok_or_else(|| QuickdocError::CorpusNotFound(src_corpus_id.to_string()))?;
    let label = c.corpus_id();
    let mut replaced = false;
    if let Some(existing) = get_corpus_by_id(dest, &label).await? {
        if !replace {
            return Err(QuickdocError::Invalid(format!(
                "corpus {label} already exists here — import it with replace=true to overwrite it"
            )));
        }
        delete_corpus(dest, existing.id).await?;
        replaced = true;
    }

    let dest_id = sqlx::query(
        "INSERT INTO corpus (library, version, status, embed_upstream, embed_model, embed_dims,
            ingest_model, ingest_prompt_version, crawl_date, source_kind, eval_score, eval_best,
            eval_regression, eval_k, eval_at, chunk_count, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
    )
    .bind(&c.library)
    .bind(&c.version)
    .bind(&c.status)
    .bind(&c.embed_upstream)
    .bind(&c.embed_model)
    .bind(c.embed_dims)
    .bind(&c.ingest_model)
    .bind(&c.ingest_prompt_version)
    .bind(&c.crawl_date)
    .bind(&c.source_kind)
    .bind(c.eval_score)
    .bind(c.eval_best)
    .bind(i64::from(c.eval_regression))
    .bind(c.eval_k)
    .bind(&c.eval_at)
    .bind(c.chunk_count)
    .bind(&c.created_at)
    .execute(dest)
    .await?
    .last_insert_rowid();

    let mut documents = 0i64;
    let mut chunks = 0i64;
    for s in list_sources(src, c.id).await? {
        let dest_source = insert_source(dest, dest_id, &s.root, &s.kind, &s.fence).await?;
        let docs = sqlx::query("SELECT * FROM document WHERE source_id = ?1 ORDER BY id")
            .bind(s.id)
            .fetch_all(src)
            .await?;
        for row in &docs {
            let d = document_from_row(row);
            let dest_doc = sqlx::query(
                "INSERT INTO document (source_id, url, content_hash, fetched_at)
                 VALUES (?1,?2,?3,?4)",
            )
            .bind(dest_source)
            .bind(&d.url)
            .bind(&d.content_hash)
            .bind(&d.fetched_at)
            .execute(dest)
            .await?
            .last_insert_rowid();
            documents += 1;
            chunks += copy_chunks(src, dest, d.id, dest_doc, dest_id).await?;
        }
    }

    for g in list_golden_queries(src, c.id).await? {
        insert_golden_query(dest, dest_id, &g.query, &g.expected_chunk_ids, &g.origin).await?;
    }
    for run in list_eval_runs(src, c.id, 0).await?.iter().rev() {
        sqlx::query(
            "INSERT INTO eval_run (corpus_id, k, queries, hit_at_k, mrr, orphaned_queries,
                regression, params, report, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        )
        .bind(dest_id)
        .bind(run.k)
        .bind(run.queries)
        .bind(run.hit_at_k)
        .bind(run.mrr)
        .bind(run.orphaned_queries)
        .bind(i64::from(run.regression))
        .bind(to_json(&run.params)?)
        .bind(to_json(&run.report)?)
        .bind(&run.created_at)
        .execute(dest)
        .await?;
    }

    refresh_chunk_count(dest, dest_id).await?;
    Ok(CopyOutcome {
        corpus_id: label,
        dest_id,
        documents,
        chunks,
        replaced,
    })
}

/// Chunk rows copy verbatim, embedding blob included: re-narrowing an f16
/// vector through f32 would be a lossy no-op, and re-embedding a corpus that
/// already has vectors is not what an import is.
async fn copy_chunks(
    src: &SqlitePool,
    dest: &SqlitePool,
    src_doc: i64,
    dest_doc: i64,
    dest_corpus: i64,
) -> Result<i64> {
    let rows = sqlx::query("SELECT * FROM chunk WHERE document_id = ?1 ORDER BY span_start")
        .bind(src_doc)
        .fetch_all(src)
        .await?;
    let mut tx = dest.begin().await?;
    for r in &rows {
        sqlx::query(
            "INSERT INTO chunk (id, document_id, corpus_id, heading_path, span_start, span_end,
                payload, embedding, derived_title, derived_summary, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(r.get::<String, _>("id"))
        .bind(dest_doc)
        .bind(dest_corpus)
        .bind(r.get::<String, _>("heading_path"))
        .bind(r.get::<i64, _>("span_start"))
        .bind(r.get::<i64, _>("span_end"))
        .bind(r.get::<String, _>("payload"))
        .bind(r.get::<Option<Vec<u8>>, _>("embedding"))
        .bind(r.get::<String, _>("derived_title"))
        .bind(r.get::<String, _>("derived_summary"))
        .bind(r.get::<String, _>("created_at"))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(rows.len() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        open_in_memory().await.unwrap()
    }

    async fn seed_corpus(pool: &SqlitePool) -> (i64, i64) {
        let cid = insert_corpus(
            pool,
            &NewCorpus::new("axum", "0.8", EmbedIdentity::new("embed", "bge-m3", 4)),
        )
        .await
        .unwrap();
        let sid = insert_source(pool, cid, "https://docs.rs/axum", "markdown", &[])
            .await
            .unwrap();
        let (did, _) = upsert_document(pool, sid, "https://docs.rs/axum/routing", "h1")
            .await
            .unwrap();
        (cid, did)
    }

    #[tokio::test]
    async fn chunk_ids_survive_a_shifted_span() {
        let a = chunk_id("u", "the same text");
        let b = chunk_id("u", "the same text");
        assert_eq!(a, b, "identity is the text, not the offsets");
        assert_ne!(a, chunk_id("other", "the same text"));
    }

    #[tokio::test]
    async fn reingesting_a_document_reuses_chunk_ids_and_keeps_the_count_right() {
        let pool = pool().await;
        let (cid, did) = seed_corpus(&pool).await;
        let url = "https://docs.rs/axum/routing";

        let first = insert_chunks(
            &pool,
            url,
            4,
            &[
                NewChunk::new(cid, did, (0, 10), "routing get"),
                NewChunk::new(cid, did, (10, 20), "routing post"),
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            get_corpus(&pool, cid).await.unwrap().unwrap().chunk_count,
            2
        );

        // The page grew a preamble: same text, different offsets, same ids.
        let second = insert_chunks(
            &pool,
            url,
            4,
            &[
                NewChunk::new(cid, did, (40, 50), "routing get"),
                NewChunk::new(cid, did, (50, 60), "routing post"),
            ],
        )
        .await
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            get_corpus(&pool, cid).await.unwrap().unwrap().chunk_count,
            2
        );
        assert_eq!(
            get_chunk(&pool, &first[0])
                .await
                .unwrap()
                .unwrap()
                .span_start,
            40
        );
    }

    #[tokio::test]
    async fn unchanged_documents_report_no_change() {
        let pool = pool().await;
        let (cid, _) = seed_corpus(&pool).await;
        let sid = list_sources(&pool, cid).await.unwrap()[0].id;
        let (id, changed) = upsert_document(&pool, sid, "https://docs.rs/axum/routing", "h1")
            .await
            .unwrap();
        assert!(!changed, "same hash must not re-run the model");
        let (again, changed) = upsert_document(&pool, sid, "https://docs.rs/axum/routing", "h2")
            .await
            .unwrap();
        assert_eq!(id, again);
        assert!(changed);
    }

    /// The cascade is the whole contract of `delete_document`: the chunks go
    /// with the row, and the FTS triggers fire for cascaded rows too, so a
    /// deleted file cannot keep answering BM25 queries.
    #[tokio::test]
    async fn deleting_a_document_takes_its_chunks_and_their_fts_postings() {
        let pool = pool().await;
        let (cid, did) = seed_corpus(&pool).await;
        let sid = list_sources(&pool, cid).await.unwrap()[0].id;
        let (other, _) = upsert_document(&pool, sid, "https://docs.rs/axum/extract", "h9")
            .await
            .unwrap();
        insert_chunks(
            &pool,
            "https://docs.rs/axum/routing",
            4,
            &[NewChunk::new(cid, did, (0, 20), "zanzibar routing table")],
        )
        .await
        .unwrap();
        insert_chunks(
            &pool,
            "https://docs.rs/axum/extract",
            4,
            &[NewChunk::new(cid, other, (0, 20), "extractor zanzibar")],
        )
        .await
        .unwrap();
        let fts_hits = |pool: SqlitePool| async move {
            sqlx::query("SELECT COUNT(*) AS n FROM chunk_fts WHERE chunk_fts MATCH '\"zanzibar\"'")
                .fetch_one(&pool)
                .await
                .unwrap()
                .get::<i64, _>("n")
        };
        assert_eq!(fts_hits(pool.clone()).await, 2);

        assert_eq!(delete_document(&pool, did).await.unwrap(), 1);
        assert!(get_document(&pool, did).await.unwrap().is_none());
        assert!(list_chunks(&pool, did).await.unwrap().is_empty());
        assert_eq!(
            fts_hits(pool.clone()).await,
            1,
            "the cascaded chunk's FTS posting must go with it"
        );
        // The other document is untouched, and a second delete is a no-op.
        assert_eq!(list_chunks(&pool, other).await.unwrap().len(), 1);
        assert_eq!(delete_document(&pool, did).await.unwrap(), 0);
        assert_eq!(refresh_chunk_count(&pool, cid).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn embeddings_round_trip_into_the_matrix() {
        let pool = pool().await;
        let (cid, did) = seed_corpus(&pool).await;
        let mut c = NewChunk::new(cid, did, (0, 3), "one");
        // Deliberately unnormalised: the store owns normalisation.
        c.embedding = Some(vec![2.0, 0.0, 0.0, 0.0]);
        let ids = insert_chunks(&pool, "u", 4, &[c]).await.unwrap();
        assert_eq!(count_unembedded(&pool, cid).await.unwrap(), 0);

        let corpus = get_corpus(&pool, cid).await.unwrap().unwrap();
        let m = load_matrix(&pool, &corpus).await.unwrap();
        assert_eq!(m.len(), 1);
        let top = m.search("axum@0.8", &[1.0, 0.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(top[0].0, ids[0]);
        assert!((top[0].1 - 1.0).abs() < 1e-3, "cosine {} != 1", top[0].1);
    }

    #[tokio::test]
    async fn a_wrong_width_vector_is_refused_not_stored() {
        let pool = pool().await;
        let (cid, did) = seed_corpus(&pool).await;
        let mut c = NewChunk::new(cid, did, (0, 3), "one");
        c.embedding = Some(vec![1.0, 0.0]);
        assert!(insert_chunks(&pool, "u", 4, &[c]).await.is_err());
    }

    #[tokio::test]
    async fn repeat_doc_requests_bump_the_counter() {
        let pool = pool().await;
        let r = file_doc_request(&pool, "tower", "", Some("no corpus"), Some("claude-code"))
            .await
            .unwrap();
        assert_eq!(r.count, 1);
        let r = file_doc_request(&pool, "tower", "", None, None)
            .await
            .unwrap();
        assert_eq!(r.count, 2, "a repeat bumps, it does not pile up");
        assert_eq!(
            r.reason.as_deref(),
            Some("no corpus"),
            "keeps the first reason"
        );
        assert_eq!(list_doc_requests(&pool, "pending").await.unwrap().len(), 1);

        assert_eq!(
            fulfill_doc_requests(&pool, "tower", "0.5").await.unwrap(),
            1
        );
        assert!(list_doc_requests(&pool, "pending")
            .await
            .unwrap()
            .is_empty());
    }

    /// Agents send `Axum`, `axum` and `AXUM` for the same library. If case
    /// survived into the row, `UNIQUE (library, version)` would compare BINARY:
    /// §7's counter would never engage, and the ingest of a corpus called
    /// `axum` would close only one of the piles — the rest would sit `pending`
    /// forever, holding the Docs tab badge above zero with nothing the owner
    /// could do but dismiss them by hand.
    #[tokio::test]
    async fn doc_requests_ignore_the_case_the_agent_used() {
        let pool = pool().await;
        for spelling in ["Axum", "axum", "AXUM"] {
            file_doc_request(&pool, spelling, "0.8", None, None)
                .await
                .unwrap();
        }
        let pending = list_doc_requests(&pool, "pending").await.unwrap();
        assert_eq!(pending.len(), 1, "three spellings are one request");
        assert_eq!(pending[0].count, 3, "the counter bumped instead");
        assert_eq!(pending[0].library, "axum");

        // The corpus keeps whatever spelling the owner typed in the wizard, so
        // fulfilment has to meet the request halfway.
        assert_eq!(
            fulfill_doc_requests(&pool, "Axum", "0.8").await.unwrap(),
            1,
            "a differently-cased corpus still closes the request it answers"
        );
        assert!(list_doc_requests(&pool, "pending")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn corpus_ids_round_trip() {
        let pool = pool().await;
        seed_corpus(&pool).await;
        let c = get_corpus_by_id(&pool, "axum@0.8").await.unwrap().unwrap();
        assert_eq!(c.corpus_id(), "axum@0.8");
        assert!(get_corpus_by_id(&pool, "axum").await.is_err());
        assert!(get_corpus_by_id(&pool, "axum@9.9").await.unwrap().is_none());
    }

    /// A candidate is a proposal: it is never scored, a decision is recorded
    /// rather than deleted, and a second run cannot re-file what was turned down.
    #[tokio::test]
    async fn candidates_are_deduped_decided_and_never_scored_as_golden_queries() {
        let pool = pool().await;
        let (cid, did) = seed_corpus(&pool).await;
        let chunk = insert_chunks(
            &pool,
            "https://docs.rs/axum/routing",
            4,
            &[NewChunk::new(cid, did, (0, 10), "routing get")],
        )
        .await
        .unwrap()[0]
            .clone();
        let candidate = |q: &str| NewGoldenCandidate {
            corpus_id: cid,
            query: q.to_string(),
            expected_chunk_ids: vec![chunk.clone()],
            rationale: "it shows the route macro".into(),
            model: "ingest-model".into(),
        };

        let first = insert_golden_candidate(&pool, &candidate("how do I add a route?"))
            .await
            .unwrap()
            .expect("filed");
        assert!(
            insert_golden_candidate(&pool, &candidate("how do I add a route?"))
                .await
                .unwrap()
                .is_none(),
            "the same question is filed once"
        );
        let second = insert_golden_candidate(&pool, &candidate("how do I nest routers?"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            list_golden_candidates(&pool, cid, "").await.unwrap().len(),
            2
        );
        assert_eq!(
            list_golden_candidates(&pool, cid, "pending")
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            list_golden_queries(&pool, cid).await.unwrap().is_empty(),
            "generating never writes a golden query"
        );
        assert_eq!(
            existing_query_texts(&pool, cid).await.unwrap().len(),
            2,
            "both are in the dedup set the next run starts from"
        );

        let expected = [chunk.clone()];
        let gid = insert_golden_query(&pool, cid, "how do I add a route?", &expected, "synthetic")
            .await
            .unwrap();
        decide_golden_candidate(&pool, first, "accepted", Some(gid))
            .await
            .unwrap();
        decide_golden_candidate(&pool, second, "rejected", None)
            .await
            .unwrap();

        let queue = list_golden_candidates(&pool, cid, "pending").await.unwrap();
        assert!(queue.is_empty(), "a decided candidate leaves the queue");
        let accepted = get_golden_candidate(&pool, first).await.unwrap().unwrap();
        assert_eq!(accepted.status, "accepted");
        assert_eq!(accepted.golden_query_id, Some(gid));
        assert!(!accepted.decided_at.is_empty());
        let rejected = get_golden_candidate(&pool, second).await.unwrap().unwrap();
        assert_eq!(rejected.status, "rejected");
        assert!(
            insert_golden_candidate(&pool, &candidate("how do I nest routers?"))
                .await
                .unwrap()
                .is_none(),
            "a rejected question is not proposed again"
        );
        assert_eq!(list_golden_queries(&pool, cid).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn sampling_asks_for_what_it_wants_and_takes_everything_at_zero() {
        let pool = pool().await;
        let (cid, did) = seed_corpus(&pool).await;
        let chunks: Vec<NewChunk> = (0..5)
            .map(|i| NewChunk::new(cid, did, (i * 10, i * 10 + 8), format!("chunk {i}")))
            .collect();
        insert_chunks(&pool, "https://docs.rs/axum/routing", 4, &chunks)
            .await
            .unwrap();
        assert_eq!(sample_chunks(&pool, cid, 0).await.unwrap().len(), 5);
        assert_eq!(sample_chunks(&pool, cid, 2).await.unwrap().len(), 2);
        assert_eq!(
            sample_chunks(&pool, cid, 50).await.unwrap().len(),
            5,
            "asking for more than there is returns what there is"
        );
    }
}
