//! Route views. Each page owns its density: admin surfaces are
//! `density-dense`, user surfaces `density-airy`. Every `.page` route renders
//! through [`PageFrame`]; chat and the labs are full-bleed shells of their own.

use leptos::prelude::*;

use crate::widgets::PageFrame;

mod agent_detail;
pub mod agents;
mod api_ref;
mod audio_catalog;
mod audio_lab;
mod audio_spec;
mod audio_stream;
pub mod backends;
pub mod benchmarks;
mod candidate_alias_editor;
pub mod chat;
/// Message actions: Copy, Edit, Delete, Regenerate, Continue.
mod chat_actions;
mod chat_approvals;
mod chat_attach;
/// Markdown to HTML for replies: math spans, `\(…\)` rewrite, raw HTML escaped.
/// Chat folders: sidebar grouping, drag-and-drop, folder settings.
mod chat_export;
mod chat_folders;
/// Knowledge bases in the Chat: settings section, `#` picker, chips.
mod chat_knowledge;
mod chat_markdown;
/// The personality-profile editor page and the Chat page's profile pickers.
mod chat_profiles;
/// A chat thread's `x-lmgw-reasoning*` overrides: fields, checks, header line.
mod chat_reasoning;
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
/// The Chat page following what other writers change (`/api/events`' `chat`
/// frame).
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
mod conversations;
pub mod docs;
mod docs_eval;
mod docs_requests;
mod docs_search;
mod docs_wizard;
mod downloads;
mod image_lab;
mod image_recipes;
pub mod knowledge;
mod knowledge_files;
mod knowledge_form;
mod knowledge_new;
mod knowledge_search;
mod knowledge_settings;
mod knowledge_source;
mod lab_frame;
mod local_edit;
/// The local model editor's Test: the load test on the saved row, its answer kept on the page.
mod local_test;
mod mcp;
mod model_catalog;
mod model_editors;
pub mod models;
mod overview;
mod settings;
mod traffic;
mod upstreams;
mod usage;
mod wiring;
mod wizard;
pub use agent_detail::AgentDetailPage;
pub use agents::{Agents, WorkflowsMoved};
pub use api_ref::ApiReference;
pub use audio_lab::AudioLab;
pub use backends::Backends;
pub use benchmarks::Benchmarks;
pub use chat::Chat;
pub use chat_profiles::ChatProfiles;
pub use conversations::TrafficConversations;
pub use docs::Docs;
pub use downloads::Downloads;
pub use image_lab::ImageLab;
pub use knowledge::{Knowledge, KnowledgeDetail};
pub use local_edit::LocalModelEdit;
pub use mcp::McpServers;
pub use model_catalog::ModelCatalogPage;
pub use models::Models;
pub use overview::Overview;
pub use settings::Settings;
pub use traffic::Traffic;
pub use upstreams::Upstreams;
pub use usage::{Usage, UsageKeys, UsagePrices};
pub use wiring::Wiring;

/// A deep link to one Settings field (`/settings/<cat>#f-<key>`), for the
/// shell, which lives outside the pages, and for pages nested one level
/// deeper (`agent_detail/`, `usage/`).
pub fn settings_href(key: &str) -> String {
    settings::href(key)
}

/// Fallback view for a URL no route matches — including the old UI's page
/// paths (`/local`, `/hf`, `/logs`, …), which the server still answers with the
/// SPA shell. Names where the content moved rather than dead-ending.
#[component]
pub fn NotFound() -> impl IntoView {
    view! {
        <PageFrame title="Not found" sub="no such page">
            <div class="card">
                "Nothing lives at this URL. Local, embedding and audio models are on "
                <a href="/models">"Models"</a> ", downloads on "
                <a href="/downloads">"Downloads"</a> ", request logs on "
                <a href="/traffic">"Traffic"</a> ", stored conversations on "
                <a href="/traffic/conversations">"Traffic → Conversations"</a> "."
            </div>
        </PageFrame>
    }
}
