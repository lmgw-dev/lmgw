//! `knowledge.db` (chat-complete design §9.1): its own SQLite file, its own
//! pool, its own migrations (`migrations_knowledge/`), clamped to 0600 like
//! `lmgw.sqlite` — the documents in it are the owner's private ones.
//!
//! Plain row functions. What a row *means* — pinning, ingestion, retrieval —
//! lives in the sibling modules.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use quickdoc_core::embed::EmbedIdentity;
use quickdoc_core::retrieve::FtsWeights;
use quickdoc_core::vector::{self, VectorMatrix};
use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};

pub type KResult<T> = Result<T, sqlx::Error>;

/// Open (creating if needed) `knowledge.db`, run its migrations, enforce
/// 0600 on the file and its WAL sidecars — before the migrator as well as
/// after, for the reason `crate::store::open` gives.
pub async fn open(path: &Path) -> anyhow::Result<SqlitePool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;
    crate::store::clamp_0600(path)?;
    sqlx::migrate!("./migrations_knowledge").run(&pool).await?;
    crate::store::clamp_0600(path)?;
    Ok(pool)
}

/// In-memory `knowledge.db` for tests.
pub async fn open_in_memory() -> anyhow::Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations_knowledge").run(&pool).await?;
    Ok(pool)
}

// ---------------------------------------------------------------------------
// Knowledge bases
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Kb {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub embed_alias: String,
    pub embed_upstream: String,
    pub embed_model: String,
    pub embed_dims: i64,
    /// Empty: no rerank stage.
    pub rerank_alias: String,
    /// Empty: text-less PDF pages are skipped and counted.
    pub vision_alias: String,
    pub chunk_tokens: i64,
    pub chunk_overlap: i64,
    pub mcp_visible: bool,
    /// `ready` | `re_embed_required`.
    pub status: String,
    /// Moves on every chunk write (a trigger), so a cached resident matrix
    /// knows when it is stale.
    pub vectors_rev: i64,
    pub created_at: String,
    pub updated_at: String,
}

impl Kb {
    pub fn embed_identity(&self) -> EmbedIdentity {
        EmbedIdentity::new(
            self.embed_upstream.clone(),
            self.embed_model.clone(),
            self.dims(),
        )
    }

    pub fn dims(&self) -> usize {
        self.embed_dims.max(0) as usize
    }

    /// How errors and notes name this base.
    pub fn label(&self) -> String {
        format!("knowledge base '{}'", self.name)
    }
}

fn kb_from_row(r: &SqliteRow) -> Kb {
    Kb {
        id: r.get("id"),
        name: r.get("name"),
        description: r.get("description"),
        embed_alias: r.get("embed_alias"),
        embed_upstream: r.get("embed_upstream"),
        embed_model: r.get("embed_model"),
        embed_dims: r.get("embed_dims"),
        rerank_alias: r.get("rerank_alias"),
        vision_alias: r.get("vision_alias"),
        chunk_tokens: r.get("chunk_tokens"),
        chunk_overlap: r.get("chunk_overlap"),
        mcp_visible: r.get::<i64, _>("mcp_visible") != 0,
        status: r.get("status"),
        vectors_rev: r.get("vectors_rev"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

#[derive(Debug, Clone)]
pub struct NewKb {
    pub name: String,
    pub description: String,
    pub embed_alias: String,
    pub embed: EmbedIdentity,
    pub rerank_alias: String,
    pub vision_alias: String,
    pub chunk_tokens: i64,
    pub chunk_overlap: i64,
    pub mcp_visible: bool,
}

pub async fn insert_kb(pool: &SqlitePool, k: &NewKb) -> KResult<i64> {
    let res = sqlx::query(
        "INSERT INTO kb (name, description, embed_alias, embed_upstream, embed_model, embed_dims,
            rerank_alias, vision_alias, chunk_tokens, chunk_overlap, mcp_visible)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
    )
    .bind(&k.name)
    .bind(&k.description)
    .bind(&k.embed_alias)
    .bind(&k.embed.upstream)
    .bind(&k.embed.model)
    .bind(k.embed.dims as i64)
    .bind(&k.rerank_alias)
    .bind(&k.vision_alias)
    .bind(k.chunk_tokens)
    .bind(k.chunk_overlap)
    .bind(k.mcp_visible as i64)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn get_kb(pool: &SqlitePool, id: i64) -> KResult<Option<Kb>> {
    let row = sqlx::query("SELECT * FROM kb WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(kb_from_row))
}

/// Case-insensitive, the way a model naming a base in `kb__search` would.
pub async fn get_kb_by_name(pool: &SqlitePool, name: &str) -> KResult<Option<Kb>> {
    let row = sqlx::query("SELECT * FROM kb WHERE name = ?1 COLLATE NOCASE")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(kb_from_row))
}

pub async fn list_kbs(pool: &SqlitePool) -> KResult<Vec<Kb>> {
    let rows = sqlx::query("SELECT * FROM kb ORDER BY name COLLATE NOCASE, id")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(kb_from_row).collect())
}

/// The columns an edit may change directly. The embedding pin is not among
/// them — it moves only through [`set_kb_embed`], which a re-embed drives.
#[derive(Debug, Clone, Default)]
pub struct KbEdit {
    pub name: Option<String>,
    pub description: Option<String>,
    pub rerank_alias: Option<String>,
    pub vision_alias: Option<String>,
    pub chunk_tokens: Option<i64>,
    pub chunk_overlap: Option<i64>,
    pub mcp_visible: Option<bool>,
}

pub async fn update_kb(pool: &SqlitePool, id: i64, e: &KbEdit) -> KResult<()> {
    sqlx::query(
        "UPDATE kb SET
            name = COALESCE(?2, name),
            description = COALESCE(?3, description),
            rerank_alias = COALESCE(?4, rerank_alias),
            vision_alias = COALESCE(?5, vision_alias),
            chunk_tokens = COALESCE(?6, chunk_tokens),
            chunk_overlap = COALESCE(?7, chunk_overlap),
            mcp_visible = COALESCE(?8, mcp_visible),
            updated_at = datetime('now')
         WHERE id = ?1",
    )
    .bind(id)
    .bind(&e.name)
    .bind(&e.description)
    .bind(&e.rerank_alias)
    .bind(&e.vision_alias)
    .bind(e.chunk_tokens)
    .bind(e.chunk_overlap)
    .bind(e.mcp_visible.map(|v| v as i64))
    .execute(pool)
    .await?;
    Ok(())
}

/// Move the pin. Only a re-embed calls this, and it clears the old vectors in
/// the same breath ([`clear_embeddings`]): vectors of two models in one base
/// are unsearchable in a way nothing downstream could detect.
pub async fn set_kb_embed(
    pool: &SqlitePool,
    id: i64,
    alias: &str,
    embed: &EmbedIdentity,
) -> KResult<()> {
    sqlx::query(
        "UPDATE kb SET embed_alias = ?2, embed_upstream = ?3, embed_model = ?4, embed_dims = ?5,
            updated_at = datetime('now') WHERE id = ?1",
    )
    .bind(id)
    .bind(alias)
    .bind(&embed.upstream)
    .bind(&embed.model)
    .bind(embed.dims as i64)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_kb_status(pool: &SqlitePool, id: i64, status: &str) -> KResult<()> {
    sqlx::query("UPDATE kb SET status = ?2, updated_at = datetime('now') WHERE id = ?1")
        .bind(id)
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete a base; its files and chunks cascade. Returns the sha256 of every
/// file it held, for the caller to drop the originals nothing else uses.
pub async fn delete_kb(pool: &SqlitePool, id: i64) -> KResult<Vec<String>> {
    let shas: Vec<String> = sqlx::query("SELECT sha256 FROM kb_file WHERE kb_id = ?1")
        .bind(id)
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| r.get("sha256"))
        .collect();
    sqlx::query("DELETE FROM kb WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(shas)
}

/// Per-base counts, for the list and for `kb__list`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct KbCounts {
    pub files: i64,
    pub ready: i64,
    pub pending: i64,
    pub ingesting: i64,
    pub failed: i64,
    pub chunks: i64,
    /// Chunks that have a vector; `chunks - embedded` are waiting for one.
    pub embedded: i64,
    pub bytes: i64,
}

/// Counts for every base. See [`kb_counts_for`].
pub async fn kb_counts(pool: &SqlitePool) -> KResult<HashMap<i64, KbCounts>> {
    counts(pool, None).await
}

/// Counts for the bases in `ids` only — what a search or a base's view needs,
/// so it never pays for the other bases.
pub async fn kb_counts_for(pool: &SqlitePool, ids: &[i64]) -> KResult<HashMap<i64, KbCounts>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    counts(pool, Some(ids)).await
}

/// The bases a count is restricted to, as ` AND kb_id IN (...)` — or nothing.
fn only(qb: &mut QueryBuilder<Sqlite>, ids: Option<&[i64]>, lead: &str) {
    if let Some(ids) = ids {
        qb.push(lead);
        qb.push("kb_id IN (");
        let mut sep = qb.separated(", ");
        for id in ids {
            sep.push_bind(*id);
        }
        qb.push(")");
    }
}

async fn counts(pool: &SqlitePool, ids: Option<&[i64]>) -> KResult<HashMap<i64, KbCounts>> {
    let mut out: HashMap<i64, KbCounts> = HashMap::new();
    let mut qb = QueryBuilder::<Sqlite>::new(
        "SELECT kb_id, status, COUNT(*) AS n, COALESCE(SUM(size), 0) AS bytes FROM kb_file",
    );
    only(&mut qb, ids, " WHERE ");
    qb.push(" GROUP BY kb_id, status");
    for r in qb.build().fetch_all(pool).await? {
        let c = out.entry(r.get("kb_id")).or_default();
        let n: i64 = r.get("n");
        c.files += n;
        c.bytes += r.get::<i64, _>("bytes");
        match r.get::<String, _>("status").as_str() {
            "ready" => c.ready += n,
            "pending" => c.pending += n,
            "ingesting" => c.ingesting += n,
            _ => c.failed += n,
        }
    }
    // Every chunk (a covering-index scan) and the few without a vector (the
    // partial index): `embedded` is the difference, never a scan of blobs.
    let mut qb = QueryBuilder::<Sqlite>::new("SELECT kb_id, COUNT(*) AS n FROM kb_chunk");
    only(&mut qb, ids, " WHERE ");
    qb.push(" GROUP BY kb_id");
    for r in qb.build().fetch_all(pool).await? {
        let c = out.entry(r.get("kb_id")).or_default();
        c.chunks = r.get("n");
        c.embedded = c.chunks;
    }
    let mut qb = QueryBuilder::<Sqlite>::new(
        "SELECT kb_id, COUNT(*) AS n FROM kb_chunk WHERE embedding IS NULL",
    );
    only(&mut qb, ids, " AND ");
    qb.push(" GROUP BY kb_id");
    for r in qb.build().fetch_all(pool).await? {
        let c = out.entry(r.get("kb_id")).or_default();
        c.embedded = c.chunks - r.get::<i64, _>("n");
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct KbFile {
    pub id: i64,
    pub kb_id: i64,
    pub name: String,
    pub kind: String,
    pub sub: String,
    pub mime: String,
    pub size: i64,
    pub sha256: String,
    /// `pending` | `ingesting` | `ready` | `failed`.
    pub status: String,
    pub error: Option<String>,
    pub pages: Option<i64>,
    pub skipped_pages: i64,
    pub notes: Vec<String>,
    pub chunk_count: i64,
    pub added_at: String,
    pub ingested_at: Option<String>,
}

/// Every column but `text`, which can be megabytes and only the source viewer
/// reads ([`file_text`]).
macro_rules! file_columns {
    () => {
        "id, kb_id, name, kind, sub, mime, size, sha256, status, error, pages, skipped_pages, \
         notes, chunk_count, added_at, ingested_at"
    };
}

fn file_from_row(r: &SqliteRow) -> KbFile {
    KbFile {
        id: r.get("id"),
        kb_id: r.get("kb_id"),
        name: r.get("name"),
        kind: r.get("kind"),
        sub: r.get("sub"),
        mime: r.get("mime"),
        size: r.get("size"),
        sha256: r.get("sha256"),
        status: r.get("status"),
        error: r.get("error"),
        pages: r.get("pages"),
        skipped_pages: r.get("skipped_pages"),
        notes: serde_json::from_str(&r.get::<String, _>("notes")).unwrap_or_default(),
        chunk_count: r.get("chunk_count"),
        added_at: r.get("added_at"),
        ingested_at: r.get("ingested_at"),
    }
}

#[derive(Debug, Clone)]
pub struct NewKbFile {
    pub kb_id: i64,
    pub name: String,
    pub kind: String,
    pub sub: String,
    pub mime: String,
    pub size: i64,
    pub sha256: String,
}

pub async fn insert_file(pool: &SqlitePool, f: &NewKbFile) -> KResult<i64> {
    let res = sqlx::query(
        "INSERT INTO kb_file (kb_id, name, kind, sub, mime, size, sha256)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
    )
    .bind(f.kb_id)
    .bind(&f.name)
    .bind(&f.kind)
    .bind(&f.sub)
    .bind(&f.mime)
    .bind(f.size)
    .bind(&f.sha256)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

/// A file re-uploaded under its name with different bytes: the row keeps its
/// id and takes the new content, back to `pending`. Its chunks stay until the
/// ingest replaces them, so the base is never emptier than it was.
pub async fn replace_file(pool: &SqlitePool, id: i64, f: &NewKbFile) -> KResult<()> {
    sqlx::query(
        "UPDATE kb_file SET kind = ?2, sub = ?3, mime = ?4, size = ?5, sha256 = ?6,
            status = 'pending', error = NULL, added_at = datetime('now')
         WHERE id = ?1",
    )
    .bind(id)
    .bind(&f.kind)
    .bind(&f.sub)
    .bind(&f.mime)
    .bind(f.size)
    .bind(&f.sha256)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_file(pool: &SqlitePool, id: i64) -> KResult<Option<KbFile>> {
    let row = sqlx::query(concat!(
        "SELECT ",
        file_columns!(),
        " FROM kb_file WHERE id = ?1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(file_from_row))
}

pub async fn list_files(pool: &SqlitePool, kb_id: i64) -> KResult<Vec<KbFile>> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        file_columns!(),
        " FROM kb_file WHERE kb_id = ?1 ORDER BY name COLLATE NOCASE, id"
    ))
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(file_from_row).collect())
}

/// Files of one base in `status`, oldest first — the ingest job's queue.
pub async fn files_in_status(pool: &SqlitePool, kb_id: i64, status: &str) -> KResult<Vec<KbFile>> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        file_columns!(),
        " FROM kb_file WHERE kb_id = ?1 AND status = ?2 ORDER BY id"
    ))
    .bind(kb_id)
    .bind(status)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(file_from_row).collect())
}

pub async fn find_file_by_sha(pool: &SqlitePool, kb_id: i64, sha: &str) -> KResult<Option<KbFile>> {
    let row = sqlx::query(concat!(
        "SELECT ",
        file_columns!(),
        " FROM kb_file WHERE kb_id = ?1 AND sha256 = ?2"
    ))
    .bind(kb_id)
    .bind(sha)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(file_from_row))
}

pub async fn find_file_by_name(
    pool: &SqlitePool,
    kb_id: i64,
    name: &str,
) -> KResult<Option<KbFile>> {
    let row = sqlx::query(concat!(
        "SELECT ",
        file_columns!(),
        " FROM kb_file WHERE kb_id = ?1 AND name = ?2 ORDER BY id LIMIT 1"
    ))
    .bind(kb_id)
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(file_from_row))
}

/// Set a file's status and its reason (`None` clears it).
pub async fn set_file_status(
    pool: &SqlitePool,
    id: i64,
    status: &str,
    error: Option<&str>,
) -> KResult<()> {
    sqlx::query("UPDATE kb_file SET status = ?2, error = ?3 WHERE id = ?1")
        .bind(id)
        .bind(status)
        .bind(error)
        .execute(pool)
        .await?;
    Ok(())
}

/// [`set_file_status`] for the version of the file a job is working on: a
/// no-op (`false`) when the row has since been re-uploaded with other bytes,
/// so a stale job can neither fail nor finish the new version.
pub async fn set_file_status_for(
    pool: &SqlitePool,
    id: i64,
    sha: &str,
    status: &str,
    error: Option<&str>,
) -> KResult<bool> {
    let res =
        sqlx::query("UPDATE kb_file SET status = ?2, error = ?3 WHERE id = ?1 AND sha256 = ?4")
            .bind(id)
            .bind(status)
            .bind(error)
            .bind(sha)
            .execute(pool)
            .await?;
    Ok(res.rows_affected() > 0)
}

/// Every file of a base back to `pending` — what a change of its chunk
/// settings asks for. Returns how many.
pub async fn mark_all_pending(pool: &SqlitePool, kb_id: i64) -> KResult<u64> {
    let res = sqlx::query(
        "UPDATE kb_file SET status = 'pending', error = NULL
         WHERE kb_id = ?1 AND status != 'ingesting'",
    )
    .bind(kb_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// A reason on every still-pending file of a base, without changing their
/// status — how a held or cancelled job says why they are still waiting.
pub async fn note_pending(pool: &SqlitePool, kb_id: i64, reason: &str) -> KResult<u64> {
    let res = sqlx::query("UPDATE kb_file SET error = ?2 WHERE kb_id = ?1 AND status = 'pending'")
        .bind(kb_id)
        .bind(reason)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Files a crash left `ingesting` go back to `pending`: nothing is running
/// them, and the next job must pick them up.
pub async fn reset_ingesting(pool: &SqlitePool, kb_id: Option<i64>) -> KResult<u64> {
    let res = sqlx::query(
        "UPDATE kb_file SET status = 'pending',
            error = 'interrupted — ingestion stopped before this file was done'
         WHERE status = 'ingesting' AND (?1 IS NULL OR kb_id = ?1)",
    )
    .bind(kb_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// What one ingest run learned about a file.
#[derive(Debug, Clone, Default)]
pub struct Ingested {
    pub pages: Option<i64>,
    pub skipped_pages: i64,
    pub notes: Vec<String>,
    pub text: String,
}

/// A chunk as the ingest job writes it.
#[derive(Debug, Clone)]
pub struct NewKbChunk {
    pub id: String,
    pub seq: i64,
    pub page: Option<i64>,
    pub heading_path: String,
    pub span_start: i64,
    pub span_end: i64,
    pub payload: String,
    pub tokens: i64,
    /// Unnormalised is fine — normalised before it is narrowed to f16.
    pub embedding: Option<Vec<f32>>,
}

/// Replace a file's chunks wholesale and mark it `ready`, in one transaction:
/// a search never sees half a file, and a chunk that no longer exists never
/// survives as an orphan. `dims` is the base's pinned width; a vector of any
/// other width is refused here rather than stored unsearchable.
///
/// `sha` is the version of the file that was ingested. When the row has been
/// re-uploaded with other bytes meanwhile (or deleted), nothing is written and
/// the answer is `false`: the row stays `pending` for the new version, and the
/// old content's chunks are not passed off as the new one's.
pub async fn finish_file(
    pool: &SqlitePool,
    kb_id: i64,
    file_id: i64,
    sha: &str,
    dims: usize,
    chunks: &[NewKbChunk],
    done: &Ingested,
) -> Result<bool, String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let current: Option<String> = sqlx::query("SELECT sha256 FROM kb_file WHERE id = ?1")
        .bind(file_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .map(|r| r.get("sha256"));
    if current.as_deref() != Some(sha) {
        return Ok(false);
    }
    sqlx::query("DELETE FROM kb_chunk WHERE file_id = ?1")
        .bind(file_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    for c in chunks {
        let blob = match &c.embedding {
            None => None,
            Some(v) if v.len() != dims => {
                return Err(format!(
                    "a {}-wide vector for a knowledge base pinned to {dims}",
                    v.len()
                ))
            }
            Some(v) => {
                let mut owned = v.clone();
                vector::l2_normalize(&mut owned);
                Some(vector::encode_f16(&owned))
            }
        };
        sqlx::query(
            "INSERT INTO kb_chunk (id, kb_id, file_id, seq, page, heading_path, span_start,
                span_end, payload, tokens, embedding)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        )
        .bind(&c.id)
        .bind(kb_id)
        .bind(file_id)
        .bind(c.seq)
        .bind(c.page)
        .bind(&c.heading_path)
        .bind(c.span_start)
        .bind(c.span_end)
        .bind(&c.payload)
        .bind(c.tokens)
        .bind(blob)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    }
    sqlx::query(
        "UPDATE kb_file SET status = 'ready', error = NULL, pages = ?2, skipped_pages = ?3,
            notes = ?4, text = ?5, chunk_count = ?6, ingested_at = datetime('now')
         WHERE id = ?1",
    )
    .bind(file_id)
    .bind(done.pages)
    .bind(done.skipped_pages)
    .bind(serde_json::to_string(&done.notes).unwrap_or_else(|_| "[]".into()))
    .bind(&done.text)
    .bind(chunks.len() as i64)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(true)
}

/// Delete one file (its chunks cascade). Returns its sha256 when it existed.
pub async fn delete_file(pool: &SqlitePool, id: i64) -> KResult<Option<String>> {
    let sha: Option<String> = sqlx::query("SELECT sha256 FROM kb_file WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .map(|r| r.get("sha256"));
    sqlx::query("DELETE FROM kb_file WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(sha)
}

/// Whether any file row still names this original.
pub async fn sha_in_use(pool: &SqlitePool, sha: &str) -> KResult<bool> {
    let row = sqlx::query("SELECT 1 FROM kb_file WHERE sha256 = ?1 LIMIT 1")
        .bind(sha)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// The extracted text the file's chunk spans point into; `None` until it has
/// been ingested once.
pub async fn file_text(pool: &SqlitePool, id: i64) -> KResult<Option<String>> {
    Ok(sqlx::query("SELECT text FROM kb_file WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .and_then(|r| r.get::<Option<String>, _>("text")))
}

// ---------------------------------------------------------------------------
// Chunks
// ---------------------------------------------------------------------------

/// A chunk with the names a citation needs.
#[derive(Debug, Clone, Serialize)]
pub struct KbChunk {
    pub id: String,
    pub kb_id: i64,
    pub kb_name: String,
    pub file_id: i64,
    pub file_name: String,
    /// The sha256 of the file version this chunk was cut from — what a
    /// stored citation names, so a later change of the file is noticed.
    pub file_sha: String,
    pub seq: i64,
    pub page: Option<i64>,
    pub heading_path: String,
    pub span_start: i64,
    pub span_end: i64,
    pub payload: String,
    pub tokens: i64,
}

impl KbChunk {
    /// What the reranker scores, the budget counts and a caller receives: the
    /// heading path over the payload.
    pub fn text(&self) -> String {
        if self.heading_path.is_empty() {
            self.payload.clone()
        } else {
            format!("{}\n{}", self.heading_path, self.payload)
        }
    }
}

macro_rules! chunk_select {
    () => {
        "SELECT c.id, c.kb_id, k.name AS kb_name, c.file_id, f.name AS file_name, f.sha256 AS file_sha, c.seq, c.page, \
         c.heading_path, c.span_start, c.span_end, c.payload, c.tokens FROM kb_chunk c \
         JOIN kb_file f ON f.id = c.file_id JOIN kb k ON k.id = c.kb_id"
    };
}

fn chunk_from_row(r: &SqliteRow) -> KbChunk {
    KbChunk {
        id: r.get("id"),
        kb_id: r.get("kb_id"),
        kb_name: r.get("kb_name"),
        file_id: r.get("file_id"),
        file_name: r.get("file_name"),
        file_sha: r.get("file_sha"),
        seq: r.get("seq"),
        page: r.get("page"),
        heading_path: r.get("heading_path"),
        span_start: r.get("span_start"),
        span_end: r.get("span_end"),
        payload: r.get("payload"),
        tokens: r.get("tokens"),
    }
}

pub async fn get_chunks(pool: &SqlitePool, ids: &[String]) -> KResult<HashMap<String, KbChunk>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut qb = QueryBuilder::<Sqlite>::new(chunk_select!());
    qb.push(" WHERE c.id IN (");
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

pub async fn get_chunk(pool: &SqlitePool, id: &str) -> KResult<Option<KbChunk>> {
    Ok(get_chunks(pool, &[id.to_string()]).await?.remove(id))
}

/// A file's chunks in reading order.
pub async fn file_chunks(pool: &SqlitePool, file_id: i64) -> KResult<Vec<KbChunk>> {
    let rows = sqlx::query(concat!(
        chunk_select!(),
        " WHERE c.file_id = ?1 ORDER BY c.seq"
    ))
    .bind(file_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(chunk_from_row).collect())
}

/// The stored vectors of chunks that already exist, by id — what a re-ingest
/// of an unchanged region reuses instead of paying for it again.
pub async fn stored_vectors(
    pool: &SqlitePool,
    kb_id: i64,
    ids: &[String],
) -> KResult<HashMap<String, Vec<f32>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut qb = QueryBuilder::<Sqlite>::new("SELECT id, embedding FROM kb_chunk WHERE kb_id = ");
    qb.push_bind(kb_id);
    qb.push(" AND embedding IS NOT NULL AND id IN (");
    let mut sep = qb.separated(", ");
    for id in ids {
        sep.push_bind(id.clone());
    }
    qb.push(")");
    let rows = qb.build().fetch_all(pool).await?;
    let mut out = HashMap::new();
    for r in rows {
        let blob: Vec<u8> = r.get("embedding");
        if let Ok(v) = vector::decode_f16_to_f32(&blob) {
            out.insert(r.get::<String, _>("id"), v);
        }
    }
    Ok(out)
}

/// BM25 over one base, `(id, score)` best first (lower is better).
pub async fn fts(
    pool: &SqlitePool,
    kb_id: i64,
    expr: &str,
    w: &FtsWeights,
    k: usize,
) -> KResult<Vec<(String, f32)>> {
    let rows = sqlx::query(
        "SELECT c.id AS id, bm25(kb_chunk_fts, ?1, ?2) AS score
         FROM kb_chunk_fts JOIN kb_chunk c ON c.rowid = kb_chunk_fts.rowid
         WHERE kb_chunk_fts MATCH ?3 AND c.kb_id = ?4
         ORDER BY score ASC LIMIT ?5",
    )
    .bind(w.payload as f64)
    .bind(w.heading_path as f64)
    .bind(expr)
    .bind(kb_id)
    .bind(k as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get("id"), r.get::<f64, _>("score") as f32))
        .collect())
}

/// One base's vectors, resident. Ordered by id so the matrix is reproducible.
pub async fn load_matrix(pool: &SqlitePool, kb: &Kb) -> Result<VectorMatrix, String> {
    let mut m = VectorMatrix::new(kb.dims());
    let label = kb.label();
    let rows = sqlx::query(
        "SELECT id, embedding FROM kb_chunk WHERE kb_id = ?1 AND embedding IS NOT NULL ORDER BY id",
    )
    .bind(kb.id)
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    for r in &rows {
        let blob: Vec<u8> = r.get("embedding");
        m.push_blob(&label, r.get::<String, _>("id"), &blob)
            .map_err(|e| e.to_string())?;
    }
    Ok(m)
}

/// Drop every vector of a base, leaving its chunks and their ids — the head
/// of a re-embed onto another model. The revision moves once.
pub async fn clear_embeddings(pool: &SqlitePool, kb_id: i64) -> KResult<u64> {
    let mut tx = pool.begin().await?;
    let res = sqlx::query("UPDATE kb_chunk SET embedding = NULL WHERE kb_id = ?1")
        .bind(kb_id)
        .execute(&mut *tx)
        .await?;
    bump_rev(&mut tx, kb_id).await?;
    tx.commit().await?;
    Ok(res.rows_affected())
}

async fn bump_rev(tx: &mut sqlx::Transaction<'_, Sqlite>, kb_id: i64) -> KResult<()> {
    sqlx::query("UPDATE kb SET vectors_rev = vectors_rev + 1 WHERE id = ?1")
        .bind(kb_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Which chunks without a vector a count or a queue takes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unembedded {
    /// Every one.
    All,
    /// Every one but those of a **failed** file: the file said why it cannot
    /// be read, its stale chunks are searched by keywords only, and they must
    /// not keep the whole base `re_embed_required`. What "the base is
    /// complete" is measured by.
    Live,
    /// The re-embed's queue: not the chunks of a file waiting to be
    /// re-ingested (that ingest replaces them, so a vector for the old text
    /// would be paid for and thrown away), and not a failed file's.
    Embeddable,
}

/// The count query for a scope; the status clause is spelled out per scope
/// so every statement is a literal.
macro_rules! unembedded_where {
    ($tail:literal, $scope:expr, $prefix:expr) => {
        match $scope {
            Unembedded::All => concat!(
                $prefix,
                " WHERE c.kb_id = ?1 AND c.embedding IS NULL",
                $tail
            ),
            Unembedded::Live => concat!(
                $prefix,
                " WHERE c.kb_id = ?1 AND c.embedding IS NULL AND f.status <> 'failed'",
                $tail
            ),
            Unembedded::Embeddable => concat!(
                $prefix,
                " WHERE c.kb_id = ?1 AND c.embedding IS NULL \
                 AND f.status NOT IN ('pending', 'failed')",
                $tail
            ),
        }
    };
}

/// Chunks with no vector, within `scope`.
pub async fn count_unembedded(pool: &SqlitePool, kb_id: i64, scope: Unembedded) -> KResult<i64> {
    let sql = unembedded_where!(
        "",
        scope,
        "SELECT COUNT(*) AS n FROM kb_chunk c JOIN kb_file f ON f.id = c.file_id"
    );
    sqlx::query(sql)
        .bind(kb_id)
        .fetch_one(pool)
        .await
        .map(|r| r.get("n"))
}

/// One file that holds chunks without a vector.
#[derive(Debug, Clone)]
pub struct UnembeddedFile {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub error: Option<String>,
    pub chunks: i64,
}

/// The files holding chunks without a vector, with how many each.
pub async fn unembedded_files(pool: &SqlitePool, kb_id: i64) -> KResult<Vec<UnembeddedFile>> {
    let rows = sqlx::query(
        "SELECT f.id, f.name, f.status, f.error, COUNT(*) AS n FROM kb_chunk c
         JOIN kb_file f ON f.id = c.file_id
         WHERE c.kb_id = ?1 AND c.embedding IS NULL GROUP BY f.id ORDER BY f.id",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| UnembeddedFile {
            id: r.get("id"),
            name: r.get("name"),
            status: r.get("status"),
            error: r.get("error"),
            chunks: r.get("n"),
        })
        .collect())
}

/// Chunks with no vector yet, in reading order — the re-embed job's queue.
/// `limit` is the caller's batch size; `scope` as in [`count_unembedded`].
pub async fn list_unembedded(
    pool: &SqlitePool,
    kb_id: i64,
    limit: i64,
    scope: Unembedded,
) -> KResult<Vec<KbChunk>> {
    let sql = unembedded_where!(
        " ORDER BY c.file_id, c.seq LIMIT ?2",
        scope,
        chunk_select!()
    );
    let rows = sqlx::query(sql)
        .bind(kb_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(chunk_from_row).collect())
}

/// How many chunks a base holds.
pub async fn chunk_total(pool: &SqlitePool, kb_id: i64) -> KResult<i64> {
    sqlx::query("SELECT COUNT(*) AS n FROM kb_chunk WHERE kb_id = ?1")
        .bind(kb_id)
        .fetch_one(pool)
        .await
        .map(|r| r.get("n"))
}

/// The `offset`-th chunk of a base in reading order, as `(heading path,
/// payload)`.
pub async fn nth_chunk(
    pool: &SqlitePool,
    kb_id: i64,
    offset: i64,
) -> KResult<Option<(String, String)>> {
    Ok(sqlx::query(
        "SELECT heading_path, payload FROM kb_chunk WHERE kb_id = ?1
         ORDER BY file_id, seq LIMIT 1 OFFSET ?2",
    )
    .bind(kb_id)
    .bind(offset)
    .fetch_optional(pool)
    .await?
    .map(|r| (r.get("heading_path"), r.get("payload"))))
}

/// One file's stored chunks against a token ceiling.
#[derive(Debug, Clone)]
pub struct FileFit {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub chunks: i64,
    /// Chunks whose stored tiktoken count times `ratio` is above the ceiling.
    pub over: i64,
}

/// Per file: how many stored chunks would exceed `ceiling` model tokens,
/// judged from the tiktoken counts stored with them (`tokens`) times `ratio`
/// — one query for the whole base, no text loaded, no tokenizer called.
/// A chunk is over when `ceil(tokens * ratio) > ceiling`, which for an integer
/// ceiling is `tokens * ratio > ceiling`.
pub async fn file_fits(
    pool: &SqlitePool,
    kb_id: i64,
    ratio: f64,
    ceiling: i64,
) -> KResult<Vec<FileFit>> {
    let rows = sqlx::query(
        "SELECT f.id, f.name, f.status, COUNT(*) AS n,
                COALESCE(SUM(CASE WHEN c.tokens * ?2 > ?3 THEN 1 ELSE 0 END), 0) AS over
         FROM kb_chunk c JOIN kb_file f ON f.id = c.file_id
         WHERE c.kb_id = ?1 GROUP BY f.id ORDER BY f.id",
    )
    .bind(kb_id)
    .bind(ratio)
    .bind(ceiling)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| FileFit {
            id: r.get("id"),
            name: r.get("name"),
            status: r.get("status"),
            chunks: r.get("n"),
            over: r.get("over"),
        })
        .collect())
}

/// Files back to `pending` for a re-chunk, each with the reason on it. A file
/// being ingested right now is left alone. Returns how many changed.
pub async fn mark_files_pending(pool: &SqlitePool, ids: &[i64], reason: &str) -> KResult<u64> {
    let mut n = 0;
    let mut tx = pool.begin().await?;
    for id in ids {
        n += sqlx::query(
            "UPDATE kb_file SET status = 'pending', error = ?2
             WHERE id = ?1 AND status <> 'ingesting'",
        )
        .bind(id)
        .bind(reason)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// Pages a vision model read
// ---------------------------------------------------------------------------

/// A page a vision model read, remembered per (file bytes, page, vision
/// alias, mode, prompt version): a re-chunk of an unchanged file never pays
/// for the same page twice. `mode` is the prompt kind (`ocr` | `structure`).
pub async fn get_page_read(
    pool: &SqlitePool,
    sha: &str,
    page: i64,
    alias: &str,
    mode: &str,
    prompt_version: &str,
) -> KResult<Option<String>> {
    Ok(sqlx::query(
        "SELECT text FROM kb_page_read
         WHERE file_sha = ?1 AND page = ?2 AND vision_alias = ?3 AND mode = ?4
           AND prompt_version = ?5",
    )
    .bind(sha)
    .bind(page)
    .bind(alias)
    .bind(mode)
    .bind(prompt_version)
    .fetch_optional(pool)
    .await?
    .map(|r| r.get("text")))
}

pub async fn put_page_read(
    pool: &SqlitePool,
    sha: &str,
    page: i64,
    alias: &str,
    mode: &str,
    prompt_version: &str,
    text: &str,
) -> KResult<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO kb_page_read
            (file_sha, page, vision_alias, mode, prompt_version, text)
         VALUES (?1,?2,?3,?4,?5,?6)",
    )
    .bind(sha)
    .bind(page)
    .bind(alias)
    .bind(mode)
    .bind(prompt_version)
    .bind(text)
    .execute(pool)
    .await?;
    Ok(())
}

/// Forget the pages read for bytes nothing names any more.
pub async fn delete_page_reads(pool: &SqlitePool, sha: &str) -> KResult<()> {
    sqlx::query("DELETE FROM kb_page_read WHERE file_sha = ?1")
        .bind(sha)
        .execute(pool)
        .await?;
    Ok(())
}

/// Write a batch of vectors in one transaction, moving the base's revision
/// once for the batch — one commit (one fsync), not one per chunk.
pub async fn set_chunk_embeddings(
    pool: &SqlitePool,
    kb_id: i64,
    batch: &[(String, Vec<f32>)],
) -> KResult<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for (id, v) in batch {
        let mut owned = v.clone();
        vector::l2_normalize(&mut owned);
        sqlx::query("UPDATE kb_chunk SET embedding = ?2 WHERE id = ?1")
            .bind(id)
            .bind(vector::encode_f16(&owned))
            .execute(&mut *tx)
            .await?;
    }
    bump_rev(&mut tx, kb_id).await?;
    tx.commit().await
}
