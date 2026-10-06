//! What the reconciliation pass after boot must leave alone, and how it
//! behaves when it cannot finish (`registry/unheld.rs`): a start in flight;
//! a benchmark's, an agent's, another lmgw's and an owner-less container; a
//! container still loading; two passes at once; a container it cannot remove;
//! a stop landing on one of its removals.

use lmgw_core::runtime::argv::OWNER_LABEL;
use lmgw_core::runtime::registry::RuntimeState;
use lmgw_core::runtime::Class;

use super::abandoned_requests::until;
use super::unheld_containers::{entry, forget_everything, pass};
use super::*;

/// A `podman ps` row for a running container this world does not run, under
/// the fixture's prefix, with `labels` on top.
fn row(name: &str, labels: &[(&str, &str)]) -> Value {
    let mut all: HashMap<String, String> = [
        ("lmgw.instance", "lmgw"),
        ("lmgw.class", "chat"),
        ("lmgw.model", "chat-model"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, v) in labels {
        all.insert(k.to_string(), v.to_string());
    }
    json!({"Names": [name], "Labels": all, "State": "running", "Created": 0})
}

/// The calls that named `name` beyond the listing itself.
fn touched(f: &Fixture, name: &str) -> Vec<Vec<String>> {
    f.world().calls_naming(name)
}

#[tokio::test]
async fn a_start_in_flight_is_left_to_finish() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.world().reports = true;
    f.world().health_held = true;
    let gateway = f.gateway.clone();
    let chatting = tokio::spawn(async move { chat(&gateway).await.status() });
    until("the start's podman run", || {
        f.runs() == vec!["chat-model".to_string()]
    })
    .await;
    let name = entry(&f, "chat-model").unwrap().container_name;
    assert_eq!(
        entry(&f, "chat-model").unwrap().state,
        RuntimeState::Starting
    );

    // Running, labelled ours, not answering — and its entry `starting`.
    assert!(!pass(&f).await, "nothing changed");
    assert!(
        touched(&f, &name)
            .iter()
            .all(|c| c[0] == "run" || c[0] == "inspect"),
        "a start's own container is not the pass's: {:?}",
        touched(&f, &name)
    );
    assert!(f.stops().is_empty());

    f.world().health_held = false;
    assert_eq!(chatting.await.unwrap(), 200);
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
}

#[tokio::test]
async fn benchmark_agent_foreign_and_ownerless_containers_are_left_alone() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.world().reports = true;
    let me = f.state.builds.instance_id().to_string();
    {
        let mut w = f.world();
        w.extra_ps = vec![
            row(
                "lmgw-bench-7",
                &[
                    (OWNER_LABEL, me.as_str()),
                    (lmgw_core::bench::BENCH_LABEL, "7"),
                ],
            ),
            row(
                "lmgw-agent-x-1",
                &[
                    (OWNER_LABEL, me.as_str()),
                    (
                        lmgw_core::agents::container::LABEL_KIND,
                        lmgw_core::agents::container::KIND_AGENT,
                    ),
                ],
            ),
            row("lmgw-chat-theirs-1", &[(OWNER_LABEL, "0ther0")]),
            row("lmgw-chat-old-1", &[]),
        ];
    }

    let (log, capturing) = crate::common::captured_log::capture_log();
    assert!(!pass(&f).await);
    assert!(!pass(&f).await, "and again, the same");
    drop(capturing);

    for name in [
        "lmgw-bench-7",
        "lmgw-agent-x-1",
        "lmgw-chat-theirs-1",
        "lmgw-chat-old-1",
    ] {
        assert!(
            touched(&f, name).is_empty(),
            "{name}: {:?}",
            touched(&f, name)
        );
    }
    assert!(f.state.runtime().list().is_empty());
    // Another lmgw's and an owner-less one are said, once each.
    let text = log.text();
    for (name, says) in [
        (
            "lmgw-chat-theirs-1",
            "belongs to another lmgw (owner 0ther0)",
        ),
        ("lmgw-chat-old-1", "carries no owner label"),
    ] {
        let lines: Vec<&str> = text.lines().filter(|l| l.contains(name)).collect();
        assert_eq!(lines.len(), 1, "{name}: {text}");
        assert!(
            lines[0].contains("WARN") && lines[0].contains(says),
            "{text}"
        );
    }
}

/// The run's own owner label is what the pass keys on: the orphan it adopts
/// carries this lmgw's instance id.
#[tokio::test]
async fn a_start_stamps_this_instances_owner_label() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    let argv = f.world().argv["chat-model"].clone();
    let me = format!("{OWNER_LABEL}={}", f.state.builds.instance_id());
    assert!(
        argv.windows(2).any(|w| w[0] == "--label" && w[1] == me),
        "{argv:?}"
    );
}

#[tokio::test]
async fn two_passes_at_once_are_one() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.world().reports = true;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    forget_everything(&f);
    let go = f.podman.hold_inspects();
    let inspects = |f: &Fixture| f.world().verb("inspect");
    let before = inspects(&f);

    let (log, capturing) = crate::common::captured_log::capture_log();
    let first = tokio::spawn({
        let state = f.state.clone();
        async move {
            lmgw_core::runtime::lifecycle::readopt(
                &state,
                lmgw_core::runtime::registry::PassWait::Join,
                Duration::from_secs(30),
            )
            .await
        }
    });
    until("the first pass inside its adoption", || {
        inspects(&f) > before
    })
    .await;
    let second = tokio::spawn({
        let state = f.state.clone();
        async move {
            lmgw_core::runtime::lifecycle::readopt(
                &state,
                lmgw_core::runtime::registry::PassWait::Join,
                Duration::from_secs(30),
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!second.is_finished(), "it waits for the running pass");
    go.send_replace(true);
    assert!(first.await.unwrap());
    assert!(
        second.await.unwrap(),
        "told what the pass it waited for did"
    );
    drop(capturing);

    assert_eq!(f.world().verb("ps"), 1, "one listing, not two");
    let text = log.text();
    assert_eq!(
        text.matches("adopted a running container").count(),
        1,
        "{text}"
    );
    assert_eq!(f.state.runtime().list().len(), 1);
}

/// A container that does not answer yet may be loading: younger than
/// `vram.load_timeout_seconds`, it is left alone. Older, it is no load.
#[tokio::test]
async fn a_silent_container_is_left_to_load_and_removed_once_it_is_old() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.world().reports = true;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    let name = entry(&f, "chat-model").unwrap().container_name;
    forget_everything(&f);
    f.world().health_held = true;

    assert!(!pass(&f).await);
    assert!(f.stops().is_empty(), "a young silent container is loading");
    assert!(entry(&f, "chat-model").is_none());

    f.world().created.insert(name.clone(), unix_now() - 3600);
    assert!(pass(&f).await);
    assert_eq!(
        f.stops(),
        vec!["chat-model".to_string()],
        "an hour old and still silent: removed"
    );
}

#[tokio::test]
async fn a_container_that_cannot_be_removed_is_retried_with_a_backoff() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.world().reports = true;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    let name = entry(&f, "chat-model").unwrap().container_name;
    forget_everything(&f);
    edit_chat_model(&f, |p| p.ctx_size = Some(4096)).await;
    *f.podman.fail_stop.lock().unwrap() = true;
    *f.podman.fail_rm.lock().unwrap() = true;

    let (log, capturing) = crate::common::captured_log::capture_log();
    assert!(!pass(&f).await, "the removal failed");
    let tried = touched(&f, &name).len();
    assert!(!pass(&f).await);
    assert!(!pass(&f).await);
    drop(capturing);

    assert_eq!(
        touched(&f, &name).len(),
        tried,
        "backing off: not tried again on the next passes"
    );
    let text = log.text();
    assert_eq!(
        text.lines()
            .filter(|l| l.contains("WARN") && l.contains("removing it failed"))
            .count(),
        1,
        "said once: {text}"
    );
    assert!(entry(&f, "chat-model").is_none(), "no entry left behind");
}

/// A stop that lands on a removal's `stopping` entry reads the entry's stop
/// timeout — `vram.unload_timeout_seconds`, not zero, which timed its `podman
/// wait` out before it began.
#[tokio::test]
async fn a_stop_during_a_removal_waits_the_unload_timeout() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.world().reports = true;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    forget_everything(&f);
    edit_chat_model(&f, |p| p.ctx_size = Some(4096)).await;
    let stops = f.podman.hold_stops();
    let waits = f.podman.hold_waits();

    let removing = tokio::spawn({
        let state = f.state.clone();
        async move {
            lmgw_core::runtime::lifecycle::readopt(
                &state,
                lmgw_core::runtime::registry::PassWait::Join,
                Duration::from_secs(30),
            )
            .await
        }
    });
    until("the removal's stopping entry", || {
        entry(&f, "chat-model").is_some_and(|v| v.state == RuntimeState::Stopping)
    })
    .await;
    let stopping = tokio::spawn({
        let reg = f.state.runtime();
        async move { reg.stop(Class::Chat, "chat-model", false).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    stops.send_replace(true);
    tokio::time::sleep(Duration::from_millis(100)).await;
    waits.send_replace(true);

    stopping
        .await
        .unwrap()
        .expect("the stop waited for the container, within the unload timeout");
    assert!(removing.await.unwrap());
    assert!(entry(&f, "chat-model").is_none());
}
