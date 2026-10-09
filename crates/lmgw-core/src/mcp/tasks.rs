//! MCP Tasks for hosted servers (MCP Tasks design §1): lmgw as the
//! requestor of task-augmented `tools/call`, on every southbound link
//! (registered servers and device rows alike).
//!
//! **Which calls** (§1.1, T3): a tool whose `execution.taskSupport` is
//! `required`, on a server that declares `capabilities.tasks.requests.tools.call`
//! and negotiated the pinned revision (2025-11-25, T1). A `required` tool on a
//! server without either is not called, and the call's error says why;
//! everything else is a normal call.
//!
//! **Where the result goes** (T4):
//! - **the late path**, for a Chat turn's call ([`Late`], set by
//!   `McpExecutor::with_late`): the call is answered `started, job <task id>`
//!   at once (T5), the task is stored (`store::mcp_tasks`) and followed
//!   ([`follow`]) across turns, and its result ends in an `ended` row the
//!   thread receives later (MCP Tasks WP2 delivers it);
//! - **the bridge** ([`bridge`]), for every other caller: lmgw follows the
//!   task inline and answers the call with its result, within the row's
//!   `timeout_ms`, and sends `tasks/cancel` when it stops waiting.
//!
//! **Revision-neutral above [`wire`]** (T2): only `wire` knows 2025-11-25's
//! shapes; the follower, the bridge and the store speak [`TaskStatus`] and
//! [`TaskInfo`].
//!
//! Children:
//! - [`wire`]: the decision, the augmented call, `tasks/get`,
//!   `tasks/result`, `tasks/cancel`, the status notification;
//! - [`follow`]: the follower of every open row, the resume, a reused id;
//! - [`registry`]: the followers by `(server, task id)`, which the reaper
//!   and the notifications read;
//! - [`cancel`]: the cancel from lmgw and the owed cancel's follower;
//! - [`bridge`]: one task followed inline for a caller with no thread;
//! - [`meta`]: what lmgw says of tasks on its own surfaces (`/mcp`'s
//!   `tools/list` without `execution`, a device call's `lmgw/task`).

mod bridge;
mod cancel;
mod follow;
pub(crate) mod meta;
mod registry;
pub(crate) mod stored;
mod wire;

use std::time::{Duration, Instant};

use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::Peer;
use rmcp::RoleClient;

use crate::config::McpServer;
use crate::ir::ToolResultBlock;

use super::host::CallFrom;
use super::{CallError, McpManager, McpStatus};

pub use cancel::{CancelOutcome, CancelRefusal};
pub(crate) use registry::Tasks;
pub(in crate::mcp) use wire::{answered, Answered};

/// A task's status, as lmgw keeps it (T2): the receiver's five, and
/// `abandoned` for a task the receiver no longer knows or whose server row
/// went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Working,
    InputRequired,
    Completed,
    Failed,
    Cancelled,
    Abandoned,
}

impl TaskStatus {
    /// As `mcp_tasks.status` and every surface write it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::InputRequired => "input_required",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Abandoned => "abandoned",
        }
    }

    /// Read back from `mcp_tasks.status`.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "working" => Self::Working,
            "input_required" => Self::InputRequired,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "abandoned" => Self::Abandoned,
            _ => return None,
        })
    }

    /// No further status follows.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Abandoned
        )
    }
}

/// What a receiver said of a task — in a `CreateTaskResult`, a `tasks/get`
/// answer or a status notification — in lmgw's terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskInfo {
    pub task_id: String,
    pub status: TaskStatus,
    pub status_message: Option<String>,
    /// The receiver's suggested poll interval, when it gave one.
    pub poll_interval_ms: Option<u64>,
    /// The receiver's stated retention; `None`: unlimited or not given.
    pub ttl_ms: Option<u64>,
}

/// A call whose task, if it becomes one, belongs to a stored Chat thread
/// (T4, T6): it takes the late path. Built per call by `McpExecutor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Late {
    pub thread_id: i64,
    /// Whether a device may know the thread's id: not for Admin Chat
    /// (client-apps L3), whose `lmgw/task` carries `thread_id: null`.
    pub device_sees_thread: bool,
    /// The model's id of the call (`agent::current_call_id`); empty when
    /// the call was not made by a model's tool loop.
    pub call_id: String,
}

/// Who a task's request rows are written under (T20): the starting
/// principal's key name (`None` for the gateway's own) and the surface's
/// `ingress_proto`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedAs {
    pub client_key: Option<String>,
    pub proto: &'static str,
}

impl Default for LoggedAs {
    /// The northbound `/mcp`'s.
    fn default() -> Self {
        Self {
            client_key: None,
            proto: "mcp",
        }
    }
}

/// Whether a call is task-augmented (§1.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::mcp) enum Augment {
    /// A normal call.
    Normal,
    /// A task-augmented call.
    Task,
    /// A `required` tool lmgw cannot call: why.
    Refused(String),
}

/// How a task ended, in lmgw's terms; [`ending_blocks`] writes the result
/// the model sees, [`bridged`] the bridge's answer.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::mcp) enum Ending {
    /// `completed`, or `failed` with a result (`isError`).
    Result {
        status: TaskStatus,
        result: CallToolResult,
    },
    /// `failed` with a JSON-RPC error: its message, and the task's last
    /// `statusMessage`.
    Error {
        message: String,
        status_message: Option<String>,
    },
    /// `cancelled`: on the server (`by` `None`), or by lmgw for `by`;
    /// `told` is `false` when the server was not connected and the cancel
    /// is owed.
    Cancelled { by: Option<String>, told: bool },
    /// `abandoned`: the sentence that says why.
    Abandoned(String),
}

impl Ending {
    pub(in crate::mcp) fn status(&self) -> TaskStatus {
        match self {
            Self::Result { status, .. } => *status,
            Self::Error { .. } => TaskStatus::Failed,
            Self::Cancelled { .. } => TaskStatus::Cancelled,
            Self::Abandoned(_) => TaskStatus::Abandoned,
        }
    }

    /// Who cancelled, for a cancel lmgw sent.
    pub(in crate::mcp) fn ended_by(&self) -> Option<&str> {
        match self {
            Self::Cancelled { by, .. } => by.as_deref(),
            _ => None,
        }
    }

    /// How the end's request row reads (T20), `header` its first line.
    pub(in crate::mcp) fn row_end(&self, header: &str) -> RowEnd {
        match self {
            Self::Result { result, .. } if result.is_error == Some(true) => {
                RowEnd::Failed(format!("{header}: {}", preview(result)))
            }
            Self::Result { .. } => RowEnd::Done,
            Self::Error { message, .. } => RowEnd::Failed(format!("{header}: {message}")),
            Self::Cancelled { .. } => RowEnd::Canceled(header.to_string()),
            Self::Abandoned(why) => RowEnd::Failed(format!("{header}: {why}")),
        }
    }
}

/// How a task's end reads in its request row (T20): a tool call's row,
/// failed with the message, or canceled with the note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::mcp) enum RowEnd {
    Done,
    Failed(String),
    Canceled(String),
}

/// The first line of every ended task's result (§1.4).
pub(in crate::mcp) fn header(task_id: &str, tool: &str, status: TaskStatus) -> String {
    format!("job {task_id} ({tool}) {}", status.as_str())
}

/// The immediate answer of a late call (T5): `started, job <task id>`, and
/// the server's own sentence for the model on the next line.
pub(in crate::mcp) fn started_text(task_id: &str, immediate: Option<&str>) -> String {
    match immediate.map(str::trim).filter(|s| !s.is_empty()) {
        Some(said) => format!("started, job {task_id}\n{said}"),
        None => format!("started, job {task_id}"),
    }
}

/// "server '<name>' no longer knows the job" (§1.4, server-tools decision
/// 5's wording).
pub(in crate::mcp) fn no_longer_knows(server: &str) -> String {
    format!(
        "server '{server}' no longer knows the job: it was abandoned; it may or may not have \
         finished"
    )
}

/// Why a task ended when its server answered a new task with its id
/// (design decision on reused ids, WP1): the server cannot know two tasks
/// by one id, so the older one is gone as far as lmgw can tell.
pub(in crate::mcp) fn reused_id(server: &str) -> String {
    format!(
        "server '{server}' reused its task id for a new job: this one was abandoned; it may or \
         may not have finished"
    )
}

/// The stored result (IR blocks, JSON) of a task whose server row was
/// removed (§1.4): "server '<name>' was removed (<why>)". For
/// `store::mcp_tasks::server_gone`, in the removal's own transaction.
pub fn removed_result(task_id: &str, tool: &str, server: &str, why: &str) -> String {
    let text = format!(
        "{}\n{}",
        header(task_id, tool, TaskStatus::Abandoned),
        removed(server, why)
    );
    serde_json::to_string(&ToolResultBlock::one(text)).unwrap_or_default()
}

/// "server '<name>' was removed (<why>): the job was abandoned" (§1.4),
/// the sentence after the header.
pub(in crate::mcp) fn removed(server: &str, why: &str) -> String {
    format!(
        "server '{server}' was removed ({why}): the job was abandoned; it may or may not have \
         finished"
    )
}

/// The result of an ended task as the model will see it (§1.4): IR
/// tool-result blocks, the header line first.
pub(in crate::mcp) fn ending_blocks(
    ending: &Ending,
    task_id: &str,
    tool: &str,
) -> Vec<ToolResultBlock> {
    let head = header(task_id, tool, ending.status());
    let line = |rest: String| ToolResultBlock::one(format!("{head}\n{rest}"));
    match ending {
        Ending::Result { result, .. } => {
            let mut blocks = super::exec::blocks_from_result(result);
            match blocks.first_mut() {
                Some(ToolResultBlock::Text { text }) => *text = format!("{head}\n{text}"),
                _ => blocks.insert(0, ToolResultBlock::text(head)),
            }
            blocks
        }
        Ending::Error {
            message,
            status_message,
        } => match status_message {
            Some(m) => line(format!("{message}\n{m}")),
            None => line(message.clone()),
        },
        Ending::Cancelled { by: None, .. } => line("cancelled on the server".into()),
        Ending::Cancelled {
            by: Some(who),
            told: true,
        } => line(format!("cancelled by {who}")),
        Ending::Cancelled {
            by: Some(who),
            told: false,
        } => line(format!(
            "cancelled by {who}; the server was not connected, and is told when it is"
        )),
        Ending::Abandoned(why) => line(why.clone()),
    }
}

/// The bridge's answer for an ended task (§1.7): its result as a normal
/// `CallToolResult`, without the `_meta` that ties it to the task (the
/// caller made a normal call); a JSON-RPC error and an abandoned task stay
/// errors.
pub(in crate::mcp) fn bridged(
    ending: &Ending,
    task_id: &str,
    tool: &str,
    server: &str,
) -> Result<CallToolResult, CallError> {
    match ending {
        Ending::Result { result, .. } => Ok(wire::untasked(result.clone())),
        Ending::Error { message, .. } => Err(CallError::Upstream {
            server: server.to_string(),
            detail: message.clone(),
        }),
        Ending::Cancelled { .. } => {
            let (text, _) = crate::ir::flatten_tool_result(&ending_blocks(ending, task_id, tool));
            Ok(CallToolResult::error(vec![ContentBlock::text(text)]))
        }
        Ending::Abandoned(why) => Err(CallError::Upstream {
            server: server.to_string(),
            detail: why.clone(),
        }),
    }
}

/// A short preview of a tool result's text, for a request row.
fn preview(result: &CallToolResult) -> String {
    let (text, _) = crate::ir::flatten_tool_result(&super::exec::blocks_from_result(result));
    text.chars().take(200).collect()
}

impl McpManager {
    /// Whether `upstream_tool` on `server`'s live session `peer` is called
    /// as a task (§1.1), from the tool as the server listed it.
    pub(in crate::mcp) async fn augment(
        &self,
        server: &McpServer,
        upstream_tool: &str,
        peer: &Peer<RoleClient>,
    ) -> Augment {
        let tool = {
            let conns = self.conns.read().await;
            conns.get(&server.id).and_then(|c| {
                c.tools
                    .iter()
                    .find(|t| t.name.as_ref() == upstream_tool)
                    .cloned()
            })
        };
        wire::augment(server, peer.peer_info().as_deref(), tool.as_ref())
    }

    /// The task branch of a registered server's call (§1.1): `None` for a
    /// normal call, which the caller makes as before; otherwise the
    /// augmented call's answer — its result, `started, job …` (the late
    /// path), or the bridge's.
    pub(in crate::mcp) async fn call_task(
        &self,
        server: &McpServer,
        exposed: &str,
        peer: &Peer<RoleClient>,
        params: &CallToolRequestParams,
        from: &CallFrom,
    ) -> Option<Result<(CallToolResult, String), CallError>> {
        let started = Instant::now();
        match self.augment(server, params.name.as_ref(), peer).await {
            Augment::Normal => None,
            Augment::Refused(why) => Some(Err(CallError::Upstream {
                server: server.name.clone(),
                detail: why,
            })),
            Augment::Task => {
                let timeout = Duration::from_millis(server.timeout_ms);
                let call = wire::call(peer, wire::augmented(params.clone()));
                let answer = tokio::time::timeout(timeout, call).await;
                if let Some(c) = self.conns.write().await.get_mut(&server.id) {
                    c.last_used = Instant::now();
                }
                Some(match answer {
                    Err(_) => Err(CallError::Timeout {
                        server: server.name.clone(),
                        timeout_ms: server.timeout_ms,
                    }),
                    Ok(Err(detail)) => Err(CallError::Upstream {
                        server: server.name.clone(),
                        detail,
                    }),
                    Ok(Ok(Answered::Result(r))) => Ok((r, server.name.clone())),
                    Ok(Ok(Answered::Task(created))) => {
                        self.task_created(server, exposed, created, from, started)
                            .await
                    }
                })
            }
        }
    }

    /// A task-augmented call was answered with a task: the late path for
    /// a call with a [`Late`], the bridge for every other (T4).
    pub(in crate::mcp) async fn task_created(
        &self,
        server: &McpServer,
        exposed: &str,
        created: wire::Created,
        from: &CallFrom,
        started: Instant,
    ) -> Result<(CallToolResult, String), CallError> {
        match &from.late {
            Some(late) => self.start_late(server, exposed, created, from, late).await,
            None => bridge::wait(self, server, exposed, created, from, started)
                .await
                .map(|r| (r, server.name.clone())),
        }
    }

    /// The session a task on `server` is followed over, when there is one:
    /// a device row's link while it is `Ready`; a registered server's
    /// session, connected by the lazy path as a call would be (§1.3) while
    /// the row is enabled.
    pub(in crate::mcp) async fn task_peer(&self, server: &McpServer) -> Option<Peer<RoleClient>> {
        if server.is_device() {
            return self.device_peer(server.id).await.map(|p| p.peer);
        }
        let ready = |conns: &std::collections::HashMap<i64, super::McpConn>| {
            conns
                .get(&server.id)
                .filter(|c| c.status == McpStatus::Ready)
                .and_then(|c| c.running.as_ref().map(|r| r.peer().clone()))
        };
        if let Some(peer) = ready(&*self.conns.read().await) {
            return Some(peer);
        }
        if !server.enabled {
            return None;
        }
        self.start_one(server).await;
        ready(&*self.conns.read().await)
    }
}

/// A registered server row for the unit tests.
#[cfg(test)]
pub(super) fn test_server() -> McpServer {
    McpServer {
        id: 1,
        name: "desktop".into(),
        enabled: true,
        transport: crate::config::McpTransport::Http,
        command: None,
        args: Vec::new(),
        env: Vec::new(),
        cwd: None,
        container_image: None,
        extra_run_args: Vec::new(),
        url: Some("http://127.0.0.1:1/mcp".into()),
        headers: Vec::new(),
        tool_prefix: "desktop".into(),
        timeout_ms: 30_000,
        autostart: false,
        idle_seconds: 0,
        allow_sampling: false,
        sampling_alias: None,
        agent_id: None,
        device_key_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_round_trip_and_the_last_four_are_terminal() {
        for s in [
            TaskStatus::Working,
            TaskStatus::InputRequired,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
            TaskStatus::Abandoned,
        ] {
            assert_eq!(TaskStatus::parse(s.as_str()), Some(s));
        }
        assert!(!TaskStatus::InputRequired.is_terminal());
        assert!(TaskStatus::Abandoned.is_terminal());
        assert_eq!(TaskStatus::parse("running"), None);
    }

    /// A task's result is model input as a call's is: its structured
    /// content only when its content is empty (client-apps design §7.5).
    #[test]
    fn a_result_s_structured_content_reaches_the_model_only_without_content() {
        let ending = |result: serde_json::Value| Ending::Result {
            status: TaskStatus::Completed,
            result: serde_json::from_value(result).unwrap(),
        };
        let both = ending(serde_json::json!({
            "content": [{"type": "text", "text": "42 files"}],
            "structuredContent": {"files": 42}
        }));
        let blocks = ending_blocks(&both, "7f3a", "desktop__run");
        assert_eq!(
            blocks,
            ToolResultBlock::one("job 7f3a (desktop__run) completed\n42 files")
        );
        let only = ending(serde_json::json!({"content": [], "structuredContent": {"files": 42}}));
        let text = crate::ir::flatten_tool_result(&ending_blocks(&only, "7f3a", "desktop__run")).0;
        assert_eq!(text, "job 7f3a (desktop__run) completed\n{\"files\":42}");
    }

    #[test]
    fn the_result_text_of_each_ending() {
        let text = |e: &Ending| {
            crate::ir::flatten_tool_result(&ending_blocks(e, "7f3a", "desktop__run")).0
        };
        let done = Ending::Result {
            status: TaskStatus::Completed,
            result: CallToolResult::success(vec![ContentBlock::text("42 files")]),
        };
        assert_eq!(text(&done), "job 7f3a (desktop__run) completed\n42 files");
        let failed = Ending::Error {
            message: "disk full".into(),
            status_message: Some("at step 3".into()),
        };
        assert_eq!(
            text(&failed),
            "job 7f3a (desktop__run) failed\ndisk full\nat step 3"
        );
        assert_eq!(
            text(&Ending::Cancelled {
                by: None,
                told: true
            }),
            "job 7f3a (desktop__run) cancelled\ncancelled on the server"
        );
        assert!(text(&Ending::Cancelled {
            by: Some("the dashboard".into()),
            told: false
        })
        .ends_with(
            "cancelled by the dashboard; the server was not connected, and is told when it is"
        ));
        assert_eq!(
            text(&Ending::Abandoned(no_longer_knows("desktop"))),
            "job 7f3a (desktop__run) abandoned\nserver 'desktop' no longer knows the job: it was \
             abandoned; it may or may not have finished"
        );
        assert!(text(&Ending::Abandoned(reused_id("desktop"))).ends_with(
            "server 'desktop' reused its task id for a new job: this one was abandoned; it may \
             or may not have finished"
        ));
        assert_eq!(started_text("7f3a", None), "started, job 7f3a");
        assert_eq!(
            started_text("7f3a", Some("Building; I will tell you.")),
            "started, job 7f3a\nBuilding; I will tell you."
        );
    }

    /// A bridged result is a normal call's: the related-task `_meta` goes,
    /// the server's other `_meta` stays.
    #[test]
    fn a_bridged_result_carries_no_related_task_meta() {
        let mut meta = rmcp::model::Meta::new();
        meta.0.insert(
            "io.modelcontextprotocol/related-task".into(),
            serde_json::json!({"taskId": "7f3a"}),
        );
        meta.0.insert("acme/trace".into(), "x".into());
        let mut result = CallToolResult::success(vec![ContentBlock::text("done")]);
        result.meta = Some(meta);
        let ending = Ending::Result {
            status: TaskStatus::Completed,
            result,
        };
        let out = bridged(&ending, "7f3a", "desktop__run", "desktop").unwrap();
        let meta = out.meta.expect("the other key stays");
        assert!(!meta.0.contains_key("io.modelcontextprotocol/related-task"));
        assert_eq!(meta.0["acme/trace"], "x");

        let mut only = rmcp::model::Meta::new();
        only.0.insert(
            "io.modelcontextprotocol/related-task".into(),
            serde_json::json!({"taskId": "7f3a"}),
        );
        let mut result = CallToolResult::success(vec![ContentBlock::text("done")]);
        result.meta = Some(only);
        let ending = Ending::Result {
            status: TaskStatus::Completed,
            result,
        };
        let out = bridged(&ending, "7f3a", "desktop__run", "desktop").unwrap();
        assert!(out.meta.is_none(), "an empty _meta goes");
    }
}
