//! A set run's input through Start (labs-store design §3.5): a JSONL file
//! with its key column; a score's label column read as a whole, refused
//! when both readings fit with different meanings until `score_labels`
//! says which; an empty state mapping refused; and the key, the run and
//! the item's index in both exports.

use lmgw_api_types::decision_lab::{
    BatchItemPage, BatchMetrics, BatchPreviewed, BatchStatus, QuestionDraft, QuestionKind,
};
use lmgw_api_types::lab_store::LabExportLine;
use serde_json::json;

use super::*;

/// Start a set run of `preset` with `body`'s file and mapping.
async fn start_with(gw: &Gw, preset: i64, body: Value) -> (u16, Value) {
    let mut body = body;
    body["preset_id"] = json!(preset);
    post(gw, "/decision-lab/api/batches", body).await
}

#[tokio::test]
async fn a_jsonl_file_runs_through_start_with_its_key() {
    let items = Items::new(Duration::ZERO);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let preset = save_set(&gw, "triage", &set(&l.w.name(CPU_ROW))).await;
    let mut text = String::new();
    for i in 0..6 {
        // The key is a number in one line, a string in the next; a label
        // may be JSON's own true and false; one row has no team label.
        let team = if i == 4 {
            json!(null)
        } else {
            json!(team_label(i))
        };
        let angry = angry_label(i).map(|a| a == "yes");
        let id = if i % 2 == 0 {
            json!(1000 + i)
        } else {
            json!(format!("T-{i}"))
        };
        text.push_str(
            &json!({"id": id, "text": format!("ticket {i}"), "team": team, "angry": angry})
                .to_string(),
        );
        text.push('\n');
    }
    let (status, body) = start_with(
        &gw,
        preset,
        json!({"format": "jsonl", "text": text, "source_name": "tickets.jsonl",
               "mapping": {"state": "text", "key": "id",
                           "labels": {"team": "team", "angry": "angry"}}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v = round_trips::<BatchView>("start", &body);
    assert_eq!(v.mapping.key.as_deref(), Some("id"));
    let v = until(&gw, v.id, |v| v.status == BatchStatus::Done).await;
    assert_eq!((v.done, v.failed, v.labelled), (6, 0, 6));
    assert_eq!(items.seen(), 6);
    let (_, body) = get(&gw, &format!("/decision-lab/api/batches/{}/items", v.id)).await;
    let page = round_trips::<BatchItemPage>("items", &body);
    let read: Vec<(Option<&str>, Option<&str>, Option<&str>)> = page
        .items
        .iter()
        .map(|g| {
            let item = g.item.as_ref().unwrap();
            (
                item.key.as_deref(),
                item.expected.get("team").map(String::as_str),
                item.expected.get("angry").map(String::as_str),
            )
        })
        .collect();
    assert_eq!(read[0], (Some("1000"), Some(team_label(0)), Some("yes")));
    assert_eq!(read[1], (Some("T-1"), Some(team_label(1)), Some("no")));
    assert_eq!(read[4].1, None, "null is no label");
}

/// A set of one score question on a scale written "1" to "5", for model
/// `model`.
fn stars(model: &str) -> QuestionSet {
    QuestionSet {
        questions: vec![QuestionDraft {
            id: "stars".into(),
            instructions: "How happy is the customer?".into(),
            kind: QuestionKind::Score {
                levels: ["1", "2", "3", "4", "5"].map(String::from).to_vec(),
            },
        }],
        default_model: model.into(),
        thresholds: Default::default(),
    }
}

#[tokio::test]
async fn a_score_column_is_read_as_a_whole_and_an_ambiguous_one_is_refused() {
    let items = Items::new(Duration::ZERO);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let laya = l.w.name(CPU_ROW);
    let preset = save_set(&gw, "stars", &stars(&laya)).await;
    let mapping = |extra: Value| {
        let mut m = json!({"state": "text", "labels": {"stars": "stars"}});
        if let Value::Object(o) = extra {
            for (k, v) in o {
                m[k] = v;
            }
        }
        m
    };
    let ambiguous = "text,stars\nticket 0,1\nticket 1,2\nticket 2,4\n";

    // "1", "2" and "4" are level texts and level indices that disagree.
    let (status, body) = post(
        &gw,
        "/decision-lab/api/batches/preview",
        json!({"format": "csv", "text": ambiguous, "questions": stars(&laya).questions,
               "mapping": mapping(json!({}))}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let p = round_trips::<BatchPreviewed>("preview", &body);
    assert_eq!(p.problems.len(), 1, "{:?}", p.problems);
    assert!(
        p.problems[0]
            .message
            .contains("set score_labels for 'stars' to text or index"),
        "{}",
        p.problems[0].message
    );
    let (status, body) = start_with(
        &gw,
        preset,
        json!({"format": "csv", "text": ambiguous, "mapping": mapping(json!({}))}),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (400, Some("bad_input")),
        "{body}"
    );
    assert_eq!(items.seen(), 0);

    // The mapping settles it: as texts, "4" is level 3.
    let (status, body) = start_with(
        &gw,
        preset,
        json!({"format": "csv", "text": ambiguous,
               "mapping": mapping(json!({"score_labels": {"stars": "text"}}))}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v = round_trips::<BatchView>("start", &body);
    let labels = |page: &BatchItemPage| -> Vec<String> {
        page.items
            .iter()
            .map(|g| {
                g.item
                    .as_ref()
                    .unwrap()
                    .expected
                    .get("stars")
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    };
    let (_, body) = get(&gw, &format!("/decision-lab/api/batches/{}/items", v.id)).await;
    assert_eq!(
        labels(&serde_json::from_value(body).unwrap()),
        ["0", "1", "3"]
    );

    // A column of 0-based indices reads as indices with no word from the
    // mapping: "0" is no level's text.
    let (status, body) = start_with(
        &gw,
        preset,
        json!({"format": "csv", "text": "text,stars\nticket 0,0\nticket 1,1\nticket 2,4\n",
               "mapping": mapping(json!({}))}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let w = round_trips::<BatchView>("start", &body);
    let (_, body) = get(&gw, &format!("/decision-lab/api/batches/{}/items", w.id)).await;
    assert_eq!(
        labels(&serde_json::from_value(body).unwrap()),
        ["0", "1", "4"]
    );

    // The container answers no stars question: each 2xx run is unreadable,
    // in the header and in the metrics alike, and none failed.
    let w = until(&gw, w.id, |v| v.status == BatchStatus::Done).await;
    assert_eq!((w.done, w.failed, w.unreadable), (3, 0, 3));
    let (_, body) = get(&gw, &format!("/decision-lab/api/batches/{}/metrics", w.id)).await;
    let m = round_trips::<BatchMetrics>("metrics", &body);
    assert_eq!(
        (
            m.questions[0].answered,
            m.questions[0].failed,
            m.questions[0].unreadable
        ),
        (0, 0, 3)
    );
}

#[tokio::test]
async fn an_empty_state_mapping_is_refused_naming_the_missing_column() {
    let items = Items::new(Duration::ZERO);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let preset = save_set(&gw, "triage", &set(&l.w.name(CPU_ROW))).await;
    // pandas' index column has an empty header; an empty state names it
    // no more.
    let (status, body) = start_with(
        &gw,
        preset,
        json!({"format": "csv", "text": ",text\n0,ticket 0\n", "mapping": {"labels": {}}}),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (400, Some("bad_input")),
        "{body}"
    );
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("state: the mapping names no state column"),
        "{body}"
    );
    assert_eq!(items.seen(), 0);
}

#[tokio::test]
async fn the_key_the_run_and_the_index_are_in_both_exports() {
    let items = Items::new(Duration::ZERO);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let preset = save_set(&gw, "triage", &set(&l.w.name(CPU_ROW))).await;
    let file = "ref,text,team\nA-7,ticket 0,billing\n,ticket 1,technical\nC-9,ticket 2,other\n";
    let (status, body) = start_with(
        &gw,
        preset,
        json!({"format": "csv", "text": file,
               "mapping": {"state": "text", "key": "ref", "labels": {"team": "team"}}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v = round_trips::<BatchView>("start", &body);
    until(&gw, v.id, |v| v.status == BatchStatus::Done).await;

    let out = text(
        &gw,
        &format!("/labs/api/decision/export?format=csv&batch={}", v.id),
    )
    .await;
    let mut rdr = ::csv::Reader::from_reader(out.as_bytes());
    let header: Vec<String> = rdr.headers().unwrap().iter().map(String::from).collect();
    assert_eq!(
        &header[..4],
        ["group_id", "batch_id", "item_ix", "item_key"]
    );
    let rows: Vec<::csv::StringRecord> = rdr.records().map(Result::unwrap).collect();
    let mut seen: Vec<(String, String, String, String)> = rows
        .iter()
        .filter(|r| &r[14] == "team")
        .map(|r| {
            (
                r[1].to_string(),
                r[2].to_string(),
                r[3].to_string(),
                r[header.iter().position(|h| h == "state").unwrap()].to_string(),
            )
        })
        .collect();
    seen.sort();
    let b = v.id.to_string();
    assert_eq!(
        seen,
        [
            (b.clone(), "0".into(), "A-7".into(), "ticket 0".into()),
            (b.clone(), "1".into(), String::new(), "ticket 1".into()),
            (b.clone(), "2".into(), "C-9".into(), "ticket 2".into()),
        ]
    );

    let jsonl = text(&gw, &format!("/labs/api/decision/export?batch={}", v.id)).await;
    let keys: Vec<Option<String>> = jsonl
        .lines()
        .map(|l| serde_json::from_str::<LabExportLine>(l).unwrap())
        .map(|line| line.group.item.unwrap().key)
        .collect();
    assert_eq!(keys, [Some("A-7".into()), None, Some("C-9".into())]);
    let raw: Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    assert_eq!(raw["group"]["item"]["key"], "A-7", "item.key on the wire");
}
