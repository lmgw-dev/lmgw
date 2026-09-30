//! Chat threads + messages (the Chat tab)

use sqlx::{Row, SqlitePool};

use super::*;

/// One registered MCP server attached to a chat thread.
///
/// Deliberately the same two fields the Responses API's `{"type":"mcp"}` tool
/// block carries, because they resolve through the same
/// [`crate::mcp::exec::resolve`]. `require_approval` is *not* stored: the Chat
/// tab has no approval round trip, so a gated call would stop a run the UI
/// cannot resume — threads run their tools or don't attach the server.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ThreadMcp {
    /// A registered server's tool prefix (or its name when it has no prefix).
    pub server_label: String,
    /// `None` — the server's whole surface. `Some(list)` narrows it, so a
    /// thread pays prompt tokens only for the tools it might call.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
}

/// A persisted chat conversation with its own model + sampling settings.
///
/// `Default` so a thread built in memory (a temporary chat, design §7) names
/// only what it sets and picks up any column added later at its default.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ChatThread {
    pub id: i64,
    pub title: String,
    pub model_alias: String,
    pub system_prompt: String,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i64>,
    /// Sampling overrides, sent with every turn like `temperature`; `None` (an
    /// empty `stop`) leaves the route's default alone. Which of them a route
    /// can take is decided per send (`web::chat_sampling`).
    pub top_p: Option<f64>,
    pub top_k: Option<i64>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub seed: Option<i64>,
    pub stop: Vec<String>,
    /// `"chat"` (a plain model conversation) or `"admin"` (Admin Chat, wired to
    /// the `lmgw__*` self-admin tools through the agent loop).
    pub kind: String,
    /// Registered MCP servers whose tools this thread attaches. Empty is the
    /// plain 1:1 chat; non-empty routes the send through the agent loop.
    pub mcp_tools: Vec<ThreadMcp>,
    /// Reasoning overrides, sent with every turn at the `x-lmgw-reasoning*`
    /// headers' tier: `x-lmgw-reasoning` on/off, `-effort`, `-budget`. `None`
    /// leaves the route's default alone.
    pub reasoning_enabled: Option<bool>,
    pub reasoning_effort: Option<String>,
    pub reasoning_budget: Option<i64>,
    /// The catalog agent this thread was opened from (agent-catalog §2.5), so
    /// it lists under that agent's Runs tab. `None` for every thread opened
    /// from the Chat page, which is every thread that predates the catalog.
    pub agent_id: Option<String>,
    /// Sits above the date groups in the sidebar and never auto-archives or
    /// auto-purges (chat-archive-pin-attachments design §1).
    pub pinned: bool,
    /// When the sweep (or the owner, by hand) archived this thread. `None` =
    /// active. The purge clock, not `updated_at` (design §1).
    pub archived_at: Option<String>,
    /// The folder this thread sits in (chat-complete design §5); `None` = no
    /// folder. A temporary thread never has one.
    pub folder_id: Option<i64>,
    /// Knowledge bases this thread uses on every turn (chat-complete design
    /// §9.3), by id in `knowledge.db`; how is `kb_mode`.
    pub kb_ids: Vec<i64>,
    pub kb_mode: KbMode,
    /// The auto-mode retrieval budget; `None` = the owner's
    /// `chat_kb_budget_tokens`.
    pub kb_budget_tokens: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

/// One stored turn in a [`ChatThread`].
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ChatMessageRow {
    pub id: i64,
    pub thread_id: i64,
    pub role: String,
    pub content: String,
    /// Reasoning / "thinking" trace for reasoning models (empty otherwise).
    pub reasoning: String,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    /// The turn's IR messages as JSON, for an agentic (Admin Chat) turn: the
    /// assistant's tool calls and their results, which `content` alone cannot
    /// hold. `None` for a plain chat turn.
    pub ir_messages: Option<String>,
    /// Knowledge bases picked with `#` for this (user) message only
    /// (chat-complete design §9.3).
    pub kb_refs: Vec<i64>,
    /// The retrieval that ran for this user message, replayed with it on
    /// every later turn; `None` when none ran.
    pub context: Option<ChatContext>,
    /// A reply's model: the alias its turn asked for (review R1 item c).
    /// `None` on user messages and on replies saved before it was recorded.
    pub model: Option<String>,
    /// The alias that actually answered, when it was not `model` — a
    /// GPU-hold or outside-VRAM fallback, a ladder climb's fallback, a
    /// candidate alias's pick. `None` when `model` itself answered.
    pub answered_by: Option<String>,
    pub created_at: String,
}

fn chat_thread_from_row(row: &sqlx::sqlite::SqliteRow) -> ChatThread {
    ChatThread {
        id: row.get("id"),
        title: row.get("title"),
        model_alias: row.get("model_alias"),
        system_prompt: row.get("system_prompt"),
        temperature: row.get("temperature"),
        max_tokens: row.get("max_tokens"),
        top_p: row.get("top_p"),
        top_k: row.get("top_k"),
        min_p: row.get("min_p"),
        repeat_penalty: row.get("repeat_penalty"),
        presence_penalty: row.get("presence_penalty"),
        frequency_penalty: row.get("frequency_penalty"),
        seed: row.get("seed"),
        stop: serde_json::from_str(row.get::<String, _>("stop").as_str()).unwrap_or_default(),
        kind: row.get("kind"),
        // A column this process wrote, so a parse failure means the row was
        // hand-edited: the thread still opens, with no servers attached, rather
        // than the whole Chat tab failing to list.
        mcp_tools: serde_json::from_str::<Vec<ThreadMcp>>(
            row.get::<String, _>("mcp_tools").as_str(),
        )
        .unwrap_or_default(),
        reasoning_enabled: row
            .get::<Option<i64>, _>("reasoning_enabled")
            .map(|v| v != 0),
        reasoning_effort: row.get("reasoning_effort"),
        reasoning_budget: row.get("reasoning_budget"),
        agent_id: row.get("agent_id"),
        pinned: row.get::<i64, _>("pinned") != 0,
        archived_at: row.get("archived_at"),
        folder_id: row.get("folder_id"),
        kb_ids: id_list(row.get::<String, _>("kb_ids").as_str()),
        kb_mode: KbMode::parse(row.get::<String, _>("kb_mode").as_str()).unwrap_or_default(),
        kb_budget_tokens: row.get("kb_budget_tokens"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

pub(super) fn chat_message_from_row(row: &sqlx::sqlite::SqliteRow) -> ChatMessageRow {
    ChatMessageRow {
        id: row.get("id"),
        thread_id: row.get("thread_id"),
        role: row.get("role"),
        content: row.get("content"),
        reasoning: row.get("reasoning"),
        prompt_tokens: row.get("prompt_tokens"),
        completion_tokens: row.get("completion_tokens"),
        ir_messages: row.get("ir_messages"),
        kb_refs: id_list(row.get::<String, _>("kb_refs").as_str()),
        context: context_from_column(row.get("context")),
        model: row.get("model"),
        answered_by: row.get("answered_by"),
        created_at: row.get("created_at"),
    }
}

pub async fn create_chat_thread(pool: &SqlitePool, model_alias: &str, kind: &str) -> DbResult<i64> {
    create_chat_thread_with_prompt(pool, model_alias, kind, "").await
}

/// [`create_chat_thread`] starting from `system_prompt` — the Chat page's
/// new threads take the configured default this way, as their own copy.
pub async fn create_chat_thread_with_prompt(
    pool: &SqlitePool,
    model_alias: &str,
    kind: &str,
    system_prompt: &str,
) -> DbResult<i64> {
    let res = sqlx::query(
        "INSERT INTO chat_threads (model_alias, kind, system_prompt) VALUES (?1, ?2, ?3)",
    )
    .bind(model_alias)
    .bind(kind)
    .bind(system_prompt)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

/// Create a thread that belongs to a catalog agent (§2.5), seeded in one
/// statement.
///
/// [`create_chat_thread`] plus a settings patch would leave a window where the
/// thread exists with no prompt and no tools; a `chat` agent's whole content is
/// that prompt and those tools, so it is one insert. The title is left at the
/// column default so the Chat page still names the thread from its first
/// message.
pub async fn create_agent_chat_thread(
    pool: &SqlitePool,
    agent_id: &str,
    model_alias: &str,
    system_prompt: &str,
    temperature: Option<f64>,
    mcp_tools: &[ThreadMcp],
) -> DbResult<i64> {
    let mcp = serde_json::to_string(mcp_tools).unwrap_or_else(|_| "[]".to_string());
    let res = sqlx::query(
        "INSERT INTO chat_threads
           (model_alias, system_prompt, temperature, kind, mcp_tools, agent_id)
         VALUES (?1, ?2, ?3, 'chat', ?4, ?5)",
    )
    .bind(model_alias)
    .bind(system_prompt)
    .bind(temperature)
    .bind(mcp)
    .bind(agent_id)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

/// Which threads [`list_chat_threads`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadListMode {
    /// Active only: pinned first, then most-recently-active. The default list.
    Active,
    /// Archived only: most-recently-archived first.
    Archived,
    /// Active *and* archived together (`?archived=all` — the Agent Runs tab,
    /// chat-archive-pin-attachments review finding 1, needs a catalog agent's
    /// archived threads too): pinned first, then most-recently-active. An
    /// archived thread sorts by the same `updated_at` clock as an active one
    /// here, since archiving never touches it — only the `Archived`-only list
    /// re-sorts by `archived_at`.
    All,
}

/// The sidebar list (chat-archive-pin-attachments design §1): active threads
/// pinned-first then most-recently-active, archived threads newest-archived-
/// first, or both together. Three orderings behind one enum rather than three
/// functions, because every caller already knows which list it wants and a
/// `WHERE`/`ORDER BY` clause is the whole difference.
pub async fn list_chat_threads(
    pool: &SqlitePool,
    mode: ThreadListMode,
) -> DbResult<Vec<ChatThread>> {
    let sql = match mode {
        ThreadListMode::Active => {
            "SELECT * FROM chat_threads WHERE archived_at IS NULL \
             ORDER BY pinned DESC, updated_at DESC, id DESC"
        }
        ThreadListMode::Archived => {
            "SELECT * FROM chat_threads WHERE archived_at IS NOT NULL \
             ORDER BY archived_at DESC, id DESC"
        }
        ThreadListMode::All => {
            "SELECT * FROM chat_threads ORDER BY pinned DESC, updated_at DESC, id DESC"
        }
    };
    let rows = sqlx::query(sql).fetch_all(pool).await?;
    Ok(rows.iter().map(chat_thread_from_row).collect())
}

/// How many threads are archived, so the sidebar's toggle can label itself
/// ("Archived (n)") without a second round trip through the full list.
pub async fn count_archived_chat_threads(pool: &SqlitePool) -> DbResult<i64> {
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM chat_threads WHERE archived_at IS NOT NULL")
            .fetch_one(pool)
            .await?;
    Ok(n)
}

pub async fn get_chat_thread(pool: &SqlitePool, id: i64) -> DbResult<Option<ChatThread>> {
    let row = sqlx::query("SELECT * FROM chat_threads WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(chat_thread_from_row))
}

pub async fn delete_chat_thread(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("DELETE FROM chat_threads WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Set a thread's pinned flag (design §1). Pinning an archived thread also
/// [`restore`](restore_chat_thread)s it — done here, in the same statement's
/// spirit, so the API handler never forgets the half that makes "pin" also
/// mean "bring back".
pub async fn set_chat_thread_pinned(pool: &SqlitePool, id: i64, pinned: bool) -> DbResult<()> {
    sqlx::query("UPDATE chat_threads SET pinned=?2 WHERE id=?1")
        .bind(id)
        .bind(pinned as i64)
        .execute(pool)
        .await?;
    if pinned {
        sqlx::query(
            "UPDATE chat_threads SET archived_at=NULL, updated_at=datetime('now') \
             WHERE id=?1 AND archived_at IS NOT NULL",
        )
        .bind(id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Archive one thread by hand — the sweep's own bulk update lives in
/// [`sweep_chat_threads`]; this is the single-row explicit action behind
/// `POST /chat/api/threads/{id}/archive`.
///
/// Also unpins (review finding 6): pinned and archived used to be able to
/// coexist through this path (the sweep itself already excludes `pinned=1`
/// threads, but a hand archive did not), which left a thread that never
/// purges — and, worse, one whose *later* unpin would purge it immediately
/// against a stale `archived_at`. Archiving is a stronger statement than
/// pinning here: it wins.
pub async fn archive_chat_thread(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("UPDATE chat_threads SET archived_at=datetime('now'), pinned=0 WHERE id=?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Bring an archived thread back: clears `archived_at` and bumps
/// `updated_at` (design §1) — without the bump, the next sweep would
/// re-archive it at once, since the idle clock never moved. The target of
/// three triggers: an explicit restore, pinning an archived thread
/// ([`set_chat_thread_pinned`]), and sending into one (`web::chat::send`).
pub async fn restore_chat_thread(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("UPDATE chat_threads SET archived_at=NULL, updated_at=datetime('now') WHERE id=?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// `archived_at + purge_days`, in the same `datetime('now')` text shape every
/// other timestamp in this table already uses — `None` when `archived_at`
/// cannot be parsed (a hand-edited row), or when `purge_days` is too large to
/// add without overflowing (review finding 4: `chrono::Duration::days` and
/// `NaiveDateTime`'s `Add` both *panic* on overflow rather than returning an
/// error, so a setting like `99999999` used to 500 every listing that had an
/// archived thread). The checked forms return `None` instead — "never purges"
/// is the honest reading of a deadline too far out to represent anyway, and
/// it matches SQLite's own behavior for the same case: `sweep_chat_threads`'s
/// `datetime('now', '-N days')` quietly returns `NULL` for a `N` this large,
/// so the comparison it feeds never matches and the sweep purges nothing.
pub fn chat_thread_purge_at(archived_at: &str, purge_days: i64) -> Option<String> {
    let dt = chrono::NaiveDateTime::parse_from_str(archived_at, "%Y-%m-%d %H:%M:%S").ok()?;
    let delta = chrono::TimeDelta::try_days(purge_days)?;
    let purge_at = dt.checked_add_signed(delta)?;
    Some(purge_at.format("%Y-%m-%d %H:%M:%S").to_string())
}

/// The hourly janitor's chat sweep (design §1): archive threads idle past
/// `archive_days`, then delete threads archived past `purge_days`. Pinned
/// threads are exempt from both — the `WHERE` clauses say so directly rather
/// than pre-filtering in Rust, so the two counts this returns are exactly
/// what changed. Either half is `0` to disable that step; `(0, 0)` when both
/// disabled without a query.
pub async fn sweep_chat_threads(
    pool: &SqlitePool,
    archive_days: i64,
    purge_days: i64,
) -> DbResult<(u64, u64)> {
    let archived = if archive_days > 0 {
        sqlx::query(
            "UPDATE chat_threads SET archived_at=datetime('now')
             WHERE pinned=0 AND archived_at IS NULL
               AND updated_at < datetime('now', ?1)",
        )
        .bind(format!("-{archive_days} days"))
        .execute(pool)
        .await?
        .rows_affected()
    } else {
        0
    };
    let purged = if purge_days > 0 {
        sqlx::query(
            "DELETE FROM chat_threads
             WHERE pinned=0 AND archived_at IS NOT NULL
               AND archived_at < datetime('now', ?1)",
        )
        .bind(format!("-{purge_days} days"))
        .execute(pool)
        .await?
        .rows_affected()
    } else {
        0
    };
    Ok((archived, purged))
}

/// Write a thread's editable settings as `t` holds them — title, model,
/// prompt, sampling, reasoning overrides, the MCP servers it attaches and its
/// knowledge bases — to the row `t.id` (the conversation's settings form).
pub async fn update_chat_thread_settings(pool: &SqlitePool, t: &ChatThread) -> DbResult<()> {
    let mcp = serde_json::to_string(&t.mcp_tools).unwrap_or_else(|_| "[]".to_string());
    sqlx::query(
        "UPDATE chat_threads SET title=?2, model_alias=?3, system_prompt=?4, temperature=?5,
         max_tokens=?6, mcp_tools=?7, reasoning_enabled=?8, reasoning_effort=?9,
         reasoning_budget=?10, top_p=?11, top_k=?12, min_p=?13, repeat_penalty=?14,
         presence_penalty=?15, frequency_penalty=?16, seed=?17, stop=?18,
         kb_ids=?19, kb_mode=?20, kb_budget_tokens=?21,
         updated_at=datetime('now') WHERE id=?1",
    )
    .bind(t.id)
    .bind(&t.title)
    .bind(&t.model_alias)
    .bind(&t.system_prompt)
    .bind(t.temperature)
    .bind(t.max_tokens)
    .bind(mcp)
    .bind(t.reasoning_enabled.map(i64::from))
    .bind(&t.reasoning_effort)
    .bind(t.reasoning_budget)
    .bind(t.top_p)
    .bind(t.top_k)
    .bind(t.min_p)
    .bind(t.repeat_penalty)
    .bind(t.presence_penalty)
    .bind(t.frequency_penalty)
    .bind(t.seed)
    .bind(serde_json::to_string(&t.stop).unwrap_or_else(|_| "[]".to_string()))
    .bind(id_list_json(&t.kb_ids))
    .bind(t.kb_mode.as_str())
    .bind(t.kb_budget_tokens)
    .execute(pool)
    .await?;
    Ok(())
}

/// Set just the title (used to auto-name a thread from its first user message).
pub async fn set_chat_thread_title(pool: &SqlitePool, id: i64, title: &str) -> DbResult<()> {
    sqlx::query("UPDATE chat_threads SET title=?2 WHERE id=?1")
        .bind(id)
        .bind(title)
        .execute(pool)
        .await?;
    Ok(())
}

/// Append a message and bump the thread's `updated_at` so it floats to the top
/// of the sidebar. Token counts are `None` for user/system turns.
#[allow(clippy::too_many_arguments)]
pub async fn append_chat_message(
    pool: &SqlitePool,
    thread_id: i64,
    role: &str,
    content: &str,
    reasoning: &str,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    ir_messages: Option<&str>,
) -> DbResult<i64> {
    let res = sqlx::query(
        "INSERT INTO chat_messages
           (thread_id, role, content, reasoning, prompt_tokens, completion_tokens, ir_messages)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(thread_id)
    .bind(role)
    .bind(content)
    .bind(reasoning)
    .bind(prompt_tokens)
    .bind(completion_tokens)
    .bind(ir_messages)
    .execute(pool)
    .await?;
    sqlx::query("UPDATE chat_threads SET updated_at=datetime('now') WHERE id=?1")
        .bind(thread_id)
        .execute(pool)
        .await?;
    Ok(res.last_insert_rowid())
}

pub async fn list_chat_messages(
    pool: &SqlitePool,
    thread_id: i64,
) -> DbResult<Vec<ChatMessageRow>> {
    let rows = sqlx::query("SELECT * FROM chat_messages WHERE thread_id = ?1 ORDER BY id")
        .bind(thread_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(chat_message_from_row).collect())
}
