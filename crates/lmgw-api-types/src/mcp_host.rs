//! The device MCP host link, `GET /mcp/host` (client-apps design §5): the
//! wire facts a device that hosts tools needs — the route, its refusals and
//! close codes, and the `_meta` lmgw stamps on every `tools/call` it forwards
//! over the link (L12, §5.5).
//!
//! The link is an lmgw transport binding, not an MCP transport (L11): the
//! device opens a WebSocket to the route with its device key, and lmgw is the
//! MCP **client** on it. One JSON-RPC message per text frame, no batches.
//! lmgw sends `initialize`, the device answers with its capabilities, lmgw
//! sends `notifications/initialized` and lists the tools.
//!
//! **Tasks** (MCP Tasks design §1, §5.3): the link speaks the Tasks of MCP
//! revision **2025-11-25**, the revision lmgw's `initialize` asks for.
//! - A device that runs open-ended work declares
//!   `capabilities.tasks: {cancel: {}, requests: {tools: {call: {}}}}` in its
//!   `initialize` result, and `execution.taskSupport: "required"` on each
//!   such tool. lmgw calls exactly those tools as tasks; `optional`,
//!   `forbidden` and absent are called normally, and a `required` tool on a
//!   device that does not declare the capability is not called (its error
//!   says why). lmgw's own `initialize` declares nothing for tasks.
//! - lmgw sends `tools/call` with `task: {}` (no `ttl`: the device states its
//!   own) and `_meta["lmgw/task"]` ([`TaskMeta`]): `delivery` `thread` (the
//!   result enters a Chat thread later) or `wait` (a caller waits for it
//!   now), and the thread's id when the device may know it.
//!   `lmgw/timeout_ms` bounds the wait for the `CreateTaskResult` only.
//! - lmgw follows the task with `tasks/get` at the task's `pollInterval`
//!   (else Settings → MCP's `mcp.task_poll_interval_s`), acts at once on
//!   `notifications/tasks/status`, fetches the payload with `tasks/result`
//!   once the task is terminal (one held open while it is
//!   `input_required`), and sends `tasks/cancel` when the owner cancels or
//!   the thread is deleted — at the next link, when there is none now.
//!   `notifications/cancelled` is never sent for a task.
//! - A link that drops ends nothing: the device is polled again as soon as
//!   it links. A device that answers `-32602` for a task's id (it
//!   restarted and forgot it) ends that task as abandoned.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The route's path.
pub const PATH: &str = "/mcp/host";

/// `403`: the key holds no hosting grant (a device without `hosts_label`), or
/// is no device key at all.
pub const HOST_NOT_GRANTED: &str = "host_not_granted";

/// `403`: the upgrade carried an `Origin` header. Devices are native
/// clients; a browser page never opens a host link (§5.6).
pub const CROSS_ORIGIN_REFUSED: &str = "cross_origin_refused";

/// The close of a link another link of the same device took over (§5.3).
/// The reason: "another connection of device 'desktop' took over".
pub const CLOSE_TAKEN_OVER: u16 = crate::realtime::CLOSE_TAKEN_OVER;

/// The close of a link whose key was revoked: disabled, rotated, deleted or
/// expired. The reason starts with the
/// [`RevokeKind`](crate::realtime::RevokeKind) token, as a realtime
/// session's 4003 does.
pub const CLOSE_REVOKED: u16 = crate::realtime::CLOSE_REVOKED;

/// The close of every link when the gateway stops or restarts.
pub const CLOSE_GOING_AWAY: u16 = crate::realtime::CLOSE_GOING_AWAY;

/// RFC 6455's "message too big": a frame or a message over
/// `mcp.host_max_frame_mb` / `mcp.host_max_message_mb`. The reason names
/// the setting.
pub const CLOSE_TOO_BIG: u16 = 1009;

/// RFC 6455's "unexpected condition": no pong within
/// `mcp.host_ping_interval_s`. The reason names the setting.
pub const CLOSE_NO_PONG: u16 = 1011;

/// RFC 6455's "protocol error": a frame the link does not take — a binary
/// frame, a JSON-RPC batch, text that is no JSON-RPC message. The reason
/// says which.
pub const CLOSE_PROTOCOL_ERROR: u16 = 1002;

/// The reason of a [`CLOSE_TAKEN_OVER`] close.
pub fn taken_over_reason(device: &str) -> String {
    format!("another connection of device '{device}' took over")
}

/// The `_meta` key of the principal a forwarded call runs as.
pub const META_CALLER: &str = "lmgw/caller";
/// The `_meta` key of the approval that decided a forwarded call, `null`
/// when none did.
pub const META_APPROVAL: &str = "lmgw/approval";
/// The `_meta` key of the call's timeout in milliseconds: when lmgw stops
/// waiting and sends `notifications/cancelled`.
pub const META_TIMEOUT_MS: &str = "lmgw/timeout_ms";

/// The `_meta` key of a task-augmented call's delivery ([`TaskMeta`]):
/// present only on a `tools/call` that carries `task`.
pub const META_TASK: &str = "lmgw/task";

/// The `_meta` key on the `initialize` lmgw sends over a host link: the
/// link's real limits ([`HostLimits`]), so a device never guesses them.
pub const META_HOST_LIMITS: &str = "lmgw/host_limits";

/// The limits of one host link, as lmgw enforces them (§5.1): read from the
/// effective `mcp.host_*` settings when the link opened, and the row's
/// `timeout_ms`. Sent once, in `initialize`'s `params._meta`
/// ([`META_HOST_LIMITS`]); a setting changed later applies to the next link.
///
/// Every part is optional as read: an older gateway sends none of it, and a
/// part it leaves out is unknown, not unbounded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostLimits {
    /// The largest message lmgw reads, in bytes; `null` = no bound
    /// (`mcp.host_max_message_mb` 0). A larger one closes the link 1009.
    pub max_message_bytes: Option<u64>,
    /// The largest frame lmgw reads, in bytes — always bounded. A larger
    /// one closes the link 1009.
    pub max_frame_bytes: Option<u64>,
    /// How often lmgw pings, in seconds; a ping unanswered by the next one
    /// closes the link 1011. 0 = no pings.
    pub ping_interval_s: Option<u64>,
    /// The row's `timeout_ms`: how long lmgw waits on a call (and on the
    /// handshake) before it cancels; each call also carries it
    /// ([`META_TIMEOUT_MS`]).
    pub call_timeout_ms: Option<u64>,
}

impl HostLimits {
    /// Read them from an `initialize`'s `params._meta`: `None` when the
    /// entry is missing or does not read.
    pub fn from_meta(meta: &Map<String, Value>) -> Option<Self> {
        meta.get(META_HOST_LIMITS)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }
}

/// What kind of principal a call runs as.
///
/// Forward compatible: a kind a newer gateway sends reads as
/// [`CallerKind::Unknown`], as sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum CallerKind {
    /// The owner: an owner key, the dashboard's session.
    Owner,
    /// The gateway's own runs on the owner's behalf: the dashboard's Chat
    /// turns, in-process agent runs.
    Gateway,
    /// A paired device (the one hosting the tool, or another).
    Device,
    /// A client API key.
    Key,
    /// A service agent's token.
    Agent,
    /// No credential. Never stamped while lmgw keeps an anonymous caller
    /// away from device-hosted tools (§5.6); said, should it ever reach
    /// one, so a device reads it as the least trusted caller there is.
    Anonymous,
    /// A kind this build does not know, as sent.
    #[serde(untagged)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unknown(String),
}

/// Who: a principal's kind and its name (a device's short name, a key's
/// name, an agent's id; `lmgw` for the gateway).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Principal {
    pub kind: CallerKind,
    pub name: String,
}

/// `lmgw/approval`: the approval that decided a call, naming who approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Approval {
    /// `approved`: a denied call is never forwarded.
    pub decision: String,
    pub by: Principal,
}

/// Where a task's result goes (MCP Tasks design T4, T15).
///
/// Forward compatible: a delivery a newer gateway sends reads as
/// [`TaskDelivery::Unknown`], as sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum TaskDelivery {
    /// The result enters a stored Chat thread when the task ends, as a tool
    /// result its next turn renders; the call's own answer is
    /// `started, job <task id>`.
    Thread,
    /// A caller with no thread waits for the result now, within the row's
    /// `timeout_ms`; lmgw cancels the task when it stops waiting.
    Wait,
    /// A delivery this build does not know, as sent.
    #[serde(untagged)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unknown(String),
}

/// `lmgw/task`: a task-augmented call's delivery, so a device deciding
/// whether to announce a result knows whether it lands in a thread later
/// or is awaited now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskMeta {
    pub delivery: TaskDelivery,
    /// The thread the result enters; `null` for [`TaskDelivery::Wait`], and
    /// for a thread the device may not know (Admin Chat).
    pub thread_id: Option<i64>,
}

/// The facts lmgw stamps on a forwarded call (§5.5). The device decides
/// what it trusts; lmgw only states them.
///
/// Every part is optional as read ([`CallMeta::from_meta`]): a call that did
/// not come through a host link, or from a gateway that left a part out,
/// reads what it has. A missing caller is an unknown one (trust it least);
/// a missing timeout is no deadline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallMeta {
    pub caller: Option<Principal>,
    pub approval: Option<Approval>,
    /// When lmgw stops waiting, in milliseconds from the call's arrival;
    /// `None` = no deadline. On a task-augmented call: the wait for the
    /// `CreateTaskResult`.
    pub timeout_ms: Option<u64>,
    /// `lmgw/task`, on a task-augmented call only (MCP Tasks design T15).
    pub task: Option<TaskMeta>,
}

impl CallMeta {
    /// The `_meta` entries, to merge into a request's `_meta`: the three of
    /// every call, and `lmgw/task` on a task-augmented one.
    pub fn to_meta(&self) -> Map<String, Value> {
        let mut m = Map::new();
        if let Some(caller) = &self.caller {
            m.insert(
                META_CALLER.into(),
                serde_json::to_value(caller).unwrap_or(Value::Null),
            );
        }
        m.insert(
            META_APPROVAL.into(),
            self.approval
                .as_ref()
                .and_then(|a| serde_json::to_value(a).ok())
                .unwrap_or(Value::Null),
        );
        if let Some(t) = self.timeout_ms {
            m.insert(META_TIMEOUT_MS.into(), Value::from(t));
        }
        if let Some(task) = &self.task {
            m.insert(
                META_TASK.into(),
                serde_json::to_value(task).unwrap_or(Value::Null),
            );
        }
        m
    }

    /// Read them back from a `tools/call`'s `params._meta`: each part that
    /// is there and reads; the rest `None`.
    pub fn from_meta(meta: &Map<String, Value>) -> Self {
        let read = |key: &str| meta.get(key).filter(|v| !v.is_null()).cloned();
        Self {
            caller: read(META_CALLER).and_then(|v| serde_json::from_value(v).ok()),
            approval: read(META_APPROVAL).and_then(|v| serde_json::from_value(v).ok()),
            timeout_ms: read(META_TIMEOUT_MS).and_then(|v| v.as_u64()),
            task: read(META_TASK).and_then(|v| serde_json::from_value(v).ok()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_limits_read_back_and_a_missing_part_is_unknown() {
        let limits = HostLimits {
            max_message_bytes: None,
            max_frame_bytes: Some(16 << 20),
            ping_interval_s: Some(20),
            call_timeout_ms: Some(60_000),
        };
        let mut meta = Map::new();
        meta.insert(
            META_HOST_LIMITS.into(),
            serde_json::to_value(&limits).unwrap(),
        );
        assert_eq!(meta[META_HOST_LIMITS]["max_message_bytes"], Value::Null);
        assert_eq!(HostLimits::from_meta(&meta), Some(limits));
        let mut partial = Map::new();
        partial.insert(
            META_HOST_LIMITS.into(),
            serde_json::json!({"ping_interval_s": 5}),
        );
        let read = HostLimits::from_meta(&partial).unwrap();
        assert_eq!(
            (read.ping_interval_s, read.max_frame_bytes),
            (Some(5), None)
        );
        assert_eq!(HostLimits::from_meta(&Map::new()), None);
    }

    #[test]
    fn the_meta_round_trips_and_reads_an_unknown_kind() {
        let meta = CallMeta {
            caller: Some(Principal {
                kind: CallerKind::Device,
                name: "phone".into(),
            }),
            approval: Some(Approval {
                decision: "approved".into(),
                by: Principal {
                    kind: CallerKind::Owner,
                    name: "dashboard".into(),
                },
            }),
            timeout_ms: Some(60_000),
            task: None,
        };
        let m = meta.to_meta();
        assert_eq!(m[META_CALLER]["kind"], "device");
        assert_eq!(m[META_TIMEOUT_MS], 60_000);
        assert!(!m.contains_key(META_TASK), "only a task-augmented call");
        assert_eq!(CallMeta::from_meta(&m), meta);

        let augmented = CallMeta {
            task: Some(TaskMeta {
                delivery: TaskDelivery::Thread,
                thread_id: Some(812),
            }),
            ..meta.clone()
        };
        let m = augmented.to_meta();
        assert_eq!(
            m[META_TASK],
            serde_json::json!({"delivery": "thread", "thread_id": 812})
        );
        assert_eq!(CallMeta::from_meta(&m), augmented);
        let mut waited = Map::new();
        waited.insert(
            META_TASK.into(),
            serde_json::json!({"delivery": "later", "thread_id": null}),
        );
        assert_eq!(
            CallMeta::from_meta(&waited).task.unwrap().delivery,
            TaskDelivery::Unknown("later".into())
        );

        let mut later = Map::new();
        later.insert(
            META_CALLER.into(),
            serde_json::json!({"kind": "robot", "name": "r2"}),
        );
        later.insert(META_APPROVAL.into(), Value::Null);
        let read = CallMeta::from_meta(&later);
        assert_eq!(
            read.caller.unwrap().kind,
            CallerKind::Unknown("robot".into())
        );
        assert!(read.approval.is_none());
        assert_eq!(read.timeout_ms, None, "absent is no deadline, never 0");

        // A missing caller keeps the other parts.
        let mut partial = Map::new();
        partial.insert(META_TIMEOUT_MS.into(), Value::from(5_000));
        let read = CallMeta::from_meta(&partial);
        assert_eq!((read.caller, read.timeout_ms), (None, Some(5_000)));
    }
}

/// Settings → MCP (`mcp.*` in `GET /api/settings-full`): the host link's
/// limits, each explicit on the link's WebSocket (§5.1), and the MCP Tasks
/// poll interval.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HostSettings {
    /// The largest message a device may send, in MiB; `0` = no bound of its
    /// own (a frame is then bounded by `host_max_frame_mb`). Not `0`
    /// together with `host_max_frame_mb`. A full-desktop screenshot is the
    /// expected large message.
    pub host_max_message_mb: u32,
    /// The largest frame, in MiB; `0` = bounded by `host_max_message_mb`.
    pub host_max_frame_mb: u32,
    /// Seconds between the link's pings; a ping with no pong for a whole
    /// interval closes the link. `0` = no pings, and no bound on a device
    /// that stopped answering.
    pub host_ping_interval_s: u32,
    /// Seconds between two `tasks/get` of an MCP task whose server suggests
    /// no `pollInterval` of its own (MCP Tasks design §5.2); a server's
    /// status notifications act at once. At least `1`.
    pub task_poll_interval_s: u32,
}
