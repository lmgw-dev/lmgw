//! The build editor (container-builds design §4 fields, §9.1 "Build
//! editor"): a large guarded modal over one [`BuildSpec`], with Resolve
//! (what a run would build right now), Check merge (does every extra still
//! merge), Save and Save & Run.
//!
//! Validation is the server's: every refusal names its field, so the
//! message is shown under that field when it can be placed, and in the
//! modal's foot otherwise. The one check made here is the slug's charset,
//! because the slug is typed and becomes the tag.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::builds::{
    moving_tag, Build, BuildCheckMergeArgs, BuildEdit, BuildEnv, BuildSetAction, BuildSetArgs,
    BuildSpec, BuildView, CheckMergeReport, EditRole, Engine, Forge, ForgePr, GpuBackend,
    RemoteRefsView, RepoPreset, ResolvedPreview,
};

use super::extras::{is_full_sha, repo_key, ExtrasPicker};
use super::reports::{MergeReportView, ResolvedView};
use super::{sha7, use_bk};
use crate::backends_api as api;
use crate::fmt::human_bytes;
use crate::scope::Scope;
use crate::widgets::{
    filter_words, matches_word, use_modal, Field, Modal, ModalFooter, ModalSize, Popover, Select,
};

/// `keep_runs` a new build starts with (§4 "The editor shows a default of 3").
/// Visible in its field and editable; empty keeps every run.
pub const DEFAULT_KEEP_RUNS: u32 = 3;

/// What the editor opens on.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorSeed {
    /// `None` creates.
    pub id: Option<i64>,
    pub spec: BuildSpec,
    /// The build has runs: its slug is its tag now and cannot change (§4).
    pub slug_locked: bool,
    /// The last run's image size — the footprint the next one will roughly
    /// take again.
    pub last_size: Option<u64>,
    /// The repository preset it started from, preselected in the picker.
    pub preset: Option<String>,
    /// The slug was made for the owner (a preset's `official-master`), not
    /// typed: it follows the name as the name is typed, until the slug
    /// itself is.
    pub slug_auto: bool,
    /// This build's moving tag, from its `BuildView`, on [`Self::edit`] —
    /// the server's fact, computed the same way its runs tag (dev namespace
    /// included), never a client-side guess. `None` for a build that does
    /// not exist yet ([`Self::blank`], [`Self::from_preset`]): the slug
    /// field's hint is then a labelled preview until Resolve returns one.
    pub moving_tag: Option<String>,
}

impl EditorSeed {
    pub fn blank() -> Self {
        Self {
            id: None,
            spec: BuildSpec {
                keep_runs: Some(DEFAULT_KEEP_RUNS),
                ..BuildSpec::default()
            },
            slug_locked: false,
            last_size: None,
            preset: None,
            slug_auto: true,
            moving_tag: None,
        }
    }

    pub fn from_preset(p: &RepoPreset) -> Self {
        let mut s = Self::blank();
        apply_preset(&mut s.spec, p);
        s.spec.name = format!("{} {}", p.name, p.default_ref);
        s.spec.slug = slugify(&format!("{}-{}", p.id, p.default_ref));
        s.preset = Some(p.id.clone());
        s
    }

    pub fn edit(v: &BuildView) -> Self {
        Self {
            moving_tag: Some(v.moving_tag.clone()),
            ..Self::edit_build(
                v.build.clone(),
                v.last_run.is_some(),
                v.last_run.as_ref().and_then(|r| r.size_bytes),
            )
        }
    }

    pub fn edit_build(b: Build, has_runs: bool, last_size: Option<u64>) -> Self {
        Self {
            id: Some(b.id),
            spec: b.spec,
            slug_locked: has_runs,
            last_size,
            preset: None,
            slug_auto: false,
            moving_tag: None,
        }
    }
}

fn apply_preset(spec: &mut BuildSpec, p: &RepoPreset) {
    spec.engine = p.engine;
    spec.repo_url = p.repo_url.clone();
    spec.forge = p.forge;
    spec.git_ref = p.default_ref.clone();
}

/// A slug from a name: lowercase, anything outside the tag charset becomes
/// `-`, runs of `-` collapse, and it starts with a letter or digit.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') {
            c
        } else {
            '-'
        };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    let out = out.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    out.trim_end_matches('-').to_string()
}

/// The slug's charset, checked as it is typed (§4: `[a-z0-9][a-z0-9._-]*`).
/// Its length limit comes from the server, which knows the longest tag it
/// has to fit.
pub fn slug_error(slug: &str) -> Option<String> {
    let s = slug.trim();
    let first = s.chars().next()?;
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Some("the slug starts with a lowercase letter or a digit".to_string());
    }
    s.chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')))
        .map(|bad| {
            format!("'{bad}' cannot be in a tag — lowercase letters, digits, '.', '_' and '-' only")
        })
}

/// Which field a server refusal is about. Every validation message starts
/// with the field's wire name (`lmgw_core::backends::validate`), possibly
/// after an op prefix (`build_set: …`).
pub fn field_of_error(msg: &str) -> Option<&'static str> {
    fn of(m: &str) -> Option<&'static str> {
        let m = m.trim_start();
        let s = |p: &str| m.starts_with(p);
        if s("the slug") || m.contains("with the slug") {
            Some("slug")
        } else if s("the name") {
            Some("name")
        } else if s("repo_url") {
            Some("repo_url")
        } else if s("ref ") || s("ref '") || s("ref:") {
            Some("ref")
        } else if s("extras") {
            Some("extras")
        } else if s("cuda_version") {
            Some("cuda")
        } else if s("arch") {
            Some("arch")
        } else if s("dockerfile") {
            Some("dockerfile")
        } else if s("target") {
            Some("target")
        } else if s("edits") {
            Some("edits")
        } else if s("ccache_max_size") {
            Some("ccache")
        } else if s("cpus") {
            Some("cpus")
        } else if s("build_args") {
            Some("build_args")
        } else if s("keep_runs") {
            Some("keep_runs")
        } else {
            None
        }
    }
    of(msg).or_else(|| msg.split_once(": ").and_then(|(_, rest)| of(rest)))
}

/// The fields in the folded Advanced section: an error there unfolds it.
fn is_advanced(field: &str) -> bool {
    matches!(
        field,
        "dockerfile" | "target" | "edits" | "ccache" | "cpus" | "build_args" | "keep_runs"
    )
}

/// `13.0.0` > `12.9`? Dotted numbers, missing parts are 0. `false` when
/// either does not parse — a warning is not raised on a guess.
pub fn version_gt(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Option<Vec<u64>> {
        s.trim().split('.').map(|p| p.parse::<u64>().ok()).collect()
    };
    let (Some(mut x), Some(mut y)) = (parse(a), parse(b)) else {
        return false;
    };
    let n = x.len().max(y.len());
    x.resize(n, 0);
    y.resize(n, 0);
    x > y
}

/// `"86, 89"` → `["86", "89"]`; empty is auto.
pub fn parse_arch(text: &str) -> Option<Vec<String>> {
    let list: Vec<String> = text
        .split([',', ' ', ';'])
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect();
    (!list.is_empty()).then_some(list)
}

/// The editor's disk line (§9.1: free space next to the last run's
/// footprint — a warning, never a block): `(warn, text)`. The warning is
/// raised only on the two real numbers: less free than the last run took.
pub fn disk_words(last_size: Option<u64>, free: Option<u64>, dir: &str) -> Option<(bool, String)> {
    let at = if dir.is_empty() {
        String::new()
    } else {
        format!(" ({dir})")
    };
    match (last_size, free) {
        (Some(l), Some(f)) => Some((
            f < l,
            format!(
                "The last run's image took {}; {} free on the builds disk{at}.{}",
                human_bytes(l),
                human_bytes(f),
                if f < l {
                    " That is less than the last run took — the next one may run out of space."
                } else {
                    ""
                }
            ),
        )),
        (Some(l), None) => Some((
            false,
            format!(
                "The last run's image took {} — a new run needs about as much again. Free space on the builds disk could not be read.",
                human_bytes(l)
            ),
        )),
        (None, Some(f)) => Some((
            false,
            format!("{} free on the builds disk{at}.", human_bytes(f)),
        )),
        (None, None) => None,
    }
}

fn disk_note(last_size: Option<u64>, env: Option<&BuildEnv>) -> Option<impl IntoView> {
    let (free, dir) = env.map_or((None, ""), |e| {
        (e.builds_dir_free_bytes, e.builds_dir.as_str())
    });
    let (warn, text) = disk_words(last_size, free, dir)?;
    Some(view! { <p class=if warn { "field-warn mini-note" } else { "dim mini-note" }>{text}</p> })
}

/// The forge a URL implies on its own: only github.com is certain (§4 —
/// GitLab is chosen, or implied by a configured token the page cannot see).
pub fn forge_for_url(url: &str) -> Option<Forge> {
    let (host, _) = repo_key(url)?;
    (host == "github.com").then_some(Forge::Github)
}

// ---------------------------------------------------------------------------
// Ref suggestions
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Chunk {
    Num(u64),
    Text(String),
}

/// Digits compare as numbers: `b6000` after `b999`.
fn natural_key(s: &str) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut digits = false;
    for c in s.chars() {
        if c.is_ascii_digit() != digits && !buf.is_empty() {
            out.push(if digits {
                Chunk::Num(buf.parse().unwrap_or(u64::MAX))
            } else {
                Chunk::Text(std::mem::take(&mut buf))
            });
            buf.clear();
        }
        digits = c.is_ascii_digit();
        buf.push(c);
    }
    if !buf.is_empty() {
        out.push(if digits {
            Chunk::Num(buf.parse().unwrap_or(u64::MAX))
        } else {
            Chunk::Text(buf)
        });
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct RefItem {
    pub name: String,
    pub sha: String,
    /// `default` | `branch` | `tag`
    pub kind: &'static str,
}

/// The combobox's list: the default branch first, the other branches by
/// name, then tags newest-looking first (natural order, descending).
pub fn ref_items(v: &RemoteRefsView) -> Vec<RefItem> {
    let bare = |n: &str| {
        n.strip_prefix("refs/heads/")
            .or_else(|| n.strip_prefix("refs/tags/"))
            .unwrap_or(n)
            .to_string()
    };
    let mut heads: Vec<RefItem> = v
        .heads
        .iter()
        .map(|h| RefItem {
            name: bare(&h.name),
            sha: h.sha.clone(),
            kind: "branch",
        })
        .collect();
    heads.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out = Vec::with_capacity(v.heads.len() + v.tags.len());
    if let Some(i) = heads.iter().position(|h| h.name == v.default_branch) {
        let mut d = heads.remove(i);
        d.kind = "default";
        out.push(d);
    }
    out.extend(heads);
    let mut tags: Vec<RefItem> = v
        .tags
        .iter()
        .map(|t| RefItem {
            name: bare(&t.name),
            sha: t.sha.clone(),
            kind: "tag",
        })
        .collect();
    tags.sort_by_key(|t| std::cmp::Reverse(natural_key(&t.name)));
    out.extend(tags);
    out
}

/// Suggestions drawn before "Show all" (a repository can hold thousands of
/// tags). Said in the list, with the way to see them all.
const REF_SHOWN: usize = 60;

static NEXT_COMBO: AtomicU64 = AtomicU64::new(1);

/// Where a key moves the keyboard's row in a list of `n` drawn rows, from
/// `cur`; `page` rows for PgUp/PgDn. Home/End only while `caret` is false
/// (with text in the filter they move its caret). `None`: not a move.
pub fn list_step(key: &str, cur: usize, n: usize, page: usize, caret: bool) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let last = n - 1;
    Some(match key {
        "ArrowDown" => (cur + 1).min(last),
        "ArrowUp" => cur.saturating_sub(1).min(last),
        "PageDown" => (cur + page).min(last),
        "PageUp" => cur.saturating_sub(page).min(last),
        "Home" if !caret => 0,
        "End" if !caret => last,
        _ => return None,
    })
}

/// Free text with the repository's branches and tags as suggestions.
#[component]
fn RefCombo(
    value: RwSignal<String>,
    repo_url: RwSignal<String>,
    on_input: Callback<()>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let anchor: NodeRef<html::Button> = NodeRef::new();
    let refs = RwSignal::new(None::<(String, Result<Vec<RefItem>, String>)>);
    let filter = RwSignal::new(String::new());
    let all = RwSignal::new(false);
    // The keyboard's row: an index into the rows drawn.
    let active = RwSignal::new(0usize);
    let list_el: NodeRef<html::Div> = NodeRef::new();
    let list_id = format!("bk-refs-{}", NEXT_COMBO.fetch_add(1, Ordering::Relaxed));
    let scope = Scope::new();
    let load = move || {
        let url = repo_url.get_untracked().trim().to_string();
        if url.is_empty() {
            refs.set(Some((url, Err("name the repository first".to_string()))));
            return;
        }
        refs.set(None);
        scope.spawn(async move {
            let res = api::forge_refs(url.clone())
                .await
                .map(|v| ref_items(&v))
                .map_err(|e| e.to_string());
            refs.set(Some((url, res)));
        });
    };
    Effect::new(move |_| {
        if !open.get() {
            return;
        }
        filter.set(String::new());
        all.set(false);
        active.set(0);
        let url = repo_url.get_untracked().trim().to_string();
        let stale = refs.with_untracked(|r| match r {
            Some((u, Ok(_))) => *u != url,
            _ => true,
        });
        if stale {
            load();
        }
    });
    let matching = Memo::new(move |_| {
        let words = filter_words(&filter.get());
        refs.with(|r| match r {
            Some((_, Ok(items))) => items
                .iter()
                .filter(|i| words.iter().all(|w| matches_word(&i.name, w)))
                .cloned()
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
    });
    let total = move || refs.with(|r| r.as_ref().and_then(|(_, r)| r.as_ref().ok().map(Vec::len)));
    let drawn = move || {
        let n = matching.with(Vec::len);
        if all.get() {
            n
        } else {
            n.min(REF_SHOWN)
        }
    };
    // The keyboard starts on the current value when the list holds it (on
    // open, and as the list arrives or narrows), else on the first row.
    Effect::new(move |_| {
        if !open.get() {
            return;
        }
        let v = value.get_untracked();
        let at = matching.with(|m| m.iter().position(|i| i.name == v));
        active.set(at.filter(|i| *i < drawn()).unwrap_or(0));
    });
    let pick = move |name: String| {
        value.set(name);
        open.set(false);
        on_input.run(());
        if let Some(b) = anchor.get_untracked() {
            let _ = b.focus();
            // A guarded modal counts a pick like a typed character.
            let init = web_sys::EventInit::new();
            init.set_bubbles(true);
            if let Ok(ev) = web_sys::Event::new_with_event_init_dict("change", &init) {
                let _ = b.dispatch_event(&ev);
            }
        }
    };
    let move_to = move |i: usize| {
        active.set(i);
        if let Some(l) = list_el.get_untracked() {
            crate::widgets::popover::reveal_child(&l, i);
        }
    };
    let close_to_anchor = move || {
        open.set(false);
        if let Some(b) = anchor.get_untracked() {
            let _ = b.focus();
        }
    };
    let on_key = move |ev: web_sys::KeyboardEvent| {
        let key = ev.key();
        let n = matching
            .with_untracked(Vec::len)
            .min(if all.get_untracked() {
                usize::MAX
            } else {
                REF_SHOWN
            });
        let page = list_el
            .get_untracked()
            .map(|l| crate::widgets::popover::page_rows(&l))
            .unwrap_or(8);
        let caret = !filter.with_untracked(String::is_empty);
        if let Some(i) = list_step(&key, active.get_untracked(), n, page, caret) {
            move_to(i);
            ev.prevent_default();
            return;
        }
        match key.as_str() {
            "Enter" => {
                ev.prevent_default();
                let at = active.get_untracked();
                if at < n {
                    if let Some(name) =
                        matching.with_untracked(|m| m.get(at).map(|i| i.name.clone()))
                    {
                        pick(name);
                    }
                }
            }
            // Esc is the list's alone: inside the editor it must not also
            // reach the modal, which would ask about unsaved changes.
            "Escape" => {
                ev.stop_propagation();
                ev.prevent_default();
                close_to_anchor();
            }
            "Tab" => close_to_anchor(),
            _ => {}
        }
    };
    let lid_rows = StoredValue::new(list_id);
    let active_desc =
        move || (drawn() > 0).then(|| lid_rows.with_value(|l| format!("{l}-{}", active.get())));
    view! {
        <div class="bk-combo">
            <input
                class="input mono"
                placeholder="branch, tag, or a full commit SHA"
                prop:value=move || value.get()
                on:input=move |ev| {
                    value.set(event_target_value(&ev));
                    on_input.run(());
                }
                // ↓ opens the suggestions, as it does on the ▾ beside it.
                on:keydown=move |ev| {
                    if ev.key() == "ArrowDown" && !open.get_untracked() {
                        ev.prevent_default();
                        open.set(true);
                    }
                }
            />
            <button
                type="button"
                class="btn ghost sm"
                node_ref=anchor
                title="Pick from the repository's branches and tags"
                aria-haspopup="listbox"
                aria-expanded=move || open.get().to_string()
                on:click=move |_| open.update(|o| *o = !*o)
            >
                "▾"
            </button>
        </div>
        <Popover open=open anchor=anchor class="select-pop" min_width=380>
            // Keyboard: ↑↓ PgUp PgDn (Home End with the filter empty) move,
            // Enter picks, Esc and Tab close — the focus stays in the filter.
            <div class="pop-inner" on:keydown=on_key>
                <div class="pop-filter">
                    <input
                        class="input"
                        type="search"
                        placeholder="Filter"
                        autocomplete="off"
                        spellcheck="false"
                        role="combobox"
                        aria-label="Filter the branches and tags"
                        aria-autocomplete="list"
                        aria-expanded="true"
                        aria-controls=lid_rows.get_value()
                        aria-activedescendant=active_desc
                        data-autofocus
                        data-untracked
                        prop:value=move || filter.get()
                        on:input=move |ev| {
                            filter.set(event_target_value(&ev));
                            all.set(false);
                            active.set(0);
                            if let Some(l) = list_el.get_untracked() {
                                l.set_scroll_top(0);
                            }
                        }
                    />
                    <span class="pop-count">
                        {move || total().map(|t| crate::fmt::of(matching.with(Vec::len), t))}
                    </span>
                </div>
                <div
                    class="pop-list"
                    role="listbox"
                    aria-label="Branches and tags"
                    tabindex="-1"
                    id=lid_rows.get_value()
                    node_ref=list_el
                >
                    {move || match refs.get() {
                        None => view! { <div class="pop-empty">"Asking the remote…"</div> }.into_any(),
                        Some((_, Err(e))) => {
                            view! {
                                <div class="pop-empty">
                                    {e} " · "
                                    <button type="button" class="link-btn" on:click=move |_| load()>
                                        "Retry"
                                    </button>
                                </div>
                            }
                                .into_any()
                        }
                        Some((_, Ok(_))) => {
                            let list = matching.get();
                            let n = list.len();
                            let cap = if all.get() { usize::MAX } else { REF_SHOWN };
                            let rows = list
                                .into_iter()
                                .take(cap)
                                .enumerate()
                                .map(|(k, i)| {
                                    let name = i.name.clone();
                                    let sel = value.with_untracked(|v| *v == i.name);
                                    let oid = lid_rows.with_value(|l| format!("{l}-{k}"));
                                    view! {
                                        <div
                                            class="select-opt bk-ref"
                                            class:sel=sel
                                            class:active=move || active.get() == k
                                            role="option"
                                            id=oid
                                            aria-selected=sel.to_string()
                                            on:pointermove=move |_| {
                                                if active.get_untracked() != k {
                                                    active.set(k);
                                                }
                                            }
                                            on:click=move |_| pick(name.clone())
                                        >
                                            <span class="mono-sm bk-x-label">{i.name.clone()}</span>
                                            <span class="dim mono-sm">{sha7(&i.sha)}</span>
                                            <span class="type-badge">{i.kind}</span>
                                        </div>
                                    }
                                })
                                .collect_view();
                            view! {
                                {rows}
                                {(n == 0).then(|| view! { <div class="pop-empty">"Nothing matches — free text still works"</div> })}
                                {(n > cap)
                                    .then(|| {
                                        view! {
                                            <div class="pop-empty">
                                                {format!("{cap} of {n} shown — type to narrow · ")}
                                                <button type="button" class="link-btn" on:click=move |_| all.set(true)>
                                                    {format!("Show all {n}")}
                                                </button>
                                            </div>
                                        }
                                    })}
                            }
                                .into_any()
                        }
                    }}
                </div>
            </div>
        </Popover>
    }
}

// ---------------------------------------------------------------------------
// Edits
// ---------------------------------------------------------------------------

static NEXT_EDIT: AtomicU64 = AtomicU64::new(1);

/// One edit in the editor: its own signals, so typing into one row never
/// re-renders another (or loses focus).
#[derive(Clone, Copy)]
struct EditRow {
    key: u64,
    name: RwSignal<String>,
    role: RwSignal<String>,
    find: RwSignal<String>,
    replace: RwSignal<String>,
    required: RwSignal<bool>,
}

impl EditRow {
    fn new(e: &BuildEdit) -> Self {
        Self {
            key: NEXT_EDIT.fetch_add(1, Ordering::Relaxed),
            name: RwSignal::new(e.name.clone()),
            role: RwSignal::new(e.role.as_str().to_string()),
            find: RwSignal::new(e.find.clone()),
            replace: RwSignal::new(e.replace.clone()),
            required: RwSignal::new(e.required),
        }
    }

    fn get(&self) -> BuildEdit {
        BuildEdit {
            name: self.name.get_untracked().trim().to_string(),
            role: role_parse(&self.role.get_untracked()),
            find: self.find.get_untracked(),
            replace: self.replace.get_untracked(),
            required: self.required.get_untracked(),
        }
    }
}

const ROLES: [EditRole; 6] = [
    EditRole::Ccache,
    EditRole::Cache,
    EditRole::BuildInfo,
    EditRole::Qualify,
    EditRole::BaseImage,
    EditRole::Other,
];

fn role_parse(s: &str) -> EditRole {
    ROLES
        .into_iter()
        .find(|r| r.as_str() == s)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The modal
// ---------------------------------------------------------------------------

#[component]
pub fn BuildEditorModal() -> impl IntoView {
    let bk = use_bk();
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = bk.editor.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && bk.editor.with_untracked(Option::is_some) {
            bk.editor.set(None);
        }
    });
    view! {
        <Modal open=open title="Build" size=ModalSize::Wide guard=true>
            {move || bk.editor.get().map(|seed| view! { <BuildForm seed=seed open=open/> })}
        </Modal>
    }
}

fn opts(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(v, l)| (v.to_string(), l.to_string()))
        .collect()
}

#[component]
fn BuildForm(seed: EditorSeed, open: RwSignal<bool>) -> impl IntoView {
    let bk = use_bk();
    let toasts = bk.toasts;
    let modal = use_modal();
    let touch = move || {
        if let Some(m) = modal {
            m.touch();
        }
    };
    let scope = Scope::new();
    // Edit rows made later, from a click, belong to the form: made under the
    // owner of the control clicked (the "Customize" button sits in a Show
    // fallback its own click swaps out), their signals would be disposed
    // with it at once.
    // Held weakly: this StoredValue lives in that owner's own arena, and a
    // strong handle would keep the owner alive through itself.
    let form_owner = StoredValue::new(Owner::current().map(|o| o.downgrade()));
    let new_row = move |e: &BuildEdit| match form_owner.get_value().and_then(|w| w.upgrade()) {
        Some(o) => o.with(|| EditRow::new(e)),
        None => EditRow::new(e),
    };
    let id = seed.id;
    let creating = id.is_none();
    let slug_locked = seed.slug_locked;
    let last_size = seed.last_size;
    let seed_moving_tag = seed.moving_tag.clone();
    let s = seed.spec;

    let preset = RwSignal::new(seed.preset.clone().unwrap_or_default());
    let name = RwSignal::new(s.name.clone());
    let slug = RwSignal::new(s.slug.clone());
    // A new build's slug follows its name until the slug is typed into — a
    // preset's made-up slug too, or renaming a preset build would still
    // tag it `official-master`.
    let slug_auto = RwSignal::new(
        creating && (seed.slug_auto || s.slug.is_empty() || s.slug == slugify(&s.name)),
    );
    let engine = RwSignal::new(s.engine.as_str().to_string());
    let repo_url = RwSignal::new(s.repo_url.clone());
    let forge = RwSignal::new(s.forge.as_str().to_string());
    let forge_auto = RwSignal::new(creating && seed.preset.is_none());
    let git_ref = RwSignal::new(s.git_ref.clone());
    let extras = RwSignal::new(s.extras.clone());
    let backend = RwSignal::new(s.backend.as_str().to_string());
    let cuda = RwSignal::new(s.cuda_version.clone().unwrap_or_default());
    let arch = RwSignal::new(s.arch.clone().map(|a| a.join(", ")).unwrap_or_default());
    let dockerfile = RwSignal::new(s.dockerfile.clone().unwrap_or_default());
    let target = RwSignal::new(s.target.clone().unwrap_or_default());
    let edits = RwSignal::new(
        s.edits
            .as_ref()
            .map(|l| l.iter().map(EditRow::new).collect::<Vec<_>>()),
    );
    let ccache = RwSignal::new(s.ccache);
    let ccache_max = RwSignal::new(s.ccache_max_size.clone());
    let cpus = RwSignal::new(s.cpus.clone().unwrap_or_default());
    let build_args = RwSignal::new(s.build_args.clone());
    let keep_layers = RwSignal::new(s.keep_layers);
    let keep_runs = RwSignal::new(s.keep_runs.map(|n| n.to_string()).unwrap_or_default());
    let notes = RwSignal::new(s.notes.clone());
    let known = RwSignal::new(HashMap::<u64, ForgePr>::new());
    let resolved = RwSignal::new(None::<Result<ResolvedPreview, String>>);
    let merge = RwSignal::new(None::<Result<CheckMergeReport, String>>);
    let busy = RwSignal::new(None::<&'static str>);
    let field_err = RwSignal::new(None::<(&'static str, String)>);
    let adv_open = RwSignal::new(
        s.dockerfile.is_some()
            || s.target.is_some()
            || s.edits.is_some()
            || !s.build_args.trim().is_empty()
            || s.cpus.is_some()
            || !s.notes.trim().is_empty(),
    );

    let env = move || bk.env.get().and_then(Result::ok);
    // The picker shows which preset a saved build's repository is.
    Effect::new(move |_| {
        let Some(env) = env() else { return };
        if !preset.get_untracked().is_empty() {
            return;
        }
        let url = repo_url.get_untracked();
        if url.trim().is_empty() {
            return;
        }
        let key = repo_key(&url);
        let hit = env
            .repo_presets
            .iter()
            .find(|p| key.is_some() && repo_key(&p.repo_url) == key)
            .map(|p| p.id.clone());
        preset.set(hit.unwrap_or_else(|| "custom".to_string()));
    });
    let preset_opts = Signal::derive(move || {
        let mut v: Vec<(String, String)> = env()
            .map(|e| {
                e.repo_presets
                    .iter()
                    .map(|p| (p.id.clone(), p.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        v.push(("custom".to_string(), "Custom URL".to_string()));
        v
    });
    let engine_opts = Signal::derive(|| {
        opts(&[
            ("llama", "llama.cpp (chat, aux)"),
            ("audio", "audio.cpp (audio)"),
            ("sdcpp", "sd.cpp (image)"),
        ])
    });
    let forge_opts = Signal::derive(|| {
        opts(&[
            ("github", "GitHub"),
            ("gitlab", "GitLab"),
            ("plain", "plain git (no PRs)"),
        ])
    });
    let backend_opts = Signal::derive(|| {
        opts(&[
            ("cuda", "CUDA"),
            ("vulkan", "Vulkan (untested)"),
            ("rocm", "ROCm (untested)"),
            ("cpu", "CPU only (untested)"),
        ])
    });
    let role_opts = Signal::derive(|| {
        ROLES
            .iter()
            .map(|r| (r.as_str().to_string(), r.as_str().replace('_', " ")))
            .collect::<Vec<_>>()
    });

    // Field errors from the server, one at a time, under their field.
    let err_for = move |f: &'static str| {
        Signal::derive(move || {
            field_err.with(|e| e.as_ref().filter(|(k, _)| *k == f).map(|(_, m)| m.clone()))
        })
    };
    let clear = move |f: &'static str| {
        if field_err.with_untracked(|e| e.as_ref().is_some_and(|(k, _)| *k == f)) {
            field_err.set(None);
        }
    };
    let stale = move || {
        resolved.set(None);
        merge.set(None);
    };
    let report = move |msg: String| match field_of_error(&msg) {
        Some(f) => {
            if is_advanced(f) {
                adv_open.set(true);
            }
            field_err.set(Some((f, msg)));
        }
        None => toasts.err(msg),
    };

    let pick_preset = move || {
        let p = preset.get_untracked();
        let Some(env) = env() else { return };
        let Some(p) = env.repo_presets.iter().find(|x| x.id == p) else {
            return;
        };
        engine.set(p.engine.as_str().to_string());
        repo_url.set(p.repo_url.clone());
        forge.set(p.forge.as_str().to_string());
        forge_auto.set(false);
        git_ref.set(p.default_ref.clone());
        if name.with_untracked(|n| n.trim().is_empty()) {
            name.set(format!("{} {}", p.name, p.default_ref));
            if slug_auto.get_untracked() {
                slug.set(slugify(&format!("{}-{}", p.id, p.default_ref)));
            }
        }
        stale();
    };

    let collect = move || -> Result<BuildSpec, (&'static str, String)> {
        if let Some(e) = slug_error(&slug.get_untracked()) {
            return Err(("slug", e));
        }
        let keep = match keep_runs.get_untracked().trim() {
            "" => None,
            t => Some(t.parse::<u32>().map_err(|_| {
                (
                    "keep_runs",
                    format!(
                        "keep runs '{t}' is not a whole number — leave it empty to keep every run"
                    ),
                )
            })?),
        };
        let opt = |s: RwSignal<String>| {
            let v = s.get_untracked().trim().to_string();
            (!v.is_empty()).then_some(v)
        };
        let be = GpuBackend::parse(&backend.get_untracked()).unwrap_or_default();
        Ok(BuildSpec {
            slug: slug.get_untracked().trim().to_string(),
            name: name.get_untracked().trim().to_string(),
            engine: Engine::parse(&engine.get_untracked()).unwrap_or_default(),
            repo_url: repo_url.get_untracked().trim().to_string(),
            forge: Forge::parse(&forge.get_untracked()).unwrap_or_default(),
            git_ref: git_ref.get_untracked().trim().to_string(),
            extras: extras.get_untracked(),
            backend: be,
            cuda_version: if be == GpuBackend::Cuda {
                opt(cuda)
            } else {
                None
            },
            arch: parse_arch(&arch.get_untracked()),
            dockerfile: opt(dockerfile),
            target: opt(target),
            edits: edits.with_untracked(|e| {
                e.as_ref()
                    .map(|rows| rows.iter().map(EditRow::get).collect())
            }),
            ccache: ccache.get_untracked(),
            ccache_max_size: ccache_max.get_untracked().trim().to_string(),
            cpus: opt(cpus),
            build_args: build_args.get_untracked(),
            keep_layers: keep_layers.get_untracked(),
            keep_runs: keep,
            notes: notes.get_untracked(),
        })
    };
    let spec_or_mark = move || match collect() {
        Ok(s) => Some(s),
        Err((f, m)) => {
            if is_advanced(f) {
                adv_open.set(true);
            }
            field_err.set(Some((f, m)));
            None
        }
    };

    let resolve = move || {
        if busy.get_untracked().is_some() {
            return;
        }
        let Some(spec) = spec_or_mark() else { return };
        busy.set(Some("resolve"));
        resolved.set(None);
        scope.spawn(async move {
            let res = api::build_resolve(spec).await;
            busy.set(None);
            match res {
                Ok(p) => resolved.set(Some(Ok(p))),
                Err(e) => {
                    let m = e.to_string();
                    if let Some(f) = field_of_error(&m) {
                        field_err.set(Some((f, m.clone())));
                    }
                    resolved.set(Some(Err(m)));
                }
            }
        });
    };
    let check_merge = move || {
        if busy.get_untracked().is_some() {
            return;
        }
        let Some(spec) = spec_or_mark() else { return };
        busy.set(Some("merge"));
        merge.set(None);
        scope.spawn(async move {
            let args = BuildCheckMergeArgs {
                id: None,
                spec: Some(spec),
            };
            let res = api::build_check_merge(&args).await;
            busy.set(None);
            match res {
                Ok(r) => merge.set(Some(Ok(r))),
                Err(e) => {
                    let m = e.to_string();
                    if let Some(f) = field_of_error(&m) {
                        field_err.set(Some((f, m.clone())));
                    }
                    merge.set(Some(Err(m)));
                }
            }
        });
    };
    let save = move |and_run: bool| {
        if busy.get_untracked().is_some() {
            return;
        }
        let Some(spec) = spec_or_mark() else { return };
        field_err.set(None);
        busy.set(Some(if and_run { "run" } else { "save" }));
        let args = BuildSetArgs {
            action: if creating {
                BuildSetAction::Create
            } else {
                BuildSetAction::Update
            },
            id,
            spec: Some(spec),
            ..Default::default()
        };
        spawn_local(async move {
            let res = api::build_set(&args).await;
            busy.set(None);
            match res {
                Ok(r) => {
                    if let Some(m) = modal {
                        m.saved();
                    }
                    let saved = r.build;
                    let label = saved
                        .as_ref()
                        .map(|b| {
                            if b.spec.name.is_empty() {
                                b.spec.slug.clone()
                            } else {
                                b.spec.name.clone()
                            }
                        })
                        .unwrap_or_default();
                    toasts.ok(if creating {
                        format!("created '{label}'")
                    } else {
                        format!("saved '{label}'")
                    });
                    for k in r.kept {
                        toasts.warn(format!("kept {}: {}", k.tag, k.reason));
                    }
                    open.set(false);
                    bk.load_builds();
                    if and_run {
                        if let Some(b) = saved {
                            bk.run(b.id, label, false);
                        }
                    }
                }
                Err(e) => report(e.to_string()),
            }
        });
    };

    let slug_err =
        Signal::derive(move || slug_error(&slug.get()).or_else(|| err_for("slug").get()));
    // The server's own facts, preferred in order: a Resolve answer (the
    // current, unsaved spec's real preview, dev namespace included) beats
    // the build's already-saved moving tag (from its `BuildView`, on
    // `EditorSeed::edit` — stale the moment the slug or engine is retyped,
    // but never a guess), which beats a client-side guess for a build that
    // has neither yet (new, or just duplicated) — clearly labelled, since
    // whether this instance is a dev one is not known without asking.
    let tag_preview = move || {
        if let Some(Ok(r)) = resolved.get() {
            return r.moving_tag;
        }
        if let Some(m) = &seed_moving_tag {
            return m.clone();
        }
        let e = Engine::parse(&engine.get()).unwrap_or_default();
        let s = slug.get();
        let s = s.trim();
        if s.is_empty() {
            format!("{}:<slug> (preview)", e.image_repo())
        } else {
            format!("{} (preview — Resolve to confirm)", moving_tag(e, s))
        }
    };
    let ref_hint = move || {
        let r = git_ref.get();
        let r = r.trim();
        if is_full_sha(r) {
            "a commit: builds exactly it, and never updates".to_string()
        } else if r.len() >= 7 && r.len() < 40 && r.bytes().all(|b| b.is_ascii_hexdigit()) {
            "a short SHA cannot be fetched — paste the full 40-character SHA, or pick a branch or tag"
                .to_string()
        } else {
            "a branch follows its head on every run; a tag stays put".to_string()
        }
    };
    let is_cuda = move || backend.get() == "cuda";
    let cuda_ph = move || {
        env()
            .map(|e| format!("{} (preset default)", e.cuda_default))
            .unwrap_or_else(|| "preset default".to_string())
    };
    let cuda_warn = move || {
        let e = env()?;
        let max = e.driver_cuda_max?;
        let want = match cuda.get().trim() {
            "" => e.cuda_default.clone(),
            v => v.to_string(),
        };
        (is_cuda() && version_gt(&want, &max)).then(|| {
            format!(
                "CUDA {want} is newer than the driver supports ({max}): the image may build, then fail to start"
            )
        })
    };
    let cuda_hint = move || {
        if !is_cuda() {
            return "only for the CUDA backend".to_string();
        }
        match env().and_then(|e| e.driver_cuda_max) {
            Some(m) => format!("the driver supports up to {m}"),
            None => "the driver's limit was not detected".to_string(),
        }
    };
    let arch_ph = move || match env() {
        Some(e) if !e.arch_auto.is_empty() => format!("auto: {}", e.arch_auto.join(", ")),
        _ => "auto (the host GPUs)".to_string(),
    };
    let backend_hint = move || {
        if is_cuda() {
            "the presets are verified on CUDA".to_string()
        } else {
            "untested: the presets are verified on CUDA only".to_string()
        }
    };

    let custom_edits = Memo::new(move |_| edits.with(Option::is_some));
    let preset_edits = move || {
        resolved.with(|r| {
            r.as_ref()
                .and_then(|r| r.as_ref().ok())
                .map(|p| p.edits.clone())
        })
    };
    let customize = move || {
        let Some(list) = preset_edits() else { return };
        edits.set(Some(list.iter().map(new_row).collect()));
        clear("edits");
        touch();
    };

    let busy_is = move |what: &'static str| busy.get() == Some(what);
    let any_busy = move || busy.get().is_some();

    view! {
        {move || match bk.env.get() {
            Some(Err(e)) => {
                Some(
                    view! {
                        <div class="notice err row editor-top">
                            <span class="spacer">"The build environment did not load: " {e}</span>
                            <button class="btn ghost sm" on:click=move |_| bk.retry_env()>
                                "Retry"
                            </button>
                        </div>
                    }
                        .into_any(),
                )
            }
            Some(Ok(env)) => {
                let git = (!env.git_ok)
                    .then(|| {
                        view! {
                            <div class="notice err editor-top">
                                "git was not found on this machine: nothing can be fetched or built until it is installed."
                            </div>
                        }
                    });
                let dir = env
                    .builds_dir_warning
                    .map(|w| view! { <div class="notice warn editor-top">{w}</div> });
                Some(view! { {git} {dir} }.into_any())
            }
            None => None,
        }}
        <div class="card-flow editor-flow bk-editor">
            <section class="card edit-section">
                <h3>"Source"</h3>
                <div class="field-grid">
                    <div class="field" on:change=move |_| pick_preset()>
                        <label>"Repository"</label>
                        <Select value=preset options=preset_opts placeholder="Pick a repository"/>
                    </div>
                    <div class="field">
                        <label>"Engine"</label>
                        <Select value=engine options=engine_opts/>
                    </div>
                    <Field label="Repository URL" wide=true error=err_for("repo_url")>
                        <input
                            class="input mono"
                            placeholder="https://github.com/owner/repo"
                            prop:value=move || repo_url.get()
                            on:input=move |ev| {
                                let v = event_target_value(&ev);
                                if forge_auto.get_untracked() {
                                    forge
                                        .set(
                                            forge_for_url(&v)
                                                .unwrap_or(Forge::Plain)
                                                .as_str()
                                                .to_string(),
                                        );
                                }
                                let still = env()
                                    .and_then(|e| {
                                        e.repo_presets
                                            .iter()
                                            .find(|p| p.id == preset.get_untracked())
                                            .map(|p| repo_key(&p.repo_url) == repo_key(&v))
                                    })
                                    .unwrap_or(false);
                                if !still && !preset.with_untracked(String::is_empty) {
                                    preset.set("custom".to_string());
                                }
                                repo_url.set(v);
                                clear("repo_url");
                                stale();
                            }
                        />
                    </Field>
                    <div class="field" on:change=move |_| forge_auto.set(false)>
                        <label>
                            "Forge" <span class="field-unit">"where its PRs come from"</span>
                        </label>
                        <Select value=forge options=forge_opts/>
                    </div>
                    <Field label="Ref" hint=Signal::derive(ref_hint) error=err_for("ref")>
                        <RefCombo
                            value=git_ref
                            repo_url=repo_url
                            on_input=Callback::new(move |()| {
                                clear("ref");
                                stale();
                            })
                        />
                    </Field>
                    <Field label="Name" error=err_for("name")>
                        <input
                            class="input"
                            placeholder="llama.cpp master"
                            prop:value=move || name.get()
                            on:input=move |ev| {
                                let v = event_target_value(&ev);
                                if slug_auto.get_untracked() && !slug_locked {
                                    slug.set(slugify(&v));
                                }
                                name.set(v);
                                clear("name");
                            }
                        />
                    </Field>
                    <Field
                        label="Slug"
                        unit=if slug_locked { "fixed: the build has runs" } else { "becomes the tag" }
                        hint=Signal::derive(tag_preview)
                        error=slug_err
                    >
                        <input
                            class="input mono"
                            placeholder="official-master"
                            disabled=slug_locked
                            prop:value=move || slug.get()
                            on:input=move |ev| {
                                slug.set(event_target_value(&ev));
                                slug_auto.set(false);
                                clear("slug");
                                stale();
                            }
                        />
                    </Field>
                </div>
            </section>

            <section class="card edit-section">
                <h3>"Extras" <span class="count">{move || extras.with(Vec::len)}</span></h3>
                <ExtrasPicker
                    extras=extras
                    repo_url=repo_url
                    forge=forge
                    known=known
                    error=err_for("extras")
                    on_change=Callback::new(move |()| {
                        touch();
                        clear("extras");
                        stale();
                    })
                />
            </section>

            <section class="card edit-section">
                <h3>"Build"</h3>
                <div class="field-grid">
                    <Field label="GPU backend" hint=Signal::derive(backend_hint)>
                        <Select value=backend options=backend_opts/>
                    </Field>
                    <Field label="CUDA version" hint=Signal::derive(cuda_hint) error=err_for("cuda")>
                        <input
                            class="input mono"
                            placeholder=cuda_ph
                            disabled=move || !is_cuda()
                            prop:value=move || cuda.get()
                            on:input=move |ev| {
                                cuda.set(event_target_value(&ev));
                                clear("cuda");
                                stale();
                            }
                        />
                        {move || cuda_warn().map(|w| view! { <div class="field-warn">{w}</div> })}
                    </Field>
                    <Field
                        label="Architectures"
                        unit="comma-separated"
                        hint="empty builds for this machine's GPUs"
                        error=err_for("arch")
                    >
                        <input
                            class="input mono"
                            placeholder=arch_ph
                            prop:value=move || arch.get()
                            on:input=move |ev| {
                                arch.set(event_target_value(&ev));
                                clear("arch");
                                stale();
                            }
                        />
                    </Field>
                </div>
                {move || disk_note(last_size, env().as_ref())}
            </section>

            <section class="card edit-section">
                <h3>
                    <button
                        type="button"
                        class="link-btn bk-toggle"
                        aria-expanded=move || adv_open.get().to_string()
                        on:click=move |_| adv_open.update(|o| *o = !*o)
                    >
                        <span class="caret-icon" aria-hidden="true">"▸"</span>
                        "Advanced"
                    </button>
                </h3>
                <Show
                    when=move || adv_open.get()
                    fallback=|| {
                        view! {
                            <p class="dim mini-note">
                                "Dockerfile and target, Dockerfile edits, compiler cache, CPUs, build args, what is kept, notes."
                            </p>
                        }
                    }
                >
                    <div class="field-grid">
                        <Field label="Dockerfile" hint="empty: the preset's candidates" error=err_for("dockerfile")>
                            <input
                                class="input mono"
                                placeholder="auto"
                                prop:value=move || dockerfile.get()
                                on:input=move |ev| {
                                    dockerfile.set(event_target_value(&ev));
                                    clear("dockerfile");
                                    stale();
                                }
                            />
                        </Field>
                        <Field label="Target stage" hint="empty: the preset's candidates" error=err_for("target")>
                            <input
                                class="input mono"
                                placeholder="auto"
                                prop:value=move || target.get()
                                on:input=move |ev| {
                                    target.set(event_target_value(&ev));
                                    clear("target");
                                    stale();
                                }
                            />
                        </Field>
                    </div>

                    <div class="bk-edits">
                        <div class="mini-head">"Dockerfile edits"</div>
                        <Show
                            when=move || custom_edits.get()
                            fallback=move || {
                                view! {
                                    <div class="row">
                                        <span class="dim spacer">
                                            {move || match preset_edits() {
                                                Some(l) => format!("The preset's {} edits for the resolved Dockerfile.", l.len()),
                                                None => "The preset's edits for whichever Dockerfile resolves — Resolve lists them.".to_string(),
                                            }}
                                        </span>
                                        <button
                                            type="button"
                                            class="btn ghost sm"
                                            disabled=move || preset_edits().is_none()
                                            title=move || {
                                                if preset_edits().is_none() {
                                                    "Resolve first: the preset's edits depend on the Dockerfile it finds"
                                                } else {
                                                    "Copy the preset's edits into an explicit list to change"
                                                }
                                            }
                                            on:click=move |_| customize()
                                        >
                                            "Customize"
                                        </button>
                                    </div>
                                }
                            }
                        >
                            <For each=move || edits.get().unwrap_or_default() key=|r| r.key let:r>
                                <div class="bk-edit">
                                    <div class="field-grid">
                                        <div class="field">
                                            <label>"Name"</label>
                                            <input
                                                class="input mono"
                                                placeholder="(unnamed)"
                                                prop:value=move || r.name.get()
                                                on:input=move |ev| r.name.set(event_target_value(&ev))
                                            />
                                        </div>
                                        <div class="field">
                                            <label>"Role"</label>
                                            <Select value=r.role options=role_opts/>
                                        </div>
                                    </div>
                                    <div class="field">
                                        <label>"Find" <span class="field-unit">"literal, every occurrence"</span></label>
                                        <textarea
                                            class="input ta mono"
                                            prop:value=move || r.find.get()
                                            on:input=move |ev| {
                                                r.find.set(event_target_value(&ev));
                                                clear("edits");
                                            }
                                        ></textarea>
                                    </div>
                                    <div class="field">
                                        <label>"Replace"</label>
                                        <textarea
                                            class="input ta mono"
                                            prop:value=move || r.replace.get()
                                            on:input=move |ev| r.replace.set(event_target_value(&ev))
                                        ></textarea>
                                    </div>
                                    <div class="row">
                                        <label class="check spacer">
                                            <input
                                                type="checkbox"
                                                prop:checked=move || r.required.get()
                                                on:change=move |ev| r.required.set(event_target_checked(&ev))
                                            />
                                            "required — the run fails when it does not match"
                                        </label>
                                        <button
                                            type="button"
                                            class="btn ghost sm"
                                            on:click=move |_| {
                                                edits.update(|e| {
                                                    if let Some(l) = e {
                                                        l.retain(|x| x.key != r.key);
                                                    }
                                                });
                                                touch();
                                            }
                                        >
                                            "Remove"
                                        </button>
                                    </div>
                                </div>
                            </For>
                            <div class="row">
                                <button
                                    type="button"
                                    class="btn ghost sm"
                                    on:click=move |_| {
                                        edits.update(|e| {
                                            e.get_or_insert_with(Vec::new).push(new_row(&BuildEdit::default()));
                                        });
                                        touch();
                                    }
                                >
                                    "Add edit"
                                </button>
                                <button
                                    type="button"
                                    class="btn ghost sm"
                                    title="Drop this list and use the preset's edits again"
                                    on:click=move |_| {
                                        edits.set(None);
                                        clear("edits");
                                        touch();
                                    }
                                >
                                    "Reset to preset"
                                </button>
                            </div>
                        </Show>
                        {move || err_for("edits").get().map(|e| view! { <div class="field-err" role="alert">{e}</div> })}
                    </div>

                    <div class="field-grid">
                        <div class="field">
                            <label class="check">
                                <input
                                    type="checkbox"
                                    prop:checked=move || ccache.get()
                                    on:change=move |ev| ccache.set(event_target_checked(&ev))
                                />
                                "Compiler cache (ccache)"
                            </label>
                        </div>
                        <Field label="ccache max size" hint="e.g. 10G, 500M; 0 = no limit" error=err_for("ccache")>
                            <input
                                class="input mono"
                                placeholder=lmgw_api_types::builds::DEFAULT_CCACHE_MAX_SIZE
                                disabled=move || !ccache.get()
                                prop:value=move || ccache_max.get()
                                on:input=move |ev| {
                                    ccache_max.set(event_target_value(&ev));
                                    clear("ccache");
                                }
                            />
                        </Field>
                        <Field label="CPUs" hint="--cpuset-cpus, e.g. 0-15; empty = all cores" error=err_for("cpus")>
                            <input
                                class="input mono"
                                placeholder="all"
                                prop:value=move || cpus.get()
                                on:input=move |ev| {
                                    cpus.set(event_target_value(&ev));
                                    clear("cpus");
                                }
                            />
                        </Field>
                        <Field label="Build args" unit="KEY=VALUE, one per line" wide=true error=err_for("build_args")>
                            <textarea
                                class="input ta mono"
                                placeholder="GGML_CUDA_FA_ALL_QUANTS=ON"
                                prop:value=move || build_args.get()
                                on:input=move |ev| {
                                    build_args.set(event_target_value(&ev));
                                    clear("build_args");
                                    stale();
                                }
                            ></textarea>
                        </Field>
                        <div class="field">
                            <label class="check">
                                <input
                                    type="checkbox"
                                    prop:checked=move || keep_layers.get()
                                    on:change=move |ev| keep_layers.set(event_target_checked(&ev))
                                />
                                "Keep podman's layer cache"
                            </label>
                            <div class="field-hint">"off: no multi-GB intermediates are left behind"</div>
                        </div>
                        <Field
                            label="Keep runs"
                            hint="previous runs' images to keep; empty = keep all"
                            error=err_for("keep_runs")
                        >
                            <input
                                class="input mono w-num"
                                placeholder="all"
                                prop:value=move || keep_runs.get()
                                on:input=move |ev| {
                                    keep_runs.set(event_target_value(&ev));
                                    clear("keep_runs");
                                }
                            />
                        </Field>
                        <Field label="Notes" wide=true>
                            <textarea
                                class="input ta"
                                prop:value=move || notes.get()
                                on:input=move |ev| notes.set(event_target_value(&ev))
                            ></textarea>
                        </Field>
                    </div>
                </Show>
            </section>

            <section class="card edit-section">
                <h3>"Preview"</h3>
                {move || match (resolved.get(), merge.get()) {
                    (None, None) => {
                        view! {
                            <p class="dim mini-note">
                                "Resolve fetches the ref and shows the base commit, the Dockerfile, its edits "
                                "and the tags a run would produce. Check merge merges every extra onto the base "
                                "in a throwaway worktree. Neither builds anything."
                            </p>
                        }
                            .into_any()
                    }
                    (r, m) => {
                        view! {
                            {r.map(|r| match r {
                                Ok(p) => view! { <ResolvedView p=p/> }.into_any(),
                                Err(e) => view! { <div class="wiz-err">"Resolve: " {e}</div> }.into_any(),
                            })}
                            {m.map(|m| match m {
                                Ok(r) => view! { <MergeReportView r=r/> }.into_any(),
                                Err(e) => view! { <div class="wiz-err">"Check merge: " {e}</div> }.into_any(),
                            })}
                        }
                            .into_any()
                    }
                }}
            </section>
        </div>

        <ModalFooter>
            <button
                class="btn ghost"
                disabled=any_busy
                title="What a run would build right now — nothing is built"
                on:click=move |_| resolve()
            >
                {move || if busy_is("resolve") { "Resolving…" } else { "Resolve" }}
            </button>
            <button
                class="btn ghost"
                disabled=any_busy
                title="Merge every extra onto the base in a throwaway worktree"
                on:click=move |_| check_merge()
            >
                {move || if busy_is("merge") { "Merging…" } else { "Check merge" }}
            </button>
            <button class="btn" disabled=any_busy on:click=move |_| save(false)>
                {move || if busy_is("save") { "Saving…" } else { "Save" }}
            </button>
            <button class="btn primary" disabled=any_busy on:click=move |_| save(true)>
                {move || if busy_is("run") { "Saving…" } else { "Save & Run" }}
            </button>
        </ModalFooter>
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_ref_list_moves_by_key_and_stays_in_bounds() {
        // 5 rows drawn, a page of 3
        assert_eq!(list_step("ArrowDown", 0, 5, 3, false), Some(1));
        assert_eq!(list_step("ArrowDown", 4, 5, 3, false), Some(4));
        assert_eq!(list_step("ArrowUp", 0, 5, 3, false), Some(0));
        assert_eq!(list_step("PageDown", 1, 5, 3, false), Some(4));
        assert_eq!(list_step("PageUp", 4, 5, 3, false), Some(1));
        assert_eq!(list_step("Home", 3, 5, 3, false), Some(0));
        assert_eq!(list_step("End", 0, 5, 3, false), Some(4));
        // text in the filter: Home/End move its caret instead
        assert_eq!(list_step("Home", 3, 5, 3, true), None);
        // the list shrank under the row (a narrower filter)
        assert_eq!(list_step("ArrowUp", 9, 5, 3, false), Some(4));
        // nothing drawn, or not a move
        assert_eq!(list_step("ArrowDown", 0, 0, 3, false), None);
        assert_eq!(list_step("Enter", 0, 5, 3, false), None);
    }

    use super::*;
    use lmgw_api_types::builds::RefEntry;

    #[test]
    fn a_name_becomes_a_tag_safe_slug() {
        assert_eq!(
            slugify("Official master + PR 16391"),
            "official-master-pr-16391"
        );
        assert_eq!(slugify("  ik_llama.cpp main "), "ik_llama.cpp-main");
        assert_eq!(slugify("--Weird!!Name--"), "weird-name");
        assert_eq!(slugify("Ärger"), "rger");
        assert_eq!(slugify(""), "");
    }

    #[test]
    fn the_slug_charset_is_checked_as_typed() {
        assert_eq!(slug_error(""), None);
        assert_eq!(slug_error("official-master_1.2"), None);
        assert!(slug_error("-x").is_some());
        assert!(slug_error("Official").is_some());
        assert!(slug_error("a b").unwrap().contains("' '"));
        assert!(slug_error("a/b").unwrap().contains("'/'"));
    }

    #[test]
    fn server_refusals_land_under_their_field() {
        assert_eq!(
            field_of_error("the slug 'x' is 90 characters; the longest a slug can be is 85"),
            Some("slug")
        );
        assert_eq!(
            field_of_error("a build with the slug 'x' already exists"),
            Some("slug")
        );
        assert_eq!(
            field_of_error("repo_url 'x' names no host"),
            Some("repo_url")
        );
        assert_eq!(field_of_error("ref 'a..b' contains '..'"), Some("ref"));
        assert_eq!(
            field_of_error("extras[1]: PR #3 is already extras[0]"),
            Some("extras")
        );
        assert_eq!(
            field_of_error("cuda_version '13' is not a CUDA version"),
            Some("cuda")
        );
        assert_eq!(
            field_of_error("arch[0] 'x' is not an architecture name"),
            Some("arch")
        );
        assert_eq!(
            field_of_error("build_args line 2: 'x' is not"),
            Some("build_args")
        );
        assert_eq!(
            field_of_error("ccache_max_size 'x' is not a size"),
            Some("ccache")
        );
        assert_eq!(
            field_of_error("build_set: target 'x y' is not a stage"),
            Some("target")
        );
        assert_eq!(
            field_of_error("git fetch failed: network unreachable"),
            None
        );
        assert!(is_advanced("build_args"));
        assert!(!is_advanced("slug"));
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(version_gt("13.0.0", "12.9"));
        assert!(version_gt("12.10", "12.9"));
        assert!(!version_gt("12.9", "12.9.0"));
        assert!(!version_gt("12.4", "13.0"));
        assert!(!version_gt("latest", "12.0"));
    }

    #[test]
    fn arch_lists_split_on_commas_or_spaces_and_empty_is_auto() {
        assert_eq!(
            parse_arch("86, 89"),
            Some(vec!["86".to_string(), "89".to_string()])
        );
        assert_eq!(parse_arch("gfx1100"), Some(vec!["gfx1100".to_string()]));
        assert_eq!(parse_arch("  "), None);
    }

    #[test]
    fn the_disk_line_warns_only_when_less_is_free_than_the_last_run_took() {
        let g = 1_000_000_000;
        let (warn, t) = disk_words(Some(5 * g), Some(100 * g), "/b").unwrap();
        assert!(!warn);
        assert!(t.contains("free on the builds disk (/b)"), "{t}");
        let (warn, t) = disk_words(Some(5 * g), Some(2 * g), "").unwrap();
        assert!(warn);
        assert!(t.contains("less than the last run took"), "{t}");
        assert!(!disk_words(Some(g), None, "/b").unwrap().0);
        assert!(!disk_words(None, Some(g), "/b").unwrap().0);
        assert_eq!(disk_words(None, None, "/b"), None);
    }

    #[test]
    fn only_github_implies_its_forge() {
        assert_eq!(
            forge_for_url("https://github.com/ggml-org/llama.cpp"),
            Some(Forge::Github)
        );
        assert_eq!(forge_for_url("https://git.example.com/a/b"), None);
    }

    #[test]
    fn refs_list_the_default_branch_first_and_tags_newest_first() {
        let e = |n: &str| RefEntry {
            name: n.to_string(),
            sha: "0".repeat(40),
        };
        let v = RemoteRefsView {
            default_branch: "master".into(),
            heads: vec![e("refs/heads/zeta"), e("master"), e("alpha")],
            tags: vec![e("b999"), e("b6000"), e("refs/tags/b1000")],
        };
        let names: Vec<(String, &str)> = ref_items(&v)
            .into_iter()
            .map(|i| (i.name, i.kind))
            .collect();
        assert_eq!(
            names,
            vec![
                ("master".to_string(), "default"),
                ("alpha".to_string(), "branch"),
                ("zeta".to_string(), "branch"),
                ("b6000".to_string(), "tag"),
                ("b1000".to_string(), "tag"),
                ("b999".to_string(), "tag"),
            ]
        );
    }

    #[test]
    fn a_preset_seeds_a_new_build_that_keeps_three_runs() {
        let p = RepoPreset {
            id: "official".into(),
            name: "llama.cpp (official)".into(),
            engine: Engine::Llama,
            repo_url: "https://github.com/ggml-org/llama.cpp".into(),
            forge: Forge::Github,
            default_ref: "master".into(),
        };
        let s = EditorSeed::from_preset(&p);
        assert_eq!(s.spec.slug, "official-master");
        assert_eq!(s.spec.git_ref, "master");
        assert_eq!(s.spec.forge, Forge::Github);
        assert_eq!(s.spec.keep_runs, Some(DEFAULT_KEEP_RUNS));
        assert_eq!(s.id, None);
        assert!(!s.slug_locked);
        // Found in the live pass: renaming a preset build left its slug (and
        // so its tag) at the preset's `official-master`.
        assert!(s.slug_auto);
        assert_eq!(
            s.moving_tag, None,
            "not built yet: nothing to show but a preview"
        );
        let edit_seed = EditorSeed::edit_build(Build::default(), false, None);
        assert!(!edit_seed.slug_auto);
        assert_eq!(
            edit_seed.moving_tag, None,
            "no BuildView here (a duplicate's fresh Build): still a preview, never guessed"
        );
    }

    /// The slug hint of an existing build is the server's own fact — never
    /// recomputed client-side, dev instance included.
    #[test]
    fn editing_a_build_carries_its_views_moving_tag() {
        let v = BuildView {
            build: Build {
                id: 9,
                spec: BuildSpec {
                    engine: Engine::Llama,
                    slug: "official-master".into(),
                    ..BuildSpec::default()
                },
                ..Build::default()
            },
            moving_tag: "localhost/lmgw-dev-llama-server:official-master".into(),
            ..BuildView::default()
        };
        let seed = EditorSeed::edit(&v);
        assert_eq!(
            seed.moving_tag.as_deref(),
            Some("localhost/lmgw-dev-llama-server:official-master")
        );
    }
}
