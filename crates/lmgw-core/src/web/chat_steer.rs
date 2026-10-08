//! What a paired device whose admin tools are below `full` may not change in
//! a thread or folder that carries lmgw's admin tools (client-apps design
//! L5's note, 2026-10-07).
//!
//! At `read_only` a device reads such a thread and runs turns in it, which
//! run at its own level. It may not change what drives the owner's later
//! turns there, which run the write tools at the gateway's level: the
//! thread's settings (its system prompt and tools among them), its messages
//! (edit, delete, regenerate), and a folder's defaults — the current thread
//! of an ongoing folder included, which a change of the folder's defaults
//! reaches (L9). At `full` it may, since it could make those changes itself.
//!
//! A write that changes nothing is no change: a client that sends a
//! thread's settings or a folder's defaults back as they are is not refused
//! (the branch review's verification, V-12). The refusal is `403`
//! [`CODE`], so a client tells it from the other 403s without reading the
//! sentence.
//!
//! The level is read from the stored key row and settings, not the
//! snapshot, as a device's `lmgw__*` call reads it (`ScopedExecutor`): a
//! lowered level is in force from its commit.

use axum::http::StatusCode;
use axum::response::Response;

use super::chat::err_json;
use super::chat_caller::Caller;
use crate::config::SelfAdmin;
use crate::state::SharedState;

/// The code of every refusal here, and of a device below `full` attaching
/// the toolset (`chat_tool_write`): what it asked for needs its admin tools
/// at `full` (§1.8).
pub(crate) const CODE: &str = "chat_toolset_needs_full";

/// What a device asked to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    /// Thread `id`'s settings, by its settings route.
    Settings(i64),
    /// Thread `id`'s messages: an edit, a delete, a regenerate.
    Messages(i64),
    /// A folder's defaults.
    Defaults,
    /// Thread `id`'s settings, as the current thread of the ongoing folder
    /// whose defaults the request changes.
    CurrentThread(i64),
}

impl Change {
    /// The sentence's subject, what it may still do, and what it may not.
    fn words(self) -> (String, &'static str, &'static str) {
        match self {
            Self::Settings(id) => (
                format!("chat thread {id} carries"),
                "read the thread and send messages to it",
                "change its settings",
            ),
            Self::Messages(id) => (
                format!("chat thread {id} carries"),
                "read the thread and send messages to it",
                "edit, delete or regenerate its messages",
            ),
            Self::Defaults => (
                "this folder's defaults carry".to_string(),
                "read the folder and use its threads",
                "change its defaults",
            ),
            Self::CurrentThread(id) => (
                format!("chat thread {id}, this ongoing folder's current thread, carries"),
                "read the thread and send messages to it",
                "change its settings, which this change of the folder's defaults would do \
                 (send apply_to_current: false to change the defaults alone)",
            ),
        }
    }
}

/// The level of lmgw's admin tools `caller`'s key has now, its own capped
/// by the gateway's, as stored. `None` for a caller that is no device.
pub(crate) async fn stored_level(
    state: &SharedState,
    caller: &Caller,
) -> Option<Result<SelfAdmin, crate::error::GatewayError>> {
    if !caller.is_device() {
        return None;
    }
    let id = caller.key_id()?;
    Some(
        async {
            let device = crate::store::device_admin_now(&state.db, id).await?;
            let gateway = crate::store::gateway_self_admin_now(&state.db).await?;
            Ok(device.capped(gateway))
        }
        .await,
    )
}

/// How a level reads in a refusal.
pub(crate) fn level_words(level: SelfAdmin) -> &'static str {
    match level {
        SelfAdmin::Off => "off",
        SelfAdmin::ReadOnly => "read only",
        SelfAdmin::Full => "full",
    }
}

/// The refusal for `caller` making `change` to a thread or folder at
/// `level` (`reach_level`), or `None`: the owner, a thread or folder without
/// the toolset, and a device whose admin tools are at `full` pass. The
/// caller asks only for a write that changes something. A level that
/// cannot be read refuses.
pub(crate) async fn refusal(
    state: &SharedState,
    caller: &Caller,
    level: u8,
    change: Change,
) -> Option<Response> {
    if level != 1 {
        return None;
    }
    let (subject, may, may_not) = change.words();
    let words = match stored_level(state, caller).await? {
        Ok(SelfAdmin::Full) => return None,
        Ok(level) => level_words(level),
        Err(e) => {
            return Some(err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                format!(
                    "whether this device may {may_not} could not be read ({e}), so nothing was \
                     changed; try again"
                ),
            ))
        }
    };
    Some(err_json(
        StatusCode::FORBIDDEN,
        CODE,
        format!(
            "{subject} lmgw's admin tools, and this device's admin tools are {words} (its own \
             level, or the gateway's self-admin level that caps it): it may {may}, but not \
             {may_not}. What a thread with these tools holds steers later turns there that may \
             change lmgw's configuration, so only a device at full may. Nothing was changed. \
             The device's level is set on its row under Usage → Devices"
        ),
    ))
}
