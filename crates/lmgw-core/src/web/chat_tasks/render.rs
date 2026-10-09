//! What the model sees of a late result (MCP Tasks design §2.2, §3.2).
//!
//! **The row.** A delivered result is a message of role `tool` whose
//! `ir_messages` hold a synthetic pair, written once: an assistant call of
//! `lmgw__job_result` (`{job, tool}`: the server's task id and the tool the
//! model called), id `lmgw_task_<row id>`, and its tool result — §1.4's
//! text, the line `job <task id> (<tool>) <status>` first. The name is in
//! lmgw's reserved namespace, so it never meets a real tool. Its `content`
//! is the same result as text, for every reader that takes no IR.
//!
//! **The placement: chronological** (the owner's decision of 2026-10-09,
//! design T11). A result row is replayed where it is stored among the
//! thread's messages: its call joins a directly preceding assistant
//! message (`chat_turn::merge`), else stands as an assistant message of its
//! own, its result follows, and whatever was stored after the row — a user
//! message, a reply — follows that. A heard turn's spoken words, which are
//! not stored yet, come last. Every result directly follows its call and
//! two assistant messages never meet; a user message may follow a result,
//! as the OpenAI, Anthropic and Gemini APIs take it (a template that refuses that is the
//! template's fault, not a reason to move the result). A row's place never
//! changes once it is stored, so every turn after it entered replays the
//! bytes the turn before sent. The first reply stored after a result answered it
//! ([`unanswered`]), whether or not a user message came between; an edit
//! that cuts the replies keeps the results (the turn after answers them
//! again).
//!
//! **A result first.** A history whose first message is a result row (the
//! messages before it deleted) would open the request with the synthetic
//! call, which Anthropic (a conversation opens with the user's turn) and
//! Gemini (a call follows a user turn or a function response) refuse. The
//! pair then follows a minimal user turn, [`OPENING`], so the request
//! opens with a user's turn as every other one does.

use lmgw_api_types::chat::MessageTask;
use serde_json::json;

use crate::ir::{flatten_tool_result, ContentPart, Message, Role, ToolResultBlock};
use crate::mcp::tasks::TaskStatus;
use crate::store::mcp_tasks::{McpTaskRow, ResultRow};
use crate::store::ChatMessageRow;

/// The synthetic call's name (§3.2): `lmgw__` is lmgw's own prefix, which
/// no server may take.
pub(crate) const JOB_RESULT: &str = "lmgw__job_result";

/// The user turn a pair that would open the request follows (module doc):
/// what happened, in lmgw's words, never the user's.
pub(crate) const OPENING: &str = "(the messages before this job's result were deleted)";

/// The result row task row `row` is delivered as (§2.2).
pub(crate) fn result_row(row: &McpTaskRow) -> ResultRow {
    let status = TaskStatus::parse(&row.status).unwrap_or(TaskStatus::Abandoned);
    let (blocks, structured_content) = row
        .result
        .as_deref()
        .and_then(crate::mcp::tasks::stored::decode)
        .unwrap_or_else(|| {
            // A result this process wrote always reads; one that does not
            // still says what ended.
            let said = ToolResultBlock::one(format!(
                "job {} ({}) {}",
                row.task_id,
                row.tool,
                status.as_str()
            ));
            (said, None)
        });
    let (content, _) = flatten_tool_result(&blocks);
    let id = format!("{}{}", crate::ir::SYNTHETIC_CALL_ID_PREFIX, row.id);
    let pair = vec![
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolUse {
                id: id.clone(),
                name: JOB_RESULT.to_string(),
                args: json!({"job": row.task_id, "tool": row.tool}),
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                id,
                name: Some(JOB_RESULT.to_string()),
                content: blocks,
                is_error: status != TaskStatus::Completed,
            }],
        },
    ];
    let task = MessageTask {
        task_id: row.task_id.clone(),
        server_label: row.server_label.clone(),
        tool: row.tool.clone(),
        status: status.as_str().to_string(),
        ended_by: row.ended_by.clone(),
        structured_content,
    };
    ResultRow {
        content,
        ir_messages: serde_json::to_string(&pair).unwrap_or_default(),
        task: serde_json::to_string(&task).unwrap_or_default(),
    }
}

/// A result row's pair (its stored `ir_messages`); `None` when it does not
/// read (a hand-edited row), and then the row replays as nothing.
pub(crate) fn pair_of(m: &ChatMessageRow) -> Option<Vec<Message>> {
    m.ir_messages
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<Message>>(raw).ok())
}

/// Whether some result in `history` waits for an answer: a result row no
/// assistant reply is stored after (a user message between answers
/// nothing).
pub(crate) fn unanswered(history: &[ChatMessageRow]) -> bool {
    match history.iter().rposition(ChatMessageRow::is_task_result) {
        Some(at) => !history[at..].iter().any(|m| m.role == "assistant"),
        None => false,
    }
}

#[cfg(test)]
mod tests;
