//! §1.2 after a failed write of lmgw's own spec: only the toolkit's own
//! failures (`cdi generate` failing, a spec naming paths that are not there)
//! offer its update (S4′); a write that ran out of the load timeout is the
//! card's (a restart), a folder that is not private is its own cause with
//! no plan, and another write holding the lock is nobody's (the next check
//! decides). The classification of every error is unit-tested beside it
//! (`machine::toolkit`).

use std::time::Duration;

use lmgw_api_types::machine::{
    ItemState, MachineCause, MachineItemId, MachineVerdict, PlanStepKind,
};

use crate::support::cdi_machine::SPEC;
use crate::support::machine_world::World;

fn item(
    c: &lmgw_api_types::machine::MachineCheck,
    id: MachineItemId,
) -> &lmgw_api_types::machine::MachineItem {
    c.items.iter().find(|i| i.id == id).unwrap()
}

fn prefix(w: &World) -> String {
    w.state.snapshot().settings.container_prefix.clone()
}

/// What `cdi generate` wrote names a library that is not there: the
/// toolkit's fault, so its update is offered.
#[tokio::test]
async fn a_spec_naming_missing_paths_offers_the_update() {
    let w = World::nvidia().await;
    w.m.generates(&SPEC.replace(
        "/usr/bin/nvidia-smi",
        "/usr/bin/nvidia-smi-that-is-not-there",
    ));
    let e = w
        .state
        .cdi()
        .ensure_fresh(&prefix(&w), Duration::from_secs(60))
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("names paths that are not there"),
        "{e}"
    );
    let c = w.check().await;
    let toolkit = item(&c, MachineItemId::CardForModels);
    assert!(
        matches!(&toolkit.cause, Some(MachineCause::SpecNotWritten { .. })),
        "{toolkit:#?}"
    );
    let kinds: Vec<PlanStepKind> = c.plan.unwrap().steps.iter().map(|s| s.kind).collect();
    assert!(kinds.contains(&PlanStepKind::S4Update), "{kinds:?}");
}

/// `cdi generate` hanging past the load timeout (NVML not answering, a card
/// off the bus): a restart, named with the load timeout's setting, and no
/// update offered — reinstalling the toolkit would not wake the card.
#[tokio::test]
async fn a_write_that_ran_out_of_time_is_the_cards_and_needs_a_restart() {
    let w = World::nvidia().await;
    w.m.hang(Duration::from_secs(3));
    let e = w
        .state
        .cdi()
        .ensure_fresh(&prefix(&w), Duration::from_millis(300))
        .await
        .unwrap_err();
    assert!(
        matches!(e, lmgw_core::runtime::cdi::CdiError::TimedOut { .. }),
        "{e:?}"
    );
    let c = w.check().await;
    let toolkit = item(&c, MachineItemId::CardForModels);
    assert_eq!(toolkit.state, ItemState::Problem);
    assert!(
        matches!(&toolkit.cause, Some(MachineCause::SpecWriteTimedOut { .. })),
        "{toolkit:#?}"
    );
    assert_eq!(c.verdict, MachineVerdict::NeedsRestart, "{c:#?}");
    assert_eq!(c.headline, toolkit.sentence);
    assert!(
        c.headline.contains("Restart the computer"),
        "{}",
        c.headline
    );
    assert!(
        toolkit
            .detail
            .iter()
            .any(|l| l.contains("Settings → GPU → Admission → Load timeout")),
        "{:?}",
        toolkit.detail
    );
    assert!(
        c.plan.is_none(),
        "no update for a card that stopped answering"
    );
}

/// A folder of lmgw's that others may write into: its own cause and plain
/// sentence, the folder in the detail, and no plan of root steps.
#[tokio::test]
async fn a_folder_that_is_not_private_is_its_own_cause_without_a_plan() {
    use std::os::unix::fs::PermissionsExt;
    let w = World::nvidia().await;
    let machine_dir = w.m.runtime_dir.path().join("lmgw-machine");
    std::fs::create_dir(&machine_dir).unwrap();
    std::fs::set_permissions(&machine_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let e = w
        .state
        .cdi()
        .ensure_fresh(&prefix(&w), Duration::from_secs(60))
        .await
        .unwrap_err();
    assert!(
        matches!(e, lmgw_core::runtime::cdi::CdiError::NotPrivate { .. }),
        "{e:?}"
    );
    let c = w.check().await;
    let toolkit = item(&c, MachineItemId::CardForModels);
    assert!(
        matches!(&toolkit.cause, Some(MachineCause::SpecDirNotPrivate { error })
            if error.contains(&machine_dir.display().to_string())),
        "{toolkit:#?}"
    );
    assert!(!toolkit.sentence.contains('/'), "no path in the sentence");
    assert!(c.plan.is_none());
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps);
    assert_eq!(c.headline, toolkit.sentence);
}

/// Another write holding the one-at-a-time lock for the whole load timeout
/// (a warm start at boot): nobody's fault, so no problem and no update; the
/// next start or check decides.
#[tokio::test]
async fn a_write_that_waited_for_another_is_no_problem() {
    use std::sync::{mpsc, Arc, Condvar, Mutex};
    let w = World::nvidia().await;
    // The first write stops inside `cdi generate`, holding the lock, until
    // the test lets it go.
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let entered_tx = Mutex::new(entered_tx);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let g = gate.clone();
    w.m.while_generating(move |_| {
        let _ = entered_tx.lock().unwrap().send(());
        let (open, cv) = &*g;
        let mut open = open.lock().unwrap();
        while !*open {
            open = cv.wait(open).unwrap();
        }
    });
    let own = w.state.cdi();
    let p = prefix(&w);
    let first = {
        let (own, p) = (own.clone(), p.clone());
        tokio::spawn(async move { own.ensure_fresh(&p, Duration::from_secs(60)).await })
    };
    tokio::task::spawn_blocking(move || entered_rx.recv().unwrap())
        .await
        .unwrap();
    let e = own
        .ensure_fresh(&p, Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(
        matches!(e, lmgw_core::runtime::cdi::CdiError::Waited { .. }),
        "{e:?}"
    );
    let c = w.check().await;
    let toolkit = item(&c, MachineItemId::CardForModels);
    assert_ne!(toolkit.state, ItemState::Problem, "{toolkit:#?}");
    assert!(c.plan.is_none(), "{:?}", c.plan);
    {
        let (open, cv) = &*gate;
        *open.lock().unwrap() = true;
        cv.notify_all();
    }
    first.await.unwrap().unwrap();
}
