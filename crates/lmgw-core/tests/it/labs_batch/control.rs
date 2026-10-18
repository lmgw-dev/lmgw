//! A set run's job under control (labs-store design §3.5): a Delete and a
//! Clear drop the calls in flight (the request log keeps their 499 rows,
//! no run is recorded); a page's cell racing the job on one item reaches
//! the model through one of them, and a second Cancel drops the call the
//! first let finish; a model's 5xx is a failed run that Resume's
//! `retry_failed` replaces, the CSV keeping each label meanwhile; the GPU
//! hold's refusal ends the job with the item left without a run, and
//! Resume continues once it is released.

use lmgw_api_types::decision_lab::{BatchItemPage, BatchMetrics, BatchStatus, BatchView};
use lmgw_api_types::lab_store::{LabCellResult, LabUsage};
use lmgw_core::store;
use std::sync::atomic::Ordering;

use serde_json::{json, Value};

use super::*;
use crate::support::decision_world::DECISION;

/// Job `job` once it is no longer live; a panic after ten seconds.
async fn ended(l: &Lab, job: i64) -> store::JobRow {
    let state = l.w.state.clone();
    eventually(&format!("job {job} ends"), || {
        state.jobs.live_one(job).is_none()
    })
    .await;
    store::get_job(&l.w.state.db, job).await.unwrap().unwrap()
}

/// The request log's `/v1/systemone` rows, newest first.
async fn systemone_rows(l: &Lab) -> Vec<store::RequestLogRow> {
    store::query_logs(&l.w.state.db, &Default::default())
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.ingress_proto == "systemone")
        .collect()
}

/// Wait until the request log has `n` 499 rows of `/v1/systemone`: a
/// dropped handler writes its row from a task of its own.
async fn rows_499(l: &Lab, n: usize) {
    for _ in 0..400 {
        let got = systemone_rows(l)
            .await
            .iter()
            .filter(|r| r.status == 499)
            .count();
        if got >= n {
            assert_eq!(got, n, "exactly the calls that were in flight");
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the request log never had {n} 499 rows");
}

/// The items of set run `id`, in order.
async fn items_of(gw: &Gw, id: i64) -> BatchItemPage {
    let (status, body) = get(gw, &format!("/decision-lab/api/batches/{id}/items")).await;
    assert_eq!(status, 200, "{body}");
    round_trips::<BatchItemPage>("items", &body)
}

#[tokio::test]
async fn a_delete_and_a_clear_drop_the_calls_in_flight_and_record_no_run() {
    let mut items = Items::new(Duration::ZERO);
    items.hang_from = 0;
    items.hang.store(true, Ordering::SeqCst);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let preset = save_set(&gw, "triage", &set(&l.w.name(CPU_ROW))).await;

    // Two calls hang in the container; Delete does not wait for them.
    let v = start(&gw, preset, csv(6), 2).await;
    let job = v.job_id.expect("the live job");
    let seen = items.clone();
    eventually("two calls in flight", || seen.seen() == 2).await;
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/delete", v.id),
        json!(null),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let usage = round_trips::<LabUsage>("delete", &body);
    assert_eq!((usage.batches, usage.groups, usage.runs), (0, 0, 0));
    let row = ended(&l, job).await;
    assert_eq!(row.status, "canceled", "{row:?}");
    rows_499(&l, 2).await;
    assert_eq!(items.seen(), 2, "no item asked after the delete");

    // Clear does the same for every running set run of the lab.
    let v = start(&gw, preset, csv(6), 2).await;
    let job = v.job_id.expect("the live job");
    let seen = items.clone();
    eventually("two more calls in flight", || seen.seen() == 4).await;
    let (status, body) = post(&gw, "/labs/api/decision/clear", json!(null)).await;
    assert_eq!(status, 200, "{body}");
    let usage = round_trips::<LabUsage>("clear", &body);
    assert_eq!((usage.batches, usage.groups, usage.runs), (0, 0, 0));
    assert_eq!(ended(&l, job).await.status, "canceled");
    rows_499(&l, 4).await;
    assert_eq!(items.seen(), 4);
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lab_runs")
        .fetch_one(&l.w.state.db)
        .await
        .unwrap();
    assert_eq!(runs, 0, "a dropped call records nothing");
}

#[tokio::test]
async fn a_pages_cell_racing_the_job_reaches_the_model_once_and_a_second_cancel_drops_the_call() {
    // Each answer takes 600 ms, so item 0's frame (the first after the
    // job's opening one) is published, and the throttle window is open when
    // the Cancel comes while item 1 hangs.
    let items = Items::new(Duration::from_millis(600));
    items.hang_items(&[1]);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let preset = save_set(&gw, "triage", &set(&l.w.name(CPU_ROW))).await;
    let v = start(&gw, preset, csv(4), 1).await;
    let job = v.job_id.unwrap();
    let seen = items.clone();
    eventually("item 1 in flight", || seen.asked(1) == 1).await;
    // The job's last published frame: its done count and the cancel flag.
    let frame = || async {
        let row = store::get_job(&l.w.state.db, job).await.unwrap().unwrap();
        let p: Value = serde_json::from_str(&row.progress).unwrap();
        (
            p["done"].as_u64(),
            p["detail"]["cancel_requested"].as_bool(),
        )
    };
    let mut landed = false;
    for _ in 0..200 {
        landed = frame().await.0 == Some(1);
        if landed {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(landed, "item 0's frame went out");

    // The first Cancel lets the call in flight finish, which hangs; the
    // second drops it. The run says the next one aborts, also to a reader
    // that did not send the first (a reloaded page).
    assert!(!v.cancel_requested);
    let sent = std::time::Instant::now();
    let cancel = format!("/decision-lab/api/batches/{}/cancel", v.id);
    let (status, body) = post(&gw, &cancel, json!(null)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["cancel_requested"], true, "{body}");
    // The frame carries it at once, inside the throttle window, though the
    // call in flight is still hanging (a Cancel from another tab or the jobs
    // route reaches a page by the frame alone).
    let mut framed = false;
    while sent.elapsed() < std::time::Duration::from_millis(400) {
        framed = frame().await.1 == Some(true);
        if framed {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(framed, "a published frame says a cancel was asked");
    assert!(
        l.w.state.jobs.live_one(job).is_some(),
        "still waiting for the call in flight"
    );

    let page = items_of(&gw, v.id).await;
    let (g1, g2) = (page.items[1].id, page.items[2].id);
    // The job holds item 1's claim: the page's post of it is the 409, and
    // reaches nothing.
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/groups/{g1}/cells/0"),
        json!(null),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("cell_answered")),
        "{body}"
    );
    assert_eq!(items.asked(1), 1);
    // Item 2, which the job will not reach now, the page answers.
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/groups/{g2}/cells/0"),
        json!(null),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let r: LabCellResult = serde_json::from_value(body).unwrap();
    assert_eq!(r.run.http_status, 200);
    let (status, body) = get(&gw, &format!("/decision-lab/api/batches/{}", v.id)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        (body["status"].as_str(), body["cancel_requested"].as_bool()),
        (Some("running"), Some(true)),
        "{body}"
    );
    let (status, body) = post(&gw, &cancel, json!(null)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ended(&l, job).await.status, "canceled");
    rows_499(&l, 1).await;
    let stopped = until(&gw, v.id, |v| v.status != BatchStatus::Running).await;
    assert_eq!(
        (stopped.status, stopped.done),
        (BatchStatus::Canceled, 2),
        "item 0 by the job, item 2 by the page"
    );
    assert!(!stopped.cancel_requested, "no job runs it");

    // Item 1 has no run: the page may answer it now, and Resume skips the
    // two answered before.
    items.hang_items(&[]);
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/groups/{g1}/cells/0"),
        json!(null),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/resume", v.id),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let done = until(&gw, v.id, |v| v.status == BatchStatus::Done).await;
    assert_eq!((done.done, done.failed), (4, 0));
    assert_eq!(
        (0..4).map(|i| items.asked(i)).collect::<Vec<_>>(),
        [1, 2, 1, 1],
        "item 1: the dropped call and the page's; every other item once"
    );
    let page = items_of(&gw, v.id).await;
    assert!(page.items.iter().all(|g| g.runs.len() == 1));
}

/// The CSV export of set run `id` as records, with the header's columns.
async fn csv_export(gw: &Gw, id: i64) -> (Vec<String>, Vec<::csv::StringRecord>) {
    let out = text(
        gw,
        &format!("/labs/api/decision/export?format=csv&batch={id}"),
    )
    .await;
    let mut rdr = ::csv::Reader::from_reader(out.as_bytes());
    let header = rdr.headers().unwrap().iter().map(String::from).collect();
    (header, rdr.records().map(Result::unwrap).collect())
}

#[tokio::test]
async fn a_models_5xx_is_a_failed_run_and_retry_failed_asks_it_again() {
    let items = Items::new(Duration::ZERO);
    items.fail_items(&[1, 3]);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let preset = save_set(&gw, "triage", &set(&l.w.name(CPU_ROW))).await;
    let v = start(&gw, preset, csv(5), 2).await;
    let v = until(&gw, v.id, |v| v.status == BatchStatus::Done).await;
    assert_eq!((v.done, v.failed, v.unreadable), (5, 2, 0));
    let (_, body) = get(&gw, &format!("/decision-lab/api/batches/{}/metrics", v.id)).await;
    let m = round_trips::<BatchMetrics>("metrics", &body);
    assert_eq!(
        (m.questions[0].failed, m.questions[0].unreadable),
        (2, 0),
        "the header and the metrics agree"
    );

    // A failed run's rows keep each label; only `correct` is empty.
    let (header, rows) = csv_export(&gw, v.id).await;
    let col = |name: &str| header.iter().position(|h| h == name).unwrap();
    let failed: Vec<&::csv::StringRecord> =
        rows.iter().filter(|r| &r[col("item_ix")] == "1").collect();
    assert_eq!(failed.len(), 2, "a row per question of the set");
    for r in &failed {
        assert_eq!(&r[col("http_status")], "500");
        assert_eq!(&r[col("answer")], "");
        assert_eq!(&r[col("correct")], "");
        let label = match &r[col("question_id")] {
            "team" => Some(team_label(1)),
            "angry" => angry_label(1),
            other => panic!("question {other}"),
        };
        assert_eq!(&r[col("expected")], label.unwrap_or(""));
    }
    assert_eq!(&failed[0][col("question_type")], "choice");

    // Without retry_failed every item has its run: done.
    let resume = format!("/decision-lab/api/batches/{}/resume", v.id);
    let (status, body) = post(&gw, &resume, json!({})).await;
    assert_eq!((status, body["code"].as_str()), (409, Some("batch_done")));
    let (_, body) = get(&gw, &format!("/decision-lab/api/batches/{}/items", v.id)).await;
    let before: BatchItemPage = serde_json::from_value(body).unwrap();
    let failed_run = before.items[1].runs[0].id.unwrap();

    // With it, the failed items are asked again and their runs replaced.
    items.fail_items(&[]);
    let (status, body) = post(&gw, &resume, json!({"retry_failed": true})).await;
    assert_eq!(status, 200, "{body}");
    let resumed = round_trips::<BatchView>("resume", &body);
    let job = resumed.job_id.unwrap();
    assert_eq!(ended(&l, job).await.status, "done");
    let (_, body) = get(&gw, &format!("/decision-lab/api/batches/{}", v.id)).await;
    let after: BatchView = serde_json::from_value(body).unwrap();
    assert_eq!(
        (after.status, after.done, after.failed),
        (BatchStatus::Done, 5, 0)
    );
    assert_eq!(
        (0..5).map(|i| items.asked(i)).collect::<Vec<_>>(),
        [1, 2, 1, 2, 1]
    );
    let page = items_of(&gw, v.id).await;
    assert!(page.items.iter().all(|g| g.runs.len() == 1));
    assert_eq!(page.items[1].runs[0].http_status, 200);
    assert_ne!(page.items[1].runs[0].id, Some(failed_run), "replaced");
    assert_eq!(
        page.items[0].runs[0].id, before.items[0].runs[0].id,
        "a 2xx run stays"
    );
    let (status, body) = post(&gw, &resume, json!({"retry_failed": true})).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("batch_done")),
        "none failed"
    );
}

#[tokio::test]
async fn the_gpu_hold_ends_the_job_with_the_item_left_and_resume_continues() {
    let items = Items::new(Duration::ZERO);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let held = l.w.name(DECISION);
    let preset = save_set(&gw, "triage", &set(&held)).await;
    lmgw_core::ops::hold_set(&l.w.state, true).await.unwrap();

    let v = start(&gw, preset, csv(3), 2).await;
    let job = v.job_id.unwrap();
    let row = ended(&l, job).await;
    assert_eq!(row.status, "failed");
    let (_, body) = get(&gw, &format!("/decision-lab/api/batches/{}", v.id)).await;
    let stopped = round_trips::<BatchView>("held", &body);
    assert_eq!(
        (stopped.status, stopped.done, stopped.failed),
        (BatchStatus::Interrupted, 0, 0),
        "the hold's refusal is no item's answer"
    );
    let why = stopped.job_error.unwrap_or_default();
    assert!(
        why.contains("lmgw could not ask the model (503 gpu_hold)")
            && why.contains("Resume runs the rest"),
        "{why}"
    );
    let (_, body) = get(&gw, "/labs/api/decision/usage").await;
    assert_eq!(body["runs"], 0, "{body}");
    assert_eq!(items.seen(), 0, "nothing reached the container");
    // The request log has the refusal itself, as for any client.
    let refused: Vec<store::RequestLogRow> = systemone_rows(&l).await;
    assert!(
        !refused.is_empty() && refused.iter().all(|r| r.status == 503),
        "{refused:?}"
    );

    lmgw_core::ops::hold_set(&l.w.state, false).await.unwrap();
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/resume", v.id),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let done = until(&gw, v.id, |v| v.status == BatchStatus::Done).await;
    assert_eq!((done.done, done.failed, done.job_error), (3, 0, None));
    assert_eq!(items.seen(), 3);
}

#[tokio::test]
async fn the_upstreams_401_ends_the_job_like_the_hold_and_resume_continues() {
    // The provider refusing lmgw's key is the same for every item: no run is
    // recorded, the job is interrupted, and Resume continues once it is fixed.
    let items = Items::new(Duration::ZERO);
    items.deny.store(true, Ordering::SeqCst);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let preset = save_set(&gw, "triage", &set(&l.w.name(CPU_ROW))).await;
    let v = start(&gw, preset, csv(3), 2).await;
    let job = v.job_id.unwrap();
    assert_eq!(ended(&l, job).await.status, "failed");
    let (_, body) = get(&gw, &format!("/decision-lab/api/batches/{}", v.id)).await;
    let stopped = round_trips::<BatchView>("denied", &body);
    assert_eq!(
        (stopped.status, stopped.done, stopped.failed),
        (BatchStatus::Interrupted, 0, 0),
        "the key's refusal is no item's answer"
    );
    let why = stopped.job_error.unwrap_or_default();
    assert!(
        why.contains("lmgw could not ask the model (502") && why.contains("Resume runs the rest"),
        "{why}"
    );
    let (_, body) = get(&gw, "/labs/api/decision/usage").await;
    assert_eq!(body["runs"], 0, "{body}");

    items.deny.store(false, Ordering::SeqCst);
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/batches/{}/resume", v.id),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let done = until(&gw, v.id, |v| v.status == BatchStatus::Done).await;
    assert_eq!((done.done, done.failed, done.job_error), (3, 0, None));
}

#[tokio::test]
async fn the_page_still_records_the_holds_refusal_as_its_cells_run() {
    // The job's mode is the job's: a cell the page posts keeps the 503.
    let items = Items::new(Duration::ZERO);
    let l = batch_lab(&items).await;
    let gw = l.gw().clone();
    let held = l.w.name(DECISION);
    let preset = save_set(&gw, "triage", &set(&held)).await;
    lmgw_core::ops::hold_set(&l.w.state, true).await.unwrap();
    let v = start(&gw, preset, csv(1), 1).await;
    ended(&l, v.job_id.unwrap()).await;
    let page = items_of(&gw, v.id).await;
    let (status, body) = post(
        &gw,
        &format!("/decision-lab/api/groups/{}/cells/0", page.items[0].id),
        json!(null),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let r: LabCellResult = serde_json::from_value(body).unwrap();
    assert_eq!(
        (r.run.http_status, r.run.error_code.as_deref()),
        (503, Some("gpu_hold"))
    );
    let v: Value = get(&gw, &format!("/decision-lab/api/batches/{}", v.id))
        .await
        .1;
    assert_eq!(
        (v["done"].as_u64(), v["failed"].as_u64()),
        (Some(1), Some(1))
    );
}
