//! Personality profiles, the store's half (personality-profiles design
//! §1.2): the `chat_profiles` table, what uses each row, and every write.
//!
//! Each write comes in two forms: `*_in`, on the caller's transaction, and
//! a pool form that opens one with [`begin_write`](super::begin_write) and
//! commits it when the write was not refused. The routes write through the
//! `_in` forms, so the feed's `profile.*` record joins the same transaction
//! (design §3.1); a refusal leaves the transaction to be dropped, which
//! rolls back whatever the caller wrote in it. After a commit the caller
//! reloads the snapshot, which holds every profile (D10).

use std::collections::HashMap;

use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::config::chat_profile::{name_key, ProfileBody};
pub use crate::config::chat_profile::{
    CreateKind, FolderRef, ProfileDeleted, ProfilePatch, ProfileRefusal, UsedBy,
};
use crate::config::ChatProfile;

use super::{feed, AdminThreads, DbResult};

pub mod builtin;

fn profile_from_row(row: &sqlx::sqlite::SqliteRow) -> ChatProfile {
    let builtin: Option<String> = row.get("builtin");
    let def = builtin.as_deref().and_then(builtin::builtin);
    ChatProfile::resolve(
        row.get("id"),
        row.get("name"),
        builtin,
        ProfileBody::from_stored(row.get::<String, _>("body").as_str()),
        def,
        row.get("created_at"),
        row.get("updated_at"),
    )
}

/// Every profile, in name order (then by id): what the snapshot holds.
pub async fn list_chat_profiles(pool: &SqlitePool) -> DbResult<Vec<ChatProfile>> {
    let rows = sqlx::query("SELECT * FROM chat_profiles ORDER BY name COLLATE NOCASE, id")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(profile_from_row).collect())
}

pub async fn get_chat_profile(pool: &SqlitePool, id: i64) -> DbResult<Option<ChatProfile>> {
    let mut conn = pool.acquire().await?;
    get_in(&mut conn, id).await
}

async fn get_in(conn: &mut SqliteConnection, id: i64) -> DbResult<Option<ChatProfile>> {
    let row = sqlx::query("SELECT * FROM chat_profiles WHERE id = ?1")
        .bind(id)
        .fetch_optional(conn)
        .await?;
    Ok(row.as_ref().map(profile_from_row))
}

/// What uses each profile, as far as `admin` reaches (the delete confirm's
/// source, §1.1): its stored threads, archived ones included, and the
/// folders whose defaults name it. A profile nothing uses is not in the
/// map.
pub async fn chat_profiles_used_by(
    pool: &SqlitePool,
    admin: AdminThreads,
) -> DbResult<HashMap<i64, UsedBy>> {
    let mut out: HashMap<i64, UsedBy> = HashMap::new();
    let threads = sqlx::query(concat!(
        "SELECT profile_id, COUNT(*) AS n FROM chat_threads
         WHERE profile_id IS NOT NULL AND ",
        self_admin_thread!(""),
        " < ?1 GROUP BY profile_id"
    ))
    .bind(admin.reach())
    .fetch_all(pool)
    .await?;
    for r in &threads {
        out.entry(r.get("profile_id")).or_default().threads = r.get("n");
    }
    let folders = sqlx::query(concat!(
        "SELECT id, name, json_extract(defaults, '$.profile_id') AS profile_id
         FROM chat_folders
         WHERE CASE WHEN json_valid(defaults)
                    THEN json_type(defaults, '$.profile_id') = 'integer' END
           AND ",
        self_admin_folder!(""),
        " < ?1 ORDER BY sort, id"
    ))
    .bind(admin.reach())
    .fetch_all(pool)
    .await?;
    for r in &folders {
        out.entry(r.get("profile_id"))
            .or_default()
            .folders
            .push(FolderRef {
                id: r.get("id"),
                name: r.get("name"),
            });
    }
    Ok(out)
}

/// What of profile `id`'s use steers lmgw's admin tools, read on `conn` (a
/// write's transaction, so the answer holds for the write): the threads
/// using it that are Admin Chat or carry the self-admin toolset
/// (`self_admin_thread!` level 1 or 2), and the folders whose defaults name
/// it and attach the toolset, so every new thread in them would. A folder
/// a device deleted (`devices_hidden`) counts only when its defaults attach
/// the toolset as well.
pub async fn admin_use_in(conn: &mut SqliteConnection, id: i64) -> DbResult<AdminUse> {
    let threads: i64 = sqlx::query_scalar(concat!(
        "SELECT COUNT(*) FROM chat_threads WHERE profile_id = ?1 AND ",
        self_admin_thread!(""),
        " >= 1"
    ))
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    let folders: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_folders
         WHERE CASE WHEN json_valid(defaults)
                    THEN json_type(defaults, '$.profile_id') = 'integer'
                         AND json_extract(defaults, '$.profile_id') = ?1
                         AND EXISTS (SELECT 1 FROM json_each(defaults, '$.mcp_tools')
                                     WHERE CASE WHEN type = 'object'
                                                THEN trim(json_extract(value, '$.server_label')) = 'lmgw'
                                                ELSE 0 END)
                    ELSE 0 END",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(AdminUse { threads, folders })
}

/// [`admin_use_in`]'s answer: counts only, so a refusal built from it names
/// the use without a thread's title or a folder's name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdminUse {
    pub threads: i64,
    pub folders: i64,
}

impl AdminUse {
    pub fn any(self) -> bool {
        self.threads > 0 || self.folders > 0
    }
}

/// The other profile whose name collides with `name` ([`name_key`]), but
/// for `except`: its name as stored.
async fn name_taken(
    conn: &mut SqliteConnection,
    name: &str,
    except: Option<i64>,
) -> DbResult<Option<String>> {
    let key = name_key(name);
    let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM chat_profiles")
        .fetch_all(conn)
        .await?;
    Ok(rows
        .into_iter()
        .find(|(id, n)| Some(*id) != except && name_key(n) == key)
        .map(|(_, n)| n))
}

/// Create a profile on the caller's transaction: the owner's own (a name,
/// normalised, and content), or a built-in again by key. Refused when the
/// name is taken, the built-in exists or there is no such built-in.
pub async fn create_in(
    conn: &mut SqliteConnection,
    kind: &CreateKind,
) -> DbResult<Result<ChatProfile, ProfileRefusal>> {
    let (name, key, body) = match kind {
        CreateKind::Named(name, draft) => (
            name.clone(),
            None,
            ProfileBody::from_draft(draft, None).to_stored(),
        ),
        CreateKind::Builtin(key) => {
            let Some(def) = builtin::builtin(key) else {
                return Ok(Err(ProfileRefusal::UnknownBuiltin(key.clone())));
            };
            let exists: Option<i64> =
                sqlx::query_scalar("SELECT id FROM chat_profiles WHERE builtin = ?1")
                    .bind(def.key)
                    .fetch_optional(&mut *conn)
                    .await?;
            if exists.is_some() {
                return Ok(Err(ProfileRefusal::BuiltinExists(def.key.to_string())));
            }
            (def.name.to_string(), Some(def.key), "{}".to_string())
        }
    };
    if let Some(other) = name_taken(&mut *conn, &name, None).await? {
        return Ok(Err(ProfileRefusal::NameTaken(other)));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO chat_profiles (name, builtin, body) VALUES (?1, ?2, ?3) RETURNING id",
    )
    .bind(&name)
    .bind(key)
    .bind(body)
    .fetch_one(&mut *conn)
    .await?;
    Ok(get_in(conn, id).await?.ok_or(ProfileRefusal::NotFound(id)))
}

/// Change profile `id` on the caller's transaction: the fields `p`
/// (normalised, `normalise_patch`) names, a built-in row's written
/// built-in text stored absent so it follows the built-in again.
pub async fn update_in(
    conn: &mut SqliteConnection,
    id: i64,
    p: &ProfilePatch,
) -> DbResult<Result<ChatProfile, ProfileRefusal>> {
    let Some(row) = sqlx::query("SELECT builtin, body FROM chat_profiles WHERE id = ?1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
    else {
        return Ok(Err(ProfileRefusal::NotFound(id)));
    };
    if let Some(name) = &p.name {
        if let Some(other) = name_taken(&mut *conn, name, Some(id)).await? {
            return Ok(Err(ProfileRefusal::NameTaken(other)));
        }
    }
    let def = row
        .get::<Option<String>, _>("builtin")
        .as_deref()
        .and_then(builtin::builtin);
    let mut body = ProfileBody::from_stored(row.get::<String, _>("body").as_str());
    body.apply_patch(p, def);
    sqlx::query(
        "UPDATE chat_profiles SET name = COALESCE(?2, name), body = ?3,
         updated_at = datetime('now') WHERE id = ?1",
    )
    .bind(id)
    .bind(&p.name)
    .bind(body.to_stored())
    .execute(&mut *conn)
    .await?;
    Ok(get_in(conn, id).await?.ok_or(ProfileRefusal::NotFound(id)))
}

/// Reset a built-in row to its built-in texts on the caller's transaction
/// (the editor's "Reset to built-in"): every field follows the built-in
/// again, and the voice is unset. The name stays.
pub async fn reset_in(
    conn: &mut SqliteConnection,
    id: i64,
) -> DbResult<Result<ChatProfile, ProfileRefusal>> {
    let builtin: Option<Option<String>> =
        sqlx::query_scalar("SELECT builtin FROM chat_profiles WHERE id = ?1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    match builtin {
        None => return Ok(Err(ProfileRefusal::NotFound(id))),
        Some(None) => return Ok(Err(ProfileRefusal::NotBuiltin(id))),
        Some(Some(_)) => {}
    }
    sqlx::query("UPDATE chat_profiles SET body = '{}', updated_at = datetime('now') WHERE id = ?1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(get_in(conn, id).await?.ok_or(ProfileRefusal::NotFound(id)))
}

/// Delete profile `id` on the caller's transaction (D16): every thread
/// using it goes back to none, every folder default naming it loses the
/// field, and the Chat's default for new threads (the settings blob's
/// `chat_profile`) is emptied when it names it — each change recorded in
/// the feed as `by`'s (`thread.updated`, `folder.updated`), and none moving
/// a thread's `updated_at` (it is no activity). The answer counts what was
/// cleared as far as `admin` reaches; everything is cleared regardless.
pub async fn delete_in(
    conn: &mut SqliteConnection,
    id: i64,
    admin: AdminThreads,
    by: feed::By<'_>,
) -> DbResult<Result<ProfileDeleted, ProfileRefusal>> {
    let found: Option<i64> = sqlx::query_scalar("SELECT id FROM chat_profiles WHERE id = ?1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    if found.is_none() {
        return Ok(Err(ProfileRefusal::NotFound(id)));
    }
    let threads: Vec<(i64, i64)> = sqlx::query_as(concat!(
        "UPDATE chat_threads SET profile_id = NULL WHERE profile_id = ?1 RETURNING id, ",
        self_admin_thread!("")
    ))
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let mut thread_ids: Vec<i64> = threads.iter().map(|(t, _)| *t).collect();
    thread_ids.sort_unstable();
    feed::record_threads_updated(&mut *conn, &thread_ids, by).await?;
    let mut folders: Vec<(i64, String, i64)> = sqlx::query_as(concat!(
        "UPDATE chat_folders SET defaults = json_remove(defaults, '$.profile_id'),
                updated_at = datetime('now')
         WHERE CASE WHEN json_valid(defaults)
                    THEN json_type(defaults, '$.profile_id') = 'integer'
                         AND json_extract(defaults, '$.profile_id') = ?1 END
         RETURNING id, name, ",
        self_admin_folder!("")
    ))
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    folders.sort_unstable_by_key(|(f, _, _)| *f);
    for (f, _, _) in &folders {
        feed::record_folder(&mut *conn, feed::kind::FOLDER_UPDATED, *f, by).await?;
    }
    let default_cleared = clear_default_in(&mut *conn, id).await?;
    sqlx::query("DELETE FROM chat_profiles WHERE id = ?1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(Ok(ProfileDeleted {
        deleted: id,
        threads_cleared: threads.iter().filter(|(_, l)| admin.sees(*l)).count() as i64,
        folders_cleared: folders
            .into_iter()
            .filter(|(_, _, l)| admin.sees(*l))
            .map(|(id, name, _)| FolderRef { id, name })
            .collect(),
        default_cleared,
    }))
}

/// Empty the Chat's profile for new threads (`chat_profile` in the
/// settings blob) when it names `id`, whether it is stored as a number or
/// as a numeric text; `true` when it did. The key is removed, which reads
/// as none.
async fn clear_default_in(conn: &mut SqliteConnection, id: i64) -> DbResult<bool> {
    let hit: Option<String> = sqlx::query_scalar(
        "UPDATE settings SET value = json_remove(value, '$.chat_profile')
         WHERE key = 'settings'
           AND CASE WHEN json_valid(value)
                    THEN json_type(value, '$.chat_profile') IN ('integer', 'text')
                         AND CAST(json_extract(value, '$.chat_profile') AS TEXT)
                             = CAST(?1 AS TEXT) END
         RETURNING key",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?;
    Ok(hit.is_some())
}

/// [`create_in`] in a transaction of its own, committed unless refused.
pub async fn create_chat_profile(
    pool: &SqlitePool,
    kind: &CreateKind,
) -> DbResult<Result<ChatProfile, ProfileRefusal>> {
    let mut tx = super::begin_write(pool).await?;
    let out = create_in(&mut tx, kind).await?;
    if out.is_ok() {
        tx.commit().await?;
    }
    Ok(out)
}

/// [`update_in`] in a transaction of its own, committed unless refused.
pub async fn update_chat_profile(
    pool: &SqlitePool,
    id: i64,
    p: &ProfilePatch,
) -> DbResult<Result<ChatProfile, ProfileRefusal>> {
    let mut tx = super::begin_write(pool).await?;
    let out = update_in(&mut tx, id, p).await?;
    if out.is_ok() {
        tx.commit().await?;
    }
    Ok(out)
}

/// [`reset_in`] in a transaction of its own, committed unless refused.
pub async fn reset_chat_profile(
    pool: &SqlitePool,
    id: i64,
) -> DbResult<Result<ChatProfile, ProfileRefusal>> {
    let mut tx = super::begin_write(pool).await?;
    let out = reset_in(&mut tx, id).await?;
    if out.is_ok() {
        tx.commit().await?;
    }
    Ok(out)
}

/// [`delete_in`] in a transaction of its own, committed unless refused.
pub async fn delete_chat_profile(
    pool: &SqlitePool,
    id: i64,
    admin: AdminThreads,
    by: feed::By<'_>,
) -> DbResult<Result<ProfileDeleted, ProfileRefusal>> {
    let mut tx = super::begin_write(pool).await?;
    let out = delete_in(&mut tx, id, admin, by).await?;
    if out.is_ok() {
        tx.commit().await?;
    }
    Ok(out)
}
