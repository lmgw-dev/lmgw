//! Traffic — the unified request log: live tail on the SSE bus, filters,
//! cursor pagination, and the opened row's every stored field (log rows carry
//! them all, so no extra fetch). Two routes: `/traffic/:log?` (Requests) and
//! `/traffic/conversations/:chain?`.
//!
//! Requests is a list beside the opened row on the shared split view
//! (`widgets::nav_split`): the list is the log, newest first under sticky day
//! headings, the filters in the URL's query, the opened row the path
//! (`/traffic/4711?alias=…`), so a filter change keeps it open and a link to
//! a row selects it. The live tail keeps the newest page of whatever the
//! filters select — a live row is matched against the same filters the
//! server applied, so a filtered view grows only with rows that belong in it.

use std::time::Duration;

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::{use_location, use_navigate, use_params_map};
use lmgw_api_types::{LogsResponse, RequestRow, UsageTopResponse, VramStatus};

use crate::fmt::{count_of, grouped};
use crate::live::use_live;
use crate::url_state::use_query_signal;
use crate::widgets::nav_split::{provide_save_key, NavSplit, SplitConf, SplitState};
use crate::widgets::{filter_words, matches_word, Popover};

// The opened row: its fields as boxed groups.
mod detail;
// The GPU plane: the header's button, its panel.
mod gpu;
// The list pane: the header, the filters, the rows.
mod list;
// A row's measured quantities and its priced parts (billable units §8.4).
mod row_cost;

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

/// Requests and Conversations, each its own route: the toggle under the
/// list's header bar. Each side comes back with the query it was left with.
#[component]
pub(super) fn TrafficSwitch(
    /// Conversations is the one shown.
    conversations: bool,
    #[prop(into)] count: Signal<Option<i64>>,
) -> impl IntoView {
    let requests = crate::url_state::with_last_query("/traffic");
    let convs = crate::url_state::with_last_query("/traffic/conversations");
    view! {
        <nav class="seg sub-nav mlist-switch" aria-label="Traffic">
            <a href=requests class="seg-btn" aria-current=(!conversations).then_some("page")>
                "Requests"
            </a>
            <a href=convs class="seg-btn" aria-current=conversations.then_some("page")>
                "Conversations"
                {move || count.get().map(|n| view! { <span class="count">{grouped(n.max(0) as u64)}</span> })}
            </a>
        </nav>
    }
}

/// The stored conversations' count, for the switch.
pub(super) fn conversations_count() -> Signal<Option<i64>> {
    let index =
        LocalResource::new(|| crate::api::get::<lmgw_api_types::ResponsesIndex>("/api/responses"));
    Signal::derive(move || index.get().and_then(|r| r.ok()).map(|i| i.total_chains))
}

/// Every filter the log is asked with, one canonical query string: it is both
/// what `/api/logs` receives and how two states are compared.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Filters {
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

/// What the server is asked to find for a typed alias part: a request that
/// named no model is stored with the alias `?` and shown as "No model", so
/// searching those words looks for it.
fn alias_needle(q: &str) -> &str {
    if q.trim().eq_ignore_ascii_case("no model") {
        "?"
    } else {
        q
    }
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
        put("alias_q", alias_needle(&self.alias_q));
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
            && has(&r.requested_alias, alias_needle(&self.alias_q))
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

/// Below this pane width the view is one pane at a time, as Models'.
const NARROW_BELOW: f64 = 800.0;

const CONF: SplitConf = SplitConf {
    class: "traffic-split",
    width_key: "traffic.list.width",
    list_width: (300, 38, 480),
    detail_min: 400,
    narrow_below: NARROW_BELOW,
    handle_label: "Resize the list",
};

/// The log's state, which the list, the status page and the detail share.
#[derive(Clone, Copy)]
pub(super) struct Log {
    pub rows: RwSignal<Vec<RequestRow>>,
    pub loading: RwSignal<bool>,
    pub error: RwSignal<Option<String>>,
    /// The last page came back full: there may be older rows.
    pub more: RwSignal<bool>,
    /// Live rows are counted, not prepended (Pause, or older rows shown).
    pub halted: Memo<bool>,
    pub paused: RwSignal<bool>,
    pub page: Memo<usize>,
    pub size_pref: RwSignal<String>,
    pub status: Signal<String>,
    pub load_older: Callback<()>,
    pub resume: Callback<()>,
    pub vram: Memo<Option<VramStatus>>,
    pub f: FilterSignals,
}

/// The filters' URL parameters, and the boxes typed into a beat before them.
#[derive(Clone, Copy)]
pub(super) struct FilterSignals {
    pub alias: RwSignal<String>,
    pub alias_q: RwSignal<String>,
    pub upstream: RwSignal<String>,
    pub upstream_q: RwSignal<String>,
    pub class: RwSignal<String>,
    pub key_id: RwSignal<String>,
    pub error_kind: RwSignal<String>,
    pub errors_only_raw: RwSignal<String>,
    pub errors_only: Signal<bool>,
    pub alias_text: RwSignal<String>,
    pub upstream_text: RwSignal<String>,
}

impl FilterSignals {
    fn new() -> Self {
        let alias_q = use_query_signal("alias_q");
        let upstream_q = use_query_signal("upstream_q");
        let errors_only_raw = use_query_signal("errors_only");
        Self {
            alias: use_query_signal("alias"),
            alias_q,
            upstream: use_query_signal("upstream"),
            upstream_q,
            class: use_query_signal("class"),
            key_id: use_query_signal("key_id"),
            error_kind: use_query_signal("error_kind"),
            errors_only_raw,
            errors_only: Signal::derive(move || truthy(&errors_only_raw.get())),
            alias_text: debounced(alias_q),
            upstream_text: debounced(upstream_q),
        }
    }

    pub fn current(self) -> Filters {
        Filters {
            alias: self.alias.get(),
            alias_q: self.alias_q.get(),
            upstream: self.upstream.get(),
            upstream_q: self.upstream_q.get(),
            class: self.class.get(),
            key_id: self.key_id.get(),
            error_kind: self.error_kind.get(),
            errors_only: self.errors_only.get(),
        }
    }

    /// The filters the popover holds are set (the search and the link's
    /// chips say their own).
    pub fn own_active(self) -> bool {
        !self.upstream_q.with(String::is_empty)
            || !self.upstream_text.with(|t| t.trim().is_empty())
            || !self.class.with(String::is_empty)
            || !self.key_id.with(String::is_empty)
            || self.errors_only.get()
    }

    pub fn clear(self) {
        for s in [
            self.alias,
            self.alias_q,
            self.upstream,
            self.upstream_q,
            self.upstream_text,
            self.class,
            self.key_id,
            self.error_kind,
            self.errors_only_raw,
        ] {
            s.set(String::new());
        }
    }
}

/// Where a row opens: its path, with the list's filters.
pub(super) fn row_href(id: i64, search: &str) -> String {
    format!(
        "/traffic/{id}{}",
        crate::widgets::list_pane::query_without(search, &[])
    )
}

/// Requests (`/traffic/:log?`).
#[component]
pub fn Traffic() -> impl IntoView {
    let live = use_live();
    let params = use_params_map();
    let location = use_location();
    let sel = Memo::new(move |_| {
        params
            .with(|p| p.get("log"))
            .and_then(|s| s.trim().parse::<i64>().ok())
    });
    let has_sel = Memo::new(move |_| sel.with(Option::is_some));

    // Filters live in the URL, so a chart on Usage has somewhere to link to
    // and a filtered view is something the owner can bookmark or paste.
    let f = FilterSignals::new();
    let current = Memo::new(move |_| f.current());

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
            // A newer fetch (the filters moved on) owns the list now.
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
    let load_older = Callback::new(move |()| {
        if let Some(last) = rows.with_untracked(|r| r.last().map(|x| x.log_id)) {
            fetch(Some(last));
        }
    });
    let resume = Callback::new(move |()| {
        paused.set(false);
        if older.get_untracked() || missed.get_untracked() > 0 {
            reload.update(|n| *n = n.wrapping_add(1));
        }
    });

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

    let halted = Memo::new(move |_| paused.get() || older.get());
    let status = Signal::derive(move || {
        let n = rows.with(Vec::len);
        if loading.get() && n == 0 {
            return "Loading…".to_string();
        }
        // The page size beside it says how long the tail is, this says what
        // feeds it.
        let what = count_of(n, "rows");
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

    let log = Log {
        rows,
        loading,
        error,
        more,
        halted,
        paused,
        page,
        size_pref,
        status,
        load_older,
        resume,
        vram: gpu::gpu_state(),
        f,
    };

    let split = SplitState::new(CONF);
    let narrow = split.narrow;
    let save_key = provide_save_key();
    let navigate = use_navigate();
    let go = StoredValue::new(move |to: String, replace: bool| {
        navigate(
            &to,
            leptos_router::NavigateOptions {
                replace,
                scroll: false,
                ..Default::default()
            },
        )
    });
    let open_row = Callback::new(move |(id, replace): (i64, bool)| {
        if sel.get_untracked() != Some(id) {
            let to = row_href(id, &location.search.get_untracked());
            go.with_value(|g| g(to, replace));
        }
    });
    let order =
        Signal::derive(move || rows.with(|r| r.iter().map(|x| x.log_id).collect::<Vec<_>>()));
    let nav = crate::widgets::list_pane::ListNav::new(
        crate::widgets::list_pane::ListRefs::new(),
        order,
        sel,
        narrow,
        open_row,
        |id| format!("/traffic/{id}"),
    );
    let back = Callback::new(move |()| {
        let q = crate::widgets::list_pane::query_without(&location.search.get_untracked(), &[]);
        go.with_value(|g| g(format!("/traffic{q}"), false));
        request_animation_frame(move || nav.focus_list());
    });

    view! {
        <NavSplit
            split=split
            has_sel=has_sel
            save_key=save_key
            search=nav.search
            search_off=Signal::derive(move || narrow.get() && has_sel.get())
            list=ViewFn::from(move || view! { <list::ListPane log=log nav=nav narrow=narrow/> })
            detail=ViewFn::from(move || {
                view! {
                    <Show
                        when=move || has_sel.get()
                        fallback=move || view! { <detail::StatusPage log=log narrow=narrow back=back/> }
                    >
                        <detail::RowDetail
                            log=log
                            id=Signal::derive(move || sel.get().unwrap_or_default())
                            narrow=narrow
                            back=back
                        />
                    </Show>
                }
            })
        />
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
            <span class="select-arrow">"▾"</span>
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
/// `unavailable` | `quota` | `unreachable` | `disabled` | `gpu_setup` |
/// `engine_download` named, anything else shown verbatim (a reason this UI
/// does not know yet is still worth showing). `benchmark`: a benchmark run
/// held the GPU (benchmark design §3.2), so the row's own fallback answered.
/// `gpu_setup`: the machine check said the graphics card is not ready
/// (machine-check design §2.5).
fn fallback_label(reason: &str) -> String {
    format!("fallback · {}", fallback_words(reason))
}

/// A fallback reason in the dashboard's words: the Traffic badge's, and the
/// labs' chips and provenance (machine-check design WP8b gave `gpu_setup`
/// and `engine_download` theirs there too, D180).
pub(crate) fn fallback_words(reason: &str) -> &str {
    match reason {
        "external_vram" => "outside VRAM",
        "unavailable" => "candidate unavailable",
        "quota" => "upstream quota",
        "unreachable" => "upstream unreachable",
        "gpu_setup" => "graphics card not ready",
        "engine_download" => "engine download failed",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searching_no_model_asks_for_the_alias_the_server_stores() {
        assert_eq!(alias_needle(" No Model "), "?");
        assert_eq!(alias_needle("haiku"), "haiku");
        assert_eq!(alias_needle(""), "");
    }
}
