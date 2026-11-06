//! A failed start's blame (machine-check design §2.5, D96): the last
//! check's verdict while the machine is as it read it; otherwise §1.1 and
//! §1.2 read again alone — one read for every start that fails meanwhile,
//! NVML left out after a write of lmgw's spec that ran out of time on the
//! card — and the full check after it, in the background, which the
//! start's answer does not wait for.

use std::time::{Duration, Instant};

use lmgw_api_types::machine::MachineVerdict;
use lmgw_core::machine::after_failed_start;
use lmgw_core::runtime::descriptor::{model_runtime, ModelRuntime};
use lmgw_core::runtime::registry::RuntimeError;
use lmgw_core::runtime::Class;
use lmgw_core::store::{self, NewLocalModel};

use crate::support::machine_world::World;

/// A chat row `id` on the card, with the class's run arguments (the NVIDIA
/// card).
async fn row(w: &World, id: &str) -> ModelRuntime {
    store::insert_local_model(
        &w.state.db,
        &NewLocalModel {
            model_id: id.into(),
            gguf_path: format!("{id}.gguf"),
            params: Default::default(),
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            fallback: None,
            capabilities_override: None,
            ladder: vec![],
        },
    )
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    model_runtime(&w.state.snapshot(), Class::Chat, id).unwrap()
}

/// The container of `id` never came up.
fn failed(id: &str) -> RuntimeError {
    RuntimeError::Start {
        class: Class::Chat,
        model_id: id.into(),
        container_name: format!("lmgw-test-{id}"),
        message: "podman run exited 125".into(),
        logs: vec![],
    }
}

fn verdict(w: &World) -> MachineVerdict {
    w.state.machine.summary().unwrap().verdict
}

/// Two starts that fail at once on a machine that changed since the check
/// (a new boot): one read of the card for both, and one full check after
/// it.
#[tokio::test]
async fn failed_starts_at_once_share_one_read_and_one_check() {
    let w = World::nvidia().await;
    let (m1, m2) = (row(&w, "m1").await, row(&w, "m2").await);
    assert_eq!(w.check().await.verdict, MachineVerdict::Ready);
    let (runs, probes) = (w.state.machine.runs(), w.probes());
    w.m.facts
        .set_boot_id("0f6c1e9a-5b7d-4e2a-9c3f-000000000003");
    *w.probe_delay.lock().unwrap() = Duration::from_millis(300);

    let (e1, e2) = (failed("m1"), failed("m2"));
    let (a, b) = tokio::join!(
        after_failed_start(&w.state, &m1, &e1),
        after_failed_start(&w.state, &m2, &e2)
    );
    assert_eq!((a, b), (None, None), "nothing wrong: their own errors");
    assert_eq!(w.state.machine.blame_reads(), 1, "one read for both");
    w.state.machine.settled().await;
    assert_eq!(w.state.machine.runs(), runs + 1, "one check after it");
    assert_eq!(
        w.probes(),
        probes + 2,
        "NVML once for the read, once for the check"
    );

    // The machine as that check read it: its verdict, nothing read again.
    assert_eq!(after_failed_start(&w.state, &m1, &e1).await, None);
    w.state.machine.settled().await;
    assert_eq!(w.state.machine.blame_reads(), 1);
    assert_eq!(w.state.machine.runs(), runs + 1);
}

/// The start's own write of lmgw's spec ran out of time on the card: its
/// answer comes without asking NVML (which would wait as long again), in
/// the toolkit item's words, before the full check in the background has
/// asked it and published.
#[tokio::test]
async fn a_write_that_ran_out_of_time_is_answered_before_the_full_check() {
    let w = World::nvidia().await;
    let m1 = row(&w, "m1").await;
    assert_eq!(w.check().await.verdict, MachineVerdict::Ready);
    let probes = w.probes();
    w.m.hang(Duration::from_secs(3));
    let prefix = w.state.snapshot().settings.container_prefix.clone();
    let e = w
        .state
        .cdi()
        .ensure_fresh(&prefix, Duration::from_millis(300))
        .await
        .unwrap_err();
    assert!(
        matches!(e, lmgw_core::runtime::cdi::CdiError::TimedOut { .. }),
        "{e:?}"
    );
    *w.probe_delay.lock().unwrap() = Duration::from_secs(2);

    let asked = Instant::now();
    let blamed = after_failed_start(&w.state, &m1, &failed("m1"))
        .await
        .expect("the card stopped answering");
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "{:?}",
        asked.elapsed()
    );
    assert_eq!(
        blamed.words(),
        "could not start on the graphics card of the computer lmgw runs on. Your graphics card \
         stopped answering while lmgw prepared it for models. Restart the computer."
    );
    assert_eq!(
        verdict(&w),
        MachineVerdict::Ready,
        "the full check still reads"
    );

    w.state.machine.settled().await;
    assert_eq!(verdict(&w), MachineVerdict::NeedsRestart);
    assert_eq!(w.probes(), probes + 1, "NVML asked by the full check alone");
}
