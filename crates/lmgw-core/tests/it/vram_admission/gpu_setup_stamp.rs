//! A start the machine check blames on this computer is stamped on the
//! check (machine-check design D182, D183, review H1): whoever asked for it
//! (a client's request, the model's Test), the last result and every one
//! published after it carry `failed_start`, the status's summary too, and
//! the live feed gets a `machine` frame, so a "Not now" said before it ends
//! on every dashboard surface. A start the check does not blame stamps
//! nothing.

use lmgw_api_types::machine::{MachineFailedStart, MachineVerdict};
use lmgw_api_types::ModelTest;
use lmgw_core::telemetry::Event;

use super::gpu_setup::{ask, driver_updated, not_ready, verdict, world, RESTART};
use super::*;
use crate::support::machine_world::Machine;

/// The stamp the last result carries, if any.
fn stamp(state: &SharedState) -> Option<MachineFailedStart> {
    state.machine.last().and_then(|c| c.failed_start.clone())
}

/// The `machine` frames the live feed got so far.
fn machine_frames(
    rx: &mut tokio::sync::broadcast::Receiver<Event>,
) -> Vec<Option<MachineFailedStart>> {
    let mut seen = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::Machine(s) = ev {
            seen.push(s.failed_start);
        }
    }
    seen
}

#[tokio::test]
async fn a_failed_start_the_machine_is_blamed_for_is_stamped_on_the_check() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, gw) = world(&machine).await;
    assert_eq!(stamp(&g.state), None, "nothing failed yet");
    let mut rx = g.state.telemetry.subscribe();
    driver_updated(&g, &machine);

    not_ready(ask(&gw, "m1", false).await, RESTART, &[]).await;
    let first = stamp(&g.state).expect("the failed start is on the check");
    assert_eq!(
        (first.model.as_str(), first.class.as_str(), first.count),
        ("m1", "chat", 1)
    );
    assert_eq!(first.at.len(), "2026-11-03 13:22:07".len(), "{first:?}");
    assert_eq!(
        g.state.machine.summary().unwrap().failed_start.as_ref(),
        Some(&first),
        "the status's summary says it too"
    );
    // The full check that follows in the background keeps it: every result
    // published after the stamp carries it.
    g.state.machine.settled().await;
    assert_eq!(verdict(&g), MachineVerdict::NeedsRestart);
    assert_eq!(stamp(&g.state), Some(first.clone()));
    let frames = machine_frames(&mut rx);
    assert!(
        frames.iter().any(|f| f.as_ref() == Some(&first)),
        "the live feed heard of it: {frames:?}"
    );

    // The next one counts on, even within the same second.
    not_ready(ask(&gw, "m2", false).await, "'m2' could not start", &[]).await;
    let second = stamp(&g.state).unwrap();
    assert_eq!((second.model.as_str(), second.count), ("m2", 2));
}

/// A start that fails on a machine the check finds nothing wrong with is
/// the start's own error, and the check is not stamped: "Not now" holds.
#[tokio::test]
async fn a_failed_start_on_a_ready_machine_stamps_nothing() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, gw) = world(&machine).await;
    g.world().fail_run.insert("m1".into());
    assert_eq!(ask(&gw, "m1", false).await.status(), 502);
    g.state.machine.settled().await;
    assert_eq!(stamp(&g.state), None);
    assert_eq!(g.state.machine.summary().unwrap().failed_start, None);
}

/// The model's Test is one of the requests whose failed start ends "Not
/// now" (review M5, H1): its start's failure is a failed test carrying
/// `gpu_setup`'s code, the container's log beside it, and the check is
/// stamped as for a client's request.
#[tokio::test]
async fn the_model_test_answers_gpu_setup_with_its_code_and_stamps_the_check() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, _gw) = world(&machine).await;
    driver_updated(&g, &machine);

    let t = lmgw_core::modelinfo::local_model_test(&g.state, "m1", None)
        .await
        .expect("a failed test, not the op's own error");
    let ModelTest::Failed(f) = t else {
        panic!("{t:?}")
    };
    assert_eq!(f.code.as_deref(), Some("gpu_setup"), "{f:?}");
    assert!(f.error.starts_with(RESTART), "{}", f.error);
    assert!(!f.loaded);
    let s = stamp(&g.state).expect("stamped");
    assert_eq!((s.model.as_str(), s.count), ("m1", 1));

    // A test whose start fails on a ready machine is a failure without
    // that code.
    let machine = Machine::nvidia_at("600.10.01");
    let (g, _gw) = world(&machine).await;
    g.world().fail_run.insert("m1".into());
    let ModelTest::Failed(f) = lmgw_core::modelinfo::local_model_test(&g.state, "m1", None)
        .await
        .unwrap()
    else {
        panic!("a failed test")
    };
    assert_ne!(f.code.as_deref(), Some("gpu_setup"), "{f:?}");
    assert_eq!(stamp(&g.state), None);
}
