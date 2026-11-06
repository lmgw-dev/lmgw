//! The Chat's stored bytes, through the transport its host hands it
//! (chat-plane design, "WP3a: the transport seam"): attachment and media
//! pictures ([`ChatImg`]), recordings ([`ChatAudio`]) and downloads
//! ([`save`]: exports, an attachment's or a knowledge file's original).
//!
//! On the dashboard the transport's link is the route itself, so every
//! element loads it as before (the browser's cache, an `<audio>`'s ranges and
//! `preload="none"`, the desktop shell's Save As with the server's name).
//! A host that relays frames has no link: the bytes come through it and are
//! shown by an object URL, revoked when the element goes
//! (`lmgw_ui_kit::http::bytes`).

use leptos::prelude::*;
use lmgw_ui_kit::http::bytes::{download, use_src, Src};

use lmgw_ui_kit::http::{use_http, Http};
use lmgw_ui_kit::widgets::{use_toasts, Toasts};

/// The route of attachment `id`'s bytes.
pub(super) fn attachment_path(id: i64) -> String {
    format!("/chat/api/attachments/{id}")
}

/// An `<img>` of `path`'s bytes. Shows nothing while they come; when they
/// could not be read (a relaying host's size bound, a file gone) a marked
/// box in the picture's place, with the reason as its tooltip and, where
/// there is room, its text.
#[component]
pub(super) fn ChatImg(
    #[prop(into)] path: Signal<String>,
    #[prop(optional, into)] class: Option<String>,
    #[prop(optional, into)] alt: Option<String>,
) -> impl IntoView {
    let src = use_src(use_http(), Signal::derive(move || Some(path.get())));
    let class = class.unwrap_or_default();
    // The `<img>` stays one element while the bytes are not refused: its
    // `src` follows, as an attribute did.
    let failed = Memo::new(move |_| match src.get() {
        Src::Failed(why) => Some(why),
        _ => None,
    });
    move || match failed.get() {
        Some(why) => view! {
            <span
                class=format!("{class} bytes-failed")
                role="img"
                aria-label=format!("picture not loaded: {why}")
                title=format!("The picture did not load: {why}")
                data-bytes-failed=why.clone()
            >
                <span class="bytes-failed-mark" aria-hidden="true">"⚠"</span>
                <span class="bytes-failed-why">{format!("not loaded: {why}")}</span>
            </span>
        }
        .into_any(),
        None => view! { <img class=class.clone() alt=alt.clone() src=move || src.get().url()/> }
            .into_any(),
    }
}

/// A recording's player. With a link it is the plain `<audio
/// preload="none">`: a thread with many recordings fetches none of them
/// until one is played. Without one, the bytes come through the transport
/// on the first Play, and the player shows then, playing; when they could
/// not be read the button is back, and a toast says why.
#[component]
pub(super) fn ChatAudio(path: String) -> impl IntoView {
    let http = use_http();
    if let Some(link) = http.link(&path) {
        return view! { <audio class="media-player" controls=true preload="none" src=link></audio> }
            .into_any();
    }
    let toasts = use_toasts();
    let wanted = RwSignal::new(None::<String>);
    let src = use_src(http, wanted.into());
    Effect::new(move |_| {
        if let Src::Failed(why) = src.get() {
            wanted.set(None);
            toasts.err(format!("the recording did not load: {why}"));
        }
    });
    let path_for_play = path.clone();
    view! {
        <Show
            when=move || src.with(|s| matches!(s, Src::Ready(_)))
            fallback=move || {
                let p = path_for_play.clone();
                let loading = move || src.with(|s| *s == Src::Loading);
                view! {
                    <button
                        class="btn sm media-load"
                        title="Fetch the recording and play it"
                        disabled=loading
                        on:click=move |_| wanted.set(Some(p.clone()))
                    >
                        {move || if loading() { "Loading…" } else { "▶ Play" }}
                    </button>
                }
            }
        >
            <audio class="media-player" controls=true autoplay=true src=move || src.get().url()></audio>
        </Show>
    }
    .into_any()
}

/// Downloads through the page's transport. Made where the page (or the
/// component) is set up, used in its handlers. `Copy`.
#[derive(Clone, Copy)]
pub(super) struct Saver {
    http: Http,
    toasts: Toasts,
}

impl Saver {
    pub(super) fn new() -> Self {
        Saver {
            http: use_http(),
            toasts: use_toasts(),
        }
    }

    /// Save `path` as a file named `fallback` unless the answer names it.
    /// With the transport's link the `<a download>` is clicked now, inside
    /// the caller's handler (the dashboard: as before); without one the
    /// bytes come first, and a failure is toasted.
    pub(super) fn save(self, path: String, fallback: String) {
        let toasts = self.toasts;
        let name = fallback.clone();
        download(self.http, &path, &fallback, move |e| {
            toasts.err(format!("downloading {name} failed: {e}"));
        });
    }
}

#[cfg(test)]
mod contract;
