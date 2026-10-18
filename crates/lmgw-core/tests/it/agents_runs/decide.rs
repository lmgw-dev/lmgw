//! A batch agent classifying through a question set (labs-store design §4,
//! §7.4) on `decision_lab`'s wiremock decision container: the output is the
//! set question's winner, stamped with the set; attention is the fallback,
//! the set's *measured* threshold (an unmeasured one is not applied) or a
//! failed call (the fallback then, llama.cpp's 500 verbatim as the error);
//! the state is the rendered `user` template with no `system` sent, the call
//! is a `systemone` row of the agents' identity folded into the run's meter,
//! and the review offers the set's options with the fallback last. An error
//! the model answered is the row's own and never aborts the run; lmgw's own
//! 5xx does; lmgw's answer for every row (the GPU hold) stops the run at once
//! with the rows done before it kept. Without a set the model call is byte
//! for byte what it was; a set whose question is no choice, or is not there,
//! and a classify with neither a model nor a set, refuse the run before any
//! tool call.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_api_types::decision_lab::{
    Measured, OptionDraft, QuestionDraft, QuestionKind, QuestionSet, Threshold,
};
use lmgw_api_types::lab_store::LabPresetView;
use lmgw_api_types::AgentRunDetail;
use lmgw_core::config::Protocol;
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::{gateway, get_json, install, mcp_stub, mcp_stub_of, op, register_stub, settle};
use crate::common::{round_trips, serve, Gw};
use crate::decision_lab::{lab, post, Lab, CPU_ROW};
use crate::support::decision_world::DECISION;

/// The categories the set offers, in its order.
pub(super) const OPTIONS: [&str; 3] = ["Work", "Finance", "Billing"];

/// The test agent of `agents_runs` with a question-set field and `decide`,
/// and a direct-call apply step (it writes nothing anywhere).
pub(super) fn decide_doc(id: &str) -> String {
    decide_doc_with(id, r#", "default": "m1""#)
}

/// [`decide_doc`] with `model_default` written after the model field's type
/// (empty: no default).
fn decide_doc_with(id: &str, model_default: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "{id}",
  "name": "Decider",
  "model": {{ "alias": "{{{{config.model}}}}", "temperature": 0.0 }},
  "config": {{ "schema": {{ "type": "object", "properties": {{
    "model":        {{ "type": "string", "title": "Model"{model_default} }},
    "decision_set": {{ "type": "integer", "format": "lmgw-question-set" }},
    "categories":   {{ "type": "array", "items": {{ "type": "string" }},
                       "default": ["Work", "Finance"] }}
  }} }} }},
  "tools": [ {{ "label": "gws" }} ],
  "run": {{ "kind": "batch",
    "source": {{ "tool": "gws__search", "args": {{ "q": "is:unread" }} }},
    "items_path": "/messages",
    "item": {{
      "id": "{{{{item.id}}}}",
      "fetch": {{ "tool": "gws__get", "args": {{ "messageId": "{{{{item.id}}}}" }} }},
      "columns": {{ "subject": "{{{{fetched.subject}}}}" }},
      "system": "Pick one of: {{{{config.categories}}}}.",
      "user": "Subject: {{{{fetched.subject}}}}",
      "output": {{ "field": "category", "enum_from": "config.categories",
                   "fallback": "Other", "decide": "config.decision_set" }}
    }},
    "review": {{ "editable": ["category"] }},
    "apply": {{ "tool": "gws__get", "args": {{ "messageId": "applied" }} }}
  }}
}}"#
    )
}

/// A set whose question `category` is a choice of [`OPTIONS`], asked of
/// `model`, with `threshold` on it, not measured.
pub(super) fn set(model: &str, threshold: f64) -> QuestionSet {
    QuestionSet {
        questions: vec![QuestionDraft {
            id: "category".into(),
            instructions: "Which category is this mail?".into(),
            kind: QuestionKind::Choice {
                options: OPTIONS
                    .map(|n| OptionDraft {
                        name: n.into(),
                        description: String::new(),
                    })
                    .to_vec(),
            },
        }],
        default_model: model.into(),
        thresholds: [(
            "category",
            Threshold {
                value: threshold,
                measured: None,
            },
        )]
        .into_iter()
        .collect(),
    }
}

/// [`set`] with its threshold measured on a (made-up) labelled set run.
pub(super) fn measured_set(model: &str, threshold: f64) -> QuestionSet {
    let mut s = set(model, threshold);
    s.thresholds.0[0].1.measured = Some(Measured {
        batch_id: 1,
        coverage: 0.8,
        accuracy: 0.9,
        labelled: 10,
    });
    s
}

/// Save `set` as `name`; its id.
pub(super) async fn save_set(gw: &Gw, name: &str, set: &QuestionSet) -> i64 {
    let (status, body) = post(
        gw,
        "/labs/api/decision/presets",
        json!({"name": name, "kind": "question_set", "body": set}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    round_trips::<LabPresetView>("preset", &body).id
}

/// llama.cpp's 500 for a state over the ubatch, spaced as it prints it.
pub(super) const LLAMA_500: &str = r#"{"error": {"code": 500, "message": "input (8519 tokens) is too large to process. increase the physical batch size", "type": "server_error"}}"#;

/// The container's answer for a state: `about m1` is Finance at 0.9, any
/// other state Work at 0.5, a state named in `other` Other at 0.95; a state
/// named in `fail` gets llama.cpp's 500 (the model's own error, relayed),
/// one named in `missing` the 404 of an image without the route (which lmgw
/// answers as its own 501); one named in `slow` is answered after that
/// long. Every body it is sent is kept.
#[derive(Clone, Default)]
pub(super) struct Mails {
    pub bodies: Arc<Mutex<Vec<String>>>,
    pub fail: Arc<Mutex<Vec<String>>>,
    pub missing: Arc<Mutex<Vec<String>>>,
    /// A state named here gets a relayed 503 (a server still loading).
    pub unavailable: Arc<Mutex<Vec<String>>>,
    pub other: Arc<Mutex<Vec<String>>>,
    pub slow: Arc<Mutex<Vec<(String, Duration)>>>,
}

impl Mails {
    pub fn bodies(&self) -> Vec<Value> {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .map(|b| serde_json::from_str(b).unwrap())
            .collect()
    }
}

/// The answer naming `choice` with `confidence`, over [`OPTIONS`] and the
/// choice when it is none of them.
pub(super) fn answer(choice: &str, confidence: f64) -> String {
    let mut names = OPTIONS.to_vec();
    if !names.contains(&choice) {
        names.push(choice);
    }
    let rest = (1.0 - confidence) / (names.len() - 1) as f64;
    let probs: Vec<String> = names
        .iter()
        .map(|o| {
            let p = if *o == choice { confidence } else { rest };
            format!("\"{o}\": {p}")
        })
        .collect();
    format!(
        r#"{{"model": "laya", "answers": {{"category": {{"type": "choice", "choice": "{choice}",
            "probabilities": {{{}}}, "confidence": {confidence}}}}},
            "usage": {{"input_tokens": 12, "output_tokens": 0}}}}"#,
        probs.join(", ")
    )
}

impl Respond for Mails {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let text = String::from_utf8_lossy(&req.body).to_string();
        self.bodies.lock().unwrap().push(text.clone());
        let body: Value = serde_json::from_str(&text).unwrap_or_default();
        let state = body["state"].as_str().unwrap_or_default().to_string();
        let named = |list: &Mutex<Vec<String>>| list.lock().unwrap().contains(&state);
        let t = if named(&self.fail) {
            ResponseTemplate::new(500).set_body_raw(LLAMA_500, "application/json")
        } else if named(&self.unavailable) {
            ResponseTemplate::new(503).set_body_raw(
                r#"{"error": {"code": 503, "message": "Loading model", "type": "unavailable_error"}}"#,
                "application/json",
            )
        } else if named(&self.missing) {
            ResponseTemplate::new(404).set_body_string("File Not Found")
        } else if named(&self.other) {
            ResponseTemplate::new(200).set_body_raw(answer("Other", 0.95), "application/json")
        } else if state == "Subject: about m1" {
            ResponseTemplate::new(200).set_body_raw(answer("Finance", 0.9), "application/json")
        } else {
            ResponseTemplate::new(200).set_body_raw(answer("Work", 0.5), "application/json")
        };
        match self.slow.lock().unwrap().iter().find(|(f, _)| state == *f) {
            Some((_, d)) => t.set_delay(*d),
            None => t,
        }
    }
}

/// `decision_lab`'s lab with its container answering through `mails`, the
/// MCP stub registered, and the decide agent `id` installed.
pub(super) async fn world(mails: &Mails, id: &str) -> (Lab, Arc<std::sync::atomic::AtomicUsize>) {
    world_of(mails, id, 2).await
}

/// [`world`] whose mailbox lists `n` messages, `m1` to `m<n>`.
async fn world_of(mails: &Mails, id: &str, n: usize) -> (Lab, Arc<std::sync::atomic::AtomicUsize>) {
    let l = lab().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(mails.clone())
        .with_priority(1)
        .mount(&l.container)
        .await;
    let (url, calls) = if n == 2 {
        mcp_stub().await
    } else {
        mcp_stub_of(n).await
    };
    register_stub(&l.w.state, &url).await;
    install(l.gw(), &decide_doc(id)).await;
    (l, calls)
}

/// The prompt mail `m` gets: the decide agent's `user` template.
fn state_of(m: &str) -> String {
    format!("Subject: about {m}")
}

/// Run phase `phase` of agent `id` with `values`; the finished job's id.
pub(super) async fn run(l: &Lab, id: &str, phase: &str, extra: Value) -> i64 {
    let mut args = json!({ "id": id, "phase": phase });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        args[k] = v;
    }
    let (status, res) = op(l.gw(), "agent_run", args).await;
    assert_eq!(status, 200, "{res}");
    let job_id = res["job_id"].as_i64().unwrap();
    settle(&l.w.state, job_id).await;
    job_id
}

async fn detail(gw: &Gw, job_id: i64) -> AgentRunDetail {
    let v = get_json(gw, &format!("/api/agents/runs/{job_id}")).await;
    serde_json::from_value(v).unwrap()
}

fn row<'a>(d: &'a AgentRunDetail, id: &str) -> &'a Value {
    d.rows
        .iter()
        .find(|r| r["id"] == json!(id))
        .unwrap_or_else(|| panic!("no row {id}: {:?}", d.rows))
}

#[tokio::test]
async fn the_winner_is_the_output_and_the_measured_threshold_is_attention() {
    let mails = Mails::default();
    let (l, _calls) = world(&mails, "decider").await;
    let laya = l.w.name(CPU_ROW);
    let preset = save_set(l.gw(), "mail", &measured_set(&laya, 0.7)).await;
    let (status, res) = op(
        l.gw(),
        "agent_config_set",
        json!({ "id": "decider", "values": { "decision_set": preset } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");

    let job_id = run(&l, "decider", "classify", json!({})).await;
    let d = detail(l.gw(), job_id).await;
    assert_eq!(d.job.status, "done", "{:?}", d.job.error);
    // m1: Finance at 0.9, over the 0.7 cut.
    let m1 = row(&d, "m1");
    assert_eq!(m1["output"], json!({"category": "Finance"}));
    assert_eq!(m1["attention"], json!(false));
    assert!(m1.get("error").is_none(), "{m1}");
    assert_eq!(m1["prompt"], json!("Subject: about m1"));
    let raw = m1["raw"].as_str().unwrap();
    assert!(
        raw.contains("\"probabilities\"") && raw.contains("0.9"),
        "{raw}"
    );
    // m2: Work at 0.5, under it: an attention row with the winner kept.
    let m2 = row(&d, "m2");
    assert_eq!(m2["output"], json!({"category": "Work"}));
    assert_eq!(m2["attention"], json!(true));
    assert!(m2.get("error").is_none(), "{m2}");
    // Each row names the set that classified it, as the run read it.
    let (_, saved) =
        super::labelled_set::get(l.gw(), &format!("/labs/api/decision/presets/{preset}")).await;
    for r in [m1, m2] {
        assert_eq!(r["set"]["digest"].as_str().map(str::len), Some(64), "{r}");
        assert_eq!(
            r["set"],
            json!({
                "id": preset, "name": "mail", "updated_at": saved["updated_at"],
                "digest": r["set"]["digest"]
            }),
            "{r}"
        );
    }

    // The state is the rendered `user` template and only the output field's
    // question is asked; the system prompt is not sent.
    let bodies = mails.bodies();
    assert_eq!(bodies.len(), 2, "one call per item");
    for b in &bodies {
        let state = b["state"].as_str().unwrap();
        assert!(state.starts_with("Subject: about m"), "{b}");
        let qs: Vec<&String> = b["questions"].as_object().unwrap().keys().collect();
        assert_eq!(qs, ["category"]);
        assert_eq!(
            b["questions"]["category"]["criteria"],
            json!({"Work": null, "Finance": null, "Billing": null})
        );
        assert!(!b.to_string().contains("Pick one of"), "{b}");
    }

    // The set's model answered: no chat model was needed (this gateway
    // serves no `m1` at all), and the run reports the set's model.
    assert_eq!(d.result.as_ref().unwrap()["model"], json!(laya));
    // Each call is the agents' `systemone` row (written by a task of its
    // own), and the run's meter has it.
    let mut logs = Vec::new();
    for _ in 0..200 {
        logs = store::query_logs(&l.w.state.db, &Default::default())
            .await
            .unwrap();
        if logs
            .iter()
            .filter(|r| r.ingress_proto == "systemone")
            .count()
            >= 2
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let rows: Vec<_> = logs
        .iter()
        .filter(|r| r.ingress_proto == "systemone")
        .collect();
    assert_eq!(rows.len(), 2, "{logs:#?}");
    for r in &rows {
        assert_eq!(r.client_key.as_deref(), Some("internal:agents"));
        assert_eq!(r.status, 200);
    }
    let result = d.result.as_ref().unwrap();
    assert_eq!(result["model_calls"], json!(2), "{result}");
    assert_eq!(result["usage"]["prompt_tokens"], json!(24), "{result}");
    assert_eq!(result["attention"], json!(1));

    // The review offers the set's options with the fallback last, and says
    // the run decides through the set.
    let shape = d.batch.as_ref().unwrap();
    assert_eq!(
        shape.editable[0].options,
        ["Work", "Finance", "Billing", "Other"]
    );
    let decide = shape.decide.as_ref().unwrap();
    assert_eq!(
        (
            decide.field.as_str(),
            decide.question.as_str(),
            decide.enum_from.as_deref()
        ),
        ("decision_set", "category", Some("categories"))
    );
    assert_eq!(
        (
            decide.set,
            decide.set_name.as_deref(),
            decide.threshold,
            decide.measured
        ),
        (Some(preset), Some("mail"), Some(0.7), true)
    );
    assert_eq!(decide.model_field.as_deref(), Some("model"));
    assert_eq!(decide.problem, None);

    // A re-run asks only the attention row again, through the set.
    let rerun = run(&l, "decider", "rerun", json!({ "base_job": job_id })).await;
    let r = detail(l.gw(), rerun).await;
    assert_eq!(r.job.status, "done", "{:?}", r.job.error);
    assert_eq!(mails.bodies().len(), 3, "the attention row only");
    assert_eq!(row(&r, "m1")["output"], json!({"category": "Finance"}));
}

#[tokio::test]
async fn a_failed_call_is_an_attention_row_with_the_fallback_and_the_500_verbatim() {
    let mails = Mails::default();
    mails.fail.lock().unwrap().push(state_of("m1"));
    let (l, _calls) = world(&mails, "decider").await;
    let preset = save_set(l.gw(), "mail", &set(&l.w.name(CPU_ROW), 0.0)).await;

    let job_id = run(
        &l,
        "decider",
        "classify",
        json!({ "values": { "decision_set": preset } }),
    )
    .await;
    let d = detail(l.gw(), job_id).await;
    assert_eq!(d.job.status, "done", "{:?}", d.job.error);
    let m1 = row(&d, "m1");
    assert_eq!(m1["output"], json!({"category": "Other"}));
    assert_eq!(m1["attention"], json!(true));
    let error = m1["error"].as_str().unwrap();
    assert!(error.contains("answered 500"), "{error}");
    assert!(
        error.contains(
            "input (8519 tokens) is too large to process. increase the physical batch size"
        ),
        "llama.cpp's message whole: {error}"
    );
    // A failed call through the set still names the set it was asked of.
    assert_eq!(m1["set"]["id"], json!(preset));
    // No threshold: a call that answered never needs a look.
    let m2 = row(&d, "m2");
    assert_eq!(
        (m2["output"].clone(), m2["attention"].clone()),
        (json!({"category": "Work"}), json!(false))
    );
}

#[tokio::test]
async fn the_models_own_500s_never_abort_the_run_and_lmgws_own_5xx_still_do() {
    // Three long mails first, each over the ubatch: the model answered every
    // one, relayed as it came, so each is its own row's failure and the run
    // goes on to the fourth (the abort used to count them as an outage).
    let mails = Mails::default();
    for m in ["m1", "m2", "m3"] {
        mails.fail.lock().unwrap().push(state_of(m));
    }
    let (l, _calls) = world_of(&mails, "decider", 4).await;
    let preset = save_set(l.gw(), "mail", &set(&l.w.name(CPU_ROW), 0.0)).await;
    let values = json!({ "values": { "decision_set": preset } });
    let job_id = run(&l, "decider", "classify", values.clone()).await;
    let d = detail(l.gw(), job_id).await;
    assert_eq!(d.job.status, "done", "{:?}", d.job.error);
    for m in ["m1", "m2", "m3"] {
        let r = row(&d, m);
        assert_eq!(r["output"], json!({"category": "Other"}), "{r}");
        assert_eq!(r["attention"], json!(true), "{r}");
        let e = r["error"].as_str().unwrap();
        assert!(
            e.contains("answered 500") && e.contains("increase the physical batch size"),
            "{e}"
        );
    }
    assert_eq!(row(&d, "m4")["output"], json!({"category": "Work"}));
    assert_eq!(mails.bodies().len(), 4, "every row was asked");

    // lmgw's own 5xx (here the 501 for an image without the route) is the
    // outage the early abort is for: three before any answer end the run.
    mails.fail.lock().unwrap().clear();
    for m in ["m1", "m2", "m3", "m4"] {
        mails.missing.lock().unwrap().push(state_of(m));
    }
    let job_id = run(&l, "decider", "classify", values).await;
    let d = detail(l.gw(), job_id).await;
    assert_eq!(d.job.status, "failed");
    let e = d.job.error.unwrap_or_default();
    assert!(
        e.starts_with("model unavailable") && e.contains("501"),
        "{e}"
    );
}

#[tokio::test]
async fn a_relayed_503_counts_toward_the_early_abort_and_three_ubatch_500s_do_not() {
    // A server still loading answers 503 with no lmgw code: not a row's own
    // failure, the outage the abort is for. Three before any answer end the
    // run (the ubatch 500s of the test above never do).
    let mails = Mails::default();
    for m in ["m1", "m2", "m3", "m4"] {
        mails.unavailable.lock().unwrap().push(state_of(m));
    }
    let (l, _calls) = world_of(&mails, "decider", 4).await;
    let preset = save_set(l.gw(), "mail", &set(&l.w.name(CPU_ROW), 0.0)).await;
    let job_id = run(
        &l,
        "decider",
        "classify",
        json!({ "values": { "decision_set": preset } }),
    )
    .await;
    let d = detail(l.gw(), job_id).await;
    assert_eq!(d.job.status, "failed", "{:?}", d.job.error);
    let e = d.job.error.unwrap_or_default();
    assert!(
        e.starts_with("model unavailable") && e.contains("503"),
        "{e}"
    );
}

#[tokio::test]
async fn the_hold_already_on_refuses_a_set_classify_before_any_tool_call() {
    let mails = Mails::default();
    let (l, calls) = world_of(&mails, "decider", 3).await;
    let preset = save_set(l.gw(), "mail", &set(&l.w.name(DECISION), 0.0)).await;
    lmgw_core::ops::hold_set(&l.w.state, true).await.unwrap();
    let job_id = run(
        &l,
        "decider",
        "classify",
        json!({ "values": { "decision_set": preset } }),
    )
    .await;
    let job = store::get_job(&l.w.state.db, job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.status, "failed");
    let e = job.error.unwrap_or_default();
    assert!(e.starts_with("model '") && e.contains("hold"), "{e}");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "a tool was called");
    assert!(mails.bodies().is_empty());
    lmgw_core::ops::hold_set(&l.w.state, false).await.unwrap();
}

#[tokio::test]
async fn a_rerun_stopped_by_the_hold_keeps_the_rows_it_never_re_asked_as_they_were() {
    // The first run fails every mail's call: three attention rows with the
    // fallback. The re-run asks them again; m1 is slow and the hold goes on
    // while it is in flight, so m1 is re-answered and m2, m3 are not asked.
    let mails = Mails::default();
    for m in ["m1", "m2", "m3"] {
        mails.fail.lock().unwrap().push(state_of(m));
    }
    let (l, _calls) = world_of(&mails, "decider", 3).await;
    let preset = save_set(l.gw(), "mail", &set(&l.w.name(DECISION), 0.0)).await;
    let values = json!({ "values": { "decision_set": preset } });
    let first = run(&l, "decider", "classify", values.clone()).await;
    let d = detail(l.gw(), first).await;
    assert_eq!(d.job.status, "done", "{:?}", d.job.error);
    mails.fail.lock().unwrap().clear();
    mails
        .slow
        .lock()
        .unwrap()
        .push((state_of("m1"), Duration::from_millis(600)));
    let before = mails.bodies().len();
    let (status, res) = op(
        l.gw(),
        "agent_run",
        json!({ "id": "decider", "phase": "rerun", "base_job": first, "values": values["values"] }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let job_id = res["job_id"].as_i64().unwrap();
    for _ in 0..400 {
        if mails.bodies().len() > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    lmgw_core::ops::hold_set(&l.w.state, true).await.unwrap();
    let job = settle(&l.w.state, job_id).await;
    assert_eq!(job.status, "failed", "{:?}", job.error);
    let d = detail(l.gw(), job_id).await;
    assert_eq!(
        row(&d, "m1")["output"],
        json!({"category": "Finance"}),
        "re-asked"
    );
    for m in ["m2", "m3"] {
        let r = row(&d, m);
        assert_eq!(r["output"], json!({"category": "Other"}), "{r}");
        assert_eq!(r["attention"], json!(true), "{r}");
        assert!(r["error"].as_str().unwrap().contains("answered 500"), "{r}");
        assert!(r.get("prompt").is_some() && r.get("set").is_some(), "{r}");
    }
    lmgw_core::ops::hold_set(&l.w.state, false).await.unwrap();
}

#[tokio::test]
async fn the_hold_going_on_mid_run_stops_the_classify_and_keeps_the_rows_done() {
    // The set asks the GPU row; m1's answer is slow, and the hold goes on
    // while it is in flight. m1 is answered; m2 would get the hold's 503, as
    // every row after it would: the run fails with that at once.
    let mails = Mails::default();
    mails
        .slow
        .lock()
        .unwrap()
        .push((state_of("m1"), Duration::from_millis(600)));
    let (l, calls) = world_of(&mails, "decider", 3).await;
    let preset = save_set(l.gw(), "mail", &set(&l.w.name(DECISION), 0.0)).await;
    let (status, res) = op(
        l.gw(),
        "agent_run",
        json!({ "id": "decider", "phase": "classify", "values": { "decision_set": preset } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let job_id = res["job_id"].as_i64().unwrap();
    for _ in 0..400 {
        if !mails.bodies().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(mails.bodies().len(), 1, "m1 is in flight");
    lmgw_core::ops::hold_set(&l.w.state, true).await.unwrap();
    let job = settle(&l.w.state, job_id).await;
    assert_eq!(job.status, "failed");
    let e = job.error.unwrap_or_default();
    assert!(
        e.starts_with("stopped at row m2: lmgw could not ask")
            && e.contains("(503 gpu_hold)")
            && e.contains("The 1 of 3 rows done before it are kept"),
        "{e}"
    );
    assert_eq!(
        mails.bodies().len(),
        1,
        "nothing else reached the container"
    );

    // The rows: m1 answered and kept, m2 and m3 as listed, as a cancel
    // leaves the rows it never reached.
    let d = detail(l.gw(), job_id).await;
    let m1 = row(&d, "m1");
    assert_eq!(m1["output"], json!({"category": "Finance"}), "{m1}");
    assert_eq!(m1["set"]["id"], json!(preset));
    for m in ["m2", "m3"] {
        let r = row(&d, m);
        assert_eq!(r["output"], Value::Null, "{r}");
        assert_eq!(r["attention"], json!(false), "{r}");
        assert!(
            r.get("error").is_none() && r.get("prompt").is_none() && r.get("set").is_none(),
            "{r}"
        );
    }
    // The tools ran as far as the fetches: the source and three gets.
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    lmgw_core::ops::hold_set(&l.w.state, false).await.unwrap();
}

#[tokio::test]
async fn an_unmeasured_threshold_is_shown_not_applied_and_the_fallback_needs_a_look() {
    let mails = Mails::default();
    mails.other.lock().unwrap().push(state_of("m1"));
    let (l, _calls) = world(&mails, "decider").await;
    // A set that offers the fallback, its 0.7 not measured.
    let mut s = set(&l.w.name(CPU_ROW), 0.7);
    if let QuestionKind::Choice { options } = &mut s.questions[0].kind {
        options.push(OptionDraft {
            name: "Other".into(),
            description: String::new(),
        });
    }
    let preset = save_set(l.gw(), "mail", &s).await;
    // Stored, so the Run tab's view reads it too.
    let (status, res) = op(
        l.gw(),
        "agent_config_set",
        json!({ "id": "decider", "values": { "decision_set": preset } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let job_id = run(&l, "decider", "classify", json!({})).await;
    let d = detail(l.gw(), job_id).await;
    assert_eq!(d.job.status, "done", "{:?}", d.job.error);
    // m1: the model answered Other at 0.95, no failure: still a look, as an
    // answer of the fallback is on the model call's path.
    let m1 = row(&d, "m1");
    assert_eq!(
        (m1["output"].clone(), m1["attention"].clone()),
        (json!({"category": "Other"}), json!(true))
    );
    assert!(m1.get("error").is_none(), "{m1}");
    // m2: Work at 0.5, under the 0.7 that is not measured: not applied.
    let m2 = row(&d, "m2");
    assert_eq!(
        (m2["output"].clone(), m2["attention"].clone()),
        (json!({"category": "Work"}), json!(false))
    );
    // The Run tab shows the kept value and that it is not measured.
    let decide = d.batch.as_ref().unwrap().decide.as_ref().unwrap();
    assert_eq!((decide.threshold, decide.measured), (Some(0.7), false));
}

#[tokio::test]
async fn a_classify_with_neither_a_model_nor_a_set_says_both_before_any_tool_call() {
    let mails = Mails::default();
    let (l, calls) = world(&mails, "decider").await;
    install(l.gw(), &decide_doc_with("bare", "")).await;
    let job_id = run(&l, "bare", "classify", json!({})).await;
    let row = store::get_job(&l.w.state.db, job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "failed");
    let e = row.error.unwrap_or_default();
    assert!(
        e.contains("this agent has no model and no question set")
            && e.contains("'decision_set' names no set")
            && e.contains("Pick a model or a question set"),
        "{e}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "a tool was called");
    // The Run tab knows the model field, to block Classify without either.
    let d = get_json(l.gw(), "/api/agents/bare").await;
    assert_eq!(d["batch"]["decide"]["model_field"], json!("model"));
}

#[tokio::test]
async fn without_a_set_the_model_call_is_byte_for_byte_what_it_was() {
    let chat = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    super::openai_sse(r#"{"category":"Finance"}"#),
                    "text/event-stream",
                ),
        )
        .mount(&chat)
        .await;
    let state = gateway(&chat.uri(), Protocol::Openai).await;
    let (url, _calls) = mcp_stub().await;
    register_stub(&state, &url).await;
    let base = serve(state.clone()).await;
    install(&base, &super::doc("m1")).await;
    install(&base, &decide_doc("decider")).await;

    let mut outputs = Vec::new();
    for id in ["labeler", "decider"] {
        let (_, res) = op(&base, "agent_run", json!({ "id": id, "phase": "classify" })).await;
        let job_id = res["job_id"].as_i64().unwrap();
        let row = settle(&state, job_id).await;
        assert_eq!(row.status, "done", "{id}: {:?}", row.error);
        let d = get_json(&base, &format!("/api/agents/runs/{job_id}")).await;
        outputs.push(d["rows"].clone());
    }
    let sent: Vec<Vec<u8>> = chat
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.body)
        .collect();
    assert_eq!(sent.len(), 4, "two items, two agents");
    assert_eq!(sent[0], sent[2], "item 1 differs");
    assert_eq!(sent[1], sent[3], "item 2 differs");
    assert_eq!(outputs[0], outputs[1]);
    let enum_ = &serde_json::from_slice::<Value>(&sent[2]).unwrap()["response_format"]
        ["json_schema"]["schema"]["properties"]["category"]["enum"];
    assert_eq!(enum_, &json!(["Work", "Finance", "Other"]));

    // The review is the categories' and says no set is chosen.
    let d = get_json(&base, "/api/agents/decider").await;
    assert_eq!(
        d["batch"]["editable"][0]["options"],
        json!(["Work", "Finance", "Other"])
    );
    assert_eq!(d["batch"]["decide"]["set"], Value::Null);
    assert_eq!(d["batch"]["decide"]["question"], json!("category"));
    let d = get_json(&base, "/api/agents/labeler").await;
    assert_eq!(d["batch"]["decide"], Value::Null);
}

#[tokio::test]
async fn a_set_that_cannot_answer_the_field_refuses_the_run_before_any_tool_call() {
    let mails = Mails::default();
    let (l, calls) = world(&mails, "decider").await;
    let laya = l.w.name(CPU_ROW);
    let mut noul = set(&laya, 0.0);
    noul.questions[0].kind = QuestionKind::Noul {
        yes: String::new(),
        no: String::new(),
    };
    let mut other = set(&laya, 0.0);
    other.questions[0].id = "team".into();
    other.thresholds = Default::default();
    let mut unserved = set("decide/nobody", 0.0);
    unserved.thresholds = Default::default();
    let cases = [
        (save_set(l.gw(), "noul", &noul).await, "as a noul question"),
        (
            save_set(l.gw(), "team", &other).await,
            "has no question 'category'",
        ),
        (
            save_set(l.gw(), "unserved", &unserved).await,
            "answers no /v1/systemone",
        ),
        (9999, "question set 9999 is not there"),
    ];
    for (preset, says) in cases {
        let job_id = run(
            &l,
            "decider",
            "classify",
            json!({ "values": { "decision_set": preset } }),
        )
        .await;
        let row = store::get_job(&l.w.state.db, job_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "failed");
        let e = row.error.unwrap_or_default();
        assert!(e.contains(says), "{says}: {e}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0, "a tool was called");
    assert!(mails.bodies().is_empty());

    // The Run tab says why before a run does.
    let (status, res) = op(
        l.gw(),
        "agent_config_set",
        json!({ "id": "decider", "values": { "decision_set": cases[0].0 } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let d = get_json(l.gw(), "/api/agents/decider").await;
    let problem = d["batch"]["decide"]["problem"].as_str().unwrap();
    assert!(problem.contains("choice question"), "{problem}");
    assert_eq!(
        d["batch"]["editable"][0]["options"],
        json!(["Work", "Finance", "Other"]),
        "an unusable set leaves the categories' values"
    );
}
