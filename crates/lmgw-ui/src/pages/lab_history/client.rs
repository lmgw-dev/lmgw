//! One lab's kept history and the calls behind it (labs-store design §1.5,
//! §1.6, §2.9): the groups read in pages, newest first, with the total and
//! how many are older; a new group put in front as soon as it is created and
//! its runs filled in as its cells land; delete, Clear, the usage and the
//! orphan files; the export hrefs.
//!
//! A page is a transport unit, not a cap: the list reads the next one when
//! its end scrolls into view, and says how many are left until none are. A
//! page that failed stops that: the list's end says why, with Retry, until
//! Retry or a new list.
//!
//! Every read that replaces the list (a reload, a filter change, a delete,
//! Clear) starts a new **generation**; an older page asked for under another
//! generation, or from another end of the list, is dropped when it lands.
//! The usage reads take tickets the same way: the newest one asked wins.
//!
//! A group **in flight** (a cell of it is being answered, its run not in
//! yet) is read again until it is not ([`LabHistory::settle`]): a page read
//! that brings one follows it by itself, so a reload during a request shows
//! the group in flight and then its run, never as stopped. A lab whose page
//! stopped its own request settles that group the same way, so the stopped
//! run lands with what it kept. What the page knows about a group with no
//! run that the store does not (it was not kept, and why) is its **note**.

use std::collections::{BTreeMap, BTreeSet};

use leptos::prelude::*;
use lmgw_api_types::lab_store::{
    LabExportFormat, LabGroupPage, LabGroupView, LabKind, LabRunView, LabUsage,
};

use crate::api;
use crate::scope::{Latest, Scope};
use crate::widgets::{use_toasts, Toasts};

/// How many groups one read of the history asks for. The server sets no
/// maximum; the list reads on while older groups are left.
pub(super) const PAGE: u32 = 20;

/// How long a group in flight waits between two reads of it: a pace, not a
/// bound. The reads go on until its run is in or the page is left.
const SETTLE_PACE_MS: i32 = 250;

/// A pause of `ms` milliseconds, on the browser's timer.
async fn pause(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(w) = web_sys::window() {
            let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// What a lab's card shows for a group with no run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::pages) enum Runless {
    /// This page's own request is out: its live view, with Stop.
    Live,
    /// A request is out for it, not this page's (another tab's, or one a
    /// reload found): in flight, its run on its way.
    InFlight,
    /// This page knows it was not kept, and why.
    NotKept(String),
    /// Its request was stopped before anything came back.
    Stopped,
}

/// [`Runless`] from what the page and the store know.
pub(in crate::pages) fn runless(live: bool, in_flight: bool, note: Option<&str>) -> Runless {
    if live {
        Runless::Live
    } else if in_flight {
        Runless::InFlight
    } else if let Some(n) = note {
        Runless::NotKept(n.to_string())
    } else {
        Runless::Stopped
    }
}

/// `g` in front of `v`, or in its place when `v` has it already: whether it
/// is new to the list (the total counts it then, once).
fn placed(v: &mut Vec<LabGroupView>, g: LabGroupView) -> bool {
    match v.iter_mut().find(|x| x.id == g.id) {
        Some(at) => {
            *at = g;
            false
        }
        None => {
            v.insert(0, g);
            true
        }
    }
}

/// Where the history stands.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::pages) enum Load {
    Loading,
    Ready,
    Failed(String),
}

/// What an export covers.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::pages) enum ExportScope {
    /// The whole lab.
    All,
    /// Every group whose plan asks this model.
    Model(String),
    /// One group.
    Group(i64),
    /// One set run's items, in item order.
    Batch(i64),
}

/// A query value, percent-encoded: everything but RFC 3986's unreserved
/// characters.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `GET /labs/api/{lab}/export` for `format` over `scope`.
pub(in crate::pages) fn export_href(
    lab: LabKind,
    format: LabExportFormat,
    scope: &ExportScope,
) -> String {
    let format = match format {
        LabExportFormat::Jsonl => "jsonl",
        LabExportFormat::Csv => "csv",
        LabExportFormat::Files => "files",
    };
    let mut href = format!("/labs/api/{}/export?format={format}", lab.as_str());
    match scope {
        ExportScope::All => {}
        ExportScope::Model(m) => href.push_str(&format!("&model={}", encode(m))),
        ExportScope::Group(id) => href.push_str(&format!("&group={id}")),
        ExportScope::Batch(id) => href.push_str(&format!("&batch={id}")),
    }
    href
}

/// `GET /labs/api/{lab}/groups` for one page.
fn groups_href(lab: LabKind, before: Option<i64>, model: Option<&str>) -> String {
    let mut href = format!("/labs/api/{}/groups?limit={PAGE}", lab.as_str());
    if let Some(b) = before {
        href.push_str(&format!("&before={b}"));
    }
    if let Some(m) = model {
        href.push_str(&format!("&model={}", encode(m)));
    }
    href
}

/// `run` in its group's runs, in cell order, replacing a run of the same
/// cell.
pub(in crate::pages) fn put_run(g: &mut LabGroupView, run: LabRunView) {
    g.runs.retain(|r| r.cell != run.cell);
    let at = g.runs.partition_point(|r| r.cell < run.cell);
    g.runs.insert(at, run);
}

/// A first page read again (`fresh`), with the runs that landed in the list
/// (`was`) while the read was on its way and are not in it yet: a cell's
/// answer can reach the page before the page read that began before it.
fn keep_landed(was: &[LabGroupView], fresh: &mut [LabGroupView]) {
    for g in fresh.iter_mut() {
        let Some(old) = was.iter().find(|o| o.id == g.id) else {
            continue;
        };
        for run in &old.runs {
            if !g.runs.iter().any(|r| r.cell == run.cell) {
                put_run(g, run.clone());
            }
        }
    }
}

/// Does an older page asked for under `asked` (the generation, the list's
/// last group) still fit the list as it is `now`?
fn page_fits(asked: (u64, i64), now: (Option<u64>, Option<i64>)) -> bool {
    now == (Some(asked.0), Some(asked.1))
}

/// Does the list read the next older page by itself? Its end is in view,
/// none is on its way, older groups are left, and the last one did not fail:
/// a failure waits for Retry or a new list, so an error is not asked again
/// at the speed of a round trip.
pub(super) fn reads_on(seen: bool, more: bool, older: u64, failed: bool) -> bool {
    seen && !more && older > 0 && !failed
}

/// Does a new group show under the model filter `model`?
fn shows_under(model: Option<&str>, g: &LabGroupView) -> bool {
    model.is_none_or(|m| g.plan.iter().any(|c| c.model == m))
}

/// One lab's history. `Copy`: the page, its cards and the results head share
/// it.
#[derive(Clone, Copy)]
pub(in crate::pages) struct LabHistory {
    pub lab: LabKind,
    /// Newest first.
    pub groups: RwSignal<Vec<LabGroupView>>,
    /// Every group the filter matches.
    pub total: RwSignal<u64>,
    /// The groups older than the last one read.
    pub older: RwSignal<u64>,
    pub load: RwSignal<Load>,
    /// An older page is on its way.
    pub more: RwSignal<bool>,
    /// Why the last older page did not load. The list stops reading on and
    /// says so at its end, with Retry.
    pub older_failed: RwSignal<Option<String>>,
    pub usage: RwSignal<Option<LabUsage>>,
    /// Only groups that ask this model.
    pub model: RwSignal<Option<String>>,
    /// Why a group of this page has no run, when the store cannot say: it
    /// was not kept, or the lab refused its request.
    pub notes: RwSignal<BTreeMap<i64, String>>,
    /// The groups in flight being read again until they are not.
    following: StoredValue<BTreeSet<i64>>,
    scope: Scope,
    /// Which list is current: a reload (a filter change), a delete and Clear
    /// start a new one, and a page read under an older one is dropped.
    generation: StoredValue<u64>,
    /// Which usage read is the newest.
    usage_reads: Latest,
    toasts: Toasts,
}

impl LabHistory {
    /// Made where the page's signals are; reads the first page and the usage.
    pub fn new(lab: LabKind, scope: Scope) -> Self {
        let h = Self {
            lab,
            groups: RwSignal::new(Vec::new()),
            total: RwSignal::new(0),
            older: RwSignal::new(0),
            load: RwSignal::new(Load::Loading),
            more: RwSignal::new(false),
            older_failed: RwSignal::new(None),
            usage: RwSignal::new(None),
            model: RwSignal::new(None),
            notes: RwSignal::new(BTreeMap::new()),
            following: StoredValue::new(BTreeSet::new()),
            scope,
            generation: StoredValue::new(0),
            usage_reads: Latest::new(),
            toasts: use_toasts(),
        };
        h.reload();
        h.refresh_usage();
        h
    }

    fn base(self) -> String {
        format!("/labs/api/{}", self.lab.as_str())
    }

    /// A new list: a page read under the old one is dropped when it lands,
    /// and the older pages start over (`more` is the new list's, and a
    /// failure is forgotten). `None` once the page is gone.
    fn new_generation(self) -> Option<u64> {
        let ticket = self.generation.try_update_value(|n| {
            *n += 1;
            *n
        })?;
        self.more.set(false);
        self.older_failed.set(None);
        Some(ticket)
    }

    /// The first page again, for the filter as it is now.
    pub fn reload(self) {
        let Some(ticket) = self.new_generation() else {
            return;
        };
        let href = groups_href(self.lab, None, self.model.get_untracked().as_deref());
        self.load.set(Load::Loading);
        self.scope.spawn(async move {
            let got = api::get::<LabGroupPage>(href).await;
            if self.generation.try_get_value() != Some(ticket) {
                return;
            }
            match got {
                Ok(p) => {
                    self.total.set(p.total);
                    self.older.set(p.older);
                    let mut fresh = p.groups;
                    let flying: Vec<i64> =
                        fresh.iter().filter(|g| g.in_flight).map(|g| g.id).collect();
                    self.groups.update(|g| {
                        keep_landed(g, &mut fresh);
                        *g = fresh;
                    });
                    self.load.set(Load::Ready);
                    for id in flying {
                        self.follow(id);
                    }
                }
                Err(e) => self.load.set(Load::Failed(e.to_string())),
            }
        });
    }

    /// The next older page, when there is one and none is on its way. Also
    /// Retry after a failed one.
    pub fn load_older(self) {
        if self.more.get_untracked() || self.older.get_untracked() == 0 {
            return;
        }
        let Some(last) = self.groups.with_untracked(|g| g.last().map(|g| g.id)) else {
            return;
        };
        let Some(generation) = self.generation.try_get_value() else {
            return;
        };
        let href = groups_href(self.lab, Some(last), self.model.get_untracked().as_deref());
        self.older_failed.set(None);
        self.more.set(true);
        self.scope.spawn(async move {
            let got = api::get::<LabGroupPage>(href).await;
            // A new list since then owns `more`, and reads its own pages.
            if self.generation.try_get_value() != Some(generation) {
                return;
            }
            self.more.set(false);
            let now = (
                Some(generation),
                self.groups.with_untracked(|g| g.last().map(|g| g.id)),
            );
            // Asked from another end: the list reads on from its own.
            if !page_fits((generation, last), now) {
                return;
            }
            match got {
                Ok(p) => {
                    self.older.set(p.older);
                    let flying: Vec<i64> = p
                        .groups
                        .iter()
                        .filter(|g| g.in_flight)
                        .map(|g| g.id)
                        .collect();
                    self.groups.update(|g| {
                        for n in p.groups {
                            if !g.iter().any(|x| x.id == n.id) {
                                g.push(n);
                            }
                        }
                    });
                    for id in flying {
                        self.follow(id);
                    }
                }
                Err(e) => self.older_failed.set(Some(e.to_string())),
            }
        });
    }

    /// A group just created, in front of the list. A model filter it would
    /// not show under is cleared first, so the ask's progress is in view.
    pub fn prepend(self, g: LabGroupView) {
        let hidden = self
            .model
            .with_untracked(|m| !shows_under(m.as_deref(), &g));
        if hidden {
            self.model.set(None);
            self.toasts
                .ok("model filter cleared: it would have hidden this ask");
        }
        let mut new = false;
        self.groups.update(|v| new = placed(v, g));
        if new {
            self.total.update(|t| *t += 1);
        }
        if hidden {
            // The whole history, this group in front, its runs kept as
            // they land meanwhile.
            self.reload();
        }
    }

    /// A fresh read of a group, in its place in the list (a group the list
    /// does not hold stays out: a filter or a delete took it).
    pub fn replace(self, g: LabGroupView) {
        self.groups.update(|v| {
            if let Some(at) = v.iter_mut().find(|x| x.id == g.id) {
                *at = g;
            }
        });
    }

    /// Why group `id` has no run, for its card.
    pub fn note(self, id: i64, why: impl Into<String>) {
        self.notes.update(|n| {
            n.insert(id, why.into());
        });
    }

    /// Group `id` read again until it is no longer in flight, each read put
    /// in its place: the group as it settled, `None` once it is gone (it is
    /// taken out of the list then), `Err` for a read that failed. Ends with
    /// the page.
    pub async fn settle(self, id: i64) -> Result<Option<LabGroupView>, String> {
        let href = format!("{}/groups/{id}", self.base());
        loop {
            match api::get::<LabGroupView>(href.clone()).await {
                Ok(g) => {
                    let flying = g.in_flight;
                    self.replace(g.clone());
                    if !flying {
                        return Ok(Some(g));
                    }
                }
                Err(e) if e.is_not_found() => {
                    self.groups.update(|v| v.retain(|g| g.id != id));
                    return Ok(None);
                }
                Err(e) => return Err(e.to_string()),
            }
            pause(SETTLE_PACE_MS).await;
        }
    }

    /// [`Self::settle`] on the history's own time, once per group: a group
    /// in flight a page read brought.
    fn follow(self, id: i64) {
        let fresh = self
            .following
            .try_update_value(|f| f.insert(id))
            .unwrap_or(false);
        if !fresh {
            return;
        }
        self.scope.spawn(async move {
            let _ = self.settle(id).await;
            self.following.try_update_value(|f| f.remove(&id));
            self.refresh_usage();
        });
    }

    /// A cell's run, into its group.
    pub fn put_run(self, group_id: i64, run: LabRunView) {
        self.groups.update(|v| {
            if let Some(g) = v.iter_mut().find(|g| g.id == group_id) {
                put_run(g, run);
            }
        });
    }

    /// The usage as `post` answers it, unless a newer read went out
    /// since.
    fn usage_from(self, ticket: Option<u64>, u: LabUsage) {
        if ticket.is_some_and(|t| self.usage_reads.is(t)) {
            self.usage.set(Some(u));
        }
    }

    pub fn refresh_usage(self) {
        let href = format!("{}/usage", self.base());
        let ticket = self.usage_reads.next();
        self.scope.spawn(async move {
            if let Ok(u) = api::get::<LabUsage>(href).await {
                self.usage_from(ticket, u);
            }
        });
    }

    /// Delete one group, its runs and the files nothing else names.
    pub fn delete(self, id: i64) {
        let href = format!("{}/groups/{id}/delete", self.base());
        let ticket = self.usage_reads.next();
        self.scope.spawn(async move {
            match api::post::<LabUsage, _>(href, &()).await {
                Ok(u) => {
                    self.usage_from(ticket, u);
                    // A page read on its way may still hold the group: the
                    // list is a new one, and a first page read again.
                    let reading = self.load.get_untracked() == Load::Loading;
                    self.groups.update(|v| v.retain(|g| g.id != id));
                    self.total.update(|t| *t = t.saturating_sub(1));
                    if reading {
                        self.reload();
                    } else {
                        self.new_generation();
                    }
                    self.toasts.ok("group deleted");
                }
                Err(e) => self.toasts.err(format!("delete failed: {e}")),
            }
        });
    }

    /// Delete the lab's whole history.
    pub fn clear(self) {
        let href = format!("{}/clear", self.base());
        let ticket = self.usage_reads.next();
        self.scope.spawn(async move {
            match api::post::<LabUsage, _>(href, &()).await {
                Ok(u) => {
                    self.usage_from(ticket, u);
                    self.new_generation();
                    self.groups.set(Vec::new());
                    self.total.set(0);
                    self.older.set(0);
                    self.load.set(Load::Ready);
                    self.toasts.ok("history cleared");
                }
                Err(e) => self.toasts.err(format!("clear failed: {e}")),
            }
        });
    }

    /// Remove the artifact files no row names.
    pub fn delete_orphans(self) {
        let href = format!("{}/orphans/delete", self.base());
        let ticket = self.usage_reads.next();
        self.scope.spawn(async move {
            match api::post::<LabUsage, _>(href, &()).await {
                Ok(u) => {
                    self.usage_from(ticket, u);
                    self.toasts.ok("orphan files removed");
                }
                Err(e) => self
                    .toasts
                    .err(format!("removing the orphan files failed: {e}")),
            }
        });
    }

    pub fn export_href(self, format: LabExportFormat, scope: &ExportScope) -> String {
        export_href(self.lab, format, scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::lab_store::LabKind;

    fn run(cell: u32) -> LabRunView {
        LabRunView {
            id: Some(i64::from(cell) + 10),
            group_id: 1,
            cell,
            variant: "as_written".into(),
            endpoint: "/v1/systemone".into(),
            model_asked: "decide/laya".into(),
            answered_by: "decide/laya".into(),
            fallback_reason: None,
            upstream_model: None,
            http_status: 200,
            error_code: None,
            error_message: None,
            request: "{}".into(),
            response: None,
            response_type: None,
            headers: Default::default(),
            input_tokens: None,
            output_tokens: None,
            latency_ms: 1,
            created_at: String::new(),
            artifacts: vec![],
        }
    }

    #[test]
    fn export_hrefs_name_the_format_and_the_scope() {
        let lab = LabKind::Decision;
        assert_eq!(
            export_href(lab, LabExportFormat::Jsonl, &ExportScope::All),
            "/labs/api/decision/export?format=jsonl"
        );
        assert_eq!(
            export_href(lab, LabExportFormat::Csv, &ExportScope::Group(7)),
            "/labs/api/decision/export?format=csv&group=7"
        );
        assert_eq!(
            export_href(lab, LabExportFormat::Jsonl, &ExportScope::Batch(3)),
            "/labs/api/decision/export?format=jsonl&batch=3"
        );
        assert_eq!(
            export_href(
                LabKind::Audio,
                LabExportFormat::Files,
                &ExportScope::Group(9)
            ),
            "/labs/api/audio/export?format=files&group=9"
        );
        assert_eq!(
            export_href(
                lab,
                LabExportFormat::Csv,
                &ExportScope::Model("decide/laya q8".into())
            ),
            "/labs/api/decision/export?format=csv&model=decide%2Flaya%20q8"
        );
    }

    #[test]
    fn a_page_asks_for_older_groups_of_the_filter() {
        assert_eq!(
            groups_href(LabKind::Decision, Some(40), Some("decide/laya")),
            "/labs/api/decision/groups?limit=20&before=40&model=decide%2Flaya"
        );
        assert_eq!(
            groups_href(LabKind::Image, None, None),
            "/labs/api/image/groups?limit=20"
        );
    }

    fn group(id: i64, models: &[&str]) -> LabGroupView {
        LabGroupView {
            id,
            lab: LabKind::Decision,
            title: "t".into(),
            form: serde_json::Value::Null,
            plan: models
                .iter()
                .map(|m| lmgw_api_types::lab_store::LabCell {
                    model: (*m).into(),
                    variant: "as_written".into(),
                })
                .collect(),
            override_text: None,
            created_at: String::new(),
            runs: vec![],
            artifacts: vec![],
            item: None,
            in_flight: false,
        }
    }

    #[test]
    fn runs_land_in_cell_order_and_a_cell_has_one() {
        let mut g = group(1, &[]);
        put_run(&mut g, run(2));
        put_run(&mut g, run(0));
        put_run(&mut g, run(1));
        put_run(&mut g, run(1));
        let cells: Vec<u32> = g.runs.iter().map(|r| r.cell).collect();
        assert_eq!(cells, [0, 1, 2]);
    }

    #[test]
    fn a_page_read_again_keeps_the_runs_that_landed_meanwhile() {
        let mut was = group(1, &["a", "a", "a"]);
        put_run(&mut was, run(0));
        put_run(&mut was, run(1));
        let mut fresh = vec![group(2, &["a"]), group(1, &["a", "a", "a"])];
        put_run(&mut fresh[1], run(0));
        keep_landed(&[was], &mut fresh);
        let cells: Vec<u32> = fresh[1].runs.iter().map(|r| r.cell).collect();
        assert_eq!(cells, [0, 1]);
        assert!(fresh[0].runs.is_empty());
    }

    #[test]
    fn an_older_page_lands_only_on_the_list_it_was_asked_for() {
        assert!(page_fits((3, 40), (Some(3), Some(40))));
        assert!(!page_fits((3, 40), (Some(4), Some(40))), "a new list");
        assert!(!page_fits((3, 40), (Some(3), Some(39))), "another end");
        assert!(!page_fits((3, 40), (None, Some(40))), "the page is gone");
    }

    #[test]
    fn a_failed_page_stops_reading_on() {
        assert!(reads_on(true, false, 5, false));
        assert!(!reads_on(true, false, 5, true), "until Retry");
        assert!(!reads_on(true, true, 5, false), "one at a time");
        assert!(!reads_on(false, false, 5, false));
        assert!(!reads_on(true, false, 0, false));
    }

    #[test]
    fn a_group_put_in_front_twice_is_counted_once() {
        let mut v = vec![group(1, &["a"])];
        assert!(placed(&mut v, group(2, &["a"])), "new");
        let mut again = group(2, &["a"]);
        again.in_flight = true;
        assert!(!placed(&mut v, again), "a reload had it already");
        assert_eq!(v.iter().map(|g| g.id).collect::<Vec<_>>(), [2, 1]);
        assert!(v[0].in_flight, "the newer read stands");
    }

    #[test]
    fn a_runless_group_is_live_in_flight_not_kept_or_stopped() {
        assert_eq!(runless(true, true, Some("x")), Runless::Live);
        assert_eq!(runless(false, true, Some("x")), Runless::InFlight);
        assert_eq!(
            runless(false, false, Some("not kept: disk full")),
            Runless::NotKept("not kept: disk full".into())
        );
        assert_eq!(runless(false, false, None), Runless::Stopped);
    }

    #[test]
    fn a_new_group_shows_under_a_filter_that_names_one_of_its_models() {
        let g = group(1, &["decide/laya", "decide/kev-4b"]);
        assert!(shows_under(None, &g));
        assert!(shows_under(Some("decide/kev-4b"), &g));
        assert!(!shows_under(Some("decide/julia"), &g));
    }
}
