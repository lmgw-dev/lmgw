//! Models → Find a model (find-a-model design §8.1, D33): `/models/find`,
//! a page of its own under Models with the [`ModelFinder`], so a link to it
//! works and the Overview's entry, part B and the share text can all point
//! there. `?q=` (or `?hf=`, a Hugging Face address) fills the box and
//! searches; every search writes `?q=` back, so a reload shows the same.
//!
//! "Say hello" creates a Chat thread on the model's public name through the
//! Chat's own API (`POST /chat/api/threads`, §6.3, D27) and opens it; the
//! first message loads the model, as for any request.

use leptos::prelude::*;
use leptos_router::hooks::use_navigate;
use lmgw_api_types::chat_threads::ThreadCreate;
use serde::Deserialize;

use crate::scope::Scope;
use crate::url_state::use_query_signal;
use crate::widgets::model_finder::{ModelFinder, ReadyModel};
use crate::widgets::{use_toasts, PageFrame};

/// The part of the created thread "Say hello" reads.
#[derive(Deserialize)]
struct Created {
    id: i64,
}

#[component]
pub fn ModelFindPage() -> impl IntoView {
    let toasts = use_toasts();
    let navigate = StoredValue::new(use_navigate());
    let q = use_query_signal("q");
    let hf = use_query_signal("hf");
    let query = Signal::derive(move || {
        let q = q.get();
        let hf = hf.get();
        [q, hf].into_iter().find(|s| !s.trim().is_empty())
    });
    let on_search = Callback::new(move |s: String| {
        if q.get_untracked() != s {
            q.set(s);
        }
        if !hf.get_untracked().is_empty() {
            hf.set(String::new());
        }
    });
    let busy = RwSignal::new(false);
    let scope = Scope::new();
    let on_ready = Callback::new(move |m: ReadyModel| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        // Not the scope's: a failed create is said even once the page is
        // left; the page is opened only while it is still here.
        leptos::task::spawn_local(async move {
            let body = ThreadCreate {
                model_alias: m.public_name.clone(),
                ..ThreadCreate::default()
            };
            let res = crate::api::post::<Created, _>("/chat/api/threads", &body).await;
            match res {
                Ok(t) if scope.alive() => {
                    busy.set(false);
                    navigate.with_value(|n| n(&format!("/chat?t={}", t.id), Default::default()))
                }
                Ok(_) => {}
                Err(e) => {
                    toasts.err(format!("Couldn't open a chat with {}: {e}", m.display_name));
                    if scope.alive() {
                        busy.set(false);
                    }
                }
            }
        });
    });
    view! {
        <PageFrame
            title="Find a model"
            sub="by the name someone told you, sized to your graphics card"
            class="find-page"
            actions=|| view! {
                <a class="btn ghost" href="/models" title="Everything the gateway can serve">"Models"</a>
            }
        >
            <ModelFinder query=query on_ready=on_ready on_search=on_search/>
        </PageFrame>
    }
}
