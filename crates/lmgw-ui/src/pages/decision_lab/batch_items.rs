//! A set run's items (labs-store design §3.3, §3.5): a table in item order,
//! read in pages from `/decision-lab/api/batches/{id}/items` (`after`,
//! `limit`, `next_after`). Each row: the item's place, its key, its state
//! (cut to one line, the whole of it a click away), each question's answer
//! and confidence with the expected answer and whether it is correct, and
//! the run's status. A row opened shows the state whole and the run in full.
//!
//! A page is a transport unit, not a cap: the table reads the next one when
//! its end scrolls into view, and says how many are left until none are.
//!
//! While the run runs, a progress frame re-reads only the loaded pages that
//! hold the first unsettled items (no run yet, or a failed one while the job
//! retries the failed). The window is sized from the frame's own counts: the
//! change of the job's done, failed and unreadable counts since the last
//! frame read, plus its concurrency. The job takes its items in item order
//! and at most `concurrency` are in flight, so every item that landed since
//! is among that many first unsettled ones (a retried item that fails again
//! lands without moving a count; it is read when the window passes it or at
//! the job's end). The job's end reads every loaded page once. One read
//! is in flight at a time; a frame meanwhile asks for one more. Each row
//! holds its item in a signal of its own, found by its id, so a re-read item
//! redraws its row alone. A read that fails is said once, with Retry, and
//! the frames stop asking until it is pressed.

use std::collections::{BTreeSet, HashMap};

use leptos::html;
use leptos::prelude::*;
use lmgw_api_types::decision_lab::metrics::is_2xx;
use lmgw_api_types::decision_lab::{BatchItemPage, DecisionForm, QuestionDraft};
use lmgw_api_types::lab_store::LabGroupView;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use super::batch_view::RunPage;
use super::item_cells::{cell, expected_only, Cell, ReadRun};
use super::run_view::RunView;
use crate::api;
use crate::fmt::grouped;
use crate::scope::Scope;

/// How many items one read asks for. The server sets no maximum; the table
/// reads on while items are left.
pub(super) const PAGE: u32 = 50;

/// The next page's cursor: the last item read, while items are left. The
/// items are numbered from 0 without gaps, so what is loaded says it.
pub(super) fn next_after(last_ix: Option<u64>, loaded: usize, total: u64) -> Option<u64> {
    if (loaded as u64) >= total {
        return None;
    }
    last_ix
}

/// How an item's run stands for the frames: none yet, failed, or final.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Standing {
    NoRun,
    Failed,
    Settled,
}

pub(super) fn standing(g: &LabGroupView) -> Standing {
    match g.runs.first() {
        None => Standing::NoRun,
        Some(r) if !is_2xx(i64::from(r.http_status)) => Standing::Failed,
        Some(_) => Standing::Settled,
    }
}

/// The loaded pages a progress frame re-reads: those that hold the first
/// `window` of `unsettled` (item indices, lowest first), each once.
pub(super) fn frame_pages(unsettled: impl Iterator<Item = u64>, window: usize) -> Vec<u64> {
    let mut pages: Vec<u64> = unsettled
        .take(window)
        .map(|ix| ix / u64::from(PAGE))
        .collect();
    pages.dedup();
    pages
}

/// The counts a frame reports that settle items: done, failed, unreadable.
pub(super) type Counts = [u64; 3];

/// How many of the first unsettled items a frame may find changed: every
/// count that moved since the last read is an item that landed, and the
/// job has `concurrency` more in flight.
pub(super) fn frame_window(now: Counts, at_last_read: Counts, concurrency: u32) -> usize {
    let moved: u64 = now
        .iter()
        .zip(at_last_read)
        .map(|(n, a)| n.abs_diff(a))
        .sum();
    usize::try_from(moved)
        .unwrap_or(usize::MAX)
        .saturating_add(concurrency.max(1) as usize)
}

/// `pages` (ascending) as reads, consecutive pages one read: each its
/// `after` cursor and its `limit`.
pub(super) fn page_reads(pages: &[u64]) -> Vec<(Option<u64>, u32)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &p in pages {
        match out.last_mut() {
            Some((_, last)) if *last + 1 == p => *last = p,
            _ => out.push((p, p)),
        }
    }
    out.into_iter()
        .map(|(first, last)| {
            let after = (first > 0).then(|| first * u64::from(PAGE) - 1);
            let n = (last - first + 1) * u64::from(PAGE);
            (after, u32::try_from(n).unwrap_or(u32::MAX))
        })
        .collect()
}

/// One row of the table: its item in a signal of its own.
#[derive(Clone)]
pub(super) struct Slot {
    pub id: i64,
    pub ix: u64,
    pub group: ArcRwSignal<LabGroupView>,
}

/// What a read covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Read {
    /// Every loaded item from the first (at least a page): the first read,
    /// the job's end, Retry.
    All,
    /// A progress frame's pages.
    Frame {
        retry_failed: bool,
        concurrency: u32,
    },
}

impl Read {
    /// Two asks while a read is in flight as the one that follows it.
    fn and(self, later: Read) -> Read {
        if self == Read::All || later == Read::All {
            Read::All
        } else {
            later
        }
    }
}

/// The run's items as read so far. `Copy`.
#[derive(Clone, Copy)]
pub(super) struct Items {
    batch: i64,
    /// In item order.
    pub rows: RwSignal<Vec<Slot>>,
    /// An item's place in `rows`, by its id.
    at: StoredValue<HashMap<i64, usize>>,
    /// The loaded items without a run, and with a failed one, by index.
    no_run: StoredValue<BTreeSet<u64>>,
    failed_run: StoredValue<BTreeSet<u64>>,
    pub total: RwSignal<u64>,
    /// The first read has not landed.
    pub loading: RwSignal<bool>,
    /// Why the last read of loaded pages failed: said once, with Retry.
    pub failed: RwSignal<Option<String>>,
    /// The next page is on its way.
    pub more: RwSignal<bool>,
    /// Why the last next page did not load: the table stops reading on.
    pub more_failed: RwSignal<Option<String>>,
    /// The rows opened, by item index.
    pub open: RwSignal<Vec<u64>>,
    /// A read is on its way, and what was asked for meanwhile.
    busy: StoredValue<bool>,
    again: StoredValue<Option<Read>>,
    /// The counts of the latest frame, and of the frame the last read
    /// covered.
    latest: StoredValue<Counts>,
    read_at: StoredValue<Counts>,
    scope: Scope,
}

impl Items {
    pub fn new(batch: i64, scope: Scope) -> Self {
        Self {
            batch,
            rows: RwSignal::new(Vec::new()),
            at: StoredValue::new(HashMap::new()),
            no_run: StoredValue::new(BTreeSet::new()),
            failed_run: StoredValue::new(BTreeSet::new()),
            total: RwSignal::new(0),
            loading: RwSignal::new(true),
            failed: RwSignal::new(None),
            more: RwSignal::new(false),
            more_failed: RwSignal::new(None),
            open: RwSignal::new(Vec::new()),
            busy: StoredValue::new(false),
            again: StoredValue::new(None),
            latest: StoredValue::new([0; 3]),
            read_at: StoredValue::new([0; 3]),
            scope,
        }
    }

    fn href(self, after: Option<u64>, limit: u32) -> String {
        let mut h = format!(
            "/decision-lab/api/batches/{}/items?limit={limit}",
            self.batch
        );
        if let Some(a) = after {
            h.push_str(&format!("&after={a}"));
        }
        h
    }

    /// A page into the rows: an item read again updates its own row (when
    /// it changed), a new one gets a row in item order.
    fn land(self, p: BatchItemPage) {
        self.total.set(p.total);
        let mut added: Vec<Slot> = Vec::new();
        for g in p.items {
            let ix = g.item.as_ref().map_or(u64::MAX, |i| i.item_ix);
            let st = standing(&g);
            self.no_run.update_value(|s| {
                if st == Standing::NoRun {
                    s.insert(ix);
                } else {
                    s.remove(&ix);
                }
            });
            self.failed_run.update_value(|s| {
                if st == Standing::Failed {
                    s.insert(ix);
                } else {
                    s.remove(&ix);
                }
            });
            let place = self.at.with_value(|at| at.get(&g.id).copied());
            let slot = place.and_then(|i| self.rows.with_untracked(|r| r.get(i).cloned()));
            match slot {
                Some(slot) => {
                    if slot.group.with_untracked(|cur| *cur != g) {
                        slot.group.set(g);
                    }
                }
                None => added.push(Slot {
                    id: g.id,
                    ix,
                    group: ArcRwSignal::new(g),
                }),
            }
        }
        if added.is_empty() {
            return;
        }
        self.rows.update(|rows| {
            let in_order = rows
                .last()
                .zip(added.first())
                .is_none_or(|(l, a)| l.ix < a.ix);
            rows.extend(added);
            if !in_order {
                rows.sort_by_key(|s| s.ix);
            }
            self.at.update_value(|at| {
                at.clear();
                at.extend(rows.iter().enumerate().map(|(i, s)| (s.id, i)));
            });
        });
    }

    /// The loaded items a frame may find changed, lowest first.
    fn unsettled(self, retry_failed: bool) -> Vec<u64> {
        self.no_run.with_value(|no_run| {
            if retry_failed {
                self.failed_run
                    .with_value(|f| no_run.union(f).copied().collect())
            } else {
                no_run.iter().copied().collect()
            }
        })
    }

    /// The first page.
    pub fn first(self) {
        self.read(Read::All);
    }

    /// Every loaded item read again: the job's end, Retry.
    pub fn refresh(self) {
        self.read(Read::All);
    }

    /// A progress frame of a job that runs `concurrency` items at once and
    /// reports `counts`: the pages that hold the first unsettled items, as
    /// many as the counts moved since the last read plus `concurrency`.
    /// Nothing while a read has failed: Retry asks again.
    pub fn on_frame(self, retry_failed: bool, concurrency: u32, counts: Counts) {
        self.latest.set_value(counts);
        if self.failed.with_untracked(Option::is_some) {
            return;
        }
        self.read(Read::Frame {
            retry_failed,
            concurrency,
        });
    }

    fn read(self, want: Read) {
        if self.busy.try_get_value() == Some(true) {
            self.again
                .try_update_value(|a| *a = Some(a.map_or(want, |x| x.and(want))));
            return;
        }
        self.busy.try_set_value(true);
        self.scope.spawn(async move {
            let mut want = want;
            loop {
                // The counts this read answers for.
                let counts = self.latest.get_value();
                let last = self.read_at.get_value();
                self.read_at.set_value(counts);
                let reads = match want {
                    Read::All => {
                        let n = (self.rows.with_untracked(Vec::len) as u32).max(PAGE);
                        vec![(None, n)]
                    }
                    Read::Frame {
                        retry_failed,
                        concurrency,
                    } => page_reads(&frame_pages(
                        self.unsettled(retry_failed).into_iter(),
                        frame_window(counts, last, concurrency),
                    )),
                };
                let mut err = None;
                for (after, limit) in reads {
                    match api::get::<BatchItemPage>(self.href(after, limit)).await {
                        Ok(p) => self.land(p),
                        Err(e) => {
                            err = Some(e.to_string());
                            break;
                        }
                    }
                }
                let failed = err.is_some();
                self.failed.set(err);
                self.loading.set(false);
                match self.again.try_update_value(Option::take).flatten() {
                    // After a failure only a whole read follows.
                    Some(next) if !failed || next == Read::All => want = next,
                    _ => break,
                }
            }
            self.busy.set_value(false);
        });
    }

    /// The next page, when items are left and none is on its way (also
    /// Retry after a failed one).
    pub fn load_more(self) {
        if self.more.get_untracked() {
            return;
        }
        let Some(after) = self.rows.with_untracked(|r| {
            next_after(r.last().map(|s| s.ix), r.len(), self.total.get_untracked())
        }) else {
            return;
        };
        self.more.set(true);
        self.more_failed.set(None);
        self.scope.spawn(async move {
            match api::get::<BatchItemPage>(self.href(Some(after), PAGE)).await {
                Ok(p) => self.land(p),
                Err(e) => self.more_failed.set(Some(e.to_string())),
            }
            self.more.set(false);
        });
    }

    pub fn left(self) -> u64 {
        self.total
            .get()
            .saturating_sub(self.rows.with(Vec::len) as u64)
    }

    pub fn toggle(self, ix: u64) {
        self.open.update(|v| {
            if let Some(i) = v.iter().position(|x| *x == ix) {
                v.remove(i);
            } else {
                v.push(ix);
            }
        });
    }
}

/// An item's state, as its form holds it.
fn state_of(g: &LabGroupView) -> String {
    serde_json::from_value::<DecisionForm>(g.form.clone())
        .map(|f| f.state)
        .unwrap_or_default()
}

/// One line of a state for its cell: the whitespace runs made one space.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[component]
pub(super) fn ItemsCard(page: RunPage) -> impl IntoView {
    let items = page.items;
    let questions = Memo::new(move |_| {
        page.view
            .with(|v| v.as_ref().map(|v| v.set.questions.clone()))
            .unwrap_or_default()
    });
    let has_key = Memo::new(move |_| {
        page.view
            .with(|v| v.as_ref().is_some_and(|v| v.mapping.key.is_some()))
    });
    let sentinel: NodeRef<html::Div> = NodeRef::new();
    let seen = RwSignal::new(false);
    type Watch = (
        web_sys::IntersectionObserver,
        Closure<dyn FnMut(js_sys::Array)>,
    );
    let watch = StoredValue::new_local(None::<Watch>);
    Effect::new(move |_| {
        let Some(el) = sentinel.get() else { return };
        let cb = Closure::<dyn FnMut(js_sys::Array)>::new(move |entries: js_sys::Array| {
            let hit = entries
                .iter()
                .filter_map(|e| e.dyn_into::<web_sys::IntersectionObserverEntry>().ok())
                .any(|e| e.is_intersecting());
            seen.try_set(hit);
        });
        let Ok(io) = web_sys::IntersectionObserver::new(cb.as_ref().unchecked_ref()) else {
            return;
        };
        io.observe(&el);
        watch.try_set_value(Some((io, cb)));
        on_cleanup(move || {
            if let Some((io, _cb)) = watch.try_update_value(Option::take).flatten() {
                io.disconnect();
            }
        });
    });
    // Rows that landed move the sentinel: what the observer last saw is
    // stale (an empty table's end is in view, the first page's is not), so
    // it looks again rather than a stale reading loading a page nobody
    // scrolled to.
    Effect::new(move |prev: Option<usize>| {
        let n = items.rows.with(Vec::len);
        if prev.is_some_and(|p| p != n) {
            seen.set(false);
            if let Some(el) = sentinel.get_untracked() {
                watch.with_value(|w| {
                    if let Some((io, _)) = w {
                        io.unobserve(&el);
                        io.observe(&el);
                    }
                });
            }
        }
        n
    });
    Effect::new(move |_| {
        let failed = items.more_failed.with(Option::is_some);
        if seen.get() && !items.more.get() && !failed && untrack(|| items.left()) > 0 {
            items.load_more();
        }
    });

    view! {
        <div class="card dl-items-card">
            <div class="dl-card-head">
                <h3 class="lab-h3">"Items"</h3>
                <span class="count">{move || grouped(items.total.get())}</span>
                <span class="spacer"></span>
                <span class="mini-note dim">"in file order · a click opens an item"</span>
            </div>
            {move || items.failed.get().map(|e| {
                let what = if items.rows.with(Vec::is_empty) {
                    "The items did not load"
                } else {
                    "The items stopped updating"
                };
                view! {
                    <div class="notice err dl-items-failed">
                        {format!("⚠ {what}: {e}")}
                        " "
                        <button class="btn ghost sm" on:click=move |_| items.refresh()>"Retry"</button>
                    </div>
                }
            })}
            <div class="dl-items-wrap">
                <table class="data dl-items">
                    <thead>
                        <tr>
                            <th class="num-h">"#"</th>
                            <Show when=move || has_key.get()>
                                <th>"key"</th>
                            </Show>
                            <th>"state"</th>
                            <For each=move || questions.get() key=|q| q.id.clone() let:q>
                                <th title=format!("{}: the answer and its confidence; the expected answer, ✓ when it is the answer", q.id)>
                                    {q.id.clone()}
                                </th>
                            </For>
                            <th>"run"</th>
                        </tr>
                    </thead>
                    <tbody>
                        <For each=move || items.rows.get() key=|s| s.id let:s>
                            <ItemRow page=page row=s questions=questions has_key=has_key/>
                        </For>
                    </tbody>
                </table>
            </div>
            <Show when=move || items.loading.get() && items.rows.with(Vec::is_empty)>
                <div class="dim lh-loading">"Reading the items…"</div>
            </Show>
            <div class="lh-more dim" node_ref=sentinel>
                {move || {
                    if let Some(e) = items.more_failed.get() {
                        return Some(view! {
                            <span class="lh-older-failed">
                                {format!("couldn't load more items: {e}")}
                                " · "
                                <button class="btn ghost sm" on:click=move |_| items.load_more()>
                                    "Retry"
                                </button>
                            </span>
                        }.into_any());
                    }
                    let left = items.left();
                    (left > 0).then(|| view! {
                        <button
                            class="btn ghost sm"
                            disabled=move || items.more.get()
                            on:click=move |_| items.load_more()
                        >
                            {move || if items.more.get() {
                                format!("{} more · reading…", grouped(left))
                            } else {
                                format!("{} more · show them", grouped(left))
                            }}
                        </button>
                    }.into_any())
                }}
            </div>
        </div>
    }
}

/// One question's cell, the item's run read once for its row (`read`).
fn answer_cell(q: &QuestionDraft, g: &LabGroupView, read: &ReadRun) -> AnyView {
    let expected = g.item.as_ref().and_then(|i| i.expected.get(&q.id).cloned());
    let c = cell(q, read, expected.as_deref());
    let exp_line = |text: String, ok: Option<bool>| {
        let (cls, mark) = match ok {
            Some(true) => ("dl-exp ok", "✓ "),
            Some(false) => ("dl-exp bad", "✗ "),
            None => ("dl-exp", ""),
        };
        view! { <span class=cls title="the expected answer">{mark}{text}</span> }
    };
    match c {
        Cell::Pending => view! {
            <td class="dl-qcell dim">
                "—"
                {expected_only(q, expected.as_deref()).map(|t| exp_line(t, None))}
            </td>
        }
        .into_any(),
        Cell::Failed => view! {
            <td class="dl-qcell">
                <span class="dl-bad-word">"failed"</span>
                {expected_only(q, expected.as_deref()).map(|t| exp_line(t, None))}
            </td>
        }
        .into_any(),
        Cell::Unreadable => view! {
            <td class="dl-qcell">
                <span class="dl-warn-word" title="the run answered, but not this question in a form it reads">"unreadable"</span>
                {expected_only(q, expected.as_deref()).map(|t| exp_line(t, None))}
            </td>
        }
        .into_any(),
        Cell::Answered {
            text,
            confidence,
            expected,
        } => view! {
            <td class="dl-qcell">
                <span class="dl-ans">
                    <span class="m">{text}</span>
                    <span class="c" title="the confidence a threshold cuts on (noul: max(p, 1−p))">
                        {format!("{confidence:.2}")}
                    </span>
                </span>
                {expected.map(|(t, ok)| exp_line(t, Some(ok)))}
            </td>
        }
        .into_any(),
    }
}

#[component]
fn ItemRow(
    page: RunPage,
    row: Slot,
    questions: Memo<Vec<QuestionDraft>>,
    has_key: Memo<bool>,
) -> impl IntoView {
    let items = page.items;
    let ix = row.ix;
    let group = row.group;
    let open = Memo::new(move |_| items.open.with(|v| v.contains(&ix)));
    move || {
        let g = group.get();
        let key = g
            .item
            .as_ref()
            .and_then(|i| i.key.clone())
            .unwrap_or_default();
        let state = state_of(&g);
        let line = one_line(&state);
        let open = open.get();
        let run = g.runs.first().cloned();
        let read = ReadRun::of(run.as_ref());
        let status = match &run {
            None => view! { <span class="dim">"—"</span> }.into_any(),
            Some(r) if is_2xx(i64::from(r.http_status)) => view! {
                <span class="mono-sm">{format!("{} · {} ms", r.http_status, grouped(r.latency_ms))}</span>
            }
            .into_any(),
            Some(r) => view! {
                <span class="mono-sm dl-bad-word" title=r.error_message.clone().unwrap_or_default()>
                    {super::run_view::failure_line(r)}
                </span>
            }
            .into_any(),
        };
        let cols = questions.with(|qs| qs.len()) + 3 + usize::from(has_key.get());
        let qcells =
            questions.with(|qs| qs.iter().map(|q| answer_cell(q, &g, &read)).collect_view());
        view! {
            <tr class="pickable" class:picked=open on:click=move |_| items.toggle(ix)>
                <td class="num">{(ix + 1).to_string()}</td>
                {has_key.get().then(|| view! { <td class="mono-sm dl-key">{key.clone()}</td> })}
                <td class="clip dl-state" title=line.clone()>{line.clone()}</td>
                {qcells}
                <td>{status}</td>
            </tr>
            {open.then(|| view! {
                <tr class="detail-row dl-item-open">
                    <td colspan=cols.to_string()>
                        <div class="dl-item-detail">
                            <div class="dl-item-state">
                                <h4 class="lab-h3">"State"</h4>
                                <pre class="preset dl-pre">{state.clone()}</pre>
                            </div>
                            <div class="dl-item-run">
                                {match run.clone() {
                                    Some(r) => {
                                        let note: Option<String> = None;
                                        view! { <RunView run=r note=note/> }.into_any()
                                    }
                                    None => view! { <p class="mini-note dim">"no run yet"</p> }.into_any(),
                                }}
                            </div>
                        </div>
                    </td>
                </tr>
            })}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(ix: u64, status: Option<u16>) -> LabGroupView {
        let runs: Vec<serde_json::Value> = status
            .map(|s| {
                serde_json::json!({
                    "id": 9, "group_id": 1, "cell": 0, "variant": "as_written",
                    "endpoint": "/v1/systemone", "model_asked": "m", "answered_by": "m",
                    "fallback_reason": null, "upstream_model": null, "http_status": s,
                    "error_code": null, "error_message": null, "request": "{}",
                    "response": null, "response_type": null, "headers": {},
                    "input_tokens": null, "output_tokens": null, "latency_ms": 1,
                    "created_at": "", "artifacts": []
                })
            })
            .into_iter()
            .collect();
        serde_json::from_value(serde_json::json!({
            "id": 100 + ix, "lab": "decision", "title": "t", "form": null,
            "plan": [{"model": "m", "variant": "as_written"}], "override": null,
            "created_at": "", "runs": runs, "artifacts": [],
            "item": {"batch_id": 1, "item_ix": ix, "key": null, "expected": {}}
        }))
        .unwrap()
    }

    #[test]
    fn a_run_is_unsettled_without_a_run_or_failed_and_settled_on_any_2xx() {
        assert_eq!(standing(&item(0, None)), Standing::NoRun);
        assert_eq!(standing(&item(0, Some(500))), Standing::Failed);
        assert_eq!(standing(&item(0, Some(200))), Standing::Settled);
        assert_eq!(standing(&item(0, Some(203))), Standing::Settled);
    }

    #[test]
    fn a_frame_reads_the_pages_of_the_first_unsettled_items_only() {
        // Items 0 to 249 loaded, 0 to 117 answered: a job of 2 at once reads
        // the page of 118 and the next (the window of 52 reaches 169).
        let window = frame_window([2, 0, 0], [2, 0, 0], 2);
        assert_eq!(window, 2);
        let window = window + PAGE as usize;
        let pages = frame_pages(118..250, window);
        assert_eq!(pages, [2, 3]);
        assert_eq!(page_reads(&pages), [(Some(99), 100)]);
        // The first page is read from the start.
        assert_eq!(page_reads(&frame_pages(0..250, window)), [(None, 100)]);
        // Scattered failures under retry: only their pages, apart.
        assert_eq!(
            page_reads(&frame_pages([3, 4, 120, 230].into_iter(), window)),
            [(None, 50), (Some(99), 50), (Some(199), 50)]
        );
        assert!(frame_pages(std::iter::empty(), window).is_empty());
    }

    #[test]
    fn the_window_covers_every_item_that_landed_since_the_last_read() {
        // 300 answered since the last read (40 of them failed) at 8 at once.
        assert_eq!(frame_window([500, 90, 3], [200, 50, 3], 8), 340 + 8);
        // Nothing moved: only what is in flight.
        assert_eq!(frame_window([5, 0, 0], [5, 0, 0], 0), 1);
        // A retried failure that now answers moves failed down, done not.
        assert_eq!(frame_window([10, 2, 0], [10, 3, 0], 4), 1 + 4);
    }

    #[test]
    fn the_next_page_follows_the_last_item_while_items_are_left() {
        assert_eq!(next_after(Some(49), 50, 120), Some(49));
        assert_eq!(next_after(Some(119), 120, 120), None);
        assert_eq!(next_after(None, 0, 30), None);
        assert_eq!(
            Read::Frame {
                retry_failed: false,
                concurrency: 1
            }
            .and(Read::All),
            Read::All
        );
    }

    #[test]
    fn a_state_is_cut_to_one_line_for_its_cell() {
        assert_eq!(one_line("Hi,\n  charged\ttwice "), "Hi, charged twice");
    }
}
