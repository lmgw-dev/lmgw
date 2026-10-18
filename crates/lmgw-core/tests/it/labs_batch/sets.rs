//! A set run's set and its measurements (labs-store design §3.5): a
//! preset update names the `updated_at` it read and is the 409
//! `set_changed` when the set moved since (a threshold written moves it
//! too); a question the update changes loses its measurement, its value
//! staying; a cut with no labelled answer at or above it is the 409
//! `no_labels`; and reliability with four billion bins answers only the
//! bin that holds the answers.

use lmgw_api_types::decision_lab::{BatchMetrics, BatchStatus};
use lmgw_api_types::lab_store::{LabPresetBody, LabPresetView};
use serde_json::json;

use super::*;

/// Preset `id` as stored now.
async fn preset(gw: &Gw, id: i64) -> LabPresetView {
    let (status, body) = get(gw, &format!("/labs/api/decision/presets/{id}")).await;
    assert_eq!(status, 200, "{body}");
    round_trips::<LabPresetView>("preset", &body)
}

fn question_set(v: &LabPresetView) -> &QuestionSet {
    let LabPresetBody::QuestionSet(s) = &v.preset else {
        panic!("a question set");
    };
    s
}

#[tokio::test]
async fn an_update_names_what_it_read_and_a_changed_question_loses_its_measurement() {
    let items = Items::new(Duration::ZERO);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let laya = l.w.name(CPU_ROW);
    let id = save_set(&gw, "triage", &set(&laya)).await;
    let created = preset(&gw, id).await;
    let v = start(&gw, id, csv(20), 2).await;
    until(&gw, v.id, |v| v.status == BatchStatus::Done).await;
    for (question, value) in [("team", 0.7), ("angry", 0.6)] {
        let (status, body) = post(
            &gw,
            &format!("/decision-lab/api/batches/{}/threshold", v.id),
            json!({"question": question, "value": value}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }
    let measured = preset(&gw, id).await;
    assert!(
        measured.updated_at > created.updated_at,
        "every write moves it: {} then {}",
        created.updated_at,
        measured.updated_at
    );

    // An edit computed from the set as created would undo both thresholds.
    let update = |body: &QuestionSet, read_at: &str| json!({"name": "triage", "kind": "question_set", "body": body, "updated_at": read_at});
    let path = format!("/labs/api/decision/presets/{id}");
    let (status, body) = post(
        &gw,
        &path,
        update(question_set(&created), &created.updated_at),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("set_changed")),
        "{body}"
    );
    assert_eq!(preset(&gw, id).await, measured, "nothing written");
    // A body without the precondition is no update: the route's 422 for a
    // body it does not read.
    let (status, _) = post(
        &gw,
        &path,
        json!({"name": "triage", "kind": "question_set", "body": question_set(&measured)}),
    )
    .await;
    assert_eq!(status, 422);

    // On the set as it is: the team question's text changes, so its
    // measurement goes and its value stays; angry keeps both.
    let mut edited = question_set(&measured).clone();
    edited.questions[0].instructions = "Who should handle it?".into();
    let (status, body) = post(&gw, &path, update(&edited, &measured.updated_at)).await;
    assert_eq!(status, 200, "{body}");
    let after = round_trips::<LabPresetView>("update", &body);
    let s = question_set(&after);
    let team = s.thresholds.get("team").unwrap();
    assert_eq!((team.value, team.measured.is_none()), (0.7, true));
    let angry = s.thresholds.get("angry").unwrap();
    assert_eq!(angry.value, 0.6);
    assert_eq!(angry.measured.as_ref().map(|m| m.batch_id), Some(v.id));
    // The same updated_at a second time is stale now.
    let (status, body) = post(&gw, &path, update(&edited, &measured.updated_at)).await;
    assert_eq!((status, body["code"].as_str()), (409, Some("set_changed")));
}

#[tokio::test]
async fn a_cut_with_no_labelled_answer_above_it_is_refused() {
    let items = Items::new(Duration::ZERO);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let id = save_set(&gw, "triage", &set(&l.w.name(CPU_ROW))).await;
    let v = start(&gw, id, csv(10), 2).await;
    until(&gw, v.id, |v| v.status == BatchStatus::Done).await;
    // The highest confidence the container gives is 0.95.
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/threshold", v.id),
        json!({"question": "team", "value": 0.99}),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("no_labels")),
        "{body}"
    );
    let LabPresetBody::QuestionSet(s) = preset(&gw, id).await.preset else {
        panic!("a question set");
    };
    assert!(s.thresholds.is_empty(), "nothing written");

    // An unlabelled run measures no cut at all.
    let unlabelled = "text\nticket 0\nticket 1\n";
    let (status, body) = post(
        &gw,
        "/decision-lab/api/batches",
        json!({"preset_id": id, "format": "csv", "text": unlabelled,
               "mapping": {"state": "text"}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let w: BatchView = serde_json::from_value(body).unwrap();
    until(&gw, w.id, |v| v.status == BatchStatus::Done).await;
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/threshold", w.id),
        json!({"question": "team", "value": 0.0}),
    )
    .await;
    assert_eq!((status, body["code"].as_str()), (409, Some("no_labels")));
}

#[tokio::test]
async fn four_billion_bins_answer_only_the_bin_that_holds_the_answers() {
    let items = Items::new(Duration::ZERO);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let id = save_set(&gw, "triage", &set(&l.w.name(CPU_ROW))).await;
    let v = start(&gw, id, csv(10), 2).await;
    until(&gw, v.id, |v| v.status == BatchStatus::Done).await;
    let (status, body) = get(
        &gw,
        &format!(
            "/decision-lab/api/batches/{}/metrics?question=team&bins=4000000000",
            v.id
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let m = round_trips::<BatchMetrics>("metrics", &body);
    assert_eq!(m.bins, 4_000_000_000);
    let bins = &m.questions[0].reliability;
    // Every winner's probability is 0.8: one bin, placed by its own edges.
    assert_eq!(bins.len(), 1, "{bins:?}");
    assert_eq!((bins[0].index, bins[0].count), (3_200_000_000, 10));
    assert!(bins[0].lower <= 0.8 && 0.8 < bins[0].upper, "{:?}", bins[0]);
    let (status, _) = get(
        &gw,
        &format!("/decision-lab/api/batches/{}/metrics?bins=0", v.id),
    )
    .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn every_write_moves_a_presets_updated_at_even_in_one_millisecond() {
    let items = Items::new(Duration::ZERO);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let id = save_set(&gw, "triage", &set(&l.w.name(CPU_ROW))).await;
    let mut stamps = vec![preset(&gw, id).await.updated_at];
    let mut body = lmgw_core::store::lab_preset(&l.w.state.db, None, id)
        .await
        .unwrap()
        .unwrap()
        .body;
    // Back to back, faster than the clock's millisecond moves.
    for n in 0..5 {
        let next = body.replacen("decide/", &format!("decide/{n}"), 1);
        let lmgw_core::store::PresetWrite::Written(row) =
            lmgw_core::store::replace_lab_preset_body(&l.w.state.db, id, &body, &next)
                .await
                .unwrap()
        else {
            panic!("the body was the one read");
        };
        stamps.push(row.updated_at);
        body = next;
    }
    assert!(
        stamps.windows(2).all(|w| w[0] < w[1]),
        "strictly later each time: {stamps:?}"
    );
}
