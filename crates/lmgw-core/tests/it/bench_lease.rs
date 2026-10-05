//! A benchmark's GPU lease against starts that were decided just before it
//! (benchmark design §3.2, §13 decision 44): every admission path asks the
//! lease on arrival, and then awaits — a footprint, the gate, a ledger read —
//! before it claims and starts. A lease taken in between must refuse the
//! start rather than let a container onto the card the run's drain has
//! already found empty. The run-level half (the drain waits for a decision
//! in flight) is `bench.rs`'s.
//!
//! A decision is held in flight by [`HeldProbe`]: the ledger read it makes
//! last before its claim waits until the test has taken the lease.

use std::sync::Arc;

use lmgw_core::bench::lease::lease;
use lmgw_core::runtime::descriptor::model_runtime;
use lmgw_core::runtime::lifecycle::acquire_spec;
use lmgw_core::runtime::registry::RuntimeError;
use lmgw_core::runtime::Class;
use lmgw_core::vram::{BackgroundStart, Fit, GpuProbe};

use crate::support::bench_fake::HeldProbe;
use crate::support::gpu_world::{Gpu, GIB};

async fn world() -> (Gpu, Arc<HeldProbe>) {
    let g = Gpu::new(24 * GIB, 2, 5).await;
    g.model("other", GIB).await;
    g.model("up", GIB).await;
    let probe = HeldProbe::wrap(g.state.vram.probe());
    g.state.vram.set_probe(probe.clone() as Arc<dyn GpuProbe>);
    (g, probe)
}

/// The net under every path: with the lease held, the registry creates no
/// entry — it refuses under its map lock — while a claim on a container
/// that is already up is still a claim, which the run's drain waits for.
#[tokio::test]
async fn the_registry_starts_nothing_under_the_lease_but_joins_what_is_up() {
    let (g, _) = world().await;
    drop(
        lmgw_core::vram::admit(&g.state, &g.route("up"), "up")
            .await
            .unwrap(),
    );
    assert_eq!(g.runs(), vec!["up".to_string()]);

    g.state.set_gpu_lease(Some(lease(7, "qwen")));
    let snap = g.state.snapshot();
    let other = model_runtime(&snap, Class::Chat, "other").unwrap();
    let err = g
        .state
        .runtime()
        .acquire(&acquire_spec(&g.state, &snap, &other))
        .await
        .map(|_| ())
        .unwrap_err();
    assert!(
        matches!(&err, RuntimeError::GpuBenchmark { run_id: 7, holder, .. } if holder == "qwen"),
        "{err}"
    );
    assert!(err.to_string().contains("benchmark run 7"), "{err}");
    assert_eq!(g.runs(), vec!["up".to_string()], "nothing was started");

    let up = model_runtime(&snap, Class::Chat, "up").unwrap();
    let joined = g
        .state
        .runtime()
        .acquire(&acquire_spec(&g.state, &snap, &up))
        .await;
    assert!(joined.is_ok(), "a claim on a ready container is a join");
    drop(joined);

    g.state.set_gpu_lease(None);
    assert!(g.state.runtime().gpu_lease().is_none());
    drop(
        g.state
            .runtime()
            .acquire(&acquire_spec(&g.state, &snap, &other))
            .await
            .unwrap(),
    );
    assert_eq!(g.runs(), vec!["up".to_string(), "other".to_string()]);
}

/// A request that passed the lease check and then found no capacity to
/// arbitrate against (telemetry gone mid-read) goes straight to `acquire` —
/// it asks again first, and is refused as the lease refuses.
#[tokio::test]
async fn a_request_decided_before_the_lease_is_refused_at_its_start() {
    let (g, probe) = world().await;
    probe.hold_next(Some(Err("the driver went away".into())));
    let (state, route) = (g.state.clone(), g.route("other"));
    let admit = tokio::spawn(async move { lmgw_core::vram::admit(&state, &route, "other").await });
    probe.held().await;
    g.state.set_gpu_lease(Some(lease(7, "qwen")));
    probe.release();
    let err = admit.await.unwrap().map(|_| ()).unwrap_err();
    assert_eq!(err.code(), "gpu_benchmark", "{err}");
    assert!(err.to_string().contains("run 7"), "{err}");
    assert!(g.runs().is_empty(), "nothing was started");
}

/// An operator or warm start holds the admission gate for its ledger read;
/// a lease taken during it is asked again under the gate, before the
/// reservation — so the drain, which takes the gate before it looks, never
/// misses the start.
#[tokio::test]
async fn an_operator_start_decided_before_the_lease_is_held_back_under_the_gate() {
    let (g, probe) = world().await;
    probe.hold_next(None);
    let state = g.state.clone();
    let check = tokio::spawn(async move {
        let snap = state.snapshot();
        match state
            .vram
            .check_background_start(&state, &snap, Class::Chat, "other")
            .await
        {
            Fit::Held(why) => Ok(why),
            Fit::Go(_) => Err("go"),
            Fit::Full(_) => Err("full"),
            Fit::Unchecked => Err("unchecked"),
        }
    });
    probe.held().await;
    g.state.set_gpu_lease(Some(lease(7, "qwen")));
    probe.release();
    let why = check.await.unwrap().expect("held back");
    assert!(why.contains("benchmark run 7"), "{why}");
    assert!(g.state.vram.pending_starts().is_empty(), "nothing reserved");
}

/// A guest's start decides under the gate too, and is refused the same way.
#[tokio::test]
async fn a_guest_start_decided_before_the_lease_is_refused() {
    let (g, probe) = world().await;
    probe.hold_next(None);
    let (state, route) = (g.state.clone(), g.route("other"));
    let start = tokio::spawn(async move {
        lmgw_core::vram::start_background(&state, &route, "bg")
            .await
            .map(|s| matches!(s, BackgroundStart::Started(_)))
    });
    probe.held().await;
    g.state.set_gpu_lease(Some(lease(7, "qwen")));
    probe.release();
    let err = start.await.unwrap().unwrap_err();
    assert_eq!(err.code(), "gpu_benchmark", "{err}");
    assert!(g.runs().is_empty(), "nothing was started");
    assert!(g.state.vram.pending_starts().is_empty(), "nothing reserved");
}

/// An operator start the lease overtakes (it passed `container`'s own gate
/// just before) names the benchmark, not the hold (review finding 8).
#[tokio::test]
async fn an_operator_start_overtaken_by_the_lease_names_the_benchmark() {
    let (g, probe) = world().await;
    probe.hold_next(None);
    let state = g.state.clone();
    let start = tokio::spawn(async move {
        lmgw_core::ops::container(&state, None, Some("other"), "start", false, None).await
    });
    probe.held().await;
    g.state.set_gpu_lease(Some(lease(7, "qwen")));
    probe.release();
    let err = start.await.unwrap().unwrap_err();
    assert!(err.contains("benchmark run 7"), "{err}");
    assert!(!err.contains("hold"), "{err}");
    assert!(g.runs().is_empty(), "nothing was started");
}
