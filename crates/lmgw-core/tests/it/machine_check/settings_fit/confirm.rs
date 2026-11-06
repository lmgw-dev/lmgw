//! The confirmation's contract (D135) and the running models (D136): a
//! confirmed list is written only while it is the switch the settings give
//! then; a class edited after the offer, or a row edited in the store
//! under the snapshot, writes nothing and answers the list as it is now;
//! a settings save refreshes the offer the cached check carries; the
//! running models a switch concerns are stopped, and one answering a
//! request is named and keeps the check from saying ready until it stops.

use lmgw_api_types::machine::{
    ItemState, MachineAction, MachineCause, MachineItemId, MachineVerdict, SettingsFitRunning,
    SettingsFitStop,
};
use lmgw_core::runtime::Class;
use lmgw_core::store;

use super::{
    a, answer, chat, every_image_here, item, offered, switch, v, AMD_ARGS, CUDA, NVIDIA_ARGS,
};
use crate::common::serve;
use crate::support::machine_world::World;

/// The owner adds an argument of their own to the chat class after the
/// offer: confirming the old list writes nothing and answers what differs
/// and the list as it is now, which keeps that argument (D138); the cached
/// check already offers that list (the save refreshed it), and confirming
/// it writes it.
#[tokio::test]
async fn a_class_edited_after_the_offer_writes_nothing_and_answers_the_new_list() {
    let w = World::amd().await;
    every_image_here(&w);
    let old = offered(&w.check().await).1;
    let gw = serve(w.state.clone()).await;
    let runs = w.state.machine.runs();
    w.settings(|s| {
        s.router
            .extra_run_args
            .extend(v(&["--shm-size", "16g", "-v", "/data:/data"]))
    })
    .await;
    let mine: Vec<&str> = NVIDIA_ARGS
        .iter()
        .copied()
        .chain(["--shm-size", "16g", "-v", "/data:/data"])
        .collect();
    let theirs: Vec<&str> = AMD_ARGS
        .iter()
        .copied()
        .chain(["--shm-size", "16g", "-v", "/data:/data"])
        .collect();

    // The save judged §1.4 again from the last run's reading, without
    // reading the machine.
    let cached = offered(&w.state.machine.last().unwrap()).1;
    assert_eq!(w.state.machine.runs(), runs, "no new run");
    assert_ne!(cached, old);

    let refused = answer(switch(&gw, "amd_vulkan", Some(&old)).await);
    assert!(!refused.applied, "{refused:#?}");
    assert!(refused.check.is_none());
    assert_eq!(
        refused.sentence,
        "The model settings changed after this list was made, so nothing was changed. The \
         list here is the switch as it is now: confirm it to switch."
    );
    let outdated = refused.outdated.as_ref().expect("what differs");
    let field = "Settings → Runtimes → Chat → Extra podman run args";
    assert_eq!(outdated.withdrawn.len(), 1, "{outdated:#?}");
    assert_eq!(outdated.withdrawn[0].field, field);
    assert_eq!(outdated.withdrawn[0].from, a(&NVIDIA_ARGS));
    assert_eq!(outdated.added.len(), 1, "{outdated:#?}");
    assert_eq!(
        (&outdated.added[0].from, &outdated.added[0].to),
        (&a(&mine), &a(&theirs)),
        "the owner's arguments stay"
    );
    assert!(outdated.models.is_empty());
    assert_eq!(refused.changes, cached, "the cached offer is the list now");
    let s = w.state.snapshot().settings.clone();
    assert_eq!(
        (s.router.image.as_str(), &s.router.extra_run_args),
        (CUDA, &v(&mine)),
        "nothing written"
    );

    let done = answer(switch(&gw, "amd_vulkan", Some(&refused.changes)).await);
    assert!(done.applied, "{done:#?}");
    assert_eq!(
        w.state.snapshot().settings.router.extra_run_args,
        v(&theirs)
    );
}

/// A row edited in the store after the snapshot the switch was worked out
/// from: its write refuses the whole save, the classes included, and the
/// answer names the model.
#[tokio::test]
async fn a_row_that_changed_under_the_switch_refuses_all_of_it() {
    let w = World::amd().await;
    every_image_here(&w);
    let shm: Vec<&str> = NVIDIA_ARGS
        .iter()
        .copied()
        .chain(["--shm-size", "8g"])
        .collect();
    store::insert_local_model(&w.state.db, &chat("shm", None, Some(&shm), true))
        .await
        .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let changes = offered(&w.check().await).1;
    assert!(
        changes.iter().any(|c| c.model.as_deref() == Some("shm")),
        "{changes:#?}"
    );
    // Written behind the snapshot's back: the switch still sees the old
    // row, the store holds the new one.
    let edited = r#"["--device","nvidia.com/gpu=all","--shm-size","1g"]"#;
    sqlx::query("UPDATE local_models SET extra_run_args = ?1 WHERE model_id = 'shm'")
        .bind(edited)
        .execute(&w.state.db)
        .await
        .unwrap();

    let gw = serve(w.state.clone()).await;
    let refused = answer(switch(&gw, "amd_vulkan", Some(&changes)).await);
    assert!(!refused.applied, "{refused:#?}");
    assert_eq!(
        refused.outdated.as_ref().map(|o| o.models.clone()),
        Some(vec!["shm".to_string()])
    );
    assert!(
        refused.sentence.starts_with(
            "Model 'shm' changed while the settings were being switched, so nothing \
                          was changed."
        ),
        "{}",
        refused.sentence
    );
    // The store, read again: neither the classes nor the row moved.
    w.state.reload_snapshot().await.unwrap();
    let snap = w.state.snapshot();
    assert_eq!(snap.settings.router.image, CUDA);
    assert_eq!(snap.settings.router.extra_run_args, v(&NVIDIA_ARGS));
    assert_eq!(snap.settings.audio.backend, "cuda");
    let row = snap
        .local_models
        .iter()
        .find(|m| m.model_id == "shm")
        .unwrap();
    assert_eq!(
        row.extra_run_args,
        Some(v(&["--device", "nvidia.com/gpu=all", "--shm-size", "1g"]))
    );
}

/// Two chat models run on the class's settings, one answering requests,
/// and one runs on its own: the offer lists the two the switch concerns;
/// confirmed, the idle one is stopped, the busy one is named with what to
/// do, and the check after it says it still runs with the old settings
/// (not ready) until it stops.
#[tokio::test]
async fn running_models_are_stopped_and_a_busy_one_keeps_the_check_from_ready() {
    let w = World::amd().await;
    every_image_here(&w);
    for row in [
        chat("idle", None, None, true),
        chat("busy", None, None, true),
        chat(
            "own",
            Some("ghcr.io/ggml-org/llama.cpp:server-vulkan"),
            Some(&AMD_ARGS),
            true,
        ),
    ] {
        store::insert_local_model(&w.state.db, &row).await.unwrap();
    }
    w.state.reload_snapshot().await.unwrap();
    let registry = w.state.runtime();
    registry.ready_for_tests(Class::Chat, "idle", 0);
    registry.ready_for_tests(Class::Chat, "busy", 2);
    registry.ready_for_tests(Class::Chat, "own", 0);

    let c = w.check().await;
    let Some(MachineAction::SettingsFit {
        changes, running, ..
    }) = &item(&c, MachineItemId::SettingsFit).action
    else {
        panic!("{c:#?}")
    };
    let offer = |m: &str| SettingsFitRunning {
        class: "chat".into(),
        model: m.into(),
        stop: None,
    };
    assert_eq!(running, &[offer("busy"), offer("idle")], "not 'own'");

    let gw = serve(w.state.clone()).await;
    let preview = answer(switch(&gw, "amd_vulkan", None).await);
    assert_eq!(&preview.running, running);
    assert!(
        preview.sentence.contains(
            "Once it has switched, it stops 'busy' and 'idle' so they start again with the new \
             settings, unless one is answering a request then."
        ),
        "{}",
        preview.sentence
    );

    let done = answer(switch(&gw, "amd_vulkan", Some(changes)).await);
    assert!(done.applied, "{done:#?}");
    let stop = |m: &str| {
        done.running
            .iter()
            .find(|r| r.model == m)
            .and_then(|r| r.stop.clone())
    };
    assert_eq!(stop("idle"), Some(SettingsFitStop::Stopped));
    assert_eq!(stop("busy"), Some(SettingsFitStop::Busy { in_flight: 2 }));
    assert_eq!(stop("own"), None, "its settings did not change");
    assert!(!registry.contains(Class::Chat, "idle"));
    assert!(registry.contains(Class::Chat, "busy"));
    assert!(registry.contains(Class::Chat, "own"), "left running");
    assert_eq!(
        done.sentence,
        "lmgw now uses the settings for AMD cards. 'idle' was stopped and starts with the new \
         settings at the next request. 'busy' is answering requests, so it keeps the old \
         settings until it stops: stop it on the Overview once it is done."
    );

    let after = done.check.expect("a check after the write");
    let fit = item(&after, MachineItemId::SettingsFit);
    assert_eq!(fit.state, ItemState::Problem, "{fit:#?}");
    assert_eq!(
        fit.cause,
        Some(MachineCause::RunningOldSettings {
            models: vec!["busy".into()]
        })
    );
    assert_eq!(
        fit.sentence,
        "Some models still run with the settings from before the switch (busy). They use the \
         new settings once they stop."
    );
    assert_eq!(after.verdict, MachineVerdict::NeedsSteps);
    assert_eq!(after.headline, fit.sentence);

    // Once that container is gone, the settings fit and the machine is
    // ready.
    registry.stop(Class::Chat, "busy", true).await.unwrap();
    let c = w.check().await;
    assert_eq!(
        item(&c, MachineItemId::SettingsFit).state,
        ItemState::Ok,
        "{c:#?}"
    );
    assert_eq!(c.verdict, MachineVerdict::Ready);
}
