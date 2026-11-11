//! The audit log (headless server design §5.8, migration 0102): one insert
//! per entry (or one more on the row a repeat collapses into, D91), the
//! retention's prune after it in short batches of its own, and the
//! Activity list's pages.

use sqlx::{Row, SqlitePool};

use super::*;

/// How long a row takes the repeats of a collapsing entry (D91): a flood
/// from one peer costs one row a minute.
pub const AUDIT_COLLAPSE_SECS: i64 = 60;

/// Rows one prune transaction removes: a long log pruned at once (keep every
/// entry, then 30 days) holds the write lock for one batch at a time, not
/// for the whole delete (D88). Every old row goes; this only cuts the work.
const PRUNE_BATCH: i64 = 1000;

fn rfc3339(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Write `entry`, `times` occurrences of it. A collapsing entry
/// ([`crate::audit::Entry::collapses`]) adds to the newest row of the same
/// peer, action, account, target and outcome begun within
/// [`AUDIT_COLLAPSE_SECS`], and is a new row otherwise.
pub async fn insert_audit(
    pool: &SqlitePool,
    entry: &crate::audit::Entry,
    times: u64,
) -> DbResult<()> {
    let now = chrono::Utc::now();
    let ts = rfc3339(now);
    let target = entry.target.to_string();
    let times = i64::try_from(times.max(1)).unwrap_or(i64::MAX);
    let mut tx = begin_write(pool).await?;
    if entry.collapses {
        let since = rfc3339(now - chrono::Duration::seconds(AUDIT_COLLAPSE_SECS));
        let added = sqlx::query(
            "UPDATE audit_log SET count = count + ?1, last_ts = ?2 WHERE id = (
                 SELECT id FROM audit_log
                 WHERE ts >= ?3 AND peer = ?4 AND action = ?5 AND account_id IS ?6
                   AND target = ?7 AND outcome = ?8
                 ORDER BY id DESC LIMIT 1)",
        )
        .bind(times)
        .bind(&ts)
        .bind(&since)
        .bind(&entry.peer)
        .bind(&entry.action)
        .bind(entry.account_id)
        .bind(&target)
        .bind(i64::from(entry.outcome))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if added > 0 {
            tx.commit().await?;
            return Ok(());
        }
    }
    sqlx::query(
        "INSERT INTO audit_log
             (ts, account_id, principal, via, peer, action, target, outcome, count, last_ts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )
    .bind(&ts)
    .bind(entry.account_id)
    .bind(&entry.principal)
    .bind(entry.via.as_str())
    .bind(&entry.peer)
    .bind(&entry.action)
    .bind(&target)
    .bind(i64::from(entry.outcome))
    .bind(times)
    .bind((times > 1).then_some(&ts))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Remove the entries `keep_days` no longer keeps (0 keeps every entry), a
/// batch per write transaction, after the entry that asked was committed:
/// the entry that changed the retention is written before the retention it
/// set prunes (ruling 18), and its own time is never older than a day. A
/// read first, so a log with nothing to remove costs no write.
pub async fn prune_audit(pool: &SqlitePool, keep_days: u32) -> DbResult<u64> {
    if keep_days == 0 {
        return Ok(0);
    }
    let cutoff = rfc3339(chrono::Utc::now() - chrono::Duration::days(i64::from(keep_days)));
    let old: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_log WHERE ts < ?1)")
        .bind(&cutoff)
        .fetch_one(pool)
        .await?;
    if !old {
        return Ok(0);
    }
    let mut removed = 0;
    loop {
        let mut tx = begin_write(pool).await?;
        let n = sqlx::query(
            "DELETE FROM audit_log WHERE id IN
                 (SELECT id FROM audit_log WHERE ts < ?1 ORDER BY ts LIMIT ?2)",
        )
        .bind(&cutoff)
        .bind(PRUNE_BATCH)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        removed += n;
        if n < PRUNE_BATCH as u64 {
            return Ok(removed);
        }
    }
}

/// Whose entries a page holds (`audit_list`'s `account`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditWho {
    Everyone,
    /// The entries that acted as this account.
    Account(i64),
    /// The entries of no account: the offline command line, a failed
    /// sign-in.
    NoAccount,
}

/// Up to `count` entries of `who` older than entry `before` (every entry
/// when `None`), newest first, and whether there are more after them. Each
/// filter is a statement of its own, so `before` is a rowid range and the
/// account filter reads its index.
pub async fn audit_page(
    pool: &SqlitePool,
    who: AuditWho,
    before: Option<i64>,
    count: u32,
) -> DbResult<(Vec<lmgw_api_types::AuditEntry>, bool)> {
    const ALL: &str = "SELECT * FROM audit_log ORDER BY id DESC LIMIT ?";
    const ALL_BEFORE: &str = "SELECT * FROM audit_log WHERE id < ? ORDER BY id DESC LIMIT ?";
    const ONE: &str = "SELECT * FROM audit_log WHERE account_id = ? ORDER BY id DESC LIMIT ?";
    const ONE_BEFORE: &str =
        "SELECT * FROM audit_log WHERE id < ? AND account_id = ? ORDER BY id DESC LIMIT ?";
    const NONE: &str = "SELECT * FROM audit_log WHERE account_id IS NULL ORDER BY id DESC LIMIT ?";
    const NONE_BEFORE: &str =
        "SELECT * FROM audit_log WHERE id < ? AND account_id IS NULL ORDER BY id DESC LIMIT ?";
    let sql = match (who, before.is_some()) {
        (AuditWho::Everyone, false) => ALL,
        (AuditWho::Everyone, true) => ALL_BEFORE,
        (AuditWho::Account(_), false) => ONE,
        (AuditWho::Account(_), true) => ONE_BEFORE,
        (AuditWho::NoAccount, false) => NONE,
        (AuditWho::NoAccount, true) => NONE_BEFORE,
    };
    let mut q = sqlx::query(sql);
    if let Some(b) = before {
        q = q.bind(b);
    }
    if let AuditWho::Account(id) = who {
        q = q.bind(id);
    }
    let rows = q.bind(i64::from(count) + 1).fetch_all(pool).await?;
    let more = rows.len() > count as usize;
    let entries = rows
        .iter()
        .take(count as usize)
        .map(|r| lmgw_api_types::AuditEntry {
            id: r.get("id"),
            ts: r.get("ts"),
            account_id: r.get("account_id"),
            principal: r.get("principal"),
            via: r.get("via"),
            peer: r.get("peer"),
            action: r.get("action"),
            target: serde_json::from_str(&r.get::<String, _>("target"))
                .unwrap_or(serde_json::Value::Null),
            outcome: u16::try_from(r.get::<i64, _>("outcome")).unwrap_or_default(),
            count: u64::try_from(r.get::<i64, _>("count")).unwrap_or(1),
            last_ts: r.get("last_ts"),
            ..Default::default()
        })
        .collect();
    Ok((entries, more))
}

/// The whole log: its rows, the bytes its table and indexes take in the
/// database file (SQLite's `dbstat`), and its oldest entry's time. Scans
/// the log: `audit_list` asks, nothing on a hot path does.
pub async fn audit_stats(pool: &SqlitePool) -> DbResult<(i64, u64, Option<String>)> {
    let (count, oldest): (i64, Option<String>) =
        sqlx::query_as("SELECT COUNT(*), MIN(ts) FROM audit_log")
            .fetch_one(pool)
            .await?;
    let bytes: Option<i64> = sqlx::query_scalar(
        "SELECT SUM(pgsize) FROM dbstat
         WHERE name IN ('audit_log', 'audit_log_ts', 'audit_log_account')",
    )
    .fetch_one(pool)
    .await?;
    Ok((count, bytes.unwrap_or(0).max(0) as u64, oldest))
}

/// Whether the log holds any entry: one index step, for the kept-line.
pub async fn audit_any(pool: &SqlitePool) -> DbResult<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_log)")
        .fetch_one(pool)
        .await?)
}
