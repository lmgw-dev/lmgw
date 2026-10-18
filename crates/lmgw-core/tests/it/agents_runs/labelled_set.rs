//! "Save as labelled set" (labs-store design §4, Q4): a finished apply
//! run's received rows as a set run not yet run, of the set stamped on the
//! rows, each item's state the classify prompt, its form the output field's
//! question alone, and its label the applied output, the owner's edit
//! included; unchecked rows are no labels. Resume runs it, its threshold
//! lands on that question of the saved set, and its export carries the
//! labels. A fixture apply job shows the rows the model was never asked
//! about and the failed calls skipped, a label the set does not offer
//! refusing the save, rows of several sets (or none) refused naming them,
//! and the runs that are no labels refused.

use lmgw_api_types::decision_lab::{
    BatchFromAgentSaved, BatchFromAgentSkip, BatchItemPage, BatchStatus, BatchView, QuestionDraft,
    QuestionKind, StateMode,
};
use lmgw_api_types::lab_store::LabPresetView;
use lmgw_core::store;
use serde_json::{json, Value};

use super::decide::{measured_set, run, save_set, set, world, Mails};
use crate::common::{round_trips, Gw};
use crate::decision_lab::{post, CPU_ROW};

pub(super) async fn get(gw: &Gw, path: &str) -> (u16, Value) {
    let resp = gw.client().get(format!("{gw}{path}")).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn text(gw: &Gw, path: &str) -> String {
    let resp = gw.client().get(format!("{gw}{path}")).send().await.unwrap();
    assert_eq!(resp.status(), 200, "{path}");
    resp.text().await.unwrap()
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
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("set run {id} never got there");
}

#[tokio::test]
async fn an_apply_runs_received_rows_become_a_labelled_set_run_that_resume_runs() {
    let mails = Mails::default();
    let (l, _calls) = world(&mails, "decider").await;
    let gw = l.gw().clone();
    // A second question the agent never asks, with a threshold of its own.
    let mut two = measured_set(&l.w.name(CPU_ROW), 0.7);
    two.questions.push(QuestionDraft {
        id: "urgent".into(),
        instructions: "Is it urgent?".into(),
        kind: QuestionKind::Noul {
            yes: String::new(),
            no: String::new(),
        },
    });
    two.thresholds.0.push((
        "urgent".into(),
        lmgw_api_types::decision_lab::Threshold {
            value: 0.6,
            measured: None,
        },
    ));
    let preset = save_set(&gw, "mail", &two).await;
    let values = json!({ "values": { "decision_set": preset } });
    let classify = run(&l, "decider", "classify", values.clone()).await;
    let (_, d) = get(&gw, &format!("/api/agents/runs/{classify}")).await;

    // The reviewer unticks m1 and corrects m2 from Work to Billing; Apply
    // receives m2 only, as the Run tab sends it.
    let mut m2 = d["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!("m2"))
        .cloned()
        .unwrap();
    m2["output"]["category"] = json!("Billing");
    let mut args = values.clone();
    args["rows"] = json!([m2]);
    let apply = run(&l, "decider", "apply", args).await;
    let row = store::get_job(&l.w.state.db, apply).await.unwrap().unwrap();
    assert_eq!(row.status, "done", "{:?}", row.error);

    // Neither the classify run nor a live one is a labelled set.
    let (status, body) = post(
        &gw,
        "/decision-lab/api/batches/from-agent",
        json!({ "job_id": classify }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["message"].as_str().unwrap().contains("classify run"));
    let (status, _) = post(
        &gw,
        "/decision-lab/api/batches/from-agent",
        json!({ "job_id": 9999 }),
    )
    .await;
    assert_eq!(status, 404);
    // `job_id` is required: no body names "job id 0".
    for body in [json!({}), json!({ "preset_id": preset })] {
        let (status, answer) = post(&gw, "/decision-lab/api/batches/from-agent", body).await;
        assert_eq!(status, 422, "{answer}");
    }

    // Saved with no set named: the one stamped on the rows.
    let (status, body) = post(
        &gw,
        "/decision-lab/api/batches/from-agent",
        json!({ "job_id": apply }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let saved = round_trips::<BatchFromAgentSaved>("from-agent", &body);
    assert!(saved.skipped.is_empty(), "{:?}", saved.skipped);
    assert!(!saved.set_changed);
    let v = saved.batch;
    assert_eq!(v.status, BatchStatus::NotStarted);
    assert_eq!((v.total, v.done, v.labelled), (1, 0, 1), "m2 only");
    assert_eq!(v.job_id, None);
    assert_eq!(
        (v.preset_id, v.preset_name.as_deref()),
        (Some(preset), Some("mail")),
        "the set stamped on the rows"
    );
    // The set as the agent asks it: the output field's question alone, with
    // its threshold.
    let asked: Vec<&str> = v.set.questions.iter().map(|q| q.id.as_str()).collect();
    assert_eq!(asked, ["category"]);
    assert_eq!(v.set.thresholds.keys().collect::<Vec<_>>(), ["category"]);
    assert_eq!(v.model, l.w.name(CPU_ROW));
    assert_eq!(v.source_name, format!("agent run {apply} of decider"));
    assert_eq!(
        (v.mapping.state.as_str(), v.mapping.key.as_deref()),
        ("prompt", Some("id"))
    );
    assert_eq!(v.mapping.state_mode, StateMode::Text);

    let (_, body) = get(&gw, &format!("/decision-lab/api/batches/{}/items", v.id)).await;
    let page = round_trips::<BatchItemPage>("items", &body);
    assert_eq!(page.items.len(), 1);
    let item = page.items[0].item.as_ref().unwrap();
    assert_eq!(item.key.as_deref(), Some("m2"));
    assert_eq!(
        item.expected.get("category").map(String::as_str),
        Some("Billing"),
        "the owner's edit is the label"
    );
    assert_eq!(page.items[0].form["state"], json!("Subject: about m2"));
    let form_questions: Vec<&str> = page.items[0].form["questions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q["id"].as_str().unwrap())
        .collect();
    assert_eq!(form_questions, ["category"], "the request the agent sends");
    assert!(page.items[0].runs.is_empty());

    // Resume on the set-run page runs it through the set's question.
    let before = mails.bodies().len();
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/resume", v.id),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v = until(&gw, v.id, |v| v.status == BatchStatus::Done).await;
    assert_eq!((v.done, v.failed), (1, 0));
    let asked = &mails.bodies()[before];
    assert_eq!(asked["state"], json!("Subject: about m2"));
    let ids: Vec<&String> = asked["questions"].as_object().unwrap().keys().collect();
    assert_eq!(
        ids,
        ["category"],
        "Resume asks what the agent asks: {asked}"
    );

    // "Use as threshold" lands on that question of the saved set, which
    // keeps its other question.
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/threshold", v.id),
        json!({ "question": "category", "value": 0.0 }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let written = round_trips::<LabPresetView>("threshold", &body);
    let lmgw_api_types::lab_store::LabPresetBody::QuestionSet(now) = &written.preset else {
        panic!("a question set: {written:?}");
    };
    assert_eq!(now.questions.len(), 2);
    let t = now.thresholds.get("category").unwrap();
    assert_eq!(
        (t.value, t.measured.as_ref().map(|m| m.batch_id)),
        (0.0, Some(v.id))
    );
    assert_eq!(now.thresholds.get("urgent").map(|t| t.value), Some(0.6));

    // The export carries the label and whether the answer (Work) was right.
    let csv_out = text(
        &gw,
        &format!("/labs/api/decision/export?format=csv&batch={}", v.id),
    )
    .await;
    let mut rdr = ::csv::Reader::from_reader(csv_out.as_bytes());
    let header: Vec<String> = rdr.headers().unwrap().iter().map(String::from).collect();
    let col = |name: &str| header.iter().position(|h| h == name).unwrap();
    let rows: Vec<::csv::StringRecord> = rdr.records().map(Result::unwrap).collect();
    assert_eq!(rows.len(), 1, "{csv_out}");
    let r = &rows[0];
    assert_eq!(&r[col("question_id")], "category");
    assert_eq!(&r[col("item_key")], "m2");
    assert_eq!(&r[col("expected")], "Billing");
    assert_eq!(&r[col("correct")], "false");
    assert_eq!(&r[col("state")], "Subject: about m2");
}

/// An apply job as the store keeps one, finished with `status`.
async fn fixture(state: &lmgw_core::state::SharedState, rows: Value, status: &str) -> i64 {
    let input = json!({ "agent_id": "decider", "phase": "apply", "rows": rows });
    let id = store::claim_job(
        &state.db,
        "agent_run",
        Some("agent:decider"),
        "decider · apply",
        &input.to_string(),
    )
    .await
    .unwrap()
    .expect("no other run of the agent is live");
    if status != "queued" {
        store::finish_job(&state.db, id, status, Some("{}"), None)
            .await
            .unwrap();
    }
    id
}

/// A row as Apply received it: asked (`prompt`), labelled `category`,
/// stamped with `set` (null: the model call classified it).
fn asked(id: &str, prompt: &str, category: Value, set: Value) -> Value {
    json!({ "id": id, "prompt": prompt, "output": { "category": category }, "set": set })
}

/// The stamp of preset `id` named `name`, read at `updated_at`.
fn stamp(id: i64, name: &str, updated_at: &str) -> Value {
    json!({ "id": id, "name": name, "updated_at": updated_at })
}

/// [`stamp`] with the output question's digest.
fn stamp_digest(id: i64, name: &str, updated_at: &str, digest: &str) -> Value {
    json!({ "id": id, "name": name, "updated_at": updated_at, "digest": digest })
}

async fn from_agent(gw: &Gw, body: Value) -> (u16, Value) {
    post(gw, "/decision-lab/api/batches/from-agent", body).await
}

#[tokio::test]
async fn rows_never_asked_are_skipped_and_a_label_the_set_lacks_refuses() {
    let mails = Mails::default();
    let (l, _calls) = world(&mails, "decider").await;
    let gw = l.gw().clone();
    let preset = save_set(&gw, "mail", &set(&l.w.name(CPU_ROW), 0.0)).await;
    let (_, view) = get(&gw, &format!("/labs/api/decision/presets/{preset}")).await;
    let now = view["updated_at"].as_str().unwrap().to_string();
    let mail = stamp(preset, "mail", &now);
    let never = json!({ "id": "r2", "error": "the tool 'gws__get' failed: gone",
                        "output": { "category": "Other" }, "attention": true });
    // Asked through the set, the call failed, the fallback left as it was.
    let failed = json!({ "id": "r5", "prompt": "Subject: five", "set": mail,
                         "error": "decide/laya answered 500: input is too large",
                         "output": { "category": "Other" }, "attention": true });

    // Still applying: not yet.
    let live = fixture(
        &l.w.state,
        json!([asked("r1", "Subject: one", json!("Finance"), mail.clone())]),
        "queued",
    )
    .await;
    let (status, body) = from_agent(&gw, json!({ "job_id": live })).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("run_live")),
        "{body}"
    );
    store::finish_job(&l.w.state.db, live, "canceled", None, None)
        .await
        .unwrap();

    // A label the set does not offer refuses the save, naming the row; the
    // failed call's untouched fallback is no label and does not.
    let rows = json!([
        asked("r1", "Subject: one", json!("Finance"), mail.clone()),
        never.clone(),
        asked("r3", "Subject: three", json!("Other"), mail.clone()),
        failed.clone(),
    ]);
    let job = fixture(&l.w.state, rows, "done").await;
    let (status, body) = from_agent(&gw, json!({ "job_id": job })).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (400, Some("bad_input")),
        "{body}"
    );
    let m = body["message"].as_str().unwrap();
    assert!(m.contains("row 3 (r3): 'Other' is no option"), "{m}");
    assert!(!m.contains("r1") && !m.contains("r5"), "{m}");

    // Without it: r2 was never asked and r5's call failed, both skipped with
    // why; r4 has no label and is an item all the same, r1 keeps Finance.
    let rows = json!([
        asked("r1", "Subject: one", json!("Finance"), mail.clone()),
        never,
        asked("r4", "Subject: four", Value::Null, mail.clone()),
        failed,
    ]);
    let job = fixture(&l.w.state, rows, "failed").await;
    let (status, body) = from_agent(&gw, json!({ "job_id": job })).await;
    assert_eq!(status, 200, "{body}");
    let saved = round_trips::<BatchFromAgentSaved>("from-agent", &body);
    let skipped: Vec<(&str, BatchFromAgentSkip)> = saved
        .skipped
        .iter()
        .map(|k| (k.id.as_str(), k.kind))
        .collect();
    assert_eq!(
        skipped,
        [
            ("r2", BatchFromAgentSkip::NeverAsked),
            ("r5", BatchFromAgentSkip::CallFailed)
        ]
    );
    assert!(
        saved.skipped[0].reason.contains("gone"),
        "{:?}",
        saved.skipped
    );
    assert!(
        saved.skipped[1].reason.starts_with("its call failed: ")
            && saved.skipped[1].reason.contains("too large"),
        "{:?}",
        saved.skipped
    );
    assert_eq!((saved.batch.total, saved.batch.labelled), (2, 1));
    assert_eq!(saved.batch.preset_id, Some(preset));

    // The JSONL export has both items, the unlabelled one without expected.
    let jsonl = text(
        &gw,
        &format!("/labs/api/decision/export?batch={}", saved.batch.id),
    )
    .await;
    let lines: Vec<lmgw_api_types::lab_store::LabExportLine> = jsonl
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let items: Vec<_> = lines
        .iter()
        .map(|l| l.group.item.clone().unwrap())
        .collect();
    assert_eq!(items.len(), 2);
    assert_eq!(
        (
            items[0].key.as_deref(),
            items[0].expected.get("category").map(String::as_str)
        ),
        (Some("r1"), Some("Finance"))
    );
    assert_eq!(items[1].key.as_deref(), Some("r4"));
    assert!(items[1].expected.is_empty());
}

#[tokio::test]
async fn the_set_is_the_one_stamped_on_the_rows_and_a_mix_is_refused_naming_them() {
    let mails = Mails::default();
    let (l, _calls) = world(&mails, "decider").await;
    let gw = l.gw().clone();
    let laya = l.w.name(CPU_ROW);
    let mail = save_set(&gw, "mail", &set(&laya, 0.0)).await;
    let other = save_set(&gw, "other", &set(&laya, 0.0)).await;
    let (_, view) = get(&gw, &format!("/labs/api/decision/presets/{mail}")).await;
    let now = view["updated_at"].as_str().unwrap().to_string();

    // Rows the model call classified name no set: the request must.
    let plain = fixture(
        &l.w.state,
        json!([asked("r1", "Subject: one", json!("Work"), Value::Null)]),
        "done",
    )
    .await;
    let (status, body) = from_agent(&gw, json!({ "job_id": plain })).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (400, Some("bad_request")),
        "{body}"
    );
    assert!(
        body["message"].as_str().unwrap().contains("preset_id"),
        "{body}"
    );
    // `preset_id` is the explicit override.
    let (status, body) = from_agent(&gw, json!({ "job_id": plain, "preset_id": other })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["batch"]["preset_name"], json!("other"));

    // Rows of two sets, and one of the model call: no one set's labels.
    let mixed = fixture(
        &l.w.state,
        json!([
            asked(
                "r1",
                "Subject: one",
                json!("Work"),
                stamp(mail, "mail", &now)
            ),
            asked(
                "r2",
                "Subject: two",
                json!("Work"),
                stamp(other, "other", &now)
            ),
            asked(
                "r3",
                "Subject: three",
                json!("Work"),
                stamp(mail, "mail", &now)
            ),
            asked("r4", "Subject: four", json!("Work"), Value::Null),
        ]),
        "done",
    )
    .await;
    let (status, body) = from_agent(&gw, json!({ "job_id": mixed })).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (400, Some("mixed_sets")),
        "{body}"
    );
    let m = body["message"].as_str().unwrap();
    assert!(
        m.contains(&format!("'mail' (set {mail}): rows r1, r3"))
            && m.contains(&format!("'other' (set {other}): row r2"))
            && m.contains("the model call, no set: row r4"),
        "{m}"
    );

    // One set, stamped at an older version of its question (a different
    // digest): saved into it, saying so.
    let older = fixture(
        &l.w.state,
        json!([
            asked(
                "r1",
                "Subject: one",
                json!("Work"),
                stamp_digest(mail, "mail", "2026-01-01T00:00:00.000Z", "stale")
            ),
            asked(
                "r2",
                "Subject: two",
                json!("Finance"),
                stamp(mail, "mail", &now)
            ),
        ]),
        "done",
    )
    .await;
    let (status, body) = from_agent(&gw, json!({ "job_id": older })).await;
    assert_eq!(status, 200, "{body}");
    let saved = round_trips::<BatchFromAgentSaved>("older", &body);
    assert_eq!(
        (saved.batch.preset_id, saved.batch.total, saved.set_changed),
        (Some(mail), 2, true)
    );

    // An older `updated_at` with the question's own digest (a threshold
    // written since): the question is as it was asked, so not changed.
    let question: QuestionDraft = serde_json::from_value(view["body"]["questions"][0].clone())
        .unwrap_or_else(|e| panic!("{e}: {view}"));
    let digest = lmgw_core::agents::batch::decide::question_digest(&question);
    let moved = fixture(
        &l.w.state,
        json!([asked(
            "r1",
            "Subject: one",
            json!("Work"),
            stamp_digest(mail, "mail", "2026-01-01T00:00:00.000Z", &digest)
        )]),
        "done",
    )
    .await;
    let (status, body) = from_agent(&gw, json!({ "job_id": moved })).await;
    assert_eq!(status, 200, "{body}");
    let saved = round_trips::<BatchFromAgentSaved>("moved", &body);
    assert!(!saved.set_changed, "{body}");
}
