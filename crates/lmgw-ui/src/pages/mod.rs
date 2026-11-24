//! Route views. Density is dense by default (the tokens are on `:root`); user
//! surfaces opt into `density-airy`. Every `.page` route renders
//! through [`PageFrame`]; chat and the labs are full-bleed shells of their own.

use leptos::prelude::*;

use crate::widgets::{Ico, Icon, PageFrame, PrefGroup};

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
/// The list pane of a collection page on the split view (Upstreams, MCP servers).
mod collection;
/// The component sample, a debug build's `/dev/components`.
#[cfg(debug_assertions)]
mod components;
mod conversations;
/// The Decision lab: decision models through `/v1/systemone`, kept in the lab store.
mod decision_lab;
pub mod docs;
mod docs_eval;
mod docs_requests;
mod docs_search;
mod docs_wizard;
mod documents_lab;
mod downloads;
/// Where an editor form sits: its modal, or the Models detail pane.
mod editor_host;
/// The one Fallback control of every model and alias editor.
mod fallback_field;
/// The GPU pin of every local model editor (device-ledger design §8).
mod gpu_pin;
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
/// Small pieces the labs' preference rows share: a file picker, the prompt.
mod lab_rows;
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
pub use agents::{Agents, WorkflowsMoved};
#[cfg(debug_assertions)]
pub use components::Components;

/// A release build has no component sample.
#[cfg(not(debug_assertions))]
#[component]
pub fn Components() -> impl IntoView {
    view! { <NotFound/> }
}
pub use api_ref::ApiReference;
pub use audio_lab::AudioLab;
pub use backends::Backends;
pub use benchmarks::Benchmarks;
pub use chat_profiles::ChatProfiles;
pub use conversations::TrafficConversations;
pub use decision_lab::{DecisionBatch, DecisionLab};
pub use docs::Docs;
pub use documents_lab::DocumentsLab;
pub use downloads::Downloads;
pub use image_lab::ImageLab;
pub use knowledge::Knowledge;
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
/// SPA shell. A status page: it says so, then names where the content moved
/// rather than dead-ending.
#[component]
pub fn NotFound() -> impl IntoView {
    let moved: [(&str, &str, &str); 4] = [
        ("Models", "Local, embedding and audio models", "/models"),
        ("Downloads", "Model files being fetched", "/downloads"),
        ("Traffic", "Request logs", "/traffic"),
        (
            "Conversations",
            "Stored conversations",
            "/traffic/conversations",
        ),
    ];
    view! {
        <PageFrame title="Not found" sub="No such page">
            <div class="detail-flow">
                <div class="detail-hero">
                    <div class="glyph">
                        <Icon ico=Ico::Search/>
                    </div>
                    <div class="hero-text">
                        <h1>"Nothing lives at this URL"</h1>
                        <div class="sub">"It may be an old link: these moved."</div>
                    </div>
                </div>
                <PrefGroup title="Where things are">
                    {moved
                        .into_iter()
                        .map(|(title, sub, href)| {
                            view! {
                                <a class="prow action" href=href>
                                    <div class="prow-text">
                                        <span class="prow-title">{title}</span>
                                        <span class="prow-sub">{sub}</span>
                                    </div>
                                    <Icon ico=Ico::Right class="prow-chevron"/>
                                </a>
                            }
                        })
                        .collect_view()}
                </PrefGroup>
            </div>
        </PageFrame>
    }
}
