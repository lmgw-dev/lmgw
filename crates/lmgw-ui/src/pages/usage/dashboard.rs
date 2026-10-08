use std::time::Duration;

use leptos::prelude::*;
use lmgw_api_types::{
    KeysResponse, PricesResponse, RequestRow, UsageCell, UsageErrorsResponse, UsageHeatResponse,
    UsageLocalResponse, UsageSeriesResponse, UsageTopResponse,
};

use crate::charts::{self};
use crate::fmt::grouped;
use crate::live::use_live;
use crate::widgets::{NavTab, PageFrame, Select, SubNav, Tone};

use super::*;

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

#[component]
pub fn Usage() -> impl IntoView {
    let live = use_live();
    // The tooltip itself is App-level (outside the size-contained pane);
    // leaving the page must not leave a tip standing over the next one.
    let tips = charts::use_tips();
    on_cleanup(move || tips.hide());

    // One filter row above everything it scopes — never per-card filters.
    // It lives in the address, like Traffic's: a view left for Keys or
    // Prices comes back as it was (review code:U1), and it can be bookmarked.
    // Absent is the default, so a bare /usage stays bare.
    let range_q = crate::url_state::use_query_signal("range"); // "" = 30d
    let range = Memo::new(move |_| {
        range_q.with(|r| {
            if r.is_empty() {
                "30d".to_string()
            } else {
                r.clone()
            }
        })
    });
    let bucket = crate::url_state::use_query_signal("bucket"); // "" = follow the range
    let group_q = crate::url_state::use_query_signal("group"); // "" = by alias
    let group_by = Signal::derive(move || {
        group_q.with(|g| {
            if g.is_empty() {
                "alias".to_string()
            } else {
                g.clone()
            }
        })
    });
    // The Select holds a value of its own; "alias" is the absent default.
    let group_sel = RwSignal::new(group_by.get_untracked());
    Effect::new(move |_| {
        let g = group_by.get();
        if group_sel.get_untracked() != g {
            group_sel.set(g);
        }
    });
    Effect::new(move |_| {
        let v = group_sel.get();
        let want = if v == "alias" { String::new() } else { v };
        if group_q.get_untracked() != want {
            group_q.set(want);
        }
    });
    let class = crate::url_state::use_query_signal("class");
    let key_id = crate::url_state::use_query_signal("key_id");
    // Bumped by the live tick (and by every op) to re-ask the server.
    let refresh = RwSignal::new(0u32);
    // Entry animation: once per mount, never per re-render.
    let entry = RwSignal::new(!charts::reduced_motion());
    set_timeout(move || entry.set(false), Duration::from_millis(1600));

    // The window re-derives on every refetch, so a page left open overnight
    // grows new buckets instead of freezing at the hour it was loaded.
    let win = Memo::new(move |_| {
        refresh.track();
        window_for(&range.get(), &bucket.get())
    });

    let series = src(LocalResource::new(move || {
        refresh.track();
        crate::api::get::<UsageSeriesResponse>(series_url(
            &win.get(),
            &group_by.get(),
            &class.get(),
            &key_id.get(),
            6,
        ))
    }));
    // The local-performance panel is always per alias: "tok/s per model" is
    // the question, whatever the page is grouped by.
    let by_alias = src(LocalResource::new(move || {
        refresh.track();
        crate::api::get::<UsageSeriesResponse>(series_url(
            &win.get(),
            "alias",
            &class.get(),
            &key_id.get(),
            6,
        ))
    }));
    let top = src(LocalResource::new(move || {
        refresh.track();
        crate::api::get::<UsageTopResponse>(scalar_url(
            "top",
            &win.get(),
            &class.get(),
            &key_id.get(),
            // Every alias: the card shows the first ten and says how many
            // more there are, so a total on it is the window's total.
            "&dim=alias",
        ))
    }));
    let heat = src(LocalResource::new(move || {
        refresh.track();
        crate::api::get::<UsageHeatResponse>(scalar_url(
            "heat",
            &win.get(),
            &class.get(),
            &key_id.get(),
            "",
        ))
    }));
    let local = src(LocalResource::new(move || {
        refresh.track();
        crate::api::get::<UsageLocalResponse>(scalar_url(
            "local",
            &win.get(),
            &class.get(),
            &key_id.get(),
            "",
        ))
    }));
    // Errors read the raw rows rather than the rollups, so this is its own
    // endpoint with its own (retention-bounded) range — the card says so.
    let errors = src(LocalResource::new(move || {
        refresh.track();
        let w = win.get();
        let bucket = format!("&bucket={}", w.bucket);
        crate::api::get::<UsageErrorsResponse>(scalar_url(
            "errors",
            &w,
            &class.get(),
            &key_id.get(),
            &bucket,
        ))
    }));
    let keys = src(LocalResource::new(move || {
        refresh.track();
        crate::api::get::<KeysResponse>("/api/usage/keys")
    }));
    // Only the Prices tab's count reads it on this tab: fetched once, not
    // on every live tick.
    let prices = src(LocalResource::new(|| {
        crate::api::get::<PricesResponse>("/api/usage/prices")
    }));

    // ---- live tick -------------------------------------------------------
    // The current bucket grows in place as `Request` frames land; everything
    // older is immutable and is never refetched for its own sake. Cost is not
    // on the frame, so the optimistic bump moves requests and tokens and the
    // debounced refetch below is what makes the money authoritative.
    let dirty = RwSignal::new(false);
    Effect::new(move |_| {
        let Some(row) = live.request.get() else {
            return;
        };
        dirty.set(true);
        // A filtered view must not grow on a row the filter excludes. Both
        // filters are checkable against the frame now — `key_id` matches on the
        // id rather than the name, so renaming a key does not silently break
        // the match.
        let cf = class.get_untracked();
        if !cf.is_empty() && row.class != cf {
            return;
        }
        let kf = key_id.get_untracked();
        if !kf.is_empty() && kf.parse::<i64>().ok() != row.key_id {
            return;
        }
        let gb = group_by.get_untracked();
        series.data.update(|opt| {
            if let Some(r) = opt.as_mut() {
                bump_current(r, &row, &gb);
            }
        });
    });
    // The optimistic bump above is authoritative for the current bucket; this
    // is the reconcile that picks up a new bucket rolling over, the other
    // endpoints, and anything the frame does not carry.
    let tick = set_interval_with_handle(
        move || {
            if dirty.get_untracked() {
                dirty.set(false);
                refresh.update(|v| *v = v.wrapping_add(1));
            }
        },
        Duration::from_secs(15),
    )
    .ok();
    on_cleanup(move || {
        if let Some(h) = tick {
            h.clear();
        }
    });

    let key_options = Signal::derive(move || {
        let mut v = vec![(String::new(), "all keys".to_string())];
        if let Some(k) = keys.data.get() {
            v.extend(k.keys.iter().map(|k| (k.id.to_string(), k.name.clone())));
        }
        v
    });
    let sub = Memo::new(move |_| win.get().label);
    // From when this window's quantities were recorded (billable units §5.5):
    // "since 15 Jan 2026" while the window starts before the counting did.
    let since = Signal::derive(move || {
        let from = win.get().from;
        series.data.with(|d| {
            d.as_ref()
                .and_then(|r| since_note(&from, r.units_since.as_deref()))
        })
    });
    // The tiles' "vs previous window" compares against the window of the
    // same length before this one: no delta while *that* window starts
    // before the recording, or it compares against hours nobody counted.
    let prev_unrecorded = Signal::derive(move || {
        let w = win.get();
        let Some(prev_from) = previous_from(&w.from, &w.to) else {
            return false;
        };
        series.data.with(|d| {
            d.as_ref()
                .is_some_and(|r| starts_before(&prev_from, r.units_since.as_deref()))
        })
    });

    // Colour slots, page-scoped (§6.3): a named series keeps its colour for
    // as long as it is on screen, whatever the filters do to the others. The
    // aliases' pool is fed by every alias-grouped response the page draws —
    // the tiles' per-alias series always, the main series when it groups by
    // alias too — so an alias wears one colour in every card.
    let slots = charts::SlotRegistry::new();
    let alias_slots = Memo::new(move |_| {
        let mut keys = by_alias.data.with(|d| {
            d.as_ref()
                .map(|r| named_keys(&r.series))
                .unwrap_or_default()
        });
        if group_by.get() == "alias" {
            for k in series.data.with(|d| {
                d.as_ref()
                    .map(|r| named_keys(&r.series))
                    .unwrap_or_default()
            }) {
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
        }
        slots.assign("alias", &keys)
    });
    let series_slots = Memo::new(move |_| {
        let gb = group_by.get();
        if gb == "alias" {
            return alias_slots.get();
        }
        let keys = series.data.with(|d| {
            d.as_ref()
                .map(|r| named_keys(&r.series))
                .unwrap_or_default()
        });
        slots.assign(&gb, &keys)
    });

    let range_btns = move || {
        ["24h", "7d", "30d", "12mo"]
            .into_iter()
            .map(|r| {
                view! {
                    <button
                        class="seg-btn"
                        class:active=move || range.get() == r
                        on:click=move |_| {
                            range_q.set(if r == "30d" { String::new() } else { r.to_string() });
                            bucket.set(String::new());
                        }
                    >
                        {r}
                    </button>
                }
            })
            .collect_view()
    };

    // The filter row is the page toolbar: it scopes everything below it, so
    // it stays on screen while the cards scroll. One line from an 1,100 px
    // pane up: the labels are the selects' own words, not extra columns.
    let filters = move || {
        view! {
            <div class="filters">
                <div class="seg">{range_btns()}</div>
                <Select
                    value=bucket
                    options=Signal::derive(move || {
                        vec![
                            (String::new(), format!("{} buckets (auto)", win.get().bucket)),
                            ("hour".into(), "hour buckets".into()),
                            ("day".into(), "day buckets".into()),
                            ("week".into(), "week buckets".into()),
                            ("month".into(), "month buckets".into()),
                        ]
                    })
                />
                <Select
                    value=group_sel
                    options=Signal::derive(|| {
                        vec![
                            ("alias".to_string(), "by alias".to_string()),
                            ("key".into(), "by key".into()),
                            ("upstream".into(), "by upstream".into()),
                            ("class".into(), "by class".into()),
                            ("none".into(), "not grouped".into()),
                        ]
                    })
                />
                <Select
                    value=class
                    options=Signal::derive(|| {
                        vec![
                            (String::new(), "all classes".to_string()),
                            ("chat".into(), "chat".into()),
                            ("aux".into(), "aux (embed/rerank)".into()),
                            ("audio".into(), "audio".into()),
                            ("image".into(), "image".into()),
                            ("tool".into(), "tool".into()),
                        ]
                    })
                />
                <Select value=key_id options=key_options/>
                <span class="spacer"></span>
                <span class="chip live" title="the current bucket grows as requests land">
                    <span class="dot"></span>
                    "live"
                </span>
                // The rollup rows for exactly what is on screen. It streams to
                // the browser's own download — nothing is uploaded anywhere.
                // `download` keeps the router's hands off it: a same-origin
                // link is otherwise an in-app route, and this one has none
                // ("Not found"). The server names the file by its window.
                <a
                    class="btn ghost"
                    download=""
                    title="Download the rollup rows behind this view as CSV. It streams to this browser; nothing is uploaded anywhere."
                    href=move || {
                        format!(
                            "/api/usage/export.csv{}",
                            series_url(&win.get(), &group_by.get(), &class.get(), &key_id.get(), 6)
                                .split_once('?')
                                .map(|(_, q)| format!("?{q}"))
                                .unwrap_or_default(),
                        )
                    }
                >
                    "Export CSV"
                </a>
            </div>
        }
    };

    view! {
        <PageFrame
            title="Usage"
            sub=move || sub.get()
            class="usage"
            head_extra=move || view! { <UsageTabs keys=keys prices=prices/> }
            toolbar=filters
        >
            <TileRow
                series=series
                by_alias=by_alias
                local=local
                since=since
                prev_unrecorded=prev_unrecorded
            />

            // Rows of related cards: spend and its budget; the three time
            // series; where it went and when; local share beside the failures.
            // Card spans are in app.css (by the pane's width).
            <div class="grid">
                <SpendCard series=series group_by=group_by slots=series_slots entry=entry tips=tips/>
                <BudgetCard series=series keys=keys entry=entry tips=tips/>
                <TokensCard series=series entry=entry tips=tips/>
                <LatencyCard series=series entry=entry tips=tips/>
                <PerfCard by_alias=by_alias local=local slots=alias_slots entry=entry tips=tips/>
                <ShareCard
                    top=top
                    class=class
                    key_id=key_id
                    slots=alias_slots
                    group_by=group_by
                    since=since
                    entry=entry
                    tips=tips
                />
                <HeatCard heat=heat tips=tips/>
                <LocalCard local=local/>
                <ErrorsCard errors=errors class=class key_id=key_id entry=entry tips=tips/>
            </div>
        </PageFrame>
    }
}

/// Grow the current bucket for one finished request. Only ever the last
/// bucket: everything older is immutable.
fn bump_current(r: &mut UsageSeriesResponse, row: &RequestRow, group_by: &str) {
    let Some(bucket) = r.buckets.last().cloned() else {
        return;
    };
    // The series key is what the server grouped on: an alias or a class is
    // itself, a key and an upstream are ids (the frame names the upstream,
    // and the series label is that name).
    let key = match group_by {
        "alias" => row.requested_alias.clone(),
        "upstream" => {
            let name = row.upstream_name.clone().unwrap_or_default();
            r.series
                .iter()
                .find(|s| !is_other(s) && s.label == name)
                .map(|s| s.key.clone())
                .unwrap_or(name)
        }
        "key" => row.key_id.unwrap_or(0).to_string(),
        "class" => row.class.clone(),
        "none" => String::new(),
        _ => return,
    };
    // An entity the response never listed is inside the folded `Other` tail.
    let key = if group_by == "none" || r.series.iter().any(|s| s.key == key) {
        key
    } else {
        match r.series.iter().find(|s| is_other(s)) {
            Some(s) => s.key.clone(),
            None => return,
        }
    };
    // A quantity the row did not measure adds 0, as the rollup's does: the
    // totals are what was measured (billable units §5.3).
    let mut delta = UsageCell {
        requests: 1,
        tokens_in: row.prompt_tokens.unwrap_or(0),
        tokens_out: row.completion_tokens.unwrap_or(0),
        tokens_cached: row.cached_in_tokens.unwrap_or(0),
        tokens_cache_write: row.cache_write_tokens.unwrap_or(0),
        audio_in_ms: row.audio_in_ms.unwrap_or(0),
        chars_in: row.chars_in.unwrap_or(0),
        images_out: row.images_out.unwrap_or(0),
        ..Default::default()
    };
    // The frame carries the money now, so the current bucket grows with the
    // real number rather than catching up on the next refetch. `None` is
    // **unpriced, not free** — it lands in the remainder the spend figures are
    // obliged to state, never as a zero added to the total.
    match row.cost_micro {
        Some(c) => delta.cost_micro = c,
        None => {
            delta.cost_unknown_requests = 1;
            delta.cost_unknown_tokens = delta.tokens_in + delta.tokens_out;
            delta.cost_unknown_audio_in_ms = delta.audio_in_ms;
            delta.cost_unknown_chars_in = delta.chars_in;
            delta.cost_unknown_images_out = delta.images_out;
        }
    }
    if row.status >= 400 {
        delta.errors = 1;
    }
    if let Some(ms) = row.total_ms {
        delta.total_sum += ms;
        delta.total_count += 1;
    }
    if let Some(ms) = row.ttfb_ms {
        delta.ttfb_sum += ms;
        delta.ttfb_count += 1;
    }
    cell_add(&mut r.totals, &delta);
    if let Some(c) = r
        .cells
        .iter_mut()
        .find(|c| c.bucket == bucket && c.series == key)
    {
        cell_add(c, &delta);
    } else {
        let mut c = delta;
        c.bucket = bucket;
        c.series = key;
        r.cells.push(c);
    }
}

// ---------------------------------------------------------------------------
// UsageTabs lives here; Keys & budgets is keys.rs, Prices is prices.rs
// ---------------------------------------------------------------------------

/// The three Usage views, each its own route. Keys and prices are policy, not
/// a lens on the window: they ignore the filter row, so they do not sit under
/// it, and neither tab pays for the chart fetches.
#[component]
pub(super) fn UsageTabs(keys: Src<KeysResponse>, prices: Src<PricesResponse>) -> impl IntoView {
    let tabs = Signal::derive(move || {
        let charts = NavTab::new("Charts", "/usage").exact();
        let mut k = NavTab::new("Keys & budgets", "/usage/keys");
        if let Some(n) = keys.data.with(|d| d.as_ref().map(|d| d.keys.len())) {
            k = k.count(grouped(n as u64));
        }
        let mut p = NavTab::new("Prices", "/usage/prices");
        if let Some((n, unpriced)) = prices.data.with(|d| {
            d.as_ref()
                .map(|d| (d.prices.len(), d.unpriced_models.len()))
        }) {
            // An unpriced model is spend no total can include: waiting on
            // the owner, so it is amber.
            p = if unpriced > 0 {
                p.count(format!("{} · {unpriced} unpriced", grouped(n as u64)))
                    .tone(Tone::Attn)
            } else {
                p.count(grouped(n as u64))
            };
        }
        vec![charts, k, p]
    });
    view! { <SubNav tabs=tabs/> }
}
