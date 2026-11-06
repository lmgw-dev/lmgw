//! The Chat's reads beside its threads and folders (chat-plane design §2),
//! [`Cap::Chat`](crate::principal::Cap::Chat): what the dashboard's Chat
//! read from the owner's `/api` before, and what every client of the Chat
//! reads now, scoped to the caller ([`Caller`](super::chat_caller::Caller)).
//!
//! - `GET /chat/api/settings` ([`settings`]): the gateway settings the Chat
//!   uses, the same for every caller.
//! - `GET /chat/api/tools` and `GET /chat/api/tools/{label}` ([`tools`]):
//!   the toolsets a thread may attach and one's tools, under the caller's
//!   tool scope — what a device's writes are checked against (L5).
//! - `GET /chat/api/knowledge/bases`, `GET
//!   /chat/api/knowledge/files/{id}/text` and `…/original` ([`knowledge`]):
//!   the bases a thread may search and the source viewer a citation opens,
//!   for a device only with the `kb__*` tools in its tool scope, and the
//!   original the owner's alone.
//!
//! What a device is shown here is never more than it reaches today through
//! its own routes (`/v1/mcp/servers`, a turn's retrieval), and every refusal
//! is said before anything is read, so an id cannot be probed.

mod knowledge;
mod settings;
mod tools;

pub(super) use knowledge::{bases, file_original, file_text};
pub(super) use settings::settings;
pub(super) use tools::{tool_detail, tool_list};
