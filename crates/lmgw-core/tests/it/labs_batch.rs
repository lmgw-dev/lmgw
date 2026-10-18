//! Set runs (labs-store design §3, §7.3) on `decision_lab`'s wiremock
//! decision container, which answers each item from its state: a 50-item
//! labelled CSV through the job (each item asked once, metrics and the
//! threshold write against a hand count, the CSV and JSONL export by batch
//! with `expected` and `correct`, the history without the items); cancel
//! (the calls in flight kept) and resume; a restart that leaves the run
//! interrupted, and its resume; an unknown label refusing the whole input
//! with its row named; and presets CRUD. Every answer is read into its
//! documented type. The children: `control` (a Delete, a Clear and a second
//! Cancel dropping the calls in flight, a page's cell racing the job, a
//! model's 5xx and `retry_failed`, the GPU hold ending the job), `input`
//! (JSONL, score label columns, the key column in both exports), `sets`
//! (the preset update's precondition, `no_labels`, sparse reliability) and
//! `boot` (a half-written run deleted at boot; the 100,000-row Start
//! measurement, ignored).

mod boot;
mod control;
mod input;
mod sets;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_api_types::decision_lab::batch::JOB_KIND;
use lmgw_api_types::decision_lab::{
    job_key, BatchItemPage, BatchJobDetail, BatchList, BatchMetrics, BatchPreviewed, BatchStatus,
    BatchView, OptionDraft, QuestionDraft, QuestionKind, QuestionSet,
};
use lmgw_api_types::lab_store::{
    LabExportLine, LabGroupPage, LabPresetBody, LabPresetList, LabPresetView, LabUsage,
};
use lmgw_core::runtime::registry::Registry;
use lmgw_core::state::{AppState, SharedState};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, Request, Respond, ResponseTemplate};

use crate::common::{round_trips, serve, Gw};
use crate::decision_lab::{lab, post, Lab, CPU_ROW};
use crate::support::decision_world::RecordingPodman;

const TEAMS: [&str; 3] = ["billing", "technical", "other"];

/// The confidence the container gives item `i`'s team: 0.50 to 0.95.
fn conf_of(i: usize) -> f64 {
    (50 + (i % 10) * 5) as f64 / 100.0
}

/// P(yes) the container gives item `i`.
fn p_yes(i: usize) -> f64 {
    if i.is_multiple_of(2) {
        0.9
    } else {
        0.2
    }
}

/// The team label of item `i`: the container's answer, but wrong for every
/// fifth item.
fn team_label(i: usize) -> &'static str {
    TEAMS[if i % 5 == 4 { (i + 1) % 3 } else { i % 3 }]
}

/// The angry label of item `i`: what P(yes) says, flipped every seventh
/// item, and none for every tenth.
fn angry_label(i: usize) -> Option<&'static str> {
    if i % 10 == 9 {
        return None;
    }
    let yes = i.is_multiple_of(2) != (i % 7 == 6);
    Some(if yes { "yes" } else { "no" })
}

/// The container's answer to item `i` (its state is `ticket <i>`).
fn answer_for(i: usize) -> String {
    let probs: Vec<String> = TEAMS
        .iter()
        .enumerate()
        .map(|(k, t)| format!("\"{t}\": {}", if k == i % 3 { 0.8 } else { 0.1 }))
        .collect();
    format!(
        r#"{{"model": "laya", "answers": {{"team": {{"type": "choice", "choice": "{}",
            "probabilities": {{{}}}, "confidence": {}}}, "angry": {{"type": "noul",
            "noul": {}}}}}, "usage": {{"input_tokens": 12, "output_tokens": 0}}}}"#,
        TEAMS[i % 3],
        probs.join(", "),
        conf_of(i),
        p_yes(i)
    )
}

/// Answers each item from its state, after `delay`; from request
/// `hang_from` on, while `hang` is up, not for an hour, and the items in
/// `hang_items` not for an hour either; the items in `fail_items` with
/// llama.cpp's 500. while `deny` is up every item with a 401. `asked` is every item asked, in order.
#[derive(Clone)]
struct Items {
    seen: Arc<AtomicUsize>,
    hang: Arc<AtomicBool>,
    hang_from: usize,
    delay: Duration,
    hang_items: Arc<Mutex<HashSet<usize>>>,
    fail_items: Arc<Mutex<HashSet<usize>>>,
    /// While up, every item is refused with a 401 (the key lmgw holds).
    deny: Arc<AtomicBool>,
    asked: Arc<Mutex<Vec<usize>>>,
}

impl Items {
    fn new(delay: Duration) -> Self {
        Self {
            seen: Arc::default(),
            hang: Arc::default(),
            hang_from: usize::MAX,
            delay,
            hang_items: Arc::default(),
            fail_items: Arc::default(),
            deny: Arc::default(),
            asked: Arc::default(),
        }
    }

    fn seen(&self) -> usize {
        self.seen.load(Ordering::SeqCst)
    }

    /// How often item `i` was asked.
    fn asked(&self, i: usize) -> usize {
        self.asked
            .lock()
            .unwrap()
            .iter()
            .filter(|a| **a == i)
            .count()
    }

    fn hang_items(&self, items: &[usize]) {
        *self.hang_items.lock().unwrap() = items.iter().copied().collect();
    }

    fn fail_items(&self, items: &[usize]) {
        *self.fail_items.lock().unwrap() = items.iter().copied().collect();
    }
}

/// llama.cpp's 500 for a prompt over the ubatch.
const LLAMA_500: &str = r#"{"error": {"code": 500, "message": "input (8519 tokens) is too
    large to process. increase the physical batch size", "type": "server_error"}}"#;

impl Respond for Items {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let i: usize = body["state"]
            .as_str()
            .and_then(|s| s.strip_prefix("ticket "))
            .and_then(|s| s.parse().ok())
            .unwrap_or_default();
        self.asked.lock().unwrap().push(i);
        let t = if self.deny.load(Ordering::SeqCst) {
            ResponseTemplate::new(401).set_body_raw(
                r#"{"error": {"code": 401, "message": "Invalid API Key", "type": "authentication_error"}}"#,
                "application/json",
            )
        } else if self.fail_items.lock().unwrap().contains(&i) {
            ResponseTemplate::new(500).set_body_raw(LLAMA_500, "application/json")
        } else {
            ResponseTemplate::new(200).set_body_raw(answer_for(i), "application/json")
        };
        let hung = self.hang_items.lock().unwrap().contains(&i);
        if hung || (n >= self.hang_from && self.hang.load(Ordering::SeqCst)) {
            t.set_delay(Duration::from_secs(3600))
        } else {
            t.set_delay(self.delay)
        }
    }
}

/// `decision_lab`'s lab with its container answering through `items`.
async fn batch_lab(items: &Items) -> Lab {
    batch_lab_on(items, lab().await).await
}

/// [`batch_lab`] on lab `l`.
async fn batch_lab_on(items: &Items, l: Lab) -> Lab {
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(items.clone())
        .with_priority(1)
        .mount(&l.container)
        .await;
    l
}

fn set(model: &str) -> QuestionSet {
    QuestionSet {
        questions: vec![
            QuestionDraft {
                id: "team".into(),
                instructions: "Which team should handle this ticket?".into(),
                kind: QuestionKind::Choice {
                    options: TEAMS
                        .map(|n| OptionDraft {
                            name: n.into(),
                            description: String::new(),
                        })
                        .to_vec(),
                },
            },
            QuestionDraft {
                id: "angry".into(),
                instructions: "Is the customer upset?".into(),
                kind: QuestionKind::Noul {
                    yes: "upset".into(),
                    no: String::new(),
                },
            },
        ],
        default_model: model.into(),
        thresholds: Default::default(),
    }
}

/// `n` labelled items as CSV, with a column the run does not read whose
/// values need quoting.
fn csv(n: usize) -> String {
    let mut out = String::from("text,team,angry,note\r\n");
    for i in 0..n {
        out.push_str(&format!(
            "ticket {i},{},{},\"said \"\"hi\"\", then {i}\"\r\n",
            team_label(i),
            angry_label(i).unwrap_or("")
        ));
    }
    out
}

fn mapping() -> Value {
    json!({"state": "text", "state_mode": "text", "labels": {"team": "team", "angry": "angry"}})
}

async fn get(gw: &Gw, path: &str) -> (u16, Value) {
    let resp = gw.client().get(format!("{gw}{path}")).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn text(gw: &Gw, path: &str) -> String {
    let resp = gw.client().get(format!("{gw}{path}")).send().await.unwrap();
    assert_eq!(resp.status(), 200, "{path}");
    resp.text().await.unwrap()
}

/// Save `set` as a question set named `name`; its id.
async fn save_set(gw: &Gw, name: &str, set: &QuestionSet) -> i64 {
    let (status, body) = post(
        gw,
        "/labs/api/decision/presets",
        json!({"name": name, "kind": "question_set", "body": set}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    round_trips::<LabPresetView>("preset", &body).id
}

/// `cond` once it holds; a panic after ten seconds.
async fn eventually(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..400 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never: {what}");
}

/// Start a set run of preset `preset` over `csv`.
async fn start(gw: &Gw, preset: i64, csv: String, concurrency: u32) -> BatchView {
    let (status, body) = post(
        gw,
        "/decision-lab/api/batches",
        json!({"preset_id": preset, "format": "csv", "text": csv, "source_name": "tickets.csv",
               "mapping": mapping(), "concurrency": concurrency}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    round_trips::<BatchView>("start", &body)
}

/// Set run `id` once `done` says so; a panic after ten seconds.
async fn until(gw: &Gw, id: i64, done: impl Fn(&BatchView) -> bool) -> BatchView {
    for _ in 0..400 {
        let (status, body) = get(gw, &format!("/decision-lab/api/batches/{id}")).await;
        assert_eq!(status, 200, "{body}");
        let v: BatchView = serde_json::from_value(body).unwrap();
        if done(&v) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("set run {id} never got there");
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[tokio::test]
async fn a_labelled_csv_runs_through_the_job_with_metrics_and_an_export() {
    let items = Items::new(Duration::ZERO);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let laya = l.w.name(CPU_ROW);
    let preset = save_set(&gw, "triage", &set(&laya)).await;

    // The preview shows the top and every column.
    let (status, body) = post(
        &gw,
        "/decision-lab/api/batches/preview",
        json!({"format": "csv", "text": csv(50), "questions": set(&laya).questions,
               "mapping": mapping()}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let p = round_trips::<BatchPreviewed>("preview", &body);
    assert_eq!((p.rows, p.sample.len()), (50, 20));
    assert_eq!(p.columns.len(), 4);
    assert_eq!(
        p.sample[3],
        ["ticket 3", "billing", "no", "said \"hi\", then 3"]
    );
    assert!(p.problems.is_empty(), "{:?}", p.problems);

    // Model blank: the set's default.
    let v = start(&gw, preset, csv(50), 4).await;
    assert_eq!(
        (v.total, v.model.as_str(), v.preset_id),
        (50, laya.as_str(), Some(preset))
    );
    assert_eq!(v.source_name, "tickets.csv");
    let v = until(&gw, v.id, |v| v.status == BatchStatus::Done).await;
    assert_eq!((v.done, v.failed, v.labelled), (50, 0, 50));
    assert_eq!(items.seen(), 50, "each item asked once");
    let job = lmgw_core::store::get_job(&l.w.state.db, v.job_id.unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (job.kind.as_str(), job.key.clone(), job.status.as_str()),
        (JOB_KIND, Some(job_key(v.id)), "done")
    );
    let detail: BatchJobDetail = serde_json::from_str(job.result.as_deref().unwrap()).unwrap();
    assert_eq!((detail.done, detail.total, detail.not_kept), (50, 50, 0));

    let (_, body) = get(&gw, "/decision-lab/api/batches").await;
    let list = round_trips::<BatchList>("list", &body);
    assert_eq!(list.batches.len(), 1);
    assert_eq!(list.batches[0].preset_name.as_deref(), Some("triage"));

    // Items in file order, each with its run and its labels.
    let (_, body) = get(
        &gw,
        &format!("/decision-lab/api/batches/{}/items?limit=20", v.id),
    )
    .await;
    let page = round_trips::<BatchItemPage>("items", &body);
    assert_eq!(
        (page.items.len(), page.total, page.next_after),
        (20, 50, Some(19))
    );
    let fourth = &page.items[4];
    let item = fourth.item.as_ref().unwrap();
    assert_eq!((item.batch_id, item.item_ix), (v.id, 4));
    assert_eq!(
        item.expected.get("team").map(String::as_str),
        Some(team_label(4))
    );
    assert_eq!(fourth.runs.len(), 1);
    assert_eq!(fourth.runs[0].http_status, 200);
    let (_, body) = get(
        &gw,
        &format!(
            "/decision-lab/api/batches/{}/items?after=19&limit=100",
            v.id
        ),
    )
    .await;
    let rest: BatchItemPage = serde_json::from_value(body).unwrap();
    assert_eq!((rest.items.len(), rest.next_after), (30, None));
    assert_eq!(rest.items[0].item.as_ref().unwrap().item_ix, 20);

    // The history does not list them; usage counts them.
    let (_, body) = get(&gw, "/labs/api/decision/groups").await;
    let history: LabGroupPage = serde_json::from_value(body).unwrap();
    assert_eq!(history.total, 0);
    let (_, body) = get(&gw, "/labs/api/decision/usage").await;
    let usage = round_trips::<LabUsage>("usage", &body);
    assert_eq!((usage.groups, usage.batches, usage.runs), (50, 1, 50));

    // Metrics against a hand count.
    let (status, body) = get(&gw, &format!("/decision-lab/api/batches/{}/metrics", v.id)).await;
    assert_eq!(status, 200, "{body}");
    let m = round_trips::<BatchMetrics>("metrics", &body);
    assert_eq!(m.bins, 10);
    let team = &m.questions[0];
    assert_eq!(team.id, "team");
    assert_eq!(
        (team.answered, team.failed, team.labelled, team.scored),
        (50, 0, 50, 50)
    );
    let right = (0..50).filter(|i| TEAMS[i % 3] == team_label(*i)).count();
    assert!(close(team.accuracy.unwrap(), right as f64 / 50.0));
    let mut confusion = vec![vec![0u64; 3]; 3];
    for i in 0..50 {
        let label = TEAMS.iter().position(|t| *t == team_label(i)).unwrap();
        confusion[label][i % 3] += 1;
    }
    assert_eq!(team.confusion, confusion);
    assert_eq!(team.curve.len(), 10, "one point per distinct confidence");
    let at = team.curve.iter().find(|p| close(p.threshold, 0.7)).unwrap();
    let kept: Vec<usize> = (0..50).filter(|i| conf_of(*i) >= 0.7).collect();
    let kept_right = kept
        .iter()
        .filter(|i| TEAMS[*i % 3] == team_label(**i))
        .count();
    assert_eq!(
        (at.covered, at.labelled, at.correct),
        (kept.len() as u64, kept.len() as u64, kept_right as u64)
    );
    assert!(close(at.coverage, kept.len() as f64 / 50.0));
    // The winner's probability is 0.8 for every item: one bin holds all,
    // and it is the only one listed.
    let held: Vec<(u32, u64)> = team
        .reliability
        .iter()
        .map(|b| (b.index, b.count))
        .collect();
    assert_eq!(held, [(8, 50)]);
    let right_share = right as f64 / 50.0;
    assert!(close(team.reliability[0].observed, right_share));
    assert_eq!((team.failed, team.unreadable), (0, 0));

    let angry = &m.questions[1];
    let labelled: Vec<usize> = (0..50).filter(|i| angry_label(*i).is_some()).collect();
    let angry_right = labelled
        .iter()
        .filter(|i| angry_label(**i) == Some(if p_yes(**i) >= 0.5 { "yes" } else { "no" }))
        .count();
    assert_eq!((angry.labelled, angry.scored), (45, 45));
    assert!(close(angry.accuracy.unwrap(), angry_right as f64 / 45.0));
    assert_eq!(angry.classes, ["yes", "no"]);

    let (_, body) = get(
        &gw,
        &format!(
            "/decision-lab/api/batches/{}/metrics?question=angry&bins=4",
            v.id
        ),
    )
    .await;
    let one: BatchMetrics = serde_json::from_value(body).unwrap();
    assert_eq!((one.questions.len(), one.bins), (1, 4));
    // A noul bins P(yes) against the rate of yes labels: 0.2 for the odd
    // items, 0.9 for the even ones; only the labelled ones count.
    let bins = &one.questions[0].reliability;
    let held: Vec<(u32, u64)> = bins.iter().map(|b| (b.index, b.count)).collect();
    let (odd, even): (Vec<usize>, Vec<usize>) = labelled.iter().partition(|i| *i % 2 == 1);
    assert_eq!(held, [(0, odd.len() as u64), (3, even.len() as u64)]);
    let yes_rate = |of: &[usize]| {
        of.iter()
            .filter(|i| angry_label(**i) == Some("yes"))
            .count() as f64
            / of.len() as f64
    };
    assert!(close(bins[0].observed, yes_rate(&odd)));
    assert!(close(bins[1].observed, yes_rate(&even)));
    let (status, _) = get(
        &gw,
        &format!("/decision-lab/api/batches/{}/metrics?bins=0", v.id),
    )
    .await;
    assert_eq!(status, 400);

    // "Use as threshold" writes the value and its measurement into the set.
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/threshold", v.id),
        json!({"question": "team", "value": 0.7}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let saved = round_trips::<LabPresetView>("threshold", &body);
    let LabPresetBody::QuestionSet(s) = saved.preset else {
        panic!("a question set");
    };
    let t = s.thresholds.get("team").unwrap();
    assert!(close(t.value, 0.7));
    let measured = t.measured.as_ref().unwrap();
    assert_eq!(
        (measured.batch_id, measured.labelled),
        (v.id, kept.len() as u64)
    );
    assert!(close(measured.coverage, at.coverage));
    assert!(close(
        measured.accuracy,
        kept_right as f64 / kept.len() as f64
    ));

    // The CSV export of the run: a row per item and question, with the
    // label and whether the answer matches it.
    let csv_out = text(
        &gw,
        &format!("/labs/api/decision/export?format=csv&batch={}", v.id),
    )
    .await;
    let mut rdr = ::csv::Reader::from_reader(csv_out.as_bytes());
    let header: Vec<String> = rdr.headers().unwrap().iter().map(String::from).collect();
    let col = |name: &str| header.iter().position(|h| h == name).unwrap();
    let rows: Vec<::csv::StringRecord> = rdr.records().map(Result::unwrap).collect();
    assert_eq!(rows.len(), 100);
    let (q, expected, correct, state) = (
        col("question_id"),
        col("expected"),
        col("correct"),
        col("state"),
    );
    for r in &rows {
        let i: usize = r[state].strip_prefix("ticket ").unwrap().parse().unwrap();
        match &r[q] {
            "team" => {
                assert_eq!(&r[expected], team_label(i));
                assert_eq!(&r[correct], (TEAMS[i % 3] == team_label(i)).to_string());
            }
            "angry" => match angry_label(i) {
                Some(l) => {
                    assert_eq!(&r[expected], l);
                    let said = if p_yes(i) >= 0.5 { "yes" } else { "no" };
                    assert_eq!(&r[correct], (said == l).to_string());
                }
                None => assert_eq!((&r[expected], &r[correct]), ("", "")),
            },
            other => panic!("question {other}"),
        }
    }

    // The JSONL export carries each item's expected answers.
    let jsonl = text(&gw, &format!("/labs/api/decision/export?batch={}", v.id)).await;
    let lines: Vec<LabExportLine> = jsonl
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 50);
    let first = lines[0].group.item.as_ref().unwrap();
    assert_eq!(first.item_ix, 0);
    assert_eq!(first.expected.get("angry").map(String::as_str), Some("yes"));
    let (status, _) = get(&gw, "/labs/api/decision/export?batch=999").await;
    assert_eq!(status, 404);

    // An item goes with its run, not alone.
    let (status, body) = post(
        &gw,
        &format!("/labs/api/decision/groups/{}/delete", fourth.id),
        json!(null),
    )
    .await;
    assert_eq!((status, body["code"].as_str()), (409, Some("batch_item")));

    // Deleting the run takes its items; the set stays.
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/delete", v.id),
        json!(null),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let usage = round_trips::<LabUsage>("delete", &body);
    assert_eq!((usage.groups, usage.batches, usage.runs), (0, 0, 0));
    let (status, _) = get(&gw, &format!("/labs/api/decision/presets/{preset}")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn cancel_keeps_the_calls_in_flight_and_resume_runs_the_rest() {
    let items = Items::new(Duration::from_millis(150));
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let preset = save_set(&gw, "triage", &set(&l.w.name(CPU_ROW))).await;
    let v = start(&gw, preset, csv(20), 1).await;
    until(&gw, v.id, |v| v.done >= 3).await;

    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/cancel", v.id),
        json!(null),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let stopped = until(&gw, v.id, |v| v.status != BatchStatus::Running).await;
    assert_eq!(stopped.status, BatchStatus::Canceled);
    assert!(stopped.done < 20, "{stopped:?}");
    assert_eq!(
        stopped.done as usize,
        items.seen(),
        "no item taken after the cancel, the one in flight kept"
    );
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/cancel", v.id),
        json!(null),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("batch_not_running"))
    );

    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/resume", v.id),
        json!({"concurrency": 4}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let resumed = round_trips::<BatchView>("resume", &body);
    assert_eq!(resumed.concurrency, 4);
    assert_ne!(resumed.job_id, stopped.job_id, "a new job");
    let v = until(&gw, v.id, |v| v.status == BatchStatus::Done).await;
    assert_eq!(v.done, 20);
    assert_eq!(items.seen(), 20, "each item asked once over both jobs");

    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/resume", v.id),
        json!({}),
    )
    .await;
    assert_eq!((status, body["code"].as_str()), (409, Some("batch_done")));
}

/// A second gateway on `state`'s database, its containers on the same mock:
/// what a restart brings up.
async fn restarted(state: &SharedState, l: &Lab) -> (SharedState, Gw) {
    lmgw_core::store::fail_orphaned_jobs(&state.db)
        .await
        .unwrap();
    let again = AppState::init_for_tests_on(state.db.clone()).await.unwrap();
    let port: u16 = l
        .container
        .uri()
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    again.set_runtime_for_tests(Arc::new(Registry::with_ports(
        Arc::new(RecordingPodman::default()),
        reqwest::Client::new(),
        Arc::new(move || Ok(port)),
    )));
    let gw = serve(again.clone()).await;
    (again, gw)
}

#[tokio::test]
async fn a_restart_leaves_the_run_interrupted_and_resume_runs_the_rest() {
    let mut items = Items::new(Duration::ZERO);
    items.hang_from = 5;
    items.hang.store(true, Ordering::SeqCst);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let preset = save_set(&gw, "triage", &set(&l.w.name(CPU_ROW))).await;
    let v = start(&gw, preset, csv(12), 1).await;
    // Five answered, the sixth hanging in the container: the moment the
    // gateway goes down.
    until(&gw, v.id, |v| v.done == 5).await;
    for _ in 0..400 {
        if items.seen() >= 6 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(items.seen(), 6);

    let (_again, gw2) = restarted(&l.w.state, &l).await;
    items.hang.store(false, Ordering::SeqCst);
    let (status, body) = get(&gw2, &format!("/decision-lab/api/batches/{}", v.id)).await;
    assert_eq!(status, 200, "{body}");
    let after = round_trips::<BatchView>("interrupted", &body);
    assert_eq!(after.status, BatchStatus::Interrupted);
    assert_eq!((after.done, after.total), (5, 12));
    assert_eq!(after.job_error.as_deref(), Some("interrupted by shutdown"));

    let (status, body) = post(
        &gw2,
        &format!("/decision-lab/api/batches/{}/resume", v.id),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let done = until(&gw2, v.id, |v| v.status == BatchStatus::Done).await;
    assert_eq!((done.done, done.failed), (12, 0));
    // The item the first gateway left in flight was asked again, once.
    assert_eq!(items.seen(), 13);
    let (_, body) = get(&gw2, &format!("/decision-lab/api/batches/{}/items", v.id)).await;
    let page: BatchItemPage = serde_json::from_value(body).unwrap();
    assert!(page.items.iter().all(|g| g.runs.len() == 1));
}

#[tokio::test]
async fn an_unknown_label_refuses_the_whole_input_naming_its_row() {
    let items = Items::new(Duration::ZERO);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let laya = l.w.name(CPU_ROW);
    let preset = save_set(&gw, "triage", &set(&laya)).await;
    let text = "text,team,angry\nticket 0,billing,yes\nticket 1,technical,no\n\
                ticket 2,biling,yes\nticket 3,other,perhaps\n";

    let (status, body) = post(
        &gw,
        "/decision-lab/api/batches/preview",
        json!({"format": "csv", "text": text, "questions": set(&laya).questions,
               "mapping": mapping()}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let p: BatchPreviewed = serde_json::from_value(body).unwrap();
    let named: Vec<(Option<u64>, Option<u64>, Option<&str>)> = p
        .problems
        .iter()
        .map(|p| (p.row, p.line, p.value.as_deref()))
        .collect();
    assert_eq!(
        named,
        [
            (Some(3), Some(4), Some("biling")),
            (Some(4), Some(5), Some("perhaps"))
        ]
    );

    let (status, body) = post(
        &gw,
        "/decision-lab/api/batches",
        json!({"preset_id": preset, "format": "csv", "text": text, "mapping": mapping()}),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (400, Some("bad_input")),
        "{body}"
    );
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("row 3 (line 4), column 'team': 'biling' is not an option of 'team'")
            && message.contains("row 4 (line 5), column 'angry': 'perhaps'"),
        "{message}"
    );
    // Nothing was stored, nothing was asked.
    let (_, body) = get(&gw, "/decision-lab/api/batches").await;
    let list: BatchList = serde_json::from_value(body).unwrap();
    assert!(list.batches.is_empty());
    let (_, body) = get(&gw, "/labs/api/decision/usage").await;
    assert_eq!(body["groups"], 0);
    assert_eq!(items.seen(), 0);

    // Neither a model that answers no /v1/systemone nor a set and a preset.
    let (status, _) = post(
        &gw,
        "/decision-lab/api/batches",
        json!({"preset_id": preset, "model": "nope", "format": "csv", "text": csv(2),
               "mapping": mapping()}),
    )
    .await;
    assert_eq!(status, 404);
    let (status, _) = post(
        &gw,
        "/decision-lab/api/batches",
        json!({"preset_id": preset, "set": set(&laya), "format": "csv", "text": csv(2),
               "mapping": mapping()}),
    )
    .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn presets_are_saved_listed_updated_and_deleted_and_a_run_keeps_its_set() {
    let items = Items::new(Duration::ZERO);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let laya = l.w.name(CPU_ROW);
    let id = save_set(&gw, "triage", &set(&laya)).await;

    let (status, body) = post(
        &gw,
        "/labs/api/decision/presets",
        json!({"name": "triage", "kind": "question_set", "body": set(&laya)}),
    )
    .await;
    assert_eq!((status, body["code"].as_str()), (409, Some("name_taken")));
    let mut bad = set(&laya);
    bad.thresholds = [(
        "urgency",
        lmgw_api_types::decision_lab::Threshold {
            value: 0.5,
            measured: None,
        },
    )]
    .into_iter()
    .collect();
    let (status, body) = post(
        &gw,
        "/labs/api/decision/presets",
        json!({"name": "bad", "kind": "question_set", "body": bad}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("'urgency'"),
        "{body}"
    );
    let (status, _) = post(
        &gw,
        "/labs/api/image/presets",
        json!({"name": "x", "kind": "question_set", "body": set(&laya)}),
    )
    .await;
    assert_eq!(status, 400, "a question set is the decision lab's");

    let (status, body) = get(&gw, "/labs/api/decision/presets?kind=question_set").await;
    assert_eq!(status, 200, "{body}");
    let list = round_trips::<LabPresetList>("presets", &body);
    assert_eq!(list.presets.len(), 1);
    assert_eq!(list.presets[0].name, "triage");

    // A run of the set, then an update: the run keeps the set as it ran.
    let v = start(&gw, id, csv(3), 1).await;
    until(&gw, v.id, |v| v.status == BatchStatus::Done).await;
    let mut changed = set(&laya);
    changed.questions[0].instructions = "Who handles it?".into();
    let read_at = list.presets[0].updated_at.clone();
    let (status, body) = post(
        &gw,
        &format!("/labs/api/decision/presets/{id}"),
        json!({"name": "triage v2", "kind": "question_set", "body": changed,
               "updated_at": read_at}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let updated = round_trips::<LabPresetView>("update", &body);
    assert_eq!(updated.name, "triage v2");
    let (_, body) = get(&gw, &format!("/decision-lab/api/batches/{}", v.id)).await;
    let ran: BatchView = serde_json::from_value(body).unwrap();
    assert_eq!(
        ran.set.questions[0].instructions,
        "Which team should handle this ticket?"
    );
    assert_eq!(ran.preset_name.as_deref(), Some("triage v2"));

    // A threshold measured on the old question does not go onto the new one.
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/threshold", v.id),
        json!({"question": "team", "value": 0.6}),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("set_changed")),
        "{body}"
    );
    // The other question is unchanged: it takes one.
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/threshold", v.id),
        json!({"question": "angry", "value": 0.6}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let (status, body) = post(
        &gw,
        &format!("/labs/api/decision/presets/{id}/delete"),
        json!(null),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let left = round_trips::<LabPresetList>("delete", &body);
    assert!(left.presets.is_empty());
    let (status, _) = get(&gw, &format!("/labs/api/decision/presets/{id}")).await;
    assert_eq!(status, 404);
    let (_, body) = get(&gw, &format!("/decision-lab/api/batches/{}", v.id)).await;
    let kept: BatchView = serde_json::from_value(body).unwrap();
    assert_eq!((kept.preset_id, kept.set.questions.len()), (None, 2));
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/threshold", v.id),
        json!({"question": "angry", "value": 0.6}),
    )
    .await;
    assert_eq!((status, body["code"].as_str()), (409, Some("set_deleted")));

    // Clear takes the lab's runs too; the presets are not its history.
    let id = save_set(&gw, "again", &set(&laya)).await;
    let (status, body) = post(&gw, "/labs/api/decision/clear", json!(null)).await;
    assert_eq!(status, 200, "{body}");
    let usage: LabUsage = serde_json::from_value(body).unwrap();
    assert_eq!((usage.groups, usage.batches, usage.runs), (0, 0, 0));
    let (status, _) = get(&gw, &format!("/labs/api/decision/presets/{id}")).await;
    assert_eq!(status, 200);
}
