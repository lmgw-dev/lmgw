//! Settings — the KDE System Settings shape (UX plan Phase 3, CFG S3–S10).
//!
//! A rail of categories with a search box over them; the pane shows one
//! category, or every field the search matches under its category. There is
//! one draft for the whole page and one Save: [`FormState`] tracks every
//! field against what the server last said, the foot counts what changed and
//! where, and Save posts a sparse patch of the changed keys only —
//! `SettingsFullPatch` and every nested patch are all-`Option`, so any mix of
//! keys is one valid patch (a refusal refuses all of it, and says so in the
//! foot until the next save).
//!
//! The page is table-driven: [`fields`] names each setting once — its patch
//! path, label, category, group and control — and the same row feeds the
//! renderer, the search, the dirty counts and the deep links
//! (`/settings/<cat>#f-<key-with-dashes>`, scrolled to and flashed). Every
//! category stays mounted and is only hidden, so a search or a category
//! switch never loses an edit.
//!
//! Controls that act the moment they are used — the GPU hold, Apply to
//! running containers, Check now, theme and scale — are never part of the
//! draft; they carry an "applies now" tag. Secrets: empty keeps what is
//! stored, the clear tick erases it.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::{use_location, use_navigate};
use lmgw_api_types::builds::MAX_BUILD_UPDATE_CHECK_HOURS;
// The names the gateway's messages point at a setting by: the rail, the
// cards and the rows that carry them are built from these.
use lmgw_api_types::settings_labels as labels;
use lmgw_api_types::MessageAck;
use serde_json::{json, Map, Value};

use crate::catalog::CatalogEntry;
use crate::live::use_live;
use crate::ops_state::{class_key, use_ops};
use crate::scope::Scope;
use crate::widgets::{
    use_dirty_guard, use_slash_focus, use_toasts, Explain, Field, FormState, ImageClass,
    ImagePicker, Kind, MachineCheck, ModelPicker, PageFrame, PageMode, SaveBar, Section, Select,
};

/// The machine check card's anchor: `/settings/gpu#machine` (machine-check
/// design §2.3), the link the tray and every other surface send people to.
pub(in crate::pages) const MACHINE_ANCHOR: &str = "machine";

/// The Realtime category (realtime design §12): its rows, the controls only
/// it has, and its VRAM budget.
mod realtime;
use realtime::{BudgetPanel, SpeechStyleField, TagHintField, VoiceField, WordsField};
/// The Chat's Voice group (chat-voice design §2.1).
/// The Chat's picture and sound tools' default models (chat-media-tools
/// design §3.8).
mod chat_media;
mod chat_profile;
mod chat_voice;
mod devices;
mod read;
/// Settings → Realtime → WebRTC calls (calls design §9).
mod webrtc;

mod controls;
mod draft;
mod explain;
mod forge;
mod render;
mod rows;
mod table;
use controls::*;
pub(super) use draft::href;
use draft::*;
use explain::*;
use forge::*;
use render::*;
use rows::*;
use table::*;

#[cfg(test)]
mod tests;

/// Every setting, in page order.
fn fields() -> impl Iterator<Item = &'static Def> {
    COMMON
        .iter()
        .chain(CHAT.iter())
        .chain(AUX[..3].iter())
        .chain(AUX_DECISION.iter())
        .chain(AUX[3..].iter())
        .chain(AUDIO.iter())
        .chain(IMAGE.iter())
        .chain(BUILDS.iter())
        .chain(REST.iter())
        .chain(chat_voice::CHAT_VOICE.iter())
        .chain(chat_media::CHAT_MEDIA.iter())
        .chain(realtime::REALTIME.iter())
        .chain(webrtc::WEBRTC.iter())
        .chain(devices::DEVICES.iter())
}

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

/// What every part of the page reads: the draft, the server's last answer,
/// the keys a save of the draft judges, the search, the category, the class
/// sections' open state.
#[derive(Clone, Copy)]
struct Page {
    form: FormState,
    data: RwSignal<Option<Value>>,
    /// [`judged_keys`] of the draft's changes.
    judged: Memo<BTreeSet<&'static str>>,
    words: Memo<Vec<String>>,
    active: Memo<&'static str>,
    folds: [(&'static str, RwSignal<bool>); 4],
    /// Dirty keys whose stored value the machine check's settings switch
    /// changed under the edit (review H1): a save writes the edit over it,
    /// which the field says.
    under: RwSignal<BTreeSet<String>>,
    /// The machine check's settings switch wrote: read the settings again.
    settings_written: Callback<()>,
    /// `#machine` was asked for: the card scrolls into view once it has
    /// its content (review L10).
    machine_scroll: RwSignal<bool>,
}

impl Page {
    fn searching(&self) -> bool {
        self.words.with(|w| !w.is_empty())
    }

    fn matches(&self, d: &Def) -> bool {
        if !in_search(d) {
            return false;
        }
        // The machine check's rows are one link per search, the first that
        // matches standing for every one (review L11).
        if matches!(d.ctl, Ctl::MachineLink) {
            return self.words.with(|w| {
                machine_hits(w)
                    .first()
                    .is_some_and(|first| first.key == d.key)
            });
        }
        self.words.with(|w| hits(d, w))
    }

    fn shows(&self, d: &Def) -> bool {
        if self.searching() {
            self.matches(d)
        } else {
            self.active.get() == d.cat && in_category(d)
        }
    }

    fn fold(&self, group: &str) -> Option<RwSignal<bool>> {
        self.folds
            .iter()
            .find(|(g, _)| *g == group)
            .map(|(_, o)| *o)
    }

    /// A boolean the server reported (`/has_hf_token`).
    fn server_flag(&self, ptr: &str) -> bool {
        self.data.with(|d| {
            d.as_ref()
                .and_then(|v| v.pointer(ptr))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
    }
}

#[component]
pub fn Settings() -> impl IntoView {
    let toasts = use_toasts();
    let navigate = use_navigate();
    let form = FormState::new(Map::new());
    let under = RwSignal::new(BTreeSet::<String>::new());
    let data = RwSignal::new(None::<Value>);
    let load_err = RwSignal::new(None::<String>);
    let saving = RwSignal::new(false);
    let save_err = RwSignal::new(None::<String>);

    // What the server holds becomes the baseline every field is compared to.
    let take = move |v: Value| {
        for d in fields() {
            if let Some(kind) = d.ctl.form_kind() {
                form.with_kind(d.key, kind);
            }
            if let Ctl::Secret(_, clear) = d.ctl {
                form.with_kind(clear, Kind::Flag);
            }
        }
        form.rebase(baseline(&v));
        data.set(Some(v));
        load_err.set(None);
        under.set(BTreeSet::new());
    };
    let scope = Scope::new();
    let load = move || {
        scope.spawn(async move {
            match read::read_settings().await {
                Ok(v) => take(v),
                Err(e) => load_err.set(Some(e)),
            }
        });
    };
    load();
    // The machine check's settings switch wrote settings this page shows
    // (review H1): they are read again, an untouched field takes what was
    // written, and an edited one keeps the edit and says that a save would
    // write it over the switch's value.
    let settings_written = Callback::new(move |()| {
        scope.spawn(async move {
            match read::read_settings().await {
                Ok(v) => {
                    let moved = form.refetched(baseline(&v));
                    data.set(Some(v));
                    under.update(|u| u.extend(moved));
                }
                Err(e) => toasts.err(format!(
                    "The graphics card switch changed the settings, but lmgw couldn't read them \
                     again ({e}). Reload the page before you save here."
                )),
            }
        });
    });

    let query = crate::url_state::use_query_signal("q");
    let words = Memo::new(move |_| {
        query.with(|q| {
            q.split_whitespace()
                .map(str::to_lowercase)
                .collect::<Vec<_>>()
        })
    });
    let active = crate::url_state::use_view("cat", CAT_SLUGS, "network");
    let fold = |group: &'static str, persist: &str, open: bool| {
        (
            group,
            crate::prefs::persisted_bool(&format!("open.{persist}"), open),
        )
    };
    let judged = Memo::new(move |_| judged_keys(&form.dirty_keys()));
    let page = Page {
        form,
        data,
        judged,
        words,
        active,
        folds: [
            fold("router", "settings.runtimes.chat", true),
            fold("aux_router", "settings.runtimes.aux", false),
            fold("audio", "settings.runtimes.audio", false),
            fold("image", "settings.runtimes.image", false),
        ],
        under,
        settings_written,
        machine_scroll: RwSignal::new(false),
    };
    chat_media::provide(page.data);

    // Numbers that parse but are out of range, and what the server would
    // refuse across fields — of the fields this save judges only, per key.
    let range_errs = Memo::new(move |_| judged.with(|j| blocking(form, j)));
    let invalid = Signal::derive(move || form.invalid_count() + range_errs.with(|r| r.len()));
    let dirty_count = Signal::derive(move || form.dirty_count());
    let dirty_by_cat = Memo::new(move |_| {
        let mut n: BTreeMap<&'static str, usize> = BTreeMap::new();
        for k in form.dirty_keys() {
            if let Some(c) = cat_of_key(&k) {
                *n.entry(c).or_default() += 1;
            }
        }
        n
    });
    let detail = Signal::derive(move || {
        dirty_by_cat.with(|by| {
            CATS.iter()
                .filter_map(|(slug, label, _)| by.get(slug).map(|n| format!("{label} ({n})")))
                .collect::<Vec<_>>()
                .join(" · ")
        })
    });
    let matches_by_cat = Memo::new(move |_| {
        let mut n: BTreeMap<&'static str, usize> = BTreeMap::new();
        if page.searching() {
            for d in fields() {
                if page.matches(d) {
                    *n.entry(d.cat).or_default() += 1;
                }
            }
        }
        n
    });
    use_dirty_guard().watch("Settings", Signal::derive(move || dirty_count.get() > 0));

    let save = Callback::new(move |()| {
        if saving.get_untracked() || invalid.get_untracked() > 0 {
            return;
        }
        let Ok(patch) = form.patch() else { return };
        let patch = finish(patch);
        let dirty = form.dirty_keys();
        // The fields stay editable while the save is out: what was sent is
        // kept apart from what is typed meanwhile, which stays a draft.
        let sent = form.snapshot();
        saving.set(true);
        spawn_local(async move {
            let res = crate::api::post::<MessageAck, _>("/api/op/settings_set_full", &patch).await;
            match res {
                Ok(v) => {
                    save_err.set(None);
                    toasts.ok(v.message);
                    // Read back what was stored: the server trims and
                    // normalises (a prefix loses its slashes), and the
                    // secrets' "stored" flags move. Not for a page that was
                    // left while the save was out (the toast above still
                    // told how it went).
                    if !scope.alive() {
                        return;
                    }
                    match read::read_settings().await {
                        Ok(v) => {
                            form.saved(&sent, baseline(&v));
                            data.set(Some(v));
                            under.set(BTreeSet::new());
                        }
                        Err(e) => {
                            // Stored all the same: what was sent is the new
                            // baseline, or the bar would say "unsaved" beside
                            // a "saved" toast (review code:S4). A secret is
                            // write-only either way: its box empties.
                            let mut now = form.baseline_after(&sent);
                            for d in fields() {
                                if let Ctl::Secret(_, clear) = d.ctl {
                                    now.insert(d.key.into(), Value::String(String::new()));
                                    now.insert(clear.into(), Value::Bool(false));
                                }
                                if let Ctl::ForgeTokens = d.ctl {
                                    let rows = forge_rows(&now[d.key]);
                                    now.insert(d.key.into(), json!(forge_after(&rows)));
                                }
                            }
                            form.saved(&sent, now);
                            toasts.err(format!("saved, but not read back: {e}"));
                        }
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    // A refusal refuses the whole patch. When one field was
                    // changed, the message is about that field: say it there
                    // too.
                    if let [only] = dirty.as_slice() {
                        form.set_error(only, msg.clone());
                    }
                    save_err.set(Some(msg));
                }
            }
            saving.set(false);
        });
    });
    let discard = Callback::new(move |()| {
        form.discard();
        save_err.set(None);
        under.set(BTreeSet::new());
    });

    // `/settings/<cat>#f-<key>`: scroll the field into view and flash it,
    // opening the class section that holds it first. Once per link, and once
    // the fields are there: a save refetches `data`, and that must not scroll
    // back, flash again or reopen a section folded since (review code:S2).
    let loc = use_location();
    let loaded = Memo::new(move |_| data.with(Option::is_some));
    // The desktop shell's moves: the tray's item sent the window here
    // again, and the card scrolls in again although the address is the same.
    let shell_moves = use_context::<crate::shell::ShellMoves>();
    Effect::new(move |_| {
        let hash = loc.hash.get();
        active.track();
        if let Some(m) = shell_moves {
            m.track();
        }
        if !loaded.get() {
            return;
        }
        let id = hash.trim_start_matches('#').to_string();
        // The machine check's card: its top into view, not its middle,
        // since it is taller than the pane can be, once it has its content
        // (the card's own effect, review L10).
        if id == MACHINE_ANCHOR {
            page.machine_scroll.set(true);
            return;
        }
        if !id.starts_with("f-") {
            return;
        }
        if let Some(open) = fields()
            .find(|d| anchor(d.key) == id)
            .and_then(|d| page.fold(d.group))
        {
            open.set(true);
        }
        set_timeout(move || flash(&id), Duration::from_millis(60));
    });

    // The narrow pane's picker for what the rail does.
    let cat_sel = RwSignal::new(active.get_untracked().to_string());
    Effect::new(move |_| {
        let a = active.get().to_string();
        if cat_sel.get_untracked() != a {
            cat_sel.set(a);
        }
    });
    Effect::new(move |_| {
        let v = cat_sel.get();
        if v != active.get_untracked() {
            navigate(&format!("/settings/{v}"), Default::default());
        }
    });
    let cat_opts = Signal::derive(|| {
        CATS.iter()
            .map(|(s, l, _)| (s.to_string(), l.to_string()))
            .collect::<Vec<_>>()
    });

    let sub = move || {
        data.with(|d| {
            d.as_ref()
                .map(|v| {
                    format!(
                        "v{} · data in {}",
                        v["version"].as_str().unwrap_or_default(),
                        v["data_dir"].as_str().unwrap_or_default()
                    )
                })
                .unwrap_or_default()
        })
    };
    let search_box = move || {
        let q_box: NodeRef<leptos::html::Input> = NodeRef::new();
        use_slash_focus(q_box);
        view! {
            <input
                class="input set-search"
                type="search"
                node_ref=q_box
                data-slash
                placeholder="Search settings"
                aria-label="Search settings"
                title="Search · / focuses, Esc clears"
                data-untracked=""
                prop:value=move || query.get()
                on:input=move |ev| query.set(event_target_value(&ev))
                on:keydown=move |ev| {
                    if ev.key() == "Escape" {
                        query.set(String::new());
                    }
                }
            />
        }
    };
    let search_line = move || {
        let q = query.get();
        if q.trim().is_empty() {
            return None;
        }
        let n: usize = matches_by_cat.with(|m| m.values().sum());
        let text = match n {
            0 => format!("No setting matches \u{201c}{}\u{201d}", q.trim()),
            1 => format!("1 setting matches \u{201c}{}\u{201d}", q.trim()),
            n => format!("{n} settings match \u{201c}{}\u{201d}", q.trim()),
        };
        Some(view! {
            <div class="set-search-line">
                {text} " · "
                <button class="link-btn" on:click=move |_| query.set(String::new())>
                    "Esc clears"
                </button>
            </div>
        })
    };

    view! {
        <PageFrame title="Settings" sub=sub mode=PageMode::Split class="settings">
            <nav class="split-rail set-rail" aria-label="Settings categories">
                {search_box}
                {CATS
                    .iter()
                    .map(|(slug, label, _)| {
                        let slug: &'static str = slug;
                        let current = move || {
                            (!page.searching() && active.get() == slug).then_some("page")
                        };
                        let dirty = move || dirty_by_cat.with(|m| m.get(slug).copied().unwrap_or(0));
                        let hits = move || matches_by_cat.with(|m| m.get(slug).copied().unwrap_or(0));
                        view! {
                            <a class="rail-item" href=format!("/settings/{slug}") aria-current=current>
                                <span class="rail-label">{*label}</span>
                                {move || {
                                    (dirty() > 0)
                                        .then(|| {
                                            view! {
                                                <span class="count attn" title="unsaved changes here">
                                                    {dirty()}
                                                </span>
                                            }
                                        })
                                }}
                                {move || {
                                    page.searching()
                                        .then(|| {
                                            view! {
                                                <span
                                                    class="count"
                                                    class:zero=move || hits() == 0
                                                    title="settings matching the search"
                                                >
                                                    {hits()}
                                                </span>
                                            }
                                        })
                                }}
                            </a>
                        }
                    })
                    .collect_view()}
            </nav>
            <div class="split-pane set-pane">
                <div class="rail-select">
                    {search_box} <Select value=cat_sel options=cat_opts/>
                </div>
                <div class="fill-pane set-scroll">
                    {search_line}
                    {move || {
                        load_err
                            .get()
                            .map(|e| {
                                view! {
                                    <div class="notice err row">
                                        "Loading the settings failed: " {e}
                                        <button class="btn ghost sm" on:click=move |_| load()>
                                            "Retry"
                                        </button>
                                    </div>
                                }
                            })
                    }}
                    <Show when=move || data.with(Option::is_some)>
                        {CATS.iter().map(|c| cat_view(c, page)).collect_view()}
                    </Show>
                    <Show when=move || data.with(Option::is_none) && load_err.with(Option::is_none)>
                        <div class="dim">"Loading…"</div>
                    </Show>
                </div>
                <div class="page-foot">
                    <SaveBar
                        dirty_count=dirty_count
                        detail=detail
                        invalid=invalid
                        saving=saving
                        error=save_err
                        on_save=save
                        on_discard=discard
                    />
                </div>
            </div>
        </PageFrame>
    }
}
