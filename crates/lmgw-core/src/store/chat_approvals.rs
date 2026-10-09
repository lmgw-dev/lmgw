//! A gated Chat turn's pending calls (client-apps design §6, L13):
//! `chat_messages.pending_approvals` (migration 0074) and the writes that
//! decide them.
//!
//! **What is stored.** A reply whose turn stopped on a gated call is saved
//! with its tool record ending in that turn's calls, no results after them,
//! and a [`PendingApprovals`] beside it: the key of the principal that
//! started the turn, the calls it stopped on — the gated ones and their
//! siblings, which wait with them (`agent::PendingCall`) — and every
//! decision made on the reply so far.
//!
//! **Deciding** ([`claim_approvals`]) is one write transaction: the decisions are
//! checked against the stored state and recorded, so of two clients
//! deciding at once one wins and the other is told who did. The resumed
//! turn then runs the calls and appends to the same reply
//! ([`resume_chat_reply`]).
//!
//! **A new message** declines what still waits ([`decline_waiting`]), inside the
//! message's own insert: every call of the record gets its result, so the
//! transcript stays valid for strict templates. A call decided but whose
//! result never reached the reply (its resumed turn was superseded or
//! failed before saving) is closed then too, approved ones saying they may
//! have run ([`APPROVED_UNSAVED`]).
//!
//! **A resumed turn that never started** — refused after the decision was
//! written, or stopped before its tool loop — closes its calls at once
//! ([`close_unrun`]): approved ones and their siblings say they were not
//! run ([`UNRUN`]), and the feed records that again for each approved one.
//!
//! Every decision is recorded in the feed (`approval.decided`) in the
//! write that makes it, as every waiting call is (`approval.requested`)
//! in the write that saves it.

use lmgw_api_types::chat_approvals::{ApprovalRequest, MOVED_ON};
use lmgw_api_types::mcp_host::{CallerKind, Principal as Who};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection, SqlitePool};

use super::DbResult;
use crate::agent::{DecidedCall, PendingCall};
use crate::ir::{ContentPart, Message, Role, ToolResultBlock};

/// A gated turn's stored state (module doc).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PendingApprovals {
    /// The key of the principal that started the turn: the resumed turn
    /// runs as it (L13). `None`: the gateway's own in-process run.
    #[serde(default)]
    pub key_id: Option<i64>,
    /// That key's name when the turn started, to name it when it is gone.
    #[serde(default)]
    pub key_name: Option<String>,
    /// The calls of the turn's last stop, in the model's order.
    #[serde(default)]
    pub calls: Vec<PendingCall>,
    /// Every decision made on this reply, oldest first.
    #[serde(default)]
    pub decided: Vec<Decision>,
}

/// One gated call, decided.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub call: PendingCall,
    pub approve: bool,
    #[serde(default)]
    pub reason: Option<String>,
    /// Who decided — for a call declined by a new message, who wrote it.
    pub by: Who,
    /// How the feed names them ("the dashboard", "device 'phone'").
    pub named: String,
    /// Declined because a new message came, not by a verdict.
    #[serde(default)]
    pub moved_on: bool,
    /// Made for a turn that never started ([`close_unrun`]): approved, the
    /// call never ran.
    #[serde(default)]
    pub not_run: bool,
}

impl Decision {
    /// Whether it lets the call run: approved, and its turn could start.
    pub fn runs(&self) -> bool {
        self.approve && !self.not_run
    }
}

/// Who decides: the principal, and how the feed names them.
#[derive(Debug, Clone, PartialEq)]
pub struct Decider {
    pub who: Who,
    pub named: String,
}

impl Decider {
    /// The gateway's own in-process writer (a test, a seed).
    pub fn gateway() -> Self {
        Self {
            who: Who {
                kind: CallerKind::Gateway,
                name: "lmgw".into(),
            },
            named: super::feed::BY_OWNER.to_string(),
        }
    }
}

/// One verdict a client sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub approval_request_id: String,
    pub approve: bool,
    pub reason: Option<String>,
}

/// Why decisions were refused; nothing was written.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// No verdict was sent.
    Empty,
    /// One was decided already, by `by` (the feed's name).
    Decided { id: String, by: String },
    /// No call of the thread waits under these ids.
    Unknown(Vec<String>),
    /// These waiting calls got no verdict.
    Missing(Vec<String>),
    /// The reply the calls belong to is no longer the thread's last
    /// message, so its turn cannot be resumed.
    MovedOn,
}

/// What a declined call's result says to the model, as `/v1/responses`
/// words it.
pub fn declined(reason: Option<&str>) -> String {
    match reason.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => format!("The user declined this tool call: {r}"),
        None => "The user declined this tool call.".to_string(),
    }
}

/// What a decided call's result says when its resumed turn ended before it
/// saved one (superseded, failed, stopped): it was to run, and may have.
pub const APPROVED_UNSAVED: &str = "decided to run, but the turn that ran it ended before its \
     result was saved, so it may or may not have run";

/// What an approved call's result says, and a sibling's beside it, when
/// the turn that was to run them never started ([`close_unrun`]).
pub const UNRUN: &str = "not run: the turn that was to run it once it was decided could not \
     start";

/// What a sibling's result says when the user moved on.
const SIBLING_MOVED_ON: &str = "not run: it waited beside a call that needed an approval, and the \
     user moved on without deciding";

/// The approval id of a gated call: `mcpr_` and 64 random bits, so an id
/// names one call in the gateway, whatever ids the model gave its calls.
pub fn mint_id() -> String {
    format!("mcpr_{:016x}", rand::random::<u64>())
}

/// The tool's own name: the name the model called without its label's
/// `<prefix>__` (realtime-server-tools decision 6).
pub fn wire_name(call: &PendingCall) -> String {
    call.name
        .strip_prefix(call.server_label.as_str())
        .and_then(|rest| rest.strip_prefix("__"))
        .unwrap_or(&call.name)
        .to_string()
}

/// A call as a client is shown it.
pub fn request_of(call: &PendingCall) -> ApprovalRequest {
    ApprovalRequest {
        approval_request_id: call.approval_id.clone(),
        server_label: call.server_label.clone(),
        name: wire_name(call),
        arguments: match &call.args {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        },
        call_id: Some(call.call_id.clone()),
    }
}

impl PendingApprovals {
    /// The stored column; `None` for none, or one that does not read.
    pub fn parse(raw: Option<&str>) -> Option<Self> {
        raw.and_then(|r| serde_json::from_str(r).ok())
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }

    /// The decision on `id`, when there is one.
    pub fn decision(&self, id: &str) -> Option<&Decision> {
        self.decided.iter().rev().find(|d| d.call.approval_id == id)
    }

    /// The gated calls that wait for a decision.
    pub fn open(&self) -> Vec<&PendingCall> {
        self.calls
            .iter()
            .filter(|c| c.needs_approval && self.decision(&c.approval_id).is_none())
            .collect()
    }

    pub fn is_open(&self) -> bool {
        !self.open().is_empty()
    }

    /// The waiting calls as a client is shown them.
    pub fn requests(&self) -> Vec<ApprovalRequest> {
        self.open().into_iter().map(request_of).collect()
    }

    /// The call `id` names, waiting or decided.
    pub fn call(&self, id: &str) -> Option<&PendingCall> {
        self.calls
            .iter()
            .find(|c| c.needs_approval && c.approval_id == id)
            .or_else(|| self.decision(id).map(|d| &d.call))
    }

    /// Whether any of `verdicts` names a call of this reply.
    fn names_any(&self, verdicts: &[Verdict]) -> bool {
        verdicts
            .iter()
            .any(|v| self.call(&v.approval_request_id).is_some())
    }

    /// Decide every waiting call with `verdicts`, as `by` (module doc):
    /// the calls the resumed turn settles first, in the model's order —
    /// each gated one with its verdict and approver, each sibling to run.
    pub fn decide(
        &mut self,
        verdicts: &[Verdict],
        by: &Decider,
    ) -> Result<Vec<DecidedCall>, Refusal> {
        if verdicts.is_empty() {
            return Err(Refusal::Empty);
        }
        for v in verdicts {
            if let Some(d) = self.decision(&v.approval_request_id) {
                return Err(Refusal::Decided {
                    id: v.approval_request_id.clone(),
                    by: d.named.clone(),
                });
            }
        }
        let open: Vec<String> = self.open().iter().map(|c| c.approval_id.clone()).collect();
        let unknown: Vec<String> = verdicts
            .iter()
            .map(|v| v.approval_request_id.clone())
            .filter(|id| !open.contains(id))
            .collect();
        if !unknown.is_empty() {
            return Err(Refusal::Unknown(unknown));
        }
        let missing: Vec<String> = open
            .iter()
            .filter(|id| !verdicts.iter().any(|v| &v.approval_request_id == *id))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(Refusal::Missing(missing));
        }
        let mut out = Vec::with_capacity(self.calls.len());
        for c in &self.calls {
            if !c.needs_approval {
                out.push(DecidedCall {
                    call: c.clone(),
                    approved: true,
                    denial: String::new(),
                    by: None,
                });
                continue;
            }
            // Every waiting call has one, checked above; the first of two
            // for one call is taken.
            let Some(v) = verdicts
                .iter()
                .find(|v| v.approval_request_id == c.approval_id)
            else {
                continue;
            };
            self.decided.push(Decision {
                call: c.clone(),
                approve: v.approve,
                reason: v.reason.clone(),
                by: by.who.clone(),
                named: by.named.clone(),
                moved_on: false,
                not_run: false,
            });
            out.push(DecidedCall {
                call: c.clone(),
                approved: v.approve,
                denial: declined(v.reason.as_deref()),
                by: Some(by.who.clone()),
            });
        }
        Ok(out)
    }

    /// Decline every waiting call because `by` wrote a new message; the
    /// ids declined.
    pub fn move_on(&mut self, by: &Decider) -> Vec<String> {
        let open: Vec<PendingCall> = self.open().into_iter().cloned().collect();
        for c in &open {
            self.decided.push(Decision {
                call: c.clone(),
                approve: false,
                reason: Some(MOVED_ON.to_string()),
                by: by.who.clone(),
                named: by.named.clone(),
                moved_on: true,
                not_run: false,
            });
        }
        open.into_iter().map(|c| c.approval_id).collect()
    }

    /// Mark the decisions on `ids` — a claim's, approved or declined — as
    /// made for a turn that never started (module doc); the ids marked.
    pub fn mark_unrun(&mut self, ids: &[String]) -> Vec<String> {
        let mut marked = Vec::new();
        for d in self.decided.iter_mut().rev() {
            if !d.moved_on
                && !d.not_run
                && ids.contains(&d.call.approval_id)
                && !marked.contains(&d.call.approval_id)
            {
                d.not_run = true;
                marked.push(d.call.approval_id.clone());
            }
        }
        marked
    }

    /// What the result of the record's `k`-th trailing call (`call_id`)
    /// says when no result of its own is there (module doc).
    fn closing_text(&self, k: usize, call_id: &str) -> String {
        let call = self
            .calls
            .iter()
            .find(|c| !call_id.is_empty() && c.call_id == call_id)
            .or_else(|| self.calls.get(k));
        let Some(call) = call else {
            return crate::agent::UNMADE_CALL.to_string();
        };
        if call.needs_approval {
            return match self.decision(&call.approval_id) {
                Some(d) if !d.approve => declined(d.reason.as_deref()),
                Some(d) if d.not_run => UNRUN.to_string(),
                Some(_) => APPROVED_UNSAVED.to_string(),
                None => declined(Some(MOVED_ON)),
            };
        }
        // A sibling runs once its turn is decided: by a verdict, it may have
        // run — unless that turn never started; by a new message, it never
        // did.
        let gated: Vec<&PendingCall> = self.calls.iter().filter(|c| c.needs_approval).collect();
        if gated
            .iter()
            .any(|g| self.decision(&g.approval_id).is_some_and(|d| d.not_run))
        {
            return UNRUN.to_string();
        }
        let by_verdict = gated
            .iter()
            .any(|g| self.decision(&g.approval_id).is_some_and(|d| !d.moved_on));
        if by_verdict {
            APPROVED_UNSAVED.to_string()
        } else {
            SIBLING_MOVED_ON.to_string()
        }
    }
}

/// `record` (a reply's `ir_messages`) with a result for every call of its
/// trailing assistant message that has none, worded by `pending` (module
/// doc); `None` when nothing needed one.
pub fn close_record(record: Option<&str>, pending: &PendingApprovals) -> Option<String> {
    let mut msgs: Vec<Message> = serde_json::from_str(record?).ok()?;
    let last = msgs.last().filter(|m| m.role == Role::Assistant)?;
    let results: Vec<ContentPart> = last
        .content
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolUse { id, name, .. } => Some((id, name)),
            _ => None,
        })
        .enumerate()
        .map(|(k, (id, name))| ContentPart::ToolResult {
            id: id.clone(),
            name: Some(name.clone()),
            content: ToolResultBlock::one(pending.closing_text(k, id)),
            is_error: true,
        })
        .collect();
    if results.is_empty() {
        return None;
    }
    msgs.push(Message {
        role: Role::Tool,
        content: results,
    });
    serde_json::to_string(&msgs).ok()
}

/// `record` closed already (a new message came while the decided calls'
/// turn had not started) with each result that says the call may have run
/// worded by `pending` instead (module doc); `None` when none says so.
fn reword_unsaved(record: Option<&str>, pending: &PendingApprovals) -> Option<String> {
    let mut msgs: Vec<Message> = serde_json::from_str(record?).ok()?;
    let n = msgs.len();
    if n < 2 || msgs[n - 1].role != Role::Tool || msgs[n - 2].role != Role::Assistant {
        return None;
    }
    let calls: Vec<String> = msgs[n - 2]
        .content
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    let mut changed = false;
    for part in msgs[n - 1].content.iter_mut() {
        let ContentPart::ToolResult { id, content, .. } = part else {
            continue;
        };
        let Some(k) = calls.iter().position(|c| c == id) else {
            continue;
        };
        let unsaved = matches!(content.as_slice(),
            [ToolResultBlock::Text { text }] if text == APPROVED_UNSAVED);
        if unsaved {
            *content = ToolResultBlock::one(pending.closing_text(k, id));
            changed = true;
        }
    }
    changed.then(|| serde_json::to_string(&msgs).ok()).flatten()
}

/// `record` once `pending`'s calls were marked not run: closed with
/// results saying so, or — closed by a new message meanwhile — reworded;
/// `None` when nothing changed.
pub fn unrun_record(record: Option<&str>, pending: &PendingApprovals) -> Option<String> {
    if record_open(record) {
        close_record(record, pending)
    } else {
        reword_unsaved(record, pending)
    }
}

/// Whether the record's trailing message is an assistant's with calls and
/// no results after them.
pub fn record_open(record: Option<&str>) -> bool {
    record
        .and_then(|r| serde_json::from_str::<Vec<Message>>(r).ok())
        .and_then(|m| m.last().cloned())
        .is_some_and(|m| {
            m.role == Role::Assistant
                && m.content
                    .iter()
                    .any(|p| matches!(p, ContentPart::ToolUse { .. }))
        })
}

// ---------------------------------------------------------------------------
// The feed
// ---------------------------------------------------------------------------

/// Record `kind` (`approval.requested` or `approval.decided`) for call
/// `approval_id` of reply `message_id` in thread `thread_id`, in this
/// transaction, at the thread's level (L3).
pub(super) async fn record(
    conn: &mut SqliteConnection,
    kind: &str,
    thread_id: i64,
    message_id: i64,
    approval_id: &str,
    by: Option<&str>,
) -> DbResult<()> {
    sqlx::query(concat!(
        "INSERT INTO chat_feed (type, thread_id, folder_id, admin, by, detail)
         SELECT ?1, id, folder_id, ",
        self_admin_thread!(""),
        ", ?2, json_object('message_id', ?4, 'approval_request_id', ?5)
         FROM chat_threads WHERE id = ?3"
    ))
    .bind(kind)
    .bind(by)
    .bind(thread_id)
    .bind(message_id)
    .bind(approval_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// `approval.requested` for every waiting call of `pending`.
pub(super) async fn record_requested(
    conn: &mut SqliteConnection,
    thread_id: i64,
    message_id: i64,
    pending: Option<&PendingApprovals>,
    by: Option<&str>,
) -> DbResult<()> {
    let Some(p) = pending else {
        return Ok(());
    };
    for c in p.open() {
        record(
            &mut *conn,
            super::feed::kind::APPROVAL_REQUESTED,
            thread_id,
            message_id,
            &c.approval_id,
            by,
        )
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// A new message is about to be written into `thread_id` by `by`: decline
/// what its last reply still waits on, and close its record (module doc).
/// Inside the message's own transaction.
pub(super) async fn decline_waiting(
    conn: &mut SqliteConnection,
    thread_id: i64,
    by: &Decider,
) -> DbResult<()> {
    let row = sqlx::query(
        "SELECT id, role, ir_messages, pending_approvals FROM chat_messages
         WHERE thread_id = ?1 ORDER BY id DESC LIMIT 1",
    )
    .bind(thread_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    if row.get::<String, _>("role") != "assistant" {
        return Ok(());
    }
    let raw: Option<String> = row.get("pending_approvals");
    let Some(mut pending) = PendingApprovals::parse(raw.as_deref()) else {
        return Ok(());
    };
    let record_raw: Option<String> = row.get("ir_messages");
    if !pending.is_open() && !record_open(record_raw.as_deref()) {
        return Ok(());
    }
    let id: i64 = row.get("id");
    let declined = pending.move_on(by);
    let record = close_record(record_raw.as_deref(), &pending).or(record_raw);
    sqlx::query("UPDATE chat_messages SET ir_messages = ?2, pending_approvals = ?3 WHERE id = ?1")
        .bind(id)
        .bind(&record)
        .bind(pending.to_json())
        .execute(&mut *conn)
        .await?;
    for a in &declined {
        record_decided(&mut *conn, thread_id, id, a, &by.named).await?;
    }
    Ok(())
}

async fn record_decided(
    conn: &mut SqliteConnection,
    thread_id: i64,
    message_id: i64,
    approval_id: &str,
    by: &str,
) -> DbResult<()> {
    record(
        conn,
        super::feed::kind::APPROVAL_DECIDED,
        thread_id,
        message_id,
        approval_id,
        Some(by),
    )
    .await
}

/// What [`claim_approvals`] decided.
#[derive(Debug, Clone)]
pub struct Claimed {
    /// The reply the decided calls belong to.
    pub message_id: i64,
    /// Its state with the decisions in.
    pub pending: PendingApprovals,
    /// What the resumed turn settles first.
    pub decided: Vec<DecidedCall>,
}

/// Decide calls of `thread_id` with `verdicts`, as `by` — one write: the
/// first decision wins (module doc). The reply is the newest one that
/// holds a call the verdicts name; only the thread's last message still
/// waits, so a call of an older one was declined when the user moved on.
pub async fn claim_approvals(
    pool: &SqlitePool,
    thread_id: i64,
    verdicts: &[Verdict],
    by: &Decider,
) -> DbResult<Result<Claimed, Refusal>> {
    let mut tx = super::begin_write(pool).await?;
    let rows = sqlx::query(
        "SELECT id, pending_approvals FROM chat_messages
         WHERE thread_id = ?1 AND pending_approvals IS NOT NULL ORDER BY id DESC",
    )
    .bind(thread_id)
    .fetch_all(&mut *tx)
    .await?;
    let found = rows.iter().find_map(|r| {
        let p =
            PendingApprovals::parse(r.get::<Option<String>, _>("pending_approvals").as_deref())?;
        p.names_any(verdicts).then(|| (r.get::<i64, _>("id"), p))
    });
    let Some((message_id, mut pending)) = found else {
        if verdicts.is_empty() {
            return Ok(Err(Refusal::Empty));
        }
        return Ok(Err(Refusal::Unknown(
            verdicts
                .iter()
                .map(|v| v.approval_request_id.clone())
                .collect(),
        )));
    };
    let decided = match pending.decide(verdicts, by) {
        Ok(d) => d,
        Err(refusal) => return Ok(Err(refusal)),
    };
    // Only a reply that is still the thread's last message can be resumed:
    // checked in this write, so a decision is never recorded for a turn
    // that cannot run.
    let last: Option<i64> =
        sqlx::query_scalar("SELECT MAX(id) FROM chat_messages WHERE thread_id = ?1")
            .bind(thread_id)
            .fetch_one(&mut *tx)
            .await?;
    if last != Some(message_id) {
        return Ok(Err(Refusal::MovedOn));
    }
    sqlx::query("UPDATE chat_messages SET pending_approvals = ?2 WHERE id = ?1")
        .bind(message_id)
        .bind(pending.to_json())
        .execute(&mut *tx)
        .await?;
    for d in decided.iter().filter(|d| d.by.is_some()) {
        record_decided(
            &mut tx,
            thread_id,
            message_id,
            &d.call.approval_id,
            &by.named,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Ok(Claimed {
        message_id,
        pending,
        decided,
    }))
}

/// Close the calls of reply `message_id` that `approved` names (approval
/// ids, approved by a claim) because the turn that was to run them never
/// started (module doc): each decision marked not run, the record given
/// results saying so — or, closed already by a new message, reworded — and
/// `approval.decided` recorded again for each, so the feed says what
/// happened. The ids named: those marked; none when the reply is gone or
/// they were marked already.
pub async fn close_unrun(
    pool: &SqlitePool,
    thread_id: i64,
    message_id: i64,
    approved: &[String],
) -> DbResult<Vec<String>> {
    let mut tx = super::begin_write(pool).await?;
    let row = sqlx::query(
        "SELECT ir_messages, pending_approvals FROM chat_messages
         WHERE id = ?1 AND thread_id = ?2 AND role = 'assistant'",
    )
    .bind(message_id)
    .bind(thread_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(Vec::new());
    };
    let Some(mut pending) =
        PendingApprovals::parse(row.get::<Option<String>, _>("pending_approvals").as_deref())
    else {
        return Ok(Vec::new());
    };
    let marked = pending.mark_unrun(approved);
    if marked.is_empty() {
        return Ok(Vec::new());
    }
    let raw: Option<String> = row.get("ir_messages");
    let closed = unrun_record(raw.as_deref(), &pending).or(raw);
    sqlx::query("UPDATE chat_messages SET ir_messages = ?2, pending_approvals = ?3 WHERE id = ?1")
        .bind(message_id)
        .bind(&closed)
        .bind(pending.to_json())
        .execute(&mut *tx)
        .await?;
    // The feed says it again for each approved one: it never ran.
    for id in &marked {
        let Some(d) = pending.decision(id).filter(|d| d.approve) else {
            continue;
        };
        let named = d.named.clone();
        record(
            &mut tx,
            super::feed::kind::APPROVAL_DECIDED,
            thread_id,
            message_id,
            id,
            Some(named.as_str()),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(marked)
}

/// Save a resumed turn onto reply `id`: its columns become `r` (the text
/// and record so far plus what the resumed turn added) and its pending
/// state `r.pending_approvals`, with `approval.requested` for a call the
/// resumed turn stopped on again. `false` — nothing written — when the
/// reply is gone.
pub async fn resume_chat_reply(
    pool: &SqlitePool,
    thread_id: i64,
    id: i64,
    r: &super::ChatReply,
    by: Option<&str>,
) -> DbResult<bool> {
    let mut tx = super::begin_write(pool).await?;
    let n = sqlx::query(
        "UPDATE chat_messages SET content=?3, reasoning=?4, prompt_tokens=?5,
           completion_tokens=?6, ir_messages=?7, model=?8, answered_by=?9, images_note=?10,
           pending_approvals=?11
         WHERE id=?1 AND thread_id=?2 AND role='assistant'",
    )
    .bind(id)
    .bind(thread_id)
    .bind(&r.content)
    .bind(&r.reasoning)
    .bind(r.prompt_tokens)
    .bind(r.completion_tokens)
    .bind(&r.ir_messages)
    .bind(&r.model)
    .bind(&r.answered_by)
    .bind(&r.images_note)
    .bind(r.pending_approvals.as_ref().map(PendingApprovals::to_json))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Ok(false);
    }
    record_requested(&mut tx, thread_id, id, r.pending_approvals.as_ref(), by).await?;
    sqlx::query("UPDATE chat_threads SET updated_at=datetime('now') WHERE id=?1")
        .bind(thread_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, gated: bool) -> PendingCall {
        PendingCall {
            approval_id: if gated {
                format!("mcpr_{id}")
            } else {
                id.into()
            },
            call_id: id.into(),
            name: "desktop__notify".into(),
            args: json!({"text": "hi"}),
            server_label: "desktop".into(),
            needs_approval: gated,
        }
    }

    fn device(name: &str) -> Decider {
        Decider {
            who: Who {
                kind: CallerKind::Device,
                name: name.into(),
            },
            named: format!("device '{name}'"),
        }
    }

    fn verdict(id: &str, approve: bool) -> Verdict {
        Verdict {
            approval_request_id: id.into(),
            approve,
            reason: (!approve).then(|| "not now".into()),
        }
    }

    fn pending() -> PendingApprovals {
        PendingApprovals {
            calls: vec![call("a", true), call("b", false), call("c", true)],
            ..Default::default()
        }
    }

    #[test]
    fn every_waiting_call_needs_a_verdict_and_the_first_decision_wins() {
        let mut p = pending();
        assert_eq!(p.requests().len(), 2);
        assert_eq!(p.requests()[0].name, "notify");
        assert_eq!(p.requests()[0].arguments, r#"{"text":"hi"}"#);
        assert_eq!(p.decide(&[], &device("phone")), Err(Refusal::Empty));
        assert_eq!(
            p.decide(&[verdict("mcpr_a", true)], &device("phone")),
            Err(Refusal::Missing(vec!["mcpr_c".into()]))
        );
        assert_eq!(
            p.decide(&[verdict("b", true)], &device("phone")),
            Err(Refusal::Unknown(vec!["b".into()])),
            "a sibling is no approval request"
        );
        let decided = p
            .decide(
                &[verdict("mcpr_a", true), verdict("mcpr_c", false)],
                &device("phone"),
            )
            .unwrap();
        assert_eq!(decided.len(), 3, "the sibling is settled with them");
        assert_eq!(
            decided[0].by.as_ref().map(|w| w.name.as_str()),
            Some("phone")
        );
        assert!(decided[1].approved && decided[1].by.is_none());
        assert!(!decided[2].approved);
        assert_eq!(
            decided[2].denial,
            "The user declined this tool call: not now"
        );
        assert!(!p.is_open());
        assert_eq!(
            p.decide(&[verdict("mcpr_a", false)], &device("desk")),
            Err(Refusal::Decided {
                id: "mcpr_a".into(),
                by: "device 'phone'".into()
            })
        );
    }

    #[test]
    fn moving_on_declines_what_waits_and_closes_the_record() {
        let record = serde_json::to_string(&vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::text("Let me check."),
                ContentPart::ToolUse {
                    id: "a".into(),
                    name: "desktop__notify".into(),
                    args: json!({}),
                },
                ContentPart::ToolUse {
                    id: "b".into(),
                    name: "desktop__notify".into(),
                    args: json!({}),
                },
                ContentPart::ToolUse {
                    id: "c".into(),
                    name: "desktop__notify".into(),
                    args: json!({}),
                },
            ],
        }])
        .unwrap();
        assert!(record_open(Some(&record)));
        let mut p = pending();
        assert_eq!(p.move_on(&device("phone")), vec!["mcpr_a", "mcpr_c"]);
        let closed = close_record(Some(&record), &p).unwrap();
        assert!(!record_open(Some(&closed)));
        let msgs: Vec<Message> = serde_json::from_str(&closed).unwrap();
        let texts: Vec<String> = msgs[1]
            .content
            .iter()
            .map(|p| match p {
                ContentPart::ToolResult { content, .. } => {
                    crate::ir::flatten_tool_result(content).0
                }
                _ => String::new(),
            })
            .collect();
        assert_eq!(
            texts,
            vec![
                "The user declined this tool call: the user moved on without deciding".to_string(),
                SIBLING_MOVED_ON.to_string(),
                "The user declined this tool call: the user moved on without deciding".to_string(),
            ]
        );

        // Decided by a verdict whose turn never saved: approved calls may
        // have run.
        let mut q = pending();
        q.decide(
            &[verdict("mcpr_a", true), verdict("mcpr_c", false)],
            &device("phone"),
        )
        .unwrap();
        assert!(q.move_on(&device("phone")).is_empty());
        let closed = close_record(Some(&record), &q).unwrap();
        let msgs: Vec<Message> = serde_json::from_str(&closed).unwrap();
        let first = match &msgs[1].content[0] {
            ContentPart::ToolResult { content, .. } => crate::ir::flatten_tool_result(content).0,
            _ => String::new(),
        };
        assert_eq!(first, APPROVED_UNSAVED);
    }
}
