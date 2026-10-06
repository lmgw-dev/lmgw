//! A start or a climb whose entry was taken from it, and a container removed
//! from under a start (`docs/design/2026-10-06-registry-owns-start.md`).

use super::*;

/// A stop wins against a climb, and a fresh start of the same model claims
/// the name before the climb's new rung settles. The climb must not remove the
/// container by name: it is the fresh start's now — the same check a start's
/// own abort makes (`Registry::holds_name`).
#[tokio::test]
async fn an_aborted_climb_leaves_a_name_another_entry_holds() {
    let old = healthy().await;
    let new = healthy().await;
    let fresh = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(
        fake.clone(),
        vec![
            old.address().port(),
            new.address().port(),
            fresh.address().port(),
        ],
    );
    let (base, top) = (ladder_runtime("lad", 0), ladder_runtime("lad", 1));
    let claim = reg.acquire(&spec(&base, 5_000)).await.unwrap();

    let mut climb = ticket(reg.mark_climb(&claim, TOP, "why"));
    climb.drain(None).await.unwrap();
    let (open, gate) = watch::channel(false);
    *fake.gate.lock().unwrap() = Some(gate);
    let run = climb.start(StartSpec::of(&spec(&top, 5_000))).unwrap();
    wait_for("the new rung's podman run", || fake.runs().len() == 2).await;

    reg.stop(Class::Chat, "lad", true).await.unwrap();
    drop(claim);
    let restarting = tokio::spawn({
        let reg = reg.clone();
        let base = base.clone();
        async move { reg.acquire(&spec(&base, 5_000)).await.map(|g| g.port()) }
    });
    wait_for("the fresh start's podman run", || fake.runs().len() == 3).await;
    open.send_replace(true);

    assert!(matches!(
        run.finish().await,
        Err(RuntimeError::Aborted { .. })
    ));
    assert_eq!(restarting.await.unwrap().unwrap(), fresh.address().port());
    let name = container_name("lmgw", Class::Chat, "lad");
    assert!(
        !fake
            .calls()
            .iter()
            .any(|c| c[0] == "rm" && c.last() == Some(&name)),
        "the aborted climb removed the fresh start's container: {:?}",
        fake.calls()
    );
    assert_eq!(reg.list().len(), 1);
    assert_eq!(reg.list()[0].state, RuntimeState::Ready);
}

/// A container removed while its model loads — by a shell, or by a pass that
/// judged it too early — answers "no such container" to the state check. That
/// is a dead start, said at once, not a load that waits out its whole budget.
#[tokio::test]
async fn a_container_removed_while_loading_fails_the_start_at_once() {
    let silent = MockServer::start().await;
    let fake = Arc::new(Fake::default());
    *fake.inspect_gone.lock().unwrap() = true;
    let reg = registry(fake.clone(), vec![silent.address().port()]);

    let rt = runtime(Class::Chat, "gone");
    let started = std::time::Instant::now();
    let err = reg.acquire(&spec(&rt, 120_000)).await.unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the start waited {:?} for a container that was gone",
        started.elapsed()
    );
    let msg = err.to_string();
    assert!(msg.contains("removed while the model was loading"), "{msg}");
    assert!(reg.list().is_empty());
}
