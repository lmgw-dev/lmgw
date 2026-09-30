//! Stored responses (§21 stage 2) — what makes `previous_response_id` work

use sqlx::{Row, SqlitePool};

use super::*;

/// One stored `/v1/responses` result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StoredResponse {
    pub id: String,
    /// Root of the `previous_response_id` chain (self, for a root).
    pub chain_id: String,
    pub previous_response_id: Option<String>,
    pub model: String,
    pub status: String,
    /// The response object as served, JSON-encoded.
    pub body: String,
    /// This turn's own `input` array, JSON-encoded.
    pub input_items: String,
    /// The IR conversation as the run ended, JSON-encoded `Vec<ir::Message>`.
    pub messages: String,
    /// Tool calls awaiting approval, JSON-encoded `Vec<agent::PendingCall>`.
    pub pending: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub created_at: String,
}

/// A conversation, summarized for the manager tab.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResponseChain {
    pub chain_id: String,
    /// Newest response in the chain — the one a client would continue from.
    pub head_id: String,
    pub model: String,
    pub status: String,
    pub responses: i64,
    pub first_at: String,
    pub last_at: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Bytes of stored body + conversation.
    ///
    /// Every response holds a full snapshot of the conversation as it stood, so
    /// a long chain costs more than the sum of its turns. That is deliberate —
    /// it makes replay exact and survives a mid-chain delete — but it is also
    /// the kind of growth that should be *visible* rather than discovered, so
    /// the manager tab shows this column.
    pub bytes: i64,
    /// Any response in the chain is waiting on an approval.
    pub awaiting_approval: bool,
}

fn stored_response_from_row(row: &sqlx::sqlite::SqliteRow) -> StoredResponse {
    StoredResponse {
        id: row.get("id"),
        chain_id: row.get("chain_id"),
        previous_response_id: row.get("previous_response_id"),
        model: row.get("model"),
        status: row.get("status"),
        body: row.get("body"),
        input_items: row.get("input_items"),
        messages: row.get("messages"),
        pending: row.get("pending"),
        input_tokens: row.get("input_tokens"),
        output_tokens: row.get("output_tokens"),
        created_at: row.get("created_at"),
    }
}

pub async fn insert_response(pool: &SqlitePool, r: &StoredResponse) -> DbResult<()> {
    sqlx::query(
        "INSERT INTO responses
           (id, chain_id, previous_response_id, model, status, body, input_items,
            messages, pending, input_tokens, output_tokens)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )
    .bind(&r.id)
    .bind(&r.chain_id)
    .bind(&r.previous_response_id)
    .bind(&r.model)
    .bind(&r.status)
    .bind(&r.body)
    .bind(&r.input_items)
    .bind(&r.messages)
    .bind(&r.pending)
    .bind(r.input_tokens)
    .bind(r.output_tokens)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_response(pool: &SqlitePool, id: &str) -> DbResult<Option<StoredResponse>> {
    let row = sqlx::query("SELECT * FROM responses WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(stored_response_from_row))
}

/// Delete one response. Its children are left alone but re-parented in effect —
/// they keep their `chain_id`, so the chain still lists and GCs as one unit.
pub async fn delete_response(pool: &SqlitePool, id: &str) -> DbResult<bool> {
    let res = sqlx::query("DELETE FROM responses WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn delete_response_chain(pool: &SqlitePool, chain_id: &str) -> DbResult<u64> {
    let res = sqlx::query("DELETE FROM responses WHERE chain_id = ?1")
        .bind(chain_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Conversations, most recently active first.
pub async fn list_response_chains(pool: &SqlitePool, limit: i64) -> DbResult<Vec<ResponseChain>> {
    // The head — the response a client would continue from — is picked with an
    // explicit correlated subquery, not SQLite's bare-column-with-MAX rule:
    // that rule is only defined with a *single* min/max aggregate, and this
    // query has two, so relying on it would silently return the wrong row.
    let rows = sqlx::query(
        "SELECT r.chain_id AS chain_id,
                (SELECT x.id     FROM responses x WHERE x.chain_id = r.chain_id
                 ORDER BY x.created_at DESC, x.rowid DESC LIMIT 1) AS head_id,
                (SELECT x.model  FROM responses x WHERE x.chain_id = r.chain_id
                 ORDER BY x.created_at DESC, x.rowid DESC LIMIT 1) AS model,
                (SELECT x.status FROM responses x WHERE x.chain_id = r.chain_id
                 ORDER BY x.created_at DESC, x.rowid DESC LIMIT 1) AS status,
                COUNT(*)                          AS responses,
                MIN(r.created_at)                 AS first_at,
                MAX(r.created_at)                 AS last_at,
                COALESCE(SUM(r.input_tokens), 0)  AS input_tokens,
                COALESCE(SUM(r.output_tokens), 0) AS output_tokens,
                SUM(LENGTH(r.body) + LENGTH(r.messages)) AS bytes,
                MAX(CASE WHEN r.pending IS NOT NULL THEN 1 ELSE 0 END) AS awaiting
         FROM responses r
         GROUP BY r.chain_id
         ORDER BY last_at DESC
         LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| ResponseChain {
            chain_id: row.get("chain_id"),
            head_id: row.get("head_id"),
            model: row.get("model"),
            status: row.get("status"),
            responses: row.get("responses"),
            first_at: row.get("first_at"),
            last_at: row.get("last_at"),
            input_tokens: row.get("input_tokens"),
            output_tokens: row.get("output_tokens"),
            bytes: row.get("bytes"),
            awaiting_approval: row.get::<i64, _>("awaiting") != 0,
        })
        .collect())
}

/// Every response in one chain, oldest first.
pub async fn list_chain_responses(
    pool: &SqlitePool,
    chain_id: &str,
) -> DbResult<Vec<StoredResponse>> {
    let rows =
        sqlx::query("SELECT * FROM responses WHERE chain_id = ?1 ORDER BY created_at, rowid")
            .bind(chain_id)
            .fetch_all(pool)
            .await?;
    Ok(rows.iter().map(stored_response_from_row).collect())
}

pub async fn count_responses(pool: &SqlitePool) -> DbResult<(i64, i64)> {
    let row =
        sqlx::query("SELECT COUNT(*) AS n, COUNT(DISTINCT chain_id) AS chains FROM responses")
            .fetch_one(pool)
            .await?;
    Ok((row.get("n"), row.get("chains")))
}

/// Chain-aware GC: evict whole conversations, never individual responses.
///
/// Both rules are keyed on the chain's **last** activity. Ageing out single
/// responses would delete each chain's root first — it is by construction the
/// oldest row — and a client continuing from the head would find its history
/// truncated under it. Returns the number of rows deleted.
pub async fn gc_responses(
    pool: &SqlitePool,
    retention_hours: i64,
    max_chains: i64,
) -> DbResult<u64> {
    let mut removed = 0u64;
    if retention_hours > 0 {
        let res = sqlx::query(
            "DELETE FROM responses WHERE chain_id IN (
                 SELECT chain_id FROM responses
                 GROUP BY chain_id
                 HAVING MAX(created_at) < datetime('now', ?1)
             )",
        )
        .bind(format!("-{retention_hours} hours"))
        .execute(pool)
        .await?;
        removed += res.rows_affected();
    }
    if max_chains > 0 {
        let res = sqlx::query(
            "DELETE FROM responses WHERE chain_id IN (
                 SELECT chain_id FROM responses
                 GROUP BY chain_id
                 ORDER BY MAX(created_at) DESC
                 LIMIT -1 OFFSET ?1
             )",
        )
        .bind(max_chains)
        .execute(pool)
        .await?;
        removed += res.rows_affected();
    }
    Ok(removed)
}
