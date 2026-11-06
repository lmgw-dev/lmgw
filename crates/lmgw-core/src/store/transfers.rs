//! The `transfers` table (device transfers design §3, §9, migration 0088):
//! one row per transfer between devices. Every state change is one write
//! transaction that holds the write lock from its first statement
//! (`begin_write`), conditional on the state it changes from, and records
//! its `transfer.updated` in the same transaction (§6). The files are
//! `crate::transfers`'; nothing here touches the disk.
//!
//! Times are RFC 3339 UTC to the second with `Z` (`stamp`), so the columns
//! compare as text in the order they compare as times.

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};

use super::feed::{record_transfer, TransferFacts};
use super::{stamp, DbResult};

/// The states a transfer is in progress in (§1): lmgw holds its bytes. A
/// macro, so every query stays one literal (`concat!`).
macro_rules! active {
    () => {
        "state IN ('uploading', 'ready')"
    };
}

/// One transfer, as stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferRow {
    pub id: String,
    pub app_id: String,
    pub kind: String,
    pub title: String,
    pub filename: String,
    pub size: i64,
    pub sha256: String,
    pub from_key_id: i64,
    /// The sender's paired name, without `device:`, at the offer.
    pub from_name: String,
    pub to_key_id: i64,
    pub to_name: String,
    /// `uploading` … `failed` (the table's CHECK).
    pub state: String,
    /// The file's length as of the last upload; how far an ended one got.
    pub received: i64,
    pub created_at: String,
    pub expires_at: String,
    /// When it became `ready`: the receiver sees it from then on.
    pub ready_at: Option<String>,
    pub ended_at: Option<String>,
    pub end_reason: Option<String>,
}

impl TransferRow {
    /// Whether it is `uploading` or `ready`.
    pub fn is_active(&self) -> bool {
        self.state == "uploading" || self.state == "ready"
    }

    /// Whether its receiver was offered it (it was `ready` once).
    pub fn offered(&self) -> bool {
        self.ready_at.is_some()
    }

    fn facts(&self) -> TransferFacts {
        TransferFacts {
            id: self.id.clone(),
            from: self.from_key_id,
            to: self.to_key_id,
            offered: self.offered(),
        }
    }
}

fn from_row(r: &sqlx::sqlite::SqliteRow) -> TransferRow {
    TransferRow {
        id: r.get("id"),
        app_id: r.get("app_id"),
        kind: r.get("kind"),
        title: r.get("title"),
        filename: r.get("filename"),
        size: r.get("size"),
        sha256: r.get("sha256"),
        from_key_id: r.get("from_key_id"),
        from_name: r.get("from_name"),
        to_key_id: r.get("to_key_id"),
        to_name: r.get("to_name"),
        state: r.get("state"),
        received: r.get("received"),
        created_at: r.get("created_at"),
        expires_at: r.get("expires_at"),
        ready_at: r.get("ready_at"),
        ended_at: r.get("ended_at"),
        end_reason: r.get("end_reason"),
    }
}

/// An offer to write (§9.2), checked by the route already.
#[derive(Debug, Clone)]
pub struct NewTransfer<'a> {
    pub id: &'a str,
    pub app_id: &'a str,
    pub kind: &'a str,
    pub title: &'a str,
    pub filename: &'a str,
    pub size: i64,
    pub sha256: &'a str,
    pub from_key_id: i64,
    pub from_name: &'a str,
    pub to_key_id: i64,
    pub to_name: &'a str,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// What an offer's write came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offered {
    Made(Box<TransferRow>),
    /// The sender's row went since the gate admitted it (I29).
    SenderGone,
    /// The sender was disabled since the gate admitted it, its name.
    SenderDisabled(String),
    /// The rows say otherwise than the snapshot the route checked about the
    /// receiver: why, in words (it went, was disabled or declared another
    /// app id).
    Refused(String),
}

/// Write offer `t` (`uploading`, nothing received), its two devices read
/// again in the transaction, so an offer never outlives a Delete or a
/// Disable that committed first (§8); with its `transfer.updated`.
pub async fn insert(pool: &SqlitePool, t: &NewTransfer<'_>) -> DbResult<Offered> {
    let mut tx = super::begin_write(pool).await?;
    let device = |id: i64| {
        sqlx::query("SELECT name, enabled, app_id FROM api_keys WHERE id = ?1 AND kind = 'device'")
            .bind(id)
    };
    let Some(from) = device(t.from_key_id).fetch_optional(&mut *tx).await? else {
        return Ok(Offered::SenderGone);
    };
    if from.get::<i64, _>("enabled") == 0 {
        return Ok(Offered::SenderDisabled(from.get("name")));
    }
    let Some(to) = device(t.to_key_id).fetch_optional(&mut *tx).await? else {
        return Ok(Offered::Refused(format!(
            "device {} is no device of this lmgw",
            t.to_key_id
        )));
    };
    if to.get::<i64, _>("enabled") == 0 {
        return Ok(Offered::Refused(format!(
            "device '{}' is disabled",
            t.to_name
        )));
    }
    if to.get::<Option<String>, _>("app_id").as_deref() != Some(t.app_id) {
        return Ok(Offered::Refused(format!(
            "device '{}' did not declare the app id {}",
            t.to_name, t.app_id
        )));
    }
    let row = sqlx::query(
        "INSERT INTO transfers (id, app_id, kind, title, filename, size, sha256, from_key_id,
                                from_name, to_key_id, to_name, state, received, created_at,
                                expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'uploading', 0, ?12, ?13)
         RETURNING *",
    )
    .bind(t.id)
    .bind(t.app_id)
    .bind(t.kind)
    .bind(t.title)
    .bind(t.filename)
    .bind(t.size)
    .bind(t.sha256)
    .bind(t.from_key_id)
    .bind(t.from_name)
    .bind(t.to_key_id)
    .bind(t.to_name)
    .bind(stamp(t.created_at))
    .bind(stamp(t.expires_at))
    .fetch_one(&mut *tx)
    .await?;
    let row = from_row(&row);
    record_transfer(&mut tx, &row.facts()).await?;
    tx.commit().await?;
    Ok(Offered::Made(Box::new(row)))
}

/// Transfer `id`, when there is one.
pub async fn get(pool: &SqlitePool, id: &str) -> DbResult<Option<TransferRow>> {
    let row = sqlx::query("SELECT * FROM transfers WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(from_row))
}

/// Whose transfers a list reads (§9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listed {
    /// The owner's: every one.
    All,
    /// Device `key_id`'s outgoing ones.
    Outgoing(i64),
    /// Device `key_id`'s incoming ones it sees: from `ready` on.
    Incoming(i64),
    /// Both of device `key_id`'s.
    Both(i64),
}

/// The transfers `which` names, newest first.
pub async fn list(pool: &SqlitePool, which: Listed) -> DbResult<Vec<TransferRow>> {
    // `?1` is the device's key id; the owner's list names none, and matches
    // every row with it unbound (NULL).
    macro_rules! listed {
        ($filter:literal) => {
            concat!(
                "SELECT * FROM transfers WHERE ",
                $filter,
                " ORDER BY created_at DESC, id"
            )
        };
    }
    let (sql, key) = match which {
        Listed::All => (listed!("?1 IS NULL"), None),
        Listed::Outgoing(k) => (listed!("from_key_id = ?1"), Some(k)),
        Listed::Incoming(k) => (listed!("to_key_id = ?1 AND ready_at IS NOT NULL"), Some(k)),
        Listed::Both(k) => (
            listed!("(from_key_id = ?1 OR (to_key_id = ?1 AND ready_at IS NOT NULL))"),
            Some(k),
        ),
    };
    let rows = sqlx::query(sql).bind(key).fetch_all(pool).await?;
    Ok(rows.iter().map(from_row).collect())
}

/// Every transfer in progress (`uploading`, `ready`).
pub async fn active(pool: &SqlitePool) -> DbResult<Vec<TransferRow>> {
    let rows = sqlx::query(concat!(
        "SELECT * FROM transfers WHERE ",
        active!(),
        " ORDER BY created_at"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(from_row).collect())
}

/// An upload wrote: its file is `received` bytes long now. Only while it
/// uploads; no feed record (I12).
pub async fn set_received(pool: &SqlitePool, id: &str, received: i64) -> DbResult<()> {
    let mut tx = super::begin_write(pool).await?;
    sqlx::query("UPDATE transfers SET received = ?2 WHERE id = ?1 AND state = 'uploading'")
        .bind(id)
        .bind(received)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Every byte arrived and the sha256 matched: `uploading` → `ready`, at
/// `now`, offered to the receiver. `None` when it was not uploading.
pub async fn mark_ready(
    pool: &SqlitePool,
    id: &str,
    now: DateTime<Utc>,
) -> DbResult<Option<TransferRow>> {
    let mut tx = super::begin_write(pool).await?;
    let row = sqlx::query(
        "UPDATE transfers SET state = 'ready', received = size, ready_at = ?2
          WHERE id = ?1 AND state = 'uploading'
         RETURNING *",
    )
    .bind(id)
    .bind(stamp(now))
    .fetch_optional(&mut *tx)
    .await?;
    finish(tx, row).await
}

/// The states an end may come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndsFrom {
    /// `uploading` or `ready`: a cancel, an expiry, a device that ended.
    Active,
    /// `ready`: the receiver's `received` and `decline`.
    Ready,
    /// `uploading`: a hash that did not match.
    Uploading,
}

impl EndsFrom {
    /// The end's statement, conditional on these states.
    fn sql(self) -> &'static str {
        macro_rules! ending {
            ($states:expr) => {
                concat!(
                    "UPDATE transfers SET state = ?2, end_reason = ?3,
                            received = COALESCE(?4, received), ended_at = ?5
                      WHERE id = ?1 AND ",
                    $states,
                    " RETURNING *"
                )
            };
        }
        match self {
            Self::Active => ending!(active!()),
            Self::Ready => ending!("state = 'ready'"),
            Self::Uploading => ending!("state = 'uploading'"),
        }
    }
}

/// How a transfer ends.
#[derive(Debug, Clone)]
pub struct Ending<'a> {
    /// `delivered`, `declined`, `canceled`, `expired` or `failed`.
    pub state: &'a str,
    /// Why, in words; `None` for `delivered`.
    pub reason: Option<&'a str>,
    /// The bytes it held when it ended, when the caller knows them (the
    /// file's length); `None` keeps the column's.
    pub received: Option<i64>,
    pub now: DateTime<Utc>,
}

/// End transfer `id` as `e` says, from one of `from`'s states, with its
/// `transfer.updated`. `None` when it was in none of them (ended already,
/// or never there).
pub async fn end(
    pool: &SqlitePool,
    id: &str,
    from: EndsFrom,
    e: &Ending<'_>,
) -> DbResult<Option<TransferRow>> {
    let mut tx = super::begin_write(pool).await?;
    let row = sqlx::query(from.sql())
        .bind(id)
        .bind(e.state)
        .bind(e.reason)
        .bind(e.received)
        .bind(stamp(e.now))
        .fetch_optional(&mut *tx)
        .await?;
    finish(tx, row).await
}

/// Record the changed row's `transfer.updated` and commit; `None` when no
/// row changed.
async fn finish(
    mut tx: sqlx::Transaction<'static, sqlx::Sqlite>,
    row: Option<sqlx::sqlite::SqliteRow>,
) -> DbResult<Option<TransferRow>> {
    let Some(row) = row.as_ref().map(from_row) else {
        return Ok(None);
    };
    record_transfer(&mut tx, &row.facts()).await?;
    tx.commit().await?;
    Ok(Some(row))
}

/// The transfers in progress that device `key_id` sends or receives.
pub async fn of_device(pool: &SqlitePool, key_id: i64) -> DbResult<Vec<String>> {
    Ok(sqlx::query_scalar(concat!(
        "SELECT id FROM transfers WHERE (from_key_id = ?1 OR to_key_id = ?1) AND ",
        active!(),
        " ORDER BY created_at"
    ))
    .bind(key_id)
    .fetch_all(pool)
    .await?)
}

/// The transfers in progress past their `expires_at` at `now`.
pub async fn overdue(pool: &SqlitePool, now: DateTime<Utc>) -> DbResult<Vec<String>> {
    Ok(sqlx::query_scalar(concat!(
        "SELECT id FROM transfers WHERE ",
        active!(),
        " AND expires_at <= ?1 ORDER BY expires_at"
    ))
    .bind(stamp(now))
    .fetch_all(pool)
    .await?)
}

/// The transfers in progress with a device whose row is gone, or not an
/// enabled device (§8's backstop): each with that device's key id and
/// whether it was deleted (`true`) or disabled.
pub async fn orphaned(pool: &SqlitePool) -> DbResult<Vec<(String, i64, bool)>> {
    let rows = sqlx::query(concat!(
        "SELECT t.id AS id, p.key_id AS key_id, k.id IS NULL AS gone
           FROM transfers t
           JOIN (SELECT id, from_key_id AS key_id FROM transfers
                 UNION ALL SELECT id, to_key_id FROM transfers) p ON p.id = t.id
           LEFT JOIN api_keys k ON k.id = p.key_id AND k.kind = 'device'
          WHERE t.",
        active!(),
        " AND (k.id IS NULL OR k.enabled = 0)
          ORDER BY t.created_at"
    ))
    .fetch_all(pool)
    .await?;
    let mut out: Vec<(String, i64, bool)> = Vec::new();
    for r in &rows {
        let id: String = r.get("id");
        // A transfer named once, by the first party that ended it.
        if out.iter().all(|(seen, _, _)| *seen != id) {
            out.push((id, r.get("key_id"), r.get::<i64, _>("gone") != 0));
        }
    }
    Ok(out)
}

/// The transfers in progress offered to a device (`key_id`; every device
/// with `None`) whose app id is not the one the device declares now (I28):
/// each with the device's app id now (`None`: it declares none).
pub async fn mismatched(
    pool: &SqlitePool,
    key_id: Option<i64>,
) -> DbResult<Vec<(String, Option<String>)>> {
    let rows = sqlx::query(concat!(
        "SELECT t.id AS id, k.app_id AS now_app_id
           FROM transfers t
           JOIN api_keys k ON k.id = t.to_key_id AND k.kind = 'device'
          WHERE t.",
        active!(),
        " AND (?1 IS NULL OR t.to_key_id = ?1)
            AND (k.app_id IS NULL OR k.app_id != t.app_id)
          ORDER BY t.created_at"
    ))
    .bind(key_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get("id"), r.get("now_app_id")))
        .collect())
}

/// Delete the ended transfers older than `days` by their `ended_at` (`0`
/// keeps every one, §7). How many went.
pub async fn prune(pool: &SqlitePool, days: i64, now: DateTime<Utc>) -> DbResult<u64> {
    if days <= 0 {
        return Ok(0);
    }
    let before = now - chrono::Duration::days(days);
    let mut tx = super::begin_write(pool).await?;
    let gone = sqlx::query("DELETE FROM transfers WHERE ended_at IS NOT NULL AND ended_at < ?1")
        .bind(stamp(before))
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(gone)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool_with_devices() -> SqlitePool {
        let pool = crate::store::open_in_memory().await.unwrap();
        for (id, name, app) in [(10, "device:desktop", "a.b"), (11, "device:phone", "a.b")] {
            sqlx::query(
                "INSERT INTO api_keys (id, name, key_hash, enabled, kind, app_id)
                 VALUES (?1, ?2, ?3, 1, 'device', ?4)",
            )
            .bind(id)
            .bind(name)
            .bind(format!("spki:{id}"))
            .bind(app)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    fn offer(id: &str) -> NewTransfer<'_> {
        let now = Utc::now();
        NewTransfer {
            id,
            app_id: "a.b",
            kind: "save",
            title: "",
            filename: "f.bin",
            size: 3,
            sha256: "00",
            from_key_id: 10,
            from_name: "desktop",
            to_key_id: 11,
            to_name: "phone",
            created_at: now,
            expires_at: now + chrono::Duration::days(1),
        }
    }

    /// Every change records its event in its own transaction, and an end
    /// from a state it is not in changes nothing.
    #[tokio::test]
    async fn every_change_is_conditional_and_records_its_event() {
        let pool = pool_with_devices().await;
        let Offered::Made(t) = insert(&pool, &offer("t1")).await.unwrap() else {
            panic!("refused")
        };
        assert_eq!((t.state.as_str(), t.received), ("uploading", 0));
        let now = Utc::now();
        let delivered = Ending {
            state: "delivered",
            reason: None,
            received: None,
            now,
        };
        assert!(end(&pool, "t1", EndsFrom::Ready, &delivered)
            .await
            .unwrap()
            .is_none());
        let ready = mark_ready(&pool, "t1", now).await.unwrap().unwrap();
        assert_eq!((ready.state.as_str(), ready.received), ("ready", 3));
        assert!(mark_ready(&pool, "t1", now).await.unwrap().is_none());
        let done = end(&pool, "t1", EndsFrom::Ready, &delivered)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.state, "delivered");
        let facts: Vec<_> = crate::store::feed::page(&pool, 0, 10)
            .await
            .unwrap()
            .iter()
            .filter_map(|r| r.transfer())
            .map(|f| f.offered)
            .collect();
        assert_eq!(facts, [false, true, true]);
        // Listed: the receiver sees it once offered; the owner always.
        assert_eq!(list(&pool, Listed::Incoming(11)).await.unwrap().len(), 1);
        assert_eq!(list(&pool, Listed::All).await.unwrap().len(), 1);
        assert!(list(&pool, Listed::Outgoing(11)).await.unwrap().is_empty());
    }

    /// The rows decide over the snapshot: a receiver that declared another
    /// app id, or was disabled, refuses the offer in its transaction.
    #[tokio::test]
    async fn the_offer_reads_its_devices_again() {
        let pool = pool_with_devices().await;
        sqlx::query("UPDATE api_keys SET app_id = 'c.d' WHERE id = 11")
            .execute(&pool)
            .await
            .unwrap();
        let r = insert(&pool, &offer("t2")).await.unwrap();
        assert!(
            matches!(&r, Offered::Refused(w) if w.contains("app id")),
            "{r:?}"
        );
        assert!(get(&pool, "t2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_party_that_went_orphans_its_transfers_and_ended_rows_are_pruned() {
        let pool = pool_with_devices().await;
        insert(&pool, &offer("t3")).await.unwrap();
        assert!(orphaned(&pool).await.unwrap().is_empty());
        sqlx::query("DELETE FROM api_keys WHERE id = 11")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            orphaned(&pool).await.unwrap(),
            [("t3".to_string(), 11, true)]
        );
        let now = Utc::now();
        end(
            &pool,
            "t3",
            EndsFrom::Active,
            &Ending {
                state: "canceled",
                reason: Some("x"),
                received: Some(1),
                now: now - chrono::Duration::days(9),
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(prune(&pool, 0, now).await.unwrap(), 0, "0 keeps every row");
        assert_eq!(prune(&pool, 7, now).await.unwrap(), 1);
    }

    /// A receiver that declares another app id, or none, leaves its
    /// incoming transfers mismatched; the sender's app id is not read.
    #[tokio::test]
    async fn a_receiver_of_another_app_id_mismatches_its_transfers() {
        let pool = pool_with_devices().await;
        insert(&pool, &offer("t4")).await.unwrap();
        assert!(mismatched(&pool, None).await.unwrap().is_empty());
        sqlx::query("UPDATE api_keys SET app_id = NULL WHERE id = 10")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            mismatched(&pool, None).await.unwrap().is_empty(),
            "the sender's"
        );
        for (app, want) in [(Some("c.d"), Some("c.d")), (None, None)] {
            sqlx::query("UPDATE api_keys SET app_id = ?1 WHERE id = 11")
                .bind(app)
                .execute(&pool)
                .await
                .unwrap();
            let want = vec![("t4".to_string(), want.map(str::to_string))];
            assert_eq!(mismatched(&pool, Some(11)).await.unwrap(), want);
            assert_eq!(mismatched(&pool, None).await.unwrap(), want);
            assert!(mismatched(&pool, Some(10)).await.unwrap().is_empty());
        }
    }
}
