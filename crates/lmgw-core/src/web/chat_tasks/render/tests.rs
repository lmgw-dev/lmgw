//! The placement of late results (MCP Tasks design §3.2's table): each
//! where it is stored, chronologically; every rendered history passes the
//! wire check, a replay keeps the bytes the turn before it sent, and a
//! history without results renders as it always did.

use std::collections::HashMap;

use super::*;
use crate::ir::Message;
use crate::store::ChatMessageRow;
use crate::web::chat_turn::build_messages;

fn row(id: i64, role: &str, text: &str) -> ChatMessageRow {
    ChatMessageRow {
        id,
        thread_id: 1,
        role: role.into(),
        content: text.into(),
        ..Default::default()
    }
}

fn user(id: i64, text: &str) -> ChatMessageRow {
    row(id, "user", text)
}

fn reply(id: i64, text: &str) -> ChatMessageRow {
    row(id, "assistant", text)
}

/// The result row task `task` of tool `desktop__build` is delivered as,
/// with message id `id`.
fn result(id: i64, task: i64, status: &str, text: &str) -> ChatMessageRow {
    let ended = McpTaskRow {
        id: task,
        server_id: 3,
        server_label: "desktop".into(),
        task_id: format!("t{task}"),
        thread_id: Some(1),
        tool: "desktop__build".into(),
        call_id: "call_1".into(),
        started_by: None,
        state: "ended".into(),
        status: status.into(),
        status_message: None,
        poll_interval_ms: None,
        ttl_ms: None,
        ended_by: None,
        result: Some(
            serde_json::to_string(&ToolResultBlock::one(format!(
                "job t{task} (desktop__build) {status}\n{text}"
            )))
            .unwrap(),
        ),
        created_at: String::new(),
        updated_at: String::new(),
    };
    let r = result_row(&ended);
    ChatMessageRow {
        id,
        thread_id: 1,
        role: "tool".into(),
        content: r.content,
        ir_messages: Some(r.ir_messages),
        task: serde_json::from_str(&r.task).ok(),
        ..Default::default()
    }
}

fn built(history: &[ChatMessageRow]) -> Vec<Message> {
    build_messages("Be brief.", (history, None), (&HashMap::new(), false))
}

/// The roles in order, a call shown as `A[call …]` and a result as
/// `T[… for <id>]`.
fn shape(msgs: &[Message]) -> Vec<String> {
    msgs.iter()
        .map(|m| {
            let calls: Vec<&str> = m
                .content
                .iter()
                .filter_map(|p| match p {
                    ContentPart::ToolUse { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect();
            let text = m.joined_text();
            match m.role {
                Role::System => "S".into(),
                Role::User => format!("U:{text}"),
                Role::Assistant if calls.is_empty() => format!("A:{text}"),
                Role::Assistant => format!("A:{text}[call {}]", calls.join(",")),
                Role::Tool => {
                    let ids: Vec<&str> = m
                        .content
                        .iter()
                        .filter_map(|p| match p {
                            ContentPart::ToolResult { id, .. } => Some(id.as_str()),
                            _ => None,
                        })
                        .collect();
                    format!("T[{}]", ids.join(","))
                }
            }
        })
        .collect()
}

/// What the OpenAI, Anthropic and Gemini wires need, and the chat
/// templates that follow them: every result directly after the assistant
/// message that called it (other results of that call between), and no two
/// assistant messages in a row. A user message after a tool result is
/// valid (the owner's decision of 2026-10-09, design T11): a template that
/// refuses it is fixed, not worked around.
pub(crate) fn assert_strict(msgs: &[Message]) {
    let mut open: Vec<String> = Vec::new();
    for (i, m) in msgs.iter().enumerate() {
        let prev = i.checked_sub(1).map(|p| msgs[p].role);
        match m.role {
            Role::Assistant => {
                assert_ne!(prev, Some(Role::Assistant), "two assistants: {msgs:#?}");
                open = m
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::ToolUse { id, .. } => Some(id.clone()),
                        _ => None,
                    })
                    .collect();
                continue;
            }
            Role::Tool => {
                for p in &m.content {
                    if let ContentPart::ToolResult { id, .. } = p {
                        assert!(
                            open.contains(id),
                            "result {id} does not follow its call: {msgs:#?}"
                        );
                    }
                }
                continue;
            }
            Role::User | Role::System => {}
        }
        open.clear();
    }
}

/// `before` is a prefix of `after`, byte for byte as the request carries
/// them.
fn assert_prefix(before: &[Message], after: &[Message]) {
    let a = serde_json::to_string(before).unwrap();
    let b = serde_json::to_string(&after[..before.len()]).unwrap();
    assert_eq!(a, b, "the replay changed what the turn before it sent");
}

/// The owner's case (design T11): a result entered the idle thread, then
/// the user wrote. The pair stays where it entered, joined to the reply
/// before it, and the new message follows it: the model answers the user,
/// the result before it as context.
#[test]
fn a_send_after_a_result_follows_the_pair() {
    let h = vec![
        user(1, "build it"),
        reply(2, "Started."),
        result(3, 41, "completed", "42 files"),
        user(4, "and?"),
    ];
    let m = built(&h);
    assert_eq!(
        shape(&m),
        [
            "S",
            "U:build it",
            "A:Started.[call lmgw_task_41]",
            "T[lmgw_task_41]",
            "U:and?"
        ]
    );
    assert_strict(&m);
    // The reply saved after the message answered both; the next send
    // replays the same bytes.
    let mut later = h.clone();
    later.push(reply(5, "It built 42 files."));
    later.push(user(6, "thanks"));
    let m2 = built(&later);
    assert_eq!(
        shape(&m2)[2..],
        [
            "A:Started.[call lmgw_task_41]",
            "T[lmgw_task_41]",
            "U:and?",
            "A:It built 42 files.",
            "U:thanks"
        ]
    );
    assert_strict(&m2);
    assert_prefix(&m, &m2);
}

/// A result that entered at the start of an edit's or a regenerate's turn
/// (design §3.1; a send's enters before its message) is stored after that
/// turn's user message, and renders there: the request ends with it, and
/// a later send replays it in place.
#[test]
fn a_result_that_entered_at_a_turn_s_start_follows_its_message() {
    let h = vec![
        user(1, "build it"),
        reply(2, "Started."),
        user(3, "and?"),
        result(4, 41, "completed", "42 files"),
    ];
    let m = built(&h);
    assert_eq!(
        shape(&m)[1..],
        [
            "U:build it",
            "A:Started.",
            "U:and?",
            "A:[call lmgw_task_41]",
            "T[lmgw_task_41]"
        ]
    );
    assert_strict(&m);
    let mut later = h.clone();
    later.push(reply(5, "It built 42 files."));
    later.push(user(6, "thanks"));
    let m2 = built(&later);
    assert_strict(&m2);
    assert_prefix(&m, &m2);
    assert_eq!(shape(&m2)[6..], ["A:It built 42 files.", "U:thanks"]);
}

#[test]
fn a_continuation_joins_the_call_to_the_reply_before_it() {
    let h = vec![
        user(1, "build it"),
        reply(2, "Started."),
        result(3, 41, "completed", "42 files"),
    ];
    let m = built(&h);
    assert_eq!(
        shape(&m),
        [
            "S",
            "U:build it",
            "A:Started.[call lmgw_task_41]",
            "T[lmgw_task_41]"
        ]
    );
    assert_strict(&m);
    let mut later = h.clone();
    later.push(reply(4, "Done: 42 files."));
    later.push(user(5, "good"));
    let m2 = built(&later);
    assert_strict(&m2);
    assert_prefix(&m, &m2);
}

#[test]
fn adjacent_results_render_in_their_order() {
    let h = vec![
        user(1, "two jobs"),
        reply(2, "Both started."),
        result(3, 41, "completed", "one"),
        result(4, 42, "failed", "two"),
        user(5, "status?"),
    ];
    let m = built(&h);
    assert_eq!(
        shape(&m)[2..],
        [
            "A:Both started.[call lmgw_task_41]",
            "T[lmgw_task_41]",
            "A:[call lmgw_task_42]",
            "T[lmgw_task_42]",
            "U:status?"
        ]
    );
    assert_strict(&m);
    // A failed job's result says so to the model.
    let Some(ContentPart::ToolResult { is_error, .. }) = m[5].content.first() else {
        panic!("a result: {m:#?}")
    };
    assert!(is_error);
}

#[test]
fn a_regenerated_reply_answers_the_result_again() {
    // `U1 A1 R U2 A2`, A2 regenerated: A2 goes, R stays where it is and
    // the new reply answers it with U2.
    let h = vec![
        user(1, "go"),
        reply(2, "Started."),
        result(3, 41, "completed", "ok"),
        user(4, "and?"),
    ];
    let m = built(&h);
    assert_eq!(
        shape(&m)[2..],
        ["A:Started.[call lmgw_task_41]", "T[lmgw_task_41]", "U:and?"]
    );
    assert!(unanswered(&h));
    assert_strict(&m);
    // `U1 R` after the reply that started it was regenerated away.
    let cut = vec![user(1, "go"), result(3, 41, "completed", "ok")];
    let m = built(&cut);
    assert_eq!(
        shape(&m)[1..],
        ["U:go", "A:[call lmgw_task_41]", "T[lmgw_task_41]"]
    );
    assert_strict(&m);
}

/// A heard turn's spoken words are this turn's, not stored yet: they
/// follow every stored row, a result too, as the row they become will.
#[test]
fn a_heard_turn_s_words_follow_a_result_stored_before_them() {
    let h = vec![
        user(1, "go"),
        reply(2, "Started."),
        result(3, 41, "completed", "ok"),
    ];
    let spoken = [ContentPart::text("what came out?")];
    let m = build_messages("", (&h, Some(&spoken)), (&HashMap::new(), false));
    assert_eq!(
        shape(&m),
        [
            "U:go",
            "A:Started.[call lmgw_task_41]",
            "T[lmgw_task_41]",
            "U:what came out?"
        ]
    );
    assert_strict(&m);
}

#[test]
fn a_tool_turn_s_record_before_a_result_stays_valid() {
    // A1 ran a tool and said nothing after it: its record ends with the
    // tool's result, and the late call follows as a message of its own.
    let record = serde_json::to_string(&vec![
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolUse {
                id: "call_1".into(),
                name: "desktop__build".into(),
                args: serde_json::json!({}),
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                id: "call_1".into(),
                name: Some("desktop__build".into()),
                content: ToolResultBlock::one("started, job t41"),
                is_error: false,
            }],
        },
    ])
    .unwrap();
    let mut a1 = reply(2, "");
    a1.ir_messages = Some(record);
    let h = vec![user(1, "go"), a1, result(3, 41, "completed", "ok")];
    let m = built(&h);
    assert_eq!(
        shape(&m)[1..],
        [
            "U:go",
            "A:[call call_1]",
            "T[call_1]",
            "A:[call lmgw_task_41]",
            "T[lmgw_task_41]"
        ]
    );
    assert_strict(&m);
}

#[test]
fn a_history_without_results_is_untouched() {
    let h = vec![
        user(1, "hi"),
        reply(2, "hello"),
        user(3, "more"),
        user(4, "and more"),
    ];
    assert_eq!(
        shape(&built(&h)),
        ["S", "U:hi", "A:hello", "U:more\n\nand more"]
    );
    assert!(!unanswered(&h));
}

/// The first reply stored after a result answers it, a user message
/// between or not.
#[test]
fn a_result_is_unanswered_until_a_reply_follows_it() {
    let mut h = vec![
        user(1, "go"),
        reply(2, "Started."),
        result(3, 41, "completed", "ok"),
    ];
    assert!(unanswered(&h));
    h.push(user(4, "and?"));
    assert!(unanswered(&h));
    h.push(reply(5, "It worked."));
    assert!(!unanswered(&h));
}

#[test]
fn the_row_holds_the_text_the_pair_and_the_facts() {
    let r = result(3, 41, "cancelled", "cancelled by the dashboard");
    assert_eq!(
        r.content,
        "job t41 (desktop__build) cancelled\ncancelled by the dashboard"
    );
    let task = r.task.as_ref().unwrap();
    assert_eq!(
        (task.task_id.as_str(), task.status.as_str()),
        ("t41", "cancelled")
    );
    let pair = pair_of(&r).unwrap();
    let ContentPart::ToolUse { name, args, .. } = &pair[0].content[0] else {
        panic!("{pair:?}")
    };
    assert_eq!(name, JOB_RESULT);
    assert_eq!(
        args,
        &serde_json::json!({"job": "t41", "tool": "desktop__build"})
    );
}

/// A result whose `task` facts do not read is still a result row, placed
/// as one: its role says so.
#[test]
fn a_result_whose_facts_do_not_read_is_still_placed() {
    let mut r = result(3, 1, "completed", "42 files");
    r.task = None;
    let history = [user(1, "build it"), reply(2, "Started."), r];
    assert!(history[2].is_task_result());
    let msgs = built(&history);
    assert_strict(&msgs);
    assert_eq!(
        shape(&msgs),
        [
            "S",
            "U:build it",
            "A:Started.[call lmgw_task_1]",
            "T[lmgw_task_1]"
        ]
    );
    assert!(unanswered(&history));
}

/// A result left first by a delete opens with a user turn saying so,
/// whatever follows it: no request starts with the synthetic call.
#[test]
fn a_result_left_first_follows_a_user_turn() {
    let alone = [result(3, 1, "completed", "42 files")];
    let msgs = built(&alone);
    assert_strict(&msgs);
    let opening = format!("U:{OPENING}");
    assert_eq!(
        shape(&msgs),
        [
            "S",
            opening.as_str(),
            "A:[call lmgw_task_1]",
            "T[lmgw_task_1]"
        ]
    );
    let answered = [result(3, 1, "completed", "42 files"), reply(4, "Built.")];
    let msgs = built(&answered);
    assert_strict(&msgs);
    assert_eq!(
        shape(&msgs),
        [
            "S",
            opening.as_str(),
            "A:[call lmgw_task_1]",
            "T[lmgw_task_1]",
            "A:Built."
        ]
    );
    // The user wrote after it: the message follows the result.
    let asked = [result(3, 1, "completed", "42 files"), user(4, "and?")];
    let msgs = built(&asked);
    assert_strict(&msgs);
    assert_eq!(
        shape(&msgs)[1..],
        [
            opening.as_str(),
            "A:[call lmgw_task_1]",
            "T[lmgw_task_1]",
            "U:and?"
        ]
    );
    // With no system prompt either.
    let bare = build_messages("", (&alone, None), (&HashMap::new(), false));
    assert_eq!(bare[0].role, Role::User);
}

/// A result whose structured content the model was not given (its content
/// said it) carries it for the thread's readers on the row's `task`; one
/// stored by an earlier lmgw (the bare block array) reads as before.
#[test]
fn a_result_row_carries_the_structured_content_for_its_readers() {
    let blocks = ToolResultBlock::one("job t9 (desktop__build) completed\n42 files");
    let mut ended = McpTaskRow {
        id: 9,
        server_id: 3,
        server_label: "desktop".into(),
        task_id: "t9".into(),
        thread_id: Some(1),
        tool: "desktop__build".into(),
        call_id: "call_1".into(),
        started_by: None,
        state: "ended".into(),
        status: "completed".into(),
        status_message: None,
        poll_interval_ms: None,
        ttl_ms: None,
        ended_by: None,
        result: Some(crate::mcp::tasks::stored::encode(
            &blocks,
            Some(&serde_json::json!({"files": 42})),
        )),
        created_at: String::new(),
        updated_at: String::new(),
    };
    let r = result_row(&ended);
    assert_eq!(r.content, "job t9 (desktop__build) completed\n42 files");
    let task: MessageTask = serde_json::from_str(&r.task).unwrap();
    assert_eq!(
        task.structured_content,
        Some(serde_json::json!({"files": 42}))
    );
    assert!(!r.ir_messages.contains("\"files\":42"), "{}", r.ir_messages);

    ended.result = Some(serde_json::to_string(&blocks).unwrap());
    let r = result_row(&ended);
    let task: MessageTask = serde_json::from_str(&r.task).unwrap();
    assert_eq!(task.structured_content, None);
    assert_eq!(r.content, "job t9 (desktop__build) completed\n42 files");
}
