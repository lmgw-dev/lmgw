//! The Chat change feed's table (client-apps design §2.3, L6): one change
//! record per write a client should hear of, written **inside the write's
//! own transaction** by [`record`] and its helpers, so a write that fails
//! records nothing and `seq` order is commit order.
//!
//! A record is a fact, not a rendering: which thread or folder, what kind of
//! change, who made it. `web::chat_feed` renders the thread or folder as it
//! is when it delivers the record, or a tombstone for one that is gone.
//!
//! **Who** is a principal's description (§1.7): "the dashboard" for the
//! owner, "device 'phone'" for a device; `None` for the gateway's own work
//! (the hourly sweep, a seed drawn on first use).
//!
//! The cursor a client resumes from is `"<epoch>:<seq>:<tag>"`: the epoch is
//! minted once per database by migration 0063, [`bounds`] says which `seq`
//! values the table can still answer for, and the tag ([`Record::tag`],
//! drawn at random when the record is written, migration 0065) checks that
//! the record at `seq` is the one the cursor was handed out for (reviews
//! W5-4, W6-3).

use sqlx::{Row, SqliteConnection, SqlitePool};

use super::DbResult;

/// Who made a change, as the feed names them; `None` is the gateway itself.
pub type By<'a> = Option<&'a str>;

/// How the owner is named as the author of a change: the dashboard and every
/// owner key (`web::chat_caller::Caller::named`). Shared with clients, which
/// tell their own changes by it (`FeedPrincipal::by`).
pub const BY_OWNER: &str = lmgw_api_types::chat_feed::BY_ADMIN;

/// The record types this build writes (§2.2). `message.*` are written once
/// the message writes record them (WP10); `approval.*` with approvals (WP9).
pub mod kind {
    pub const THREAD_CREATED: &str = "thread.created";
    pub const THREAD_UPDATED: &str = "thread.updated";
    pub const THREAD_DELETED: &str = "thread.deleted";
    pub const FOLDER_CREATED: &str = "folder.created";
    pub const FOLDER_UPDATED: &str = "folder.updated";
    pub const FOLDER_DELETED: &str = "folder.deleted";
    /// An ongoing folder's current thread moved (§3). Recorded by
    /// [`record_folder_current`](super::feed::record_folder_current), in
    /// the transaction of every write that moves it
    /// (`store::chat_ongoing`).
    pub const FOLDER_CURRENT: &str = "folder.current";
    /// A device's level of lmgw's admin tools changed
    /// ([`record_device_reach`]): that device's own stream hears the
    /// threads and folders with the self-admin toolset come or go, and its
    /// level; no other reader hears anything.
    pub const DEVICE_REACH: &str = "device.reach";
    /// The gateway's self-admin level changed ([`record_gateway_reach`],
    /// 2026-10-07): it caps every device's own, so each device's stream
    /// hears the toolset's threads and folders come or go where what its
    /// admin tools may do moved to or from `off`, and the level where it
    /// moved at all. No reader renders it.
    pub const GATEWAY_REACH: &str = "gateway.reach";
}

/// One change, as [`record`] writes it.
#[derive(Debug, Clone, Default)]
pub struct Change<'a> {
    pub kind: &'a str,
    pub thread_id: Option<i64>,
    pub folder_id: Option<i64>,
    /// How far into the self-admin plane the thread or folder reached at
    /// the write (`self_admin_thread!`'s level; for `folder.current`: the
    /// folder's or its new thread's, whichever is higher) — a reader that
    /// does not see that level never hears of it (L3). It can change (review
    /// W4-12): a write that changes it stores `admin_was` in `detail`, and a
    /// device's rendering re-checks the state as it is now.
    pub admin: i64,
    pub by: By<'a>,
    /// An event's facts that are not state, as JSON: `folder.current`'s
    /// previous thread and reason, a flip's `admin_was`.
    pub detail: Option<String>,
}

/// Write one change record on `conn` — the transaction of the write it
/// records.
pub async fn record(conn: &mut SqliteConnection, c: Change<'_>) -> DbResult<()> {
    sqlx::query(
        "INSERT INTO chat_feed (type, thread_id, folder_id, admin, by, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(c.kind)
    .bind(c.thread_id)
    .bind(c.folder_id)
    .bind(c.admin)
    .bind(c.by)
    .bind(c.detail)
    .execute(conn)
    .await?;
    Ok(())
}

/// Record `kind` for thread `thread_id` as the row stands in this
/// transaction: its folder and its level (L3) are read from it. A thread that is not there records nothing — an
/// update that matched no row changed nothing a client could hear of.
pub async fn record_thread(
    conn: &mut SqliteConnection,
    kind: &str,
    thread_id: i64,
    by: By<'_>,
) -> DbResult<()> {
    record_thread_since(conn, kind, thread_id, by, None).await
}

/// Thread `thread_id`'s level now (`self_admin_thread!`), in this
/// transaction (`None`: no such thread) — read before a write that may
/// change it, for [`record_thread_since`].
pub async fn self_admin_now(conn: &mut SqliteConnection, thread_id: i64) -> DbResult<Option<i64>> {
    Ok(sqlx::query_scalar(concat!(
        "SELECT ",
        self_admin_thread!(""),
        " FROM chat_threads WHERE id = ?1"
    ))
    .bind(thread_id)
    .fetch_optional(conn)
    .await?)
}

/// [`record_thread`] after a write that may have attached or removed the
/// self-admin toolset: `was` is the level [`self_admin_now`] said before
/// it. A write that changed it says so in the record's `detail`
/// (`admin_was`), so a device hears the thread go — or come — rather than
/// nothing (review W3-1).
pub async fn record_thread_since(
    conn: &mut SqliteConnection,
    kind: &str,
    thread_id: i64,
    by: By<'_>,
    was: Option<i64>,
) -> DbResult<()> {
    sqlx::query(concat!(
        "INSERT INTO chat_feed (type, thread_id, folder_id, admin, by, detail)
         SELECT ?1, id, folder_id, a, ?2,
                CASE WHEN ?4 IS NOT NULL AND a != ?4 THEN json_object('admin_was', ?4) END
         FROM (SELECT id, folder_id, ",
        self_admin_thread!(""),
        " AS a FROM chat_threads WHERE id = ?3)"
    ))
    .bind(kind)
    .bind(by)
    .bind(thread_id)
    .bind(was)
    .execute(conn)
    .await?;
    Ok(())
}

/// Record [`kind::THREAD_UPDATED`] for each of `ids` (a bulk update's
/// `RETURNING id`).
pub async fn record_threads_updated(
    conn: &mut SqliteConnection,
    ids: &[i64],
    by: By<'_>,
) -> DbResult<()> {
    for id in ids {
        record_thread(&mut *conn, kind::THREAD_UPDATED, *id, by).await?;
    }
    Ok(())
}

/// A thread a `DELETE … RETURNING id, kind, folder_id, mcp_tools` just
/// removed in this transaction.
#[derive(Debug, Clone, Copy)]
pub struct Gone {
    pub id: i64,
    /// Its level (L3): a reader that did not see it never heard of it.
    pub admin: i64,
    pub folder_id: Option<i64>,
}

impl Gone {
    /// The row of a `DELETE FROM chat_threads … RETURNING id, kind,
    /// folder_id, mcp_tools`. Tools that do not parse attach nothing, as
    /// the thread's own read takes them.
    pub fn of(row: &sqlx::sqlite::SqliteRow) -> Self {
        let tools: Vec<super::ThreadMcp> =
            serde_json::from_str(row.get::<String, _>("mcp_tools").as_str()).unwrap_or_default();
        let level = if row.get::<String, _>("kind") == "admin" {
            2
        } else {
            i64::from(super::carries_self_admin(&tools))
        };
        Self {
            id: row.get("id"),
            admin: level,
            folder_id: row.get("folder_id"),
        }
    }
}

/// Record [`kind::THREAD_DELETED`] for each thread in `gone`.
pub async fn record_threads_deleted(
    conn: &mut SqliteConnection,
    gone: &[Gone],
    by: By<'_>,
) -> DbResult<()> {
    for g in gone {
        record(
            &mut *conn,
            Change {
                kind: kind::THREAD_DELETED,
                thread_id: Some(g.id),
                folder_id: g.folder_id,
                admin: g.admin,
                by,
                detail: None,
            },
        )
        .await?;
    }
    Ok(())
}

/// Record a folder event as the folder stands in this transaction: its
/// level (L3, review W3-1, W6-1) is read from it. A folder that is not there
/// records nothing.
pub async fn record_folder(
    conn: &mut SqliteConnection,
    kind: &str,
    folder_id: i64,
    by: By<'_>,
) -> DbResult<()> {
    record_folder_since(conn, kind, folder_id, by, None).await
}

/// Folder `folder_id`'s level now (`self_admin_folder!`; `None`: no such
/// folder), for [`record_folder_since`].
pub async fn self_admin_folder_now(
    conn: &mut SqliteConnection,
    folder_id: i64,
) -> DbResult<Option<i64>> {
    Ok(sqlx::query_scalar(concat!(
        "SELECT ",
        self_admin_folder!(""),
        " FROM chat_folders WHERE id = ?1"
    ))
    .bind(folder_id)
    .fetch_optional(conn)
    .await?)
}

/// [`record_folder`] after a write that may have changed the folder's level
/// (`was`, as [`self_admin_folder_now`] said it before): a change is said in
/// `detail`.
pub async fn record_folder_since(
    conn: &mut SqliteConnection,
    kind: &str,
    folder_id: i64,
    by: By<'_>,
    was: Option<i64>,
) -> DbResult<()> {
    sqlx::query(concat!(
        "INSERT INTO chat_feed (type, folder_id, admin, by, detail)
         SELECT ?1, id, a, ?2,
                CASE WHEN ?4 IS NOT NULL AND a != ?4 THEN json_object('admin_was', ?4) END
         FROM (SELECT id, ",
        self_admin_folder!(""),
        " AS a FROM chat_folders WHERE id = ?3)"
    ))
    .bind(kind)
    .bind(by)
    .bind(folder_id)
    .bind(was)
    .execute(conn)
    .await?;
    Ok(())
}

/// Record that device key `key_id`'s level of lmgw's admin tools is now
/// `self_admin`, where it was `was`, in the transaction of the key's write:
/// its feed hears the threads and folders with the self-admin toolset come
/// (`*.created`) or go (`*.deleted`) when the level moves above `off` or
/// back, and a `state` with the level either way — now if it is open, as a
/// `resync` at its next catch-up otherwise (`web::chat_feed`). No other
/// reader hears of it: the record is at the level no device sees.
pub async fn record_device_reach(
    conn: &mut SqliteConnection,
    key_id: i64,
    self_admin: crate::config::DeviceAdmin,
    was: crate::config::DeviceAdmin,
) -> DbResult<()> {
    record(
        conn,
        Change {
            kind: kind::DEVICE_REACH,
            admin: 2,
            by: Some(BY_OWNER),
            detail: Some(
                serde_json::json!({
                    "key_id": key_id,
                    "self_admin": self_admin.as_str(),
                    "self_admin_was": was.as_str(),
                })
                .to_string(),
            ),
            ..Default::default()
        },
    )
    .await
}

/// Record that the gateway's self-admin level is now `self_admin`, where it
/// was `was`, in the transaction of the settings save (`save_settings`):
/// every device's feed reads it as a move of its own capped level (the
/// toolset's threads and folders come or go where that moved to or from
/// `off`, and a `state` where it moved), live or, in a catch-up, as a
/// `resync`. The record is at the level no device sees.
pub async fn record_gateway_reach(
    conn: &mut SqliteConnection,
    self_admin: crate::config::SelfAdmin,
    was: crate::config::SelfAdmin,
) -> DbResult<()> {
    record(
        conn,
        Change {
            kind: kind::GATEWAY_REACH,
            admin: 2,
            by: Some(BY_OWNER),
            detail: Some(
                serde_json::json!({
                    "self_admin": self_admin.as_str(),
                    "self_admin_was": was.as_str(),
                })
                .to_string(),
            ),
            ..Default::default()
        },
    )
    .await
}

/// What a device's admin tools may do at one point of the feed: its own
/// level and the gateway's (client-apps design L3's note, 2026-10-07).
/// What it sees follows the capped level: the toolset's threads and
/// folders above `off`, none of them at `off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Levels {
    pub device: crate::config::DeviceAdmin,
    pub gateway: crate::config::SelfAdmin,
}

impl Levels {
    /// The device's level capped by the gateway's.
    pub fn may_do(self) -> crate::config::SelfAdmin {
        self.device.capped(self.gateway)
    }

    /// How far a device at these levels reaches (L3).
    pub fn reach(self) -> super::AdminThreads {
        super::AdminThreads::of_device(self.may_do().allows_read())
    }
}

/// Record that ongoing folder `folder_id`'s current thread is now
/// `thread_id` (`None`: it has none any more) where it was `previous`, for
/// `reason` (`store::reason`, §3.2), inside the transaction of the write
/// that moved it.
///
/// A reader never hears of it while it does not see the folder or the new
/// thread (L3, review W3-1): the record's level is the higher of the two. A
/// previous thread's level is noted in `detail` (`previous_level`), so a
/// device's rendering does not name one it does not see.
pub async fn record_folder_current(
    conn: &mut SqliteConnection,
    folder_id: i64,
    thread_id: Option<i64>,
    previous: Option<i64>,
    reason: &str,
    by: By<'_>,
) -> DbResult<()> {
    let folder_level = self_admin_folder_now(&mut *conn, folder_id)
        .await?
        .unwrap_or(0);
    let thread_level = match thread_id {
        Some(id) => self_admin_now(&mut *conn, id).await?.unwrap_or(0),
        None => 0,
    };
    let previous_level = match previous {
        Some(id) => self_admin_now(&mut *conn, id).await?.unwrap_or(0),
        None => 0,
    };
    let mut detail = serde_json::json!({ "previous_thread_id": previous, "reason": reason });
    if previous_level > 0 {
        detail["previous_level"] = serde_json::json!(previous_level);
    }
    record(
        conn,
        Change {
            kind: kind::FOLDER_CURRENT,
            thread_id,
            folder_id: Some(folder_id),
            admin: folder_level.max(thread_level),
            by,
            detail: Some(detail.to_string()),
        },
    )
    .await
}

impl Record {
    /// `folder.current`'s previous thread's level when the record was
    /// written ([`record_folder_current`]); a record from before the levels
    /// (`previous_admin: true`) reads as one no device sees.
    pub fn previous_level(&self) -> i64 {
        let Some(v) = self
            .detail
            .as_deref()
            .and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok())
        else {
            return 0;
        };
        if let Some(level) = v.get("previous_level").and_then(serde_json::Value::as_i64) {
            return level;
        }
        match v.get("previous_admin").and_then(serde_json::Value::as_bool) {
            Some(true) => 2,
            _ => 0,
        }
    }

    /// The level before this write, when the write changed it
    /// (`record_thread_since`, `record_folder_since`); a record from before
    /// the levels (`true`) reads as one no device saw.
    pub fn admin_was(&self) -> Option<i64> {
        let v: serde_json::Value = serde_json::from_str(self.detail.as_deref()?).ok()?;
        let was = v.get("admin_was")?;
        was.as_i64()
            .or_else(|| was.as_bool().map(|b| if b { 2 } else { 0 }))
    }

    /// A [`kind::GATEWAY_REACH`] record's new level.
    pub fn gateway_reach(&self) -> Option<crate::config::SelfAdmin> {
        if self.kind != kind::GATEWAY_REACH {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(self.detail.as_deref()?).ok()?;
        crate::config::SelfAdmin::parse(v.get("self_admin")?.as_str()?)
    }

    /// A [`kind::DEVICE_REACH`] record's key and the device's new level.
    pub fn device_reach(&self) -> Option<(i64, crate::config::DeviceAdmin)> {
        if self.kind != kind::DEVICE_REACH {
            return None;
        }
        reach_of(self.detail.as_deref()?)
    }
}

/// A `device.reach` record's key and level, from its `detail`: the level's
/// name, or a switch from before the levels (`true` reads as `read_only`,
/// as migration 0068 reads a switched-on row).
fn reach_of(detail: &str) -> Option<(i64, crate::config::DeviceAdmin)> {
    use crate::config::DeviceAdmin;
    let v: serde_json::Value = serde_json::from_str(detail).ok()?;
    let level = match v.get("self_admin")? {
        serde_json::Value::Bool(true) => DeviceAdmin::ReadOnly,
        serde_json::Value::Bool(false) => DeviceAdmin::Off,
        serde_json::Value::String(s) => DeviceAdmin::parse(s)?,
        _ => return None,
    };
    Some((v.get("key_id")?.as_i64()?, level))
}

/// The device's level before the first [`kind::DEVICE_REACH`] record of key
/// `key_id` after `seq` and up to `through`, when there is one: a stream
/// that catches up from `seq` to `through` had the reach of that level when
/// it reads the records up to it (`web::chat_feed`).
pub async fn first_device_reach_after(
    pool: &SqlitePool,
    key_id: i64,
    seq: i64,
    through: i64,
) -> DbResult<Option<crate::config::DeviceAdmin>> {
    let detail: Option<String> = sqlx::query_scalar(
        "SELECT detail FROM chat_feed
         WHERE seq > ?2 AND seq <= ?4 AND type = ?3 AND json_valid(detail)
           AND json_extract(detail, '$.key_id') = ?1
         ORDER BY seq LIMIT 1",
    )
    .bind(key_id)
    .bind(seq)
    .bind(kind::DEVICE_REACH)
    .bind(through)
    .fetch_optional(pool)
    .await?;
    Ok(detail.as_deref().and_then(level_before))
}

/// The gateway's level before the first [`kind::GATEWAY_REACH`] record after
/// `seq` and up to `through`, when there is one: a device's stream that
/// catches up from `seq` had the reach that level gave it when it reads the
/// records up to it.
pub async fn first_gateway_reach_after(
    pool: &SqlitePool,
    seq: i64,
    through: i64,
) -> DbResult<Option<crate::config::SelfAdmin>> {
    let was: Option<Option<String>> = sqlx::query_scalar(
        "SELECT json_extract(detail, '$.self_admin_was') FROM chat_feed
         WHERE seq > ?1 AND seq <= ?3 AND type = ?2 AND json_valid(detail)
         ORDER BY seq LIMIT 1",
    )
    .bind(seq)
    .bind(kind::GATEWAY_REACH)
    .bind(through)
    .fetch_optional(pool)
    .await?;
    Ok(was
        .flatten()
        .as_deref()
        .and_then(crate::config::SelfAdmin::parse))
}

/// The level a `device.reach` record says the device had before it: its
/// `self_admin_was`, or, on a record from before the levels, the other
/// value of the switch.
fn level_before(detail: &str) -> Option<crate::config::DeviceAdmin> {
    use crate::config::DeviceAdmin;
    let v: serde_json::Value = serde_json::from_str(detail).ok()?;
    if let Some(was) = v
        .get("self_admin_was")
        .and_then(serde_json::Value::as_str)
        .and_then(DeviceAdmin::parse)
    {
        return Some(was);
    }
    let (_, now) = reach_of(detail)?;
    Some(if now.is_on() {
        DeviceAdmin::Off
    } else {
        DeviceAdmin::ReadOnly
    })
}

/// One stored record, as delivery reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub seq: i64,
    pub at: String,
    pub kind: String,
    pub thread_id: Option<i64>,
    pub folder_id: Option<i64>,
    pub message_ids: Option<String>,
    pub by: Option<String>,
    /// The level of what it is about (`Change::admin`).
    pub admin: i64,
    pub detail: Option<String>,
    /// The record's check, the third part of its cursor (reviews W4-11,
    /// W5-4, W6-3): 16 hex digits drawn at random when it was written
    /// (migration 0065's trigger) and stored with it. A cursor names one
    /// record for good only within one database's history: a database
    /// restored from an older copy keeps its epoch and writes its numbers a
    /// second time, so the tag a cursor carries is compared with the one
    /// stored at its number. It is no function of the record, so a device
    /// handed the tag of a record it may not see learns nothing of it.
    pub tag: String,
}

fn record_from_row(row: &sqlx::sqlite::SqliteRow) -> Record {
    Record {
        seq: row.get("seq"),
        at: row.get("at"),
        kind: row.get("type"),
        thread_id: row.get("thread_id"),
        folder_id: row.get("folder_id"),
        message_ids: row.get("message_ids"),
        by: row.get("by"),
        admin: row.get("admin"),
        detail: row.get("detail"),
        tag: row.get("tag"),
    }
}

/// The record `seq`, when the table still holds it.
pub async fn record_at(pool: &SqlitePool, seq: i64) -> DbResult<Option<Record>> {
    let row = sqlx::query("SELECT * FROM chat_feed WHERE seq = ?1")
        .bind(seq)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(record_from_row))
}

/// This database's epoch: the first half of every cursor.
pub async fn epoch(pool: &SqlitePool) -> DbResult<String> {
    Ok(
        sqlx::query_scalar("SELECT epoch FROM chat_feed_meta WHERE id = 1")
            .fetch_one(pool)
            .await?,
    )
}

/// Which `seq` values the table can answer for: every record after
/// `pruned_through` is kept, and `head` is the newest ever written (`0`
/// before the first). A cursor `c` is whole when `pruned_through <= c <=
/// head`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub pruned_through: i64,
    pub head: i64,
}

pub async fn bounds(pool: &SqlitePool) -> DbResult<Bounds> {
    let mut conn = pool.acquire().await?;
    bounds_on(&mut conn).await
}

async fn bounds_on(conn: &mut SqliteConnection) -> DbResult<Bounds> {
    let row = sqlx::query(
        "SELECT m.pruned_through AS pruned_through,
                COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'chat_feed'), 0) AS head
         FROM chat_feed_meta m WHERE m.id = 1",
    )
    .fetch_one(conn)
    .await?;
    Ok(Bounds {
        pruned_through: row.get("pruned_through"),
        head: row.get("head"),
    })
}

/// [`bounds`], device key `key_id`'s level of lmgw's admin tools (`off`
/// without a key) and the gateway's self-admin level, read in one
/// transaction (review P-5, 2026-10-07): the levels are the ones in force
/// at `head`. Every `device.reach` and `gateway.reach` record up to `head`
/// is in them, and one after `head` is a change the stream reads after it,
/// so no move falls between the reads.
pub async fn bounds_and_switch(
    pool: &SqlitePool,
    key_id: Option<i64>,
) -> DbResult<(Bounds, Levels)> {
    // Reads only, for one snapshot: deferred, as no write lock is needed
    // (`begin_write` is for a transaction that writes).
    #[allow(clippy::disallowed_methods)]
    let mut tx = pool.begin().await?;
    let device = match key_id {
        Some(id) => super::device_self_admin(&mut tx, id).await?,
        None => crate::config::DeviceAdmin::Off,
    };
    let gateway = super::gateway_self_admin_in(&mut tx).await?;
    let bounds = bounds_on(&mut tx).await?;
    tx.commit().await?;
    Ok((bounds, Levels { device, gateway }))
}

/// [`page`] and [`bounds`] read in one transaction, so they agree (review
/// W4-4): a prune that lands while a stream catches up shows in the bounds
/// read with the page it left, never as records skipped without a word.
pub async fn page_with_bounds(
    pool: &SqlitePool,
    after: i64,
    limit: u32,
) -> DbResult<(Vec<Record>, Bounds)> {
    // Reads only, for one snapshot: deferred, as no write lock is needed
    // (`begin_write` is for a transaction that writes).
    #[allow(clippy::disallowed_methods)]
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT m.pruned_through AS pruned_through,
                COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'chat_feed'), 0) AS head
         FROM chat_feed_meta m WHERE m.id = 1",
    )
    .fetch_one(&mut *tx)
    .await?;
    let bounds = Bounds {
        pruned_through: row.get("pruned_through"),
        head: row.get("head"),
    };
    let rows = sqlx::query("SELECT * FROM chat_feed WHERE seq > ?1 ORDER BY seq LIMIT ?2")
        .bind(after)
        .bind(i64::from(limit.max(1)))
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((rows.iter().map(record_from_row).collect(), bounds))
}

/// Up to `limit` records after `after`, oldest first.
pub async fn page(pool: &SqlitePool, after: i64, limit: u32) -> DbResult<Vec<Record>> {
    let rows = sqlx::query("SELECT * FROM chat_feed WHERE seq > ?1 ORDER BY seq LIMIT ?2")
        .bind(after)
        .bind(i64::from(limit.max(1)))
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(record_from_row).collect())
}

/// Drop the records older than `days` (`0` keeps every record), oldest
/// first and as one prefix of the sequence, so `pruned_through` stays a
/// single bound. How many went.
pub async fn prune(pool: &SqlitePool, days: i64) -> DbResult<u64> {
    if days <= 0 {
        return Ok(0);
    }
    // Reads before it writes: the write lock first (review W4-10).
    let mut tx = super::begin_write(pool).await?;
    let through: Option<i64> =
        sqlx::query_scalar("SELECT MAX(seq) FROM chat_feed WHERE at < datetime('now', ?1)")
            .bind(format!("-{days} days"))
            .fetch_one(&mut *tx)
            .await?;
    let Some(through) = through else {
        return Ok(0);
    };
    let gone = sqlx::query("DELETE FROM chat_feed WHERE seq <= ?1")
        .bind(through)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    sqlx::query("UPDATE chat_feed_meta SET pruned_through = MAX(pruned_through, ?1) WHERE id = 1")
        .bind(through)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(gone)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        crate::store::open_in_memory().await.unwrap()
    }

    /// A folder event for folder `id`, whether or not it exists.
    async fn folder_event(conn: &mut SqliteConnection, id: i64) {
        record(
            conn,
            Change {
                kind: kind::FOLDER_UPDATED,
                folder_id: Some(id),
                ..Change::default()
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn records_come_back_in_commit_order_with_their_thread_s_kind() {
        let pool = pool().await;
        let chat = crate::store::create_chat_thread(&pool, "m", "chat")
            .await
            .unwrap();
        let admin = crate::store::create_chat_thread(&pool, "m", "admin")
            .await
            .unwrap();
        let all = page(&pool, 0, 100).await.unwrap();
        let got: Vec<_> = all
            .iter()
            .map(|r| (r.kind.as_str(), r.thread_id, r.admin))
            .collect();
        assert_eq!(
            got,
            vec![
                (kind::THREAD_CREATED, Some(chat), 0),
                (kind::THREAD_CREATED, Some(admin), 2),
            ]
        );
        assert!(all[0].seq < all[1].seq);
        assert_eq!(bounds(&pool).await.unwrap().head, all[1].seq);
        // Paged: one at a time, every row.
        let first = page(&pool, 0, 1).await.unwrap();
        assert_eq!(first, vec![all[0].clone()]);
        assert_eq!(
            page(&pool, first[0].seq, 1).await.unwrap(),
            vec![all[1].clone()]
        );
    }

    #[tokio::test]
    async fn a_thread_that_is_not_there_records_nothing() {
        let pool = pool().await;
        let mut conn = pool.acquire().await.unwrap();
        record_thread(&mut conn, kind::THREAD_UPDATED, 404, Some(BY_OWNER))
            .await
            .unwrap();
        drop(conn);
        assert!(page(&pool, 0, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_rolled_back_write_records_nothing() {
        let pool = pool().await;
        let mut tx = crate::store::begin_write(&pool).await.unwrap();
        folder_event(&mut tx, 1).await;
        tx.rollback().await.unwrap();
        assert!(page(&pool, 0, 10).await.unwrap().is_empty());
        assert_eq!(bounds(&pool).await.unwrap().head, 0);
    }

    #[tokio::test]
    async fn pruning_keeps_a_prefix_bound_and_never_reuses_a_number() {
        let pool = pool().await;
        for id in 1..=3 {
            let mut conn = pool.acquire().await.unwrap();
            folder_event(&mut conn, id).await;
        }
        // The first two are old.
        sqlx::query("UPDATE chat_feed SET at = datetime('now', '-9 days') WHERE seq <= 2")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(prune(&pool, 0).await.unwrap(), 0, "0 keeps every record");
        assert_eq!(prune(&pool, 7).await.unwrap(), 2);
        let b = bounds(&pool).await.unwrap();
        assert_eq!((b.pruned_through, b.head), (2, 3));
        // Everything pruned: the head is still the newest number handed out.
        sqlx::query("UPDATE chat_feed SET at = datetime('now', '-9 days')")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(prune(&pool, 7).await.unwrap(), 1);
        assert_eq!(
            bounds(&pool).await.unwrap(),
            Bounds {
                pruned_through: 3,
                head: 3
            }
        );
        let mut conn = pool.acquire().await.unwrap();
        folder_event(&mut conn, 9).await;
        drop(conn);
        assert_eq!(page(&pool, 3, 10).await.unwrap()[0].seq, 4);
    }

    #[tokio::test]
    async fn the_self_admin_flag_follows_the_thread_and_says_when_it_changed() {
        let pool = pool().await;
        let id = crate::store::create_chat_thread(&pool, "m", "chat")
            .await
            .unwrap();
        let mut tx = crate::store::begin_write(&pool).await.unwrap();
        let was = self_admin_now(&mut tx, id).await.unwrap();
        assert_eq!(was, Some(0));
        sqlx::query("UPDATE chat_threads SET mcp_tools = ?2 WHERE id = ?1")
            .bind(id)
            .bind(r#"[{"server_label":" lmgw "}]"#)
            .execute(&mut *tx)
            .await
            .unwrap();
        record_thread_since(&mut tx, kind::THREAD_UPDATED, id, None, was)
            .await
            .unwrap();
        // A malformed list is no toolset: read as none, never an error.
        sqlx::query("UPDATE chat_threads SET mcp_tools = '[\"lmgw\", 3' WHERE id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap();
        record_thread_since(&mut tx, kind::THREAD_UPDATED, id, None, Some(1))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let all = page(&pool, 0, 10).await.unwrap();
        let got: Vec<_> = all.iter().map(|r| (r.admin, r.admin_was())).collect();
        assert_eq!(got, vec![(0, None), (1, Some(0)), (0, Some(1))]);
    }

    #[tokio::test]
    async fn the_epoch_is_minted_once_per_database() {
        let a = epoch(&pool().await).await.unwrap();
        let b = epoch(&pool().await).await.unwrap();
        assert_eq!(a.len(), 32, "{a}");
        assert_ne!(a, b);
    }

    /// A `device.reach` record says the level and the one before it; one
    /// written before the levels (a switch) reads as `read_only` for `true`,
    /// as migration 0068 reads a switched-on row (the pre-merge review's
    /// P-3).
    #[test]
    fn a_reach_record_reads_its_level_and_an_earlier_switch() {
        use crate::config::DeviceAdmin;
        let new = r#"{"key_id":4,"self_admin":"full","self_admin_was":"read_only"}"#;
        assert_eq!(reach_of(new), Some((4, DeviceAdmin::Full)));
        assert_eq!(level_before(new), Some(DeviceAdmin::ReadOnly));
        let on = r#"{"key_id":4,"self_admin":true}"#;
        assert_eq!(reach_of(on), Some((4, DeviceAdmin::ReadOnly)));
        assert_eq!(level_before(on), Some(DeviceAdmin::Off));
        let off = r#"{"key_id":4,"self_admin":false}"#;
        assert_eq!(level_before(off), Some(DeviceAdmin::ReadOnly));
        assert_eq!(reach_of(r#"{"key_id":4,"self_admin":"root"}"#), None);
    }
}
