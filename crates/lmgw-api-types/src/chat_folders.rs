//! An ongoing-conversation folder's wire shapes: the folder's `ongoing`
//! field, what a folder create or patch takes for it, and
//! `POST /chat/api/folders/{id}/current`'s body and answer.
//!
//! A folder marked as one ongoing conversation has a **current thread**:
//! the thread every client continues in. `current` answers it, and starts a
//! new one when the folder has none, when the conversation was idle longer
//! than the folder says, or when a client asks. Every change of the current
//! thread is a `folder.current` event in the change feed.
// The client-apps design record, §3.

use serde::{Deserialize, Serialize};

use crate::chat::present;

/// The `ongoing` field of a folder: `null` for a folder that is not an
/// ongoing conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FolderOngoing {
    /// Minutes without a message after which the next `current` call starts
    /// a new thread. `0`: a new thread only when a client asks for one.
    pub idle_minutes: i64,
    /// The thread the conversation continues in; `null` until the first
    /// `current` call, after its thread was deleted, moved out of the
    /// folder or archived by hand, and, for a device key, while the thread
    /// is out of the key's reach.
    pub current_thread_id: Option<i64>,
}

/// `ongoing` in a folder create or patch: an object marks the folder as one
/// ongoing conversation (its defaults must name a model), `null` ends that.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OngoingInput {
    /// Minutes without a message after which a new thread starts; `0`: only
    /// when a client asks. A whole number of zero or more.
    pub idle_minutes: i64,
}

/// `POST /chat/api/folders/{id}/current`'s body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CurrentRequest {
    /// Start a new thread now. A current thread without any message is
    /// answered instead, so no empty threads pile up.
    pub new: bool,
}

/// Why `current` answered with a new thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum CurrentReason {
    /// The folder had no current thread (for a device key: none it can
    /// reach).
    First,
    /// Its current thread can no longer be continued in the folder. Only
    /// the dashboard and admin keys are given this reason.
    Gone,
    /// Its current thread had no message for the folder's idle minutes.
    Idle,
    /// The caller asked for a new thread (`new: true`).
    Requested,
    /// A reason this build does not know (a newer gateway's); `note` says
    /// it in words.
    #[serde(other)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unknown,
}

impl CurrentReason {
    /// The name the feed's `folder.current` carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Gone => "gone",
            Self::Idle => "idle",
            Self::Requested => "requested",
            Self::Unknown => "unknown",
        }
    }
}

/// `POST /chat/api/folders/{id}/current`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CurrentThread {
    /// The current thread, as `GET /chat/api/threads/{id}` answers its
    /// `thread`.
    #[serde(serialize_with = "crate::chat::wire_order")]
    pub thread: crate::chat::Thread,
    /// Whether this call started it.
    pub rolled_over: bool,
    /// Why this call started it; `null` when it did not.
    pub reason: Option<CurrentReason>,
    /// The same in a sentence that names the folder setting behind it (the
    /// idle minutes, for `idle`); `null` when this call did not start it.
    pub note: Option<String>,
}

/// What a folder patch applied to the folder's current thread (`applied`
/// in the patch's answer): the thread and the settings it changed, as the
/// thread settings name them (`temperature`, `voice.tts_alias`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppliedToCurrent {
    pub thread_id: i64,
    pub fields: Vec<String>,
}

/// `POST /chat/api/folders/{id}`'s body: a **patch**. An absent field is
/// unchanged. Unknown fields are a 422 bad_request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FolderPatch {
    /// The new name; names need not be unique.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The folder's place in the list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<i64>,
    /// Replaces the folder's defaults whole (`{}` clears them). Not beside
    /// `defaults_patch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<crate::chat::ThreadDefaults>")
    )]
    pub defaults: Option<serde_json::Value>,
    /// Changes the defaults field by field instead: each field given
    /// replaces the stored one (`null` unsets it), the others stay as
    /// stored, so a save of the fields one client changed never writes back
    /// what another changed meanwhile. `voice` is laid field by field the
    /// same way (`voice: null` unsets the whole voice). Not beside
    /// `defaults`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<crate::chat::ThreadDefaults>")
    )]
    pub defaults_patch: Option<serde_json::Value>,
    /// An object marks the folder as one ongoing conversation (or changes
    /// its idle minutes; its defaults must name a model); `null` ends that.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub ongoing: Option<Option<OngoingInput>>,
    /// The folder's own retention in days before an inactive thread is
    /// archived; `null` goes back to the Chat's setting, `0` never. A device
    /// key cannot set it (403 `forbidden`).
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub archive_days: Option<Option<i64>>,
    /// Days after archiving before a thread is deleted; as `archive_days`.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub purge_days: Option<Option<i64>>,
    /// `false` shows a folder that a device's delete hid from devices to
    /// them again; the owner's alone. `true` is refused (400): a device's
    /// delete is what hides a folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devices_hidden: Option<bool>,
    /// For an ongoing folder: also apply the defaults' changes to its
    /// current thread. Default `true`.
    #[serde(default = "apply_by_default", skip_serializing_if = "is_true")]
    pub apply_to_current: bool,
}

fn apply_by_default() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

impl Default for FolderPatch {
    fn default() -> Self {
        Self {
            name: None,
            sort: None,
            defaults: None,
            defaults_patch: None,
            ongoing: None,
            archive_days: None,
            purge_days: None,
            devices_hidden: None,
            apply_to_current: true,
        }
    }
}

/// What a folder delete does with the threads in the folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ThreadsFate {
    /// The threads stay, in no folder.
    Keep,
    /// The threads are deleted with the folder.
    Delete,
}

/// `POST /chat/api/folders/{id}/delete`'s body. The choice is required:
/// there is no default for destroying conversations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FolderDelete {
    pub threads: ThreadsFate,
}

/// `POST /chat/api/folders/{id}`'s answer: the folder as the list carries
/// it, and what the patch applied to its current thread.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FolderPatched {
    #[serde(flatten)]
    pub folder: crate::chat::Folder,
    /// For an ongoing folder whose defaults changed: the current thread
    /// and the settings that reached it; `null` otherwise.
    pub applied: Option<AppliedToCurrent>,
}
