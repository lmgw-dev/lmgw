//! Leaving a page with unsaved edits asks first (UX plan §2 #14, CFG S3,
//! MOD P7): a draft that took ten minutes should not vanish because the
//! sidebar was one click away.
//!
//! A page registers what could be lost — `use_dirty_guard().watch("Settings",
//! form_dirty)`, or `watch_page` for a draft that lives on one page only —
//! for as long as it is mounted. A click on a link out of the draft's scope
//! (the sidebar included) is then held in the capture phase, before the
//! router sees it, and an App-level modal asks: leave and discard, or stay.
//! A reload or closing the tab gets the browser's own question.
//!
//! Not guarded: Back/Forward (the history has already moved when the router
//! hears of it) and navigations code makes itself.

use std::sync::atomic::{AtomicU64, Ordering};

use leptos::prelude::*;
use leptos::wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use super::{Modal, ModalFooter};

static NEXT_WATCH: AtomicU64 = AtomicU64::new(1);

/// A leave that is waiting for the answer: where to, and what would be lost.
#[derive(Clone, PartialEq)]
struct Pending {
    to: String,
    labels: Vec<&'static str>,
}

/// One guarded draft: what it is called, whether it has changes, and the
/// path it lives under — a link that stays within it keeps the draft.
#[derive(Clone)]
struct Watch {
    id: u64,
    label: &'static str,
    dirty: Signal<bool>,
    scope: Signal<String>,
}

#[derive(Clone, Copy)]
pub struct DirtyGuard {
    /// The host's router path, read when a page registers (see [`provide_dirty_guard`]).
    path: fn() -> String,
    watched: RwSignal<Vec<Watch>>,
    pending: RwSignal<Option<Pending>>,
}

impl DirtyGuard {
    /// Guard `dirty` under `label` (the page's name, as the question says it:
    /// "Unsaved changes on Settings") until the calling component unmounts.
    /// The draft belongs to the page's first path segment: its own tabs
    /// (`/settings/gpu` → `/settings/network`) keep it.
    pub fn watch(&self, label: &'static str, dirty: Signal<bool>) {
        let here = current_path(self);
        let first = here.trim_start_matches('/').split('/').next().unwrap_or("");
        self.watch_in(label, dirty, Signal::stored(format!("/{first}")));
    }

    /// Guard a draft that lives on this one page — an editor at
    /// `/models/local/7`, which a link to `/models` would unmount.
    pub fn watch_page(&self, label: &'static str, dirty: Signal<bool>) {
        self.watch_in(label, dirty, Signal::stored(current_path(self)));
    }

    /// Guard a draft whose page is a path prefix the caller names — an
    /// agent's tabs (`/agents/<id>`, `/agents/<id>/definition`) share one
    /// draft. A signal, because the router reuses the page for the next
    /// agent: the scope follows the id.
    pub fn watch_in(&self, label: &'static str, dirty: Signal<bool>, scope: Signal<String>) {
        let id = NEXT_WATCH.fetch_add(1, Ordering::Relaxed);
        self.watched.update(|w| {
            w.push(Watch {
                id,
                label,
                dirty,
                scope,
            })
        });
        let watched = self.watched;
        on_cleanup(move || {
            watched.try_update(|w| w.retain(|x| x.id != id));
        });
    }

    /// The drafts with changes that going to `to` (a path; `None` = leaving
    /// the app) would lose.
    fn dirty_labels(&self, to: Option<&str>) -> Vec<&'static str> {
        self.watched.with_untracked(|w| {
            let mut out: Vec<&'static str> = w
                .iter()
                .filter(|x| x.dirty.try_get_untracked().unwrap_or(false))
                .filter(|x| {
                    to.is_none_or(|p| !x.scope.try_with_untracked(|s| within(s, p)).unwrap_or(true))
                })
                .map(|x| x.label)
                .collect();
            out.dedup();
            out
        })
    }
}

/// The path of the page being built. The host router's, not the address bar's: on
/// an in-app move the router renders the new page first and only then
/// pushes its URL, so while a page registers its watch the address bar still
/// shows the page the user came from — a draft scoped to "/" would never be
/// asked about, and one scoped to /models would ask on its own tabs.
fn current_path(guard: &DirtyGuard) -> String {
    (guard.path)()
}

/// Is `path` the scope itself or somewhere under it?
fn within(scope: &str, path: &str) -> bool {
    let scope = scope.trim_end_matches('/');
    path == scope || path.starts_with(&format!("{scope}/"))
}

/// Install the guard. Call once, in `App`; render [`DirtyGuardHost`] inside
/// the router. `path` returns the router's current path (untracked); the kit
/// knows no router, so the host passes it.
pub fn provide_dirty_guard(path: fn() -> String) {
    provide_context(DirtyGuard {
        path,
        watched: RwSignal::new(Vec::new()),
        pending: RwSignal::new(None),
    });
}

pub fn use_dirty_guard() -> DirtyGuard {
    expect_context::<DirtyGuard>()
}

/// The guard, where one is installed — for a widget that also renders
/// outside the app (a test).
pub fn try_use_dirty_guard() -> Option<DirtyGuard> {
    use_context::<DirtyGuard>()
}

/// The capture-phase link and unload listeners, and the question.
#[component]
pub fn DirtyGuardHost(
    /// Moves the app to a path, the way the host's router does.
    navigate: Callback<String>,
) -> impl IntoView {
    let guard = expect_context::<DirtyGuard>();
    let open = RwSignal::new(false);
    let go = RwSignal::new(None::<String>);

    let on_click =
        Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
            if ev.default_prevented()
                || ev.button() != 0
                || ev.meta_key()
                || ev.ctrl_key()
                || ev.shift_key()
                || ev.alt_key()
            {
                return;
            }
            let Some((path, to)) = link_target(&ev) else {
                return;
            };
            let labels = guard.dirty_labels(Some(&path));
            if labels.is_empty() {
                return;
            }
            // Held before the router's own listener (window, bubbling) or the
            // link itself sees it.
            ev.prevent_default();
            ev.stop_propagation();
            guard.pending.set(Some(Pending { to, labels }));
        });
    let on_unload = Closure::<dyn FnMut(web_sys::Event)>::new(move |ev: web_sys::Event| {
        if !guard.dirty_labels(None).is_empty() {
            ev.prevent_default();
            let _ = js_sys::Reflect::set(&ev, &"returnValue".into(), &"".into());
        }
    });
    let w = window();
    let _ = w.add_event_listener_with_callback_and_bool(
        "click",
        on_click.as_ref().unchecked_ref(),
        true,
    );
    let _ = w.add_event_listener_with_callback("beforeunload", on_unload.as_ref().unchecked_ref());
    let held = StoredValue::new_local((on_click, on_unload));
    on_cleanup(move || {
        held.with_value(|(c, u)| {
            let w = window();
            let _ = w.remove_event_listener_with_callback_and_bool(
                "click",
                c.as_ref().unchecked_ref(),
                true,
            );
            let _ =
                w.remove_event_listener_with_callback("beforeunload", u.as_ref().unchecked_ref());
        });
    });

    // The modal follows the question; closing it any way but "Leave" stays.
    Effect::new(move |_| {
        let want = guard.pending.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && guard.pending.with_untracked(Option::is_some) {
            guard.pending.set(None);
        }
    });
    Effect::new(move |_| {
        if let Some(to) = go.get() {
            go.set(None);
            navigate.run(to);
        }
    });

    let leave = move |_| {
        let to = guard.pending.get_untracked().map(|p| p.to);
        guard.pending.set(None);
        go.set(to);
    };
    let what = move || {
        guard.pending.with(|p| {
            p.as_ref()
                .map(|p| p.labels.join(" and "))
                .unwrap_or_default()
        })
    };

    view! {
        // App-level: no page density around it.
        <div class="density-dense">
            <Modal open=open title="Unsaved changes">
                <p class="dirty-q">
                    "Unsaved changes on " <b>{what}</b> ". Leaving this page discards them."
                </p>
                <ModalFooter>
                    <span class="foot-danger">
                        <button type="button" class="btn danger" on:click=leave>
                            "Leave and discard"
                        </button>
                    </span>
                    <button type="button" class="btn primary" on:click=move |_| open.set(false)>
                        "Stay"
                    </button>
                </ModalFooter>
            </Modal>
        </div>
    }
}

/// Where a click is taking the app, when it is a link within it (same
/// origin, no new tab or download): the path, which decides whether a draft
/// is left behind, and the whole target to navigate to.
fn link_target(ev: &web_sys::MouseEvent) -> Option<(String, String)> {
    let el = ev.target()?.dyn_into::<web_sys::Element>().ok()?;
    let a = el.closest("a[href]").ok()??;
    if a.has_attribute("download") || a.get_attribute("target").is_some_and(|t| t != "_self") {
        return None;
    }
    let href = a.get_attribute("href")?;
    let loc = window().location();
    let base = loc.href().ok()?;
    let url = web_sys::Url::new_with_base(&href, &base).ok()?;
    if url.origin() != loc.origin().ok()? {
        return None;
    }
    let path = url.pathname();
    let to = format!("{path}{}{}", url.search(), url.hash());
    Some((path, to))
}

#[cfg(test)]
mod tests {
    use super::within;

    #[test]
    fn a_draft_stays_within_its_scope() {
        assert!(within("/settings", "/settings"));
        assert!(within("/settings", "/settings/gpu"));
        assert!(!within("/settings", "/settings-old"));
        assert!(!within("/settings", "/models"));
        // an editor's own page: its list is a leave
        assert!(within("/models/local/7", "/models/local/7"));
        assert!(!within("/models/local/7", "/models"));
        assert!(!within("/models/local/7", "/models/local/8"));
    }
}
