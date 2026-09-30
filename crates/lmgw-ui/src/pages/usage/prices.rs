use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{KeysResponse, PriceRowView, PricesResponse, UpstreamsResponse};
use serde_json::{json, Value};

use crate::fmt::grouped;
use crate::widgets::{
    use_toasts, ConfirmButton, Explain, Facet, FacetSet, FilterBar, GroupRow, Modal, ModalFooter,
    PageFrame, PageMode, Select,
};

use super::*;

/// Prices (`/usage/prices`): the price sheets decide how much of the spend on
/// Charts is knowable at all, so the unpriced models sit with them — pinned
/// first, each with its Price action.
///
/// One fill table: a band per upstream (by name, with its count, and the
/// kind and unit its rows all share), every row in the one scroller.
#[component]
pub fn UsagePrices() -> impl IntoView {
    let toasts = use_toasts();
    let refresh = RwSignal::new(0u32);
    let prices = src(LocalResource::new(move || {
        refresh.track();
        crate::api::get::<PricesResponse>("/api/usage/prices")
    }));
    // The Keys tab's count, once.
    let keys = src(LocalResource::new(|| {
        crate::api::get::<KeysResponse>("/api/usage/keys")
    }));
    // An upstream-model sheet is keyed `<upstream id>:<model>`; the band
    // says the upstream's name.
    let upstreams = LocalResource::new(|| crate::api::get::<UpstreamsResponse>("/api/upstreams"));
    let names = Memo::new(move |_| {
        upstreams
            .get()
            .and_then(|r| r.ok())
            .map(|r| {
                r.upstreams
                    .iter()
                    .map(|u| (u.id.to_string(), u.name.clone()))
                    .collect::<HashMap<String, String>>()
            })
            .unwrap_or_default()
    });

    let query = crate::url_state::use_query_signal("q");
    let source = crate::url_state::use_query_signal("source");
    let sort = crate::url_state::use_query_signal("sort");

    // The editor.
    let open = RwSignal::new(false);
    let scope_kind = RwSignal::new("alias".to_string());
    let scope_key = RwSignal::new(String::new());
    let p_in = RwSignal::new(String::new());
    let p_out = RwSignal::new(String::new());
    let p_cr = RwSignal::new(String::new());
    let p_cw = RwSignal::new(String::new());
    let edit_id = RwSignal::new(0i64);
    // The editor shows prices as typed numbers: `fmt::price`, never the
    // float noise the sheet stores (0.7999999999999999).
    let num = |v: Option<f64>| v.map(crate::fmt::price).unwrap_or_default();
    let load = move |r: Option<PriceRowView>, key: String| {
        match r {
            Some(r) => {
                edit_id.set(r.id);
                scope_kind.set(r.scope_kind);
                scope_key.set(r.scope_key);
                p_in.set(num(r.price_in));
                p_out.set(num(r.price_out));
                p_cr.set(num(r.price_cache_read));
                p_cw.set(num(r.price_cache_write));
            }
            None => {
                edit_id.set(0);
                scope_kind.set("alias".into());
                scope_key.set(key);
                p_in.set(String::new());
                p_out.set(String::new());
                p_cr.set(String::new());
                p_cw.set(String::new());
            }
        }
        open.set(true);
    };
    let edit = Callback::new(move |id: i64| {
        let row = prices.data.with_untracked(|d| {
            d.as_ref()
                .and_then(|p| p.prices.iter().find(|r| r.id == id).cloned())
        });
        if let Some(r) = row {
            load(Some(r), String::new());
        }
    });
    let price_it = Callback::new(move |name: String| load(None, name));
    let delete = Callback::new(move |id: i64| {
        spawn_local(async move {
            match crate::api::post::<Value, _>("/api/op/price_delete", &json!({ "id": id })).await {
                Ok(_) => {
                    toasts.ok("Price removed");
                    refresh.update(|v| *v = v.wrapping_add(1));
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    });
    // Catalog sync (usage-analytics §2.2). The only way to populate the price
    // table without typing every row by hand, and until now it existed solely
    // as an MCP tool — nothing in the dashboard could reach it, so a fresh
    // install's spend total stayed at zero with no way to find out why.
    let syncing = RwSignal::new(false);
    let sync = move || {
        if syncing.get_untracked() {
            return;
        }
        syncing.set(true);
        spawn_local(async move {
            match crate::api::post::<Value, _>("/api/op/prices_sync", &json!({})).await {
                Ok(v) => {
                    let rows = v.get("rows_written").and_then(Value::as_i64).unwrap_or(0);
                    let unpriced = v
                        .get("unpriced_models")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    // An upstream whose catalog fetch failed is named rather
                    // than folded into the count: "0 rows" and "could not ask"
                    // are different answers.
                    let failed: Vec<String> = v
                        .get("upstreams")
                        .and_then(Value::as_array)
                        .map(|us| {
                            us.iter()
                                .filter(|u| u.get("error").is_some_and(|e| !e.is_null()))
                                .map(|u| {
                                    u.get("upstream")
                                        .and_then(Value::as_str)
                                        .unwrap_or("upstream")
                                        .to_string()
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut msg = format!(
                        "{} price row(s) written · {unpriced} model(s) advertise no price",
                        grouped(rows.max(0) as u64),
                    );
                    if !failed.is_empty() {
                        msg.push_str(&format!(" · could not ask: {}", failed.join(", ")));
                    }
                    toasts.ok(msg);
                    syncing.set(false);
                    refresh.update(|v| *v = v.wrapping_add(1));
                }
                Err(e) => {
                    toasts.err(e.to_string());
                    syncing.set(false);
                }
            }
        });
    };
    let save = move |_| {
        // Empty clears that price; anything else has to be one. Text that is
        // not a number used to go out as null — clearing the price — and
        // toast "Price saved".
        let parsed = [
            ("input", p_in),
            ("output", p_out),
            ("cache read", p_cr),
            ("cache write", p_cw),
        ]
        .map(|(label, sig)| price_field(label, &sig.get_untracked()));
        if let Some(Err(e)) = parsed.iter().find(|r| r.is_err()) {
            toasts.err(e.clone());
            return;
        }
        let [p_in_v, p_out_v, p_cr_v, p_cw_v] = parsed.map(Result::unwrap_or_default);
        let body = json!({
            "id": edit_id.get_untracked(),
            "scope_kind": scope_kind.get_untracked(),
            "scope_key": scope_key.get_untracked(),
            "unit": "per_mtok",
            "price_in": p_in_v,
            "price_out": p_out_v,
            "price_cache_read": p_cr_v,
            "price_cache_write": p_cw_v,
            "source": "manual",
        });
        spawn_local(async move {
            match crate::api::post::<Value, _>("/api/op/price_set", &body).await {
                Ok(_) => {
                    toasts.ok("Price saved");
                    open.set(false);
                    refresh.update(|v| *v = v.wrapping_add(1));
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let cur = Memo::new(move |_| {
        prices
            .data
            .with(|p| p.as_ref().map(|p| p.currency.clone()).unwrap_or_default())
    });
    let sheet = Memo::new(move |_| {
        let Some(p) = prices.data.get() else {
            return PriceSheet::default();
        };
        PriceSheet::build(&p, &names.get(), &query.get(), &source.get(), &sort.get())
    });
    let facets = FacetSet {
        items: Signal::derive(move || {
            sheet.with(|s| {
                s.sources
                    .iter()
                    .map(|(id, n)| Facet {
                        id: id.clone(),
                        label: id.clone(),
                        count: *n,
                    })
                    .collect()
            })
        }),
        active: source,
    };
    let sort_th = move |label: &'static str, key: &'static str, class: &'static str| {
        let state = move || {
            let s = sort.get();
            let (k, desc) = match s.strip_prefix('-') {
                Some(k) => (k.to_string(), true),
                None => (s.clone(), false),
            };
            let k = if k.is_empty() { "model".to_string() } else { k };
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
                    on:click=move |_| {
                        let next = match state() {
                            // The default (model A→Z) is the absent parameter.
                            Some(false) => format!("-{key}"),
                            Some(true) if key != "model" => key.to_string(),
                            Some(true) => String::new(),
                            None if key == "model" => String::new(),
                            None => key.to_string(),
                        };
                        sort.set(next);
                    }
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

    let render_group = move |g: PriceGroup| {
        let open = crate::prefs::persisted_bool(&format!("open.usage.prices.{}", g.id), true);
        let narrowed = Signal::derive(move || !query.with(String::is_empty));
        let rows = StoredValue::new(g.rows);
        let unpriced = StoredValue::new(g.unpriced);
        let meta = g.meta;
        view! {
            <GroupRow
                colspan=PRICE_COLS
                label=g.label
                count=Signal::stored(g.count)
                open=open
                meta=move || view! { <span title=meta.clone()>{meta.clone()}</span> }
            />
            <Show when=move || open.get() || narrowed.get()>
                <For each=move || unpriced.get_value() key=|u| u.clone() let:u>
                    <UnpricedRow u=u price=price_it/>
                </For>
                <For each=move || rows.get_value() key=|r| r.clone() let:r>
                    <PriceRow r=r edit=edit delete=delete/>
                </For>
            </Show>
        }
    };

    view! {
        <PageFrame
            title="Usage"
            sub="what a token costs, per alias and per upstream model"
            mode=PageMode::Fill
            head_extra=move || view! { <UsageTabs keys=keys prices=prices/> }
            actions=move || {
                view! {
                    <button
                        class="btn ghost"
                        disabled=move || syncing.get()
                        title="re-read every upstream's catalog and write the prices it advertises"
                        on:click=move |_| sync()
                    >
                        {move || if syncing.get() { "Syncing…" } else { "Sync prices" }}
                    </button>
                    <button class="btn" on:click=move |_| load(None, String::new())>
                        "Add price"
                    </button>
                }
            }
            toolbar=move || {
                view! {
                    <FilterBar
                        query=query
                        placeholder="Filter by model or upstream"
                        shown=Signal::derive(move || sheet.with(|s| s.shown))
                        total=Signal::derive(move || sheet.with(|s| s.total))
                        noun="prices"
                        facets=facets
                    />
                }
            }
        >
            <Explain
                summary="Catalog prices come from each upstream's own listing; a manual row always wins."
                persist="usage.explain.prices"
            >
                "An upstream that publishes no prices — Gemini, for one — needs a manual row for "
                "each model it serves. Leave a field empty and it stays unknown rather than "
                "becoming a zero. Sync prices re-reads every catalog; it never overwrites a "
                "manual row."
            </Explain>
            <CardErr err=prices.err/>
            <div class="fill-pane card pad0" class:stale=move || prices.stale()>
                <table class="data price-table">
                    <thead>
                        <tr>
                            {sort_th("Model", "model", "")}
                            {sort_th("In", "in", "num-h")}
                            {sort_th("Out", "out", "num-h")}
                            {sort_th("Cache read", "cr", "num-h col-p2")}
                            {sort_th("Cache write", "cw", "num-h col-p2")}
                            {sort_th("Source", "source", "col-p3")}
                            <th></th>
                        </tr>
                    </thead>
                    <tbody>
                        <Show when=move || prices.data.with(Option::is_none) && prices.err.with(Option::is_none)>
                            <tr>
                                <td colspan=PRICE_COLS class="dim">"Loading…"</td>
                            </tr>
                        </Show>
                        <For each=move || sheet.get().groups key=|g| g.clone() let:g>
                            {render_group(g)}
                        </For>
                        <Show when=move || sheet.with(|s| s.groups.is_empty()) && prices.data.with(Option::is_some)>
                            <tr>
                                <td colspan=PRICE_COLS class="empty">
                                    {move || {
                                        if sheet.with(|s| s.total == 0) {
                                            "No price sheets yet — Sync prices reads them from the upstreams' catalogs, Add price writes one by hand."
                                        } else {
                                            "No price matches."
                                        }
                                    }}
                                </td>
                            </tr>
                        </Show>
                    </tbody>
                </table>
            </div>
            <Modal open=open title="Price sheet" guard=true>
                <div class="form">
                    <div class="row">
                        <span class="lbl">"scope"</span>
                        <Select
                            value=scope_kind
                            options=Signal::derive(|| {
                                vec![
                                    ("alias".to_string(), "alias".to_string()),
                                    ("upstream_model".into(), "upstream model".into()),
                                ]
                            })
                        />
                        <input
                            class="input mono"
                            style="flex:1"
                            placeholder="claude-opus-5"
                            prop:value=move || scope_key.get()
                            on:input=move |ev| scope_key.set(event_target_value(&ev))
                        />
                    </div>
                    <div class="field-grid" style="--field-min:120px; margin-top:12px">
                        <PriceField label="in" value=p_in/>
                        <PriceField label="out" value=p_out/>
                        <PriceField label="cache read" value=p_cr/>
                        <PriceField label="cache write" value=p_cw/>
                    </div>
                    <div class="mini-note dim">
                        {move || format!("Per Mtok, in {}. ", cur.get())}
                        "A manual row always wins over a catalog row for the same scope; leave a field empty and it stays unknown rather than becoming a zero."
                    </div>
                    <ModalFooter>
                        <button class="btn ghost" on:click=move |_| open.set(false)>
                            "Cancel"
                        </button>
                        <button class="btn primary" on:click=save>
                            "Save"
                        </button>
                    </ModalFooter>
                </div>
            </Modal>
        </PageFrame>
    }
}

const PRICE_COLS: u32 = 7;

/// One price box as the sheet stores it, per Mtok: empty is no price (null),
/// a decimal comma is a point, anything else must be a finite number ≥ 0.
fn price_field(label: &str, text: &str) -> Result<Option<f64>, String> {
    let t = text.trim().replace(',', ".");
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(Some)
        .ok_or_else(|| {
            format!(
                "{label} price: \u{201c}{}\u{201d} is not a price",
                text.trim()
            )
        })
}

#[component]
fn PriceField(label: &'static str, value: RwSignal<String>) -> impl IntoView {
    view! {
        <label class="field">
            <span class="lbl">{label}</span>
            <input
                class="input mono"
                inputmode="decimal"
                prop:value=move || value.get()
                on:input=move |ev| value.set(event_target_value(&ev))
            />
        </label>
    }
}

/// One price as the table prints it: numbers through `fmt::price`, the
/// scope key without the upstream id its band already names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PriceLine {
    id: i64,
    model: String,
    title: String,
    price_in: String,
    price_out: String,
    cache_read: String,
    cache_write: String,
    source: String,
}

/// A model no sheet covers, as its pinned row prints it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct UnpricedLine {
    name: String,
    requests: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PriceGroup {
    /// Stable id for the persisted fold state.
    id: String,
    label: String,
    /// "12 of 371" while filtered.
    count: String,
    meta: String,
    unpriced: Vec<UnpricedLine>,
    rows: Vec<PriceLine>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct PriceSheet {
    groups: Vec<PriceGroup>,
    /// Price sheets matching the filters, and all of them.
    shown: usize,
    total: usize,
    /// Source facet counts, over everything the query leaves.
    sources: Vec<(String, usize)>,
}

impl PriceSheet {
    fn build(
        p: &PricesResponse,
        names: &HashMap<String, String>,
        query: &str,
        source: &str,
        sort: &str,
    ) -> PriceSheet {
        let words = crate::widgets::filter_words(query);
        let fmt = |v: Option<f64>| v.map(crate::fmt::price).unwrap_or_else(|| "—".into());
        // Which band a sheet sits in, and its name there.
        let band = |r: &PriceRowView| -> (String, String, String) {
            if r.scope_kind == "upstream_model" {
                if let Some((id, model)) = r.scope_key.split_once(':') {
                    let name = names
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| format!("upstream {id}"));
                    return (format!("up.{name}"), name, model.to_string());
                }
            }
            if r.scope_kind == "alias" {
                return ("alias".into(), "Aliases".into(), r.scope_key.clone());
            }
            (
                format!("kind.{}", r.scope_kind),
                r.scope_kind.clone(),
                r.scope_key.clone(),
            )
        };
        let matches = |hay: &[&str]| {
            words
                .iter()
                .all(|w| hay.iter().any(|h| crate::widgets::matches_word(h, w)))
        };

        let mut sources: std::collections::BTreeMap<String, usize> = Default::default();
        let mut by: std::collections::BTreeMap<(u8, String), (String, Vec<&PriceRowView>, usize)> =
            Default::default();
        let mut shown = 0usize;
        for r in &p.prices {
            let (id, label, model) = band(r);
            let rank = if id == "alias" { 1 } else { 2 };
            let entry = by
                .entry((rank, label.clone()))
                .or_insert_with(|| (id.clone(), Vec::new(), 0));
            entry.2 += 1;
            if !matches(&[&model, &label]) {
                continue;
            }
            *sources.entry(r.source.clone()).or_default() += 1;
            if !source.is_empty() && r.source != source {
                continue;
            }
            shown += 1;
            entry.1.push(r);
        }

        let (key, desc) = match sort.strip_prefix('-') {
            Some(k) => (k, true),
            None => (sort, false),
        };
        let num_key = |r: &PriceRowView| match key {
            "in" => r.price_in,
            "out" => r.price_out,
            "cr" => r.price_cache_read,
            "cw" => r.price_cache_write,
            _ => None,
        };
        let mut groups = Vec::new();

        // Pinned first: spend no total can include, waiting on the owner.
        let unpriced: Vec<UnpricedLine> = p
            .unpriced_models
            .iter()
            .filter(|u| matches(&[&u.name, "unpriced"]))
            .map(|u| UnpricedLine {
                name: u.name.clone(),
                requests: u.requests,
            })
            .collect();
        if !unpriced.is_empty() && source.is_empty() {
            let reqs: i64 = unpriced.iter().map(|u| u.requests.max(0)).sum();
            groups.push(PriceGroup {
                id: "unpriced".into(),
                label: "Unpriced".into(),
                count: crate::fmt::of(unpriced.len(), p.unpriced_models.len()),
                meta: format!(
                    "{} req so far · spend no total can include — price each below",
                    grouped(reqs as u64)
                ),
                unpriced,
                rows: Vec::new(),
            });
        }

        for ((_, label), (id, mut rows, total)) in by {
            if rows.is_empty() {
                continue;
            }
            rows.sort_by(|a, b| {
                use std::cmp::Ordering::*;
                let o = match key {
                    "in" | "out" | "cr" | "cw" => match (num_key(a), num_key(b)) {
                        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Equal),
                        // Unknown sorts last either way: no price is not free.
                        (Some(_), None) => return Less,
                        (None, Some(_)) => return Greater,
                        (None, None) => Equal,
                    },
                    "source" => a.source.cmp(&b.source),
                    _ => Equal,
                };
                let o = if desc { o.reverse() } else { o };
                o.then_with(|| {
                    let o = band(a).2.to_lowercase().cmp(&band(b).2.to_lowercase());
                    if desc && !matches!(key, "in" | "out" | "cr" | "cw" | "source") {
                        o.reverse()
                    } else {
                        o
                    }
                })
            });
            // The kind and unit every row of the band shares go in its head;
            // a band that mixes them says so, and each row's title has its own.
            let kinds: std::collections::BTreeSet<&str> =
                rows.iter().map(|r| r.scope_kind.as_str()).collect();
            let units: std::collections::BTreeSet<&str> =
                rows.iter().map(|r| r.unit.as_str()).collect();
            let kind = match kinds.iter().next() {
                Some(k) if kinds.len() == 1 => k.replace('_', " "),
                _ => "mixed kinds".into(),
            };
            let unit = match units.iter().next() {
                Some(&"per_mtok") if units.len() == 1 => format!("{} per Mtok", p.currency),
                Some(u) if units.len() == 1 => u.to_string(),
                _ => "mixed units".into(),
            };
            let lines = rows
                .iter()
                .map(|r| {
                    let model = band(r).2;
                    PriceLine {
                        id: r.id,
                        title: format!(
                            "{} · {} · {} · {} · updated {}",
                            r.scope_key, r.scope_kind, r.unit, r.source, r.updated_at
                        ),
                        model,
                        price_in: fmt(r.price_in),
                        price_out: fmt(r.price_out),
                        cache_read: fmt(r.price_cache_read),
                        cache_write: fmt(r.price_cache_write),
                        source: r.source.clone(),
                    }
                })
                .collect::<Vec<_>>();
            groups.push(PriceGroup {
                id: id.clone(),
                label,
                count: crate::fmt::of(lines.len(), total),
                meta: format!("{kind} · {unit}"),
                unpriced: Vec::new(),
                rows: lines,
            });
        }
        PriceSheet {
            groups,
            shown,
            total: p.prices.len(),
            sources: sources.into_iter().collect(),
        }
    }
}

#[component]
fn PriceRow(r: PriceLine, edit: Callback<i64>, delete: Callback<i64>) -> impl IntoView {
    let id = r.id;
    view! {
        <tr>
            <td class="clip mono-sm" title=r.title>{r.model}</td>
            <td class="num">{r.price_in}</td>
            <td class="num">{r.price_out}</td>
            <td class="num col-p2">{r.cache_read}</td>
            <td class="num col-p2">{r.cache_write}</td>
            <td class="dim col-p3">{r.source}</td>
            <td class="actions">
                <div class="row-acts">
                    <button class="btn ghost sm" on:click=move |_| edit.run(id)>
                        "Edit"
                    </button>
                    <ConfirmButton
                        label="✕"
                        confirm="Remove this price?"
                        title="Remove this price"
                        on_confirm=Callback::new(move |()| delete.run(id))
                    />
                </div>
            </td>
        </tr>
    }
}

/// A model no price sheet covers: its spend is unknown, never zero, and the
/// row's one action prices it.
#[component]
fn UnpricedRow(u: UnpricedLine, price: Callback<String>) -> impl IntoView {
    let name = u.name.clone();
    view! {
        <tr class="unpriced-row">
            <td class="clip mono-sm" title=format!("{} — no price sheet covers it", u.name)>
                {u.name.clone()}
            </td>
            <td class="num" colspan="4">
                <span class="chip warn">
                    {format!("{} req unpriced", grouped(u.requests.max(0) as u64))}
                </span>
            </td>
            <td class="col-p3"></td>
            <td class="actions">
                <div class="row-acts">
                    <button
                        class="btn sm"
                        title="Write a price for this model"
                        on:click=move |_| price.run(name.clone())
                    >
                        "Price"
                    </button>
                </div>
            </td>
        </tr>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_price_box_is_empty_or_a_price_never_silently_null() {
        assert_eq!(price_field("input", ""), Ok(None));
        assert_eq!(price_field("input", " 0,8 "), Ok(Some(0.8)));
        assert_eq!(price_field("input", "15"), Ok(Some(15.0)));
        assert!(price_field("output", "1.2.3")
            .unwrap_err()
            .contains("output"));
        assert!(price_field("input", "-1").is_err());
        assert!(price_field("input", "inf").is_err());
    }
}
