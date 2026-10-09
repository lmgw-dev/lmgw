//! Chat threads + messages (the Chat tab)

use sqlx::{Row, SqlitePool};

use super::*;

/// One registered MCP server attached to a chat thread.
///
/// Deliberately the fields the Responses API's `{"type":"mcp"}` tool block
/// carries, because they resolve through the same
/// [`crate::mcp::exec::resolve`].
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ThreadMcp {
    /// A registered server's tool prefix (or its name when it has no prefix).
    /// Read trimmed, from a request and from a stored row alike: a label
    /// names what its trimmed text names (the tool-scope and approval
    /// checks read it so), so it is stored and resolved that way too.
    #[serde(deserialize_with = "trimmed_label")]
    pub server_label: String,
    /// `None` — the server's whole surface. `Some(list)` narrows it, so a
    /// thread pays prompt tokens only for the tools it might call.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Which of its tools wait for an approval before they run
    /// (client-apps design §6.1), in OpenAI's shapes as written: `"never"`,
    /// `"always"` or `{always: {tool_names}, never: {tool_names}}`, read by
    /// [`crate::mcp::spec::parse_require_approval`] and checked when it is
    /// written (`read_only` refused). `None` is `"never"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_approval: Option<serde_json::Value>,
}

/// A `server_label` with the spaces around it taken off.
fn trimmed_label<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let label = <String as serde::Deserialize>::deserialize(d)?;
    Ok(match label.trim() {
        t if t.len() == label.len() => label,
        t => t.to_string(),
    })
}

impl ThreadMcp {
    /// What [`Self::require_approval`] gates. A value that does not parse
    /// (written by hand into the database) gates every tool: the reading
    /// that fails closed.
    pub fn approval_rule(&self) -> crate::mcp::spec::ApprovalRule {
        crate::mcp::spec::parse_require_approval(self.require_approval.as_ref(), &self.server_label)
            .unwrap_or(crate::mcp::spec::ApprovalRule::Always)
    }
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
    /// The owner's approval floor (client-apps design §6.6): the
    /// `require_approval` of the entries the owner last wrote, per target —
    /// what a device's later write of `mcp_tools` may not go below, even
    /// after removing the entry. Only the owner's writes change it
    /// (`web::chat_tool_write`'s `approval` module); a thread made in a
    /// folder starts from the folder's, and a thread that leaves a folder
    /// takes the folder's rules for the targets it has none for
    /// (`chat_approval_floor::fold_folder_floor`). Never on the wire.
    #[serde(skip)]
    pub approval_floor: Vec<ThreadMcp>,
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
    /// The personality profile this thread talks with
    /// (personality-profiles design §1); `None` is none ("Default"), as is
    /// an id whose profile is gone (a temporary thread's, held in memory).
    pub profile_id: Option<i64>,
    /// Knowledge bases this thread uses on every turn (chat-complete design
    /// §9.3), by id in `knowledge.db`; how is `kb_mode`.
    pub kb_ids: Vec<i64>,
    pub kb_mode: KbMode,
    /// The auto-mode retrieval budget; `None` = the owner's
    /// `chat_kb_budget_tokens`.
    pub kb_budget_tokens: Option<i64>,
    /// The thread's own voice settings (chat-voice design §2.2); every
    /// absent field inherits Settings → Chat → Voice.
    pub voice: ThreadVoice,
    pub created_at: String,
    pub updated_at: String,
}

impl ChatThread {
    /// Whether this thread drives the self-admin plane (client-apps design
    /// L3, review W3-1): an Admin Chat thread, or one with the self-admin
    /// toolset attached ([`Self::reach_level`] above `0`).
    pub fn drives_self_admin(&self) -> bool {
        self.reach_level() > 0
    }

    /// How far into the self-admin plane it reaches (`self_admin_thread!`
    /// says the same in SQL): `2` Admin Chat, which no device sees; `1` the
    /// self-admin toolset attached, which only a device that may use lmgw's
    /// admin tools sees; `0` anything else. A reader sees it when
    /// [`AdminThreads::sees`] says so.
    pub fn reach_level(&self) -> u8 {
        if self.kind == "admin" {
            2
        } else if carries_self_admin(&self.mcp_tools) {
            1
        } else {
            0
        }
    }
}

/// Whether `tools` attach the self-admin toolset. Spaces around the label
/// are trimmed and nothing else, exactly as SQL's `trim()` in
/// `self_admin_thread!` trims (review W4-17): the two are one predicate.
pub fn carries_self_admin(tools: &[ThreadMcp]) -> bool {
    tools
        .iter()
        .any(|m| m.server_label.trim_matches(' ') == crate::mcp::exec::SELF_ADMIN_LABEL)
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
    /// A reply a fallback that cannot see answered: who, for which model,
    /// and what went to it in the images' place (`chat_turn::blind`).
    /// `None` on every other row, and then left out of the JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images_note: Option<String>,
    /// How the turn was spoken (chat-voice design §3); `None` for a typed
    /// turn.
    pub voice: Option<MessageVoice>,
    /// A gated turn's calls and their decisions (client-apps design §6):
    /// `None` on every other row. Not in the row's JSON: the thread's read
    /// lists the waiting calls as `pending_approvals` itself.
    #[serde(skip)]
    pub pending_approvals: Option<PendingApprovals>,
    /// A late MCP task result's facts (MCP Tasks design §2.2): `Some` on a
    /// row of role `tool`, `None` on every other row, and then left out of
    /// the JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<lmgw_api_types::chat::MessageTask>,
    pub created_at: String,
}

impl ChatMessageRow {
    /// A late MCP task result (MCP Tasks design §2.2): a row of role `tool`,
    /// the only rows that role names. Its task's facts are not asked for: a
    /// row whose `task` does not read is still placed as a result
    /// (`web::chat_tasks::render`).
    pub fn is_task_result(&self) -> bool {
        self.role == "tool"
    }
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
        approval_floor: super::chat_approval_floor::from_column(
            row.get::<String, _>("approval_floor").as_str(),
        ),
        reasoning_enabled: row
            .get::<Option<i64>, _>("reasoning_enabled")
            .map(|v| v != 0),
        reasoning_effort: row.get("reasoning_effort"),
        reasoning_budget: row.get("reasoning_budget"),
        agent_id: row.get("agent_id"),
        pinned: row.get::<i64, _>("pinned") != 0,
        archived_at: row.get("archived_at"),
        folder_id: row.get("folder_id"),
        profile_id: row.get("profile_id"),
        kb_ids: id_list(row.get::<String, _>("kb_ids").as_str()),
        kb_mode: KbMode::parse(row.get::<String, _>("kb_mode").as_str()).unwrap_or_default(),
        kb_budget_tokens: row.get("kb_budget_tokens"),
        voice: ThreadVoice::from_stored(row.get::<String, _>("voice").as_str()),
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
        images_note: row.get("images_note"),
        voice: MessageVoice::from_stored(row.get("voice")),
        pending_approvals: PendingApprovals::parse(
            row.get::<Option<String>, _>("pending_approvals").as_deref(),
        ),
        // A column this process wrote: one that does not parse reads as
        // none, and the row as the text it holds.
        task: row
            .get::<Option<String>, _>("task")
            .and_then(|t| serde_json::from_str(&t).ok()),
        created_at: row.get("created_at"),
    }
}

/// A new thread with no prompt, recorded in the feed as the gateway's own
/// (`by` none): the seed of tests and in-process callers.
pub async fn create_chat_thread(pool: &SqlitePool, model_alias: &str, kind: &str) -> DbResult<i64> {
    create_chat_thread_with_prompt(pool, model_alias, kind, "", None).await
}

/// [`create_chat_thread`] starting from `system_prompt` — the Chat page's
/// new threads take the configured default this way, as their own copy —
/// recorded in the feed (`thread.created`) as `by`'s.
pub async fn create_chat_thread_with_prompt(
    pool: &SqlitePool,
    model_alias: &str,
    kind: &str,
    system_prompt: &str,
    by: feed::By<'_>,
) -> DbResult<i64> {
    let mut tx = super::begin_write(pool).await?;
    let res = sqlx::query(
        "INSERT INTO chat_threads (model_alias, kind, system_prompt) VALUES (?1, ?2, ?3)",
    )
    .bind(model_alias)
    .bind(kind)
    .bind(system_prompt)
    .execute(&mut *tx)
    .await?;
    let id = res.last_insert_rowid();
    feed::record_thread(&mut tx, feed::kind::THREAD_CREATED, id, by).await?;
    tx.commit().await?;
    Ok(id)
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
    let mut tx = super::begin_write(pool).await?;
    let res = sqlx::query(
        // The catalog is the owner's: its tools' rules are the floor.
        "INSERT INTO chat_threads
           (model_alias, system_prompt, temperature, kind, mcp_tools, agent_id, approval_floor)
         VALUES (?1, ?2, ?3, 'chat', ?4, ?5, ?4)",
    )
    .bind(model_alias)
    .bind(system_prompt)
    .bind(temperature)
    .bind(mcp)
    .bind(agent_id)
    .execute(&mut *tx)
    .await?;
    let id = res.last_insert_rowid();
    // Opened from the catalog, which only the owner reaches.
    feed::record_thread(
        &mut tx,
        feed::kind::THREAD_CREATED,
        id,
        Some(feed::BY_OWNER),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
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

/// How far a reader reaches into the threads and folders that drive the
/// self-admin plane (client-apps design L3, review W3-1; the per-device
/// switch, 2026-10-07): the owner sees all of them; a device that may use
/// lmgw's admin tools sees the ones with the self-admin toolset, never Admin
/// Chat nor a folder a device deleted; any other device none of them — for
/// it they do not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminThreads {
    /// The owner.
    Shown,
    /// A device allowed lmgw's admin tools (`ApiKey::self_admin`).
    ToolsShown,
    /// Any other device.
    Hidden,
}

/// The level only the owner sees (`self_admin_thread!`'s `2`): the first a
/// device that may use lmgw's admin tools does not reach.
pub const OWNER_ONLY: i64 = AdminThreads::ToolsShown.reach();

impl AdminThreads {
    /// The first level the reader does not see (`self_admin_thread!`,
    /// `self_admin_folder!`): what a query's `… < ?n` binds.
    pub const fn reach(self) -> i64 {
        match self {
            Self::Shown => 3,
            Self::ToolsShown => 2,
            Self::Hidden => 1,
        }
    }

    /// Whether the reader sees a thread or folder of `level`.
    pub fn sees(self, level: impl Into<i64>) -> bool {
        level.into() < self.reach()
    }

    /// Whether the reader is a device (anything but the owner).
    pub fn is_device(self) -> bool {
        self != Self::Shown
    }

    /// A device's reach with its admin-tools switch `on` or off.
    pub fn of_device(on: bool) -> Self {
        if on {
            Self::ToolsShown
        } else {
            Self::Hidden
        }
    }

    /// The one of the two that sees less.
    pub fn narrower(self, other: Self) -> Self {
        if other.reach() < self.reach() {
            other
        } else {
            self
        }
    }
}

/// The sidebar list (chat-archive-pin-attachments design §1): active threads
/// pinned-first then most-recently-active, archived threads newest-archived-
/// first, or both together. Three orderings behind one enum rather than three
/// functions, because every caller already knows which list it wants and a
/// `WHERE`/`ORDER BY` clause is the whole difference. Every kind is listed;
/// [`list_chat_threads_as`] leaves Admin Chat out for a device.
pub async fn list_chat_threads(
    pool: &SqlitePool,
    mode: ThreadListMode,
) -> DbResult<Vec<ChatThread>> {
    list_chat_threads_as(pool, mode, AdminThreads::Shown).await
}

/// [`list_chat_threads`], as far as `admin` reaches.
pub async fn list_chat_threads_as(
    pool: &SqlitePool,
    mode: ThreadListMode,
    admin: AdminThreads,
) -> DbResult<Vec<ChatThread>> {
    let sql = match mode {
        ThreadListMode::Active => concat!(
            "SELECT * FROM chat_threads WHERE archived_at IS NULL AND ",
            self_admin_thread!(""),
            " < ?1 ORDER BY pinned DESC, updated_at DESC, id DESC"
        ),
        ThreadListMode::Archived => concat!(
            "SELECT * FROM chat_threads WHERE archived_at IS NOT NULL AND ",
            self_admin_thread!(""),
            " < ?1 ORDER BY archived_at DESC, id DESC"
        ),
        ThreadListMode::All => concat!(
            "SELECT * FROM chat_threads WHERE ",
            self_admin_thread!(""),
            " < ?1 ORDER BY pinned DESC, updated_at DESC, id DESC"
        ),
    };
    let rows = sqlx::query(sql).bind(admin.reach()).fetch_all(pool).await?;
    Ok(rows.iter().map(chat_thread_from_row).collect())
}

/// The stored threads among `ids` as far as `admin` reaches, active and
/// archived alike, in the active list's order (pinned first, then
/// `updated_at DESC, id DESC`): the Chat page re-reads only the rows a
/// change named (`GET /chat/api/threads/rows`). An id that is not there, or
/// out of reach, is left out; repeats are read once. One query however many
/// ids, bound as a JSON array.
pub async fn list_chat_threads_by_ids(
    pool: &SqlitePool,
    ids: &[i64],
    admin: AdminThreads,
) -> DbResult<Vec<ChatThread>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
    let rows = sqlx::query(concat!(
        "SELECT * FROM chat_threads WHERE id IN (SELECT value FROM json_each(?1)) AND ",
        self_admin_thread!(""),
        " < ?2 ORDER BY pinned DESC, updated_at DESC, id DESC"
    ))
    .bind(ids)
    .bind(admin.reach())
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(chat_thread_from_row).collect())
}

/// How many threads are archived, so the sidebar's toggle can label itself
/// ("Archived (n)") without a second round trip through the full list.
pub async fn count_archived_chat_threads(pool: &SqlitePool, admin: AdminThreads) -> DbResult<i64> {
    let n: i64 = sqlx::query_scalar(concat!(
        "SELECT COUNT(*) FROM chat_threads WHERE archived_at IS NOT NULL AND ",
        self_admin_thread!(""),
        " < ?1"
    ))
    .bind(admin.reach())
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

/// Delete a thread with everything in it, recorded in the feed
/// (`thread.deleted`) as `by`'s. A thread that is not there records nothing.
/// The current thread of an ongoing folder leaves it first (`folder.current`,
/// reason `gone`, client-apps design §3.3).
pub async fn delete_chat_thread(pool: &SqlitePool, id: i64, by: feed::By<'_>) -> DbResult<()> {
    let mut tx = super::begin_write(pool).await?;
    super::chat_ongoing::clear_current(&mut tx, id, None, by).await?;
    let gone: Vec<feed::Gone> = sqlx::query(
        "DELETE FROM chat_threads WHERE id = ?1 RETURNING id, kind, folder_id, mcp_tools",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?
    .iter()
    .map(feed::Gone::of)
    .collect();
    feed::record_threads_deleted(&mut tx, &gone, by).await?;
    // Their MCP tasks: an open one owes its cancel, a result goes
    // (MCP Tasks design §1.5).
    super::mcp_tasks::threads_gone(&mut tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Set a thread's pinned flag (design §1). Pinning an archived thread also
/// [`restore`](restore_chat_thread)s it — done here, in the same
/// transaction, so the API handler never forgets the half that makes "pin"
/// also mean "bring back".
pub async fn set_chat_thread_pinned(
    pool: &SqlitePool,
    id: i64,
    pinned: bool,
    by: feed::By<'_>,
) -> DbResult<()> {
    let mut tx = super::begin_write(pool).await?;
    sqlx::query("UPDATE chat_threads SET pinned=?2 WHERE id=?1")
        .bind(id)
        .bind(pinned as i64)
        .execute(&mut *tx)
        .await?;
    if pinned {
        sqlx::query(
            "UPDATE chat_threads SET archived_at=NULL, updated_at=datetime('now') \
             WHERE id=?1 AND archived_at IS NOT NULL",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    feed::record_thread(&mut tx, feed::kind::THREAD_UPDATED, id, by).await?;
    tx.commit().await?;
    Ok(())
}

/// Archive one thread by hand — the sweep's own bulk update lives in
/// [`sweep_chat_threads`]; this is the single-row explicit action behind
/// `POST /chat/api/threads/{id}/archive`. An ongoing folder's current thread
/// archived by hand is its current thread no more (`folder.current`, reason
/// `gone`, client-apps design §3.3).
///
/// Also unpins (review finding 6): pinned and archived used to be able to
/// coexist through this path (the sweep itself already excludes `pinned=1`
/// threads, but a hand archive did not), which left a thread that never
/// purges — and, worse, one whose *later* unpin would purge it immediately
/// against a stale `archived_at`. Archiving is a stronger statement than
/// pinning here: it wins.
pub async fn archive_chat_thread(pool: &SqlitePool, id: i64, by: feed::By<'_>) -> DbResult<()> {
    let mut tx = super::begin_write(pool).await?;
    sqlx::query("UPDATE chat_threads SET archived_at=datetime('now'), pinned=0 WHERE id=?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    feed::record_thread(&mut tx, feed::kind::THREAD_UPDATED, id, by).await?;
    super::chat_ongoing::clear_current(&mut tx, id, None, by).await?;
    tx.commit().await?;
    Ok(())
}

/// Bring an archived thread back: clears `archived_at` and bumps
/// `updated_at` (design §1) — without the bump, the next sweep would
/// re-archive it at once, since the idle clock never moved. The target of
/// three triggers: an explicit restore, pinning an archived thread
/// ([`set_chat_thread_pinned`]), and sending into one (`web::chat::send`).
pub async fn restore_chat_thread(pool: &SqlitePool, id: i64, by: feed::By<'_>) -> DbResult<()> {
    let mut tx = super::begin_write(pool).await?;
    sqlx::query("UPDATE chat_threads SET archived_at=NULL, updated_at=datetime('now') WHERE id=?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    feed::record_thread(&mut tx, feed::kind::THREAD_UPDATED, id, by).await?;
    tx.commit().await?;
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
/// their archive days, then delete threads archived past their purge days.
/// A thread in a folder with its own retention (client-apps design §11 Q2)
/// takes the folder's days, every other thread `archive_days` and
/// `purge_days`; `0` disables that step, for the folder or globally. Pinned
/// threads are exempt from both, and so is every ongoing folder's current
/// thread (client-apps design §3.5) — the `WHERE` clauses say so directly
/// rather than pre-filtering in Rust, so the two counts this returns are
/// exactly what changed.
///
/// Each half records what it changed in the feed, in its own transaction
/// (client-apps design §2.2): `thread.updated` for every thread it archived,
/// `thread.deleted` for every thread it purged, as the gateway's own work.
pub async fn sweep_chat_threads(
    pool: &SqlitePool,
    archive_days: i64,
    purge_days: i64,
) -> DbResult<(u64, u64)> {
    let (archived, purged) = sweep_chat_threads_ids(pool, archive_days, purge_days).await?;
    Ok((archived, purged.len() as u64))
}

/// [`sweep_chat_threads`], with the ids of the threads it purged: what the
/// upkeep ends the live turns and bound sessions of (`LiveTurns::purged`).
pub async fn sweep_chat_threads_ids(
    pool: &SqlitePool,
    archive_days: i64,
    purge_days: i64,
) -> DbResult<(u64, Vec<i64>)> {
    // A day count past what `datetime()` can subtract makes it NULL, so the
    // comparison never matches: "never" is the honest reading of it.
    let archived = {
        let mut tx = super::begin_write(pool).await?;
        let ids: Vec<i64> = sqlx::query_scalar(concat!(
            "UPDATE chat_threads AS t SET archived_at=datetime('now')
             WHERE t.pinned=0 AND t.archived_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM chat_folders c WHERE c.current_thread_id = t.id)
               AND ",
            folder_days!("archive_days"),
            " > 0
               AND t.updated_at < datetime('now', '-' || ",
            folder_days!("archive_days"),
            " || ' days')
             RETURNING id"
        ))
        .bind(archive_days.max(0))
        .fetch_all(&mut *tx)
        .await?;
        feed::record_threads_updated(&mut tx, &ids, None).await?;
        tx.commit().await?;
        ids.len() as u64
    };
    let purged = {
        let mut tx = super::begin_write(pool).await?;
        let gone: Vec<feed::Gone> = sqlx::query(concat!(
            "DELETE FROM chat_threads AS t
             WHERE t.pinned=0 AND t.archived_at IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM chat_folders c WHERE c.current_thread_id = t.id)
               AND ",
            folder_days!("purge_days"),
            " > 0
               AND t.archived_at < datetime('now', '-' || ",
            folder_days!("purge_days"),
            " || ' days')
             RETURNING id, kind, folder_id, mcp_tools"
        ))
        .bind(purge_days.max(0))
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(feed::Gone::of)
        .collect();
        feed::record_threads_deleted(&mut tx, &gone, None).await?;
        // Their MCP tasks: an open one owes its cancel, a result goes
        // (MCP Tasks design §1.5).
        super::mcp_tasks::threads_gone(&mut tx).await?;
        tx.commit().await?;
        gone.iter().map(|g| g.id).collect::<Vec<i64>>()
    };
    Ok((archived, purged))
}

/// Write a thread's editable settings as `t` holds them — title, model,
/// prompt, sampling, reasoning overrides, the MCP servers it attaches, its
/// knowledge bases, its voice and its profile — to the row `t.id` (the
/// conversation's settings form). A profile that is gone by the time the
/// write lands is written as none, as its delete would have left it. With [`SeedWrite::Keep`] the voice's seed is the one
/// stored when the write lands, not `t`'s copy: a seed drawn since `t` was
/// read survives (chat-voice design §2.2). The voice as stored, `None` when
/// there is no such row.
pub async fn update_chat_thread_settings(
    pool: &SqlitePool,
    t: &ChatThread,
    seed: SeedWrite,
    by: feed::By<'_>,
) -> DbResult<Option<ThreadVoice>> {
    Ok(write_settings_of(pool, t, seed, by, AdminThreads::Shown)
        .await?
        .map(|w| w.voice))
}

/// [`update_chat_thread_settings`] with what the write saw: the voice as
/// stored and the thread's level before and after (review W4-3). `None`
/// when there is no such row, or when the writer (`admin`, a device) does
/// not see the thread's level.
pub async fn write_settings_of(
    pool: &SqlitePool,
    t: &ChatThread,
    seed: SeedWrite,
    by: feed::By<'_>,
    admin: AdminThreads,
) -> DbResult<Option<SettingsWritten>> {
    let mut tx = super::begin_write(pool).await?;
    let written = write_chat_thread_settings(&mut tx, t, seed, by, admin).await?;
    tx.commit().await?;
    Ok(written)
}

/// What [`write_chat_thread_settings`] wrote: the voice as stored, and the
/// thread's level (`self_admin_thread!`) before and after the write — as the
/// write's own transaction saw it, so a caller decides a flip from the
/// store's state, not from what it read before (review W4-3).
#[derive(Debug, Clone)]
pub struct SettingsWritten {
    pub thread_id: i64,
    pub voice: ThreadVoice,
    pub level_before: u8,
    pub level_after: u8,
}

/// [`update_chat_thread_settings`]'s write and its record, on the caller's
/// transaction: a folder patch writes its current thread's settings with it
/// (client-apps design L9).
///
/// **The backstop of L3** (review W5-2). The write is a whole row, read by
/// the caller earlier; a writer that does not see the thread's level as
/// this transaction finds it (`admin`: a device) writes nothing, and gets
/// `None`. A device's stale copy can then never undo the owner's attach of
/// the toolset, whatever the route read before.
pub(super) async fn write_chat_thread_settings(
    conn: &mut sqlx::SqliteConnection,
    t: &ChatThread,
    seed: SeedWrite,
    by: feed::By<'_>,
    admin: AdminThreads,
) -> DbResult<Option<SettingsWritten>> {
    let mcp = serde_json::to_string(&t.mcp_tools).unwrap_or_else(|_| "[]".to_string());
    let mut voice = t.voice.clone();
    if seed == SeedWrite::Keep {
        voice.seed = None;
    }
    // SET expressions read the row as it was, so `voice` on the right is the
    // stored one: its seed is laid over the new object. The JSON functions
    // run only on valid JSON — a malformed stored `voice` makes them raise,
    // which `AND` does not prevent and a `CASE` does.
    // The stored seed is kept when the draw would keep it (`seed_held!`).
    let was = feed::self_admin_now(&mut *conn, t.id).await?;
    if was.is_some_and(|level| !admin.sees(level)) {
        return Ok(None);
    }
    let stored: Option<String> = sqlx::query_scalar(concat!(
        "UPDATE chat_threads SET title=?2, model_alias=?3, system_prompt=?4, temperature=?5,
         max_tokens=?6, mcp_tools=?7, reasoning_enabled=?8, reasoning_effort=?9,
         reasoning_budget=?10, top_p=?11, top_k=?12, min_p=?13, repeat_penalty=?14,
         presence_penalty=?15, frequency_penalty=?16, seed=?17, stop=?18,
         kb_ids=?19, kb_mode=?20, kb_budget_tokens=?21, approval_floor=?25,
         profile_id = (SELECT id FROM chat_profiles WHERE id = ?24),
         voice = CASE
           WHEN ?23 AND ",
        seed_held!(),
        "
           THEN json_set(?22, '$.seed', json_extract(voice, '$.seed'))
           ELSE ?22 END,
         updated_at=datetime('now') WHERE id=?1
         RETURNING voice",
    ))
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
    .bind(voice.to_stored())
    .bind(seed == SeedWrite::Keep)
    .bind(t.profile_id)
    .bind(super::chat_approval_floor::to_column(&t.approval_floor))
    .fetch_optional(&mut *conn)
    .await?;
    feed::record_thread_since(&mut *conn, feed::kind::THREAD_UPDATED, t.id, by, was).await?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let now = feed::self_admin_now(&mut *conn, t.id).await?;
    let level = |l: Option<i64>| u8::try_from(l.unwrap_or(0)).unwrap_or(2);
    Ok(Some(SettingsWritten {
        thread_id: t.id,
        voice: ThreadVoice::from_stored(&stored),
        level_before: level(was),
        level_after: level(now),
    }))
}

/// Set just the title (used to auto-name a thread from its first user
/// message), recorded in the feed as `by`'s.
pub async fn set_chat_thread_title(
    pool: &SqlitePool,
    id: i64,
    title: &str,
    by: feed::By<'_>,
) -> DbResult<()> {
    let mut tx = super::begin_write(pool).await?;
    sqlx::query("UPDATE chat_threads SET title=?2 WHERE id=?1")
        .bind(id)
        .bind(title)
        .execute(&mut *tx)
        .await?;
    feed::record_thread(&mut tx, feed::kind::THREAD_UPDATED, id, by).await?;
    tx.commit().await?;
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
