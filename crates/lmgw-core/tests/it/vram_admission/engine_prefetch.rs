//! The pre-fetch (machine-check design §5, D16's second half, D72, D73,
//! WP11): once the machine check turns `ready` — at boot or later — the
//! engine image of each class with a model on the graphics card, when it is
//! not on this machine, begins downloading through the `image_pull` job a
//! start would use — once per turn to `ready`, nothing started, nothing
//! waited for, nothing on a metered connection — and a first request joins
//! that job.
//!
//! The admission fixture (two rows, a chat and an embedding model), its
//! podman answering the check as a working one does, the machine stated by
//! `support/machine_world.rs`, Podman's API the scripted recording of
//! [`super::engine_download`] and the registry its mock.

use lmgw_api_types::machine::MachineVerdict;
use lmgw_core::machine::Prefetched;
use lmgw_machine::CmdOut;

use super::engine_download::{engines, image_pulls, registry, scripted, ENGINE};
use super::*;
use crate::support::machine_world::Machine;

/// The chat class's image, which the fixture's podman has.
const HERE: &str = "ghcr.io/ggml-org/llama.cpp:server-cuda";

async fn check(f: &Fixture) -> MachineVerdict {
    lmgw_core::machine::check(&f.state, true).await.verdict
}

/// Ready: the missing image begins downloading, once, and nothing starts;
/// another ready check begins nothing; the first request then joins the
/// same job instead of pulling again.
#[tokio::test]
async fn ready_begins_the_missing_image_once_and_a_start_joins_it() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let machine = Machine::nvidia();
    machine.state_on(&f.state);
    let _reg = registry(&f).await;
    let (api, release) = scripted(&f, None);
    engines(&f, ENGINE, HERE).await;

    assert_eq!(check(&f).await, MachineVerdict::Ready);
    let done = f.state.machine.prefetched().await.expect("it began");
    let pulls = image_pulls(&f);
    assert_eq!(pulls.len(), 1);
    assert_eq!(
        done,
        [Prefetched::Pulling {
            image: ENGINE.into(),
            job: pulls[0].id
        }]
    );
    assert_eq!(
        pulls[0].key.as_deref(),
        Some(format!("image:{ENGINE}").as_str())
    );
    assert!(f.runs().is_empty(), "nothing started");
    assert_eq!(vram_status(&f.gateway).await["downloading"], json!([]));

    assert_eq!(check(&f).await, MachineVerdict::Ready);
    assert_eq!(
        f.state.machine.prefetched().await,
        None,
        "ready again: none"
    );

    let gw = f.gateway.clone();
    let req = tokio::spawn(async move { chat(&gw).await.status() });
    until_vram(
        &f,
        "the request waiting for the pre-fetch's download",
        |v| v["downloading"][0]["model"] == "chat-model",
    )
    .await;
    assert_eq!(image_pulls(&f).len(), 1, "one job");
    release.send(true).unwrap();
    assert_eq!(req.await.unwrap(), 200);
    assert_eq!(api.calls(), vec![ENGINE.to_string()], "one pull");
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
}

/// Not ready (a restart is due): nothing is fetched; once it turns ready,
/// it is.
#[tokio::test]
async fn not_ready_fetches_nothing_until_it_turns_ready() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let machine = Machine::nvidia_at("600.10.01");
    machine.m.facts.set_driver_libraries("600.20.03");
    machine.state_on(&f.state);
    let _reg = registry(&f).await;
    let (api, _release) = scripted(&f, None);
    engines(&f, ENGINE, HERE).await;

    assert_eq!(check(&f).await, MachineVerdict::NeedsRestart);
    assert_eq!(f.state.machine.prefetched().await, None);
    assert!(image_pulls(&f).is_empty());
    assert!(api.calls().is_empty());

    machine
        .m
        .facts
        .set_kernel_module("600.20.03")
        .set_boot_id("0f6c1e9a-5b7d-4e2a-9c3f-000000000002");
    assert_eq!(check(&f).await, MachineVerdict::Ready);
    let done = f.state.machine.prefetched().await.expect("it began");
    assert!(
        matches!(&done[..], [Prefetched::Pulling { image, .. }] if image == ENGINE),
        "{done:?}"
    );
    assert_eq!(image_pulls(&f).len(), 1);
}

/// Every image here: nothing to fetch, and podman asked only whether they
/// are.
#[tokio::test]
async fn images_that_are_here_begin_nothing() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let machine = Machine::nvidia();
    machine.state_on(&f.state);
    let (api, _release) = scripted(&f, None);

    assert_eq!(check(&f).await, MachineVerdict::Ready);
    assert_eq!(f.state.machine.prefetched().await, Some(vec![]));
    assert!(image_pulls(&f).is_empty());
    assert!(api.calls().is_empty());
}

/// A `localhost/` build is never pulled, and says why; a disabled row's
/// missing image is not fetched at all.
#[tokio::test]
async fn a_local_build_and_a_disabled_rows_image_are_not_fetched() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let machine = Machine::nvidia();
    machine.state_on(&f.state);
    let _reg = registry(&f).await;
    let (api, _release) = scripted(&f, None);
    let local = "localhost/lmgw-test/engine:cuda";
    engines(&f, local, ENGINE).await;
    f.world().missing_images.insert(local.into());
    sqlx::query("UPDATE aux_models SET enabled = 0 WHERE model_id = 'embed-model'")
        .execute(&f.state.db)
        .await
        .unwrap();
    f.state.reload_snapshot().await.unwrap();

    assert_eq!(check(&f).await, MachineVerdict::Ready);
    let done = f.state.machine.prefetched().await.expect("it began");
    let [Prefetched::Left { image, why }] = &done[..] else {
        panic!("{done:?}")
    };
    assert_eq!(image, local);
    assert!(why.contains("built on this machine"), "{why}");
    assert!(image_pulls(&f).is_empty());
    assert!(api.calls().is_empty());
}

/// The boot run's result turning `ready` begins it as a later check's does.
#[tokio::test]
async fn the_boot_runs_ready_begins_it() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let machine = Machine::nvidia();
    machine.state_on(&f.state);
    let _reg = registry(&f).await;
    let (_api, _release) = scripted(&f, None);
    engines(&f, ENGINE, HERE).await;

    lmgw_core::machine::boot(&f.state).await;
    assert_eq!(
        f.state.machine.summary().unwrap().verdict,
        MachineVerdict::Ready
    );
    let done = f.state.machine.prefetched().await.expect("it began");
    assert!(
        matches!(&done[..], [Prefetched::Pulling { image, .. }] if image == ENGINE),
        "{done:?}"
    );
    assert!(f.runs().is_empty(), "nothing started");
}

/// `ready`, another verdict, `ready` again: it begins at each turn to
/// `ready`, not at the one in between.
#[tokio::test]
async fn ready_after_another_verdict_begins_it_again() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let machine = Machine::nvidia_at("600.10.01");
    machine.state_on(&f.state);
    let (api, _release) = scripted(&f, None);

    assert_eq!(check(&f).await, MachineVerdict::Ready);
    assert_eq!(f.state.machine.prefetched().await, Some(vec![]));
    machine.m.facts.set_driver_libraries("600.20.03");
    assert_eq!(check(&f).await, MachineVerdict::NeedsRestart);
    assert_eq!(f.state.machine.prefetched().await, None);
    machine
        .m
        .facts
        .set_kernel_module("600.20.03")
        .set_boot_id("0f6c1e9a-5b7d-4e2a-9c3f-000000000002");
    assert_eq!(check(&f).await, MachineVerdict::Ready);
    assert_eq!(
        f.state.machine.prefetched().await,
        Some(vec![]),
        "begun again"
    );
    assert!(api.calls().is_empty());
}

/// A class whose rows are all on the processor gets no pre-fetch: its
/// engine comes with its first start (D72, D16's scope).
#[tokio::test]
async fn a_class_with_rows_only_on_the_processor_is_not_fetched() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let machine = Machine::nvidia();
    machine.state_on(&f.state);
    let _reg = registry(&f).await;
    let (api, _release) = scripted(&f, None);
    engines(&f, HERE, ENGINE).await;
    sqlx::query("UPDATE aux_models SET backend = 'cpu' WHERE model_id = 'embed-model'")
        .execute(&f.state.db)
        .await
        .unwrap();
    f.state.reload_snapshot().await.unwrap();

    assert_eq!(check(&f).await, MachineVerdict::Ready);
    assert_eq!(f.state.machine.prefetched().await, Some(vec![]));
    assert!(image_pulls(&f).is_empty());
    assert!(api.calls().is_empty());
}

/// NetworkManager says the default route's device is metered: nothing is
/// fetched, and the result says so.
#[tokio::test]
async fn a_metered_connection_fetches_nothing() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let machine = Machine::nvidia();
    machine.m.facts.file(
        "/proc/net/route",
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
         wwan0\t00000000\t010200C0\t0003\t0\t0\t700\t00000000\t0\t0\t0\n",
    );
    machine.m.facts.program("/usr/bin/nmcli", |args| {
        assert_eq!(args.last().map(String::as_str), Some("wwan0"));
        Ok(CmdOut::success("GENERAL.METERED:yes\n"))
    });
    machine.state_on(&f.state);
    let _reg = registry(&f).await;
    let (api, _release) = scripted(&f, None);
    engines(&f, ENGINE, HERE).await;

    assert_eq!(check(&f).await, MachineVerdict::Ready);
    assert_eq!(
        f.state.machine.prefetched().await,
        Some(vec![Prefetched::Metered {
            how: "wwan0: yes".into()
        }])
    );
    assert!(image_pulls(&f).is_empty());
    assert!(api.calls().is_empty());
}
