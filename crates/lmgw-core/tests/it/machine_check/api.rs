//! The check's API (§4.2): the `machine_check` op with and without `fresh`,
//! joined runs, `lmgw__machine_check` as a read, and the status summary.

use std::time::Duration;

use lmgw_api_types::machine::{MachineCheck, MachineVerdict};
use serde_json::json;

use crate::common::serve;
use crate::support::device_pair::op;
use crate::support::machine_world::World;
use crate::support::self_admin::{read_back, Admin};

/// Without `fresh` the op answers the last result (running one only when
/// there is none); with it, a new run.
#[tokio::test]
async fn the_op_answers_the_last_check_or_a_fresh_one() {
    let w = World::nvidia().await;
    let gw = serve(w.state.clone()).await;
    let (status, body) = op(&gw, "machine_check", json!({})).await;
    assert_eq!(status, 200, "{body}");
    let first: MachineCheck = serde_json::from_value(body).unwrap();
    assert_eq!(first.verdict, MachineVerdict::Ready);
    assert_eq!(w.state.machine.runs(), 1);

    let (_, body) = op(&gw, "machine_check", json!({})).await;
    let again: MachineCheck = serde_json::from_value(body).unwrap();
    assert_eq!(again, first, "the last result, not a new run");
    assert_eq!(w.state.machine.runs(), 1);

    let (_, body) = op(&gw, "machine_check", json!({ "fresh": true })).await;
    let _: MachineCheck = serde_json::from_value(body).unwrap();
    assert_eq!(w.state.machine.runs(), 2);

    // An argument it does not know is refused by name.
    let (status, body) = op(&gw, "machine_check", json!({ "frseh": true })).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_string().contains("frseh"), "{body}");
}

/// A fresh check never answers with what a run read before it was asked:
/// one asked while a run is reading gets a run of its own, and two asked
/// while that run reads share the next one.
#[tokio::test]
async fn a_fresh_check_joins_only_a_run_that_began_after_it_asked() {
    let w = World::nvidia().await;
    *w.probe_delay.lock().unwrap() = Duration::from_millis(300);
    let (tx, rx) = std::sync::mpsc::channel();
    *w.probe_started.lock().unwrap() = Some(tx);
    let first = {
        let state = w.state.clone();
        tokio::spawn(async move { lmgw_core::machine::check(&state, true).await })
    };
    // The first run is reading (its probe has started) before the others ask.
    tokio::task::spawn_blocking(move || rx.recv().unwrap())
        .await
        .unwrap();
    // The card changes after the first run read it.
    w.m.facts.set_driver_libraries("600.30.05");
    let (a, b) = tokio::join!(
        lmgw_core::machine::check(&w.state, true),
        lmgw_core::machine::check(&w.state, true)
    );
    let first = first.await.unwrap();
    assert_eq!(first.verdict, MachineVerdict::Ready);
    assert_eq!(
        a.verdict,
        MachineVerdict::NeedsRestart,
        "read after it asked"
    );
    assert_eq!(a, b, "the two that asked together share a run");
    assert_eq!(w.state.machine.runs(), 2);
    assert_eq!(w.probes(), 1, "no NVML while the halves differ");
}

/// The boot run (§2.6): the check, lmgw's spec written, the check again —
/// published only at the end, so the status never says ready while the
/// spec is still being written, and a check asked meanwhile takes the
/// boot's result.
#[tokio::test]
async fn the_boot_run_publishes_only_after_the_spec_is_written() {
    let w = World::nvidia().await;
    let seen: std::sync::Arc<std::sync::Mutex<Vec<Option<MachineVerdict>>>> = Default::default();
    {
        let (state, seen) = (w.state.clone(), seen.clone());
        w.m.while_generating(move |_| {
            seen.lock()
                .unwrap()
                .push(state.machine.summary().map(|s| s.verdict));
        });
    }
    lmgw_core::machine::boot(&w.state).await;
    assert_eq!(w.m.generations(), 1);
    assert_eq!(*seen.lock().unwrap(), [None], "nothing published mid-write");
    assert_eq!(
        w.state.machine.runs(),
        2,
        "the check, then again after the write"
    );
    let c = lmgw_core::machine::check(&w.state, false).await;
    assert_eq!(c.verdict, MachineVerdict::Ready);
    let toolkit = c
        .items
        .iter()
        .find(|i| i.id == lmgw_api_types::machine::MachineItemId::CardForModels)
        .unwrap();
    assert!(
        toolkit
            .detail
            .iter()
            .any(|l| l.contains("true of this machine now")),
        "the published result is the one after the write: {:?}",
        toolkit.detail
    );

    // A boot whose write fails publishes the failure, never a ready first.
    let w = World::nvidia().await;
    w.m.fail_with(Some(lmgw_machine::CmdOut::failure(
        1,
        "level=error msg=\"boom\"\n",
    )));
    lmgw_core::machine::boot(&w.state).await;
    let summary = w.state.machine.summary().unwrap();
    assert_eq!(summary.verdict, MachineVerdict::NeedsSteps, "{summary:?}");
    assert_eq!(w.state.machine.runs(), 2);

    // A machine whose spec is already true needs no second run.
    let w2 = World::nvidia().await;
    lmgw_core::machine::boot(&w2.state).await;
    let before = w2.state.machine.runs();
    lmgw_core::machine::boot(&w2.state).await;
    assert_eq!(w2.state.machine.runs(), before + 1, "fresh spec: one run");
}

/// `GET /api/status` carries the verdict once a check finished; none before.
#[tokio::test]
async fn the_status_carries_the_summary_once_checked() {
    let w = World::nvidia().await;
    let status = lmgw_core::ops::status(&w.state).await.unwrap();
    assert!(status.machine.is_none());
    let json = serde_json::to_value(&status).unwrap();
    assert!(
        json.get("machine").is_none(),
        "absent, not null, before the check"
    );
    lmgw_core::machine::check(&w.state, true).await;
    let status = lmgw_core::ops::status(&w.state).await.unwrap();
    let summary = status.machine.unwrap();
    assert_eq!(summary.verdict, MachineVerdict::Ready);
    assert_eq!(summary.headline, "This computer is ready for local models.");
}

/// `lmgw__machine_check` is a read: listed without `writes`, its answer the
/// op's type, its description saying the fix runs only from the app.
#[tokio::test]
async fn the_self_admin_tool_is_a_read_of_the_same_check() {
    // Listed at `read only`, so it is not a writer.
    let ro = lmgw_core::mcp::selfadmin::list(lmgw_core::config::SelfAdmin::ReadOnly);
    let tool = ro
        .iter()
        .find(|t| t["name"] == "lmgw__machine_check")
        .expect("listed at read only");
    let description = tool["description"].as_str().unwrap();
    assert!(
        description.contains("only from lmgw's own app window"),
        "{description}"
    );
    assert!(!lmgw_core::mcp::selfadmin::writes("lmgw__machine_check"));

    let w = World::nvidia().await;
    let admin = Admin::new(&w.state).await;
    let text = admin
        .text("lmgw__machine_check", json!({ "fresh": true }))
        .await;
    let c: MachineCheck = read_back(&text);
    assert_eq!(c.verdict, MachineVerdict::Ready);
    assert_eq!(w.state.machine.runs(), 1);
    let (failed, text) = admin
        .call("lmgw__machine_check", json!({ "fix": true }))
        .await;
    assert!(failed && text.contains("unknown argument 'fix'"), "{text}");
}
