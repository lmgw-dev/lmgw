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
/// The personality-profile editor page (the Chat's pickers are lmgw-chat-ui's).
mod chat_profiles;
mod conversations;
/// The Decision lab: decision models through `/v1/systemone`, kept in the lab store.
mod decision_lab;
pub mod docs;
mod docs_eval;
mod docs_requests;
mod docs_search;
mod docs_wizard;
mod downloads;
/// The one Fallback control of every model and alias editor.
mod fallback_field;
mod image_lab;
mod image_recipes;
pub mod knowledge;
mod knowledge_files;
mod knowledge_form;
mod knowledge_new;
mod knowledge_search;
mod knowledge_settings;
mod lab_frame;
/// The lab store's client: a lab's kept history, its usage and export.
mod lab_history;
/// A lab's named form parts (the Image lab's prompts, the Audio lab's settings).
mod lab_presets;
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
/// The Voices lab: candidate voice samples, saved to the voice library.
mod voice_lab;
mod wiring;
pub(crate) mod wizard;
pub use agent_detail::AgentDetailPage;
pub use agents::{Agents, WorkflowsMoved};
pub use api_ref::ApiReference;
pub use audio_lab::AudioLab;
pub use backends::Backends;
pub use benchmarks::Benchmarks;
pub use chat_profiles::ChatProfiles;
pub use conversations::TrafficConversations;
pub use decision_lab::{DecisionBatch, DecisionLab};
pub use docs::Docs;
pub use downloads::Downloads;
pub use image_lab::ImageLab;
pub use knowledge::{Knowledge, KnowledgeDetail};
pub use local_edit::LocalModelEdit;
pub use mcp::McpServers;
pub use model_catalog::ModelCatalogPage;
pub use models::{ModelFindPage, Models};
pub use overview::Overview;
pub use settings::Settings;
pub use traffic::Traffic;
pub use upstreams::Upstreams;
pub use usage::{Usage, UsageKeys, UsagePrices};
pub use voice_lab::VoiceLab;
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
