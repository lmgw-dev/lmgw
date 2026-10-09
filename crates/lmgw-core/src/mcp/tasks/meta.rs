//! What lmgw says of tasks on its own surfaces (MCP Tasks design §1.8,
//! T15, T18).
//!
//! - **`/mcp` lists every tool without `execution`** ([`listed`]): `/mcp`
//!   declares no `tasks` capability and bridges every call (T4), and a
//!   client that saw `taskSupport: "required"` would, by the spec, refuse
//!   to call the tool. `/v1/mcp/servers` never carried it.
//! - **A task-augmented device call carries `_meta["lmgw/task"]`**
//!   ([`device_task`]): whether its result enters a thread later or a
//!   caller waits for it now, and the thread's id when the device may know
//!   it.

use lmgw_api_types::mcp_host::{TaskDelivery, TaskMeta};
use rmcp::model::Tool;
use serde_json::Value;

use super::super::host::CallFrom;

/// `tool` as `/mcp`'s `tools/list` carries it: as listed, without its
/// `execution` member.
pub(crate) fn listed(tool: &Tool) -> Option<Value> {
    let mut v = serde_json::to_value(tool).ok()?;
    if let Some(o) = v.as_object_mut() {
        o.remove("execution");
    }
    Some(v)
}

/// `lmgw/task` for a task-augmented device call made for `from`.
pub(crate) fn device_task(from: &CallFrom) -> TaskMeta {
    match &from.late {
        Some(late) => TaskMeta {
            delivery: TaskDelivery::Thread,
            thread_id: late.device_sees_thread.then_some(late.thread_id),
        },
        None => TaskMeta {
            delivery: TaskDelivery::Wait,
            thread_id: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{TaskSupport, ToolExecution};

    #[test]
    fn a_listed_tool_has_no_execution() {
        let t = Tool::new("build", "b", serde_json::Map::new())
            .with_execution(ToolExecution::new().with_task_support(TaskSupport::Required));
        assert!(serde_json::to_value(&t).unwrap().get("execution").is_some());
        let v = listed(&t).unwrap();
        assert!(v.get("execution").is_none(), "{v}");
        assert_eq!(v["name"], "build");
    }

    #[test]
    fn the_delivery_says_where_the_result_goes() {
        let mut from = CallFrom::gateway();
        assert_eq!(
            device_task(&from),
            TaskMeta {
                delivery: TaskDelivery::Wait,
                thread_id: None
            }
        );
        from.late = Some(super::super::Late {
            thread_id: 812,
            device_sees_thread: true,
            call_id: "c1".into(),
        });
        assert_eq!(device_task(&from).thread_id, Some(812));
        from.late.as_mut().unwrap().device_sees_thread = false;
        let admin = device_task(&from);
        assert_eq!(
            (admin.delivery, admin.thread_id),
            (TaskDelivery::Thread, None)
        );
    }
}
