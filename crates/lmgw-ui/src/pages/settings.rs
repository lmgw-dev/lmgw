//! Settings — a libadwaita preferences view on the shared split view
//! (modern-sample.html; `crate::widgets::nav_split`): the categories as a
//! list pane beside the shown category, whose groups are boxed preference
//! groups that flow into two columns in a pane over 1000 px. Under 800 px of
//! pane one shows at a time, the category with a back button. A search box
//! over the categories shows every setting it matches, under its category;
//! in a narrow window it lists the matches in the pane instead.
//!
//! There is one draft for the whole page and one save: [`FormState`] tracks
//! every field against what the server last said, each changed row carries
//! the accent dot, the categories say how many of theirs changed, and the
//! save toast offers Discard and Save (Ctrl+S). Save posts a sparse patch of
//! the changed keys only — `SettingsFullPatch` and every nested patch are
//! all-`Option`, so any mix of keys is one valid patch (a refusal refuses
//! all of it, and says so in the toast until the next save).
//!
//! The page is table-driven: [`fields`] names each setting once — its patch
//! path, label, category, group and control — and the same row feeds the
//! renderer, the search, the dirty counts and the deep links
//! (`/settings/<cat>#f-<key-with-dashes>`, scrolled to and flashed). Every
//! category stays mounted and is only hidden, so a search or a category
//! switch never loses an edit, and only leaving Settings asks about them.
//!
//! Controls that act the moment they are used — the GPU hold, Apply to
//! running containers, Check now, theme and scale — are never part of the
//! draft; they carry an "applies now" tag. Secrets: empty keeps what is
//! stored, the clear tick erases it.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::{use_location, use_navigate, use_params_map};
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
use crate::widgets::nav_split::{provide_save_key, NavSplit, SaveKey, SplitConf, SplitState};
use crate::widgets::{
    use_dirty_guard, use_toasts, Explain, FormState, ImageClass, ImagePicker, Kind, MachineCheck,
    ModelPicker, SaveToast, Select,
};

/// The machine check card's anchor: `/settings/gpu#machine` (machine-check
/// design §2.3), the link the tray and every other surface send people to.
pub(in crate::pages) const MACHINE_ANCHOR: &str = "machine";

/// The Realtime category (realtime design §12): its rows, the controls only
/// it has, and its VRAM budget.
mod realtime;
use realtime::{BudgetPanel, SpeechStyleField, TagHintField, VoiceField, WordsField};
/// Settings → About (headless server design D20).
mod about;
/// What the page words differently in the server image: the pinned rows,
/// the restart unit (headless server design §2.2, §6.2).
mod deployment;
use deployment::*;
/// Settings → Network & access → Accounts (headless server design §5).
mod accounts;
/// The Chat's Voice group (chat-voice design §2.1).
/// The Chat's picture and sound tools' default models (chat-media-tools
/// design §3.8).
mod chat_media;
mod chat_profile;
mod chat_voice;
mod devices;
/// Settings → Documents (document-conversion design §8).
mod documents;
mod read;
/// Settings → Network & access → TLS (headless server design §4.7).
mod tls;
/// Settings → Realtime → WebRTC calls (calls design §9).
mod webrtc;

mod controls;
mod draft;
mod explain;
mod forge;
mod headroom;
/// The category pane.
mod list;
/// The row a setting is drawn in.
mod prow;
mod render;
mod rows;
mod table;
use controls::*;
pub(super) use draft::href;
use draft::*;
use explain::*;
use forge::*;
use prow::SetField as Field;
use render::*;
use rows::*;
use table::*;

#[cfg(test)]
mod tests;

/// Every setting, in page order.
fn fields() -> impl Iterator<Item = &'static Def> {
    COMMON[..2]
        .iter()
        .chain(tls::TLS.iter())
        .chain(accounts::ACCOUNTS.iter())
        .chain(COMMON[2..].iter())
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
        .chain(documents::DOCUMENTS.iter())
        .chain(realtime::REALTIME.iter())
        .chain(webrtc::WEBRTC.iter())
        .chain(devices::DEVICES.iter())
        .chain(about::ABOUT.iter())
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
    /// The router's move to a path (and whether it replaces the entry).
    go: Callback<(String, bool)>,
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
        go: Callback::new(move |(to, replace): (String, bool)| {
            navigate(
                &to,
                leptos_router::NavigateOptions {
                    replace,
                    scroll: false,
                    ..Default::default()
                },
            )
        }),
    };
    chat_media::provide(page.data);
    documents::provide();

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
        if saving.get_untracked() || invalid.get_untracked() > 0 || dirty_count.get_untracked() == 0
        {
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

    let sub = Signal::derive(move || {
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
    });

    // The split: the address names a category, or none (`/settings`: the
    // category list in a narrow window, Network beside it in a wide one).
    let params = use_params_map();
    let picked = Memo::new(move |_| {
        params.with(|p| {
            p.get_str("cat")
                .and_then(|c| CAT_SLUGS.iter().copied().find(|s| *s == c))
        })
    });
    let split = SplitState::new(SPLIT);
    let narrow = split.narrow;
    let save_key = provide_save_key();
    SaveKey::install(save);
    let refs = crate::widgets::list_pane::ListRefs::new();
    let open_cat = Callback::new(move |(slug, replace): (&'static str, bool)| {
        let to = format!("/settings/{slug}");
        if to != loc.pathname.get_untracked() || !loc.search.get_untracked().is_empty() {
            page.go.run((to, replace));
        }
    });
    let back = Callback::new(move |()| {
        let q = loc.search.get_untracked();
        let q = if q.is_empty() { q } else { format!("?{q}") };
        page.go.run((format!("/settings{q}"), false));
        refs.focus_later();
    });
    let title = Signal::derive(move || {
        if page.searching() {
            "Search results".to_string()
        } else {
            CATS.iter()
                .find(|(s, ..)| *s == active.get())
                .map_or(String::new(), |(_, l, _)| l.to_string())
        }
    });
    let scrolled = RwSignal::new(false);
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

    let list = ViewFn::from(move || {
        view! {
            <list::CatList
                page=page
                query=query
                picked=picked
                narrow=narrow
                refs=refs
                dirty_by_cat=dirty_by_cat
                matches_by_cat=matches_by_cat
                sub=sub
                open_cat=open_cat
            />
            // A narrow window's list hides the detail pane, whose toast
            // would leave unsaved edits with no Save or Discard in sight.
            <Show when=move || narrow.get() && picked.with(Option::is_none)>
                <SaveToast
                    dirty_count=dirty_count
                    invalid=invalid
                    saving=saving
                    error=save_err
                    on_save=save
                    on_discard=discard
                />
            </Show>
        }
    });
    let detail = ViewFn::from(move || {
        view! {
            <crate::widgets::nav_split::DetailHead
                narrow=narrow
                back=back
                back_label="Back to the categories"
                title=title
                scrolled=scrolled
            />
            <div
                class="detail-scroll mdetail-scroll set-scroll"
                tabindex="-1"
                on:scroll=move |ev| crate::widgets::nav_split::track_scroll(&ev, scrolled)
            >
                {search_line}
                {move || {
                    load_err
                        .get()
                        .map(|e| {
                            view! {
                                <div class="notice err row set-load-err">
                                    "Loading the settings failed: " {e}
                                    <button class="btn sm" on:click=move |_| load()>
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
                    <div class="mdetail-empty">"Loading…"</div>
                </Show>
            </div>
            <SaveToast
                dirty_count=dirty_count
                invalid=invalid
                saving=saving
                error=save_err
                on_save=save
                on_discard=discard
            />
        }
    });

    view! {
        <NavSplit
            split=split
            has_sel=Signal::derive(move || picked.with(Option::is_some))
            save_key=save_key
            search=refs.search
            search_off=Signal::derive(move || narrow.get() && picked.with(Option::is_some))
            list=list
            detail=detail
        />
    }
}

/// The split's bounds: the categories' list is narrower than the Models
/// list, the breakpoint the same (a 1024 px window at 125 % shows one pane).
const SPLIT: SplitConf = SplitConf {
    class: "settings-split",
    width_key: "settings.list.width",
    list_width: (210, 24, 300),
    detail_min: 480,
    narrow_below: 800.0,
    handle_label: "Resize the categories",
};
