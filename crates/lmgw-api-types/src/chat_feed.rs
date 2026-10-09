//! The Chat change feed's wire shapes:
//! `GET /chat/api/feed`, one SSE stream per client.
//!
//! A **stored** event (threads, folders, profiles, approvals, tasks) has an SSE
//! `id:` — its [`Cursor`] — and is rendered when it is delivered:
//! `thread.created` and `thread.updated` carry the thread as
//! `GET /chat/api/threads` lists it now, with the change's author beside it
//! as `by`, or a [`ThreadGone`] once the thread is gone; `profile.*` carry
//! the profile's id and name as the write left them ([`FeedProfile`]);
//! `approval.*` the call as the reply's pending state holds it
//! ([`ApprovalRequested`], [`ApprovalDecided`]); `task.*` an MCP task a
//! turn started and its result entering the thread, as the record's write
//! left them ([`TaskStarted`], [`TaskDone`]); `device.revoked` a paired
//! device whose key was deleted ([`DeviceRevoked`]). A **live** event (`turn.*`, `voice.*`, `hold`, `state`,
//! `resync`, `revoked`) has no `id:`; what is live now is in [`Hello`] and
//! [`LiveState`].
//!
//! The shapes the gateway writes and a client reads, so the two cannot
//! drift: a sans-IO client needs nothing else to follow the feed.

// The client-apps design record, §2.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The `event:` names.
pub mod event {
    pub const HELLO: &str = "hello";
    pub const STATE: &str = "state";
    pub const RESYNC: &str = "resync";
    pub const REVOKED: &str = "revoked";
    pub const TURN_STARTED: &str = "turn.started";
    pub const TURN_DONE: &str = "turn.done";
    pub const VOICE_BOUND: &str = "voice.bound";
    pub const VOICE_ENDED: &str = "voice.ended";
    pub const HOLD: &str = "hold";
    pub const THREAD_CREATED: &str = "thread.created";
    pub const THREAD_UPDATED: &str = "thread.updated";
    pub const THREAD_DELETED: &str = "thread.deleted";
    pub const FOLDER_CREATED: &str = "folder.created";
    pub const FOLDER_UPDATED: &str = "folder.updated";
    pub const FOLDER_DELETED: &str = "folder.deleted";
    pub const FOLDER_CURRENT: &str = "folder.current";
    pub const PROFILE_CREATED: &str = "profile.created";
    pub const PROFILE_UPDATED: &str = "profile.updated";
    pub const PROFILE_DELETED: &str = "profile.deleted";
    pub const APPROVAL_REQUESTED: &str = "approval.requested";
    pub const APPROVAL_DECIDED: &str = "approval.decided";
    pub const TASK_STARTED: &str = "task.started";
    pub const TASK_DONE: &str = "task.done";
    pub const DEVICE_REVOKED: &str = "device.revoked";
}

// Review W4-1.
/// The most live events `chat_feed_live_buffer` may hold.
///
/// The buffer is allocated whole when it is made and again on every
/// resize: tokio's broadcast ring rounds the size up to a power of two and
/// reserves every slot up front, about 80 bytes a slot, plus the event a
/// filled slot keeps (a few hundred bytes of JSON). 65 536 slots are about
/// 5 MiB up front and some tens of MiB full, while live events come a few
/// per turn, so a client this far behind has long been sent a fresh
/// `state`. An unbounded value could ask for hundreds of GB at a save, and
/// again at every start.
// Shared with the dashboard, which shows it as the field's maximum.
pub const MAX_LIVE_BUFFER: u32 = 65_536;

// Review W4-1.
/// The most records `chat_feed_page_size` may read per catch-up query: a
/// page is held in memory, each record rendered as the
/// thread or folder it names (a few KB at most), so 10 000 records are some
/// tens of MiB for one catching-up client. Every record is still sent,
/// page after page.
pub const MAX_PAGE_SIZE: u32 = 10_000;

/// A position in the feed, `"<epoch>:<seq>:<tag>"`: what a stored event's
/// `id:` is, what `hello.cursor` is, and what `?since=` and `Last-Event-ID`
/// take. A client treats it as opaque text and hands it back as it got it.
/// The epoch is minted once per database, and the tag checks the record at
/// `seq` (a copy of the database restored from before the cursor writes
/// other records at the same numbers); a cursor from another database, or
/// one whose record does not match, is answered with a `resync`. A cursor
/// without a tag (`"<epoch>:<seq>"`) is still read, checked by its numbers
/// alone; `"<epoch>:0"` is the start of an empty feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: String,
    pub seq: i64,
    pub tag: Option<String>,
}

impl Cursor {
    /// `"<epoch>:<seq>"` or `"<epoch>:<seq>:<tag>"`: the epoch non-empty,
    /// the sequence a number of zero or more, the tag letters and digits;
    /// `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.trim().splitn(3, ':');
        let epoch = parts.next()?;
        let seq: i64 = parts.next()?.parse().ok()?;
        let tag = match parts.next() {
            None => None,
            Some(t) if !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric()) => {
                Some(t.to_string())
            }
            Some(_) => return None,
        };
        (!epoch.is_empty() && seq >= 0).then(|| Self {
            epoch: epoch.to_string(),
            seq,
            tag,
        })
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.epoch, self.seq)?;
        match &self.tag {
            Some(t) => write!(f, ":{t}"),
            None => Ok(()),
        }
    }
}

/// Who the feed is for: its kind and the name it is known by, without the
/// kind's prefix. A paired device is kind `device`, named as it was paired
/// ("desktop", not "device:desktop"); the gateway's administrator has the
/// other kind: the dashboard (named `dashboard`) or an admin key (named as
/// the key without its prefix).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FeedPrincipal {
    #[cfg_attr(feature = "schema", schemars(extend("enum" = ["device", "owner"])))]
    pub kind: String,
    pub name: String,
}

/// How the gateway's administrator is named as the author of a change, the
/// `by` of every event the dashboard or an admin key causes.
pub const BY_ADMIN: &str = "the dashboard";

/// How a paired device is named as the author of a change: `device 'phone'`
/// for the device paired as `phone`.
pub fn by_device(name: &str) -> String {
    format!("device '{name}'")
}

impl FeedPrincipal {
    /// The `by` the events this principal causes carry: a client tells its
    /// own changes from another client's by it.
    pub fn by(&self) -> String {
        if self.kind == "device" {
            by_device(&self.name)
        } else {
            BY_ADMIN.to_string()
        }
    }
}

/// The GPU hold: whether local models are paused, and the alias that answers
/// for them meanwhile. The `hold` event and part of
/// [`Hello`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FeedHold {
    pub active: bool,
    pub fallback_alias: Option<String>,
}

/// A realtime session bound to a thread now, and who bound it ("the
/// dashboard", "device 'phone'").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LiveVoice {
    pub thread_id: i64,
    pub by: String,
}

/// A turn answering a thread now, who started it, and whether it is a
/// bound realtime session's voice turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LiveTurn {
    pub thread_id: i64,
    pub by: String,
    pub voice: bool,
}

/// The feed's first event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Hello {
    /// This database's epoch.
    pub epoch: String,
    /// Where the stream continues from: the cursor the client resumed with,
    /// or the newest record when it gave none (or one the feed answers with
    /// `resync`).
    pub cursor: String,
    /// Seconds between keep-alive comments (`chat_feed_keepalive_s`, fixed
    /// for this stream): a client derives its dead-link timeout from it.
    pub keepalive_s: u32,
    /// How many days the feed keeps its records (`chat_feed_retention_days`,
    /// `0` = all): a client away longer gets a `resync`.
    pub retention_days: i64,
    pub principal: FeedPrincipal,
    /// The label a device may host MCP tools under; `null` without a
    /// hosting grant, and on a feed that is not a device's.
    pub hosts_label: Option<String>,
    /// What this device's admin tools may do now: `off`, `read_only` (read
    /// lmgw's configuration and state) or `full` (also change it) — the
    /// level set on the device, capped by the gateway's own self-admin
    /// level. Above `off` the device sees the threads and folders that
    /// carry the self-admin toolset (`lmgw`), and at `full` it may attach it
    /// and change them; at `read_only` it reads them and sends to them only;
    /// at `off` it sees none of them. When this value moves above `off` or back while
    /// the feed is open, whether the device's level or the gateway's moved
    /// it, those threads and folders arrive (`*.created`) or go
    /// (`*.deleted`) in order. A `state` with this value follows any change
    /// of the device's level, and any change of the gateway's that moves
    /// this value. When this value moved above `off` or back within what a
    /// resumed feed catches up, the feed sends `resync` at that point
    /// instead: reload what you show. `off` on a feed that is not a
    /// device's.
    pub self_admin: crate::AdminLevel,
    pub hold: FeedHold,
    /// The realtime sessions bound now.
    pub voice: Vec<LiveVoice>,
    /// The turns running now.
    pub turns: Vec<LiveTurn>,
}

/// `state`: [`Hello`]'s live part again. Sent when the stream could not
/// keep a client's live events in order — it read slower than they
/// happened past `chat_feed_live_buffer`, or the buffer was resized — and
/// to a device when what it may see or do changed: a thread it reaches took
/// or lost the self-admin toolset, its own admin-tools level moved, or the
/// gateway's self-admin level moved what its admin tools may do. Stored
/// events are never lost; `reason` says what happened and names the
/// setting.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LiveState {
    /// [`Hello::self_admin`] as it is now: a device learns here that what
    /// its admin tools may do moved while the feed is open.
    pub self_admin: crate::AdminLevel,
    pub hold: FeedHold,
    pub voice: Vec<LiveVoice>,
    pub turns: Vec<LiveTurn>,
    pub reason: Option<String>,
}

/// `turn.started`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TurnStarted {
    pub thread_id: i64,
    pub by: String,
    /// A bound realtime session's voice turn.
    pub voice: bool,
}

/// `turn.done`: how a turn ended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TurnDone {
    pub thread_id: i64,
    /// The reply row it saved or continued; `null` when it saved none.
    pub message_id: Option<i64>,
    pub saved: bool,
    /// The code of the last error the turn reported (`superseded`,
    /// `gpu_hold`, `not_saved`, …); `null` when it reported none.
    pub code: Option<String>,
}

/// `voice.bound`: a realtime session bound to the thread.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceBound {
    pub thread_id: i64,
    pub by: String,
}

/// `voice.ended`: a bound session let its thread go.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceEnded {
    pub thread_id: i64,
    /// Who had bound the session that ended.
    pub by: String,
    /// `closed`, `taken_over`, `thread_gone` (its thread was deleted while
    /// it was bound; a device's feed does not receive this event for a
    /// deleted thread, only its `thread.deleted`, as for a thread taken out
    /// of its reach) or `revoked` (the binder's key was revoked, or the
    /// thread was taken out of the binding device's reach).
    pub reason: String,
    /// Who took it over, for `taken_over`.
    pub taken_over_by: Option<String>,
}

/// `resync`: the cursor cannot be honoured; reload what you show. The
/// stream continues from now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Resync {
    pub reason: String,
}

/// `revoked`: the key was disabled, rotated, deleted or expired; the stream
/// ends after it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Revoked {
    /// `disabled`, `rotated`, `deleted` or `expired`.
    pub reason: String,
    /// The reason in a sentence: "device 'phone' was rotated — pair it
    /// again".
    pub message: String,
    /// What a client does about it, the token a realtime session's 4003
    /// close starts its reason with: `key_unknown` means pair the device
    /// again. Every event this gateway sends carries it. An event without it
    /// (an older gateway's) is of an unknown kind, as a close reason without
    /// a token is: tell the user the sentence in `message`.
    // Absent reads as an empty `RevokeKind::Unknown` (`#[serde(default)]`),
    // which is no value of the schema's enum, so the schema states no
    // default (the branch review's verification, V-8).
    #[cfg_attr(feature = "schema", schemars(transform = no_default))]
    pub kind: crate::realtime::RevokeKind,
}

/// A field's schema without the `default` the struct's `#[serde(default)]`
/// gives it, where that default is no value the schema allows.
#[cfg(feature = "schema")]
fn no_default(schema: &mut schemars::Schema) {
    schema.remove("default");
}

/// `thread.deleted`, and `thread.created` / `thread.updated` for a thread
/// that is gone by the time the event is delivered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadGone {
    pub thread_id: i64,
    pub deleted: bool,
    /// Who made the change; `null` for the gateway's own (the sweep).
    pub by: Option<String>,
}

/// `folder.deleted`, and `folder.created` / `folder.updated` for a folder
/// that is gone by the time the event is delivered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FolderGone {
    pub folder_id: i64,
    pub deleted: bool,
    pub by: Option<String>,
}

/// `folder.current`: an ongoing folder's current thread moved.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FolderCurrent {
    pub folder_id: i64,
    /// The current thread now; `null` when the folder has none any more.
    pub thread_id: Option<i64>,
    pub previous_thread_id: Option<i64>,
    /// Why it moved. To a new thread: `first` (the folder had none the
    /// reader can reach), `gone` (its thread could no longer be continued
    /// in it; only the dashboard and admin keys read this reason for a new
    /// thread), `idle` (no message for the folder's idle minutes) or
    /// `requested` (a client asked for a new thread, or created one in the
    /// folder). To none: `gone` (the thread was deleted, moved out of the
    /// folder or archived) or `not_ongoing` (the folder stopped being an
    /// ongoing conversation).
    pub reason: String,
    pub by: Option<String>,
}

/// `profile.created`, `profile.updated` and `profile.deleted`: a
/// personality profile (`GET /chat/api/profiles`) was created, changed or
/// deleted, named as that write left it. Every reader receives them,
/// devices included. After a `resync` the feed sends the whole list again
/// as `profile.created` events (with no `id:`), so a client that keeps the
/// names (a menu of profiles) has them again without a request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FeedProfile {
    /// The profile's id.
    pub id: i64,
    pub name: String,
    /// Who made the change; `null` for the gateway's own, and on the list
    /// repeated after a `resync`.
    pub by: Option<String>,
}

/// `approval.requested`: a turn in the thread stopped on a call that waits
/// for an approval (client-apps design §6.5). Rendered from the reply's
/// pending state at delivery; a reply gone by then (deleted, regenerated)
/// is not delivered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApprovalRequested {
    pub thread_id: i64,
    /// The reply that waits.
    pub message_id: i64,
    pub approval_request_id: String,
    pub server_label: String,
    /// The tool's own name, as [`crate::chat_approvals::ApprovalRequest`].
    pub name: String,
    /// A JSON string.
    pub arguments: String,
    /// The call's id, as the turn's `ready`, `approval` and `result` frames
    /// carry it. Absent from an lmgw before 2026-10-09.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

/// `approval.decided`: a waiting call was decided — approved or declined
/// by a client, or declined because a new message came
/// ([`crate::chat_approvals::MOVED_ON`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApprovalDecided {
    pub thread_id: i64,
    pub message_id: i64,
    pub approval_request_id: String,
    /// Whether the call was approved — and, once that is known, could run:
    /// `false` with [`Self::not_run`] for one approved whose turn could not
    /// start.
    pub approve: bool,
    /// Who decided, as every author is named ("the dashboard", "device
    /// 'phone'").
    pub by: Option<String>,
    /// Approved, but the turn that was to run it could not start (its
    /// reply moved on, its starter went, its slot was refused), so it never
    /// ran and its result says so. Recorded as a second `approval.decided`
    /// for the call; both render this.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub not_run: bool,
    /// The call's id, as `approval.requested` has it. Absent from an lmgw
    /// before 2026-10-09.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

/// `task.started`: a tool call of a turn in the thread became an MCP task
/// (MCP Tasks design §4.1); its result enters the thread later. Named as
/// the write left it, so it renders after the task is gone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskStarted {
    pub thread_id: i64,
    /// lmgw's id of the task (the cancel route's `{task}`).
    pub id: i64,
    /// The server's id of the task.
    pub task_id: String,
    pub server_label: String,
    /// The name the model called.
    pub tool: String,
    /// Who started the turn that called it.
    pub by: Option<String>,
}

/// `task.done`: a task's result entered the thread as message `message_id`
/// (role `tool`). `by` names who cancelled it, for a cancel lmgw sent;
/// `null` otherwise.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskDone {
    pub thread_id: i64,
    /// The result row.
    pub message_id: i64,
    pub id: i64,
    pub task_id: String,
    pub server_label: String,
    pub tool: String,
    /// `completed`, `failed`, `cancelled` or `abandoned`.
    #[cfg_attr(
        feature = "schema",
        schemars(extend("enum" = ["completed", "failed", "cancelled", "abandoned"]))
    )]
    pub status: String,
    pub by: Option<String>,
}

/// `device.revoked`: a paired device's key was deleted, so the device is
/// gone for good; a client that runs work it started — a host of MCP tasks
/// whose `lmgw/caller` named it — may cancel that work now (MCP Tasks
/// design §4.1; client-apps design §2.2). Its tasks' results still enter
/// their threads as they end.
///
/// Received by the dashboard and admin keys; by a device the feed showed
/// the deleted device's changes to (a stored event it may see carries the
/// deleted device's `by`), and by a device hosting a task the deleted
/// device started that was still running. No other device hears of it: it
/// names no device the reader was not shown.
///
/// Only a delete is said: a disabled or expired key can be enabled or
/// renewed again, and its tasks wait meanwhile; a rotated one keeps its
/// row and pairs again under its name; a cleared hosting grant ends the
/// device's own hosted tasks, each said by its `task.done` (`abandoned`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceRevoked {
    /// The deleted device, as [`Hello::principal`] names a device (kind
    /// `device`, its paired name) and as `lmgw/caller` names it on a
    /// forwarded call; [`FeedPrincipal::by`] is the `by` its changes
    /// carried.
    pub device: FeedPrincipal,
    /// Who deleted it ("the dashboard").
    pub by: Option<String>,
}

/// `thread.created` / `thread.updated` for a thread that exists at delivery:
/// the thread as `GET /chat/api/threads` lists it now, with `by` beside its
/// fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadChanged {
    #[serde(flatten)]
    pub thread: crate::chat::ThreadRow,
    /// Who made the change; `null` for the gateway's own, such as the sweep.
    pub by: Option<String>,
}

/// `folder.created` / `folder.updated` for a folder that exists at
/// delivery: the folder as `GET /chat/api/folders` lists it now (its counts
/// as the reader may see them), with `by` beside its fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FolderChanged {
    #[serde(flatten)]
    pub folder: crate::chat::Folder,
    /// Who made the change; `null` for the gateway's own.
    pub by: Option<String>,
}

/// What a `thread.created` or `thread.updated` event carries: the thread as
/// it is now, or its tombstone when it is gone by the time the event is
/// delivered.
#[derive(Debug, Clone, PartialEq)]
// Owned values, no boxes: a client (or an FFI wrapper) matches them as they are.
#[allow(clippy::large_enum_variant)]
pub enum ThreadNow {
    Row(ThreadChanged),
    Gone(ThreadGone),
}

/// What a `folder.created` or `folder.updated` event carries.
#[derive(Debug, Clone, PartialEq)]
// Owned values, no boxes: a client (or an FFI wrapper) matches them as they are.
#[allow(clippy::large_enum_variant)]
pub enum FolderNow {
    Row(FolderChanged),
    Gone(FolderGone),
}

/// One event of the feed, by its `event:` name, its data typed.
///
/// Non-exhaustive: a newer gateway adds events (messages), and
/// a client built against this one reads them as [`FeedEvent::Unknown`].
#[derive(Debug, Clone, PartialEq)]
// Owned values, no boxes: a client (or an FFI wrapper) matches them as they are.
#[allow(clippy::large_enum_variant)]
#[non_exhaustive]
pub enum FeedEvent {
    Hello(Hello),
    ThreadCreated(ThreadNow),
    ThreadUpdated(ThreadNow),
    ThreadDeleted(ThreadGone),
    FolderCreated(FolderNow),
    FolderUpdated(FolderNow),
    FolderDeleted(FolderGone),
    FolderCurrent(FolderCurrent),
    ProfileCreated(FeedProfile),
    ProfileUpdated(FeedProfile),
    ProfileDeleted(FeedProfile),
    ApprovalRequested(ApprovalRequested),
    ApprovalDecided(ApprovalDecided),
    TaskStarted(TaskStarted),
    TaskDone(TaskDone),
    DeviceRevoked(DeviceRevoked),
    TurnStarted(TurnStarted),
    TurnDone(TurnDone),
    VoiceBound(VoiceBound),
    VoiceEnded(VoiceEnded),
    Hold(FeedHold),
    State(LiveState),
    Resync(Resync),
    Revoked(Revoked),
    /// An event this build does not know (a newer gateway's), as sent: a
    /// client skips it, and may log it.
    Unknown {
        event: String,
        data: String,
    },
}

/// A known event whose data does not read as its type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedEventError {
    pub event: String,
    pub message: String,
}

impl std::fmt::Display for FeedEventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "feed event '{}' unreadable: {}",
            self.event, self.message
        )
    }
}

impl std::error::Error for FeedEventError {}

/// Every `event:` name [`FeedEvent`] reads.
pub const EVENTS: &[&str] = &[
    event::HELLO,
    event::THREAD_CREATED,
    event::THREAD_UPDATED,
    event::THREAD_DELETED,
    event::FOLDER_CREATED,
    event::FOLDER_UPDATED,
    event::FOLDER_DELETED,
    event::FOLDER_CURRENT,
    event::PROFILE_CREATED,
    event::PROFILE_UPDATED,
    event::PROFILE_DELETED,
    event::APPROVAL_REQUESTED,
    event::APPROVAL_DECIDED,
    event::TASK_STARTED,
    event::TASK_DONE,
    event::DEVICE_REVOKED,
    event::TURN_STARTED,
    event::TURN_DONE,
    event::VOICE_BOUND,
    event::VOICE_ENDED,
    event::HOLD,
    event::STATE,
    event::RESYNC,
    event::REVOKED,
];

impl FeedEvent {
    /// The event `name` with `data` (an SSE record's `event:` and `data:`).
    /// An unknown name is [`FeedEvent::Unknown`]; a known one whose data
    /// does not read is an error naming it. A value a newer gateway added to
    /// one of the data's enums reads as that enum's `Unknown`, so it does
    /// not make the event unreadable.
    pub fn parse(name: &str, data: &str) -> Result<Self, FeedEventError> {
        fn read<T: serde::de::DeserializeOwned>(
            name: &str,
            data: &str,
        ) -> Result<T, FeedEventError> {
            serde_json::from_str(data).map_err(|e| FeedEventError {
                event: name.to_string(),
                message: e.to_string(),
            })
        }
        fn gone(name: &str, data: &str) -> Result<bool, FeedEventError> {
            let v: serde_json::Value = read(name, data)?;
            Ok(v.get("deleted").and_then(serde_json::Value::as_bool) == Some(true))
        }
        fn thread(name: &str, data: &str) -> Result<ThreadNow, FeedEventError> {
            Ok(if gone(name, data)? {
                ThreadNow::Gone(read(name, data)?)
            } else {
                ThreadNow::Row(read(name, data)?)
            })
        }
        fn folder(name: &str, data: &str) -> Result<FolderNow, FeedEventError> {
            Ok(if gone(name, data)? {
                FolderNow::Gone(read(name, data)?)
            } else {
                FolderNow::Row(read(name, data)?)
            })
        }
        Ok(match name {
            event::HELLO => Self::Hello(read(name, data)?),
            event::THREAD_CREATED => Self::ThreadCreated(thread(name, data)?),
            event::THREAD_UPDATED => Self::ThreadUpdated(thread(name, data)?),
            event::THREAD_DELETED => Self::ThreadDeleted(read(name, data)?),
            event::FOLDER_CREATED => Self::FolderCreated(folder(name, data)?),
            event::FOLDER_UPDATED => Self::FolderUpdated(folder(name, data)?),
            event::FOLDER_DELETED => Self::FolderDeleted(read(name, data)?),
            event::FOLDER_CURRENT => Self::FolderCurrent(read(name, data)?),
            event::PROFILE_CREATED => Self::ProfileCreated(read(name, data)?),
            event::PROFILE_UPDATED => Self::ProfileUpdated(read(name, data)?),
            event::PROFILE_DELETED => Self::ProfileDeleted(read(name, data)?),
            event::APPROVAL_REQUESTED => Self::ApprovalRequested(read(name, data)?),
            event::APPROVAL_DECIDED => Self::ApprovalDecided(read(name, data)?),
            event::TASK_STARTED => Self::TaskStarted(read(name, data)?),
            event::TASK_DONE => Self::TaskDone(read(name, data)?),
            event::DEVICE_REVOKED => Self::DeviceRevoked(read(name, data)?),
            event::TURN_STARTED => Self::TurnStarted(read(name, data)?),
            event::TURN_DONE => Self::TurnDone(read(name, data)?),
            event::VOICE_BOUND => Self::VoiceBound(read(name, data)?),
            event::VOICE_ENDED => Self::VoiceEnded(read(name, data)?),
            event::HOLD => Self::Hold(read(name, data)?),
            event::STATE => Self::State(read(name, data)?),
            event::RESYNC => Self::Resync(read(name, data)?),
            event::REVOKED => Self::Revoked(read(name, data)?),
            other => Self::Unknown {
                event: other.to_string(),
                data: data.to_string(),
            },
        })
    }

    /// Its `event:` name.
    pub fn name(&self) -> &str {
        match self {
            Self::Hello(_) => event::HELLO,
            Self::ThreadCreated(_) => event::THREAD_CREATED,
            Self::ThreadUpdated(_) => event::THREAD_UPDATED,
            Self::ThreadDeleted(_) => event::THREAD_DELETED,
            Self::FolderCreated(_) => event::FOLDER_CREATED,
            Self::FolderUpdated(_) => event::FOLDER_UPDATED,
            Self::FolderDeleted(_) => event::FOLDER_DELETED,
            Self::FolderCurrent(_) => event::FOLDER_CURRENT,
            Self::ProfileCreated(_) => event::PROFILE_CREATED,
            Self::ProfileUpdated(_) => event::PROFILE_UPDATED,
            Self::ProfileDeleted(_) => event::PROFILE_DELETED,
            Self::ApprovalRequested(_) => event::APPROVAL_REQUESTED,
            Self::ApprovalDecided(_) => event::APPROVAL_DECIDED,
            Self::TaskStarted(_) => event::TASK_STARTED,
            Self::TaskDone(_) => event::TASK_DONE,
            Self::DeviceRevoked(_) => event::DEVICE_REVOKED,
            Self::TurnStarted(_) => event::TURN_STARTED,
            Self::TurnDone(_) => event::TURN_DONE,
            Self::VoiceBound(_) => event::VOICE_BOUND,
            Self::VoiceEnded(_) => event::VOICE_ENDED,
            Self::Hold(_) => event::HOLD,
            Self::State(_) => event::STATE,
            Self::Resync(_) => event::RESYNC,
            Self::Revoked(_) => event::REVOKED,
            Self::Unknown { event, .. } => event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_reads_by_its_name() {
        for name in EVENTS {
            let data = match *name {
                event::THREAD_CREATED | event::THREAD_UPDATED => r#"{"id": 4, "by": null}"#,
                _ => "{}",
            };
            let ev = FeedEvent::parse(name, data).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(ev.name(), *name);
        }
        let ev = FeedEvent::parse("message.created", r#"{"x":1}"#).unwrap();
        assert_eq!(
            ev,
            FeedEvent::Unknown {
                event: "message.created".into(),
                data: r#"{"x":1}"#.into()
            }
        );
        let e = FeedEvent::parse(event::TURN_DONE, "not json").unwrap_err();
        assert_eq!(e.event, "turn.done");
    }

    #[test]
    fn a_thread_event_is_the_row_or_its_tombstone() {
        let ev = FeedEvent::parse(
            event::THREAD_UPDATED,
            r#"{"id": 9, "title": "Plan", "folder_id": 2, "by": "device 'phone'"}"#,
        )
        .unwrap();
        let FeedEvent::ThreadUpdated(ThreadNow::Row(t)) = ev else {
            panic!("a row: {ev:?}")
        };
        assert_eq!((t.thread.id, t.thread.title.as_str()), (9, "Plan"));
        assert_eq!(t.thread.folder_id, Some(2));
        assert_eq!(t.by.as_deref(), Some("device 'phone'"));
        let ev = FeedEvent::parse(
            event::THREAD_CREATED,
            r#"{"thread_id": 9, "deleted": true, "by": null}"#,
        )
        .unwrap();
        assert_eq!(
            ev,
            FeedEvent::ThreadCreated(ThreadNow::Gone(ThreadGone {
                thread_id: 9,
                deleted: true,
                by: None
            }))
        );
        let ev = FeedEvent::parse(
            event::FOLDER_UPDATED,
            r#"{"folder_id": 3, "deleted": true, "by": "the dashboard"}"#,
        )
        .unwrap();
        assert!(matches!(ev, FeedEvent::FolderUpdated(FolderNow::Gone(_))));
    }

    #[test]
    fn a_profile_event_names_the_profile() {
        let ev = FeedEvent::parse(
            event::PROFILE_UPDATED,
            r#"{"id": 3, "name": "Concise", "by": "device 'desktop'"}"#,
        )
        .unwrap();
        assert_eq!(
            ev,
            FeedEvent::ProfileUpdated(FeedProfile {
                id: 3,
                name: "Concise".into(),
                by: Some("device 'desktop'".into()),
            })
        );
        assert_eq!(ev.name(), "profile.updated");
    }

    #[test]
    fn a_device_revoked_event_names_the_device_as_a_principal() {
        let ev = FeedEvent::parse(
            event::DEVICE_REVOKED,
            r#"{"device": {"kind": "device", "name": "phone"}, "by": "the dashboard"}"#,
        )
        .unwrap();
        let FeedEvent::DeviceRevoked(d) = &ev else {
            panic!("device.revoked: {ev:?}")
        };
        assert_eq!(d.device.by(), "device 'phone'");
        assert_eq!(d.by.as_deref(), Some(BY_ADMIN));
        assert_eq!(ev.name(), "device.revoked");
    }

    #[test]
    fn a_principal_knows_its_own_by() {
        let device = FeedPrincipal {
            kind: "device".into(),
            name: "desktop".into(),
        };
        assert_eq!(device.by(), "device 'desktop'");
        let admin = FeedPrincipal {
            kind: "owner".into(),
            name: "cli".into(),
        };
        assert_eq!(admin.by(), BY_ADMIN);
    }

    #[test]
    fn a_cursor_round_trips_and_refuses_what_is_not_one() {
        let c = Cursor::parse("0a1b:42:9f00c1d2").unwrap();
        assert_eq!(
            c,
            Cursor {
                epoch: "0a1b".into(),
                seq: 42,
                tag: Some("9f00c1d2".into()),
            }
        );
        assert_eq!(c.to_string(), "0a1b:42:9f00c1d2");
        let untagged = Cursor::parse("0a1b:42").unwrap();
        assert_eq!((untagged.tag, untagged.seq), (None, 42));
        assert_eq!(Cursor::parse("e:0").unwrap().to_string(), "e:0");
        for bad in [
            "", "42", ":42", "e:", "e:x", "e:-1", "e:4:", "e:4:a-b", "e:4:a:b",
        ] {
            assert_eq!(Cursor::parse(bad), None, "{bad}");
        }
    }
}
