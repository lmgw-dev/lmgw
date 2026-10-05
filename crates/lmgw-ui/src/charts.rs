//! Inline-SVG chart primitives for the Usage page.
//!
//! No JavaScript charting library (usage-analytics design §6.2): the dashboard
//! is CSR WASM, so a JS chart lib would mean a `wasm-bindgen` bridge for every
//! datum, a second layout system that knows nothing about the CSS tokens, and
//! a bundle bigger than the app. The chart set is small, fixed and mostly
//! rectangles — this module is the scales, ticks, path builders, hover layer
//! and legend that the eleven cards share, so no card invents its own.
//!
//! The mark specs here are not negotiable per-chart (§6.2): ≤24px columns with
//! a 4px rounded data end, 2px lines, ≥8px markers with a 2px surface ring,
//! 10%-opacity area washes, a **solid** hairline grid (dashing is reserved for
//! projection), and a 2px surface gap between touching fills.

use std::time::Duration;

use leptos::prelude::*;
use leptos::wasm_bindgen::JsCast;

// ---------------------------------------------------------------------------
// Series colour (§6.3)
// ---------------------------------------------------------------------------

/// Chart colour for a series' **slot**, never its rank in this response.
///
/// The slot comes from `SeriesMeta::slot`, assigned from the entity's position
/// in the gateway's own sorted list — filtering a series out must not repaint
/// the survivors. `None` is the folded `Other` tail, which wears the neutral.
/// This replaces [`crate::fmt::hue_for`] for charts: the FNV hue is fine for a
/// chip standing alone and unfixable in a chart (unstable under CVD, no
/// lightness discipline, collides).
pub fn slot_color(slot: Option<u8>) -> &'static str {
    match slot {
        Some(0) => "var(--c1)",
        Some(1) => "var(--c2)",
        Some(2) => "var(--c3)",
        Some(3) => "var(--c4)",
        Some(4) => "var(--c5)",
        Some(5) => "var(--c6)",
        _ => "var(--c-other)",
    }
}

/// How many categorical slots a chart page has (`--c1` … `--c6`).
pub const SLOTS: usize = 6;

/// One dimension's slots (aliases, keys, …): which series holds which colour.
///
/// The rule it keeps (usage design §6.3, UX plan Phase 4): a named series
/// takes the first free slot and keeps it for as long as the page shows it,
/// so a filter that drops a series never repaints the survivors; a slot is
/// only taken back from a series that is not on screen any more. Grey is not
/// a slot — it is the folded "Other" and the unpriced remainder, never a
/// named series (a seventh shown series wears it only because six is all the
/// palette has, and the server folds past six anyway).
#[derive(Clone, Debug, Default)]
pub struct SlotPool {
    held: std::collections::HashMap<String, u8>,
}

impl SlotPool {
    /// The slots for `shown` — every named series of this dimension the page
    /// draws right now, busiest first.
    pub fn assign(&mut self, shown: &[String]) -> std::collections::HashMap<String, u8> {
        let mut out = std::collections::HashMap::new();
        let mut taken = [false; SLOTS];
        // Whoever is still on screen keeps what they had.
        for k in shown {
            if let Some(&s) = self.held.get(k) {
                if !taken[s as usize] {
                    taken[s as usize] = true;
                    out.insert(k.clone(), s);
                }
            }
        }
        for k in shown {
            if out.contains_key(k) {
                continue;
            }
            // A slot nobody ever held, else one whose holder left the screen.
            let never =
                (0..SLOTS).find(|&s| !taken[s] && !self.held.values().any(|&h| h as usize == s));
            let Some(s) = never.or_else(|| (0..SLOTS).find(|&s| !taken[s])) else {
                continue;
            };
            self.held.retain(|_, h| *h as usize != s);
            self.held.insert(k.clone(), s as u8);
            taken[s] = true;
            out.insert(k.clone(), s as u8);
        }
        out
    }
}

/// The page's slot pools, one per dimension — page-scoped: a colour learned
/// on this page holds until the page is left.
#[derive(Clone, Copy)]
pub struct SlotRegistry {
    pools: StoredValue<std::collections::HashMap<String, SlotPool>>,
}

impl SlotRegistry {
    pub fn new() -> Self {
        Self {
            pools: StoredValue::new(Default::default()),
        }
    }

    /// [`SlotPool::assign`] on the pool of `dim` ("alias", "key", …).
    pub fn assign(&self, dim: &str, shown: &[String]) -> std::collections::HashMap<String, u8> {
        let mut out = Default::default();
        self.pools.update_value(|p| {
            out = p.entry(dim.to_string()).or_default().assign(shown);
        });
        out
    }
}

/// The colour a series wears on this page: its slot, else the neutral.
pub fn slot_of(slots: &std::collections::HashMap<String, u8>, key: &str) -> &'static str {
    slot_color(slots.get(key).copied())
}

/// A chart's height for its measured width: proportional, within bounds, so
/// a wide card does not draw a letterbox and a narrow one keeps its marks
/// readable.
pub fn chart_height(w: f64) -> f64 {
    (w * 0.30).clamp(180.0, 360.0)
}

/// Single-hue ordinal ramp for the heatmap. Index 0 is "no requests at all",
/// one step off the card surface, so an empty cell reads as empty and not as
/// the bottom of the scale.
pub const SEQ: [&str; 6] = [
    "var(--seq0)",
    "var(--seq1)",
    "var(--seq2)",
    "var(--seq3)",
    "var(--seq4)",
    "var(--seq5)",
];

/// Ordinal bucket for `v` on a 0..=max ramp.
pub fn seq_step(v: f64, max: f64) -> usize {
    if v <= 0.0 || max <= 0.0 {
        return 0;
    }
    (1 + ((v / max) * 4.999) as usize).min(5)
}

// ---------------------------------------------------------------------------
// Scales
// ---------------------------------------------------------------------------

/// A plot box: outer size plus margins. All scale methods work in SVG user
/// units, which are CSS pixels here — the `<svg>` is sized, never scaled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plot {
    pub w: f64,
    pub h: f64,
    pub l: f64,
    pub r: f64,
    pub t: f64,
    pub b: f64,
}

impl Plot {
    pub fn new(w: f64, h: f64, l: f64, r: f64, t: f64, b: f64) -> Self {
        Self { w, h, l, r, t, b }
    }
    /// Inner width (the band or point x extent).
    pub fn iw(&self) -> f64 {
        (self.w - self.l - self.r).max(1.0)
    }
    /// Inner height.
    pub fn ih(&self) -> f64 {
        (self.h - self.t - self.b).max(1.0)
    }
    pub fn right(&self) -> f64 {
        self.w - self.r
    }
    pub fn bottom(&self) -> f64 {
        self.h - self.b
    }
    /// Band scale: width of one of `n` equal bands.
    pub fn band_w(&self, n: usize) -> f64 {
        self.iw() / (n.max(1) as f64)
    }
    /// Band scale: left edge of band `i`.
    pub fn band_x(&self, i: usize, n: usize) -> f64 {
        self.l + (i as f64) * self.band_w(n)
    }
    /// Band scale: centre of band `i` — where a line's vertex sits.
    pub fn band_mid(&self, i: usize, n: usize) -> f64 {
        self.band_x(i, n) + self.band_w(n) / 2.0
    }
    /// Point scale: `i` of `n` spread edge-to-edge (cumulative lines).
    pub fn lin_x(&self, i: usize, n: usize) -> f64 {
        if n <= 1 {
            self.l
        } else {
            self.l + (i as f64) / ((n - 1) as f64) * self.iw()
        }
    }
    /// Linear y for a value against `max`, measured up from the baseline.
    pub fn y(&self, v: f64, max: f64) -> f64 {
        if max <= 0.0 {
            self.t + self.ih()
        } else {
            self.t + self.ih() - (v / max) * self.ih()
        }
    }
    /// Column width: `frac` of the band, capped at 24px (the mark spec).
    pub fn bar_w(&self, n: usize, frac: f64) -> f64 {
        (self.band_w(n) * frac).clamp(1.0, 24.0)
    }
    pub fn view_box(&self) -> String {
        format!("0 0 {:.0} {:.0}", self.w, self.h)
    }
}

/// Human tick steps (1 / 2 / 2.5 / 5 × 10ⁿ) from 0 to `max`, aiming for `n`
/// intervals. Never invents a cap: the last tick covers `max`.
pub fn nice_ticks(max: f64, n: usize) -> Vec<f64> {
    if !max.is_finite() || max <= 0.0 || n == 0 {
        return vec![0.0];
    }
    let raw = max / (n as f64);
    let mag = 10f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let step = if norm <= 1.0 {
        1.0
    } else if norm <= 2.0 {
        2.0
    } else if norm <= 2.5 {
        2.5
    } else if norm <= 5.0 {
        5.0
    } else {
        10.0
    } * mag;
    let mut out = Vec::new();
    let mut v = 0.0;
    while v <= max * 1.0001 && out.len() < 32 {
        out.push(v);
        v += step;
    }
    out
}

// ---------------------------------------------------------------------------
// Path builders
// ---------------------------------------------------------------------------

/// Column with a 4px rounded **data end** at the top and square roots — the
/// rounding marks where the value is, so it only ever goes on the top segment
/// of a stack.
pub fn top_round(x: f64, y: f64, w: f64, h: f64, r: f64) -> String {
    let r = r.min(w / 2.0).min(h).max(0.0);
    format!(
        "M{x:.2},{:.2}L{x:.2},{:.2}Q{x:.2},{y:.2} {:.2},{y:.2}L{:.2},{y:.2}Q{:.2},{y:.2} {:.2},{:.2}L{:.2},{:.2}Z",
        y + h,
        y + r,
        x + r,
        x + w - r,
        x + w,
        x + w,
        y + r,
        x + w,
        y + h,
    )
}

/// Column hanging below a zero rule: rounded data end at the bottom.
pub fn bot_round(x: f64, y: f64, w: f64, h: f64, r: f64) -> String {
    let r = r.min(w / 2.0).min(h).max(0.0);
    format!(
        "M{x:.2},{y:.2}L{:.2},{y:.2}L{:.2},{:.2}Q{:.2},{:.2} {:.2},{:.2}L{:.2},{:.2}Q{x:.2},{:.2} {x:.2},{:.2}Z",
        x + w,
        x + w,
        y + h - r,
        x + w,
        y + h,
        x + w - r,
        y + h,
        x + r,
        y + h,
        y + h,
        y + h - r,
    )
}

/// A plain rectangle as a path, so a stack's middle segments and its rounded
/// top can share one element type.
pub fn rect_path(x: f64, y: f64, w: f64, h: f64) -> String {
    format!("M{x:.2},{y:.2}h{w:.2}v{h:.2}h{:.2}Z", -w)
}

/// Polyline through `pts`.
pub fn line_path(pts: &[(f64, f64)]) -> String {
    if pts.is_empty() {
        return String::new();
    }
    let mut s = String::with_capacity(pts.len() * 14);
    for (i, (x, y)) in pts.iter().enumerate() {
        s.push(if i == 0 { 'M' } else { 'L' });
        s.push_str(&format!("{x:.2},{y:.2}"));
    }
    s
}

/// Line closed down to `base_y` — the 10%-opacity wash under a line.
pub fn area_path(pts: &[(f64, f64)], base_y: f64) -> String {
    if pts.is_empty() {
        return String::new();
    }
    let mut s = line_path(pts);
    let last = pts[pts.len() - 1].0;
    let first = pts[0].0;
    s.push_str(&format!("L{last:.2},{base_y:.2}L{first:.2},{base_y:.2}Z"));
    s
}

/// Split a gappy series into runs of consecutive present points.
///
/// A bucket with no successful request has no percentile, and joining across
/// it would draw a line through a value that was never measured. One path per
/// run, so a gap stays a gap.
pub fn line_runs(pts: &[Option<(f64, f64)>]) -> Vec<Vec<(f64, f64)>> {
    let mut out: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut run: Vec<(f64, f64)> = Vec::new();
    for p in pts {
        match p {
            Some(p) => run.push(*p),
            None => {
                if !run.is_empty() {
                    out.push(std::mem::take(&mut run));
                }
            }
        }
    }
    if !run.is_empty() {
        out.push(run);
    }
    out
}

/// Polyline length, for the `--len` the left-to-right draw animation needs:
/// `getTotalLength()` wants an element that does not exist yet at build time,
/// and a polyline's length is exact arithmetic anyway.
pub fn path_len(pts: &[(f64, f64)]) -> f64 {
    pts.windows(2)
        .map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt())
        .sum()
}

/// The region between two lines (a p50/p95-style band).
pub fn band_path(upper: &[(f64, f64)], lower: &[(f64, f64)]) -> String {
    if upper.is_empty() || lower.is_empty() {
        return String::new();
    }
    let mut s = line_path(upper);
    for (x, y) in lower.iter().rev() {
        s.push_str(&format!("L{x:.2},{y:.2}"));
    }
    s.push('Z');
    s
}

// ---------------------------------------------------------------------------
// Axes
// ---------------------------------------------------------------------------

/// Ticks for a **count** axis: the step never goes fractional, so an axis whose
/// max is 1 does not label two gridlines "1".
pub fn nice_ticks_int(max: f64, n: usize) -> Vec<f64> {
    let t = nice_ticks(max, n);
    if t.len() < 2 || t[1] >= 1.0 {
        return t;
    }
    let mut out = Vec::new();
    let mut v = 0.0;
    while v <= max.max(1.0) && out.len() < 32 {
        out.push(v);
        v += 1.0;
    }
    out
}

/// Horizontal gridlines + left value labels. **Solid** hairlines: dashing
/// means projection everywhere on this page, so it can mean nothing else.
pub fn y_grid(p: &Plot, max: f64, n: usize, fmt: &dyn Fn(f64) -> String) -> Vec<AnyView> {
    y_grid_ticks(p, max, nice_ticks(max, n), fmt)
}

/// [`y_grid`] with the tick values chosen by the caller.
pub fn y_grid_ticks(
    p: &Plot,
    max: f64,
    ticks: Vec<f64>,
    fmt: &dyn Fn(f64) -> String,
) -> Vec<AnyView> {
    ticks
        .into_iter()
        .map(|v| {
            let y = p.y(v, max);
            let label = fmt(v);
            view! {
                <line x1=p.l x2=p.right() y1=y y2=y class="gridline"></line>
                <text x=p.l - 8.0 y=y + 3.5 class="ax" text-anchor="end">
                    {label}
                </text>
            }
            .into_any()
        })
        .collect()
}

/// Band-centred x labels, every `every`-th band (and always the last).
pub fn x_labels(p: &Plot, labels: &[String], every: usize) -> Vec<AnyView> {
    let n = labels.len();
    let every = every.max(1);
    labels
        .iter()
        .enumerate()
        .filter(|(i, _)| i % every == 0 || *i == n - 1)
        .map(|(i, l)| {
            let x = p.band_mid(i, n);
            let l = l.clone();
            view! {
                <text x=x y=p.h - 8.0 class="ax" text-anchor="middle">
                    {l}
                </text>
            }
            .into_any()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Legend
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct LegendItem {
    pub color: String,
    pub label: String,
    /// Draw the swatch as a 2px rule rather than a square — lines get lines.
    pub line: bool,
    /// The swatch wears the exact fill of the mark it names. Where several
    /// series share one *meaning* — every refusal is amber — the opacity step
    /// that separates them in the chart has to be on the swatch too, or the
    /// legend stops being a key.
    pub opacity: f64,
    /// Optional click-through for this series. The legend is the keyboard path
    /// to a per-series link: chart segments are pointer-sized, and some of them
    /// are two pixels tall.
    pub href: Option<String>,
}

impl LegendItem {
    pub fn fill(color: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            color: color.into(),
            label: label.into(),
            line: false,
            opacity: 1.0,
            href: None,
        }
    }
    pub fn rule(color: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            color: color.into(),
            label: label.into(),
            line: true,
            opacity: 1.0,
            href: None,
        }
    }
    pub fn at(mut self, opacity: f64) -> Self {
        self.opacity = opacity;
        self
    }
    pub fn linking(mut self, href: impl Into<String>) -> Self {
        self.href = Some(href.into());
        self
    }
}

/// A legend whenever there are ≥2 series — the chart's identity channel.
/// Below that a direct label carries it, so one item renders nothing.
pub fn legend(items: Vec<LegendItem>) -> AnyView {
    if items.len() < 2 {
        return ().into_any();
    }
    let spans: Vec<AnyView> = items
        .into_iter()
        .map(|i| {
            let style = format!("background:{};opacity:{}", i.color, i.opacity);
            let cls = if i.line { "line" } else { "" };
            match i.href {
                Some(href) => view! {
                    <a class="legend-link" href=href>
                        <i class=cls style=style></i>
                        {i.label}
                    </a>
                }
                .into_any(),
                None => view! {
                    <span>
                        <i class=cls style=style></i>
                        {i.label}
                    </span>
                }
                .into_any(),
            }
        })
        .collect();
    view! { <div class="legend">{spans}</div> }.into_any()
}

// ---------------------------------------------------------------------------
// Crosshair + tooltip layer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Default)]
pub struct TipRow {
    pub color: String,
    pub label: String,
    pub value: String,
}

impl TipRow {
    pub fn new(
        color: impl Into<String>,
        label: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            color: color.into(),
            label: label.into(),
            value: value.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Tip {
    pub head: String,
    pub rows: Vec<TipRow>,
    /// A footer row set off by a rule — "total", "ratio", whatever sums.
    pub total: Option<(String, String)>,
}

impl Tip {
    pub fn new(head: impl Into<String>, rows: Vec<TipRow>) -> Self {
        Self {
            head: head.into(),
            rows,
            total: None,
        }
    }
    pub fn with_total(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.total = Some((label.into(), value.into()));
        self
    }
}

/// Handle on the page's single tooltip. Tooltips *enhance*: the `<details>`
/// table view under every chart is what guarantees no value is pointer-gated.
#[derive(Clone, Copy)]
pub struct Tips {
    state: RwSignal<Option<Tip>>,
    pos: RwSignal<(f64, f64)>,
}

impl Tips {
    pub fn show(&self, ev: &web_sys::MouseEvent, tip: Tip) {
        self.pos.set((ev.client_x() as f64, ev.client_y() as f64));
        // mousemove fires per pixel; only re-render when the content changed.
        if self.state.with_untracked(|s| s.as_ref() != Some(&tip)) {
            self.state.set(Some(tip));
        }
    }
    pub fn hide(&self) {
        if self.state.with_untracked(|s| s.is_some()) {
            self.state.set(None);
        }
    }
}

pub fn provide_tips() -> Tips {
    let tips = Tips {
        state: RwSignal::new(None),
        pos: RwSignal::new((0.0, 0.0)),
    };
    provide_context(tips);
    tips
}

pub fn use_tips() -> Tips {
    expect_context::<Tips>()
}

/// The one floating tooltip element. Mount once per page.
#[component]
pub fn TipLayer() -> impl IntoView {
    let tips = use_tips();
    let node: NodeRef<leptos::html::Div> = NodeRef::new();
    // Placement runs after the content renders, so the flip near a viewport
    // edge is measured rather than guessed.
    Effect::new(move |_| {
        let (mx, my) = tips.pos.get();
        let on = tips.state.with(|s| s.is_some());
        let Some(el) = node.get() else { return };
        if !on {
            return;
        }
        let he: &web_sys::HtmlElement = JsCast::unchecked_ref(&el);
        let w = he.offset_width() as f64;
        let h = he.offset_height() as f64;
        let vw = window()
            .inner_width()
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(1280.0);
        let pad = 14.0;
        let mut x = mx + pad;
        let mut y = my - h - pad;
        if x + w > vw - 8.0 {
            x = (mx - w - pad).max(8.0);
        }
        if y < 8.0 {
            y = my + pad;
        }
        let style = he.style();
        let _ = style.set_property("left", &format!("{x:.0}px"));
        let _ = style.set_property("top", &format!("{y:.0}px"));
    });
    view! {
        <div node_ref=node class="tt" class:on=move || tips.state.with(|s| s.is_some())>
            {move || {
                tips.state
                    .get()
                    .map(|t| {
                        let rows: Vec<AnyView> = t
                            .rows
                            .into_iter()
                            .map(|r| {
                                let style = format!("background:{}", r.color);
                                view! {
                                    <div class="tt-r">
                                        <i style=style></i>
                                        <span>{r.label}</span>
                                        <b>{r.value}</b>
                                    </div>
                                }
                                    .into_any()
                            })
                            .collect();
                        let total = t
                            .total
                            .map(|(l, v)| {
                                view! {
                                    <div class="tt-r tt-tot">
                                        <span>{l}</span>
                                        <b>{v}</b>
                                    </div>
                                }
                                    .into_any()
                            });
                        view! {
                            <div class="tt-h">{t.head}</div>
                            {rows}
                            {total}
                        }
                    })
            }}
        </div>
    }
}

/// Invisible per-band hit targets, one `Tip` each, plus the hovered index for
/// the crosshair. Keyboard users get the same values from the table view.
pub fn hit_bands(
    p: &Plot,
    tips_data: Vec<Tip>,
    ctl: Tips,
    hover: RwSignal<Option<usize>>,
) -> Vec<AnyView> {
    let n = tips_data.len();
    let bw = p.band_w(n);
    tips_data
        .into_iter()
        .enumerate()
        .map(|(i, tip)| {
            let x = p.band_x(i, n);
            view! {
                <rect
                    x=x
                    y=p.t
                    width=bw
                    height=p.ih()
                    fill="transparent"
                    on:mousemove=move |ev: web_sys::MouseEvent| {
                        hover.set(Some(i));
                        ctl.show(&ev, tip.clone());
                    }
                    on:mouseleave=move |_| {
                        hover.set(None);
                        ctl.hide();
                    }
                ></rect>
            }
            .into_any()
        })
        .collect()
}

/// The vertical rule that follows the pointer across a time chart.
pub fn crosshair(p: &Plot, hover: RwSignal<Option<usize>>, n: usize) -> AnyView {
    let p = *p;
    view! {
        {move || {
            hover
                .get()
                .filter(|i| *i < n)
                .map(|i| {
                    let x = p.band_mid(i, n);
                    view! { <line x1=x x2=x y1=p.t y2=p.t + p.ih() class="crosshair"></line> }
                })
        }}
    }
    .into_any()
}

// ---------------------------------------------------------------------------
// Sparkline
// ---------------------------------------------------------------------------

/// Tile sparkline: recessive history, the last quarter in the series colour,
/// one ≥8px-equivalent end marker with a 2px surface ring.
pub fn sparkline(vals: &[f64], w: f64, h: f64, color: &str) -> AnyView {
    if vals.len() < 2 {
        return view! { <svg width=w height=h class="spark"></svg> }.into_any();
    }
    let max = vals.iter().cloned().fold(f64::MIN, f64::max);
    let min = vals.iter().cloned().fold(f64::MAX, f64::min);
    let span = if (max - min).abs() < f64::EPSILON {
        1.0
    } else {
        max - min
    };
    let pts: Vec<(f64, f64)> = vals
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                2.0 + (i as f64) * (w - 4.0) / ((vals.len() - 1) as f64),
                h - 2.0 - ((v - min) / span) * (h - 5.0),
            )
        })
        .collect();
    let cut = ((vals.len() as f64) * 0.75) as usize;
    let tail = line_path(&pts[cut.min(pts.len() - 2)..]);
    let all = line_path(&pts);
    let (lx, ly) = pts[pts.len() - 1];
    view! {
        <svg width=w height=h class="spark" viewBox=format!("0 0 {w:.0} {h:.0}")>
            <path
                d=all
                fill="none"
                stroke="var(--text-3)"
                stroke-width="1.5"
                stroke-linejoin="round"
                stroke-linecap="round"
            ></path>
            <path
                d=tail
                fill="none"
                stroke=color.to_string()
                stroke-width="2"
                stroke-linejoin="round"
                stroke-linecap="round"
            ></path>
            <circle
                cx=lx
                cy=ly
                r="3"
                fill=color.to_string()
                stroke="var(--surface)"
                stroke-width="2"
            ></circle>
        </svg>
    }
    .into_any()
}

// ---------------------------------------------------------------------------
// Motion (§6.4) and layout measurement
// ---------------------------------------------------------------------------

/// Every animation on this page is off under `prefers-reduced-motion: reduce`.
/// CSS handles the declarative ones; this is for the two that are driven from
/// Rust (the tile count-up and the entry stagger).
pub fn reduced_motion() -> bool {
    window()
        .match_media("(prefers-reduced-motion: reduce)")
        .ok()
        .flatten()
        .map(|m| m.matches())
        .unwrap_or(false)
}

/// Interpolate a tile figure toward its new value over ~500 ms. Returns the
/// target untouched under reduced motion.
pub fn count_up(target: Signal<f64>) -> Signal<f64> {
    if reduced_motion() {
        return target;
    }
    let cur = RwSignal::new(0.0_f64);
    let handle = set_interval_with_handle(
        move || {
            let t = target.get_untracked();
            if !t.is_finite() {
                return;
            }
            let c = cur.get_untracked();
            let d = t - c;
            // Converges in ~30 frames (0.84³⁰ ≈ 0.005), i.e. about half a second.
            if d.abs() <= (t.abs() * 0.0015).max(1e-9) {
                if c != t {
                    cur.set(t);
                }
                return;
            }
            cur.set(c + d * 0.16);
        },
        Duration::from_millis(16),
    )
    .ok();
    on_cleanup(move || {
        if let Some(h) = handle {
            h.clear();
        }
    });
    cur.into()
}

/// The content-box size of an element, kept current by a `ResizeObserver`.
///
/// A chart has to follow its own card, not the window: the card also changes
/// width when the sidebar folds to a rail, when the page body grows a
/// scrollbar, or when a container query moves the card to another column —
/// none of which is a window resize. Several observations in one frame land
/// as one update; the observer is disconnected with the owner.
pub fn use_element_size(node: NodeRef<leptos::html::Div>) -> Signal<(f64, f64)> {
    use leptos::wasm_bindgen::closure::Closure;
    use std::cell::Cell;
    use std::rc::Rc;

    let size = RwSignal::new((0.0_f64, 0.0_f64));
    Effect::new(move |_| {
        let Some(el) = node.get() else { return };
        let latest = Rc::new(Cell::new(None::<(f64, f64)>));
        let cb = Closure::<dyn FnMut(js_sys::Array)>::new(move |entries: js_sys::Array| {
            let Some(entry) = entries.iter().last() else {
                return;
            };
            let rect = entry
                .unchecked_into::<web_sys::ResizeObserverEntry>()
                .content_rect();
            // Only the first observation of a frame schedules the write; the
            // later ones just replace what it will write.
            if latest
                .replace(Some((rect.width(), rect.height())))
                .is_none()
            {
                let latest = latest.clone();
                request_animation_frame(move || {
                    let Some(v) = latest.take() else { return };
                    // `None` once the owner is gone: a frame can outlive it.
                    if size.try_get_untracked().is_some_and(|s| s != v) {
                        size.try_set(v);
                    }
                });
            }
        });
        let Ok(observer) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) else {
            return;
        };
        observer.observe(&el);
        // A local arena slot: the cleanup must be `Send`, the JS handles are
        // not. Cleanups run before the owner's slots are freed.
        let held = StoredValue::new_local((observer, cb));
        on_cleanup(move || held.with_value(|(o, _)| o.disconnect()));
    });
    size.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_series_keeps_its_slot_while_it_is_shown() {
        let mut p = SlotPool::default();
        let a = p.assign(&keys(&["a", "b", "c"]));
        assert_eq!((a["a"], a["b"], a["c"]), (0, 1, 2));
        // A filter drops b: the survivors are not repainted.
        let b = p.assign(&keys(&["a", "c"]));
        assert_eq!((b["a"], b["c"]), (0, 2));
        // Rank order changes: still no repaint.
        let c = p.assign(&keys(&["c", "a"]));
        assert_eq!((c["a"], c["c"]), (0, 2));
    }

    #[test]
    fn a_newcomer_takes_a_slot_nobody_held_before_one_given_up() {
        let mut p = SlotPool::default();
        p.assign(&keys(&["a", "b"]));
        // b leaves the screen; d arrives. Slot 1 is b's to come back to while
        // a never-used slot is free.
        let m = p.assign(&keys(&["a", "d"]));
        assert_eq!(m["d"], 2);
        let back = p.assign(&keys(&["a", "b", "d"]));
        assert_eq!((back["a"], back["b"], back["d"]), (0, 1, 2));
    }

    #[test]
    fn slots_are_recycled_only_from_series_off_screen_and_never_shared() {
        let mut p = SlotPool::default();
        p.assign(&keys(&["a", "b", "c", "d", "e", "f"]));
        let m = p.assign(&keys(&["a", "b", "c", "d", "e", "g"]));
        // f is gone from the screen: g takes its slot, nobody else moves.
        assert_eq!(m["g"], 5);
        assert_eq!(m["a"], 0);
        let mut seen: Vec<u8> = m.values().copied().collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 6, "no two shown series share a colour");
        // f returns while g is still shown: f cannot take g's slot.
        let back = p.assign(&keys(&["f", "g"]));
        assert_ne!(back["f"], back["g"]);
        assert_eq!(back["g"], 5);
    }

    #[test]
    fn a_seventh_shown_series_gets_no_slot() {
        let mut p = SlotPool::default();
        let m = p.assign(&keys(&["a", "b", "c", "d", "e", "f", "g"]));
        assert_eq!(m.len(), SLOTS);
        assert!(!m.contains_key("g"));
    }

    #[test]
    fn chart_height_follows_the_width_within_bounds() {
        assert_eq!(chart_height(300.0), 180.0);
        assert_eq!(chart_height(800.0), 240.0);
        assert_eq!(chart_height(2000.0), 360.0);
    }
}
