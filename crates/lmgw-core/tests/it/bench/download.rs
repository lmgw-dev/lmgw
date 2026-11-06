//! A run whose engine image is not on this machine downloads it first
//! (machine-check design §5): before it takes the GPU's lease, so the
//! download neither holds the card nor is timed as the load, and the bench
//! container then starts with `--pull=never`. The stored command line keeps
//! podman's default, so a copy of it pulls what it needs.

use super::*;
use crate::support::scripted_pull::ScriptedPull;

/// A registry on this machine's loopback: a run may pull from it, and no
/// manifest read reaches out for it (lmgw reads no manifest of a
/// `localhost:<port>` registry). Nothing listens there: the pull is scripted.
const ENGINE: &str = "localhost:5000/lmgw-test/engine:cuda";

#[tokio::test]
async fn a_run_downloads_its_missing_engine_before_it_takes_the_gpu() {
    let w = World::new(quick()).await;
    w.g.world().missing_images.insert(ENGINE.into());
    w.launcher.image_missing.store(true, Ordering::SeqCst);
    let (world, launcher) = (w.g.world.clone(), w.launcher.clone());
    let (api, release) = ScriptedPull::new(
        Vec::new(),
        Vec::new(),
        None,
        Arc::new(move || {
            world.lock().unwrap().missing_images.clear();
            launcher.image_missing.store(false, Ordering::SeqCst);
        }),
    );
    w.state().builds.set_pull_api(api.clone());

    // Not blocked: the plan says the run downloads it first.
    let plan = w
        .ok("bench_plan", json!({"model_id": "qwen", "image": ENGINE}))
        .await;
    assert_eq!(plan["blocked"], Value::Null, "{plan}");
    assert!(
        plan["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("the run downloads it first")),
        "{plan}"
    );

    let (run_id, job) = w
        .start(json!({"model_id": "qwen", "image": ENGINE, "repetitions": 1}))
        .await;
    let stage = w
        .stage(job, |s| s.contains("downloading the engine image"))
        .await;
    assert!(stage.contains(ENGINE), "{stage}");
    // While it downloads the card is still the owner's: no lease, and a
    // local model is admitted as ever.
    assert!(w.state().snapshot().gpu_lease.is_none(), "no lease yet");
    assert_eq!(w.chat("other").await.status(), 200);
    assert!(w.launcher.verb("run").is_empty());

    release.send(true).unwrap();
    w.finished(job).await;
    let d = w.run(run_id).await;
    assert_eq!(d["run"]["status"], "done", "{}", d["run"]["error"]);
    let runs = w.launcher.verb("run");
    assert_eq!(runs.len(), 1);
    let verb = runs[0].iter().position(|a| a == "run").unwrap();
    assert_eq!(runs[0][verb + 1], "--pull=never", "{:?}", runs[0]);
    let cl = d["run"]["command_line"].as_str().unwrap();
    assert!(cl.contains(ENGINE), "{cl}");
    assert!(
        !cl.contains("--pull"),
        "the stored line keeps the default: {cl}"
    );
    assert_eq!(api.calls(), vec![ENGINE.to_string()]);
}

/// A `localhost/` build that is not here cannot be downloaded: the run is
/// refused before it starts, as before.
#[tokio::test]
async fn a_missing_local_build_is_still_refused() {
    let w = World::new(quick()).await;
    let image = "localhost/lmgw-llama-server:gone";
    w.g.world().missing_images.insert(image.into());
    w.launcher.image_missing.store(true, Ordering::SeqCst);
    let msg = w
        .refused("bench_start", json!({"model_id": "qwen", "image": image}))
        .await;
    assert!(
        msg.contains("not on this machine") && msg.contains("pull or build it first"),
        "{msg}"
    );
    assert!(w.launcher.verb("run").is_empty());
}
