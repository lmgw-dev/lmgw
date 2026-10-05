//! Chat folders (chat-complete design §5): one level of grouping for threads,
//! each with optional [`ThreadDefaults`] a new thread in it starts from.

use sqlx::{Row, SqlitePool};

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
}

impl ThreadDefaults {
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
    }
}

/// A folder as stored.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatFolder {
    pub id: i64,
    pub name: String,
    pub sort: i64,
    pub defaults: ThreadDefaults,
    pub created_at: String,
    pub updated_at: String,
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
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

/// Every folder in sidebar order (`sort`, then creation) with its thread
/// counts.
pub async fn list_chat_folders(pool: &SqlitePool) -> DbResult<Vec<ChatFolderListed>> {
    let rows = sqlx::query(
        "SELECT f.*,
                COALESCE(SUM(t.id IS NOT NULL AND t.archived_at IS NULL), 0) AS n_active,
                COALESCE(SUM(t.id IS NOT NULL AND t.archived_at IS NOT NULL), 0) AS n_archived
         FROM chat_folders f LEFT JOIN chat_threads t ON t.folder_id = f.id
         GROUP BY f.id ORDER BY f.sort, f.id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| ChatFolderListed {
            folder: folder_from_row(r),
            threads_active: r.get("n_active"),
            threads_archived: r.get("n_archived"),
        })
        .collect())
}

pub async fn get_chat_folder(pool: &SqlitePool, id: i64) -> DbResult<Option<ChatFolder>> {
    let row = sqlx::query("SELECT * FROM chat_folders WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(folder_from_row))
}

/// Add a folder at the end of the list (`sort` = one past the current last).
pub async fn create_chat_folder(
    pool: &SqlitePool,
    name: &str,
    defaults: &ThreadDefaults,
) -> DbResult<i64> {
    let res = sqlx::query(
        "INSERT INTO chat_folders (name, sort, defaults)
         VALUES (?1, (SELECT COALESCE(MAX(sort), 0) + 1 FROM chat_folders), ?2)",
    )
    .bind(name)
    .bind(serde_json::to_string(defaults).unwrap_or_else(|_| "{}".into()))
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

/// A folder patch: `None` leaves that column alone.
#[derive(Debug, Default)]
pub struct ChatFolderPatch {
    pub name: Option<String>,
    pub sort: Option<i64>,
    pub defaults: Option<ThreadDefaults>,
}

/// Apply a patch; `false` when there is no such folder.
pub async fn update_chat_folder(pool: &SqlitePool, id: i64, p: &ChatFolderPatch) -> DbResult<bool> {
    let defaults = p
        .defaults
        .as_ref()
        .map(|d| serde_json::to_string(d).unwrap_or_else(|_| "{}".into()));
    let res = sqlx::query(
        "UPDATE chat_folders SET name = COALESCE(?2, name), sort = COALESCE(?3, sort),
         defaults = COALESCE(?4, defaults), updated_at = datetime('now') WHERE id = ?1",
    )
    .bind(id)
    .bind(&p.name)
    .bind(p.sort)
    .bind(defaults)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Delete a folder. `delete_threads` false: its threads stay, without a
/// folder. True: they are deleted with it (messages and attachments cascade).
/// One transaction either way. `false` when there is no such folder.
pub async fn delete_chat_folder(
    pool: &SqlitePool,
    id: i64,
    delete_threads: bool,
) -> DbResult<bool> {
    let mut tx = pool.begin().await?;
    if delete_threads {
        sqlx::query("DELETE FROM chat_threads WHERE folder_id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query("UPDATE chat_threads SET folder_id = NULL WHERE folder_id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    let res = sqlx::query("DELETE FROM chat_folders WHERE id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(res.rows_affected() > 0)
}

/// Move a thread into a folder (`None` = out of any). Does not touch
/// `updated_at`: filing a thread is not activity, and must not float it up or
/// reset the archive clock.
pub async fn set_chat_thread_folder(
    pool: &SqlitePool,
    thread_id: i64,
    folder_id: Option<i64>,
) -> DbResult<()> {
    sqlx::query("UPDATE chat_threads SET folder_id = ?2 WHERE id = ?1")
        .bind(thread_id)
        .bind(folder_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Insert a new thread built from `t`'s settings (model, prompt, sampling,
/// reasoning, MCP servers, knowledge bases, voice, `kind`, `folder_id`) in one statement, so a thread
/// seeded from a folder's defaults never exists half-configured. Title and
/// timestamps take the column defaults. Returns the new id.
pub async fn create_chat_thread_from(pool: &SqlitePool, t: &ChatThread) -> DbResult<i64> {
    let mcp = serde_json::to_string(&t.mcp_tools).unwrap_or_else(|_| "[]".into());
    let res = sqlx::query(
        "INSERT INTO chat_threads
           (model_alias, system_prompt, temperature, max_tokens, kind, mcp_tools,
            reasoning_enabled, reasoning_effort, reasoning_budget, folder_id,
            top_p, top_k, min_p, repeat_penalty, presence_penalty, frequency_penalty,
            seed, stop, kb_ids, kb_mode, kb_budget_tokens, voice)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                 ?19, ?20, ?21, ?22)",
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
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}
