//! After the root helper ran (§3.5, WP7): `machine::after_fix` writes
//! lmgw's own spec and checks again, the ledger's probe is replaced only
//! when it read nothing and the card works, and a plan the helper read
//! differently (D88) is offered as instructions from then on.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use lmgw_api_types::machine::{MachineVerdict, PlanHow, PlanStepKind};
use lmgw_core::vram::{GpuMemory, GpuProbe};

use crate::support::machine_world::{World, NVIDIA_OWNER};

fn install_helper(w: &World) {
    w.m.facts
        .file_mode(lmgw_machine::plan::embedded::HELPER, b"\x7fELF", 0o755)
        .file_mode(
            lmgw_machine::plan::embedded::POLICY,
            b"<policyconfig/>",
            0o644,
        )
        .file_mode(lmgw_machine::plan::embedded::PKEXEC, b"\x7fELF", 0o4755);
}

/// A probe that reads nothing, or the card.
struct Probe(Option<&'static str>);

impl GpuProbe for Probe {
    fn devices(&self) -> Result<Vec<GpuMemory>, String> {
        match self.0 {
            Some(name) => Ok(vec![GpuMemory {
                index: 0,
                name: name.into(),
                total_bytes: 24 << 30,
                used_bytes: 0,
                free_bytes: 24 << 30,
            }]),
            None => Err("NVML unavailable: Driver Not Loaded".into()),
        }
    }

    fn source(&self) -> String {
        match self.0 {
            Some(name) => format!("stated probe ({name})"),
            None => "NVML unavailable: Driver Not Loaded".into(),
        }
    }
}

static DETECTED: AtomicUsize = AtomicUsize::new(0);

fn detect_the_card() -> Arc<dyn GpuProbe> {
    DETECTED.fetch_add(1, Ordering::SeqCst);
    Arc::new(Probe(Some("detected after the setup")))
}

/// The toolkit missing and the button offered; the steps install it; the
/// check afterwards writes lmgw's spec and says ready, and the ledger's
/// probe, which read nothing since boot, is replaced by one that reads the
/// card.
#[tokio::test]
async fn after_the_steps_the_spec_is_written_the_check_agrees_and_the_probe_reads() {
    let w = World::nvidia().await;
    w.remove_toolkit();
    install_helper(&w);
    let before = w.check().await;
    let plan = before.plan.as_ref().unwrap();
    assert_eq!(plan.how, PlanHow::Helper);
    assert!(plan.steps.iter().any(|s| s.kind == PlanStepKind::S4Install));
    w.state.vram.set_probe(Arc::new(Probe(None)));

    // What the helper's steps did.
    w.m.install_toolkit();
    *w.owner.lock().unwrap() = Some(NVIDIA_OWNER.to_string());
    let generations = w.m.generations();

    let before_detect = DETECTED.load(Ordering::SeqCst);
    let (check, fixed) = lmgw_core::machine::after_fix(&w.state, detect_the_card).await;
    assert_eq!(check.verdict, MachineVerdict::Ready, "{check:#?}");
    assert!(w.m.generations() > generations, "lmgw's spec was written");
    assert!(
        fixed.spec.contains("CDI spec after the setup"),
        "{}",
        fixed.spec
    );
    assert!(fixed.probe_installed);
    assert_eq!(DETECTED.load(Ordering::SeqCst), before_detect + 1);
    assert_eq!(
        w.state.vram.probe().source(),
        "stated probe (detected after the setup)"
    );
    // Published: the status says it.
    assert_eq!(
        w.state.machine.summary().unwrap().verdict,
        MachineVerdict::Ready
    );
}

static NEVER: AtomicUsize = AtomicUsize::new(0);

fn detect_never() -> Arc<dyn GpuProbe> {
    NEVER.fetch_add(1, Ordering::SeqCst);
    Arc::new(Probe(None))
}

/// A probe that reads the card already is left alone, and so is one on a
/// machine whose card the check says does not work.
#[tokio::test]
async fn a_probe_that_reads_or_a_card_that_does_not_work_is_left_alone() {
    let w = World::nvidia().await;
    w.state
        .vram
        .set_probe(Arc::new(Probe(Some("the boot's probe"))));
    let (_, fixed) = lmgw_core::machine::after_fix(&w.state, detect_never).await;
    assert!(!fixed.probe_installed);
    assert_eq!(
        w.state.vram.probe().source(),
        "stated probe (the boot's probe)"
    );

    // A driver whose two halves disagree (NVML says so): no NVIDIA card
    // that works, so the probe is not detected again.
    let w = World::nvidia().await;
    w.state.vram.set_probe(Arc::new(Probe(None)));
    w.m.facts
        .file("/usr/lib64/libnvidia-ml.so", b"\x7fELF")
        .link("/usr/lib64/libnvidia-ml.so.1", "libnvidia-ml.so");
    *w.nvml.lock().unwrap() = Some(lmgw_core::machine::probe::NvmlAnswer::Failed {
        kind: "version_mismatch".into(),
        error: "RM has detected an NVML/RM version mismatch.".into(),
    });
    let (check, fixed) = lmgw_core::machine::after_fix(&w.state, detect_never).await;
    assert_eq!(check.verdict, MachineVerdict::NeedsRestart, "{check:#?}");
    assert!(!fixed.probe_installed);
    assert_eq!(NEVER.load(Ordering::SeqCst), 0, "nothing was detected");
}

/// A plan the helper read differently is instructions from then on, saying
/// so; the button is gone with it.
#[tokio::test]
async fn a_plan_the_helper_read_differently_is_offered_as_instructions() {
    let w = World::nvidia().await;
    w.remove_toolkit();
    install_helper(&w);
    let before = w.check().await;
    assert_eq!(before.verdict, MachineVerdict::NeedsFix);
    let hash = before.plan.unwrap().hash;
    w.state.machine.read_differently(&hash);
    let after = w.check().await;
    let plan = after.plan.unwrap();
    assert_eq!(plan.hash, hash, "the same plan, the same commands");
    assert_eq!(
        plan.how,
        PlanHow::Instructions {
            why: lmgw_machine::plan::instructions::READ_DIFFERENTLY.into()
        }
    );
    assert_eq!(after.verdict, MachineVerdict::NeedsSteps);
}
