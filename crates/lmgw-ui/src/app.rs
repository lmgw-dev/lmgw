use leptos::prelude::*;
use leptos_router::components::{Route, Router, Routes};
use leptos_router::path;

use crate::charts::TipLayer;
use crate::pages;
use crate::shell::Sidebar;
use crate::ui_scale::ZoomHud;
use crate::widgets::{DirtyGuardHost, ToastHost};

#[component]
pub fn App() -> impl IntoView {
    crate::live::provide_live_bus();
    // The Chat's transport is `fetch`: nothing to provide, except in a debug
    // build's test host (`test_host`). Its other seams are the `/chat`
    // route's (`chat_host`): the shell's stream as its live updates
    // (chat-plane design D1), so the page opens no stream of its own.
    crate::test_host::provide();
    // `/v1/models` once the session is open, for every model picker.
    crate::catalog::provide_model_catalog();
    // The local container images, for every image picker; fetched when one
    // mounts or opens, never on a timer.
    crate::widgets::image_picker::provide_image_list();
    // The window width and the sidebar rail it can force.
    let sidebar = crate::shell::provide_sidebar();
    // The window's own frame (LMGW_WINDOW=csd): where the header bars put
    // the window buttons. Nothing in a browser or in KWin's frame.
    crate::frame::provide_frame();
    // Dark or light, for Settings and the main menu alike.
    crate::theme::provide_theme();
    // The chart tooltip is `position: fixed`, and the content pane is a size
    // container, which makes it the containing block of every fixed
    // descendant — so the one tooltip lives out here, beside the toasts.
    crate::charts::provide_tips();
    crate::ops_state::provide_ops_state();
    // The machine check and the graphics card's setup, one for the app: a
    // page left while the setup runs shows it again (machine-check D160).
    // The Chat's view of it is the `/chat` route's (`chat_host`), in its
    // host's `machine`.
    crate::widgets::machine_check::provide_machine_state();
    // The tray's moves into the window, without a reload (D162).
    crate::shell::provide_shell_moves();
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
            <crate::shell::ShellNavigate/>
            <ZoomHud/>
            <TipLayer/>
            <div class="app" class:rail=move || sidebar.rail.get()>
                <Sidebar/>
                // The sidebar stays while the gate is up; only the page is
                // swapped — and it is swapped, not hidden, so signing in
                // re-mounts the routes and every resource on them fetches
                // again (principals §8).
                <main class="content">
                    <Show
                        when=move || locked.get()
                        fallback=|| {
                            view! {
                                <Routes fallback=pages::NotFound>
                                    <Route path=path!("") view=pages::Overview/>
                                    <Route path=path!("chat") view=crate::chat_host::ChatRoute/>
                                    <Route path=path!("chat/profiles/:sel?") view=pages::ChatProfiles/>
                                    <Route path=path!("audio-lab") view=pages::AudioLab/>
                                    <Route path=path!("image-lab") view=pages::ImageLab/>
                                    <Route path=path!("decision-lab") view=pages::DecisionLab/>
                                    <Route path=path!("decision-lab/batches/:id") view=pages::DecisionBatch/>
                                    <Route path=path!("documents-lab") view=pages::DocumentsLab/>
                                    <Route path=path!("voice-lab") view=pages::VoiceLab/>
                                    // One route for the list and the selected agent's
                                    // tabs (`/agents/<id>`, `/agents/<id>/runs`): the
                                    // page stays mounted while the selection moves.
                                    <Route path=path!("agents/:id?/:tab?") view=pages::Agents/>
                                    // The Workflows surface became the agent catalog
                                    // (agent-catalog §6); old deep links land there.
                                    <Route path=path!("workflows") view=pages::WorkflowsMoved/>
                                    // Before the Models route, which would read
                                    // `catalog` and `find` as a class.
                                    <Route path=path!("models/catalog") view=pages::ModelCatalogPage/>
                                    <Route path=path!("models/find") view=pages::ModelFindPage/>
                                    // One route for the list and every selection
                                    // (`/models/local/7`, `/models/aux/2`, …): the
                                    // page stays mounted while the selection moves.
                                    <Route path=path!("models/:class?/:id?") view=pages::Models/>
                                    <Route path=path!("downloads") view=pages::Downloads/>
                                    <Route path=path!("backends/:tab?/:id?") view=pages::Backends/>
                                    <Route path=path!("benchmarks/:run?") view=pages::Benchmarks/>
                                    // `/docs/<tab>`, and a selected corpus at `/docs/corpora/<id>`.
                                    <Route path=path!("docs/:tab?/:id?") view=pages::Docs/>
                                    // One route for the list and the selected base's tabs
                                    // (`/knowledge/<id>`, `/knowledge/<id>/search`): the page
                                    // stays mounted while the selection moves.
                                    <Route path=path!("knowledge/:id?/:tab?") view=pages::Knowledge/>
                                    // The list and every selection (`/upstreams/3`, `/upstreams/new`).
                                    <Route path=path!("upstreams/:id?") view=pages::Upstreams/>
                                    <Route path=path!("api-reference/:op?") view=pages::ApiReference/>
                                    // `mcp-servers`, not `mcp`: `/mcp` is the northbound MCP protocol
                                    // endpoint, so a reload on that path would hit JSON-RPC, not the SPA.
                                    <Route path=path!("mcp-servers/:id?/:label?") view=pages::McpServers/>
                                    // Before Requests, which would read `conversations`
                                    // as a row; each keeps its page mounted while its
                                    // selection (the path's last part) moves.
                                    <Route path=path!("traffic/conversations/:chain?") view=pages::TrafficConversations/>
                                    <Route path=path!("traffic/:log?") view=pages::Traffic/>
                                    <Route path=path!("usage") view=pages::Usage/>
                                    // One page for the list and the selected key
                                    // (`/usage/keys/7`).
                                    <Route path=path!("usage/keys/:id?") view=pages::UsageKeys/>
                                    <Route path=path!("usage/prices") view=pages::UsagePrices/>
                                    <Route path=path!("wiring") view=pages::Wiring/>
                                    <Route path=path!("settings/:cat?") view=pages::Settings/>
                                    // A debug build's sample of the shared widgets;
                                    // a release build answers it as not found.
                                    <Route path=path!("dev/components") view=pages::Components/>
                                </Routes>
                            }
                        }
                    >
                        <div class="login-route">
                            <crate::widgets::HeadBar title="Sign in"/>
                            <crate::session::LoginCard/>
                        </div>
                    </Show>
                </main>
            </div>
        </Router>
    }
}
