//! The build-run executor and the images module (container-builds design §5,
//! §14, §15) end to end, on the shared fixtures of `support/backends_fake.rs`:
//! real git against local fixture repositories, and a fake podman that is both
//! the registry's `CommandRunner` and the agent `Spawner`. Nothing here touches
//! the real podman, the real build lock or `/var/tmp`.

use crate::support::backends_fake as support;

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use lmgw_api_types::builds::{BuildCheckMergeArgs, ImageUseKind, MergeOutcome};
use lmgw_core::agents::container::Signal;
use lmgw_core::backends::run::{self, LOG_CHUNK_BYTES};
use lmgw_core::backends::validate::validate_build;
use lmgw_core::backends::{
    images, paths, BuildExtra, BuildRunPatch, BuildRunStatus, BuildTrigger, Engine, Forge,
    ForgeLookup, ForgePr, NewBuildRun,
};
use lmgw_core::jobs;
use lmgw_core::store;
use support::*;

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_run_builds_verifies_and_promotes_with_every_phase_logged() {
    let h = Harness::new().await;
    let spec = h.spec();
    let build_id = h.build(&spec).await;
    h.podman.set_behavior(Behavior::HoldThenSucceed);

    let started = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();

    // Progress reaches the job detail as the build prints it.
    let detail = wait_detail(&h.state, started.job_id, |d| {
        d["phase"] == "build" && d["percent"] == 50
    })
    .await;
    assert_eq!(detail["run_id"], started.run_id);
    assert_eq!(detail["build_id"], build_id);
    assert_eq!(detail["step"], "1/3", "{detail}");
    assert!(
        detail["last_line"].as_str().unwrap().contains("[20/40]")
            || detail["last_line"].as_str().unwrap().contains("stderr"),
        "{detail}"
    );
    let live = h.state.jobs.live_one(started.job_id).unwrap();
    assert_eq!(live.kind, "build_run");
    assert_eq!(live.key.as_deref(), Some(&*format!("build:{build_id}")));

    // One live run per build.
    let again = run::start_run(&h.state, build_id, BuildTrigger::Manual, false).await;
    assert!(
        again.unwrap_err().contains("already has a run in progress"),
        "a second run of the same build is refused"
    );

    h.podman.0.release.notify_one();
    let r = wait_run(&h.state, started.run_id).await;
    assert_eq!(r.status, BuildRunStatus::Succeeded, "{:?}", r.error);
    assert!(r.promoted);
    let base = h.base_sha();
    assert_eq!(r.base_sha.as_deref(), Some(base.as_str()));
    let cfg = r.cfg_hash.clone().unwrap();
    let immutable = format!(
        "localhost/lmgw-llama-server:official-master-{}-{}",
        &base[..7],
        &cfg[..6]
    );
    let moving = "localhost/lmgw-llama-server:official-master";
    assert_eq!(r.tags, vec![immutable.clone(), moving.to_string()]);
    let image = r.image_id.clone().unwrap();
    assert_eq!(r.size_bytes, Some(2_500_000_000));
    assert_eq!(r.verify.as_ref().unwrap()["help_ok"], true);
    assert_eq!(r.verify.as_ref().unwrap()["gpu_verified"], true);
    assert!(r.finished_at.is_some());
    let resolved = r.inputs.resolved.clone().unwrap();
    assert_eq!(resolved.dockerfile, ".devops/cuda.Dockerfile");
    assert_eq!(resolved.target, "server");
    assert_eq!(resolved.arch, vec!["89"]);
    assert_eq!(h.podman.world().id_of(moving), Some(image.clone()));

    // The build argv (§5 step 5).
    let (argv, containerfile, ignorefile) = {
        let spawns = h.podman.0.spawns.lock().unwrap();
        assert_eq!(spawns.len(), 1);
        (
            spawns[0].argv.clone(),
            spawns[0].containerfile.clone(),
            spawns[0].ignorefile.clone(),
        )
    };
    let argv = &argv;
    let instance = h.state.builds.instance_id().to_string();
    let ctx = paths::context_dir(&h.builds_dir(), &instance, r.id);
    let worktree = paths::worktree_dir(&h.builds_dir(), &instance, r.id);
    assert!(
        ctx.ends_with(format!("work/run-{instance}-{}.ctx", r.id)),
        "{ctx:?}"
    );
    assert_eq!(argv[0], "build");
    assert!(has_arg_pair(argv, "--target", "server"), "{argv:?}");
    assert!(has_arg_pair(
        argv,
        "-f",
        &ctx.join("Containerfile").display().to_string()
    ));
    assert!(has_arg_pair(
        argv,
        "--ignorefile",
        &ctx.join("ignorefile").display().to_string()
    ));
    assert!(has_arg_pair(
        argv,
        "--iidfile",
        &ctx.join("iid").display().to_string()
    ));
    assert!(argv.contains(&"--layers=false".to_string()));
    assert!(!argv.contains(&"--pull=newer".to_string()));
    for arg in [
        "CUDA_VERSION=13.0.0".to_string(),
        "CUDA_DOCKER_ARCH=89".to_string(),
        format!("APP_REVISION={base}"),
        "GGML_CUDA_FA_ALL_QUANTS=ON".to_string(),
    ] {
        assert!(has_arg_pair(argv, "--build-arg", &arg), "{arg} in {argv:?}");
    }
    for label in [
        format!("dev.lmgw.instance={instance}"),
        format!("dev.lmgw.run={}", r.id),
        format!("dev.lmgw.build={build_id}"),
        "dev.lmgw.engine=llama".to_string(),
        "dev.lmgw.slug=official-master".to_string(),
        format!("dev.lmgw.repo={}", spec.repo_url),
        "dev.lmgw.ref=master".to_string(),
        format!("dev.lmgw.base={base}"),
        "dev.lmgw.extras=[]".to_string(),
        "dev.lmgw.backend=cuda".to_string(),
        "dev.lmgw.arch=89".to_string(),
        "dev.lmgw.cuda=13.0.0".to_string(),
        format!("dev.lmgw.cfg={cfg}"),
        format!("org.opencontainers.image.revision={base}"),
    ] {
        assert!(has_arg_pair(argv, "--label", &label), "{label} in {argv:?}");
    }
    // Built under the run's own tag; the immutable one is earned by verify.
    assert!(has_arg_pair(argv, "-t", &format!("{immutable}-r{}", r.id)));
    assert!(!h
        .podman
        .world()
        .images
        .iter()
        .any(|i| i.names.iter().any(|n| n.ends_with(&format!("-r{}", r.id)))));
    // …in a TMPDIR of its own, removed afterwards; buildah's cache root is
    // linked in from where it lives, and survives.
    let scratch = paths::run_tmp_dir(&h.builds_dir(), &instance, r.id);
    let env = h.podman.0.spawns.lock().unwrap()[0].env.clone();
    assert_eq!(
        env,
        vec![("TMPDIR".to_string(), scratch.display().to_string())]
    );
    assert!(!scratch.exists(), "the scratch dir is removed");
    let uid = unsafe { libc::getuid() };
    assert!(h
        .root
        .join("vartmp")
        .join(format!("buildah-cache-{uid}"))
        .is_dir());
    assert_eq!(argv.last().unwrap(), &worktree.display().to_string());
    // The Containerfile is the edited copy; the ignore file is the preset's.
    assert!(
        containerfile.contains("--mount=type=cache,id=lmgw-llama-cuda,target=/ccache"),
        "the ccache edit is applied, its id rendered"
    );
    assert_eq!(ignorefile, lmgw_core::backends::presets::IGNORE_FILE);

    // The probes ran with the chat class's GPU args.
    let runs = h.podman.calls_of("run");
    assert!(runs
        .iter()
        .any(|c| c.contains(&"--list-devices".to_string())));
    assert!(runs
        .iter()
        .all(|c| has_arg_pair(c, "--device", "nvidia.com/gpu=all")));

    // The log: every phase, in order, and the build's own output.
    let log = whole_log(&h.state, r.id).await;
    let phases = phases_in(&log);
    let want = [
        "resolve", "fetch", "assemble", "prepare", "build", "verify", "promote", "cleanup",
    ];
    let pos: Vec<usize> = want
        .iter()
        .map(|p| {
            phases
                .iter()
                .position(|x| x == p)
                .unwrap_or_else(|| panic!("no {p} header in {phases:?}"))
        })
        .collect();
    assert!(pos.windows(2).all(|w| w[0] < w[1]), "{phases:?}");
    assert!(log.contains("[20/40] Building CUDA object"), "{log}");
    assert!(log.contains("a warning on stderr"));
    assert!(log.contains("edit edits[0]"), "each edit is logged");
    assert!(log.contains(&format!("tagged {moving}")), "{log}");
    assert!(
        log.contains(&format!("run {} ended: succeeded", r.id)),
        "{log}"
    );

    // Cleanup: no worktree, no context, and the job row closed `done`.
    assert!(!worktree.exists());
    assert!(!ctx.exists());
    assert_eq!(
        r.log_path.as_deref(),
        Some(
            &*paths::log_path(&h.builds_dir(), &instance, r.id)
                .display()
                .to_string()
        ),
        "the log carries the instance too"
    );
    let job = store::get_job(&h.state.db, started.job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.status, "done");
}

#[tokio::test]
async fn the_same_inputs_again_are_up_to_date_and_build_nothing() {
    let h = Harness::new().await;
    let build_id = h.build(&h.spec()).await;
    let first = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();
    let first = wait_run(&h.state, first.run_id).await;
    assert_eq!(first.status, BuildRunStatus::Succeeded, "{:?}", first.error);

    let second = run::start_run(&h.state, build_id, BuildTrigger::Mcp, false)
        .await
        .unwrap();
    let second = wait_run(&h.state, second.run_id).await;
    assert_eq!(
        second.status,
        BuildRunStatus::UpToDate,
        "{:?}",
        second.error
    );
    assert_eq!(second.image_id, first.image_id);
    assert_eq!(second.trigger, BuildTrigger::Mcp);
    assert_eq!(
        h.podman.spawn_count(),
        1,
        "nothing was built the second time"
    );
    let log = whole_log(&h.state, second.id).await;
    assert!(log.contains("up to date"), "{log}");
    assert!(!phases_in(&log).contains(&"assemble".to_string()));

    // Rebuild anyway builds again, pulling newer bases, and removes the
    // image it replaced under the same tag once that is unused.
    let third = run::start_run(&h.state, build_id, BuildTrigger::Manual, true)
        .await
        .unwrap();
    let third = wait_run(&h.state, third.run_id).await;
    assert_eq!(third.status, BuildRunStatus::Succeeded, "{:?}", third.error);
    assert_ne!(third.image_id, first.image_id);
    assert_eq!(h.podman.spawn_count(), 2);
    assert!(h.podman.0.spawns.lock().unwrap()[1]
        .argv
        .contains(&"--pull=newer".to_string()));
    let old = first.image_id.unwrap();
    assert_eq!(
        h.podman.world().find(&old),
        None,
        "the replaced image is gone"
    );
    assert!(whole_log(&h.state, third.id)
        .await
        .contains("removed the image this rebuild replaced"));
}

#[tokio::test]
async fn a_conflicting_extra_fails_the_run_with_the_report() {
    let h = Harness::new().await;
    // A fork branched off the base, changing a line master changes too.
    let fork = Repo::new(h.root.join("fork.git"), true);
    h.work.git(&["checkout", "--quiet", "-b", "feature"]);
    h.work.write("a.txt", "one\nFORK\nthree\n");
    h.work.commit("fork change");
    h.work.push(&fork, "refs/heads/feature:refs/heads/feature");
    h.work.git(&["checkout", "--quiet", "master"]);
    h.work.write("a.txt", "one\nMASTER\nthree\n");
    h.work.commit("master change");
    h.work
        .push(&h.upstream, "refs/heads/master:refs/heads/master");

    let mut spec = h.spec();
    spec.extras = vec![BuildExtra::Ref {
        remote_url: fork.url(),
        git_ref: "feature".into(),
        pin: None,
    }];
    let build_id = h.build(&validate_build(spec.clone()).unwrap()).await;
    let started = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();
    let r = wait_run(&h.state, started.run_id).await;
    assert_eq!(r.status, BuildRunStatus::Failed);
    let err = r.error.unwrap();
    assert!(err.contains("does not merge cleanly"), "{err}");
    assert!(err.contains("a.txt"), "{err}");
    assert_eq!(h.podman.spawn_count(), 0);
    let log = whole_log(&h.state, r.id).await;
    assert!(log.contains("conflicts in a.txt"), "{log}");
    assert!(!paths::worktree_dir(&h.builds_dir(), h.state.builds.instance_id(), r.id).exists());
    let job = store::get_job(&h.state.db, started.job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.status, "failed");

    // Check merge says the same without a run.
    let report = run::check_merge(
        &h.state,
        BuildCheckMergeArgs {
            id: Some(build_id),
            spec: None,
        },
    )
    .await
    .unwrap();
    assert!(!report.ok);
    assert_eq!(report.steps.len(), 1);
    assert_eq!(report.steps[0].outcome, MergeOutcome::Conflict);
    assert_eq!(report.steps[0].files, vec!["a.txt"]);
}

#[tokio::test]
async fn cancel_sends_sigterm_and_sweeps_only_the_new_leftovers() {
    let h = Harness::new().await;
    let build_id = h.build(&h.spec()).await;
    h.podman.set_behavior(Behavior::HoldUntilSignal);
    let started = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();
    wait_detail(&h.state, started.job_id, |d| d["phase"] == "build").await;
    jobs::cancel(&h.state, started.job_id).await.unwrap();
    let r = wait_run(&h.state, started.run_id).await;
    assert_eq!(r.status, BuildRunStatus::Canceled);
    assert_eq!(*h.podman.0.signals.lock().unwrap(), vec![Signal::Term]);

    // Only the working container that appeared during the build goes: not
    // the one that was there before, not the exited one, and not the model
    // container `podman run --replace` recreated while it ran.
    let rm = h.podman.calls_of("rm");
    assert_eq!(rm, vec![vec!["rm".to_string(), "-f".into(), "new1".into()]]);
    let left: Vec<String> = h
        .podman
        .world()
        .external
        .iter()
        .map(|c| c.id.clone())
        .collect();
    assert_eq!(left, ["old1", "exited1", "model2"]);
    // Its scratch dir was the run's TMPDIR, removed whole; /var/tmp is
    // untouched.
    let vartmp = h.root.join("vartmp");
    let scratch = paths::run_tmp_dir(&h.builds_dir(), h.state.builds.instance_id(), r.id);
    assert!(!scratch.exists());
    assert!(
        vartmp.join("buildah111").exists(),
        "another build's scratch dir stays"
    );
    let log = whole_log(&h.state, r.id).await;
    assert!(log.contains("SIGTERM"), "{log}");
    assert!(
        log.contains("cuda-working-container"),
        "the removal is logged"
    );
    let job = store::get_job(&h.state.db, started.job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.status, "canceled");
}

/// A cancel that races the build's successful end: the image it still
/// produced carries only the run's own tag and no run owns it — removed.
#[tokio::test]
async fn an_image_finished_as_the_cancel_went_out_is_removed() {
    let h = Harness::new().await;
    let build_id = h.build(&h.spec()).await;
    h.podman.set_behavior(Behavior::HoldThenFinishOnSignal);
    let started = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();
    wait_detail(&h.state, started.job_id, |d| d["phase"] == "build").await;
    jobs::cancel(&h.state, started.job_id).await.unwrap();
    let r = wait_run(&h.state, started.run_id).await;
    assert_eq!(r.status, BuildRunStatus::Canceled);
    assert!(
        h.podman.world().images.is_empty(),
        "{:?}",
        h.podman.world().images
    );
    let rmi = h.podman.calls_of("rmi");
    assert_eq!(rmi.len(), 1, "{rmi:?}");
    assert!(rmi[0][1].ends_with(&format!("-r{}", r.id)), "{rmi:?}");
    assert!(whole_log(&h.state, r.id)
        .await
        .contains("removed the image the build finished as it was canceled"));
}

/// A broken **Rebuild anyway** never takes the immutable tag from the
/// verified image it names — a model pinned to that tag keeps the good image
/// — and keeps its own `-r<run>` tag, which retention and delete handle like
/// any run's tag.
#[tokio::test]
async fn a_broken_rebuild_keeps_the_immutable_tag_on_the_verified_image() {
    let h = Harness::new().await;
    let build_id = h.build(&h.spec()).await;
    let first = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();
    let first = wait_run(&h.state, first.run_id).await;
    assert_eq!(first.status, BuildRunStatus::Succeeded, "{:?}", first.error);
    let immutable = first.tags[0].clone();
    let good = first.image_id.clone().unwrap();

    h.podman.world().devices_out = "Available devices:\n".into();
    let second = run::start_run(&h.state, build_id, BuildTrigger::Manual, true)
        .await
        .unwrap();
    let second = wait_run(&h.state, second.run_id).await;
    assert_eq!(second.status, BuildRunStatus::Broken);
    let bad = second.image_id.clone().unwrap();
    assert_ne!(bad, good);
    let own = format!("{immutable}-r{}", second.id);
    assert_eq!(second.tags, vec![own.clone()]);
    assert_eq!(h.podman.world().id_of(&immutable), Some(good.clone()));
    assert_eq!(h.podman.world().id_of(&own), Some(bad.clone()));
    assert!(whole_log(&h.state, second.id)
        .await
        .contains(&format!("kept as {own}")));

    // Fixed and rebuilt: the verified image takes the immutable tag, and the
    // good image it replaced goes once unused (nothing tags it any more).
    h.podman.world().devices_out = "  CUDA0: NVIDIA GeForce RTX 4090\n".into();
    let third = run::start_run(&h.state, build_id, BuildTrigger::Manual, true)
        .await
        .unwrap();
    let third = wait_run(&h.state, third.run_id).await;
    assert_eq!(third.status, BuildRunStatus::Succeeded, "{:?}", third.error);
    assert_eq!(h.podman.world().id_of(&immutable), third.image_id);
    assert_eq!(h.podman.world().find(&good), None, "replaced and unused");
    assert_eq!(
        h.podman.world().id_of(&own),
        Some(bad),
        "the broken run keeps its own tag (keep_runs decides about it)"
    );

    // The first image of a set of inputs takes the immutable tag even
    // unverified: nothing better holds it.
    let h2 = Harness::new().await;
    h2.podman.world().devices_out = "Available devices:\n".into();
    let b2 = h2.build(&h2.spec()).await;
    let r2 = run::start_run(&h2.state, b2, BuildTrigger::Manual, false)
        .await
        .unwrap();
    let r2 = wait_run(&h2.state, r2.run_id).await;
    assert_eq!(r2.status, BuildRunStatus::Broken);
    assert!(!r2.tags[0].contains("-r"), "{:?}", r2.tags);
    assert_eq!(h2.podman.world().id_of(&r2.tags[0]), r2.image_id);
}

/// A moving-tag move that fails after a good verify: the run is what it is —
/// built and verified — with the error recorded, and Make current retries.
#[tokio::test]
async fn a_failed_promote_leaves_a_succeeded_run_make_current_can_finish() {
    let h = Harness::new().await;
    let moving = "localhost/lmgw-llama-server:official-master".to_string();
    h.podman.world().fail_tag.push(moving.clone());
    let build_id = h.build(&h.spec()).await;
    let started = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();
    let r = wait_run(&h.state, started.run_id).await;
    assert_eq!(r.status, BuildRunStatus::Succeeded);
    assert!(!r.promoted);
    let error = r.error.clone().unwrap();
    assert!(error.contains("Make current retries it"), "{error}");
    let job = store::get_job(&h.state.db, started.job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.status, "done");

    h.podman.world().fail_tag.clear();
    let promoted = run::promote_run(&h.state, r.id).await.unwrap();
    assert_eq!(promoted.moving_tag, moving);
    assert_eq!(h.podman.world().id_of(&moving), r.image_id);
    let r = store::get_build_run(&h.state.db, r.id)
        .await
        .unwrap()
        .unwrap();
    assert!(r.promoted);
}

/// A dev instance builds into its own namespace and never names a
/// production tag.
#[tokio::test]
async fn a_dev_instance_tags_only_into_the_dev_namespace() {
    let h = Harness::new().await;
    h.state.set_dev_for_tests(true);
    let build_id = h.build(&h.spec()).await;
    let started = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();
    let r = wait_run(&h.state, started.run_id).await;
    assert_eq!(r.status, BuildRunStatus::Succeeded, "{:?}", r.error);
    assert!(r.promoted);
    assert_eq!(r.tags.len(), 2);
    for t in &r.tags {
        assert!(t.starts_with("localhost/lmgw-dev-llama-server:"), "{t}");
    }
    for call in h.podman.calls_of("tag") {
        assert!(call[2].starts_with("localhost/lmgw-dev-"), "{call:?}");
    }
    assert_eq!(
        h.podman
            .world()
            .id_of("localhost/lmgw-llama-server:official-master"),
        None
    );
}

/// A forge that never answers.
struct SilentForge;

#[async_trait]
impl ForgeLookup for SilentForge {
    async fn pr(&self, _repo_url: &str, _forge: Forge, _number: u64) -> Result<ForgePr, String> {
        std::future::pending().await
    }
}

/// Cancel reaches a run stuck before the build — here in the forge lookup of
/// its resolve phase, the same select that stops a long fetch or merge.
#[tokio::test]
async fn cancel_stops_a_run_before_the_build_too() {
    let h = Harness::new().await;
    h.state.builds.set_forge(Arc::new(SilentForge));
    let mut spec = h.spec();
    spec.forge = Forge::Github;
    spec.extras = vec![BuildExtra::Pr {
        number: 1,
        pin: None,
    }];
    let build_id = h.build(&spec).await;
    let started = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();
    wait_detail(&h.state, started.job_id, |d| d["phase"] == "resolve").await;
    jobs::cancel(&h.state, started.job_id).await.unwrap();
    let r = wait_run(&h.state, started.run_id).await;
    assert_eq!(r.status, BuildRunStatus::Canceled);
    assert_eq!(h.podman.spawn_count(), 0, "nothing was built");
    let log = whole_log(&h.state, r.id).await;
    assert!(log.contains("stopping the resolve step"), "{log}");
}

#[tokio::test]
async fn a_run_waits_for_the_lock_naming_its_holder() {
    let h = Harness::new().await;
    let build_id = h.build(&h.spec()).await;
    // Another build (another lmgw, say) holds the machine-wide lock.
    let lock_path = h.root.join("run").join("lmgw-build.lock");
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let held = std::fs::File::create(&lock_path).unwrap();
    held.lock().unwrap();
    std::fs::write(
        h.root.join("run").join("lmgw-build.lock.holder"),
        r#"{"slug":"ik-main","run_id":3,"pid":4711}"#,
    )
    .unwrap();

    let started = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();
    let detail = wait_detail(&h.state, started.job_id, |d| {
        d["phase"] == "waiting" && d["waiting_for"].is_string()
    })
    .await;
    assert_eq!(detail["waiting_for"], "ik-main (run 3, pid 4711)");
    assert_eq!(
        h.state.jobs.live_one(started.job_id).unwrap().stage,
        "waiting for ik-main (run 3, pid 4711)"
    );
    assert_eq!(h.podman.spawn_count(), 0);

    held.unlock().unwrap();
    drop(held);
    let r = wait_run(&h.state, started.run_id).await;
    assert_eq!(r.status, BuildRunStatus::Succeeded, "{:?}", r.error);
}

#[tokio::test]
async fn a_failed_verify_is_broken_and_not_promoted() {
    let h = Harness::new().await;
    let build_id = h.build(&h.spec()).await;
    h.podman.world().devices_out = "Available devices:\n".into();
    let started = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();
    let r = wait_run(&h.state, started.run_id).await;
    assert_eq!(r.status, BuildRunStatus::Broken);
    assert!(!r.promoted);
    assert!(r.error.unwrap().contains("reported no device"));
    assert_eq!(r.verify.as_ref().unwrap()["help_ok"], true);
    assert_eq!(r.verify.as_ref().unwrap()["gpu_verified"], false);
    assert_eq!(
        h.podman
            .world()
            .id_of("localhost/lmgw-llama-server:official-master"),
        None,
        "the moving tag was not moved"
    );
    assert!(run::promote_run(&h.state, r.id)
        .await
        .unwrap_err()
        .contains("broken"));

    // Verify now, with the card answering this time: succeeded, promoted.
    // When the run finished stays put (the history's "Took").
    sqlx::query("UPDATE build_runs SET finished_at='2026-01-01 00:00:00' WHERE id=?1")
        .bind(r.id)
        .execute(&h.state.db)
        .await
        .unwrap();
    h.podman.world().devices_out = "  CUDA0: NVIDIA GeForce RTX 4090\n".into();
    let report = run::verify_run(&h.state, r.id).await.unwrap();
    assert!(report.gpu_verified, "{report:?}");
    let r = store::get_build_run(&h.state.db, r.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.status, BuildRunStatus::Succeeded);
    assert!(r.promoted);
    assert_eq!(r.error, None);
    assert_eq!(r.finished_at.as_deref(), Some("2026-01-01 00:00:00"));
    assert_eq!(
        h.podman
            .world()
            .id_of("localhost/lmgw-llama-server:official-master"),
        r.image_id
    );
}

#[tokio::test]
async fn under_the_gpu_hold_a_run_is_unverified_and_nothing_is_probed() {
    let h = Harness::new().await;
    h.settings(|s| s.hold.active = true).await;
    let build_id = h.build(&h.spec()).await;
    let started = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();
    let r = wait_run(&h.state, started.run_id).await;
    assert_eq!(r.status, BuildRunStatus::Unverified);
    assert!(!r.promoted);
    assert!(r.error.unwrap().contains("GPU hold"));
    assert!(
        h.podman.calls_of("run").is_empty(),
        "no container was started"
    );
    let job = store::get_job(&h.state.db, started.job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.status, "done");

    // "Make current" still works for an unverified run — the owner's call.
    let promoted = run::promote_run(&h.state, r.id).await.unwrap();
    assert_eq!(
        promoted.moving_tag,
        "localhost/lmgw-llama-server:official-master"
    );
    assert_eq!(promoted.from_image, None);
    assert_eq!(Some(promoted.to_image), r.image_id);
}

/// Three older runs of the build, newest last, each with an image under its
/// own immutable tag.
async fn seed_old_runs(h: &Harness, build_id: i64) -> Vec<String> {
    let build = store::get_build(&h.state.db, build_id)
        .await
        .unwrap()
        .unwrap();
    let mut tags = Vec::new();
    for (n, hex) in [(3, "333333"), (2, "222222"), (1, "111111")] {
        let tag = format!("localhost/lmgw-llama-server:official-master-000000{n}-{hex}");
        let id = format!("{:0>64}", format!("a{n}"));
        h.podman.add_image(&id, &[&tag], &[]);
        let run_id =
            store::insert_build_run(&h.state.db, &NewBuildRun::of(&build, BuildTrigger::Manual))
                .await
                .unwrap();
        store::update_build_run(
            &h.state.db,
            run_id,
            &BuildRunPatch {
                image_id: Some(id),
                tags: Some(vec![tag.clone()]),
                ..BuildRunPatch::default()
            },
        )
        .await
        .unwrap();
        store::finish_build_run(&h.state.db, run_id, BuildRunStatus::Succeeded, None)
            .await
            .unwrap();
        tags.push(tag);
    }
    tags
}

fn local_model(model_id: &str, image: Option<String>) -> store::NewLocalModel {
    store::NewLocalModel {
        model_id: model_id.into(),
        gguf_path: format!("{model_id}.gguf"),
        params: Default::default(),
        args: vec![],
        idle_seconds: 300,
        enabled: true,
        public: true,
        image,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
        ladder: vec![],
    }
}

#[tokio::test]
async fn retention_keeps_the_newest_and_whatever_is_in_use() {
    let h = Harness::new().await;
    let mut spec = h.spec();
    spec.keep_runs = Some(1);
    let build_id = h.build(&spec).await;
    let old = seed_old_runs(&h, build_id).await; // [3, 2, 1], 1 newest
                                                 // Run 2's image is pinned by a chat model.
    store::insert_local_model(&h.state.db, &local_model("pinned", Some(old[1].clone())))
        .await
        .unwrap();
    h.state.reload_snapshot().await.unwrap();

    let started = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();
    let r = wait_run(&h.state, started.run_id).await;
    assert_eq!(r.status, BuildRunStatus::Succeeded, "{:?}", r.error);
    {
        let w = h.podman.world();
        assert!(
            w.id_of(&old[2]).is_some(),
            "the newest previous run is kept"
        );
        assert!(w.id_of(&old[1]).is_some(), "the pinned one is kept");
        assert!(w.id_of(&old[0]).is_none(), "the rest goes");
    }
    let log = whole_log(&h.state, r.id).await;
    assert!(log.contains(&format!("kept {}", old[2])), "{log}");
    assert!(log.contains("keep_runs"), "{log}");
    assert!(
        log.contains(&format!("kept {}", old[1])) && log.contains("in use by chat model 'pinned'"),
        "{log}"
    );
    assert!(log.contains(&format!("removed {}", old[0])), "{log}");
}

#[tokio::test]
async fn a_dev_instance_prunes_nothing_and_deletes_nothing() {
    let h = Harness::new().await;
    h.state.set_dev_for_tests(true);
    let mut spec = h.spec();
    spec.keep_runs = Some(0);
    let build_id = h.build(&spec).await;
    let old = seed_old_runs(&h, build_id).await;
    let started = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();
    let r = wait_run(&h.state, started.run_id).await;
    assert_eq!(r.status, BuildRunStatus::Succeeded, "{:?}", r.error);
    for tag in &old {
        assert!(h.podman.world().id_of(tag).is_some(), "{tag} kept");
    }
    assert!(h.podman.calls_of("rmi").is_empty());
    assert!(whole_log(&h.state, r.id)
        .await
        .contains("keep_runs pruning skipped: this is a dev instance"));

    // Not even forced.
    let err = images::delete_image(&h.state, &old[0], true)
        .await
        .unwrap_err();
    assert!(err.contains("dev instance"), "{err}");
    let err = images::tag_image(&h.state, &old[0], None, Some(&old[0]))
        .await
        .unwrap_err();
    assert!(err.contains("dev instance"), "{err}");
    let err = images::tag_image(&h.state, &old[0], Some("localhost/x:y"), None)
        .await
        .unwrap_err();
    assert!(err.contains("dev instance"), "{err}");
    // Its own namespace is its own.
    let tagged = images::tag_image(
        &h.state,
        &old[0],
        Some("localhost/lmgw-dev-llama-server:mine"),
        None,
    )
    .await
    .unwrap();
    assert!(tagged
        .tags
        .contains(&"localhost/lmgw-dev-llama-server:mine".to_string()));
    images::tag_image(
        &h.state,
        &old[0],
        None,
        Some("localhost/lmgw-dev-llama-server:mine"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_builds_dir_on_tmpfs_is_refused_before_anything_starts() {
    let h = Harness::new().await;
    if !lmgw_core::backends::paths::is_tmpfs(Path::new("/dev/shm")) {
        return; // no tmpfs to point at on this box
    }
    h.settings(|s| s.builds_dir = Some("/dev/shm/lmgw-test-never-created/builds".into()))
        .await;
    let build_id = h.build(&h.spec()).await;
    let err = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap_err();
    assert!(err.contains("tmpfs"), "{err}");
    assert!(store::list_build_runs(&h.state.db, Some(build_id), 0)
        .await
        .unwrap()
        .is_empty());
    assert!(!Path::new("/dev/shm/lmgw-test-never-created").exists());
    let env = run::build_env(&h.state).await;
    assert!(env.builds_dir_warning.unwrap().contains("tmpfs"));
}

#[tokio::test]
async fn the_log_is_read_by_offset_and_done_only_at_the_end_of_a_finished_run() {
    let h = Harness::new().await;
    let build_id = h.build(&h.spec()).await;
    let build = store::get_build(&h.state.db, build_id)
        .await
        .unwrap()
        .unwrap();
    let run_id =
        store::insert_build_run(&h.state.db, &NewBuildRun::of(&build, BuildTrigger::Manual))
            .await
            .unwrap();
    let path = h.root.join("some.log");
    // Just short of a chunk, then a two-byte character straddling the
    // boundary, then more.
    let mut text = "a".repeat(LOG_CHUNK_BYTES as usize - 1);
    text.push('é');
    text.push_str("tail\n");
    std::fs::write(&path, &text).unwrap();
    store::update_build_run(
        &h.state.db,
        run_id,
        &BuildRunPatch {
            log_path: Some(path.display().to_string()),
            ..BuildRunPatch::default()
        },
    )
    .await
    .unwrap();

    let first = run::read_log(&h.state, run_id, 0).await.unwrap();
    assert_eq!(first.next_offset, LOG_CHUNK_BYTES - 1, "never splits the é");
    assert!(!first.done);
    let second = run::read_log(&h.state, run_id, first.next_offset)
        .await
        .unwrap();
    assert_eq!(second.text, "étail\n");
    assert_eq!(second.next_offset, text.len() as u64);
    assert!(!second.done, "at the end, but the run is still going");

    store::finish_build_run(&h.state.db, run_id, BuildRunStatus::Failed, Some("x"))
        .await
        .unwrap();
    let chunk = run::read_log(&h.state, run_id, 0).await.unwrap();
    assert!(!chunk.done, "finished, but the reader is not at the end");
    let end = run::read_log(&h.state, run_id, second.next_offset)
        .await
        .unwrap();
    assert_eq!(end.text, "");
    assert!(end.done);
    let past = run::read_log(&h.state, run_id, 10 * LOG_CHUNK_BYTES)
        .await
        .unwrap();
    assert_eq!(past.next_offset, text.len() as u64);
    assert!(past.done);
}

// ---------------------------------------------------------------------------
// Resolve, check merge, env, boot
// ---------------------------------------------------------------------------

/// A forge that reports every PR merged, as `merge_commit` (empty: no
/// merge commit reported).
struct MergedPr {
    merge_commit: String,
}

#[async_trait]
impl ForgeLookup for MergedPr {
    async fn pr(&self, _repo_url: &str, _forge: Forge, number: u64) -> Result<ForgePr, String> {
        Ok(ForgePr {
            number,
            state: "closed".into(),
            merged_at: Some("2026-09-20T10:00:00Z".into()),
            merge_commit_sha: self.merge_commit.clone(),
            ..ForgePr::default()
        })
    }
}

#[tokio::test]
async fn resolve_previews_the_run_and_the_forge_decides_merged_upstream() {
    let h = Harness::new().await;
    // PR #1 on the upstream, as GitHub publishes it.
    h.work.git(&["checkout", "--quiet", "-b", "pr1"]);
    h.work.write("b.txt", "new\n");
    h.work.commit("pr 1");
    h.work.push(&h.upstream, "refs/heads/pr1:refs/pull/1/head");
    h.work.git(&["checkout", "--quiet", "master"]);

    let mut spec = h.spec();
    let preview = run::resolve_preview(&h.state, spec.clone()).await.unwrap();
    let base = h.base_sha();
    assert_eq!(preview.base_sha, base);
    assert_eq!(preview.dockerfile, ".devops/cuda.Dockerfile");
    assert_eq!(preview.target, "server");
    assert_eq!(preview.profile, "llama-official-cuda");
    assert_eq!(preview.build_number, Some(1));
    assert_eq!(
        preview.moving_tag,
        "localhost/lmgw-llama-server:official-master"
    );
    assert!(preview.immutable_tag.starts_with(&format!(
        "localhost/lmgw-llama-server:official-master-{}-",
        &base[..7]
    )));
    assert!(preview
        .build_args
        .contains(&("CUDA_DOCKER_ARCH".to_string(), "89".to_string())));
    assert!(preview.edits.iter().any(|e| e.name.contains("ccache")));

    // The preview's tag is the one a run then builds.
    let build_id = h.build(&spec).await;
    let started = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();
    let r = wait_run(&h.state, started.run_id).await;
    assert_eq!(r.tags.first(), Some(&preview.immutable_tag));

    // With the forge saying PR #1 was merged, it is skipped — and it does
    // not change the image, so the tag stays the same.
    spec.forge = Forge::Github;
    spec.extras = vec![BuildExtra::Pr {
        number: 1,
        pin: None,
    }];
    let unknown = run::check_merge(
        &h.state,
        BuildCheckMergeArgs {
            id: None,
            spec: Some(spec.clone()),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        unknown.steps[0].outcome,
        MergeOutcome::Merged,
        "no forge: merged"
    );
    // Merged upstream as a squash commit on master: the PR's head is not in
    // master's history, its merge commit is.
    h.work.write("b.txt", "new\n");
    let squash = h.work.commit("pr 1, squashed");
    h.work
        .push(&h.upstream, "refs/heads/master:refs/heads/master");
    let check = |spec: lmgw_core::backends::BuildSpec| {
        let state = h.state.clone();
        async move {
            run::check_merge(
                &state,
                BuildCheckMergeArgs {
                    id: None,
                    spec: Some(spec),
                },
            )
            .await
            .unwrap()
        }
    };
    h.state.builds.set_forge(Arc::new(MergedPr {
        merge_commit: squash.clone(),
    }));
    let report = check(spec.clone()).await;
    assert!(report.ok);
    assert_eq!(report.steps[0].outcome, MergeOutcome::MergedUpstream);
    assert!(
        report.steps[0].note.contains("forge: closed, merged"),
        "{:?}",
        report.steps[0]
    );
    // A merged PR does not change the image: the tag is master's alone.
    let merged = run::resolve_preview(&h.state, spec.clone()).await.unwrap();
    let mut plain = spec.clone();
    plain.extras.clear();
    let plain = run::resolve_preview(&h.state, plain).await.unwrap();
    assert_eq!(merged.immutable_tag, plain.immutable_tag);

    // Built from the old base, which predates the merge: the forge's
    // "merged" does not make it part of that base — it is merged in, and
    // still noted as merged upstream.
    let mut old = spec.clone();
    old.git_ref = base.clone();
    let report = check(old.clone()).await;
    assert!(report.ok);
    assert_eq!(report.steps[0].outcome, MergeOutcome::Merged);
    assert!(
        report.steps[0].note.contains("forge: closed, merged"),
        "{:?}",
        report.steps[0]
    );
    let old_preview = run::resolve_preview(&h.state, old.clone()).await.unwrap();
    assert_ne!(
        old_preview.immutable_tag, preview.immutable_tag,
        "the PR is in this image"
    );

    // No merge commit from the forge: the head decides, and PR #1's head
    // is not in master (it was squashed) — merged, and found already there.
    h.state.builds.set_forge(Arc::new(MergedPr {
        merge_commit: String::new(),
    }));
    let report = check(spec.clone()).await;
    assert!(report.ok);
    assert_eq!(report.steps[0].outcome, MergeOutcome::AlreadyInBase);
}

#[tokio::test]
async fn build_env_reports_the_host_and_the_presets() {
    let h = Harness::new().await;
    let env = run::build_env(&h.state).await;
    assert_eq!(env.arch_auto, vec!["89"]);
    assert_eq!(env.driver_cuda_max.as_deref(), Some("13.1"));
    assert_eq!(env.cuda_default, "13.0.0");
    assert!(env.git_ok);
    assert_eq!(env.builds_dir, h.builds_dir().display().to_string());
    assert_eq!(env.builds_dir_warning, None);
    assert_eq!(env.repo_presets.len(), 4);
    assert!(env.repo_presets.iter().any(|p| p.id == "ik"));
}

#[tokio::test]
async fn the_boot_sweep_removes_stale_run_dirs_unless_a_build_holds_the_lock() {
    let h = Harness::new().await;
    let work = h.builds_dir().join("work");
    let tmp = h.builds_dir().join("tmp");
    let me = h.state.builds.instance_id().to_string();
    let (mine, mine_ctx) = (format!("run-{me}-7"), format!("run-{me}-7.ctx"));
    for d in [
        mine.as_str(),
        mine_ctx.as_str(),
        "run-ffffffff-7",
        "7",
        "notes",
    ] {
        std::fs::create_dir_all(work.join(d)).unwrap();
    }
    std::fs::create_dir_all(tmp.join(&mine)).unwrap();
    std::fs::create_dir_all(tmp.join("run-ffffffff-7")).unwrap();
    let lock_path = h.root.join("run").join("lmgw-build.lock");
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let held = std::fs::File::create(&lock_path).unwrap();
    held.lock().unwrap();
    run::boot_sweep(&h.state).await;
    assert!(work.join(&mine).exists(), "a build is running somewhere");
    held.unlock().unwrap();
    drop(held);

    run::boot_sweep(&h.state).await;
    assert!(!work.join(&mine).exists());
    assert!(!work.join(&mine_ctx).exists());
    assert!(!tmp.join(&mine).exists());
    for other in ["run-ffffffff-7", "7", "notes"] {
        assert!(work.join(other).exists(), "{other} is not this instance's");
    }
    assert!(tmp.join("run-ffffffff-7").exists());
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

#[tokio::test]
async fn images_are_listed_with_their_users_by_image_id() {
    let h = Harness::new().await;
    let (a, b, c, d, e) = (
        "a".repeat(64),
        "b".repeat(64),
        "c".repeat(64),
        "d".repeat(64),
        "e".repeat(64),
    );
    h.podman
        .add_image(&a, &["localhost/llama-server-cuda:official-latest"], &[]);
    h.podman.add_image(
        &b,
        &["localhost/lmgw-llama-server:official-master-abcdef0-123456"],
        &[
            ("dev.lmgw.run", "5"),
            ("dev.lmgw.build", "2"),
            ("dev.lmgw.slug", "official-master"),
            ("dev.lmgw.engine", "llama"),
            ("dev.lmgw.backend", "cuda"),
            ("dev.lmgw.repo", "https://github.com/ggml-org/llama.cpp"),
            ("dev.lmgw.ref", "master"),
            ("dev.lmgw.base", "abcdef0123"),
            (
                "dev.lmgw.extras",
                r#"[{"extra":{"kind":"pr","number":7},"sha":"1111111111111111111111111111111111111111"}]"#,
            ),
        ],
    );
    h.podman
        .add_image(&c, &["ghcr.io/leejet/stable-diffusion.cpp:cuda"], &[]);
    h.podman
        .add_image(&d, &["docker.io/library/postgres:17-alpine"], &[]);
    h.podman
        .add_image(&e, &["localhost/llama-server-cuda:ik-old"], &[]);
    h.settings(|s| {
        s.router.image = "localhost/llama-server-cuda:official-latest".into();
        // Another spelling of the same image.
        s.aux_router.image = format!("sha256:{}", &a[..12]);
    })
    .await;
    // The model pins b by its immutable tag.
    store::insert_local_model(
        &h.state.db,
        &local_model(
            "qwen",
            Some("localhost/lmgw-llama-server:official-master-abcdef0-123456".into()),
        ),
    )
    .await
    .unwrap();
    h.state.reload_snapshot().await.unwrap();
    h.podman.world().running.push(FakeContainer {
        name: "lmgw-image-flux-a1b2c3".into(),
        image_id: c.clone(),
        class: "image".into(),
        model: "flux".into(),
    });

    let list = images::list_images(&h.state, None, true).await.unwrap();
    let by_id = |id: &str| list.images.iter().find(|i| i.id == id).cloned();
    assert_eq!(list.images.len(), 4, "postgres is no engine's");
    assert!(by_id(&d).is_none());

    let img_a = by_id(&a).unwrap();
    assert!(img_a.external);
    assert_eq!(img_a.engine, Some(Engine::Llama));
    let classes: Vec<(ImageUseKind, String)> = img_a
        .used_by
        .iter()
        .map(|u| (u.kind, u.class.clone()))
        .collect();
    assert_eq!(
        classes,
        vec![
            (ImageUseKind::ClassDefault, "chat".to_string()),
            (ImageUseKind::ClassDefault, "aux".to_string()),
        ]
    );

    let img_b = by_id(&b).unwrap();
    assert!(!img_b.external);
    assert_eq!(img_b.backend.as_deref(), Some("cuda"));
    let prov = img_b.provenance.unwrap();
    assert_eq!((prov.run_id, prov.build_id), (5, 2));
    assert_eq!(prov.slug, "official-master");
    assert_eq!(prov.extras.len(), 1);
    assert_eq!(img_b.used_by.len(), 1);
    assert_eq!(img_b.used_by[0].kind, ImageUseKind::ModelOverride);
    assert_eq!(img_b.used_by[0].model_id.as_deref(), Some("qwen"));

    let img_c = by_id(&c).unwrap();
    assert_eq!(img_c.engine, Some(Engine::Sdcpp));
    assert_eq!(img_c.used_by[0].kind, ImageUseKind::RunningContainer);
    assert_eq!(
        img_c.used_by[0].container.as_deref(),
        Some("lmgw-image-flux-a1b2c3")
    );
    assert!(by_id(&e).unwrap().used_by.is_empty());

    let only_sd = images::list_images(&h.state, Some(Engine::Sdcpp), false)
        .await
        .unwrap();
    assert_eq!(only_sd.images.len(), 1);
    assert!(images::list_images(&h.state, Some(Engine::Audio), false)
        .await
        .unwrap()
        .images
        .is_empty());

    // The footer: podman's totals, the buildah scratch dir on disk and the
    // working container in `storage` (the exited container is no orphan).
    assert_eq!(list.disk.images_total, 3_000_000);
    assert_eq!(list.disk.reclaimable, 1_000_000);
    assert_eq!(
        list.disk.buildah_orphans,
        vec![
            h.root
                .join("vartmp")
                .join("buildah111")
                .display()
                .to_string(),
            "buildah container old-working-container (old1)".to_string()
        ]
    );

    // Deleting refuses while used (unless forced), and names the users.
    let err = images::delete_image(
        &h.state,
        "localhost/llama-server-cuda:official-latest",
        false,
    )
    .await
    .unwrap_err();
    assert!(err.contains("the chat class default"), "{err}");
    assert!(err.contains("the aux class default"), "{err}");
    let err = images::delete_image(&h.state, &c, false).await.unwrap_err();
    assert!(
        err.contains("running container lmgw-image-flux-a1b2c3"),
        "{err}"
    );
    let gone = images::delete_image(&h.state, "localhost/llama-server-cuda:ik-old", false)
        .await
        .unwrap();
    assert_eq!(
        gone.removed,
        vec!["localhost/llama-server-cuda:ik-old".to_string(), e.clone()]
    );
    assert!(h.podman.world().find(&e).is_none());

    // Tags: add one, and refuse removing one a class names.
    let tagged = images::tag_image(&h.state, &a, Some("localhost/keep:me"), None)
        .await
        .unwrap();
    assert!(tagged.tags.contains(&"localhost/keep:me".to_string()));
    let err = images::tag_image(
        &h.state,
        &a,
        None,
        Some("localhost/llama-server-cuda:official-latest"),
    )
    .await
    .unwrap_err();
    assert!(err.contains("chat class default"), "{err}");
    // Adding a name that names another image moves it: refused while
    // anything follows it — here a container runs the image it names, and
    // the model pinned to b's immutable tag — and nothing is half-applied.
    let err = images::tag_image(
        &h.state,
        &a,
        Some("ghcr.io/leejet/stable-diffusion.cpp:cuda"),
        None,
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("running container lmgw-image-flux-a1b2c3"),
        "{err}"
    );
    let err = images::tag_image(
        &h.state,
        &a,
        Some("localhost/lmgw-llama-server:official-master-abcdef0-123456"),
        Some("localhost/keep:me"),
    )
    .await
    .unwrap_err();
    assert!(err.contains("chat model 'qwen'"), "{err}");
    assert!(
        h.podman
            .world()
            .images
            .iter()
            .any(|i| i.id == a && i.names.contains(&"localhost/keep:me".to_string())),
        "the remove half was not applied either"
    );
    // An unused name of another image moves freely.
    h.podman
        .add_image(&"f".repeat(64), &["localhost/scratch:x"], &[]);
    images::tag_image(&h.state, &a, Some("localhost/scratch:x"), None)
        .await
        .unwrap();
    assert_eq!(
        h.podman.world().id_of("localhost/scratch:x"),
        Some(a.clone())
    );
    // With a second tag, deleting the unused one only untags it.
    let untagged = images::delete_image(&h.state, "localhost/keep:me", false)
        .await
        .unwrap();
    assert_eq!(untagged.removed, vec!["localhost/keep:me".to_string()]);
    assert!(h.podman.world().find(&a).is_some());
}
