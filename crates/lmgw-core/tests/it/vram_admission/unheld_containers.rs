//! Running containers the registry does not hold, after boot (§3.4,
//! `docs/design/2026-10-06-registry-owns-start.md`): the reaper tick's
//! reconciliation pass adopts one that runs what its model renders now and
//! removes one that does not; an admission with nothing of lmgw's to evict
//! looks once before it waits; `container stop` stops one by name.
//!
//! The orphan is made the way a lost entry looks from podman's side: the
//! model is started normally, then the registry is swapped for one that
//! holds nothing, over the same fake world — the container runs on, unheld.

use std::time::Instant;

use lmgw_core::runtime::registry::{PassWait, RuntimeState, RuntimeView};

use super::*;

/// One reconciliation pass, run here rather than on the tick, and its verdict.
pub(super) async fn pass(f: &Fixture) -> bool {
    lmgw_core::runtime::lifecycle::readopt(&f.state, PassWait::Join, Duration::from_secs(30)).await
}

/// `model`'s registry entry, if it has one.
pub(super) fn entry(f: &Fixture, model: &str) -> Option<RuntimeView> {
    f.state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.model_id == model)
}

/// Swap in a registry that holds nothing (module doc). Its starts get the
/// fixture's second and third containers: the first is the orphan's. Wired by
/// the same state, so it is the same lmgw — the orphan's owner label is its.
pub(super) fn forget_everything(f: &Fixture) {
    let ports = Arc::new(Mutex::new(VecDeque::from([
        f._second.address().port(),
        f._third.address().port(),
    ])));
    f.state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        f.podman.clone(),
        reqwest::Client::new(),
        Arc::new(move || {
            ports.lock().unwrap().pop_front().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    "test allocator ran out of ports — an unexpected extra start",
                )
            })
        }),
    )));
    assert!(f.state.runtime().list().is_empty());
}

#[tokio::test]
async fn a_running_container_the_registry_lost_is_adopted_on_the_next_tick() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.world().reports = true;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    let name = entry(&f, "chat-model").unwrap().container_name;
    forget_everything(&f);

    let (log, capturing) = crate::common::captured_log::capture_log();
    // The tick's pass is its own task: the reaping does not wait for it.
    lmgw_core::runtime::lifecycle::reap_idle(&f.state).await;
    super::abandoned_requests::until("the tick's pass to adopt it", || {
        entry(&f, "chat-model").is_some()
    })
    .await;
    drop(capturing);

    let e = entry(&f, "chat-model").expect("adopted on the tick");
    assert_eq!(e.state, RuntimeState::Ready);
    assert_eq!(e.in_flight, 0, "idle: nobody holds a claim on it");
    assert_eq!(e.container_name, name);
    assert_eq!(e.port, f.first.address().port());
    let text = log.text();
    assert!(
        text.contains("WARN")
            && text.contains("adopted a running container")
            && text.contains(&name),
        "{text}"
    );

    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert_eq!(
        f.runs(),
        vec!["chat-model".to_string()],
        "nothing started twice"
    );
    assert!(f.stops().is_empty(), "{:?}", f.stops());
}

#[tokio::test]
async fn a_lost_container_that_no_longer_matches_its_row_is_removed() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.world().reports = true;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    let name = entry(&f, "chat-model").unwrap().container_name;
    forget_everything(&f);
    // Edited since it started: the container runs the old command line.
    edit_chat_model(&f, |p| p.ctx_size = Some(4096)).await;

    let (log, capturing) = crate::common::captured_log::capture_log();
    assert!(pass(&f).await, "the pass changed what the registry holds");
    drop(capturing);

    assert!(entry(&f, "chat-model").is_none(), "not adopted");
    assert_eq!(
        f.stops(),
        vec!["chat-model".to_string()],
        "taken off the card"
    );
    assert!(!f.world().loaded.contains("chat-model"));
    let text = log.text();
    assert!(
        text.contains("removed a running container")
            && text.contains(&name)
            && text.contains("its command is not the one this model renders now"),
        "{text}"
    );

    // The next request starts it on the edited row.
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert_eq!(f.runs(), vec!["chat-model".to_string(); 2]);
}

/// The incident's own shape: the memory in the way is a model lmgw started
/// and lost track of. An admission that finds nothing of lmgw's to evict
/// looks once before it settles into waiting — instead of waiting out
/// `vram.queue_timeout_seconds` and refusing with `vram_queue_timeout`.
#[tokio::test]
async fn an_admission_with_nothing_to_evict_adopts_before_it_waits() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.world().reports = true;
    assert_eq!(embed(&f.gateway).await.status(), 200);
    forget_everything(&f);

    // chat needs 6.5 GiB with 5 free: only the embedder is in the way.
    let started = Instant::now();
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "answered well inside the queue timeout, not at its end: {:?}",
        started.elapsed()
    );
    assert_eq!(
        f.stops(),
        vec!["embed-model".to_string()],
        "adopted, then evicted like any idle model"
    );
    assert_eq!(
        f.runs(),
        vec!["embed-model".to_string(), "chat-model".to_string()]
    );
}

#[tokio::test]
async fn container_stop_without_an_entry_stops_the_running_container_by_name() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.world().reports = true;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    let name = entry(&f, "chat-model").unwrap().container_name;
    forget_everything(&f);

    let out = crate::common::container_wire(
        lmgw_core::ops::container(&f.state, None, Some("chat-model"), "stop", false, None)
            .await
            .unwrap(),
    );
    let message = out["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("no lmgw entry") && message.contains(&name),
        "says what it stopped, and why it is news: {out}"
    );
    assert_eq!(f.stops(), vec!["chat-model".to_string()]);
    assert!(!f.world().loaded.contains("chat-model"));
    assert!(
        f.state.runtime().list().is_empty(),
        "no entry left behind: {:?}",
        f.state.runtime().list()
    );

    // Nothing running: it says so, rather than "stopped".
    let out = crate::common::container_wire(
        lmgw_core::ops::container(&f.state, None, Some("chat-model"), "stop", false, None)
            .await
            .unwrap(),
    );
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not running"),
        "{out}"
    );
    assert_eq!(f.stops().len(), 1);
}
