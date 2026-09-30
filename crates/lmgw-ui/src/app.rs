use leptos::prelude::*;
use leptos_router::components::{Route, Router, Routes};
use leptos_router::path;

use crate::charts::TipLayer;
use crate::pages;
use crate::shell::{Sidebar, Titlebar};
use crate::ui_scale::ZoomHud;
use crate::widgets::{DirtyGuardHost, ToastHost};

#[component]
pub fn App() -> impl IntoView {
    crate::live::provide_live_bus();
    // `/v1/models` once the session is open, for every model picker.
    crate::catalog::provide_model_catalog();
    // The local container images, for every image picker; fetched when one
    // mounts or opens, never on a timer.
    crate::widgets::image_picker::provide_image_list();
    // The window width and the sidebar rail it can force.
    let sidebar = crate::shell::provide_sidebar();
    // The chart tooltip is `position: fixed`, and the content pane is a size
    // container, which makes it the containing block of every fixed
    // descendant — so the one tooltip lives out here, beside the toasts.
    crate::charts::provide_tips();
    crate::ops_state::provide_ops_state();
    // Unsaved edits ask before a link leaves their page (the host, which
    // navigates, is inside the router).
    crate::widgets::dirty_guard::provide_dirty_guard();
    // Window zoom (Ctrl +/-/0) and the stored interface scale.
    crate::ui_scale::provide_ui_scale();
    // Needs the live bus: the badge re-counts when an ingest job ends, because
    // a finished ingest auto-fulfils the requests it satisfies.
    pages::docs::provide_docs_pending();
    // One `GET /api/session` on load; unauthenticated puts the card up
    // (principals §3.4).
    crate::session::start();
    let locked = crate::session::locked_signal();
    // The SPA owns `/` (plan P8): no router base, every href is root-absolute.
    view! {
        <Router>
            <ToastHost/>
            <DirtyGuardHost/>
            <ZoomHud/>
            <TipLayer/>
            <div class="app" class:rail=move || sidebar.rail.get()>
                <Titlebar/>
                <Sidebar/>
                // The titlebar and the sidebar stay while the gate is up: in
                // the Tauri shell the titlebar *is* the drag region and the
                // window controls, and a login card you cannot move or close
                // is not an improvement. Only the page is swapped — and it is
                // swapped, not hidden, so signing in re-mounts the routes and
                // every resource on them fetches again (principals §8).
                <main class="content">
                    <Show
                        when=move || locked.get()
                        fallback=|| {
                            view! {
                                <Routes fallback=pages::NotFound>
                                    <Route path=path!("") view=pages::Overview/>
                                    <Route path=path!("chat") view=pages::Chat/>
                                    <Route path=path!("audio-lab") view=pages::AudioLab/>
                                    <Route path=path!("image-lab") view=pages::ImageLab/>
                                    <Route path=path!("agents") view=pages::Agents/>
                                    <Route path=path!("agents/:id/:tab?") view=pages::AgentDetailPage/>
                                    // The Workflows surface became the agent catalog
                                    // (agent-catalog §6); old deep links land there.
                                    <Route path=path!("workflows") view=pages::WorkflowsMoved/>
                                    <Route path=path!("models") view=pages::Models/>
                                    <Route path=path!("models/catalog") view=pages::ModelCatalogPage/>
                                    <Route path=path!("models/local/:id") view=pages::LocalModelEdit/>
                                    <Route path=path!("downloads") view=pages::Downloads/>
                                    <Route path=path!("backends/:tab?") view=pages::Backends/>
                                    <Route path=path!("benchmarks") view=pages::Benchmarks/>
                                    <Route path=path!("docs/:tab?") view=pages::Docs/>
                                    <Route path=path!("knowledge") view=pages::Knowledge/>
                                    <Route path=path!("knowledge/:id/:tab?") view=pages::KnowledgeDetail/>
                                    <Route path=path!("upstreams") view=pages::Upstreams/>
                                    <Route path=path!("api-reference") view=pages::ApiReference/>
                                    // `mcp-servers`, not `mcp`: `/mcp` is the northbound MCP protocol
                                    // endpoint, so a reload on that path would hit JSON-RPC, not the SPA.
                                    <Route path=path!("mcp-servers") view=pages::McpServers/>
                                    <Route path=path!("traffic") view=pages::Traffic/>
                                    <Route path=path!("traffic/conversations") view=pages::TrafficConversations/>
                                    <Route path=path!("usage") view=pages::Usage/>
                                    <Route path=path!("usage/keys") view=pages::UsageKeys/>
                                    <Route path=path!("usage/prices") view=pages::UsagePrices/>
                                    <Route path=path!("wiring") view=pages::Wiring/>
                                    <Route path=path!("settings/:cat?") view=pages::Settings/>
                                </Routes>
                            }
                        }
                    >
                        <crate::session::LoginCard/>
                    </Show>
                </main>
            </div>
        </Router>
    }
}
