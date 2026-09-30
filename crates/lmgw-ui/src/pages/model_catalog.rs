//! Models → Upstream catalogs: every model an `expose_all` upstream offers,
//! fetched live per upstream from `/api/upstream-models`, hidden ones
//! included. The DB holds only the hidden set, and `/v1/models` leaves hidden
//! models out, so this is the one place that shows both sides of that set and
//! moves models across it.
//!
//! A split page: the rail picks the upstream, the visibility and the vendor
//! (kilo alone has 60-odd), the pane is one table that scrolls on its own.
//! Filters, the rail's picks and the sort live in the URL, so a view is a
//! link (`/models/catalog?upstream=kilo-gw&vendor=qwen`).

use std::collections::{BTreeMap, HashMap, HashSet};

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{AliasView, UpstreamModelEntry, UpstreamModelsResponse};
use serde_json::{json, Value};

use super::models::{ModelsData, ModelsTabs};
use crate::catalog::{use_model_catalog, ModelCatalog};
use crate::fmt::{grouped, hue_for, local_datetime, of, per_mtok, price};
use crate::url_state::use_query_signal;
use crate::widgets::model_picker::ctx_label;
use crate::widgets::{
    filter_words, use_toasts, ConfirmButton, FilterBar, GroupRow, MenuItem, PageFrame, PageMode,
    RowMenu, Select, Toasts,
};

const COLS: u32 = 8;

/// A passthrough upstream, as `/api/models/full` lists it.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Upstream {
    id: i64,
    name: String,
    prefix: String,
}

/// One upstream catalog's state on this page. `Ready` holds hidden models
/// too: hidden is a flag on an entry here, never a reason to drop it.
#[derive(Clone, PartialEq)]
enum Fetch {
    Loading,
    Ready(Vec<Entry>),
    Failed(String),
}

/// One catalog model, flattened for the table.
#[derive(Clone, PartialEq, Debug)]
struct Entry {
    upstream: i64,
    /// Bare, as the upstream names it: what visibility and aliases take.
    id: String,
    /// What a client requests: `kilo/openai/gpt-5`.
    name: String,
    /// The maker the id names (`openai/gpt-5` → openai). An id without one
    /// files under its upstream's name, which is who makes it.
    vendor: String,
    ctx: Option<u64>,
    max_out: Option<u64>,
    /// Per million tokens. A negative published price (a router whose
    /// cost depends on where it sends the request) is `None` here and
    /// `price_varies` instead.
    price_in: Option<f64>,
    price_out: Option<f64>,
    price_varies: bool,
    created: Option<i64>,
    vision: bool,
    tools: bool,
    /// Can think: a reasoning control the catalog states, or a fixed
    /// reasoning that is on.
    thinks: bool,
    reasoning: Option<String>,
    task: Option<String>,
}

impl Entry {
    fn from_api(u: &Upstream, e: UpstreamModelEntry) -> Self {
        let prefix = u.prefix.trim_matches('/');
        let name = if prefix.is_empty() {
            e.id.clone()
        } else {
            format!("{prefix}/{}", e.id)
        };
        let vendor = match e.id.split_once('/') {
            Some((v, _)) if !v.is_empty() => v.to_string(),
            _ => u.name.clone(),
        };
        let per_m = |p: &Option<String>| p.as_deref().and_then(per_mtok);
        let (pin, pout) = (per_m(&e.price_prompt), per_m(&e.price_completion));
        let varies = pin.is_some_and(|p| p < 0.0) || pout.is_some_and(|p| p < 0.0);
        let thinks = match e.reasoning.as_deref() {
            Some("fixed") => e.reasoning_enabled == Some(true),
            Some(_) => true,
            None => false,
        };
        Self {
            upstream: u.id,
            vendor,
            ctx: e.context_length,
            max_out: e.max_output_tokens,
            price_in: pin.filter(|p| *p >= 0.0),
            price_out: pout.filter(|p| *p >= 0.0),
            price_varies: varies,
            // Some catalogs publish 0 for "no date"; 1970 is not a release.
            created: e.created.filter(|c| *c > 0),
            vision: e
                .input_modalities
                .as_ref()
                .is_some_and(|m| m.iter().any(|x| x == "image")),
            tools: e.tools == Some(true),
            thinks,
            reasoning: e.reasoning,
            task: e.task,
            name,
            id: e.id,
        }
    }

    fn matches(&self, words: &[String]) -> bool {
        if words.is_empty() {
            return true;
        }
        let hay = format!("{} {}", self.name, self.vendor).to_lowercase();
        words.iter().all(|w| hay.contains(w.as_str()))
    }

    fn key(&self) -> (i64, String) {
        (self.upstream, self.id.clone())
    }
}

/// The sortable columns. Sorting by name keeps the vendor groups; any other
/// order is one flat list, since "newest first" across vendors is the point
/// of sorting by date (and cheapest first, of sorting by price).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SortKey {
    Name,
    Ctx,
    Price,
    Created,
}

impl SortKey {
    fn param(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Ctx => "ctx",
            Self::Price => "price",
            Self::Created => "created",
        }
    }

    /// The direction a first click sorts in: what you want to see first
    /// (A→Z, the biggest context, the cheapest, the newest).
    fn first_desc(self) -> bool {
        matches!(self, Self::Ctx | Self::Created)
    }
}

/// `?sort=` → (column, descending). `-` prefixes a descending sort; empty or
/// unknown is by name, A→Z.
fn parse_sort(s: &str) -> (SortKey, bool) {
    let (desc, key) = match s.strip_prefix('-') {
        Some(k) => (true, k),
        None => (false, s),
    };
    let key = match key {
        "ctx" => SortKey::Ctx,
        "price" => SortKey::Price,
        "created" => SortKey::Created,
        _ => return (SortKey::Name, desc && key == "name"),
    };
    (key, desc)
}

/// The `?sort=` a click on `key`'s header writes: the other direction when
/// it already sorts, its first direction when it does not. The default
/// (name, A→Z) is the absent parameter.
fn next_sort(cur: &str, key: SortKey) -> String {
    let (k, desc) = parse_sort(cur);
    let desc = if k == key { !desc } else { key.first_desc() };
    match (key, desc) {
        (SortKey::Name, false) => String::new(),
        (k, true) => format!("-{}", k.param()),
        (k, false) => k.param().to_string(),
    }
}

/// Unknown sorts last in either direction: "no price published" is not
/// "free", and a missing date is not the oldest.
fn cmp_opt<T: PartialOrd>(a: Option<T>, b: Option<T>, desc: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (Some(a), Some(b)) => {
            let o = a.partial_cmp(&b).unwrap_or(Equal);
            if desc {
                o.reverse()
            } else {
                o
            }
        }
        (Some(_), None) => Less,
        (None, Some(_)) => Greater,
        (None, None) => Equal,
    }
}

fn sort_entries(list: &mut [&Entry], key: SortKey, desc: bool) {
    list.sort_by(|a, b| {
        let o = match key {
            SortKey::Name => {
                let o = (&a.vendor, &a.name).cmp(&(&b.vendor, &b.name));
                if desc {
                    o.reverse()
                } else {
                    o
                }
            }
            SortKey::Ctx => cmp_opt(a.ctx, b.ctx, desc),
            SortKey::Price => cmp_opt(a.price_in, b.price_in, desc),
            SortKey::Created => cmp_opt(a.created, b.created, desc),
        };
        o.then_with(|| a.name.cmp(&b.name))
    });
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Item {
    /// An upstream whose catalog is loading or failed.
    Status(i64),
    /// A vendor group: (key, vendor, upstream name).
    Group(String, String, String),
    /// (upstream, bare id, fetch generation): a refetch makes a new row.
    Row(i64, String, u32),
}

/// The table under the current filters, with every count the page shows.
#[derive(Clone, PartialEq, Default)]
struct Shown {
    items: Vec<Item>,
    /// The rows left after every filter, in order: what "shown" acts on.
    keys: Vec<(i64, String)>,
    /// Entries in the chosen upstream(s), before any filter.
    total: usize,
    /// Per vendor group: (shown, in scope).
    groups: HashMap<String, (usize, usize)>,
}

/// What the rail lists, all counted within the chosen upstream(s) and before
/// the text filter, so it holds still while you type.
#[derive(Clone, PartialEq, Default)]
struct Rail {
    /// (upstream, models, failure) — models is `None` while loading.
    upstreams: Vec<(Upstream, Option<usize>, Option<String>)>,
    /// Every loaded catalog together, whatever upstream is picked.
    everything: usize,
    /// The picked upstream(s).
    all: usize,
    hidden: usize,
    vendors: Vec<(String, usize)>,
}

/// Visibility ops, applied here first: the row flips at once and stays where
/// it is, and a refusal puts it back with the reason.
#[derive(Clone, Copy)]
struct Visibility {
    hidden: RwSignal<HashMap<i64, HashSet<String>>>,
    toasts: Toasts,
    catalog: ModelCatalog,
}

impl Visibility {
    fn is_hidden(&self, upstream: i64, id: &str) -> bool {
        self.hidden
            .with(|h| h.get(&upstream).is_some_and(|s| s.contains(id)))
    }

    fn set(self, targets: Vec<(i64, String)>, hide: bool) {
        let mut by: BTreeMap<i64, Vec<String>> = BTreeMap::new();
        for (u, id) in targets {
            by.entry(u).or_default().push(id);
        }
        for (upstream, ids) in by {
            let before: Vec<(String, bool)> = ids
                .iter()
                .map(|id| {
                    (
                        id.clone(),
                        self.hidden
                            .with_untracked(|h| h.get(&upstream).is_some_and(|s| s.contains(id))),
                    )
                })
                .collect();
            self.hidden.update(|h| {
                let s = h.entry(upstream).or_default();
                for id in &ids {
                    if hide {
                        s.insert(id.clone());
                    } else {
                        s.remove(id);
                    }
                }
            });
            spawn_local(async move {
                let res = crate::api::post::<Value, _>(
                    "/api/op/model_visibility",
                    &json!({
                        "action": if hide { "hide" } else { "unhide" },
                        "upstream_id": upstream,
                        "model_ids": ids,
                    }),
                )
                .await;
                match res {
                    Ok(v) => {
                        self.toasts.ok(v
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("done")
                            .to_string());
                        self.catalog.refresh();
                    }
                    Err(e) => {
                        self.hidden.update(|h| {
                            let s = h.entry(upstream).or_default();
                            for (id, was) in &before {
                                if *was {
                                    s.insert(id.clone());
                                } else {
                                    s.remove(id);
                                }
                            }
                        });
                        self.toasts.err(format!(
                            "{} failed, nothing changed: {e}",
                            if hide { "Hiding" } else { "Unhiding" }
                        ));
                    }
                }
            });
        }
    }
}

#[component]
pub fn ModelCatalogPage() -> impl IntoView {
    let data = ModelsData::new();
    let catalog = use_model_catalog();
    let toasts = use_toasts();
    let catalogs: RwSignal<HashMap<i64, (u32, Fetch)>> = RwSignal::new(HashMap::new());
    let vis = Visibility {
        hidden: RwSignal::new(HashMap::new()),
        toasts,
        catalog,
    };
    let selected: RwSignal<HashSet<(i64, String)>> = RwSignal::new(HashSet::new());
    let closed: RwSignal<HashSet<String>> = RwSignal::new(HashSet::new());
    let alias_edit: RwSignal<Option<AliasView>> = RwSignal::new(None);

    let upstream = use_query_signal("upstream");
    let vendor = use_query_signal("vendor");
    let query = use_query_signal("q");
    let hidden_only = use_query_signal("hidden");
    let sort = use_query_signal("sort");

    let upstreams = Memo::new(move |_| {
        data.full.with(|f| {
            let mut v: Vec<Upstream> = f
                .as_ref()
                .map(|f| {
                    f.passthrough
                        .iter()
                        .map(|p| Upstream {
                            id: p.id,
                            name: p.upstream.clone(),
                            prefix: p.prefix.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            v.sort_by(|a, b| a.name.cmp(&b.name));
            v
        })
    });

    let fetch = move |u: Upstream| {
        let mut gen = 0;
        catalogs.update(|m| {
            gen = m.get(&u.id).map_or(0, |(g, _)| *g) + 1;
            m.insert(u.id, (gen, Fetch::Loading));
        });
        spawn_local(async move {
            let res = crate::api::get::<UpstreamModelsResponse>(format!(
                "/api/upstream-models?id={}",
                u.id
            ))
            .await;
            let state = match res {
                Ok(r) => {
                    // An older gateway sends ids only; they still list.
                    let entries = if r.entries.is_empty() {
                        r.models
                            .into_iter()
                            .map(|id| UpstreamModelEntry {
                                id,
                                ..Default::default()
                            })
                            .collect()
                    } else {
                        r.entries
                    };
                    Fetch::Ready(
                        entries
                            .into_iter()
                            .map(|e| Entry::from_api(&u, e))
                            .collect(),
                    )
                }
                Err(e) => Fetch::Failed(e.to_string()),
            };
            catalogs.update(|m| {
                if m.get(&u.id).map(|(g, _)| *g) == Some(gen) {
                    m.insert(u.id, (gen, state));
                }
            });
        });
    };

    // The hidden set is the server's word whenever the list (re)loads, and
    // each upstream's catalog is fetched once it is known.
    Effect::new(move |_| {
        data.full.with(|f| {
            if let Some(f) = f {
                vis.hidden.set(
                    f.passthrough
                        .iter()
                        .map(|p| (p.id, p.hidden.iter().cloned().collect()))
                        .collect(),
                );
            }
        });
        for u in upstreams.get() {
            if !catalogs.with_untracked(|m| m.contains_key(&u.id)) {
                fetch(u);
            }
        }
    });

    // Another upstream is another set of vendors: picking one drops the
    // vendor. Done where the owner picks, not in an Effect on `upstream` — that
    // also ran when the URL moved underneath (Back, leaving the page) and
    // rewrote the vendor out of the address (review code:M2).
    let pick_upstream = move |name: String| {
        if upstream.get_untracked() != name {
            vendor.set(String::new());
            upstream.set(name);
        }
    };
    // The narrow pane's Select writes a signal of its own, so its pick goes
    // through the same door.
    let upstream_pick = RwSignal::new(upstream.get_untracked());
    Effect::new(move |_| {
        let u = upstream.get();
        if upstream_pick.get_untracked() != u {
            upstream_pick.set(u);
        }
    });
    Effect::new(move |_| {
        let v = upstream_pick.get();
        if v != upstream.get_untracked() {
            pick_upstream(v);
        }
    });
    // A selection made in another upstream would act on rows no longer on
    // screen. It is the page's own, not the address's: whatever moved the
    // upstream drops it.
    Effect::new(move |prev: Option<String>| {
        let u = upstream.get();
        if prev.is_some_and(|p| p != u) {
            selected.set(HashSet::new());
        }
        u
    });

    let index = Memo::new(move |_| {
        catalogs.with(|m| {
            let mut idx: HashMap<(i64, String), Entry> = HashMap::new();
            for (_, f) in m.values() {
                if let Fetch::Ready(list) = f {
                    for e in list {
                        idx.insert(e.key(), e.clone());
                    }
                }
            }
            idx
        })
    });

    let in_scope = move |u: &Upstream| upstream.with(|s| s.is_empty() || *s == u.name);

    let rail = Memo::new(move |_| {
        let mut r = Rail::default();
        let mut vendors: BTreeMap<String, usize> = BTreeMap::new();
        catalogs.with(|m| {
            for u in upstreams.get() {
                let (n, err) = match m.get(&u.id).map(|(_, f)| f) {
                    Some(Fetch::Ready(list)) => (Some(list.len()), None),
                    Some(Fetch::Failed(e)) => (None, Some(e.clone())),
                    _ => (None, None),
                };
                r.everything += n.unwrap_or(0);
                if let (Some(n), true) = (n, in_scope(&u)) {
                    r.all += n;
                    if let Some(Fetch::Ready(list)) = m.get(&u.id).map(|(_, f)| f) {
                        for e in list {
                            *vendors.entry(e.vendor.clone()).or_default() += 1;
                            if vis.is_hidden(u.id, &e.id) {
                                r.hidden += 1;
                            }
                        }
                    }
                }
                r.upstreams.push((u, n, err));
            }
        });
        r.vendors = vendors.into_iter().collect();
        r
    });

    let shown = Memo::new(move |_| {
        let words = filter_words(&query.get());
        let vend = vendor.get();
        let only_hidden = hidden_only.with(|h| h == "1");
        let (key, desc) = sort.with(|s| parse_sort(s));
        let grouped_view = key == SortKey::Name;
        let narrowed = !words.is_empty() || !vend.is_empty() || only_hidden;
        let mut s = Shown::default();
        let mut pool: Vec<Entry> = Vec::new();
        let mut gens: HashMap<i64, u32> = HashMap::new();
        let many = upstream.with(String::is_empty) && upstreams.with(|u| u.len() > 1);
        catalogs.with(|m| {
            for u in upstreams.get().iter().filter(|u| in_scope(u)) {
                match m.get(&u.id) {
                    Some((g, Fetch::Ready(list))) => {
                        gens.insert(u.id, *g);
                        pool.extend(list.iter().cloned());
                    }
                    _ => s.items.push(Item::Status(u.id)),
                }
            }
        });
        s.total = pool.len();
        let names: HashMap<i64, String> =
            upstreams.with(|us| us.iter().map(|u| (u.id, u.name.clone())).collect());
        let group_key = |e: &Entry| format!("{}\u{1f}{}", e.upstream, e.vendor);
        for e in &pool {
            s.groups.entry(group_key(e)).or_insert((0, 0)).1 += 1;
        }
        let mut hits: Vec<&Entry> = pool
            .iter()
            .filter(|e| {
                e.matches(&words)
                    && (vend.is_empty() || e.vendor == vend)
                    && (!only_hidden || vis.is_hidden(e.upstream, &e.id))
            })
            .collect();
        if grouped_view {
            // Groups follow the upstream, then the vendor; name order within.
            // Z→A turns the whole list over, groups too: reversed only inside
            // each group, "-name" still opened on the A vendors (review
            // code:M5).
            hits.sort_by(|a, b| {
                let o = (names.get(&a.upstream), &a.vendor, &a.name).cmp(&(
                    names.get(&b.upstream),
                    &b.vendor,
                    &b.name,
                ));
                if desc {
                    o.reverse()
                } else {
                    o
                }
            });
        } else {
            sort_entries(&mut hits, key, desc);
        }
        let closed = closed.get();
        let mut last_group = String::new();
        for e in hits {
            s.keys.push(e.key());
            let row = Item::Row(
                e.upstream,
                e.id.clone(),
                gens.get(&e.upstream).copied().unwrap_or(0),
            );
            if !grouped_view {
                s.items.push(row);
                continue;
            }
            let gk = group_key(e);
            s.groups.entry(gk.clone()).or_insert((0, 0)).0 += 1;
            if gk != last_group {
                let up = if many && names.get(&e.upstream) != Some(&e.vendor) {
                    names.get(&e.upstream).cloned().unwrap_or_default()
                } else {
                    String::new()
                };
                s.items.push(Item::Group(gk.clone(), e.vendor.clone(), up));
                last_group = gk.clone();
            }
            if narrowed || !closed.contains(&gk) {
                s.items.push(row);
            }
        }
        s
    });

    let selection_on = move || selected.with(|s| !s.is_empty());
    let narrowed = move || {
        !query.with(|q| q.trim().is_empty())
            || !vendor.with(String::is_empty)
            || hidden_only.with(|h| h == "1")
    };
    // What the bulk buttons act on: the ticked rows, or else every row the
    // filters leave on screen.
    let targets = move |hide: bool| -> Vec<(i64, String)> {
        let pool: Vec<(i64, String)> = if selection_on() {
            selected.with(|s| s.iter().cloned().collect())
        } else if narrowed() {
            shown.with(|s| s.keys.clone())
        } else {
            Vec::new()
        };
        pool.into_iter()
            .filter(|(u, id)| vis.is_hidden(*u, id) != hide)
            .collect()
    };
    let bulk = move || {
        let hide_n = targets(true).len();
        let unhide_n = targets(false).len();
        let what = if selection_on() { "selected" } else { "shown" };
        let button = move |hide: bool, n: usize| {
            let verb = if hide { "Hide" } else { "Unhide" };
            let label = format!("{verb} {} {what}", grouped(n as u64));
            let run = Callback::new(move |()| {
                vis.set(targets(hide), hide);
                selected.set(HashSet::new());
            });
            if n == 1 {
                view! {
                    <button class="btn ghost sm" on:click=move |_| run.run(())>
                        {label}
                    </button>
                }
                .into_any()
            } else {
                view! {
                    <ConfirmButton
                        label=label
                        confirm=format!("{verb} {}?", grouped(n as u64))
                        on_confirm=run
                        title=if hide {
                            "Leave them out of /v1/models; they stay requestable by name"
                        } else {
                            "List them in /v1/models again"
                        }
                    />
                }
                .into_any()
            }
        };
        view! {
            {(hide_n > 0).then(|| button(true, hide_n))}
            {(unhide_n > 0).then(|| button(false, unhide_n))}
            {selection_on()
                .then(|| {
                    view! {
                        <button
                            class="btn ghost sm"
                            title="Clear the selection"
                            on:click=move |_| selected.set(HashSet::new())
                        >
                            {format!("✕ {} selected", grouped(selected.with(HashSet::len) as u64))}
                        </button>
                    }
                })}
        }
    };

    let chips = Signal::derive(move || {
        let mut v: Vec<(String, Callback<()>)> = Vec::new();
        let vend = vendor.get();
        if !vend.is_empty() {
            v.push((
                format!("vendor: {vend}"),
                Callback::new(move |()| vendor.set(String::new())),
            ));
        }
        if hidden_only.with(|h| h == "1") {
            v.push((
                "hidden only".to_string(),
                Callback::new(move |()| hidden_only.set(String::new())),
            ));
        }
        v
    });

    let configured = Signal::derive(move || {
        data.full.with(|f| {
            f.as_ref().map(|f| {
                f.local.len() + f.aux.len() + f.audio.len() + f.image.len() + f.aliases.len()
            })
        })
    });
    // Hidden ids a loaded catalog no longer offers: the upstream retired the
    // model. They stay in the hidden set, where no row can ever show them or
    // unhide them, so they are named apart with a way to forget them (review
    // code:M3).
    let retired = Memo::new(move |_| {
        let mut out: Vec<(i64, String, Vec<String>)> = Vec::new();
        catalogs.with(|m| {
            vis.hidden.with(|h| {
                for u in upstreams.get() {
                    let (Some((_, Fetch::Ready(list))), Some(ids)) = (m.get(&u.id), h.get(&u.id))
                    else {
                        continue;
                    };
                    let offered: HashSet<&str> = list.iter().map(|e| e.id.as_str()).collect();
                    let mut gone: Vec<String> = ids
                        .iter()
                        .filter(|id| !offered.contains(id.as_str()))
                        .cloned()
                        .collect();
                    if !gone.is_empty() {
                        gone.sort();
                        out.push((u.id, u.name.clone(), gone));
                    }
                }
            })
        });
        out
    });
    let catalog_counts = Signal::derive(move || {
        let (mut n, mut failed) = (0, 0);
        catalogs.with(|m| {
            for (_, f) in m.values() {
                match f {
                    Fetch::Ready(list) => n += list.len(),
                    Fetch::Failed(_) => failed += 1,
                    Fetch::Loading => {}
                }
            }
        });
        // Hidden models of the catalogs: a retired id is not one (it is said
        // apart, above the table), so the tab agrees with the rail.
        let all: usize = vis.hidden.with(|h| h.values().map(HashSet::len).sum());
        let gone: usize = retired.with(|r| r.iter().map(|(_, _, ids)| ids.len()).sum());
        data.full
            .with(Option::is_some)
            .then_some((n, all - gone, failed))
    });

    let upstream_by_id =
        move |id: i64| upstreams.with(|us| us.iter().find(|u| u.id == id).cloned());

    let render = move |it: Item| -> AnyView {
        match it {
            Item::Status(id) => {
                let Some(u) = upstream_by_id(id) else {
                    return ().into_any();
                };
                view! { <StatusRow u=u catalogs=catalogs on_retry=Callback::new(fetch)/> }
                    .into_any()
            }
            Item::Group(key, vend, up) => {
                let open = RwSignal::new(closed.with_untracked(|c| !c.contains(&key)));
                let k = key.clone();
                // The group's open state lives in `closed`, which the table
                // reads; the row's own signal only drives its caret.
                Effect::new(move |prev: Option<bool>| {
                    let o = open.get();
                    if prev.is_some() {
                        closed.update(|c| {
                            if o {
                                c.remove(&k);
                            } else {
                                c.insert(k.clone());
                            }
                        });
                    }
                    o
                });
                let count = Signal::derive(move || {
                    shown.with(|s| {
                        s.groups
                            .get(&key)
                            .map(|(n, t)| of(*n, *t))
                            .unwrap_or_default()
                    })
                });
                view! {
                    <GroupRow
                        colspan=COLS
                        label=vend
                        count=count
                        open=open
                        meta=move || up.clone()
                    />
                }
                .into_any()
            }
            Item::Row(u, id, _) => {
                let Some(e) = index.with_untracked(|m| m.get(&(u, id)).cloned()) else {
                    return ().into_any();
                };
                view! { <CatalogRow e=e vis=vis selected=selected alias_edit=alias_edit toasts=toasts/> }.into_any()
            }
        }
    };

    let all_ticked = move || {
        shown.with(|s| {
            !s.keys.is_empty() && selected.with(|sel| s.keys.iter().all(|k| sel.contains(k)))
        })
    };
    let some_ticked =
        move || shown.with(|s| selected.with(|sel| s.keys.iter().any(|k| sel.contains(k))));
    let tick_all = move |_| {
        let keys = shown.with_untracked(|s| s.keys.clone());
        if all_ticked() {
            selected.update(|sel| {
                for k in &keys {
                    sel.remove(k);
                }
            });
        } else {
            selected.update(|sel| sel.extend(keys));
        }
    };

    let sort_th = move |label: &'static str, key: SortKey, class: &'static str| {
        let state = move || {
            let (k, desc) = sort.with(|s| parse_sort(s));
            (k == key).then_some(desc)
        };
        view! {
            <th
                class=class
                aria-sort=move || match state() {
                    Some(true) => "descending",
                    Some(false) => "ascending",
                    None => "none",
                }
            >
                <button
                    type="button"
                    class="th-sort"
                    title="Sort by this column; again to reverse"
                    on:click=move |_| sort.set(next_sort(&sort.get_untracked(), key))
                >
                    {label}
                    <span class="th-arrow" aria-hidden="true">
                        {move || match state() {
                            Some(true) => "▼",
                            Some(false) => "▲",
                            None => "",
                        }}
                    </span>
                </button>
            </th>
        }
    };

    let upstream_opts = Signal::derive(move || {
        rail.with(|r| {
            let mut v = vec![(
                String::new(),
                format!("All upstreams · {}", grouped(r.everything as u64)),
            )];
            for (u, n, err) in &r.upstreams {
                let size = match (n, err) {
                    (Some(n), _) => grouped(*n as u64),
                    (None, Some(_)) => "failed".into(),
                    (None, None) => "loading".into(),
                };
                v.push((u.name.clone(), format!("{} · {size}", u.name)));
            }
            v
        })
    });
    let vendor_opts = Signal::derive(move || {
        rail.with(|r| {
            let mut v = vec![(String::new(), format!("All vendors · {}", r.vendors.len()))];
            v.extend(
                r.vendors
                    .iter()
                    .map(|(name, n)| (name.clone(), format!("{name} · {n}"))),
            );
            v
        })
    });

    view! {
        <PageFrame
            title="Models"
            sub="every model the passthrough upstreams offer, fetched live"
            mode=PageMode::Split
            head_extra=move || view! { <ModelsTabs configured=configured catalogs=catalog_counts/> }
            toolbar=move || {
                view! {
                    <FilterBar
                        query=query
                        placeholder="Filter by name or vendor"
                        shown=Signal::derive(move || shown.with(|s| s.keys.len()))
                        total=Signal::derive(move || shown.with(|s| s.total))
                        noun="models"
                        chips=chips
                        extra=move || bulk
                        on_clear=Callback::new(move |()| {
                            vendor.set(String::new());
                            hidden_only.set(String::new());
                        })
                    />
                }
            }
        >
            <nav class="split-rail" aria-label="Catalog scope">
                <div class="rail-head">"Upstreams"</div>
                <RailItem
                    label="All upstreams"
                    count=Signal::derive(move || rail.with(|r| Some(r.everything)))
                    active=Signal::derive(move || upstream.with(String::is_empty))
                    on_pick=Callback::new(move |()| pick_upstream(String::new()))
                />
                <For
                    each=move || rail.with(|r| r.upstreams.clone())
                    key=|(u, n, err)| (u.clone(), *n, err.clone())
                    let:item
                >
                    {
                        let (u, n, err) = item;
                        let name = u.name.clone();
                        let pick = name.clone();
                        view! {
                            <RailItem
                                label=name.clone()
                                count=Signal::stored(n)
                                hue=hue_for(&name)
                                bad=err.is_some()
                                active=Signal::derive(move || upstream.with(|s| *s == name))
                                on_pick=Callback::new(move |()| pick_upstream(pick.clone()))
                            />
                            {err.map(|e| view! { <div class="rail-err" title=e.clone()>{e.clone()}</div> })}
                        }
                    }
                </For>
                <div class="rail-head">"Visibility"</div>
                <RailItem
                    label="Listed + hidden"
                    count=Signal::derive(move || rail.with(|r| Some(r.all)))
                    active=Signal::derive(move || hidden_only.with(|h| h != "1"))
                    on_pick=Callback::new(move |()| hidden_only.set(String::new()))
                />
                <RailItem
                    label="Hidden"
                    count=Signal::derive(move || rail.with(|r| Some(r.hidden)))
                    active=Signal::derive(move || hidden_only.with(|h| h == "1"))
                    on_pick=Callback::new(move |()| hidden_only.set("1".into()))
                />
                <div class="rail-head">
                    "Vendors" <span class="count">{move || rail.with(|r| r.vendors.len())}</span>
                </div>
                <RailItem
                    label="All vendors"
                    count=Signal::derive(move || rail.with(|r| Some(r.all)))
                    active=Signal::derive(move || vendor.with(String::is_empty))
                    on_pick=Callback::new(move |()| vendor.set(String::new()))
                />
                <For each=move || rail.with(|r| r.vendors.clone()) key=|v| v.clone() let:v>
                    {
                        let (name, n) = v;
                        let pick = name.clone();
                        let me = name.clone();
                        view! {
                            <RailItem
                                label=name.clone()
                                count=Signal::stored(Some(n))
                                hue=hue_for(&name)
                                active=Signal::derive(move || vendor.with(|s| *s == me))
                                on_pick=Callback::new(move |()| vendor.set(pick.clone()))
                            />
                        }
                    }
                </For>
            </nav>
            <div class="split-pane">
                <div class="rail-select">
                    <Select value=upstream_pick options=upstream_opts/>
                    <Select value=vendor options=vendor_opts/>
                    <button
                        type="button"
                        class="facet"
                        class:on=move || hidden_only.with(|h| h == "1")
                        aria-pressed=move || hidden_only.with(|h| h == "1").to_string()
                        on:click=move |_| {
                            let on = hidden_only.with_untracked(|h| h == "1");
                            hidden_only.set(if on { String::new() } else { "1".into() });
                        }
                    >
                        "Hidden only"
                        <span class="count">{move || rail.with(|r| grouped(r.hidden as u64))}</span>
                    </button>
                </div>
                <For
                    each=move || retired.get()
                    key=|r| r.clone()
                    let:r
                >
                    {
                        let (up, name, ids) = r;
                        let n = ids.len();
                        let tip = format!("No row can show these:\n{}", ids.join("\n"));
                        let forget = ids.iter().map(|id| (up, id.clone())).collect::<Vec<_>>();
                        let forget = StoredValue::new(forget);
                        view! {
                            <div class="notice row catalog-retired" title=tip>
                                <span>
                                    {format!(
                                        "{} on the hidden list {} no longer offered by {name}",
                                        crate::fmt::count_of(n, "ids"),
                                        if n == 1 { "is" } else { "are" },
                                    )}
                                </span>
                                <button
                                    class="btn ghost sm"
                                    title="Take them off the hidden list; a model that comes back is listed again"
                                    on:click=move |_| vis.set(forget.get_value(), false)
                                >
                                    "Forget them"
                                </button>
                            </div>
                        }
                    }
                </For>
                {move || {
                    data.error
                        .get()
                        .map(|e| {
                            view! {
                                <div class="notice err row">
                                    "Loading the upstream list failed: " {e}
                                    <button class="btn ghost sm" on:click=move |_| data.load()>
                                        "Retry"
                                    </button>
                                </div>
                            }
                        })
                }}
                <div class="fill-pane card pad0">
                    <table class="data">
                        <thead>
                            <tr>
                                <th class="pick">
                                    <input
                                        type="checkbox"
                                        title="Select every shown model"
                                        prop:checked=all_ticked
                                        prop:indeterminate=move || some_ticked() && !all_ticked()
                                        on:change=tick_all
                                    />
                                </th>
                                {sort_th("Model", SortKey::Name, "")}
                                {sort_th("Ctx", SortKey::Ctx, "num-h")}
                                <th class="col-p3 num-h" title="The most tokens one response may generate">"Max out"</th>
                                {sort_th("Price $/M", SortKey::Price, "num-h")}
                                <th class="col-p2">"Caps"</th>
                                {sort_th("Created", SortKey::Created, "col-p3 num-h")}
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>
                            <Show when=move || data.full.with(Option::is_none) && data.error.with(Option::is_none)>
                                <tr>
                                    <td colspan=COLS class="dim">"Loading…"</td>
                                </tr>
                            </Show>
                            <Show when=move || data.full.with(Option::is_some) && upstreams.with(Vec::is_empty)>
                                <tr>
                                    <td colspan=COLS class="wrap dim">
                                        "No upstream exposes its whole catalog. Turn on “expose all models” for one on "
                                        <a href="/upstreams">"Upstreams"</a>
                                        " and its models are listed here."
                                    </td>
                                </tr>
                            </Show>
                            <For each=move || shown.with(|s| s.items.clone()) key=|it| it.clone() let:it>
                                {render(it)}
                            </For>
                        </tbody>
                    </table>
                </div>
                <super::model_editors::AliasEditor
                    editing=alias_edit
                    on_saved=move || {
                        data.load();
                        catalog.refresh();
                    }
                />
            </div>
        </PageFrame>
    }
}

/// One rail entry: a label, its count, maybe a swatch; the current pick is
/// marked for assistive tech as well as by colour.
#[component]
fn RailItem(
    #[prop(into)] label: String,
    /// `None` while its catalog is loading or after it failed.
    #[prop(into)]
    count: Signal<Option<usize>>,
    #[prop(optional)] hue: Option<u32>,
    #[prop(optional)] bad: bool,
    #[prop(into)] active: Signal<bool>,
    on_pick: Callback<()>,
) -> impl IntoView {
    view! {
        <button
            type="button"
            class="rail-item"
            aria-current=move || active.get().to_string()
            title=label.clone()
            on:click=move |_| on_pick.run(())
        >
            {(hue.is_some() || bad)
                .then(|| {
                    view! {
                        <i
                            class="rail-dot"
                            class:bad=bad
                            style=hue.map(|h| format!("--hue:{h}")).unwrap_or_default()
                        ></i>
                    }
                })}
            <span class="rail-label">{label.clone()}</span>
            {move || {
                count.get().map(|n| view! { <span class="count">{grouped(n as u64)}</span> })
            }}
        </button>
    }
}

/// An upstream whose catalog is still loading, or failed — with the
/// server's reason and a retry, never an empty table that says nothing.
#[component]
fn StatusRow(
    u: Upstream,
    catalogs: RwSignal<HashMap<i64, (u32, Fetch)>>,
    on_retry: Callback<Upstream>,
) -> impl IntoView {
    let id = u.id;
    let name = u.name.clone();
    let failure = move || {
        catalogs.with(|m| match m.get(&id) {
            Some((_, Fetch::Failed(e))) => Some(e.clone()),
            _ => None,
        })
    };
    let u = StoredValue::new(u);
    view! {
        <tr class="group">
            <td colspan=COLS>
                <div class="group-cell">
                    {move || match failure() {
                        Some(e) => {
                            let line = format!("{name} catalog failed: {e}");
                            let tip = line.clone();
                            view! {
                                <span class="problem group-problem" title=tip>{line}</span>
                                <span class="group-actions">
                                    <button class="btn ghost sm" on:click=move |_| on_retry.run(u.get_value())>
                                        "Retry"
                                    </button>
                                </span>
                            }
                                .into_any()
                        }
                        None => view! { <span class="dim">{format!("Loading the {name} catalog…")}</span> }.into_any(),
                    }}
                </div>
            </td>
        </tr>
    }
}

#[component]
fn CatalogRow(
    e: Entry,
    vis: Visibility,
    selected: RwSignal<HashSet<(i64, String)>>,
    alias_edit: RwSignal<Option<AliasView>>,
    toasts: Toasts,
) -> impl IntoView {
    let key = StoredValue::new(e.key());
    let upstream = e.upstream;
    let id = StoredValue::new(e.id.clone());
    let name = StoredValue::new(e.name.clone());
    let is_hidden = move || id.with_value(|i| vis.is_hidden(upstream, i));
    let ticked = move || key.with_value(|k| selected.with(|s| s.contains(k)));
    let (pfx, base) = match e.name.rfind('/') {
        Some(i) => e.name.split_at(i + 1),
        None => ("", e.name.as_str()),
    };
    let (pfx, base) = (pfx.to_string(), base.to_string());
    let price_txt = match (e.price_in, e.price_out) {
        _ if e.price_varies => "varies".to_string(),
        (Some(i), Some(o)) if i == 0.0 && o == 0.0 => "free".to_string(),
        (Some(i), Some(o)) => format!("{} · {}", price(i), price(o)),
        (Some(i), None) => format!("{} · —", price(i)),
        (None, Some(o)) => format!("— · {}", price(o)),
        (None, None) => "—".to_string(),
    };
    let num = |n: Option<u64>| n.map(ctx_label).unwrap_or_else(|| "—".into());
    let tokens = |n: Option<u64>, what: &str| n.map(|n| format!("{} tokens {what}", grouped(n)));
    let created = e.created.map(|t| local_datetime(t as f64));
    let copy = move |text: String| {
        let _ = window().navigator().clipboard().write_text(&text);
        toasts.ok(format!("Copied {text}"));
    };
    let items = Signal::derive(move || {
        vec![
            MenuItem::new("Alias…", move || {
                let bare = id.get_value();
                let short = bare.rsplit('/').next().unwrap_or(&bare).to_string();
                alias_edit.set(Some(AliasView {
                    alias: short,
                    upstream_id: upstream,
                    upstream_model_id: bare,
                    ..Default::default()
                }))
            })
            .title("Give it a friendly name of your own, with parameter overrides if you like"),
            MenuItem::new("Copy model name", move || copy(name.get_value()))
                .title("The name a client requests"),
            MenuItem::new("Copy upstream id", move || copy(id.get_value()))
                .title("The id as the upstream itself names it"),
        ]
    });
    view! {
        <tr class:muted=is_hidden>
            <td class="pick">
                <input
                    type="checkbox"
                    aria-label="Select"
                    prop:checked=ticked
                    on:change=move |_| {
                        let k = key.get_value();
                        selected.update(|s| {
                            if !s.remove(&k) {
                                s.insert(k);
                            }
                        });
                    }
                />
            </td>
            <td class="clip mono-sm" title=e.name.clone()>
                // The shared prefix gives way first, so a narrow column still
                // shows what tells one model from the next.
                <div class="id-cell">
                    <i class="swatch" style=format!("--hue:{}", hue_for(&e.vendor))></i>
                    <span class="pfx">{pfx}</span>
                    <span class="id-base">{base}</span>
                    // Listed is the normal state and goes unsaid; hidden is
                    // said on the row itself, which is also dimmed.
                    <Show when=is_hidden>
                        <span class="chip off id-flag" title="Left out of /v1/models; still requestable by name">
                            "hidden"
                        </span>
                    </Show>
                </div>
            </td>
            <td class="num" title=tokens(e.ctx, "of context")>{num(e.ctx)}</td>
            <td class="num col-p3" title=tokens(e.max_out, "per response at most")>{num(e.max_out)}</td>
            <td
                class="num"
                title=if e.price_varies {
                    "The upstream publishes no fixed price: it depends on where the router sends each request"
                } else {
                    "US$ per million tokens, in · out"
                }
            >
                {price_txt}
            </td>
            <td class="col-p2">
                {e.task.clone().filter(|t| t != "chat").map(|t| view! { <span class="type-badge">{t}</span> })}
                {e.vision.then(|| view! { <span class="type-badge" title="takes images">"vision"</span> })}
                {e.tools.then(|| view! { <span class="type-badge" title="calls tools">"tools"</span> })}
                {e.thinks
                    .then(|| {
                        let how = e.reasoning.clone().unwrap_or_default();
                        view! {
                            <span class="type-badge" title=format!("can reason ({how})")>"think"</span>
                        }
                    })}
            </td>
            <td class="num col-p3" title=created.clone()>
                {created.clone().map(|c| c[..10].to_string()).unwrap_or_else(|| "—".into())}
            </td>
            <td class="actions">
                <button
                    class="btn ghost sm"
                    title=move || {
                        if is_hidden() {
                            "List it in /v1/models again"
                        } else {
                            "Leave it out of /v1/models; it stays requestable by name"
                        }
                    }
                    on:click=move |_| {
                        let hide = !is_hidden();
                        vis.set(vec![key.get_value()], hide);
                    }
                >
                    {move || if is_hidden() { "Unhide" } else { "Hide" }}
                </button>
                " "
                <RowMenu items=items/>
            </td>
        </tr>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_sorts_its_first_way_then_flips() {
        assert_eq!(next_sort("", SortKey::Created), "-created");
        assert_eq!(next_sort("-created", SortKey::Created), "created");
        assert_eq!(next_sort("created", SortKey::Price), "price");
        assert_eq!(next_sort("", SortKey::Name), "-name");
        assert_eq!(next_sort("-name", SortKey::Name), "");
    }

    #[test]
    fn an_unknown_sort_is_by_name() {
        assert_eq!(parse_sort(""), (SortKey::Name, false));
        assert_eq!(parse_sort("bogus"), (SortKey::Name, false));
        assert_eq!(parse_sort("-ctx"), (SortKey::Ctx, true));
    }

    #[test]
    fn unknown_values_sort_last_both_ways() {
        let u = Upstream {
            id: 1,
            name: "up".into(),
            prefix: "up".into(),
        };
        let mk = |id: &str, p: Option<&str>| {
            Entry::from_api(
                &u,
                UpstreamModelEntry {
                    id: id.into(),
                    price_prompt: p.map(str::to_string),
                    ..Default::default()
                },
            )
        };
        let (a, b, c) = (
            mk("a", Some("0.000001")),
            mk("b", None),
            mk("c", Some("0.000002")),
        );
        for desc in [false, true] {
            let mut l = vec![&b, &c, &a];
            sort_entries(&mut l, SortKey::Price, desc);
            assert_eq!(l.last().unwrap().id, "b", "desc={desc}");
        }
    }

    #[test]
    fn an_entry_without_a_vendor_files_under_its_upstream() {
        let u = Upstream {
            id: 3,
            name: "aistudio".into(),
            prefix: "aistudio".into(),
        };
        let e = Entry::from_api(
            &u,
            UpstreamModelEntry {
                id: "gemini-pro".into(),
                reasoning: Some("fixed".into()),
                reasoning_enabled: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(
            (e.vendor.as_str(), e.name.as_str()),
            ("aistudio", "aistudio/gemini-pro")
        );
        assert!(!e.thinks, "a fixed-off model does not think");
        let k = Entry::from_api(
            &Upstream {
                id: 2,
                name: "kilo-gw".into(),
                prefix: "kilo".into(),
            },
            UpstreamModelEntry {
                id: "openai/gpt-5".into(),
                reasoning: Some("levels".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            (k.vendor.as_str(), k.name.as_str()),
            ("openai", "kilo/openai/gpt-5")
        );
        assert!(k.thinks);
    }
}
