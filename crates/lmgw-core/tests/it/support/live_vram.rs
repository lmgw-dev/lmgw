//! Free VRAM for the owner's live audio tests: what a test needs, with where
//! every figure comes from, against what its own gateway sees free — and,
//! when that is short, who holds the memory and how to free it.
//!
//! A test is a run of **phases**, each the models it holds at once. A
//! phase needs the larger of:
//! - what the test gateway's admission lets through: every row's charge —
//!   its learned residency, or before it has one its on-disk size (realtime
//!   design §9.4; a test's rows are new, so the on-disk size) — plus the
//!   `vram.headroom_mb` admission keeps free;
//! - what the models are measured to hold, where a measurement is recorded
//!   (the caller passes it with its source). A row without one counts at its
//!   charge, which the message calls the lower bound it is: audio.cpp has
//!   been measured at 1.3–7.5× its files (§9.4).
//!
//! Holders are named the way lmgw attributes its own share
//! (`vram::attribution`): the driver's per-process list (NVML), each process
//! put to the podman container whose init process (`podman inspect`'s
//! `State.Pid`) it is or descends from (`/proc`), else named by its command.

#![allow(dead_code)]

use std::collections::HashMap;

use lmgw_core::runtime::registry::HOST_PID_FORMAT;
use lmgw_core::state::SharedState;
use lmgw_core::vram::plan::PlanCache;
use lmgw_core::vram::{GpuProbe, ProcFs, ProcTree};

const MIB: u64 = 1024 * 1024;

/// A model's figure measured elsewhere, and where it is recorded.
#[derive(Debug, Clone, Copy)]
pub struct Measured {
    pub bytes: u64,
    pub source: &'static str,
}

/// The models a test holds at once.
pub struct Phase<'a> {
    /// What the phase is, for the message ("Fish, to speak").
    pub what: &'a str,
    /// Audio model ids, each with its measured figure when one is recorded.
    pub models: Vec<(&'a str, Option<Measured>)>,
}

fn mib(bytes: u64) -> u64 {
    bytes.div_ceil(MIB)
}

/// One model's line, and its charge and the figure it is counted at.
async fn model_need(
    state: &SharedState,
    plans: &PlanCache,
    id: &str,
    measured: Option<Measured>,
) -> (u64, u64, String) {
    let snap = state.snapshot();
    let s = &snap.settings.audio;
    let row = snap
        .audio_models
        .iter()
        .find(|m| m.model_id == id)
        .unwrap_or_else(|| panic!("the test's own row '{id}' is not in its gateway"));
    let (charge, how) = match lmgw_core::vram::residency::learned(row, s) {
        Some(b) => (b, "its learned residency".to_string()),
        None => (
            plans.audio(&s.models_dir, row).await.total_bytes,
            "its on-disk size, lmgw's charge for a row that has learned nothing (realtime \
             design §9.4)"
                .to_string(),
        ),
    };
    let line = match measured {
        Some(m) => format!(
            "{id}: {} MiB measured ({}); admission charges it {} MiB, {how}",
            mib(m.bytes),
            m.source,
            mib(charge)
        ),
        None => format!(
            "{id}: {} MiB, {how} — no measured figure is recorded, so this is a lower bound \
             (audio.cpp has been measured at 1.3–7.5× its files, §9.4)",
            mib(charge)
        ),
    };
    let held = measured.map_or(charge, |m| m.bytes.max(charge));
    (charge, held, line)
}

/// Panic unless the GPU has room for every phase of `test` (module doc),
/// saying what is needed and why, and who holds the memory now.
pub async fn require(state: &SharedState, test: &str, phases: &[Phase<'_>]) {
    let view = state.vram.view(state).await;
    let plans = PlanCache::default();
    let headroom = view.headroom_bytes;
    let mut need = 0u64;
    let mut lines = Vec::new();
    for phase in phases {
        let (mut charges, mut held) = (0u64, 0u64);
        let mut models = Vec::new();
        for (id, measured) in &phase.models {
            let (c, h, line) = model_need(state, &plans, id, *measured).await;
            charges += c;
            held += h;
            models.push(format!("    - {line}"));
        }
        let admitted = charges + headroom;
        let phase_need = admitted.max(held);
        need = need.max(phase_need);
        lines.push(format!(
            "  {}: {} MiB — the larger of what admission wants free (the charges {} + \
             vram.headroom_mb {} = {} MiB) and what the models hold ({} MiB)\n{}",
            phase.what,
            mib(phase_need),
            mib(charges),
            mib(headroom),
            mib(admitted),
            mib(held),
            models.join("\n")
        ));
    }
    let free = match view.free_bytes.filter(|_| view.free_measured) {
        Some(f) => f,
        None => panic!(
            "{test} cannot run: the free VRAM cannot be measured here ({}), and it needs {} \
             MiB:\n{}",
            view.telemetry,
            mib(need),
            lines.join("\n")
        ),
    };
    if free >= need {
        eprintln!(
            "{test}: {} MiB of VRAM free ({}), {} MiB needed",
            free / MIB,
            view.telemetry,
            mib(need)
        );
        return;
    }
    let phased = if phases.len() > 1 {
        ", what the largest of its phases needs (they run one after another)"
    } else {
        ""
    };
    panic!(
        "{test} cannot run: only {} MiB of VRAM are free ({}) and it needs {} MiB{phased}:\n{}\n\n\
         Held now:\n{}\n\n\
         To free it: stop the idle lmgw models that hold it — from the dashboard, or \
         `lmgw__container model=<id> action=stop` (`lmgw__container target=audio action=stop` \
         stops every audio model). Memory held by anything else is not lmgw's to free.",
        free / MIB,
        view.telemetry,
        mib(need),
        lines.join("\n"),
        holders(&*state.vram.probe())
    );
}

/// `probe`'s per-process list — the gateway's own, the driver's on a live
/// run — largest first, each process named by its podman container or its
/// command (module doc).
pub fn holders(probe: &dyn GpuProbe) -> String {
    let listed = match probe.processes(&[], &[]) {
        Ok(l) => l,
        Err(e) => return format!("  (the driver lists no processes: {e})"),
    };
    if listed.is_empty() {
        return "  (the driver lists no process holding memory)".into();
    }
    let containers = container_pids();
    let tree = ProcFs::default();
    let owner: HashMap<u32, &str> = containers
        .iter()
        .flat_map(|(name, root)| {
            std::iter::once(*root)
                .chain(tree.descendants(*root))
                .map(move |pid| (pid, name.as_str()))
        })
        .collect();
    let mut rows: Vec<(Option<u64>, String)> = listed
        .iter()
        .map(|p| {
            let comm = command(p.pid);
            let who = match owner.get(&p.pid) {
                Some(c) => format!("container {c} ({comm}, pid {})", p.pid),
                None => format!("{comm} (pid {})", p.pid),
            };
            (p.bytes, who)
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    rows.iter()
        .map(|(bytes, who)| match bytes {
            Some(b) => format!("  - {who}: {} MiB", b / MIB),
            None => format!("  - {who}: the driver cannot say how much"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A process's command: its program's file name, from `/proc/<pid>/cmdline`
/// (up to the first space too — Chromium rewrites its title into one
/// string), else the kernel's 15-character `comm`, else `?`.
fn command(pid: u32) -> String {
    let argv0 = std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .and_then(|c| {
            let line = String::from_utf8_lossy(&c).into_owned();
            let program = line.split(['\0', ' ']).next()?;
            let name = program.rsplit('/').next()?.trim().to_string();
            (!name.is_empty()).then_some(name)
        });
    argv0
        .or_else(|| {
            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .ok()
                .map(|c| c.trim().to_string())
        })
        .unwrap_or_else(|| "?".into())
}

/// Every running podman container's name and host init PID.
fn container_pids() -> Vec<(String, u32)> {
    let Ok(ps) = std::process::Command::new("podman")
        .args(["ps", "--format", "{{.Names}}"])
        .output()
    else {
        return Vec::new();
    };
    let names: Vec<String> = String::from_utf8_lossy(&ps.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        return Vec::new();
    }
    let Ok(out) = std::process::Command::new("podman")
        .args(["inspect", "--format", HOST_PID_FORMAT])
        .args(&names)
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (name, pid) = l.trim().rsplit_once(' ')?;
            let pid: u32 = pid.parse().ok().filter(|p| *p > 0)?;
            Some((name.trim_start_matches('/').to_string(), pid))
        })
        .collect()
}
