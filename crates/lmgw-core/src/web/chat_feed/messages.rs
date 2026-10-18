//! A `message.*` record as a client receives it (client-apps design §2.2,
//! §2.3; WP10, device link D18): rendered at delivery, never from a copy.
//!
//! - **`message.added`, `message.updated`**: the thread's id and its title
//!   now (`thread_title`, so a phone in the background builds a
//!   notification from the event alone), and the messages the record names
//!   as `GET /chat/api/threads/{id}` lists them now, whole, with their
//!   attachments (`chat_wire::message`, the same conversion). A message
//!   gone by then is left out: its own `message.deleted` follows. A record
//!   whose messages are all gone, or whose thread is, renders as nothing:
//!   a later record (`message.deleted`, `thread.deleted`) says they went.
//! - **`message.deleted`**: the ids the record names; nothing to read.
//!
//! **L3.** The record carries the thread's level at the write, which the
//! stream checked before this (`render`). Here the thread is read as it is
//! now: one a later write took out of the reader's reach is not shown (that
//! write's own record says it went), as a `thread.*` record is not.

use lmgw_api_types::chat_feed::{MessagesChanged, MessagesDeleted};
use serde_json::Value;

use super::super::chat_wire::{message, wire};
use crate::state::AppState;
use crate::store::feed::{kind, Record};
use crate::store::{self, AdminThreads};

/// `r`'s data for a reader that reaches as far as `admin` (L3); `None` when
/// it renders as nothing (module doc). A read that fails is an error, so
/// the stream waits at the record and reads it again (review W4-5); so is
/// a record whose thread or message list does not read, which the stream
/// then passes with a `resync` that names it, never silently (review
/// W5-16).
pub(super) async fn render(
    state: &AppState,
    r: &Record,
    admin: AdminThreads,
) -> Result<Option<Value>, crate::error::GatewayError> {
    let (Some(thread_id), Some(ids)) = (r.thread_id, r.message_ids()) else {
        return Err(crate::error::GatewayError::Internal(format!(
            "record {} ({}) names no thread or no message list it can read ({:?})",
            r.seq, r.kind, r.message_ids
        )));
    };
    let thread = store::get_chat_thread(&state.db, thread_id).await?;
    if r.kind == kind::MESSAGE_DELETED {
        if thread.is_some_and(|t| !admin.sees(t.reach_level())) {
            return Ok(None);
        }
        return Ok(Some(wire(&MessagesDeleted {
            thread_id,
            message_ids: ids,
            by: r.by.clone(),
        })));
    }
    let Some(thread) = thread.filter(|t| admin.sees(t.reach_level())) else {
        return Ok(None);
    };
    let (rows, attachments) = store::chat_messages_by_ids(&state.db, thread_id, &ids).await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let messages = rows
        .iter()
        .map(|m| {
            let own: Vec<&store::ChatAttachmentMeta> = attachments
                .iter()
                .filter(|a| a.message_id == Some(m.id))
                .collect();
            message(m, &own)
        })
        .collect();
    Ok(Some(wire(&MessagesChanged {
        thread_id,
        thread_title: thread.title,
        messages,
        by: r.by.clone(),
    })))
}
