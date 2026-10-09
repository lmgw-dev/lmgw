use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{mpsc, Notify};

use super::*;
use crate::ir::ToolResultBlock;
use crate::state::AppState;

#[test]
fn arguments_are_an_object_or_say_why_not() {
    assert_eq!(arguments("").unwrap(), serde_json::json!({}));
    assert_eq!(arguments(" \n").unwrap(), serde_json::json!({}));
    assert_eq!(
        arguments(r#"{"q": 1}"#).unwrap(),
        serde_json::json!({"q": 1})
    );
    let bad = arguments(r#"{"q": "#).unwrap_err();
    assert!(bad.contains("not valid JSON (EOF"), "{bad}");
    let list = arguments("[1]").unwrap_err();
    assert!(list.contains("are an array"), "{list}");
}

/// An executor whose call has returned and whose row is written, and that
/// hands back its outcome only once `tail` lets it: a row's write still in
/// flight.
struct SlowRow {
    state: SharedState,
    written: Arc<Notify>,
    tail: Arc<Notify>,
}

#[async_trait]
impl ToolExecutor for SlowRow {
    async fn call(&self, name: &str, _args: &Value) -> ToolOutcome {
        crate::mcp::ingress::record_tool_call(
            &self.state,
            &RequestCtx::default(),
            REALTIME_TOOL_PROTO,
            name,
            Some("alpha".into()),
            Instant::now(),
            None,
        )
        .await;
        self.written.notify_one();
        self.tail.notified().await;
        ToolOutcome {
            blocks: ToolResultBlock::one("done".to_string()),
            is_error: false,
            structured: None,
            content: None,
        }
    }
}

/// Final review #5: the stop comes while the call's executor writes its
/// row. The call has returned: it finishes and is reported, and the row is
/// the executor's alone — no `canceled` one beside it.
#[tokio::test]
async fn a_stop_while_the_executor_writes_its_row_leaves_one_row() {
    let state = AppState::init_for_tests().await.unwrap();
    let ctx = RequestCtx::default();
    let (written, tail) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let exec = SlowRow {
        state: state.clone(),
        written: written.clone(),
        tail: tail.clone(),
    };
    let call = Call {
        index: 0,
        id: "call_r1".into(),
        name: "a__echo".into(),
        args: "{}".into(),
    };
    let (handle, stop) = crate::proxy::stop_pair();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let report = Report::direct(7, &tx);
    let rows = dropped::Rows {
        state: &state,
        ctx: &ctx,
        builtin: &[],
    };
    let drive = async {
        written.notified().await;
        handle.stop();
        // The race sees the stop before the row's tail ends.
        tokio::task::yield_now().await;
        tail.notify_one();
    };
    tokio::join!(one(&exec, &call, Some(&stop), &report, &rows), drive);

    let mut done = false;
    while let Ok((gen, msg)) = rx.try_recv() {
        assert_eq!(gen, 7);
        done |= matches!(msg, Msg::ToolDone { index: 0, .. });
    }
    assert!(done, "the call that returned is reported");
    let filter = crate::store::LogFilter {
        limit: 50,
        ..Default::default()
    };
    let logged: Vec<_> = crate::store::query_logs(&state.db, &filter)
        .await
        .unwrap()
        .into_iter()
        .filter(|l| l.ingress_proto == REALTIME_TOOL_PROTO)
        .collect();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert_eq!(
        (logged[0].status, logged[0].error_kind.as_deref()),
        (200, None)
    );
}

/// An executor that answers with the call id it runs under.
struct CallIdEcho;

#[async_trait]
impl ToolExecutor for CallIdEcho {
    async fn call(&self, _name: &str, _args: &Value) -> ToolOutcome {
        let id = crate::agent::current_call_id().unwrap_or_else(|| "(none)".into());
        ToolOutcome {
            blocks: ToolResultBlock::one(id),
            is_error: false,
            structured: None,
            content: None,
        }
    }
}

/// A call runs under its own id, as the agent loop's do: an MCP task it
/// starts records the call that started it (MCP Tasks design §2.1).
#[tokio::test]
async fn a_call_runs_under_its_call_id() {
    let state = AppState::init_for_tests().await.unwrap();
    let ctx = RequestCtx::default();
    let call = Call {
        index: 0,
        id: "call_r7".into(),
        name: "a__echo".into(),
        args: "{}".into(),
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let report = Report::direct(1, &tx);
    let rows = dropped::Rows {
        state: &state,
        ctx: &ctx,
        builtin: &[],
    };
    one(&CallIdEcho, &call, None, &report, &rows).await;
    let mut seen = None;
    while let Ok((_, msg)) = rx.try_recv() {
        if let Msg::ToolDone { outcome, .. } = msg {
            seen = Some(crate::ir::flatten_tool_result(&outcome.blocks).0);
        }
    }
    assert_eq!(seen.as_deref(), Some("call_r7"));
}
