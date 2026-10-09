//! The owed set and what a read says (module doc of `super`): new result
//! rows past the last read, the results no reply follows, a saved reply's
//! cut and a read that stands against it, a first read that says nothing,
//! and the facts of `lmgw.task.done`.

use super::*;
use crate::ir::ToolResultBlock;
use crate::store::mcp_tasks::McpTaskRow;

fn row(id: i64, role: &str) -> ChatMessageRow {
    ChatMessageRow {
        id,
        thread_id: 7,
        role: role.into(),
        content: format!("{role} {id}"),
        ..Default::default()
    }
}

/// The result row of task row `task` (`t<task>`), message id `id`.
fn result(id: i64, task: i64, ended_by: Option<&str>) -> ChatMessageRow {
    let ended = McpTaskRow {
        id: task,
        server_id: 3,
        server_label: "desktop".into(),
        task_id: format!("t{task}"),
        thread_id: Some(7),
        tool: "desktop__build".into(),
        call_id: "call_1".into(),
        started_by: None,
        state: "ended".into(),
        status: if ended_by.is_some() {
            "cancelled"
        } else {
            "completed"
        }
        .into(),
        status_message: None,
        poll_interval_ms: None,
        ttl_ms: None,
        ended_by: ended_by.map(str::to_string),
        result: Some(serde_json::to_string(&ToolResultBlock::one("done")).unwrap()),
        created_at: String::new(),
        updated_at: String::new(),
    };
    let r = render::result_row(&ended);
    ChatMessageRow {
        id,
        thread_id: 7,
        role: "tool".into(),
        content: r.content,
        ir_messages: Some(r.ir_messages),
        task: serde_json::from_str(&r.task).ok(),
        ..Default::default()
    }
}

fn ids(rows: &[&ChatMessageRow]) -> Vec<i64> {
    rows.iter().map(|m| m.id).collect()
}

/// What `store::mcp_tasks::results_read` finds in `history` for a reader
/// at watermark `since`, its sequence at `watermark` (the store's own test
/// holds the query to this).
fn found(history: &[ChatMessageRow], since: Option<i64>, watermark: i64) -> ResultsRead {
    let last_reply = history
        .iter()
        .filter(|m| m.role == "assistant")
        .map(|m| m.id)
        .max()
        .unwrap_or_default();
    let bound = last_reply.min(since.unwrap_or(i64::MAX));
    ResultsRead {
        last_reply,
        watermark,
        any: history.iter().any(ChatMessageRow::is_task_result),
        rows: history
            .iter()
            .filter(|m| m.is_task_result() && m.id > bound)
            .cloned()
            .collect(),
    }
}

/// `owed` reads `history`, the largest id handed out `watermark`: the
/// ids of the results it says.
fn read_at(owed: &mut Owed, history: &[ChatMessageRow], watermark: i64) -> Vec<i64> {
    let found = found(history, owed.seen_through, watermark);
    ids(&owed.take(&found))
}

/// [`read_at`] with no row deleted past the newest.
fn read(owed: &mut Owed, history: &[ChatMessageRow]) -> Vec<i64> {
    let newest = history.iter().map(|m| m.id).max().unwrap_or_default();
    read_at(owed, history, newest)
}

fn owing(owed: &Owed) -> Vec<i64> {
    owed.results.iter().copied().collect()
}

#[test]
fn a_read_says_the_results_past_the_last_and_owes_the_unanswered() {
    let mut owed = Owed::default();
    // At the bind: one result answered, one not — owed, said by nobody
    // (the feed did), and not new to a later read.
    let bind = [
        row(1, "user"),
        row(2, "assistant"),
        result(3, 41, None),
        row(4, "assistant"),
        result(5, 42, None),
    ];
    assert!(
        read(&mut owed, &bind).is_empty(),
        "the bind's read says none"
    );
    assert_eq!(owing(&owed), [5]);
    assert!(!owed.is_empty());

    // A second result entered: only it is new; both are owed.
    let later: Vec<ChatMessageRow> = bind.iter().cloned().chain([result(6, 43, None)]).collect();
    assert_eq!(read(&mut owed, &later), [6]);
    assert_eq!(owing(&owed), [5, 6]);

    // A reply saved after them answers both; a result after that stays.
    let mut answered = later.clone();
    answered.push(row(7, "assistant"));
    answered.push(result(8, 44, None));
    owed.answered_through(7);
    assert!(owed.is_empty(), "both were before the reply");
    assert_eq!(read(&mut owed, &answered), [8]);
    assert_eq!(owing(&owed), [8]);

    // Its reply deleted (heard by nobody): owed again, said no second time.
    let deleted: Vec<ChatMessageRow> = answered.iter().filter(|m| m.id != 7).cloned().collect();
    assert!(read(&mut owed, &deleted).is_empty());
    assert_eq!(owing(&owed), [5, 6, 8]);
}

#[test]
fn a_read_of_nothing_keeps_the_watermark() {
    let mut owed = Owed::default();
    read(&mut owed, &[row(1, "user"), result(2, 41, None)]);
    // A thread whose messages were all deleted: nothing owed, and a result
    // with an older id is never said again.
    assert!(read_at(&mut owed, &[], 2).is_empty());
    assert!(owed.is_empty());
    assert_eq!(owed.seen_through, Some(2));
}

/// A bind whose read failed knows no watermark: the first read that
/// succeeds owes what no reply answered and says none of it — not the
/// thread's whole history of results — and the one after says what
/// entered since (review finding 3).
#[test]
fn the_first_read_after_a_failed_bind_says_nothing() {
    let mut owed = Owed::default();
    assert_eq!(owed.seen_through, None);
    let history = [
        row(1, "user"),
        result(2, 41, None),
        row(3, "assistant"),
        result(4, 42, None),
        result(5, 43, None),
    ];
    assert!(read(&mut owed, &history).is_empty());
    assert_eq!(owing(&owed), [4, 5]);
    assert_eq!(owed.seen_through, Some(5));
    let later: Vec<ChatMessageRow> = history
        .iter()
        .cloned()
        .chain([result(6, 44, None)])
        .collect();
    assert_eq!(read(&mut owed, &later), [6]);
    assert_eq!(owing(&owed), [4, 5, 6]);
}

/// A saved reply's `done` frame and a read race (review finding 2): a read
/// that covered the reply's id stands — it found the reply deleted, so its
/// results are owed again, and the frame that comes after it cuts nothing;
/// a read taken before the reply was saved does not undo the frame's cut.
#[test]
fn a_read_that_covered_a_reply_stands_against_its_done_frame() {
    let history = [row(1, "user"), row(2, "assistant"), result(3, 41, None)];
    // The reply (7) was saved and deleted before the read; the sequence
    // handed out 7, so the read's watermark covers it.
    let mut owed = Owed::default();
    read(&mut owed, &history);
    let after_delete: Vec<ChatMessageRow> = history.to_vec();
    assert!(read_at(&mut owed, &after_delete, 7).is_empty());
    assert_eq!(owing(&owed), [3]);
    owed.answered_through(7);
    assert_eq!(owing(&owed), [3], "the read saw the reply gone");

    // The frame first, then a read whose snapshot predates the reply.
    let mut owed = Owed::default();
    read(&mut owed, &history);
    owed.answered_through(7);
    assert!(owed.is_empty());
    assert!(read_at(&mut owed, &history, 3).is_empty());
    assert!(
        owed.is_empty(),
        "a read from before the reply cuts as the frame did"
    );
    // A read after the reply stands: the reply is there, nothing is owed.
    let saved: Vec<ChatMessageRow> = history
        .iter()
        .cloned()
        .chain([row(7, "assistant")])
        .collect();
    assert!(read(&mut owed, &saved).is_empty());
    assert!(owed.is_empty());
}

/// Only a delivery writes a result row: a history write moves nothing for
/// a thread whose last read found none — unless no read succeeded yet.
#[test]
fn a_history_write_skips_the_read_while_the_thread_holds_no_result() {
    let mut owed = Owed::default();
    assert!(!owed.unmoved_by_history(), "nothing read yet");
    read(&mut owed, &[row(1, "user"), row(2, "assistant")]);
    assert!(owed.unmoved_by_history());
    // An answered result is one a history write can make owed again.
    read(
        &mut owed,
        &[row(1, "user"), result(3, 41, None), row(4, "assistant")],
    );
    assert!(owed.is_empty());
    assert!(!owed.unmoved_by_history());
}

#[test]
fn the_event_carries_the_feed_s_task_done_facts() {
    let done = done_of(7, &result(9, 41, Some("device 'phone'"))).unwrap();
    assert_eq!(
        done,
        TaskDone {
            thread_id: 7,
            message_id: 9,
            id: 41,
            task_id: "t41".into(),
            server_label: "desktop".into(),
            tool: "desktop__build".into(),
            status: "cancelled".into(),
            by: Some("device 'phone'".into()),
        }
    );
    let v = serde_json::to_value(ServerEvent::LmgwTaskDone { done: done.clone() }).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"type": "lmgw.task.done", "thread_id": 7, "message_id": 9, "id": 41,
            "task_id": "t41", "server_label": "desktop", "tool": "desktop__build",
            "status": "cancelled", "by": "device 'phone'"})
    );
    assert_eq!(
        v["type"],
        lmgw_api_types::chat::TASK_DONE_EVENT,
        "the constant clients match on"
    );
    assert_eq!(
        serde_json::from_value::<ServerEvent>(v).unwrap(),
        ServerEvent::LmgwTaskDone { done }
    );
    // A hand-edited row is owed, but not said.
    let mut bare = result(10, 42, None);
    bare.task = None;
    assert!(done_of(7, &bare).is_none());
    let mut owed = Owed::default();
    read(&mut owed, &[row(1, "user"), bare]);
    assert!(!owed.is_empty());
}
