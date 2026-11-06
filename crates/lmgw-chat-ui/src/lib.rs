//! lmgw's Chat as a library (chat-plane design, "WP3b: the lmgw-chat-ui
//! crate"; kai-firefox design §8.2 item 3): threads and folders, search, the
//! transcript with tools, approvals, attachments and media, the composer and
//! voice. lmgw's dashboard mounts it at `/chat`; a Firefox sidebar is the
//! second host (kai-firefox WP8).
//!
//! The Chat knows no router and no dashboard. What it needs from its host:
//!
//! - the kit's transport, `lmgw_ui_kit::http::Http`, in context (provided
//!   before `lmgw_ui_kit::catalog::provide_model_catalog`, which captures it);
//! - a [`ChatHost`] ([`provide_chat_host`]): where it was opened and where it
//!   keeps the open thread ([`ChatLocation`]), whether the session is locked,
//!   its live updates ([`ChatLive`]), its preview frame ([`PreviewFrame`]),
//!   voice ([`ChatVoice`], or none), the base its assets are served under,
//!   and the machine check's surfaces ([`ChatMachine`], or none);
//! - the kit's toasts (`lmgw_ui_kit::widgets::ToastHost`, rendered before the
//!   Chat) and its dirty guard (`widgets::dirty_guard::provide_dirty_guard`,
//!   with `DirtyGuardHost` for its question);
//! - its stylesheets and assets: `kit.css`, `profiles.css`, this crate's
//!   `assets/chat.css` and `assets/voice.css`, `assets/codeblocks.js` with
//!   `assets/vendor/`, and `assets/voice/` under the asset base.
//!
//! Then it mounts [`Chat`]. Below 520 px of its own width the thread list and
//! the thread's settings become drawers over the conversation, opened from
//! buttons in its head; it works from 280 px up.

/// The host's seams.
pub mod host;
/// The source viewer for a cited knowledge file (the Chat's and the
/// dashboard's Knowledge page's).
pub mod knowledge_source;
/// What the Chat's live updates named, kept until the Chat takes it.
pub mod live;

mod chat;
/// Message actions: Copy, Edit, Delete, Regenerate, Continue.
mod chat_actions;
mod chat_approvals;
mod chat_attach;
/// The Chat's stored bytes through its transport: pictures, recordings, downloads.
mod chat_bytes;
/// Exports of a thread, a folder and all threads.
mod chat_export;
/// Chat folders: sidebar grouping, drag-and-drop, folder settings.
mod chat_folders;
/// Knowledge bases in the Chat: settings section, `#` picker, chips.
mod chat_knowledge;
/// The Chat's live updates, from the source its host hands it (the
/// dashboard's shell stream, else `/chat/api/feed`): what the follower reads
/// again, and the GPU hold the voice controls show.
mod chat_live;
/// The machine check in the Chat: the host's slot (banner, block above the
/// empty state, the `gpu_setup` card's fix) and the Chat's own reading.
mod chat_machine;
/// Markdown to HTML for replies: math spans, `\(…\)` rewrite, raw HTML escaped.
mod chat_markdown;
/// The image and audio tools in the Chat: the live pane of a running call,
/// the image card and its actions, the player of a spoken result.
mod chat_media;
/// The media tools' defaults: the Chat's settings section, and the boxes
/// Settings → Chat → Media reuses ([`media_defaults`]).
mod chat_media_defaults;
/// The HTML preview's frame, as the host hands it.
mod chat_preview;
/// The Chat's profile pickers and the list they read.
mod chat_profiles;
/// A chat thread's `x-lmgw-reasoning*` overrides: fields, checks, header line.
mod chat_reasoning;
mod chat_rename;
/// What a finished reply says of itself: who answered, whether it was stored.
mod chat_reply;
/// What a turn retrieved: the summary above an answer and its `[n]` citations.
mod chat_retrieval;
/// The open thread's messages taking stored rows in place.
mod chat_rows;
/// A chat thread's sampling parameters: draft, checks, fields.
mod chat_sampling;
/// Chat search: the sidebar's Messages section and reveal-and-flash of a hit.
mod chat_search;
/// The thread settings' fields, shared with a folder's defaults form.
mod chat_settings;
mod chat_stream;
/// The Chat page following what other writers change (its live updates).
mod chat_sync;
/// MCP Tasks in the Chat: a late result's card, the jobs strip, Answer.
mod chat_tasks;
/// Temporary chats: sidebar group, banner, Keep, silent discard.
mod chat_temp;
/// One streamed turn, however it started.
mod chat_turn;
/// Chat voice: a thread's voice overrides, their resolution, the Voice
/// section of its settings.
mod chat_voice;

/// The media tools' default-model boxes, which the dashboard's Settings →
/// Chat → Media reuses.
pub mod media_defaults {
    pub use crate::chat_media_defaults::{use_view, MediaModelBox, MediaVoiceBox};
}

pub use chat::Chat;
pub use chat_markdown::md_to_html;
pub use host::{
    provide_chat_host, ChatAddress, ChatHost, ChatLive, ChatLocation, ChatMachine, ChatOrigin,
    ChatVoice, PreviewFrame,
};
