//! The Decision lab's and the lab store's routes against their documented
//! types (`lmgw-api-types::decision_lab`, `lab_store`): each answer, read
//! into its type and written back, is the answer again, key by key, and
//! validates against the admin document. A field the gateway adds to an
//! answer without adding it to the type is dropped by the read and fails
//! here.

use lmgw_api_types::decision_lab::{DecisionCounted, DecisionLabModels, ImageInput, Variant};
use lmgw_api_types::lab_store::{LabCellResult, LabGroupPage, LabGroupView, LabUsage};
use serde_json::{json, Value};

use crate::common::{round_trips, Gw};
use crate::decision_lab::{lab, post, ticket, CPU_ROW};

async fn get(gw: &Gw, path: &str) -> Value {
    let resp = gw.client().get(format!("{gw}{path}")).send().await.unwrap();
    assert_eq!(resp.status(), 200, "{path}");
    resp.json().await.unwrap()
}

#[tokio::test]
async fn every_decision_lab_answer_is_its_documented_type() {
    let l = lab().await;
    let gw = l.gw().clone();
    let laya = l.w.name(CPU_ROW);

    let (status, live) = post(
        &gw,
        "/decision-lab/api/count",
        json!({"model": laya, "form": ticket()}),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let not_counted = round_trips::<DecisionCounted>("count, not loaded", &live);
    assert!(not_counted.not_counted.is_some());

    let mut form = ticket();
    form.images = vec![ImageInput::DataUrl {
        name: "x.png".into(),
        url: crate::labs_store::typed_data_url("data:image/png;base64", crate::labs_store::PNG),
    }];
    let (status, live) = post(
        &gw,
        "/decision-lab/api/groups",
        json!({"form": form, "models": [laya], "variants": [Variant::NoDescriptions]}),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let g = round_trips::<LabGroupView>("ask", &live);

    let (status, live) = post(
        &gw,
        &format!("/decision-lab/api/groups/{}/cells/0", g.id),
        json!(null),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    round_trips::<LabCellResult>("cell", &live);

    let (status, live) = post(
        &gw,
        "/decision-lab/api/count",
        json!({"model": laya, "form": ticket()}),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let counted = round_trips::<DecisionCounted>("count", &live);
    assert!(
        counted.not_counted.is_none() && !counted.estimates.is_empty(),
        "{live}"
    );

    let live = get(&gw, "/decision-lab/api/models").await;
    let models = round_trips::<DecisionLabModels>("models", &live);
    assert!(
        models.models.iter().any(|m| m.state.is_some()),
        "a running row: {live}"
    );

    let live = get(&gw, "/labs/api/decision/groups?limit=1").await;
    let page = round_trips::<LabGroupPage>("history", &live);
    assert_eq!(page.groups[0].runs.len(), 1);
    assert_eq!(page.groups[0].artifacts.len(), 1);

    let live = get(&gw, &format!("/labs/api/decision/groups/{}", g.id)).await;
    round_trips::<LabGroupView>("group", &live);

    let live = get(&gw, "/labs/api/decision/usage").await;
    round_trips::<LabUsage>("usage", &live);
    let (status, live) = post(&gw, "/labs/api/decision/orphans/delete", json!(null)).await;
    assert_eq!(status, 200);
    round_trips::<LabUsage>("orphans", &live);
    let (status, live) = post(
        &gw,
        &format!("/labs/api/decision/groups/{}/delete", g.id),
        json!(null),
    )
    .await;
    assert_eq!(status, 200);
    round_trips::<LabUsage>("delete", &live);
    let (status, live) = post(&gw, "/labs/api/decision/clear", json!(null)).await;
    assert_eq!(status, 200);
    let cleared = round_trips::<LabUsage>("clear", &live);
    assert_eq!(cleared.groups, 0);
}

/// A run the store could not keep is answered with no `id` and the reason:
/// still the documented type.
#[tokio::test]
async fn a_cell_the_store_could_not_keep_is_still_its_type() {
    let l = lab().await;
    let gw = l.gw().clone();
    let laya = l.w.name(CPU_ROW);
    let (_, live) = post(
        &gw,
        "/decision-lab/api/groups",
        json!({"form": ticket(), "models": [laya]}),
    )
    .await;
    let id = live["id"].as_i64().unwrap();
    // The store refuses every run from here on: the cell still answers.
    sqlx::query(
        "CREATE TRIGGER refuse_runs BEFORE INSERT ON lab_runs \
         BEGIN SELECT RAISE(ABORT, 'disk full'); END",
    )
    .execute(&l.w.state.db)
    .await
    .unwrap();
    let (status, live) = post(
        &gw,
        &format!("/decision-lab/api/groups/{id}/cells/0"),
        json!(null),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let r = round_trips::<LabCellResult>("unkept cell", &live);
    assert_eq!(r.run.id, None);
    assert!(
        r.store_error.as_deref().unwrap().contains("disk full"),
        "{live}"
    );
    assert_eq!(r.run.http_status, 200, "the route's answer all the same");
}
