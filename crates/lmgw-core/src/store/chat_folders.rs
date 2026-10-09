//! Chat folders (chat-complete design §5): one level of grouping for threads,
//! each with optional [`ThreadDefaults`] a new thread in it starts from.

use sqlx::{Row, SqlitePool};

pub use lmgw_api_types::chat_folders::FolderOngoing;

use super::*;

/// What a new thread in a folder starts with. Every field is optional: unset
/// means the global behaviour (the default system prompt, the route's own
/// sampling, …). A **copy** is taken at creation ([`ThreadDefaults::apply`]);
/// changing the folder later never reaches a thread that exists.
///
/// This struct is the one place a new thread setting joins the folders
/// feature: add the field here and to [`ThreadDefaults::apply`], and (for the
/// input checks) to `web::chat_folders::check_defaults`. The UI keeps folder
/// defaults as JSON: its folder form is the thread settings' fields
/// (`lmgw-ui`'s `pages/chat_folders.rs`, `FolderSettingsForm`).
///
/// Input is strict (`deny_unknown_fields`: a typo is a 400, not a silently
/// ignored default); what is *stored* is read tolerantly
/// ([`ThreadDefaults::from_stored`]) so a row written by a newer build, with a
/// field this one does not know, still lists and applies.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThreadDefaults {
    pub model_alias: Option<String>,
    pub system_prompt: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i64>,
    pub top_p: Option<f64>,
    pub top_k: Option<i64>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub seed: Option<i64>,
    pub stop: Option<Vec<String>>,
    pub reasoning_enabled: Option<bool>,
    pub reasoning_effort: Option<String>,
    pub reasoning_budget: Option<i64>,
    pub mcp_tools: Option<Vec<ThreadMcp>>,
    /// Knowledge bases (chat-complete design §9.3): the selection, the mode
    /// and the retrieval budget.
    pub kb_ids: Option<Vec<i64>>,
    pub kb_mode: Option<KbMode>,
    pub kb_budget_tokens: Option<i64>,
    /// Voice overrides (chat-voice design §2.2), laid field by field over
    /// the new thread's (empty) voice.
    pub voice: Option<ThreadVoice>,
    /// The personality profile a new thread starts with
    /// (personality-profiles design D9); whether it exists is the route's
    /// check, and a delete strips it from every folder.
    pub profile_id: Option<i64>,
}

impl ThreadDefaults {
    /// Whether every new thread in a folder with these defaults would
    /// attach the self-admin toolset — such a folder does not exist for a
    /// device (client-apps design L3, review W3-1). The store's SQL says the
    /// same (`self_admin_folder!`).
    pub fn drives_self_admin(&self) -> bool {
        self.mcp_tools
            .as_deref()
            .is_some_and(super::chat::carries_self_admin)
    }

    /// Read the stored JSON, ignoring keys this build does not know (a row
    /// from a newer build) and falling back to "no defaults" for text that
    /// is not an object at all — the folder still lists and can be repaired.
    pub fn from_stored(text: &str) -> Self {
        let Ok(serde_json::Value::Object(mut map)) = serde_json::from_str(text) else {
            return Self::default();
        };
        // The voice is read on its own and tolerantly (chat-voice design
        // §2.2): it is strict on input, so a key a newer build added inside
        // it would fail the whole read below, and `unwrap_or_default` would
        // wipe every default of the folder.
        let voice = map.remove("voice").and_then(ThreadVoice::from_value);
        // Every field serializes (no skip), so the default's keys are the
        // known set — adding a field needs no second list.
        if let Ok(serde_json::Value::Object(known)) = serde_json::to_value(Self::default()) {
            map.retain(|k, _| known.contains_key(k));
        }
        let mut d: Self =
            serde_json::from_value(serde_json::Value::Object(map)).unwrap_or_default();
        d.voice = voice;
        d
    }

    /// Lay the set fields over `t`. Unset fields leave the thread's own
    /// (global) value alone.
    pub fn apply(&self, t: &mut ChatThread) {
        let d = self.clone();
        if let Some(v) = d.model_alias {
            t.model_alias = v;
        }
        if let Some(v) = d.system_prompt {
            t.system_prompt = v;
        }
        t.temperature = d.temperature.or(t.temperature);
        t.max_tokens = d.max_tokens.or(t.max_tokens);
        t.top_p = d.top_p.or(t.top_p);
        t.top_k = d.top_k.or(t.top_k);
        t.min_p = d.min_p.or(t.min_p);
        t.repeat_penalty = d.repeat_penalty.or(t.repeat_penalty);
        t.presence_penalty = d.presence_penalty.or(t.presence_penalty);
        t.frequency_penalty = d.frequency_penalty.or(t.frequency_penalty);
        t.seed = d.seed.or(t.seed);
        if let Some(v) = d.stop {
            t.stop = v;
        }
        t.reasoning_enabled = d.reasoning_enabled.or(t.reasoning_enabled);
        t.reasoning_effort = d.reasoning_effort.or_else(|| t.reasoning_effort.take());
        t.reasoning_budget = d.reasoning_budget.or(t.reasoning_budget);
        if let Some(v) = d.mcp_tools {
            t.mcp_tools = v;
        }
        if let Some(v) = d.kb_ids {
            t.kb_ids = v;
        }
        t.kb_mode = d.kb_mode.unwrap_or(t.kb_mode);
        t.kb_budget_tokens = d.kb_budget_tokens.or(t.kb_budget_tokens);
        if let Some(v) = &d.voice {
            t.voice.overlay(v);
        }
        t.profile_id = d.profile_id.or(t.profile_id);
    }
}

/// A folder as stored.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ChatFolder {
    pub id: i64,
    pub name: String,
    pub sort: i64,
    pub defaults: ThreadDefaults,
    /// One ongoing conversation (client-apps design §3): its idle rollover
    /// and its current thread. `None` for an ordinary folder.
    pub ongoing: Option<FolderOngoing>,
    /// The folder's own retention for its threads (§11 Q2): days of
    /// inactivity before the sweep archives one, and days after archiving
    /// before it deletes one. `None` is the global setting
    /// (`chat_archive_days`, `chat_purge_days`); `0` disables the step.
    pub archive_days: Option<i64>,
    pub purge_days: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
    /// A device deleted it while it held threads out of the device's reach
    /// (review W6-1): it stays for those threads, and is out of every
    /// device's reach until the owner shows it to devices again
    /// (`ChatFolderPatch::show_to_devices`, review F-7). The owner's list says
    /// so; a device never sees such a folder.
    pub devices_hidden: bool,
    /// The owner's approval floor for the defaults' `mcp_tools`
    /// (client-apps design §6.6; `ChatThread::approval_floor`): set by the
    /// owner's writes of the defaults alone, and what a new thread in the
    /// folder starts its own from. Never on the wire.
    #[serde(skip)]
    pub approval_floor: Vec<ThreadMcp>,
}

impl ChatFolder {
    /// Its current thread, when it is an ongoing conversation with one.
    pub fn current_thread_id(&self) -> Option<i64> {
        self.ongoing.as_ref().and_then(|o| o.current_thread_id)
    }

    /// How far into the self-admin plane it reaches (L3; the store's SQL
    /// says the same, `self_admin_folder!`): `2` a device deleted it while
    /// it held threads out of the device's reach (review W6-1), and no
    /// device sees it until the owner shows it to devices again; `1` its
    /// defaults attach the self-admin toolset (review W3-1), which only a
    /// device that may use lmgw's admin tools sees; `0` anything else.
    pub fn reach_level(&self) -> u8 {
        if self.devices_hidden {
            2
        } else if self.defaults.drives_self_admin() {
            1
        } else {
            0
        }
    }
}

/// A folder with how many stored threads it holds, for the sidebar.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatFolderListed {
    #[serde(flatten)]
    pub folder: ChatFolder,
    pub threads_active: i64,
    pub threads_archived: i64,
}

fn folder_from_row(row: &sqlx::sqlite::SqliteRow) -> ChatFolder {
    ChatFolder {
        id: row.get("id"),
        name: row.get("name"),
        sort: row.get("sort"),
        defaults: ThreadDefaults::from_stored(row.get::<String, _>("defaults").as_str()),
        ongoing: row
            .get::<Option<i64>, _>("ongoing_idle_minutes")
            .map(|idle_minutes| FolderOngoing {
                idle_minutes,
                current_thread_id: row.get("current_thread_id"),
            }),
        archive_days: row.get("archive_days"),
        purge_days: row.get("purge_days"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        devices_hidden: row.get::<i64, _>("devices_hidden") != 0,
        approval_floor: super::chat_approval_floor::from_column(
            row.get::<String, _>("approval_floor").as_str(),
        ),
    }
}

/// A listed folder from a row of [`list_chat_folders`]'s query: for a reader
/// that may not see Admin Chat (a device), a current thread that drives the
/// self-admin plane is not named (L3) — the thread does not exist for it.
fn listed_from_row(row: &sqlx::sqlite::SqliteRow) -> ChatFolderListed {
    let mut folder = folder_from_row(row);
    if row.get::<i64, _>("current_hidden") != 0 {
        if let Some(o) = folder.ongoing.as_mut() {
            o.current_thread_id = None;
        }
    }
    ChatFolderListed {
        folder,
        threads_active: row.get("n_active"),
        threads_archived: row.get("n_archived"),
    }
}

/// Every folder in sidebar order (`sort`, then creation) with its thread
/// counts — for a device, without the folders and threads that drive the
/// self-admin plane (client-apps design L3, review W3-1), which do not exist
/// for it.
pub async fn list_chat_folders(
    pool: &SqlitePool,
    admin: super::AdminThreads,
) -> DbResult<Vec<ChatFolderListed>> {
    let rows = sqlx::query(concat!(
        "SELECT f.*,
                COALESCE(SUM(t.id IS NOT NULL AND t.archived_at IS NULL), 0) AS n_active,
                COALESCE(SUM(t.id IS NOT NULL AND t.archived_at IS NOT NULL), 0) AS n_archived,
                EXISTS (SELECT 1 FROM chat_threads c
                        WHERE c.id = f.current_thread_id AND ",
        self_admin_thread!("c."),
        " >= ?1) AS current_hidden
         FROM chat_folders f LEFT JOIN chat_threads t
              ON t.folder_id = f.id AND ",
        self_admin_thread!("t."),
        " < ?1
         WHERE ",
        self_admin_folder!("f."),
        " < ?1
         GROUP BY f.id ORDER BY f.sort, f.id"
    ))
    .bind(admin.reach())
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(listed_from_row).collect())
}

/// One folder with its counts as [`list_chat_folders`] counts them — without
/// the threads that drive the self-admin plane when `admin` hides them — or
/// `None` when it is gone: what the change feed renders a `folder.*` record
/// as. The folder itself is returned whatever its defaults; whether the
/// reader may see it is the caller's
/// ([`ThreadDefaults::drives_self_admin`]).
pub async fn get_chat_folder_listed(
    pool: &SqlitePool,
    id: i64,
    admin: super::AdminThreads,
) -> DbResult<Option<ChatFolderListed>> {
    let row = sqlx::query(concat!(
        "SELECT f.*,
                COALESCE(SUM(t.id IS NOT NULL AND t.archived_at IS NULL), 0) AS n_active,
                COALESCE(SUM(t.id IS NOT NULL AND t.archived_at IS NOT NULL), 0) AS n_archived,
                EXISTS (SELECT 1 FROM chat_threads c
                        WHERE c.id = f.current_thread_id AND ",
        self_admin_thread!("c."),
        " >= ?2) AS current_hidden
         FROM chat_folders f LEFT JOIN chat_threads t
              ON t.folder_id = f.id AND ",
        self_admin_thread!("t."),
        " < ?2
         WHERE f.id = ?1
         GROUP BY f.id"
    ))
    .bind(id)
    .bind(admin.reach())
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(listed_from_row))
}

pub async fn get_chat_folder(pool: &SqlitePool, id: i64) -> DbResult<Option<ChatFolder>> {
    let row = sqlx::query("SELECT * FROM chat_folders WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(folder_from_row))
}

/// Add a folder at the end of the list (`sort` = one past the current last),
/// recorded in the feed (`folder.created`) as `by`'s. Its defaults' rules
/// are its approval floor: the gateway's own callers write as the owner.
pub async fn create_chat_folder(
    pool: &SqlitePool,
    name: &str,
    defaults: &ThreadDefaults,
    by: feed::By<'_>,
) -> DbResult<i64> {
    let floor = defaults.mcp_tools.clone().unwrap_or_default();
    create_chat_folder_with(pool, name, defaults, &FolderOptions::default(), &floor, by).await
}

/// What a folder is besides its name and defaults: whether it is one
/// ongoing conversation (its idle minutes) and its own retention. `None`
/// throughout is an ordinary folder on the global retention.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FolderOptions {
    pub ongoing_idle_minutes: Option<i64>,
    pub archive_days: Option<i64>,
    pub purge_days: Option<i64>,
}

/// [`create_chat_folder`] with its [`FolderOptions`] and its approval
/// floor (the owner's defaults' `mcp_tools`, none for a device's folder).
pub async fn create_chat_folder_with(
    pool: &SqlitePool,
    name: &str,
    defaults: &ThreadDefaults,
    opts: &FolderOptions,
    floor: &[ThreadMcp],
    by: feed::By<'_>,
) -> DbResult<i64> {
    let mut tx = super::begin_write(pool).await?;
    default_profile_exists(&mut tx, defaults).await?;
    let res = sqlx::query(
        "INSERT INTO chat_folders (name, sort, defaults, ongoing_idle_minutes, archive_days,
                                   purge_days, approval_floor)
         VALUES (?1, (SELECT COALESCE(MAX(sort), 0) + 1 FROM chat_folders), ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(name)
    .bind(serde_json::to_string(defaults).unwrap_or_else(|_| "{}".into()))
    .bind(opts.ongoing_idle_minutes)
    .bind(opts.archive_days)
    .bind(opts.purge_days)
    .bind(super::chat_approval_floor::to_column(floor))
    .execute(&mut *tx)
    .await?;
    let id = res.last_insert_rowid();
    feed::record_folder(&mut tx, feed::kind::FOLDER_CREATED, id, by).await?;
    tx.commit().await?;
    Ok(id)
}

/// The profile folder defaults `d` name exists, read on the write's
/// transaction (profiles review fix 7): the route checked it against the
/// snapshot, and a delete in between strips the id from every folder's
/// defaults in its own transaction, so a write after it must not put the
/// gone id back. Refused as the route refuses an unknown one (`400
/// unknown_profile`); the dropped transaction writes nothing.
async fn default_profile_exists(
    conn: &mut sqlx::SqliteConnection,
    d: &ThreadDefaults,
) -> DbResult<()> {
    let Some(id) = d.profile_id else {
        return Ok(());
    };
    let found: Option<i64> = sqlx::query_scalar("SELECT id FROM chat_profiles WHERE id = ?1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    match found {
        Some(_) => Ok(()),
        None => Err(crate::error::GatewayError::InvalidRequest {
            code: "unknown_profile",
            message: format!(
                "profile_id: there is no profile with id {id} (it was deleted; GET \
                 /chat/api/profiles lists them)"
            ),
        }),
    }
}

/// A folder patch: `None` leaves that column alone; `Some(None)` clears a
/// nullable one (the folder stops being ongoing, its retention goes back to
/// the global setting).
#[derive(Debug, Default)]
pub struct ChatFolderPatch {
    pub name: Option<String>,
    pub sort: Option<i64>,
    pub defaults: Option<ThreadDefaults>,
    pub ongoing_idle_minutes: Option<Option<i64>>,
    pub archive_days: Option<Option<i64>>,
    pub purge_days: Option<Option<i64>>,
    /// The owner shows a folder a device deleted to devices again (review
    /// F-7): its `devices_hidden` mark is cleared, in the patch's own
    /// transaction (review P-18). Recorded as the folder's level change and
    /// then each thread in it, so a device that sees the folder now
    /// receives it as `folder.created`, then its threads in it as
    /// `thread.updated` (it read them in no folder while it was hidden); the
    /// threads a device does not see stay out of its reach by their own
    /// level.
    pub show_to_devices: bool,
    /// The folder's new approval floor (client-apps design §6.6), when the
    /// owner wrote the defaults' tools; `None` leaves it alone.
    pub approval_floor: Option<Vec<ThreadMcp>>,
}

/// The current thread's settings a folder patch writes with it (client-apps
/// design L9): the thread as it is to be stored, and whether its stored
/// voice seed stays.
#[derive(Debug, Clone, Copy)]
pub struct CurrentSettings<'a> {
    pub thread: &'a ChatThread,
    pub seed: SeedWrite,
    /// Whether the patch's writer sees Admin Chat: a device's writes
    /// nothing to a thread that drives the self-admin plane by the time it
    /// lands (review W5-2).
    pub admin: super::AdminThreads,
}

/// What [`update_chat_folder`] wrote.
#[derive(Debug, Default)]
pub struct FolderUpdated {
    /// The folder was there.
    pub found: bool,
    /// What the current thread's settings write did, when it was made;
    /// `None` when it was not: none given, the thread gone or no longer the
    /// folder's current thread (review W5-12), or out of the writer's
    /// reach (review W5-2).
    pub current: Option<SettingsWritten>,
}

/// Apply a patch, recorded in the feed (`folder.updated`) as `by`'s — and,
/// in the same transaction, `current`'s settings onto the folder's current
/// thread (`thread.updated`). A folder that stops being ongoing loses its
/// current thread (`folder.current`, reason `not_ongoing`).
pub async fn update_chat_folder(
    pool: &SqlitePool,
    id: i64,
    p: &ChatFolderPatch,
    current: Option<CurrentSettings<'_>>,
    by: feed::By<'_>,
) -> DbResult<FolderUpdated> {
    let defaults = p
        .defaults
        .as_ref()
        .map(|d| serde_json::to_string(d).unwrap_or_else(|_| "{}".into()));
    // Reads before its writes: the write lock first, so another writer's
    // commit in between cannot fail it (review W4-10).
    let mut tx = super::begin_write(pool).await?;
    if let Some(d) = &p.defaults {
        default_profile_exists(&mut tx, d).await?;
    }
    let was = feed::self_admin_folder_now(&mut tx, id).await?;
    let previous: Option<i64> =
        sqlx::query_scalar("SELECT current_thread_id FROM chat_folders WHERE id = ?1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
    let ends_ongoing = p.ongoing_idle_minutes == Some(None);
    let res = sqlx::query(
        "UPDATE chat_folders SET name = COALESCE(?2, name), sort = COALESCE(?3, sort),
         defaults = COALESCE(?4, defaults),
         ongoing_idle_minutes = CASE WHEN ?5 THEN ?6 ELSE ongoing_idle_minutes END,
         current_thread_id = CASE WHEN ?5 AND ?6 IS NULL THEN NULL ELSE current_thread_id END,
         archive_days = CASE WHEN ?7 THEN ?8 ELSE archive_days END,
         purge_days = CASE WHEN ?9 THEN ?10 ELSE purge_days END,
         devices_hidden = CASE WHEN ?11 THEN 0 ELSE devices_hidden END,
         approval_floor = COALESCE(?12, approval_floor),
         updated_at = datetime('now') WHERE id = ?1",
    )
    .bind(id)
    .bind(&p.name)
    .bind(p.sort)
    .bind(defaults)
    .bind(p.ongoing_idle_minutes.is_some())
    .bind(p.ongoing_idle_minutes.flatten())
    .bind(p.archive_days.is_some())
    .bind(p.archive_days.flatten())
    .bind(p.purge_days.is_some())
    .bind(p.purge_days.flatten())
    .bind(p.show_to_devices)
    .bind(
        p.approval_floor
            .as_deref()
            .map(super::chat_approval_floor::to_column),
    )
    .execute(&mut *tx)
    .await?;
    let found = res.rows_affected() > 0;
    let mut out = FolderUpdated {
        found,
        current: None,
    };
    if found {
        feed::record_folder_since(&mut tx, feed::kind::FOLDER_UPDATED, id, by, was).await?;
        // The folder came into or left a device's reach (review W4-7): its
        // threads, rendered for a device with the folder or without it,
        // are each a change to it — after the folder, so a thread's folder
        // exists when the thread is read.
        if was.is_some() && was != feed::self_admin_folder_now(&mut tx, id).await? {
            let members: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM chat_threads WHERE folder_id = ?1 ORDER BY id")
                    .bind(id)
                    .fetch_all(&mut *tx)
                    .await?;
            feed::record_threads_updated(&mut tx, &members, by).await?;
        }
        if ends_ongoing && previous.is_some() {
            feed::record_folder_current(
                &mut tx,
                id,
                None,
                previous,
                super::chat_ongoing::reason::NOT_ONGOING,
                by,
            )
            .await?;
        }
        // Only onto the thread that is still the current one: a move out
        // or an archive (they take no folder lock) may have ended it since
        // the patch read it (review W5-12).
        if let Some(c) = current.filter(|c| previous == Some(c.thread.id)) {
            out.current =
                super::chat::write_chat_thread_settings(&mut tx, c.thread, c.seed, by, c.admin)
                    .await?;
        }
    }
    tx.commit().await?;
    Ok(out)
}

/// The threads of folder `id` a delete with its threads would delete for a
/// reader that reaches as far as `admin`.
pub async fn chat_folder_thread_ids(
    pool: &SqlitePool,
    id: i64,
    admin: super::AdminThreads,
) -> DbResult<Vec<i64>> {
    Ok(sqlx::query_scalar(concat!(
        "SELECT id FROM chat_threads WHERE folder_id = ?1 AND ",
        self_admin_thread!(""),
        " < ?2"
    ))
    .bind(id)
    .bind(admin.reach())
    .fetch_all(pool)
    .await?)
}

/// What a folder delete did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderDeleted {
    /// The folder is gone.
    Deleted,
    /// A device's delete of a folder that held threads out of its reach
    /// (review W6-1): the device's threads went as it asked, and the folder
    /// stays for the others, out of every device's reach.
    HiddenFromDevices,
}

/// Delete a folder. `delete_threads` false: its threads stay, without a
/// folder. True: they are deleted with it (messages and attachments
/// cascade). One transaction either way, and the feed records each thread
/// deleted or taken out and the folder's own end in it, as `by`'s. `None`
/// when there is no such folder.
///
/// **A device's delete** (`admin` short of the owner's reach; client-apps
/// design L3, review W6-1, decided by the owner) touches only the threads
/// the device sees. When the folder holds others — the owner's Admin Chat
/// threads, and its self-admin threads for a device not allowed lmgw's
/// admin tools — it stays in place holding them, its retention unchanged,
/// and is out of every device's reach from then on
/// ([`FolderDeleted::HiddenFromDevices`]), until the owner shows it to
/// devices again (`ChatFolderPatch::show_to_devices`). A device's feed reads
/// it as a delete, in a delete's order: the threads, then the folder's
/// removal; a current thread the device's threads took with them ends
/// without a word to devices. Nothing the deleting device receives says
/// that other threads were there. A device that sees some of the threads
/// that stay (one allowed lmgw's admin tools, beside a deleter that is
/// not) reads each of them in no folder, before the folder's removal
/// (review P-7).
pub async fn delete_chat_folder(
    pool: &SqlitePool,
    id: i64,
    delete_threads: bool,
    admin: super::AdminThreads,
    by: feed::By<'_>,
) -> DbResult<Option<FolderDeleted>> {
    Ok(delete_chat_folder_ids(pool, id, delete_threads, admin, by)
        .await?
        .map(|(how, _)| how))
}

/// [`delete_chat_folder`], with the ids of the threads it deleted — at
/// its commit, whatever moved in or out of the folder since a read before
/// it: what the route ends the live turns and bound sessions of.
pub async fn delete_chat_folder_ids(
    pool: &SqlitePool,
    id: i64,
    delete_threads: bool,
    admin: super::AdminThreads,
    by: feed::By<'_>,
) -> DbResult<Option<(FolderDeleted, Vec<i64>)>> {
    // The write lock from the first statement (review F-6): the read below
    // comes first, and a deferred transaction would fail with
    // `SQLITE_BUSY_SNAPSHOT` at its first write whenever another connection
    // committed in between.
    let mut tx = super::begin_write(pool).await?;
    let stays: i64 = sqlx::query_scalar(concat!(
        "SELECT EXISTS (SELECT 1 FROM chat_threads WHERE folder_id = ?1 AND ",
        self_admin_thread!(""),
        " >= ?2)"
    ))
    .bind(id)
    .bind(admin.reach())
    .fetch_one(&mut *tx)
    .await?;
    if stays != 0 {
        return delete_for_devices(tx, id, delete_threads, admin, by).await;
    }
    let mut deleted = Vec::new();
    if delete_threads {
        let gone: Vec<feed::Gone> = sqlx::query(concat!(
            "DELETE FROM chat_threads WHERE folder_id = ?1 AND ",
            self_admin_thread!(""),
            " < ?2 RETURNING id, kind, folder_id, mcp_tools"
        ))
        .bind(id)
        .bind(admin.reach())
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(feed::Gone::of)
        .collect();
        feed::record_threads_deleted(&mut tx, &gone, by).await?;
        // Their MCP tasks: an open one owes its cancel, a result goes
        // (MCP Tasks design §1.5).
        super::mcp_tasks::threads_gone(&mut tx).await?;
        deleted = gone.iter().map(|g| g.id).collect();
    }
    // What is left in it (everything, for `keep`) leaves the folder, with
    // the folder's approval rules in its floor.
    let leaving: Vec<i64> = sqlx::query_scalar("SELECT id FROM chat_threads WHERE folder_id = ?1")
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
    super::chat_approval_floor::fold_folder_floor(&mut tx, id, &leaving).await?;
    let left: Vec<i64> = sqlx::query_scalar(
        "UPDATE chat_threads SET folder_id = NULL WHERE folder_id = ?1 RETURNING id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    feed::record_threads_updated(&mut tx, &left, by).await?;
    // Recorded while the row is there to say whether a device saw it.
    feed::record_folder(&mut tx, feed::kind::FOLDER_DELETED, id, by).await?;
    let res = sqlx::query("DELETE FROM chat_folders WHERE id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let found = res.rows_affected() > 0;
    tx.commit().await?;
    Ok(found.then_some((FolderDeleted::Deleted, deleted)))
}

/// [`delete_chat_folder`] by a device, of a folder that holds threads out
/// of its reach: its own threads go as it asked, the folder stays for the
/// others, hidden from devices.
async fn delete_for_devices(
    mut tx: sqlx::Transaction<'_, sqlx::Sqlite>,
    id: i64,
    delete_threads: bool,
    admin: super::AdminThreads,
    by: feed::By<'_>,
) -> DbResult<Option<(FolderDeleted, Vec<i64>)>> {
    let was = feed::self_admin_folder_now(&mut tx, id).await?;
    let mut deleted = Vec::new();
    // Out of the devices' reach first, so what follows about the folder
    // (its current thread's end) is not a device's to hear.
    sqlx::query("UPDATE chat_folders SET devices_hidden = 1 WHERE id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    // A current thread the device's threads take with them ends here.
    let current: Option<i64> = sqlx::query_scalar(concat!(
        "SELECT t.id FROM chat_folders f JOIN chat_threads t ON t.id = f.current_thread_id
         WHERE f.id = ?1 AND ",
        self_admin_thread!("t."),
        " < ?2"
    ))
    .bind(id)
    .bind(admin.reach())
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(current) = current {
        super::chat_ongoing::clear_current(&mut tx, current, None, by).await?;
    }
    if delete_threads {
        let gone: Vec<feed::Gone> = sqlx::query(concat!(
            "DELETE FROM chat_threads WHERE folder_id = ?1 AND ",
            self_admin_thread!(""),
            " < ?2 RETURNING id, kind, folder_id, mcp_tools"
        ))
        .bind(id)
        .bind(admin.reach())
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(feed::Gone::of)
        .collect();
        feed::record_threads_deleted(&mut tx, &gone, by).await?;
        // Their MCP tasks: an open one owes its cancel, a result goes
        // (MCP Tasks design §1.5).
        super::mcp_tasks::threads_gone(&mut tx).await?;
        deleted = gone.iter().map(|g| g.id).collect();
    } else {
        let leaving: Vec<i64> = sqlx::query_scalar(concat!(
            "SELECT id FROM chat_threads WHERE folder_id = ?1 AND ",
            self_admin_thread!(""),
            " < ?2"
        ))
        .bind(id)
        .bind(admin.reach())
        .fetch_all(&mut *tx)
        .await?;
        super::chat_approval_floor::fold_folder_floor(&mut tx, id, &leaving).await?;
        let moved: Vec<i64> = sqlx::query_scalar(concat!(
            "UPDATE chat_threads SET folder_id = NULL WHERE folder_id = ?1 AND ",
            self_admin_thread!(""),
            " < ?2 RETURNING id"
        ))
        .bind(id)
        .bind(admin.reach())
        .fetch_all(&mut *tx)
        .await?;
        feed::record_threads_updated(&mut tx, &moved, by).await?;
    }
    // The threads that stay (review P-7): a device that sees some of them —
    // one allowed lmgw's admin tools, beside a deleter that is not — reads
    // each in no folder now, as for a folder that left its reach by a patch
    // (`update_chat_folder`). Before the folder's removal: a delete's order.
    let stayed: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM chat_threads WHERE folder_id = ?1 ORDER BY id")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    feed::record_threads_updated(&mut tx, &stayed, by).await?;
    // The folder's removal for the devices that saw it (it left their reach
    // with this write), an update for the owner.
    feed::record_folder_since(&mut tx, feed::kind::FOLDER_UPDATED, id, by, was).await?;
    tx.commit().await?;
    Ok(Some((FolderDeleted::HiddenFromDevices, deleted)))
}

/// Move a thread into a folder (`None` = out of any), recorded in the feed
/// (`thread.updated`) as `by`'s. Does not touch `updated_at`: filing a thread
/// is not activity, and must not float it up or reset the archive clock.
///
/// A thread moved out of the ongoing folder it is the current thread of is
/// that folder's current thread no more (`folder.current`, reason `gone`).
/// One that leaves a folder takes the folder's approval rules into its own
/// floor (`chat_approval_floor::fold_folder_floor`; client-apps design
/// §6.6).
pub async fn set_chat_thread_folder(
    pool: &SqlitePool,
    thread_id: i64,
    folder_id: Option<i64>,
    by: feed::By<'_>,
) -> DbResult<()> {
    let mut tx = super::begin_write(pool).await?;
    let was: Option<i64> = sqlx::query_scalar("SELECT folder_id FROM chat_threads WHERE id = ?1")
        .bind(thread_id)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
    if let Some(was) = was.filter(|w| Some(*w) != folder_id) {
        super::chat_approval_floor::fold_folder_floor(&mut tx, was, &[thread_id]).await?;
    }
    sqlx::query("UPDATE chat_threads SET folder_id = ?2 WHERE id = ?1")
        .bind(thread_id)
        .bind(folder_id)
        .execute(&mut *tx)
        .await?;
    feed::record_thread(&mut tx, feed::kind::THREAD_UPDATED, thread_id, by).await?;
    super::chat_ongoing::clear_current(&mut tx, thread_id, folder_id, by).await?;
    tx.commit().await?;
    Ok(())
}

/// Insert a new thread built from `t`'s settings (model, prompt, sampling,
/// reasoning, MCP servers, knowledge bases, voice, profile, `kind`,
/// `folder_id`) in one statement, so a thread
/// seeded from a folder's defaults never exists half-configured. Title and
/// timestamps take the column defaults. Recorded in the feed
/// (`thread.created`) as `by`'s. Returns the new id.
pub async fn create_chat_thread_from(
    pool: &SqlitePool,
    t: &ChatThread,
    by: feed::By<'_>,
) -> DbResult<i64> {
    let mut tx = super::begin_write(pool).await?;
    let id = insert_chat_thread(&mut tx, t, by).await?;
    tx.commit().await?;
    Ok(id)
}

/// [`create_chat_thread_from`]'s insert and its record, on the caller's
/// transaction.
pub(super) async fn insert_chat_thread(
    conn: &mut sqlx::SqliteConnection,
    t: &ChatThread,
    by: feed::By<'_>,
) -> DbResult<i64> {
    let mcp = serde_json::to_string(&t.mcp_tools).unwrap_or_else(|_| "[]".into());
    let res = sqlx::query(
        "INSERT INTO chat_threads
           (model_alias, system_prompt, temperature, max_tokens, kind, mcp_tools,
            reasoning_enabled, reasoning_effort, reasoning_budget, folder_id,
            top_p, top_k, min_p, repeat_penalty, presence_penalty, frequency_penalty,
            seed, stop, kb_ids, kb_mode, kb_budget_tokens, voice, profile_id, approval_floor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                 ?19, ?20, ?21, ?22, (SELECT id FROM chat_profiles WHERE id = ?23), ?24)",
    )
    .bind(&t.model_alias)
    .bind(&t.system_prompt)
    .bind(t.temperature)
    .bind(t.max_tokens)
    .bind(&t.kind)
    .bind(mcp)
    .bind(t.reasoning_enabled.map(i64::from))
    .bind(&t.reasoning_effort)
    .bind(t.reasoning_budget)
    .bind(t.folder_id)
    .bind(t.top_p)
    .bind(t.top_k)
    .bind(t.min_p)
    .bind(t.repeat_penalty)
    .bind(t.presence_penalty)
    .bind(t.frequency_penalty)
    .bind(t.seed)
    .bind(serde_json::to_string(&t.stop).unwrap_or_else(|_| "[]".into()))
    .bind(super::chat_knowledge::id_list_json(&t.kb_ids))
    .bind(t.kb_mode.as_str())
    .bind(t.kb_budget_tokens)
    .bind(t.voice.to_stored())
    .bind(t.profile_id)
    .bind(super::chat_approval_floor::to_column(&t.approval_floor))
    .execute(&mut *conn)
    .await?;
    let id = res.last_insert_rowid();
    feed::record_thread(&mut *conn, feed::kind::THREAD_CREATED, id, by).await?;
    Ok(id)
}
