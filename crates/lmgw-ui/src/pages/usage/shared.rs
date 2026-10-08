use std::collections::HashMap;

use leptos::prelude::*;
use lmgw_api_types::{SeriesMeta, UsageCell, UsageSeriesResponse};

use crate::charts::{self};
use crate::fmt::{compact, grouped};

// ---------------------------------------------------------------------------
// One fetch, held across refetches
// ---------------------------------------------------------------------------

/// A resource plus the last value it delivered.
///
/// A refetch must not blank a card: the previous render is held at reduced
/// opacity (`.stale`) instead of flashing a skeleton, and an endpoint that
/// fails says so *in the card* rather than drawing an empty chart that a
/// reader would take for "no traffic".
pub(super) struct Src<T: Send + Sync + 'static> {
    pub(super) data: RwSignal<Option<T>>,
    pub(super) err: RwSignal<Option<String>>,
    pub(super) busy: Memo<bool>,
}

impl<T: Send + Sync + 'static> Clone for Src<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Send + Sync + 'static> Copy for Src<T> {}

impl<T: Send + Sync + 'static> Src<T> {
    /// True while a refetch is in flight over an existing render.
    pub(super) fn stale(&self) -> bool {
        self.busy.get() && self.data.with(|d| d.is_some())
    }
}

pub(super) fn src<T>(res: LocalResource<crate::api::Result<T>>) -> Src<T>
where
    T: Clone + Send + Sync + 'static,
{
    let data = RwSignal::new(None::<T>);
    let err = RwSignal::new(None::<String>);
    Effect::new(move |_| match res.get() {
        Some(Ok(v)) => {
            data.set(Some(v));
            err.set(None);
        }
        Some(Err(e)) => err.set(Some(e.to_string())),
        None => {}
    });
    let busy = Memo::new(move |_| res.get().is_none());
    Src { data, err, busy }
}

#[component]
pub(super) fn CardErr(err: RwSignal<Option<String>>) -> impl IntoView {
    view! {
        {move || {
            err.get().map(|e| view! { <div class="chart-err">"Could not load — " {e}</div> })
        }}
    }
}

// ---------------------------------------------------------------------------
// Time, money, labels
// ---------------------------------------------------------------------------

pub(super) const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
pub(super) const DOWS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

pub(super) fn now_ms() -> f64 {
    js_sys::Date::now()
}

/// Minutes east of UTC — what `/api/usage/*` wants for `tz`, so the buckets it
/// rolls to days are the reader's days and not UTC's (design §3.4).
pub(super) fn tz_minutes() -> i32 {
    -(js_sys::Date::new_0().get_timezone_offset() as i32)
}

pub(super) fn tz_label() -> String {
    let m = tz_minutes();
    let sign = if m < 0 { '-' } else { '+' };
    format!("UTC{sign}{:02}:{:02}", m.abs() / 60, m.abs() % 60)
}

/// The UTC hour key `YYYY-MM-DDTHH` the range parameters are expressed in.
pub(super) fn utc_hour_key(ms: f64) -> String {
    let d = js_sys::Date::new(&leptos::wasm_bindgen::JsValue::from_f64(ms));
    format!(
        "{:04}-{:02}-{:02}T{:02}",
        d.get_utc_full_year(),
        d.get_utc_month() + 1,
        d.get_utc_date(),
        d.get_utc_hours()
    )
}

pub(super) fn local_day(ms: f64) -> String {
    let d = js_sys::Date::new(&leptos::wasm_bindgen::JsValue::from_f64(ms));
    format!(
        "{} {} {}",
        d.get_date(),
        MONTHS[(d.get_month() as usize).min(11)],
        d.get_full_year()
    )
}

/// Short axis label for a bucket key (`2026-09-18`, `2026-09-18T14`,
/// `2026-W38`, `2026-09`).
pub(super) fn bucket_label(b: &str) -> String {
    let bytes = b.as_bytes();
    if b.len() == 13 && bytes.get(10) == Some(&b'T') {
        format!("{}:00", &b[11..13])
    } else if b.len() == 8 && bytes.get(5) == Some(&b'W') {
        b[5..].to_string()
    } else if b.len() == 10 {
        let m: usize = b[5..7].parse().unwrap_or(1);
        format!("{} {}", &b[8..10], MONTHS[m.saturating_sub(1).min(11)])
    } else if b.len() == 7 {
        let m: usize = b[5..7].parse().unwrap_or(1);
        format!("{} {}", MONTHS[m.saturating_sub(1).min(11)], &b[2..4])
    } else {
        b.to_string()
    }
}

/// The same bucket, spelled out for a tooltip or a table row.
pub(super) fn bucket_full(b: &str) -> String {
    if b.len() == 13 {
        format!("{} {}:00", bucket_label(&b[..10]), &b[11..13])
    } else if b.len() == 10 {
        let m: usize = b[5..7].parse().unwrap_or(1);
        format!(
            "{} {} {}",
            &b[8..10],
            MONTHS[m.saturating_sub(1).min(11)],
            &b[..4]
        )
    } else if b.len() == 7 {
        let m: usize = b[5..7].parse().unwrap_or(1);
        format!("{} {}", MONTHS[m.saturating_sub(1).min(11)], &b[..4])
    } else {
        b.to_string()
    }
}

pub(super) fn urlenc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

/// A click-through into Traffic with the filter pre-applied: the Usage page is
/// a lens on the same rows, and the click-through is how a chart that
/// disagrees with the log table gets noticed (design §6.1).
///
/// Only what `/api/logs` can actually filter on goes in. A parameter the
/// server ignores would return the unfiltered head while looking like a
/// filtered view, which is a worse answer than no link at all.
#[derive(Clone, Debug, Default)]
pub(super) struct TrafficLink {
    alias: Option<String>,
    key_id: Option<String>,
    error_kind: Option<String>,
    class: Option<String>,
    errors_only: bool,
}

impl TrafficLink {
    /// Seeded with whatever the page itself is scoped to, so a link inherits
    /// the filter row rather than widening silently on the way over.
    pub(super) fn scoped(class: &str, key_id: &str) -> Self {
        Self {
            class: Some(class.to_string()).filter(|s| !s.is_empty()),
            key_id: Some(key_id.to_string()).filter(|s| !s.is_empty()),
            ..Default::default()
        }
    }
    pub(super) fn alias(mut self, v: impl Into<String>) -> Self {
        self.alias = Some(v.into()).filter(|s| !s.is_empty());
        self
    }
    pub(super) fn key(mut self, id: i64) -> Self {
        self.key_id = Some(id.to_string());
        self
    }
    pub(super) fn kind(mut self, v: impl Into<String>) -> Self {
        self.error_kind = Some(v.into()).filter(|s| !s.is_empty());
        self
    }
    pub(super) fn failures(mut self) -> Self {
        self.errors_only = true;
        self
    }
    pub(super) fn href(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut put = |k: &str, v: &Option<String>| {
            if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
                parts.push(format!("{k}={}", urlenc(v)));
            }
        };
        put("alias", &self.alias);
        put("upstream", &None);
        put("class", &self.class);
        put("key_id", &self.key_id);
        put("error_kind", &self.error_kind);
        // `=true`, not `=1`: Traffic passes this straight to `/api/logs`,
        // whose `Option<bool>` answers 400 for `1`.
        if self.errors_only {
            parts.push("errors_only=true".to_string());
        }
        if parts.is_empty() {
            "/traffic".to_string()
        } else {
            format!("/traffic?{}", parts.join("&"))
        }
    }
}

/// Granularity of a bucket key, for a sentence.
pub(super) fn bucket_word(key: &str) -> &'static str {
    match key.len() {
        13 => "hour",
        8 => "week",
        7 => "month",
        _ => "day",
    }
}

pub(super) fn cur_sym(currency: &str) -> &str {
    match currency {
        "EUR" | "eur" => "€",
        "USD" | "usd" => "$",
        "GBP" | "gbp" => "£",
        "CHF" | "chf" => "CHF ",
        other => other,
    }
}

/// Integer micro-units → a money string. Micro-units all the way in, because a
/// float that has been through JSON twice does not add up.
///
/// Shared with the Agents page's Runs tab, so a run's cost is printed exactly
/// the way every other cost on the dashboard is.
pub(crate) fn money(micro: i64, currency: &str) -> String {
    let neg = micro < 0;
    let v = (micro.abs() as f64) / 1e6;
    let whole = v.trunc() as i64;
    let body = if v < 1000.0 {
        let cents = ((v - whole as f64) * 100.0).round() as i64;
        let (whole, cents) = if cents >= 100 {
            (whole + 1, 0)
        } else {
            (whole, cents)
        };
        format!("{}.{:02}", grouped(whole as u64), cents)
    } else {
        grouped(v.round() as u64)
    };
    let sym = cur_sym(currency);
    if neg {
        format!("-{sym}{body}")
    } else {
        format!("{sym}{body}")
    }
}

/// [`money`] for one request's amount and its parts, which are often below
/// a cent: a transcription at 0.006 a minute costs 0.00274, and a rounded
/// "0.00" would read as free — nor would parts rounded to cents add up to
/// the total beside them. It keeps the digits the amount has
/// (`fmt::price`), never fewer than two decimals; from a thousand up it is
/// [`money`].
pub(crate) fn money_fine(micro: i64, currency: &str) -> String {
    let v = micro.abs() as f64 / 1e6;
    if v >= 1000.0 {
        return money(micro, currency);
    }
    let mut s = crate::fmt::price(v);
    if s.split_once('.').map_or(0, |(_, d)| d.len()) < 2 {
        s = format!("{v:.2}");
    }
    let sign = if micro < 0 { "-" } else { "" };
    format!("{sign}{}{s}", cur_sym(currency))
}

/// The §2.3 obligation in one string: what the money figure beside it does
/// **not** cover. Returns `None` when nothing is unpriced.
pub(super) fn unpriced_note(reqs: i64, toks: i64) -> Option<String> {
    if reqs <= 0 && toks <= 0 {
        return None;
    }
    Some(format!(
        "{} requests ({} tokens) unpriced",
        grouped(reqs.max(0) as u64),
        compact(toks.max(0) as f64)
    ))
}

/// A money axis's tick labels. Cents while the ticks are a cent apart or
/// more; below that as many decimals as the step needs, or a sub-cent scale
/// reads "$0.00" on every tick (review ux:U-10).
pub(super) fn money_ticks(max_micro: f64, currency: &str) -> impl Fn(f64) -> String {
    let step = charts::nice_ticks(max_micro, 4)
        .get(1)
        .copied()
        .unwrap_or(0.0)
        / 1e6;
    let decimals = if step <= 0.0 || step >= 0.01 {
        2
    } else {
        (3..=8)
            .find(|d| {
                let f = 10f64.powi(*d);
                ((step * f).round() - step * f).abs() < 1e-6
            })
            .unwrap_or(8) as usize
    };
    let currency = currency.to_string();
    move |v: f64| {
        if decimals == 2 {
            money(v as i64, &currency)
        } else {
            format!("{}{:.*}", cur_sym(&currency), decimals, v / 1e6)
        }
    }
}

/// A spend chart's scale never shrinks below a cent: a window with nothing
/// (or next to nothing) spent still draws a readable axis instead of ticks a
/// millionth of a unit apart.
pub(super) const MIN_MONEY_SCALE: f64 = 10_000.0;

// ---------------------------------------------------------------------------
// The (bucket × series) matrix
// ---------------------------------------------------------------------------

pub(super) fn cell_add(dst: &mut UsageCell, s: &UsageCell) {
    dst.requests += s.requests;
    dst.tokens_in += s.tokens_in;
    dst.tokens_out += s.tokens_out;
    dst.tokens_cached += s.tokens_cached;
    dst.tokens_cache_write += s.tokens_cache_write;
    dst.tokens_reasoning += s.tokens_reasoning;
    dst.cost_micro += s.cost_micro;
    dst.cost_unknown_requests += s.cost_unknown_requests;
    dst.cost_unknown_tokens += s.cost_unknown_tokens;
    dst.errors += s.errors;
    dst.refusals += s.refusals;
    dst.ttfb_sum += s.ttfb_sum;
    dst.ttfb_count += s.ttfb_count;
    dst.total_sum += s.total_sum;
    dst.total_count += s.total_count;
    dst.decode_tokens += s.decode_tokens;
    dst.decode_ms += s.decode_ms;
    dst.prefill_ms += s.prefill_ms;
    dst.prompt_n += s.prompt_n;
    dst.cache_n += s.cache_n;
    dst.draft_n += s.draft_n;
    dst.draft_accepted += s.draft_accepted;
    dst.audio_in_ms += s.audio_in_ms;
    dst.chars_in += s.chars_in;
    dst.images_out += s.images_out;
    dst.cost_unknown_audio_in_ms += s.cost_unknown_audio_in_ms;
    dst.cost_unknown_chars_in += s.cost_unknown_chars_in;
    dst.cost_unknown_images_out += s.cost_unknown_images_out;
}

/// `cells` flattened into `[bucket][series]`, with every bucket in the window
/// present — including the empty ones, because a chart that silently drops
/// quiet days draws a lie about its own x-axis.
pub(super) struct Grid {
    pub(super) buckets: Vec<String>,
    pub(super) series: Vec<SeriesMeta>,
    pub(super) cells: Vec<Vec<UsageCell>>,
    pub(super) totals: Vec<UsageCell>,
}

impl Grid {
    pub(super) fn build(r: &UsageSeriesResponse) -> Grid {
        let series: Vec<SeriesMeta> = if r.series.is_empty() {
            vec![SeriesMeta {
                key: String::new(),
                label: "all".into(),
                slot: Some(0),
                local: false,
                folded: None,
            }]
        } else {
            r.series.clone()
        };
        let bi: HashMap<&str, usize> = r
            .buckets
            .iter()
            .enumerate()
            .map(|(i, b)| (b.as_str(), i))
            .collect();
        let si: HashMap<&str, usize> = series
            .iter()
            .enumerate()
            .map(|(i, s)| (s.key.as_str(), i))
            .collect();
        let mut cells = vec![vec![UsageCell::default(); series.len()]; r.buckets.len()];
        for c in &r.cells {
            let (Some(&b), Some(&s)) = (bi.get(c.bucket.as_str()), si.get(c.series.as_str()))
            else {
                continue;
            };
            cells[b][s] = c.clone();
        }
        let totals = cells
            .iter()
            .map(|row| {
                let mut t = UsageCell::default();
                for c in row {
                    cell_add(&mut t, c);
                }
                t
            })
            .collect();
        Grid {
            buckets: r.buckets.clone(),
            series,
            cells,
            totals,
        }
    }
    pub(super) fn nb(&self) -> usize {
        self.buckets.len()
    }
    pub(super) fn labels(&self) -> Vec<String> {
        self.buckets.iter().map(|b| bucket_label(b)).collect()
    }
    /// Every how many buckets an x label fits in `iw` pixels of plot: a
    /// label is about 48 px wide, and labels must not touch.
    pub(super) fn every(&self, iw: f64) -> usize {
        let fit = ((iw / 58.0).floor() as usize).clamp(2, 12);
        self.nb().div_ceil(fit).max(1)
    }
}

pub(super) fn heads(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Mean latency in ms from the rollup's sum/count pair — a mean, and labelled
/// as one. Percentiles do not average, so the window's p50/p95 come from the
/// stored histograms via the response, never from these.
pub(super) fn mean_ms(sum: i64, count: i64) -> Option<f64> {
    (count > 0).then(|| sum as f64 / count as f64)
}

// ---------------------------------------------------------------------------
// Chart card furniture: size, head, the table twin
// ---------------------------------------------------------------------------

/// The width a card's chart has: its box's content width, followed through
/// every resize of the card (a rail folding, a column moving), not only the
/// window's. Height never feeds back: a chart re-renders on width alone.
pub(super) fn chart_width(node: NodeRef<leptos::html::Div>) -> Memo<f64> {
    let size = charts::use_element_size(node);
    Memo::new(move |_| size.get().0)
}

/// A card's head: its title, a one-line note (a click reads it whole), and
/// the Chart | Table switch.
#[component]
pub(crate) fn ChartHead(
    title: &'static str,
    #[prop(into)] note: TextProp,
    /// `None` for a card that is its own table.
    #[prop(optional)]
    tv: Option<RwSignal<bool>>,
    /// Something to say after the note — a link.
    #[prop(optional, into)]
    extra: Option<ViewFn>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let tip = note.clone();
    view! {
        <div class="chart-head">
            <h3>{title}</h3>
            <span
                class="note"
                class:open=move || open.get()
                title=move || tip.get().to_string()
                on:click=move |_| open.update(|o| *o = !*o)
            >
                {move || note.get()}
            </span>
            {extra.map(|e| e.run())}
            {tv.map(|tv| view! { <ViewToggle tv=tv/> })}
        </div>
    }
}

/// Chart | Table. Every chart has its table twin: tooltips enhance, the
/// table is what guarantees no value is gated behind a pointer. It swaps in
/// the same card, which grows to hold it — never a scroller of its own.
///
/// The choice is the card's own signal, not re-created per render: a chart
/// re-renders whenever a live frame lands, and a table the reader opened
/// must not snap back under them.
#[component]
pub(super) fn ViewToggle(tv: RwSignal<bool>) -> impl IntoView {
    view! {
        <div class="seg view-toggle" role="group" aria-label="Show as">
            <button
                type="button"
                class="seg-btn"
                class:active=move || !tv.get()
                aria-pressed=move || (!tv.get()).to_string()
                on:click=move |_| tv.set(false)
            >
                "Chart"
            </button>
            <button
                type="button"
                class="seg-btn"
                class:active=move || tv.get()
                aria-pressed=move || tv.get().to_string()
                on:click=move |_| tv.set(true)
            >
                "Table"
            </button>
        </div>
    }
}

/// A chart's table: the first column is the row's label, the rest numbers.
/// Headers carry their full text in a tooltip, since a series name can be
/// longer than its column.
fn data_table(headers: Vec<String>, rows: Vec<Vec<String>>) -> AnyView {
    let head: Vec<AnyView> = headers
        .into_iter()
        .enumerate()
        .map(|(i, h)| {
            let cls = if i == 0 { "" } else { "num-h" };
            let tip = h.clone();
            view! { <th class=cls title=tip>{h}</th> }.into_any()
        })
        .collect();
    let body: Vec<AnyView> = rows
        .into_iter()
        .map(|r| {
            let tds: Vec<AnyView> = r
                .into_iter()
                .enumerate()
                .map(|(i, c)| {
                    let cls = if i == 0 { "" } else { "num" };
                    view! { <td class=cls>{c}</td> }.into_any()
                })
                .collect();
            view! { <tr>{tds}</tr> }.into_any()
        })
        .collect();
    view! {
        <div class="table-scroll chart-table-wrap">
            <table class="data chart-table">
                <thead>
                    <tr>{head}</tr>
                </thead>
                <tbody>{body}</tbody>
            </table>
        </div>
    }
    .into_any()
}

/// The chart, or its table in the same place.
pub(crate) fn chart_or_table(
    tv: RwSignal<bool>,
    chart: impl FnOnce() -> AnyView,
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
) -> AnyView {
    if tv.get() {
        data_table(headers, rows)
    } else {
        chart()
    }
}

/// The folded tail of a series response, rather than a named series.
pub(super) fn is_other(s: &SeriesMeta) -> bool {
    s.folded.is_some() || (s.slot.is_none() && s.key == "other" && s.label == "Other")
}

/// Every named series of a response, busiest first — what claims a colour.
pub(super) fn named_keys(series: &[SeriesMeta]) -> Vec<String> {
    series
        .iter()
        .filter(|s| !is_other(s))
        .map(|s| s.key.clone())
        .collect()
}

/// A series' colour on this page: its slot, and grey only for the tail.
pub(super) fn series_color(slots: &HashMap<String, u8>, s: &SeriesMeta) -> &'static str {
    if is_other(s) {
        "var(--c-other)"
    } else {
        charts::slot_of(slots, &s.key)
    }
}

/// A series' name in a legend: the folded tail says how much it holds —
/// "Other (49 aliases)" — so the grey swatch is never a mystery size.
pub(super) fn series_label(s: &SeriesMeta, group_by: &str) -> String {
    match s.folded {
        Some(n) if is_other(s) => {
            let noun = match (group_by, n) {
                (_, 1) => match group_by {
                    "key" => "key",
                    "upstream" => "upstream",
                    "class" => "class",
                    _ => "alias",
                },
                ("key", _) => "keys",
                ("upstream", _) => "upstreams",
                ("class", _) => "classes",
                _ => "aliases",
            };
            format!("{} ({} {noun})", s.label, grouped(n as u64))
        }
        _ => s.label.clone(),
    }
}

// ---------------------------------------------------------------------------
// Filter window
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Win {
    pub(super) from: String,
    pub(super) to: String,
    pub(super) bucket: String,
    pub(super) label: String,
}

pub(super) fn window_for(range: &str, bucket_override: &str) -> Win {
    let now = now_ms();
    let (hours, default_bucket) = match range {
        "24h" => (24.0, "hour"),
        "7d" => (24.0 * 7.0, "day"),
        "12mo" => (24.0 * 365.0, "month"),
        _ => (24.0 * 30.0, "day"),
    };
    let start = now - hours * 3_600_000.0;
    let bucket = if bucket_override.is_empty() {
        default_bucket.to_string()
    } else {
        bucket_override.to_string()
    };
    Win {
        from: utc_hour_key(start),
        to: utc_hour_key(now),
        label: format!(
            "{} – {} · {} · {} buckets",
            local_day(start),
            local_day(now),
            tz_label(),
            bucket
        ),
        bucket,
    }
}

pub(super) fn series_url(w: &Win, group_by: &str, class: &str, key: &str, limit: usize) -> String {
    let mut s = format!(
        "/api/usage/series?from={}&to={}&bucket={}&tz={}&group_by={}&limit={}",
        w.from,
        w.to,
        w.bucket,
        tz_minutes(),
        group_by,
        limit
    );
    if !class.is_empty() {
        s.push_str(&format!("&class={class}"));
    }
    if !key.is_empty() {
        s.push_str(&format!("&key_id={key}"));
    }
    s
}

pub(super) fn scalar_url(path: &str, w: &Win, class: &str, key: &str, extra: &str) -> String {
    let mut s = format!(
        "/api/usage/{}?from={}&to={}&tz={}{}",
        path,
        w.from,
        w.to,
        tz_minutes(),
        extra
    );
    if !class.is_empty() {
        s.push_str(&format!("&class={class}"));
    }
    if !key.is_empty() {
        s.push_str(&format!("&key_id={key}"));
    }
    s
}
