//! Traffic — the unified request log: live tail on the SSE bus, filters,
//! cursor pagination, expandable per-row detail (log rows carry every stored
//! field, so no extra fetch). Two routes: `/traffic` (Requests) and
//! `/traffic/conversations`.
//!
//! Requests is a fill page: the filters and the table head stay put, the rows
//! are the one scroller. The live tail keeps the newest page of whatever the
//! filters select — a live row is matched against the same filters the server
//! applied, so a filtered view grows only with rows that belong in it.

use std::time::Duration;

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{KeysResponse, LogsResponse, RequestRow, UsageTopResponse, VramStatus};

use crate::fmt::{count_of, grouped, hue_for, human_bytes, log_time};
use crate::live::use_live;
use crate::url_state::use_query_signal;
use crate::widgets::{
    filter_words, matches_word, FilterBar, NavTab, PageFrame, PageMode, Popover, Select, SubNav,
};

/// The page sizes on offer. The size is also how many rows the live tail
/// keeps (PLAN §7.5), so it is said next to the tail, not buried in a menu.
const PAGE_SIZES: [&str; 4] = ["50", "100", "250", "1000"];

fn urlenc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

/// Accept what a hand-written or linked URL might plausibly carry.
fn truthy(v: &str) -> bool {
    matches!(v, "1" | "true" | "yes" | "on")
}

/// The Traffic tabs, each its own route.
#[component]
pub(super) fn TrafficTabs(#[prop(into)] conversations: Signal<Option<i64>>) -> impl IntoView {
    let tabs = Signal::derive(move || {
        let mut c = NavTab::new("Conversations", "/traffic/conversations");
        if let Some(n) = conversations.get() {
            c = c.count(grouped(n.max(0) as u64));
        }
        vec![NavTab::new("Requests", "/traffic").exact(), c]
    });
    view! { <SubNav tabs=tabs/> }
}

/// The stored conversations' count, for the tab.
pub(super) fn conversations_count() -> Signal<Option<i64>> {
    let index =
        LocalResource::new(|| crate::api::get::<lmgw_api_types::ResponsesIndex>("/api/responses"));
    Signal::derive(move || index.get().and_then(|r| r.ok()).map(|i| i.total_chains))
}

/// Every filter the log is asked with, one canonical query string: it is both
/// what `/api/logs` receives and how two states are compared.
#[derive(Clone, Debug, Default, PartialEq)]
struct Filters {
    /// Exact — what a Usage chart links with.
    alias: String,
    /// A part of the alias, case-insensitive — what the box types.
    alias_q: String,
    upstream: String,
    upstream_q: String,
    class: String,
    key_id: String,
    error_kind: String,
    errors_only: bool,
}

impl Filters {
    /// `errors_only=true`, not `=1`: this string is also the query `/api/logs`
    /// receives, and older servers' `Option<bool>` rejected `1` with a 400.
    fn qs(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut put = |k: &str, v: &str| {
            if !v.trim().is_empty() {
                parts.push(format!("{k}={}", urlenc(v.trim())));
            }
        };
        put("alias", &self.alias);
        put("alias_q", &self.alias_q);
        put("upstream", &self.upstream);
        put("upstream_q", &self.upstream_q);
        put("class", &self.class);
        put("key_id", &self.key_id);
        put("error_kind", &self.error_kind);
        if self.errors_only {
            parts.push("errors_only=true".to_string());
        }
        parts.join("&")
    }

    /// Would the server have returned this row for these filters? The live
    /// tail asks before it prepends, so a filtered view never grows a row the
    /// filter excludes. Substrings compare the way SQLite's LIKE does:
    /// ASCII case folded.
    fn admits(&self, r: &RequestRow) -> bool {
        let has = |hay: &str, needle: &str| {
            let n = needle.trim();
            n.is_empty() || hay.to_ascii_lowercase().contains(&n.to_ascii_lowercase())
        };
        let up = r.upstream_name.as_deref().unwrap_or("");
        let exact = |want: &str, have: &str| want.trim().is_empty() || want.trim() == have;
        exact(&self.alias, &r.requested_alias)
            && has(&r.requested_alias, &self.alias_q)
            && (self.upstream.trim().is_empty() || self.upstream.trim() == up)
            && (self.upstream_q.trim().is_empty() || (!up.is_empty() && has(up, &self.upstream_q)))
            && exact(&self.class, &r.class)
            && (self.key_id.trim().is_empty() || self.key_id.trim().parse::<i64>().ok() == r.key_id)
            && (self.error_kind.trim().is_empty()
                || r.error_kind.as_deref() == Some(self.error_kind.trim()))
            && (!self.errors_only || r.status >= 400)
    }
}

/// A text box whose value reaches the URL (and the fetch) a beat after the
/// typing stops — one request per pause, not per keystroke. A URL that
/// changes underneath (Clear, a link, Back) is read back into the box.
fn debounced(param: RwSignal<String>) -> RwSignal<String> {
    let text = RwSignal::new(param.get_untracked());
    let gen = StoredValue::new(0u64);
    Effect::new(move |_| {
        let v = param.get();
        if text.get_untracked().trim() != v {
            text.set(v);
        }
    });
    Effect::new(move |prev: Option<()>| {
        let v = text.get();
        if prev.is_none() {
            return;
        }
        gen.update_value(|g| *g += 1);
        let mine = gen.get_value();
        set_timeout(
            move || {
                if gen.try_get_value() != Some(mine) {
                    return;
                }
                let v = v.trim().to_string();
                if param.get_untracked() != v {
                    param.set(v);
                }
            },
            Duration::from_millis(250),
        );
    });
    text
}

/// Requests (`/traffic`).
#[component]
pub fn Traffic() -> impl IntoView {
    let live = use_live();

    // Filters live in the URL, so a chart on Usage has somewhere to link to
    // and a filtered view is something the owner can bookmark or paste.
    let alias = use_query_signal("alias");
    let alias_q = use_query_signal("alias_q");
    let upstream = use_query_signal("upstream");
    let upstream_q = use_query_signal("upstream_q");
    let class = use_query_signal("class");
    let key_id = use_query_signal("key_id");
    let error_kind = use_query_signal("error_kind");
    let errors_only_raw = use_query_signal("errors_only");
    let alias_text = debounced(alias_q);
    let upstream_text = debounced(upstream_q);
    let errors_only = Signal::derive(move || truthy(&errors_only_raw.get()));

    let current = Memo::new(move |_| Filters {
        alias: alias.get(),
        alias_q: alias_q.get(),
        upstream: upstream.get(),
        upstream_q: upstream_q.get(),
        class: class.get(),
        key_id: key_id.get(),
        error_kind: error_kind.get(),
        errors_only: errors_only.get(),
    });

    let size_pref = crate::prefs::persisted_string("pagesize.traffic", "50");
    let page = Memo::new(move |_| {
        size_pref
            .get()
            .parse::<usize>()
            .ok()
            .filter(|n| PAGE_SIZES.contains(&n.to_string().as_str()))
            .unwrap_or(50)
    });

    let rows = RwSignal::new(Vec::<RequestRow>::new());
    let loading = RwSignal::new(true);
    let error = RwSignal::new(None::<String>);
    // The last page came back full: there may be older rows.
    let more = RwSignal::new(false);
    // Older pages were appended: the view is no longer the head, so the live
    // tail stops (it would push them off the end) and counts instead.
    let older = RwSignal::new(false);
    // A "Load older" is out: the page it asked for starts below the last row,
    // so a live row must not push that row off the end meanwhile (review
    // code:U3) — it is counted as missed instead, like while older rows show.
    let older_out = RwSignal::new(false);
    let paused = RwSignal::new(false);
    let missed = RwSignal::new(0usize);
    let reload = RwSignal::new(0u32);
    let gen = StoredValue::new(0u64);

    let fetch = move |before: Option<i64>| {
        let qs = current.get_untracked().qs();
        let n = page.get_untracked();
        let mut url = format!("/api/logs?limit={n}");
        if !qs.is_empty() {
            url.push('&');
            url.push_str(&qs);
        }
        if let Some(b) = before {
            url.push_str(&format!("&before_id={b}"));
        }
        gen.update_value(|g| *g += 1);
        let mine = gen.get_value();
        loading.set(true);
        older_out.set(before.is_some());
        spawn_local(async move {
            let res = crate::api::get::<LogsResponse>(url).await;
            // A newer fetch (the filters moved on) owns the table now.
            if gen.try_get_value() != Some(mine) {
                return;
            }
            loading.set(false);
            older_out.set(false);
            match res {
                Ok(resp) => {
                    error.set(None);
                    more.set(resp.logs.len() >= n);
                    if before.is_some() {
                        rows.update(|r| r.extend(resp.logs));
                        older.set(true);
                    } else {
                        rows.set(resp.logs);
                        older.set(false);
                        missed.set(0);
                    }
                }
                Err(e) => {
                    error.set(Some(e.to_string()));
                    // Rows that landed while the older page was out were
                    // counted, not shown: say so, and Resume brings them.
                    if before.is_some() && missed.get_untracked() > 0 {
                        older.set(true);
                    }
                }
            }
        });
    };
    // The head: on open, on every filter or page-size change, and on Resume.
    Effect::new(move |_| {
        current.track();
        page.track();
        reload.track();
        fetch(None);
    });
    let load_older = move |_| {
        if let Some(last) = rows.with_untracked(|r| r.last().map(|x| x.log_id)) {
            fetch(Some(last));
        }
    };
    let resume = move || {
        paused.set(false);
        if older.get_untracked() || missed.get_untracked() > 0 {
            reload.update(|n| *n = n.wrapping_add(1));
        }
    };

    // Live prepend: rows these filters select, while the view is the head.
    Effect::new(move |_| {
        let Some(row) = live.request.get() else {
            return;
        };
        if !current.with_untracked(|f| f.admits(&row)) {
            return;
        }
        if paused.get_untracked() || older.get_untracked() || older_out.get_untracked() {
            if rows.with_untracked(|r| r.iter().all(|x| x.log_id != row.log_id)) {
                missed.update(|n| *n += 1);
            }
            return;
        }
        let keep = page.get_untracked();
        rows.update(|r| {
            if r.iter().take(8).any(|x| x.log_id == row.log_id) {
                return; // duplicate frame (reconnect replay)
            }
            r.insert(0, row);
            r.truncate(keep);
        });
    });

    // Model and Upstream share the width the other columns leave by what
    // they hold, not by a fixed 45/25: whichever has the longer values gets
    // more, and neither drops below a quarter of the pair (review ux:U-5).
    let name_share = Memo::new(move |_| {
        rows.with(|r| {
            let widest = |f: &dyn Fn(&RequestRow) -> usize| r.iter().map(f).max().unwrap_or(0);
            let name = widest(&|x| {
                x.mcp_tool
                    .as_deref()
                    .filter(|_| x.ingress_proto == "mcp")
                    .unwrap_or(&x.requested_alias)
                    .chars()
                    .count()
                    + 3
            });
            let up = widest(&|x| {
                let u = x.upstream_name.as_deref().map_or(0, |u| u.chars().count());
                let m = x
                    .upstream_model
                    .as_deref()
                    .map_or(0, |m| m.chars().count() + 3);
                u + m
            });
            if name + up == 0 {
                0.5
            } else {
                (name as f64 / (name + up) as f64).clamp(0.25, 0.75)
            }
        })
    });
    let col_widths = move || {
        let share = name_share.get();
        format!(
            "--w-name:{:.1}%;--w-up:{:.1}%",
            70.0 * share,
            70.0 * (1.0 - share)
        )
    };

    let halted = Memo::new(move |_| paused.get() || older.get());
    let status = Signal::derive(move || {
        let n = rows.with(Vec::len);
        if loading.get() && n == 0 {
            return "Loading…".to_string();
        }
        // Short enough to stay on the filter row: the page size beside it
        // already says how long the tail is, this says what feeds it.
        let what = format!("Showing {} rows", grouped(n as u64));
        if halted.get() {
            let k = missed.get();
            if k > 0 {
                format!("{what} · paused ({} new)", grouped(k as u64))
            } else {
                format!("{what} · paused")
            }
        } else {
            format!("{what} · live tail keeps {}", grouped(page.get() as u64))
        }
    });

    // Key names for the picker. A key filter matches on the id — a name match
    // breaks the moment a key is renamed — so the id is what travels in the
    // URL and this is only the label for it.
    let keys = LocalResource::new(|| crate::api::get::<KeysResponse>("/api/usage/keys"));
    let key_options = Signal::derive(move || {
        let mut v = vec![(String::new(), "all keys".to_string())];
        if let Some(Ok(k)) = keys.get() {
            v.extend(k.keys.iter().map(|k| (k.id.to_string(), k.name.clone())));
        }
        // A URL can carry an id the list does not know (a deleted key still has
        // history). Show the raw id rather than silently falling back to "all".
        let cur = key_id.get();
        if !cur.is_empty() && !v.iter().any(|(val, _)| *val == cur) {
            v.push((cur.clone(), format!("key #{cur}")));
        }
        v
    });
    let class_options = Signal::derive(|| {
        vec![
            (String::new(), "all classes".to_string()),
            ("chat".into(), "chat".into()),
            ("aux".into(), "aux (embed/rerank)".into()),
            ("audio".into(), "audio".into()),
            ("image".into(), "image".into()),
            ("tool".into(), "tool".into()),
        ]
    });
    let size_options = Signal::derive(|| {
        PAGE_SIZES
            .iter()
            .map(|s| {
                let n: u64 = s.parse().unwrap_or(0);
                (s.to_string(), format!("{} rows", grouped(n)))
            })
            .collect::<Vec<_>>()
    });

    // Filters a link set that no control shows: each a chip with its remover.
    let chips = Signal::derive(move || {
        let mut v: Vec<(String, Callback<()>)> = Vec::new();
        let a = alias.get();
        if !a.is_empty() {
            v.push((
                format!("alias = {a}"),
                Callback::new(move |()| alias.set(String::new())),
            ));
        }
        let u = upstream.get();
        if !u.is_empty() {
            v.push((
                format!("upstream = {u}"),
                Callback::new(move |()| upstream.set(String::new())),
            ));
        }
        let k = error_kind.get();
        if !k.is_empty() {
            v.push((
                format!("kind: {k}"),
                Callback::new(move |()| error_kind.set(String::new())),
            ));
        }
        v
    });
    let own_active = Signal::derive(move || {
        !upstream_q.with(String::is_empty)
            || !upstream_text.with(|t| t.trim().is_empty())
            || !class.with(String::is_empty)
            || !key_id.with(String::is_empty)
            || errors_only.get()
    });
    let clear = Callback::new(move |()| {
        alias.set(String::new());
        alias_q.set(String::new());
        upstream.set(String::new());
        upstream_q.set(String::new());
        upstream_text.set(String::new());
        class.set(String::new());
        key_id.set(String::new());
        error_kind.set(String::new());
        errors_only_raw.set(String::new());
    });

    let vram = gpu_state();
    let gpu_open = RwSignal::new(false);

    let expanded = RwSignal::new(None::<i64>);
    let items = Memo::new(move |_| {
        rows.with(|r| {
            let mut out = Vec::with_capacity(r.len() + 4);
            let mut day = String::new();
            for row in r {
                let t = log_time(&row.ts);
                if t.day != day {
                    day = t.day.clone();
                    out.push(Item::Day(t.day, t.day_label));
                }
                out.push(Item::Row(row.clone()));
            }
            out
        })
    });

    view! {
        <PageFrame
            title="Traffic"
            mode=PageMode::Fill
            class="traffic"
            head_extra=move || view! { <TrafficTabs conversations=conversations_count()/> }
            actions=move || {
                view! {
                    <GpuButton vram=vram open=gpu_open/>
                    <button
                        class="btn ghost"
                        title="Stop prepending live rows (they are counted meanwhile)"
                        on:click=move |_| {
                            if halted.get_untracked() { resume() } else { paused.set(true) }
                        }
                    >
                        {move || if halted.get() { "Resume" } else { "Pause" }}
                    </button>
                }
            }
            toolbar=move || {
                view! {
                    <FilterBar
                        query=alias_text
                        placeholder="alias contains…"
                        shown=Signal::derive(move || rows.with(Vec::len))
                        total=Signal::derive(move || rows.with(Vec::len))
                        noun="rows"
                        chips=chips
                        active=own_active
                        status=status
                        on_clear=clear
                        query_addon=move || {
                            view! {
                                <KnownValues
                                    dim="alias"
                                    noun="aliases"
                                    text=alias_text
                                    apply=alias_q
                                />
                            }
                        }
                        extra=move || {
                            view! {
                                <span class="known-box">
                                    <input
                                        class="input filter-q traffic-up"
                                        type="search"
                                        placeholder="upstream contains…"
                                        autocomplete="off"
                                        spellcheck="false"
                                        data-untracked
                                        prop:value=move || upstream_text.get()
                                        on:input=move |ev| upstream_text.set(event_target_value(&ev))
                                    />
                                    <KnownValues
                                        dim="upstream"
                                        noun="upstreams"
                                        text=upstream_text
                                        apply=upstream_q
                                    />
                                </span>
                                <Select value=class options=class_options/>
                                <Select value=key_id options=key_options/>
                                <label class="check errors-only">
                                    <input
                                        type="checkbox"
                                        prop:checked=move || errors_only.get()
                                        on:change=move |ev| {
                                            errors_only_raw
                                                .set(
                                                    if event_target_checked(&ev) {
                                                        "true".into()
                                                    } else {
                                                        String::new()
                                                    },
                                                )
                                        }
                                    />
                                    "errors only"
                                </label>
                                <span
                                    class="page-size"
                                    title="Rows per page — and how many the live tail keeps"
                                >
                                    <Select value=size_pref options=size_options/>
                                </span>
                            }
                        }
                    />
                }
            }
        >
            <Show when=move || gpu_open.get() || vram.with(|v| v.as_ref().is_some_and(|v| !v.queue.is_empty()))>
                <GpuPanel vram=vram/>
            </Show>
            {move || error.get().map(|e| view! { <div class="chart-err">"Could not load — " {e}</div> })}
            <div class="fill-pane card pad0 log-pane" class:stale=move || loading.get() && rows.with(|r| !r.is_empty())>
                <table class="data log-table many-cols" style=col_widths>
                    <thead>
                        <tr>
                            <th title="local time">"Time"</th>
                            <th>"Model"</th>
                            <th>"Upstream"</th>
                            <th class="col-p2">"Route"</th>
                            <th>"Status"</th>
                            <th class="num-h col-p3">"TTFB"</th>
                            <th class="num-h">"Total"</th>
                            <th class="num-h">"Tokens"</th>
                        </tr>
                    </thead>
                    <tbody>
                        <For each=move || items.get() key=Item::key let:it>
                            {match it {
                                Item::Day(_, label) => {
                                    view! {
                                        <tr class="day-sep">
                                            <td colspan="8">{label}</td>
                                        </tr>
                                    }
                                        .into_any()
                                }
                                Item::Row(r) => view! { <TrafficRow r=r expanded=expanded/> }.into_any(),
                            }}
                        </For>
                    </tbody>
                </table>
                <Show when=move || !loading.get() && rows.with(Vec::is_empty)>
                    <div class="empty">"No matching requests."</div>
                </Show>
                <Show when=move || rows.with(|r| !r.is_empty())>
                    <div class="log-more">
                        <Show
                            when=move || more.get()
                            fallback=|| {
                                view! {
                                    <span class="dim">
                                        "The start of the log for these filters — nothing older is kept."
                                    </span>
                                }
                            }
                        >
                            <button class="btn ghost" disabled=move || loading.get() on:click=load_older>
                                {move || format!("Load {} older", grouped(page.get() as u64))}
                            </button>
                            <span class="dim">"appends below; the live tail pauses while older rows are shown"</span>
                        </Show>
                    </div>
                </Show>
            </div>
        </PageFrame>
    }
}

/// One entry of the log table: a day's separator, or a request.
#[derive(Clone, PartialEq)]
// Short-lived render rows; boxing `Row` would ripple through every match arm for no gain.
#[allow(clippy::large_enum_variant)]
enum Item {
    Day(String, String),
    Row(RequestRow),
}

impl Item {
    fn key(&self) -> String {
        match self {
            Self::Day(day, _) => format!("d{day}"),
            Self::Row(r) => r.log_id.to_string(),
        }
    }
}

/// The values a box can be filled with: every alias (or upstream) that has
/// usage in the last 30 days, narrowed by what the box already holds. A pick
/// fills the box and applies at once.
#[component]
fn KnownValues(
    dim: &'static str,
    noun: &'static str,
    text: RwSignal<String>,
    apply: RwSignal<String>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let btn: NodeRef<html::Button> = NodeRef::new();
    let asked = RwSignal::new(false);
    Effect::new(move |_| {
        if open.get() {
            asked.set(true);
        }
    });
    // Fetched on first open; no limit, so the list is every value there is.
    let values = LocalResource::new(move || {
        let go = asked.get();
        async move {
            if !go {
                return None;
            }
            Some(
                crate::api::get::<UsageTopResponse>(format!("/api/usage/top?dim={dim}"))
                    .await
                    .map(|r| {
                        let mut v: Vec<(String, i64)> = r
                            .rows
                            .iter()
                            .filter_map(|c| {
                                let label = r
                                    .series
                                    .iter()
                                    .find(|s| s.key == c.series)
                                    .map(|s| s.label.clone())
                                    .unwrap_or_else(|| c.series.clone());
                                // The rollup keeps an upstream's id; one that
                                // is gone (or none at all) has no name a box
                                // could match in the log.
                                let nameless = label.is_empty()
                                    || label.starts_with('(')
                                    || (dim == "upstream"
                                        && label == format!("upstream {}", c.series));
                                (!nameless).then_some((label, c.requests))
                            })
                            .collect();
                        // Busiest first: the value most likely wanted.
                        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                        v
                    }),
            )
        }
    });
    let shown = Memo::new(move |_| {
        let words = text.with(|t| filter_words(t));
        match values.get().flatten() {
            Some(Ok(all)) => {
                let total = all.len();
                let v: Vec<(String, i64)> = all
                    .into_iter()
                    .filter(|(l, _)| words.iter().all(|w| matches_word(l, w)))
                    .collect();
                Ok((v, total))
            }
            Some(Err(e)) => Err(e.to_string()),
            None => Err(String::new()),
        }
    });
    let pick = move |v: String| {
        text.set(v.clone());
        apply.set(v);
        open.set(false);
    };
    view! {
        <button
            type="button"
            class="btn ghost sm known-btn"
            node_ref=btn
            title=format!("Known {noun} (seen in the last 30 days)")
            aria-haspopup="listbox"
            aria-expanded=move || open.get().to_string()
            on:click=move |_| open.update(|o| *o = !*o)
        >
            "▾"
        </button>
        <Popover open=open anchor=btn class="known-pop" min_width=260>
            {move || match shown.get() {
                Err(e) if e.is_empty() => view! { <div class="pop-empty">"Loading…"</div> }.into_any(),
                Err(e) => view! { <div class="pop-empty status-err">{e}</div> }.into_any(),
                Ok((v, total)) => {
                    let count = if v.len() == total {
                        format!("{} seen in the last 30 days", count_of(total, noun))
                    } else {
                        format!(
                            "{} of {} seen in the last 30 days",
                            grouped(v.len() as u64),
                            count_of(total, noun),
                        )
                    };
                    view! {
                        <div class="pop-count known-count">{count}</div>
                        <div class="pop-list known-list" role="listbox">
                            {v
                                .into_iter()
                                .map(|(label, n)| {
                                    let l = label.clone();
                                    let tip = label.clone();
                                    view! {
                                        <button
                                            type="button"
                                            class="known-item"
                                            role="option"
                                            title=tip
                                            on:click=move |_| pick(l.clone())
                                        >
                                            <span class="mono-sm known-name">{label}</span>
                                            <span class="dim mono-sm">{format!("{} req", grouped(n.max(0) as u64))}</span>
                                        </button>
                                    }
                                })
                                .collect_view()}
                        </div>
                    }
                        .into_any()
                }
            }}
        </Popover>
    }
}

/// The GPU ledger for this page: the bus when it has spoken, else one fetch
/// — a page opened mid-session should not wait for the next tick.
fn gpu_state() -> Memo<Option<VramStatus>> {
    let live = use_live();
    let initial = LocalResource::new(|| crate::api::get::<VramStatus>("/api/vram"));
    Memo::new(move |_| {
        live.vram
            .get()
            .or_else(|| initial.get().and_then(|r| r.ok()))
            .filter(|v| v.enabled)
    })
}

/// The GPU in one line in the head: free memory, and anything waiting for
/// it. A click opens the full panel above the log; a queue opens it by
/// itself.
#[component]
fn GpuButton(vram: Memo<Option<VramStatus>>, open: RwSignal<bool>) -> impl IntoView {
    view! {
        {move || {
            vram.get()
                .map(|v| {
                    let free = match v.free_bytes {
                        Some(f) => {
                            format!(
                                "GPU · {} free of {}",
                                human_bytes(f),
                                human_bytes(v.capacity_bytes),
                            )
                        }
                        None => "GPU".to_string(),
                    };
                    let waiting = v.queue.len();
                    view! {
                        <button
                            class="btn ghost gpu-line"
                            aria-expanded=move || open.get().to_string()
                            title=format!(
                                "{} · {} resident — click for what is resident and what waits",
                                if v.active { "admitting" } else { "reporting only" },
                                v.resident.len(),
                            )
                            on:click=move |_| open.update(|o| *o = !*o)
                        >
                            {free}
                            {(waiting > 0)
                                .then(|| {
                                    view! {
                                        <span class="count attn">{format!("{waiting} waiting")}</span>
                                    }
                                })}
                            <span class="caret-icon" aria-hidden="true">"▸"</span>
                        </button>
                    }
                })
        }}
    }
}

/// The GPU plane (quickdoc §9b): what is resident, what it is measured to cost,
/// and anything queued behind it.
///
/// It lives on Traffic because a request waiting for VRAM *is* traffic — it has
/// arrived, it is not being served yet, and the reason is here. Measured and
/// estimated figures are labelled apart on purpose: the estimate is weights +
/// KV read from the GGUF and is a lower bound, so presenting the two as one
/// number would be the wrong kind of tidy.
#[component]
fn GpuPanel(vram: Memo<Option<VramStatus>>) -> impl IntoView {
    let used_pct = move || {
        vram.get()
            .filter(|v| v.capacity_bytes > 0)
            .map(|v| {
                let free = v.free_bytes.unwrap_or(0);
                let used = v.capacity_bytes.saturating_sub(free);
                (used as f64 * 100.0 / v.capacity_bytes as f64).clamp(0.0, 100.0)
            })
            .unwrap_or(0.0)
    };

    view! {
        {move || {
            let v = vram.get().unwrap_or_default();
            let queue = v.queue.clone();
            let resident = v.resident.clone();
            let telemetry = v.telemetry.clone();
            let reason = v.inactive_reason.clone();
            let capacity = v.capacity_bytes;
            let free = v.free_bytes;
            let estimated = v.estimated_resident_bytes;
            let measured = v.free_measured;
            let hold_active = v.hold_active;
            let draining_n = v.draining.len();
            view! {
                <section class="card gpu-card">
                    <div class="row">
                        <strong>"GPU memory"</strong>
                        <span class=if v.active { "chip ok" } else { "chip off" }>
                            <span class="dot"></span>
                            {if v.active { "admitting" } else { "reporting only" }}
                        </span>
                        <span class="spacer" style="flex:1"></span>
                        <span class="dim mono-sm" title=telemetry.clone()>
                            {match free {
                                Some(f) => {
                                    format!(
                                        "{} free of {} ({})",
                                        human_bytes(f),
                                        human_bytes(capacity),
                                        if measured { "measured" } else { "derived from estimates" },
                                    )
                                }
                                None => telemetry.clone(),
                            }}
                        </span>
                    </div>
                    // `!= 0` rather than `> 0`: inside `view!` a bare `>`
                    // closes the tag.
                    <Show when=move || capacity != 0>
                        <div class="progress" style="margin-top:8px">
                            <i style=move || format!("width:{:.1}%", used_pct())></i>
                        </div>
                    </Show>
                    <div class="row" style="margin-top:8px">
                        {resident
                            .iter()
                            .map(|r| {
                                let hue = hue_for(&r.model);
                                // The learned peak rides beside the idle
                                // estimate rather than inside it: it is
                                // what admission keeps free on top, and
                                // the note in the tooltip says whether the
                                // row has one at all.
                                // What an audio model that has not loaded
                                // yet is about to take is part of its
                                // estimate, not on top of it: the driver
                                // does not show it yet, so admission keeps
                                // it free.
                                let label = format!(
                                    "{} · {} · {}{}{}",
                                    r.container,
                                    r.state,
                                    human_bytes(r.estimated_bytes),
                                    match r.peak_extra_bytes {
                                        Some(p) => format!(" + {} peak", human_bytes(p)),
                                        None => String::new(),
                                    },
                                    match r.pending_bytes {
                                        Some(p) => format!(", {} of it to load", human_bytes(p)),
                                        None => String::new(),
                                    },
                                );
                                let title = match (&r.note, r.idle_seconds) {
                                    (Some(n), _) => n.clone(),
                                    (None, Some(s)) => format!("idle {s}s"),
                                    (None, None) => "not routed to by lmgw yet".into(),
                                };
                                view! {
                                    <span class="model-chip" style=format!("--hue:{hue}") title=title>
                                        <i></i>
                                        {r.model.clone()}
                                        <span class="dim mono-sm">{label}</span>
                                        <Show when={
                                            let n = r.in_flight;
                                            move || n > 0
                                        }>
                                            <span class="chip live">
                                                <span class="dot"></span>
                                                "busy"
                                            </span>
                                        </Show>
                                    </span>
                                }
                            })
                            .collect_view()}
                        <Show when={
                            let empty = resident.is_empty();
                            move || empty
                        }>
                            <span class="dim mono-sm">"nothing resident"</span>
                        </Show>
                    </div>
                    <div class="dim mini-note">
                        {format!("estimated resident {}", human_bytes(estimated))}
                        {reason.map(|r| format!(" — {r}")).unwrap_or_default()}
                    </div>
                    <Show when=move || hold_active>
                        <div class="dim mini-note">{format!("Hold active · {draining_n} draining")}</div>
                    </Show>
                    <Show when={
                        let n = queue.len();
                        move || n > 0
                    }>
                        <div style="margin-top:10px">
                            <strong class="mono-sm">
                                {format!("{} waiting for GPU memory", queue.len())}
                            </strong>
                            {queue
                                .iter()
                                .map(|w| {
                                    view! {
                                        <div class="row mono-sm" style="margin-top:4px">
                                            <span class="type-badge">{w.position}</span>
                                            <span>{w.alias.clone()}</span>
                                            <span class="dim">
                                                {format!(
                                                    "needs {} · {} · waiting {:.1}s",
                                                    human_bytes(w.needs_bytes),
                                                    w.stage,
                                                    w.waiting_ms as f64 / 1000.0,
                                                )}
                                            </span>
                                        </div>
                                    }
                                })
                                .collect_view()}
                        </div>
                    </Show>
                </section>
            }
        }}
    }
}

/// `(fresh, cache read, cache write)` for a row whose provider reported a
/// cache split — `None` when it reported none, which is not the same as a row
/// that read nothing from cache (§2.1). Fresh is the remainder, never a fourth
/// number: `prompt_tokens` is the total, cache included.
fn input_split(r: &RequestRow) -> Option<(i64, i64, i64)> {
    let read = r.cached_in_tokens?;
    let write = r.cache_write_tokens.unwrap_or(0);
    if read == 0 && write == 0 {
        return None;
    }
    let total = r.prompt_tokens.unwrap_or(0);
    Some(((total - read - write).max(0), read, write))
}

/// `request_logs.fallback_reason` (unified-KV design §12.32) as short display
/// text: `hold` | `benchmark` | `external_vram` | `background` |
/// `unavailable` named, anything else shown verbatim (a reason this UI does
/// not know yet is still worth showing). `benchmark`: a benchmark run held
/// the GPU (benchmark design §3.2), so the row's hold fallback answered.
fn fallback_label(reason: &str) -> String {
    match reason {
        "hold" => "fallback · hold".to_string(),
        "benchmark" => "fallback · benchmark".to_string(),
        "external_vram" => "fallback · outside VRAM".to_string(),
        "background" => "fallback · background".to_string(),
        "unavailable" => "fallback · candidate unavailable".to_string(),
        other => format!("fallback · {other}"),
    }
}

#[component]
fn TrafficRow(r: RequestRow, expanded: RwSignal<Option<i64>>) -> impl IntoView {
    let id = r.log_id;
    let ok = r.status < 400;
    let t = log_time(&r.ts);
    let name = if r.ingress_proto == "mcp" {
        r.mcp_tool
            .clone()
            .unwrap_or_else(|| r.requested_alias.clone())
    } else {
        r.requested_alias.clone()
    };
    let hue = hue_for(&name);
    let route = format!(
        "{} → {}",
        r.ingress_proto,
        r.egress_proto.clone().unwrap_or_else(|| "—".into())
    );
    let tokens = match (r.prompt_tokens, r.completion_tokens) {
        (Some(p), Some(c)) => format!("{p} → {c}"),
        (Some(p), None) => p.to_string(),
        _ => "—".into(),
    };
    // Two rows with the same token count can cost 8x each other, and the only
    // thing that says why is how much of the input was cache. Hover says it;
    // the detail row spells it out.
    let split = input_split(&r);
    let tokens_title = split
        .map(|(fresh, read, write)| {
            format!("input: {fresh} fresh · {read} cached · {write} written")
        })
        .unwrap_or_default();
    let upstream = r.upstream_name.clone().unwrap_or_default();
    let up_model = r.upstream_model.clone().unwrap_or_default();
    let up_title = if up_model.is_empty() {
        upstream.clone()
    } else {
        format!("{upstream} · {up_model}")
    };
    // A fallback answered instead of the requested local model (unified-KV
    // design §12.32): the badge sits with the upstream it names, and the
    // tooltip carries the raw reason for one this UI does not label yet.
    let fallback_badge = r.fallback_reason.clone().map(|reason| {
        let label = fallback_label(&reason);
        view! {
            <span class="type-badge fallback-badge" title=reason>
                {label}
            </span>
        }
    });
    // `request_logs.rung` (ladder design §6, §12 entry 36): the rung this
    // request was judged on, 1-based, empty for a row without a ladder and
    // for one a fallback answered (`GateHeaders::fall_back` clears it, same
    // as the header).
    let rung_badge = r.rung.map(|k| {
        view! {
            <span class="type-badge" title="ladder rung this request was judged on">
                {format!("rung {k}")}
            </span>
        }
    });
    let is_open = move || expanded.get() == Some(id);
    let detail = r.clone();
    view! {
        <tr
            class:err-row=!ok
            class="clickable"
            title=t.utc.clone()
            on:click=move |_| expanded.update(|e| *e = if *e == Some(id) { None } else { Some(id) })
        >
            <td class="dim mono-sm">{t.time.clone()}</td>
            <td class="log-name" title=name.clone()>
                <i class="swatch" style=format!("--hue:{hue}")></i>
                <span class="mono-sm">{name.clone()}</span>
                {rung_badge}
            </td>
            <td class="clip dim" title=up_title>
                {upstream}
                <span class="mono-sm">
                    {(!up_model.is_empty()).then(|| format!(" · {up_model}"))}
                </span>
                {fallback_badge}
            </td>
            <td class="dim mono-sm col-p2">{route}</td>
            <td>
                <span class=if ok { "status-ok" } else { "status-err" }>{r.status}</span>
                {r.streamed.then(|| view! { <span class="dim">" ~"</span> })}
            </td>
            <td class="num col-p3">{r.ttfb_ms.map(|v| format!("{v} ms")).unwrap_or_default()}</td>
            <td class="num">{r.total_ms.map(|v| format!("{v} ms")).unwrap_or_default()}</td>
            <td class="num" title=tokens_title>
                {tokens}
            </td>
        </tr>
        <Show when=is_open>
            <tr class="detail-row">
                <td colspan="8">
                    <div class="detail-grid">
                        <span class="dim">"Log id"</span>
                        <span class="mono-sm">{detail.log_id}</span>
                        <span class="dim">"Timestamp"</span>
                        <span class="mono-sm">{format!("{} · {} {} local", t.utc, t.day, t.time)}</span>
                        <span class="dim">"Requested"</span>
                        <span class="mono-sm">{detail.requested_alias.clone()}</span>
                        <span class="dim">"Upstream model"</span>
                        <span class="mono-sm">{detail.upstream_model.clone().unwrap_or_default()}</span>
                        <span class="dim">"Route"</span>
                        <span class="mono-sm">
                            {format!(
                                "{} → {}",
                                detail.ingress_proto,
                                detail.egress_proto.clone().unwrap_or_else(|| "—".into()),
                            )}
                        </span>
                        <span class="dim">"TTFB"</span>
                        <span class="mono-sm">
                            {detail.ttfb_ms.map(|v| format!("{v} ms")).unwrap_or_else(|| "—".into())}
                        </span>
                        {split
                            .map(|(fresh, read, write)| {
                                view! {
                                    <span class="dim">"Input split"</span>
                                    <span class="mono-sm">
                                        {format!("{fresh} fresh · {read} cached · {write} written")}
                                    </span>
                                }
                            })}
                        {detail
                            .max_tokens_clamped
                            .map(|n| {
                                view! {
                                    <span class="dim">"Max tokens"</span>
                                    <span class="mono-sm">{format!("clamped to {n}")}</span>
                                }
                            })}
                        {detail
                            .fallback_reason
                            .clone()
                            .map(|reason| {
                                view! {
                                    <span class="dim">"Fallback"</span>
                                    <span class="mono-sm">{fallback_label(&reason)}</span>
                                }
                            })}
                        {detail
                            .rung
                            .map(|k| {
                                view! {
                                    <span class="dim">"Rung"</span>
                                    <span class="mono-sm">{k.to_string()}</span>
                                }
                            })}
                        {detail
                            .client_key
                            .clone()
                            .filter(|k| !k.is_empty())
                            .map(|k| {
                                view! {
                                    <span class="dim">"Client key"</span>
                                    <span class="mono-sm">{k}</span>
                                }
                            })}
                        {detail
                            .error_kind
                            .clone()
                            .map(|k| {
                                view! {
                                    <span class="dim">"Error kind"</span>
                                    <span class="mono-sm status-err">{k}</span>
                                }
                            })}
                    </div>
                    {detail.error_msg.clone().map(|m| view! { <pre class="preset err-pre">{m}</pre> })}
                </td>
            </tr>
        </Show>
    }
}
