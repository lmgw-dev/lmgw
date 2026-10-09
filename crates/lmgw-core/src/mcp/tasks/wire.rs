//! The one module that knows MCP **2025-11-25**'s Tasks (MCP Tasks design
//! T1, T2), in rmcp 2.0's types: everything above it speaks
//! [`TaskStatus`] and [`TaskInfo`], so the move to 2026-07-28's extension
//! (§1.9) changes this module, not the feature.
//!
//! - **The decision** ([`augment`], §1.1): the tool's
//!   `execution.taskSupport` is `required`, the server's `initialize`
//!   result declares `capabilities.tasks.requests.tools.call`, and the
//!   revision negotiated is 2025-11-25.
//! - **The call**: `tools/call` with `task: {}` — no `ttl` (T9), the
//!   receiver states its own. Its answer is a `CreateTaskResult` (a
//!   [`Created`]), or a `CallToolResult` from a server that ignored `task`,
//!   which is a normal result ([`answered`]). A `CreateTaskResult` to a
//!   call lmgw did not augment is a protocol error.
//! - **`tasks/get`**, **`tasks/result`** (rmcp has no typed answer for it:
//!   the payload arrives as whichever `ServerResult` its JSON matches, and
//!   is read here as the `CallToolResult` of the original `tools/call`),
//!   **`tasks/cancel`**, and the `notifications/tasks/status` the handler
//!   hands over ([`note`]).
//! - **`pollInterval: 0`** is read as not given: no interval, so the
//!   setting's (T8).
//! - **`tasks/cancel`** is sent only to a session whose `initialize` result
//!   declares `capabilities.tasks.cancel` ([`can_cancel`]).
//! - **A bridged result** loses its `_meta["io.modelcontextprotocol/related-task"]`
//!   ([`untasked`]): its caller made a normal call.
//! - **`-32602`** for a task's id is [`WireError::Unknown`]: the receiver
//!   does not know the task (it restarted, the id expired) — or, for
//!   `tasks/cancel`, the task is already terminal.

use std::time::Duration;

use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelTaskParams, CancelTaskRequest,
    ClientRequest, CreateTaskResult, ErrorCode, GetTaskParams, GetTaskPayloadParams,
    GetTaskPayloadRequest, GetTaskRequest, InitializeResult, ProtocolVersion, ServerResult, Task,
    TaskMetadata, TaskStatus as WireStatus, TaskStatusNotificationParam, TaskSupport, Tool,
};
use rmcp::service::{Peer, ServiceError};
use rmcp::RoleClient;

use crate::config::McpServer;

use super::{Augment, TaskInfo, TaskStatus};

/// The revision whose Tasks lmgw speaks (T1).
pub(super) const REVISION: ProtocolVersion = ProtocolVersion::V_2025_11_25;

/// The `_meta` key of a `CreateTaskResult`'s sentence for the model while
/// the task runs (the spec's note on returning control to the model).
pub(super) const IMMEDIATE_RESPONSE: &str = "io.modelcontextprotocol/model-immediate-response";

/// The `_meta` key that ties a message to its task.
const RELATED_TASK: &str = "io.modelcontextprotocol/related-task";

/// Whether `tool` (as `server` listed it) is called as a task, by the
/// session's `info` (§1.1). A tool the session did not list is called
/// normally: the server answers for it.
pub(super) fn augment(
    server: &McpServer,
    info: Option<&InitializeResult>,
    tool: Option<&Tool>,
) -> Augment {
    let Some(tool) = tool else {
        return Augment::Normal;
    };
    if tool.task_support() != TaskSupport::Required {
        return Augment::Normal;
    }
    let name = tool.name.as_ref();
    let who = &server.name;
    let Some(info) = info else {
        return Augment::Refused(format!(
            "server '{who}' requires a task for '{name}', and its session has no initialize \
             result to read its task capability from"
        ));
    };
    if info.protocol_version != REVISION {
        return Augment::Refused(format!(
            "server '{who}' requires a task for '{name}' but negotiated MCP {}, whose Tasks \
             lmgw does not speak (it speaks the Tasks of {})",
            info.protocol_version.as_str(),
            REVISION.as_str()
        ));
    }
    let declared = info
        .capabilities
        .tasks
        .as_ref()
        .is_some_and(|t| t.supports_tools_call());
    if !declared {
        return Augment::Refused(format!(
            "server '{who}' requires a task for '{name}' but does not declare \
             `tasks.requests.tools.call`"
        ));
    }
    Augment::Task
}

/// Whether the session's `initialize` result declares `tasks.cancel`.
pub(super) fn can_cancel(peer: &Peer<RoleClient>) -> bool {
    peer.peer_info()
        .as_deref()
        .and_then(|i| i.capabilities.tasks.as_ref())
        .is_some_and(|t| t.cancel.is_some())
}

/// A task's result as a normal call's: without the `_meta` that relates it
/// to its task (an emptied `_meta` goes too).
pub(super) fn untasked(mut result: CallToolResult) -> CallToolResult {
    if let Some(meta) = result.meta.as_mut() {
        meta.0.remove(RELATED_TASK);
        if meta.0.is_empty() {
            result.meta = None;
        }
    }
    result
}

/// `params` as a task-augmented call: `task: {}`.
pub(super) fn augmented(params: CallToolRequestParams) -> CallToolRequestParams {
    params.with_task(TaskMetadata::new())
}

/// A `CreateTaskResult`, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::mcp) struct Created {
    pub info: TaskInfo,
    /// The server's `model-immediate-response`, when it gave one (T5).
    pub immediate: Option<String>,
}

/// What a `tools/call` was answered with.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::mcp) enum Answered {
    Result(CallToolResult),
    Task(Created),
}

/// Read a `tools/call`'s answer; `augmented` is whether it carried `task`.
pub(in crate::mcp) fn answered(answer: ServerResult, augmented: bool) -> Result<Answered, String> {
    match answer {
        ServerResult::CallToolResult(r) => Ok(Answered::Result(r)),
        ServerResult::CreateTaskResult(c) if augmented => Ok(Answered::Task(created(c))),
        ServerResult::CreateTaskResult(c) => Err(format!(
            "answered with a task (CreateTaskResult, task '{}') a tools/call lmgw did not send as \
             a task: a protocol error — a tool that runs as a task declares \
             execution.taskSupport 'required'",
            c.task.task_id
        )),
        _ => Err("answered tools/call with something that is not a tool result".into()),
    }
}

fn created(c: CreateTaskResult) -> Created {
    let immediate = c
        .meta
        .as_ref()
        .and_then(|m| m.0.get(IMMEDIATE_RESPONSE))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Created {
        info: info(c.task),
        immediate,
    }
}

fn info(t: Task) -> TaskInfo {
    TaskInfo {
        status: status(&t.status),
        task_id: t.task_id,
        status_message: t.status_message,
        // 0 is no interval: as not given (T8).
        poll_interval_ms: t.poll_interval.filter(|&ms| ms > 0),
        ttl_ms: t.ttl,
    }
}

fn status(s: &WireStatus) -> TaskStatus {
    match s {
        WireStatus::InputRequired => TaskStatus::InputRequired,
        WireStatus::Completed => TaskStatus::Completed,
        WireStatus::Failed => TaskStatus::Failed,
        WireStatus::Cancelled => TaskStatus::Cancelled,
        // `working`, and a status a later rmcp adds: the task still runs.
        _ => TaskStatus::Working,
    }
}

/// A `notifications/tasks/status`, read.
pub(super) fn note(params: TaskStatusNotificationParam) -> TaskInfo {
    info(params.task)
}

/// Send the augmented `params` on a registered server's session.
pub(super) async fn call(
    peer: &Peer<RoleClient>,
    params: CallToolRequestParams,
) -> Result<Answered, String> {
    let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
    let answer = peer
        .send_request(request)
        .await
        .map_err(|e| e.to_string())?;
    answered(answer, true)
}

/// Why a task request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WireError {
    /// `-32602`: the receiver does not know the task (or, for
    /// `tasks/cancel`, it is already terminal). Its message.
    Unknown(String),
    /// Another JSON-RPC error: its message.
    Rpc(String),
    /// No answer: the session went, or did not answer within the bound.
    Link(String),
}

fn wire_error(e: ServiceError) -> WireError {
    match e {
        ServiceError::McpError(d) if d.code == ErrorCode::INVALID_PARAMS => {
            WireError::Unknown(d.message.to_string())
        }
        ServiceError::McpError(d) => WireError::Rpc(d.message.to_string()),
        other => WireError::Link(other.to_string()),
    }
}

/// `request`, bounded by `bound` when there is one.
async fn send(
    peer: &Peer<RoleClient>,
    request: ClientRequest,
    bound: Option<Duration>,
) -> Result<ServerResult, WireError> {
    let sent = peer.send_request(request);
    let answer = match bound {
        Some(b) => tokio::time::timeout(b, sent).await.map_err(|_| {
            WireError::Link(format!(
                "no answer within {} ms (the server's timeout_ms)",
                b.as_millis()
            ))
        })?,
        None => sent.await,
    };
    answer.map_err(wire_error)
}

/// The task's `Task`, from a `tasks/get` or `tasks/cancel` answer.
fn task_of(answer: ServerResult) -> Result<TaskInfo, WireError> {
    match answer {
        ServerResult::GetTaskResult(r) => Ok(info(r.task)),
        ServerResult::CancelTaskResult(r) => Ok(info(r.task)),
        _ => Err(WireError::Rpc(
            "the answer is not a task (no taskId and status)".into(),
        )),
    }
}

/// `tasks/get`, bounded by `bound`.
pub(super) async fn get(
    peer: &Peer<RoleClient>,
    task_id: &str,
    bound: Duration,
) -> Result<TaskInfo, WireError> {
    let request = ClientRequest::GetTaskRequest(GetTaskRequest::new(GetTaskParams::new(task_id)));
    task_of(send(peer, request, Some(bound)).await?)
}

/// `tasks/cancel`, bounded by `bound`: the task as the receiver now has it.
pub(super) async fn cancel(
    peer: &Peer<RoleClient>,
    task_id: &str,
    bound: Duration,
) -> Result<TaskInfo, WireError> {
    let request =
        ClientRequest::CancelTaskRequest(CancelTaskRequest::new(CancelTaskParams::new(task_id)));
    task_of(send(peer, request, Some(bound)).await?)
}

/// What `tasks/result` answered: the original call's result, or its
/// JSON-RPC error's message.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Payload {
    Tool(CallToolResult),
    Error(String),
}

/// `tasks/result`: bounded by `bound` for a terminal task (it answers at
/// once), unbounded for one held open while it is `input_required` (T19).
pub(super) async fn result(
    peer: &Peer<RoleClient>,
    task_id: &str,
    bound: Option<Duration>,
) -> Result<Payload, WireError> {
    let request = ClientRequest::GetTaskPayloadRequest(GetTaskPayloadRequest::new(
        GetTaskPayloadParams::new(task_id),
    ));
    match send(peer, request, bound).await {
        Ok(ServerResult::CallToolResult(r)) => Ok(Payload::Tool(r)),
        Ok(other) => serde_json::to_value(&other)
            .ok()
            .and_then(|v| serde_json::from_value::<CallToolResult>(v).ok())
            .map(Payload::Tool)
            .ok_or_else(|| {
                WireError::Rpc("tasks/result answered something that is not a tool result".into())
            }),
        Err(WireError::Rpc(message)) => Ok(Payload::Error(message)),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{ServerCapabilities, TasksCapability, ToolExecution};

    fn server() -> McpServer {
        super::super::test_server()
    }

    fn tool(support: Option<TaskSupport>) -> Tool {
        let mut t = Tool::new("build", "b", serde_json::Map::new());
        if let Some(s) = support {
            t = t.with_execution(ToolExecution::new().with_task_support(s));
        }
        t
    }

    fn init(version: ProtocolVersion, tasks: bool) -> InitializeResult {
        let mut caps = ServerCapabilities::default();
        caps.tasks = tasks.then(TasksCapability::server_default);
        let mut i = InitializeResult::new(caps);
        i.protocol_version = version;
        i
    }

    #[test]
    fn only_required_tools_on_a_declaring_server_are_tasks() {
        let s = server();
        let ok = init(REVISION, true);
        for support in [
            None,
            Some(TaskSupport::Forbidden),
            Some(TaskSupport::Optional),
        ] {
            assert_eq!(
                augment(&s, Some(&ok), Some(&tool(support))),
                Augment::Normal
            );
        }
        let required = tool(Some(TaskSupport::Required));
        assert_eq!(augment(&s, Some(&ok), Some(&required)), Augment::Task);
        assert_eq!(augment(&s, Some(&ok), None), Augment::Normal);

        let Augment::Refused(why) = augment(&s, Some(&init(REVISION, false)), Some(&required))
        else {
            panic!("refused")
        };
        assert_eq!(
            why,
            "server 'desktop' requires a task for 'build' but does not declare \
             `tasks.requests.tools.call`"
        );
        let old = init(ProtocolVersion::V_2025_06_18, true);
        assert!(matches!(
            augment(&s, Some(&old), Some(&required)),
            Augment::Refused(w) if w.contains("2025-06-18")
        ));
    }

    #[test]
    fn a_created_task_reads_its_immediate_response() {
        let mut meta = rmcp::model::Meta::new();
        meta.0.insert(IMMEDIATE_RESPONSE.into(), "on it".into());
        let task = Task::new(
            "t1".into(),
            WireStatus::Working,
            "2026-10-09T10:00:00Z".into(),
            "2026-10-09T10:00:00Z".into(),
        )
        .with_poll_interval(250);
        let c = CreateTaskResult::new(task).with_meta(meta);
        let Ok(Answered::Task(read)) = answered(ServerResult::CreateTaskResult(c.clone()), true)
        else {
            panic!("a task")
        };
        assert_eq!(read.immediate.as_deref(), Some("on it"));
        assert_eq!(read.info.poll_interval_ms, Some(250));
        assert_eq!(read.info.status, TaskStatus::Working);

        let zero = Task::new(
            "t2".into(),
            WireStatus::Working,
            "2026-10-09T10:00:00Z".into(),
            "2026-10-09T10:00:00Z".into(),
        )
        .with_poll_interval(0);
        assert_eq!(info(zero).poll_interval_ms, None, "0 reads as not given");
        let e = answered(ServerResult::CreateTaskResult(c), false).unwrap_err();
        assert!(e.contains("protocol error"), "{e}");
    }
}
