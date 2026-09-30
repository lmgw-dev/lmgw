//! Degradation

use super::*;

/// A host with no NVML (CI, a laptop, this suite) has nothing to admit against.
/// The ledger has to say so in words and then get out of the way — never a
/// panic, and never a silent refusal of traffic that used to work.
///
/// "Out of the way" now means something stronger than it did in router mode:
/// the router used to autoload on this path, so admission could return
/// `Ok(None)` and forward. Per-model, nothing is running until lmgw starts it,
/// so an inactive scheduler still acquires — unarbitrated (§3.2).
#[tokio::test]
async fn without_gpu_telemetry_admission_is_inactive_and_still_starts_the_container() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    // Back to the default test probe: no telemetry at all.
    f.state
        .vram
        .set_probe(Arc::new(lmgw_core::vram::nvml::NoTelemetry(
            "no driver on this host".into(),
        )));

    let v = vram_status(&f.gateway).await;
    assert_eq!(v["enabled"], true);
    assert_eq!(v["active"], false, "nothing to measure against: {v}");
    assert_eq!(v["telemetry_ok"], false);
    let reason = v["inactive_reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("budget_mb") && reason.contains("no driver on this host"),
        "the reason must name both the missing driver and the way out: {reason}"
    );
    assert!(v["free_bytes"].is_null(), "no measurement, no free figure");

    // …and traffic still flows — which, with one container per model, means the
    // container was started even though nothing arbitrated the start.
    assert_eq!(
        embed(&f.gateway).await.status(),
        200,
        "degraded must never mean blocked"
    );
    assert_eq!(
        f.runs(),
        vec!["embed-model".to_string()],
        "an inactive scheduler still has to make the model reachable"
    );

    // The same figures reach the tool plane, which is where an agent looks.
    let status = lmgw_core::ops::status(&f.state).await.unwrap();
    assert_eq!(status["vram"]["active"], false);
    assert!(status["vram"]["inactive_reason"].is_string());
}

/// The switch off entirely: no ledger, no queue, no eviction — and still a
/// running container and a usable endpoint. `admit` is the only thing that
/// makes a local model reachable, so "vram.enabled = false" cannot be allowed
/// to mean "local models do not work" (§3.2).
#[tokio::test]
async fn with_admission_disabled_a_local_route_still_acquires_an_endpoint() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let mut s = f.state.snapshot().settings.clone();
    s.vram.enabled = false;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();

    let route = f.state.snapshot().resolve("chat-model").unwrap();
    let hold = lmgw_core::vram::admit(&f.state, &route, "chat-model")
        .await
        .expect("a disabled scheduler must not refuse")
        .expect("a local model always gets a hold");

    assert_eq!(
        hold.endpoint(),
        format!("http://127.0.0.1:{}/v1", f.first.address().port()),
        "the hold carries the container's own base URL, `/v1` included"
    );
    assert_eq!(hold.model_id(), "chat-model");
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);

    // And the request that follows really does land there, not on the dead
    // class port the route was resolved against.
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert_eq!(f.runs().len(), 1, "the second request reused the container");
}

/// With no driver but a declared budget the owner has supplied the one number
/// NVML would have: plan against it, on estimates alone, and label the free
/// figure as derived rather than measured.
#[tokio::test]
async fn a_declared_budget_substitutes_for_missing_telemetry() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 0).await;
    f.state
        .vram
        .set_probe(Arc::new(lmgw_core::vram::nvml::NoTelemetry("none".into())));
    let mut s = f.state.snapshot().settings.clone();
    s.vram.budget_mb = 4096;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();

    let v = vram_status(&f.gateway).await;
    assert_eq!(v["active"], true);
    assert_eq!(v["capacity_bytes"], 4 * GIB);
    assert_eq!(v["free_measured"], false, "derived, and it says so: {v}");

    // 6 GiB of chat model against a 4 GiB declared budget can never fit.
    assert_eq!(chat(&f.gateway).await.status(), 507);
    assert!(f.runs().is_empty(), "and nothing was started for it");
}
