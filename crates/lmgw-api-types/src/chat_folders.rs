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
