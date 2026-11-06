//! The output items recorded per native answer (migrations 0093, 0096,
//! 0100) and the check of what a client key names against them (openai
//! egress design §6.1, §7.3.2; WP8 review S-R3).

use serde::Serialize;
use sqlx::{Row, SqlitePool};

use crate::ir::UpstreamItem;

use super::super::DbResult;

/// Record the output items of the native response `response_id` (already
/// recorded by [`super::insert_native_response`]) under `key_id`, each with
/// the hash of its encrypted content where it has one. The first record of
/// an item id stays, as a response's does; a later record of the same item
/// (the same answer and key: a stream's item `done`, then its terminal
/// event, WP8 verify V-F4) adds the hash of the encrypted content it
/// carries to the ones recorded (migration 0100): OpenAI encrypts an item
/// anew each time it writes it, so one answer carries several. An item
/// recorded without one matches no claimed hash ([`first_item_not_of`]).
pub async fn insert_native_response_items(
    pool: &SqlitePool,
    response_id: &str,
    key_id: Option<i64>,
    items: &[UpstreamItem],
) -> DbResult<()> {
    if items.is_empty() {
        return Ok(());
    }
    let mut tx = super::super::begin_write(pool).await?;
    for item in items.iter().filter(|i| !i.id.is_empty()) {
        sqlx::query(
            "INSERT INTO native_response_items (item_id, response_id, key_id)
             VALUES (?1, ?2, ?3) ON CONFLICT(item_id) DO NOTHING",
        )
        .bind(&item.id)
        .bind(response_id)
        .bind(key_id)
        .execute(&mut *tx)
        .await?;
        let Some(hash) = &item.encrypted_sha256 else {
            continue;
        };
        // Only to the record of this answer and key: an id another answer
        // recorded first is not this one's to add to.
        sqlx::query(
            "INSERT INTO native_response_item_contents (item_id, encrypted_sha256)
             SELECT item_id, ?4 FROM native_response_items
             WHERE item_id = ?1 AND response_id = ?2 AND key_id IS ?3
             ON CONFLICT DO NOTHING",
        )
        .bind(&item.id)
        .bind(response_id)
        .bind(key_id)
        .bind(hash)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Which of `item_ids` an answer given to `reader` carried: the owner's,
/// any recorded item; a key's, its own (the anonymous principal's, those
/// recorded under none).
pub async fn recorded_item_ids(
    pool: &SqlitePool,
    item_ids: &[String],
    reader: super::ResponseReader,
) -> DbResult<std::collections::HashSet<String>> {
    if item_ids.is_empty() {
        return Ok(Default::default());
    }
    let (owner, key) = reader.binds();
    let ids = serde_json::to_string(item_ids).expect("a list of strings serializes");
    let rows = sqlx::query(
        "SELECT n.item_id FROM native_response_items AS n \
         WHERE n.item_id IN (SELECT value FROM json_each(?1)) AND (?2 OR n.key_id IS ?3)",
    )
    .bind(ids)
    .bind(owner)
    .bind(key)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(|r| r.get::<String, _>("item_id")).collect())
}

/// One item a request names, and what it claims of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ItemClaim {
    /// The item's id.
    pub id: String,
    /// The upstream row that answered with it, where the request says so
    /// (a seal names its row): the recorded answer must be that row's.
    pub upstream_id: Option<i64>,
    /// The hash of the encrypted content the request sends with it
    /// ([`crate::ir::encrypted_sha256`]): the recorded item must have been
    /// answered with exactly that, in one of the forms it was.
    pub encrypted_sha256: Option<String>,
}

impl ItemClaim {
    /// An item named by its id alone.
    pub fn id(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            upstream_id: None,
            encrypted_sha256: None,
        }
    }
}

/// The id of the first of `claims` that no native answer given to `key_id`
/// carried as claimed: an item the key was not given, from another row than
/// the one a seal names, or with other encrypted content than the request
/// sends. What a client key may not send to the upstream. One query for the
/// whole list, which `json_each` walks in its order.
pub async fn first_item_not_of(
    pool: &SqlitePool,
    claims: &[ItemClaim],
    key_id: Option<i64>,
) -> DbResult<Option<String>> {
    if claims.is_empty() {
        return Ok(None);
    }
    let list = serde_json::to_string(claims).expect("a list of claims serializes");
    let row = sqlx::query(
        "SELECT json_extract(j.value, '$.id') AS id FROM json_each(?1) AS j \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM native_response_items AS n \
             JOIN native_responses AS r ON r.id = n.response_id \
             WHERE n.item_id = json_extract(j.value, '$.id') AND n.key_id IS ?2 \
               AND (json_extract(j.value, '$.upstream_id') IS NULL \
                    OR r.upstream_id = json_extract(j.value, '$.upstream_id')) \
               AND (json_extract(j.value, '$.encrypted_sha256') IS NULL \
                    OR EXISTS (SELECT 1 FROM native_response_item_contents AS c \
                               WHERE c.item_id = n.item_id \
                                 AND c.encrypted_sha256 = json_extract(j.value, '$.encrypted_sha256')))) \
         ORDER BY j.key LIMIT 1",
    )
    .bind(list)
    .bind(key_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.get::<String, _>("id")))
}
