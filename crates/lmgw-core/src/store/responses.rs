//! Stored responses (§21 stage 2) — what makes `previous_response_id` work

use sqlx::{Row, SqlitePool};

use super::*;

mod items;
pub use items::*;

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
    /// The `api_keys.id` of the request that made it; `None` for one made
    /// without a key (and every row from before migration 0091). Who may
    /// read it: [`ResponseReader`].
    pub key_id: Option<i64>,
}

/// Whose stored responses a caller may read (openai egress design §6): the
/// owner every one; anyone else only those made under its own key, a request
/// without a key only those made without one. A response another caller made
/// answers exactly as an unknown id does, so an id is no existence oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseReader {
    Owner,
    /// The caller's `api_keys.id`; `None` for a request without a key.
    Key(Option<i64>),
}

impl ResponseReader {
    /// The two bind values of the reader filter: whether the reader is the
    /// owner, and its key.
    fn binds(self) -> (bool, Option<i64>) {
        match self {
            Self::Owner => (true, None),
            Self::Key(id) => (false, id),
        }
    }
}

// Every query a reader filters spells it `(?2 OR key_id IS ?3)`, its two
// binds from `binds()`: owner, and the key (`IS` matches NULL to NULL).

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
        key_id: row.get("key_id"),
    }
}

pub async fn insert_response(pool: &SqlitePool, r: &StoredResponse) -> DbResult<()> {
    sqlx::query(
        "INSERT INTO responses
           (id, chain_id, previous_response_id, model, status, body, input_items,
            messages, pending, input_tokens, output_tokens, key_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
    .bind(r.key_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The response `id`, when `reader` may read it.
pub async fn get_response(
    pool: &SqlitePool,
    id: &str,
    reader: ResponseReader,
) -> DbResult<Option<StoredResponse>> {
    let (owner, key) = reader.binds();
    let row = sqlx::query("SELECT * FROM responses WHERE id = ?1 AND (?2 OR key_id IS ?3)")
        .bind(id)
        .bind(owner)
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(stored_response_from_row))
}

/// Whether a response with this id is stored that `reader` may read: the
/// lookup that keeps a chain lmgw synthesized in its own loop (openai egress
/// design §6), without reading the conversation it holds.
pub async fn has_response(pool: &SqlitePool, id: &str, reader: ResponseReader) -> DbResult<bool> {
    let (owner, key) = reader.binds();
    let found: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM responses WHERE id = ?1 AND (?2 OR key_id IS ?3)")
            .bind(id)
            .bind(owner)
            .bind(key)
            .fetch_optional(pool)
            .await?;
    Ok(found.is_some())
}

/// Delete one response `reader` may read. Its children are left alone but
/// re-parented in effect — they keep their `chain_id`, so the chain still
/// lists and GCs as one unit.
pub async fn delete_response(
    pool: &SqlitePool,
    id: &str,
    reader: ResponseReader,
) -> DbResult<bool> {
    let (owner, key) = reader.binds();
    let res = sqlx::query("DELETE FROM responses WHERE id = ?1 AND (?2 OR key_id IS ?3)")
        .bind(id)
        .bind(owner)
        .bind(key)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// A response an upstream made natively (migration 0091): where its id is
/// to be fetched again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeResponse {
    /// The upstream's own response id.
    pub id: String,
    pub upstream_id: i64,
    /// As [`StoredResponse::key_id`].
    pub key_id: Option<i64>,
    /// The alias the client asked for.
    pub model: String,
    /// The model the upstream was sent.
    pub upstream_model: String,
    /// Whether the upstream keeps it (`store` not false; migration 0093).
    /// Only a stored one is fetched or continued again; an unstored one is
    /// recorded so that its items are its key's
    /// ([`insert_native_response_items`]).
    pub stored: bool,
}

/// Remember a native response. The first record of an id stays: a later
/// answer carrying the same upstream id (a compatible upstream that reuses
/// ids) must not take it over from the key that made it.
pub async fn insert_native_response(pool: &SqlitePool, r: &NativeResponse) -> DbResult<()> {
    sqlx::query(
        "INSERT INTO native_responses (id, upstream_id, key_id, model, upstream_model, stored)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(id) DO NOTHING",
    )
    .bind(&r.id)
    .bind(r.upstream_id)
    .bind(r.key_id)
    .bind(&r.model)
    .bind(&r.upstream_model)
    .bind(r.stored)
    .execute(pool)
    .await?;
    Ok(())
}

/// The stored native response `id`, when `reader` may read it. An unstored
/// one is not there to fetch or continue.
pub async fn get_native_response(
    pool: &SqlitePool,
    id: &str,
    reader: ResponseReader,
) -> DbResult<Option<NativeResponse>> {
    let (owner, key) = reader.binds();
    let row = sqlx::query(
        "SELECT id, upstream_id, key_id, model, upstream_model FROM native_responses
         WHERE id = ?1 AND stored = 1 AND (?2 OR key_id IS ?3)",
    )
    .bind(id)
    .bind(owner)
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| NativeResponse {
        id: row.get("id"),
        upstream_id: row.get("upstream_id"),
        key_id: row.get("key_id"),
        model: row.get("model"),
        upstream_model: row.get("upstream_model"),
        stored: true,
    }))
}

/// Forget a native response and the item ids recorded under it (its
/// upstream deleted it, or never stored it: a stream cut before its terminal
/// event).
pub async fn delete_native_response(pool: &SqlitePool, id: &str) -> DbResult<()> {
    let mut tx = super::begin_write(pool).await?;
    sqlx::query("DELETE FROM native_response_items WHERE response_id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM native_responses WHERE id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Age eviction of the native response records, by `responses`' retention
/// setting (`0` keeps them all). Returns the rows deleted.
pub async fn gc_native_responses(pool: &SqlitePool, retention_hours: i64) -> DbResult<u64> {
    if retention_hours <= 0 {
        return Ok(0);
    }
    let res = sqlx::query("DELETE FROM native_responses WHERE created_at < datetime('now', ?1)")
        .bind(format!("-{retention_hours} hours"))
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The first id of the list, in its order, that no answer given to the
    /// key carried; another key's items and unknown ids are not the key's.
    #[tokio::test]
    async fn the_first_item_a_key_was_not_given_in_the_list_s_order() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let db = &state.db;
        let up = insert_upstream(
            db,
            &NewUpstream {
                name: "up".into(),
                protocol: crate::config::Protocol::OpenaiCompatible,
                kind: crate::config::UpstreamKind::Generic,
                base_url: "http://127.0.0.1:9/v1".into(),
                api_key: None,
                extra_headers: vec![],
                timeout_ms: 5_000,
                enabled: true,
                expose_all: false,
                expose_prefix: String::new(),
                supports_responses: true,
                catalog_all_modalities: true,
            },
        )
        .await
        .unwrap();
        for (id, key, items) in [("r1", Some(7), ["a", "b"]), ("r2", Some(8), ["c", "d"])] {
            insert_native_response(
                db,
                &NativeResponse {
                    id: id.into(),
                    upstream_id: up,
                    key_id: key,
                    model: "m".into(),
                    upstream_model: "m".into(),
                    stored: true,
                },
            )
            .await
            .unwrap();
            // Each item with its encrypted content `enc-<id>`.
            let items: Vec<crate::ir::UpstreamItem> = items
                .iter()
                .map(|s| crate::ir::UpstreamItem {
                    id: s.to_string(),
                    encrypted_sha256: Some(crate::ir::encrypted_sha256(&format!("enc-{s}"))),
                })
                .collect();
            insert_native_response_items(db, id, key, &items)
                .await
                .unwrap();
        }
        let ids = |v: &[&str]| v.iter().map(|s| ItemClaim::id(*s)).collect::<Vec<_>>();
        let first = |v: Vec<ItemClaim>, key: Option<i64>| async move {
            first_item_not_of(db, &v, key).await.unwrap()
        };
        assert_eq!(first(ids(&[]), Some(7)).await, None);
        assert_eq!(first(ids(&["a", "b"]), Some(7)).await, None);
        assert_eq!(
            first(ids(&["b", "c", "x"]), Some(7)).await.as_deref(),
            Some("c")
        );
        assert_eq!(first(ids(&["x", "c"]), Some(7)).await.as_deref(), Some("x"));
        assert_eq!(first(ids(&["a"]), None).await.as_deref(), Some("a"));
        assert_eq!(first(ids(&["d", "c"]), Some(8)).await, None);
        // A claim of the row and the encrypted content (WP8 review S-R3):
        // both must be as recorded.
        let claim = |row: i64, data: &str| ItemClaim {
            id: "a".into(),
            upstream_id: Some(row),
            encrypted_sha256: Some(crate::ir::encrypted_sha256(data)),
        };
        assert_eq!(first(vec![claim(up, "enc-a")], Some(7)).await, None);
        assert_eq!(
            first(vec![claim(up, "enc-b")], Some(7)).await.as_deref(),
            Some("a"),
            "another item's encrypted content behind the key's own id"
        );
        assert_eq!(
            first(vec![claim(up + 1, "enc-a")], Some(7))
                .await
                .as_deref(),
            Some("a"),
            "a seal naming another row"
        );

        // A later record of an item adds the hash it carries to the ones
        // recorded — OpenAI encrypts an item anew each time it writes it
        // (api-compat design §9, finding 9) — and never touches another
        // answer's or key's item (WP8 verify V-F4); an item recorded without
        // one matches no claimed hash.
        let item = |id: &str, data: Option<&str>| crate::ir::UpstreamItem {
            id: id.into(),
            encrypted_sha256: data.map(crate::ir::encrypted_sha256),
        };
        let hashed = |data: &str| ItemClaim {
            encrypted_sha256: Some(crate::ir::encrypted_sha256(data)),
            ..ItemClaim::id("e")
        };
        insert_native_response_items(db, "r1", Some(7), &[item("e", None)])
            .await
            .unwrap();
        assert_eq!(
            first(vec![hashed("enc-e")], Some(7)).await.as_deref(),
            Some("e"),
            "NULL matches nothing"
        );
        assert_eq!(first(ids(&["e"]), Some(7)).await, None);
        insert_native_response_items(db, "r2", Some(8), &[item("e", Some("enc-x"))])
            .await
            .unwrap();
        insert_native_response_items(db, "r1", Some(7), &[item("e", Some("enc-e"))])
            .await
            .unwrap();
        insert_native_response_items(db, "r1", Some(7), &[item("e", Some("enc-y"))])
            .await
            .unwrap();
        assert_eq!(first(vec![hashed("enc-e")], Some(7)).await, None);
        assert_eq!(first(vec![hashed("enc-y")], Some(7)).await, None);
        assert_eq!(
            first(vec![hashed("enc-x")], Some(7)).await.as_deref(),
            Some("e"),
            "another answer's content is not this item's"
        );
        assert_eq!(
            first(vec![hashed("enc-x")], Some(8)).await.as_deref(),
            Some("e"),
            "nor the other key's"
        );
        assert_eq!(first(ids(&["e"]), Some(8)).await.as_deref(), Some("e"));
        // Forgetting the response takes its items' contents along.
        delete_native_response(db, "r1").await.unwrap();
        let left: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM native_response_item_contents WHERE item_id = 'e'",
        )
        .fetch_one(db)
        .await
        .unwrap();
        assert_eq!(left, 0);
    }
}
