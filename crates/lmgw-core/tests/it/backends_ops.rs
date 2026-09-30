//! The Backends ops (container-builds design §9.2, §15) over the real
//! router — `POST /api/op/<name>` — and the `lmgw__build*` self-admin tools
//! over `/mcp/admin`, on the shared fake podman and fixture repositories of
//! `support/backends_fake.rs`. What the layers below do is covered in
//! `backends_run.rs`, `backends_git.rs` and `backends_forge.rs`; this suite
//! covers the seams: argument and answer shapes, the dispatch, refusals in
//! words, delete-with-images, and the mode gate.

use crate::common;
use crate::support::backends_fake as support;

use std::sync::Arc;
use std::time::Duration;

use lmgw_api_types::builds::{BuildSetAction, BuildSetArgs, BuildSpec};
use lmgw_core::backends::forge::{ForgeClient, GatewayForge};
use lmgw_core::backends::tags;
use lmgw_core::backends::NewBuildRun;
use lmgw_core::backends::{
    BuildExtra, BuildRunPatch, BuildRunStatus, BuildTrigger, Engine, Forge, GpuBackend,
};
use lmgw_core::config::SelfAdmin;
use lmgw_core::state::AppState;
use lmgw_core::store;
use serde_json::{json, Value};
use support::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const MOVING: &str = "localhost/lmgw-llama-server:official-master";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn op(gw: &common::Gw, name: &str, args: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn op_ok(gw: &common::Gw, name: &str, args: Value) -> Value {
    let (status, body) = op(gw, name, args).await;
    assert_eq!(status, 200, "{name}: {body}");
    body
}

/// A refused op: `400 op_failed` and its sentence.
async fn op_err(gw: &common::Gw, name: &str, args: Value) -> String {
    let (status, body) = op(gw, name, args).await;
    assert_eq!(status, 400, "{name} should be refused: {body}");
    assert_eq!(body["code"], "op_failed", "{body}");
    body["message"].as_str().unwrap().to_string()
}

fn create_args(spec: BuildSpec) -> Value {
    serde_json::to_value(BuildSetArgs {
        action: BuildSetAction::Create,
        spec: Some(spec),
        ..BuildSetArgs::default()
    })
    .unwrap()
}

async fn create(gw: &common::Gw, spec: BuildSpec) -> i64 {
    let v = op_ok(gw, "build_set", create_args(spec)).await;
    v["build"]["id"].as_i64().unwrap()
}

/// Follow a run's log through `build_run_log` until it says `done`.
async fn follow_log(gw: &common::Gw, run_id: i64) -> String {
    let (mut text, mut offset) = (String::new(), 0u64);
    for _ in 0..2000 {
        let chunk = op_ok(
            gw,
            "build_run_log",
            json!({"run_id": run_id, "offset": offset}),
        )
        .await;
        text.push_str(chunk["text"].as_str().unwrap());
        offset = chunk["next_offset"].as_u64().unwrap();
        if chunk["done"] == true {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("run {run_id}'s log never finished: {text}");
}

/// A finished run of `build_id` with `image` carrying `tags`, recorded as
/// the executor would.
async fn seed_run(h: &Harness, build_id: i64, image: &str, tags: &[&str], promoted: bool) -> i64 {
    let build = store::get_build(&h.state.db, build_id)
        .await
        .unwrap()
        .unwrap();
    let run_id =
        store::insert_build_run(&h.state.db, &NewBuildRun::of(&build, BuildTrigger::Manual))
            .await
            .unwrap();
    store::update_build_run(
        &h.state.db,
        run_id,
        &BuildRunPatch {
            image_id: Some(image.into()),
            tags: Some(tags.iter().map(|t| t.to_string()).collect()),
            ..BuildRunPatch::default()
        },
    )
    .await
    .unwrap();
    store::finish_build_run(&h.state.db, run_id, BuildRunStatus::Succeeded, None)
        .await
        .unwrap();
    if promoted {
        store::promote_build_run(&h.state.db, run_id).await.unwrap();
    }
    run_id
}

fn local_model(model_id: &str, image: &str) -> store::NewLocalModel {
    store::NewLocalModel {
        model_id: model_id.into(),
        gguf_path: format!("{model_id}.gguf"),
        params: Default::default(),
        args: vec![],
        idle_seconds: 300,
        enabled: true,
        public: true,
        image: Some(image.into()),
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
        ladder: vec![],
    }
}

fn imm(n: u32) -> String {
    format!("{MOVING}-000000{n}-{n}{n}{n}{n}{n}{n}")
}

// ---------------------------------------------------------------------------
// Builds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_build_is_created_listed_read_updated_duplicated_and_deleted() {
    let h = Harness::new().await;
    let gw = common::serve(h.state.clone()).await;

    let created = op_ok(&gw, "build_set", create_args(h.spec())).await;
    let id = created["build"]["id"].as_i64().unwrap();
    assert_eq!(created["build"]["slug"], "official-master");
    assert_eq!(created["build"]["ref"], "master");
    assert_eq!(created["removed_images"], json!([]));

    let list = op_ok(&gw, "builds", json!({})).await;
    let row = &list["builds"][0];
    assert_eq!(list["builds"].as_array().unwrap().len(), 1);
    assert_eq!(row["build"]["id"], id);
    assert!(row["last_run"].is_null() && row["current_run"].is_null());
    assert!(row["live_job_id"].is_null());
    assert!(row["update"].is_null());
    assert_eq!(row["used_by"], json!([]));
    assert_eq!(
        row["moving_tag"], MOVING,
        "the server's, for a never-run build too"
    );
    assert!(list.get("usage_error").is_none(), "{list}");

    // A class default that follows the build counts as a user of it, even
    // before the tag names anything on this machine.
    h.settings(|s| s.aux_router.image = MOVING.into()).await;
    let list = op_ok(&gw, "builds", json!({})).await;
    let used_by = list["builds"][0]["used_by"].as_array().unwrap().clone();
    assert_eq!(used_by.len(), 1, "{used_by:?}");
    assert_eq!(used_by[0]["kind"], "class_default");
    assert_eq!(used_by[0]["class"], "aux");

    let got = op_ok(&gw, "build_get", json!({"id": id})).await;
    assert_eq!(got["view"]["build"]["slug"], "official-master");
    assert_eq!(got["view"]["moving_tag"], MOVING);
    // A dev instance's builds follow the dev namespace — never-run or not.
    h.state.set_dev_for_tests(true);
    let dev = op_ok(&gw, "build_get", json!({"id": id})).await;
    assert_eq!(
        dev["view"]["moving_tag"],
        "localhost/lmgw-dev-llama-server:official-master"
    );
    h.state.set_dev_for_tests(false);
    assert_eq!(got["view"]["used_by"].as_array().unwrap().len(), 1);
    assert_eq!(
        (got["runs"].clone(), got["more"].clone()),
        (json!([]), json!(false))
    );

    let mut spec = h.spec();
    spec.notes = "pinned for the 4090".into();
    spec.keep_runs = None;
    let updated = op_ok(
        &gw,
        "build_set",
        json!({"action": "update", "id": id, "spec": spec}),
    )
    .await;
    assert_eq!(updated["build"]["notes"], "pinned for the 4090");
    assert!(updated["build"]["keep_runs"].is_null());

    let copy = op_ok(&gw, "build_set", json!({"action": "duplicate", "id": id})).await;
    let copy_id = copy["build"]["id"].as_i64().unwrap();
    assert_ne!(copy_id, id);
    assert_eq!(copy["build"]["slug"], "official-master-copy");
    assert_eq!(copy["build"]["notes"], "pinned for the 4090");
    let named = op_ok(
        &gw,
        "build_set",
        json!({"action": "duplicate", "id": id, "slug": "official-master-pr1", "name": "PR 1"}),
    )
    .await;
    assert_eq!(named["build"]["slug"], "official-master-pr1");
    assert_eq!(named["build"]["name"], "PR 1");

    for gone in [copy_id, named["build"]["id"].as_i64().unwrap()] {
        let deleted = op_ok(&gw, "build_set", json!({"action": "delete", "id": gone})).await;
        assert!(deleted["build"].is_null());
    }
    let list = op_ok(&gw, "builds", json!({})).await;
    assert_eq!(list["builds"].as_array().unwrap().len(), 1);
    let err = op_err(&gw, "build_get", json!({"id": copy_id})).await;
    assert_eq!(err, format!("no build with id {copy_id}"));
}

#[tokio::test]
async fn a_refused_definition_names_the_field() {
    let h = Harness::new().await;
    let gw = common::serve(h.state.clone()).await;

    let mut no_ref = h.spec();
    no_ref.git_ref = String::new();
    let err = op_err(&gw, "build_set", create_args(no_ref)).await;
    assert!(err.starts_with("ref cannot be empty"), "{err}");

    let mut pr_on_plain = h.spec();
    pr_on_plain.extras = vec![BuildExtra::Pr {
        number: 7,
        pin: None,
    }];
    let err = op_err(&gw, "build_set", create_args(pr_on_plain)).await;
    assert!(err.starts_with("extras[0]: PR #7 needs a forge"), "{err}");

    create(&gw, h.spec()).await;
    let err = op_err(&gw, "build_set", create_args(h.spec())).await;
    assert!(
        err.contains("slug 'official-master' already exists"),
        "{err}"
    );
    assert!(
        !err.starts_with("bad request"),
        "the store's prefix is not shown: {err}"
    );

    let err = op_err(&gw, "build_set", json!({"action": "update", "id": 1})).await;
    assert!(err.starts_with("update needs spec"), "{err}");
    let err = op_err(&gw, "build_set", json!({"action": "rename"})).await;
    assert!(err.starts_with("invalid arguments"), "{err}");
}

#[tokio::test]
async fn a_run_starts_as_a_job_its_log_reads_by_offset_and_its_history_pages() {
    let h = Harness::new().await;
    let gw = common::serve(h.state.clone()).await;
    let id = create(&gw, h.spec()).await;
    h.podman.set_behavior(Behavior::HoldThenSucceed);

    let started = op_ok(&gw, "build_run", json!({"id": id})).await;
    let job_id = started["job_id"].as_i64().unwrap();
    let run_id = started["run_id"].as_i64().unwrap();
    wait_detail(&h.state, job_id, |d| d["phase"] == "build").await;

    // While it runs: the row shows the job, and the build cannot be deleted.
    let list = op_ok(&gw, "builds", json!({})).await;
    assert_eq!(list["builds"][0]["live_job_id"], job_id);
    assert_eq!(list["builds"][0]["last_run"]["id"], run_id);
    assert_eq!(list["builds"][0]["last_run"]["status"], "running");
    let err = op_err(&gw, "build_set", json!({"action": "delete", "id": id})).await;
    assert!(err.contains("has a run in progress"), "{err}");
    let err = op_err(&gw, "build_run", json!({"id": id})).await;
    assert!(err.contains("already has a run in progress"), "{err}");

    h.podman.0.release.notify_one();
    let log = follow_log(&gw, run_id).await;
    assert!(
        log.contains(&format!("run {run_id} of build official-master (manual)")),
        "{log}"
    );
    for phase in ["resolve", "fetch", "build", "verify", "promote", "cleanup"] {
        assert!(phases_in(&log).iter().any(|p| p == phase), "{phase}: {log}");
    }

    let got = op_ok(&gw, "build_get", json!({"id": id, "limit": 10})).await;
    assert_eq!(got["runs"][0]["id"], run_id);
    assert_eq!(got["runs"][0]["status"], "succeeded");
    assert_eq!(got["runs"][0]["trigger"], "manual");
    assert_eq!(got["view"]["current_run"]["id"], run_id);
    assert!(got["view"]["live_job_id"].is_null());

    // The same inputs again: up to date. Two runs page one at a time.
    let again = op_ok(&gw, "build_run", json!({"id": id})).await;
    let second = again["run_id"].as_i64().unwrap();
    assert!(follow_log(&gw, second).await.contains("up to date"));
    let page = op_ok(&gw, "build_get", json!({"id": id, "limit": 1})).await;
    assert_eq!(page["runs"].as_array().unwrap().len(), 1);
    assert_eq!(page["runs"][0]["id"], second);
    assert_eq!(page["runs"][0]["status"], "up_to_date");
    assert_eq!(page["more"], true);
    let older = op_ok(
        &gw,
        "build_get",
        json!({"id": id, "limit": 1, "before": second}),
    )
    .await;
    assert_eq!(older["runs"][0]["id"], run_id);
    assert_eq!(older["more"], false);
    // `before` is an exclusive id cursor: `run_id + 1` with limit 1 is
    // exactly that run (the log panel's lookup), whether or not a run with
    // that id exists.
    let exact = op_ok(
        &gw,
        "build_get",
        json!({"id": id, "limit": 1, "before": second + 1}),
    )
    .await;
    assert_eq!(exact["runs"][0]["id"], second);
    let exact = op_ok(
        &gw,
        "build_get",
        json!({"id": id, "limit": 1, "before": run_id + 1}),
    )
    .await;
    assert_eq!(exact["runs"][0]["id"], run_id);
    let none = op_ok(&gw, "build_get", json!({"id": id, "before": run_id})).await;
    assert_eq!(
        (none["runs"].clone(), none["more"].clone()),
        (json!([]), json!(false))
    );

    // The moving tag now names the run's image: a model on it is a user.
    store::insert_local_model(&h.state.db, &local_model("qwen", MOVING))
        .await
        .unwrap();
    h.state.reload_snapshot().await.unwrap();
    let list = op_ok(&gw, "builds", json!({})).await;
    let used_by = &list["builds"][0]["used_by"];
    assert_eq!(used_by[0]["kind"], "model_override", "{used_by}");
    assert_eq!(used_by[0]["model_id"], "qwen");
}

#[tokio::test]
async fn delete_with_images_removes_what_is_unused_and_says_what_it_kept() {
    let h = Harness::new().await;
    let gw = common::serve(h.state.clone()).await;
    let id = create(&gw, h.spec()).await;
    let (x, y, z) = ("1".repeat(64), "2".repeat(64), "3".repeat(64));
    let (imm_a, imm_b, imm_c) = (imm(1), imm(2), imm(3));
    h.podman.add_image(&x, &[&imm_a], &[]);
    h.podman.add_image(&y, &[&imm_b, MOVING], &[]);
    h.podman
        .add_image(&z, &[&imm_c, "localhost/mine:keep"], &[]);
    let run_a = seed_run(&h, id, &x, &[&imm_a], false).await;
    seed_run(&h, id, &y, &[&imm_b, MOVING], true).await;
    seed_run(&h, id, &z, &[&imm_c], false).await;
    // Run A's image is pinned by a model.
    store::insert_local_model(&h.state.db, &local_model("pinned", &imm_a))
        .await
        .unwrap();
    h.state.reload_snapshot().await.unwrap();

    let resp = op_ok(
        &gw,
        "build_set",
        json!({"action": "delete", "id": id, "delete_images": true}),
    )
    .await;
    assert!(resp["build"].is_null());
    let mut removed: Vec<String> = serde_json::from_value(resp["removed_images"].clone()).unwrap();
    removed.sort();
    let mut want = vec![imm_b.clone(), MOVING.to_string(), imm_c.clone()];
    want.sort();
    assert_eq!(removed, want);
    let kept = resp["kept"].as_array().unwrap();
    let reason = |tag: &str| {
        kept.iter()
            .find(|k| k["tag"] == tag)
            .map(|k| k["reason"].as_str().unwrap().to_string())
            .unwrap_or_else(|| panic!("{tag} not in kept: {kept:?}"))
    };
    assert!(reason(&imm_a).contains("in use by chat model 'pinned'"));
    assert!(reason("localhost/mine:keep").contains("not one of this build's tags"));
    assert_eq!(kept.len(), 2, "{kept:?}");
    {
        let w = h.podman.world();
        assert!(w.id_of(&imm_a).is_some(), "in use: kept");
        assert!(w.find(&y).is_none(), "only this build's: gone");
        let zi = w.find(&z).expect("tagged outside the build: stays");
        assert_eq!(w.images[zi].names, vec!["localhost/mine:keep".to_string()]);
    }
    // The history outlives the build.
    let r = store::get_build_run(&h.state.db, run_a)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.build_id, None);
    assert_eq!(op_ok(&gw, "builds", json!({})).await["builds"], json!([]));
}

#[tokio::test]
async fn a_dev_instance_deletes_the_build_but_none_of_its_images() {
    let h = Harness::new().await;
    h.state.set_dev_for_tests(true);
    let gw = common::serve(h.state.clone()).await;
    let id = create(&gw, h.spec()).await;
    let y = "2".repeat(64);
    h.podman.add_image(&y, &[&imm(2), MOVING], &[]);
    seed_run(&h, id, &y, &[&imm(2), MOVING], true).await;

    let resp = op_ok(
        &gw,
        "build_set",
        json!({"action": "delete", "id": id, "delete_images": true}),
    )
    .await;
    assert!(resp["build"].is_null());
    assert_eq!(resp["removed_images"], json!([]));
    let kept = resp["kept"].as_array().unwrap();
    assert_eq!(kept.len(), 2, "{kept:?}");
    for k in kept {
        assert!(
            k["reason"].as_str().unwrap().contains("dev instance"),
            "{k}"
        );
    }
    assert!(h.podman.world().find(&y).is_some());
    assert!(h.podman.calls_of("rmi").is_empty());
    assert!(h.podman.calls_of("untag").is_empty());
    assert!(store::get_build(&h.state.db, id).await.unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Per-build cache cleanup on delete (§14.2)
// ---------------------------------------------------------------------------

/// Where buildah would keep `id`'s cache-mount data under this harness's
/// scratch root — the same math `remove_build_caches` uses, reconstructed
/// from the public pieces ([`tags::cache_mount_dir_name`], `libc::getuid`)
/// rather than by calling into `lmgw_core::backends::run`, which is
/// crate-private.
fn cache_dir(h: &Harness, id: &str) -> std::path::PathBuf {
    let uid = unsafe { libc::getuid() };
    h.root
        .join("vartmp")
        .join(format!("buildah-cache-{uid}"))
        .join(tags::cache_mount_dir_name(id))
}

fn touch_cache_dir(dir: &std::path::Path, bytes: usize) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("stats"), vec![0u8; bytes]).unwrap();
}

#[tokio::test]
async fn delete_removes_only_this_builds_own_cache_dirs() {
    let h = Harness::new().await;
    let gw = common::serve(h.state.clone()).await;
    let mut spec_a = h.spec();
    spec_a.slug = "official-master".into();
    let mut spec_b = h.spec();
    spec_b.slug = "official-master-pr1".into();
    let id_a = create(&gw, spec_a).await;
    create(&gw, spec_b).await;

    let [own_ccache_a, own_npm_a] =
        tags::own_cache_ids(Engine::Llama, GpuBackend::Cuda, "official-master");
    let [own_ccache_b, own_npm_b] =
        tags::own_cache_ids(Engine::Llama, GpuBackend::Cuda, "official-master-pr1");
    let shared_ccache = tags::ccache_id(Engine::Llama, GpuBackend::Cuda, "unused", false);
    let shared_npm = tags::npm_cache_id(Engine::Llama, "unused", false);

    for (id, size) in [
        (&own_ccache_a, 10),
        (&own_npm_a, 20),
        (&own_ccache_b, 30),
        (&own_npm_b, 40),
        (&shared_ccache, 50),
        (&shared_npm, 60),
    ] {
        touch_cache_dir(&cache_dir(&h, id), size);
    }

    let resp = op_ok(&gw, "build_set", json!({"action": "delete", "id": id_a})).await;
    assert!(resp["build"].is_null());
    let mut removed: Vec<String> = serde_json::from_value(resp["removed_caches"].clone()).unwrap();
    removed.sort();
    assert_eq!(removed.len(), 2, "{removed:?}");
    assert!(
        removed[0].starts_with(&format!("{own_ccache_a} (")),
        "{removed:?}"
    );
    assert!(
        removed[1].starts_with(&format!("{own_npm_a} (")),
        "{removed:?}"
    );

    // This build's own two are gone…
    assert!(!cache_dir(&h, &own_ccache_a).exists());
    assert!(!cache_dir(&h, &own_npm_a).exists());
    // …but the other build's own two, and the shared (non-suffixed) ones
    // every plain build would read, are untouched.
    assert!(cache_dir(&h, &own_ccache_b).exists());
    assert!(cache_dir(&h, &own_npm_b).exists());
    assert!(cache_dir(&h, &shared_ccache).exists());
    assert!(cache_dir(&h, &shared_npm).exists());
}

/// A build switched from CUDA to Vulkan still owns the caches its CUDA runs
/// filled: delete removes those too.
#[tokio::test]
async fn delete_removes_the_caches_of_every_backend_its_runs_used() {
    let h = Harness::new().await;
    let gw = common::serve(h.state.clone()).await;
    let mut spec = h.spec();
    spec.backend = GpuBackend::Vulkan;
    let id = create(&gw, spec.clone()).await;
    let mut cuda = spec.clone();
    cuda.backend = GpuBackend::Cuda;
    let build = store::get_build(&h.state.db, id).await.unwrap().unwrap();
    let mut old =
        lmgw_core::backends::NewBuildRun::of(&build, lmgw_core::backends::BuildTrigger::Manual);
    old.inputs.config = cuda;
    store::insert_build_run(&h.state.db, &old).await.unwrap();
    let [cuda_ccache, npm] =
        tags::own_cache_ids(Engine::Llama, GpuBackend::Cuda, "official-master");
    let [vulkan_ccache, _] =
        tags::own_cache_ids(Engine::Llama, GpuBackend::Vulkan, "official-master");
    for id in [&cuda_ccache, &npm, &vulkan_ccache] {
        touch_cache_dir(&cache_dir(&h, id), 10);
    }

    let resp = op_ok(&gw, "build_set", json!({"action": "delete", "id": id})).await;
    let removed: Vec<String> = serde_json::from_value(resp["removed_caches"].clone()).unwrap();
    assert_eq!(removed.len(), 3, "{removed:?}");
    for id in [&cuda_ccache, &npm, &vulkan_ccache] {
        assert!(!cache_dir(&h, id).exists(), "{id}");
    }
}

#[tokio::test]
async fn a_dev_instance_keeps_the_cache_dirs_on_delete() {
    let h = Harness::new().await;
    h.state.set_dev_for_tests(true);
    let gw = common::serve(h.state.clone()).await;
    let id = create(&gw, h.spec()).await;
    let [own_ccache, own_npm] =
        tags::own_cache_ids(Engine::Llama, GpuBackend::Cuda, "official-master");
    touch_cache_dir(&cache_dir(&h, &own_ccache), 10);
    touch_cache_dir(&cache_dir(&h, &own_npm), 20);

    let resp = op_ok(&gw, "build_set", json!({"action": "delete", "id": id})).await;
    assert!(resp["build"].is_null());
    assert_eq!(resp["removed_caches"], json!([]));
    assert!(cache_dir(&h, &own_ccache).exists());
    assert!(cache_dir(&h, &own_npm).exists());
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

#[tokio::test]
async fn images_list_with_users_and_an_image_in_use_is_not_deleted() {
    let h = Harness::new().await;
    let gw = common::serve(h.state.clone()).await;
    let (a, e) = ("a".repeat(64), "e".repeat(64));
    h.podman
        .add_image(&a, &["localhost/llama-server-cuda:official-latest"], &[]);
    h.podman
        .add_image(&e, &["localhost/llama-server-cuda:ik-old"], &[]);
    h.settings(|s| s.router.image = "localhost/llama-server-cuda:official-latest".into())
        .await;

    let list = op_ok(&gw, "container_images", json!({})).await;
    let images = list["images"].as_array().unwrap();
    assert_eq!(images.len(), 2);
    let img_a = images.iter().find(|i| i["id"] == a).unwrap();
    assert_eq!(img_a["engine"], "llama");
    assert_eq!(img_a["external"], true);
    assert_eq!(img_a["used_by"][0]["kind"], "class_default");
    assert_eq!(list["disk"]["images_total"], 3_000_000);
    let audio = op_ok(&gw, "container_images", json!({"engine": "audio"})).await;
    assert_eq!(audio["images"], json!([]));

    let err = op_err(
        &gw,
        "container_image_delete",
        json!({"image": "localhost/llama-server-cuda:official-latest"}),
    )
    .await;
    assert!(err.contains("in use by the chat class default"), "{err}");
    assert!(h.podman.world().find(&a).is_some());

    let gone = op_ok(
        &gw,
        "container_image_delete",
        json!({"image": "localhost/llama-server-cuda:ik-old"}),
    )
    .await;
    assert_eq!(
        gone["removed"],
        json!(["localhost/llama-server-cuda:ik-old", e])
    );

    let tagged = op_ok(
        &gw,
        "container_image_tag",
        json!({"image": a, "add": "localhost/keep:me"}),
    )
    .await;
    assert!(tagged["tags"]
        .as_array()
        .unwrap()
        .contains(&json!("localhost/keep:me")));
}

/// An image in use — here by an exited container nobody configured, a
/// running one, and the chat class default — is not deleted on the first ask:
/// the refusal names every user and what forcing would do, instead of
/// podman's own error (and its 64-hex container ID). Forced, it is deleted:
/// the containers are stopped and removed first, and the class default is
/// left naming a missing image, which the answer says.
#[tokio::test]
async fn an_image_in_use_is_deleted_only_when_forced_and_then_really_is() {
    let h = Harness::new().await;
    let gw = common::serve(h.state.clone()).await;
    let a = "a".repeat(64);
    let tag = "localhost/llama-server-cuda:64b38b5";
    h.podman.add_image(&a, &[tag], &[]);
    {
        let mut w = h.podman.world();
        w.external
            .push(FakeExternal::new(&"c".repeat(64), "llama-Qwen3-old", "exited").on_image(&a));
        w.running.push(FakeContainer {
            name: "someone-elses".into(),
            image_id: a.clone(),
            class: String::new(),
            model: String::new(),
        });
    }
    h.settings(|s| s.router.image = tag.into()).await;

    let list = op_ok(&gw, "container_images", json!({})).await;
    let users = list["images"][0]["used_by"].as_array().unwrap().clone();
    let kinds: Vec<&str> = users.iter().map(|u| u["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["class_default", "running_container", "stopped_container"],
        "{users:?}"
    );
    assert_eq!(users[2]["container"], "llama-Qwen3-old");

    let err = op_err(&gw, "container_image_delete", json!({"image": a})).await;
    for want in [
        "the chat class default",
        "running container someone-elses",
        "stopped container llama-Qwen3-old",
        "stops and removes someone-elses, llama-Qwen3-old",
        "leaves the chat class default naming a missing image",
    ] {
        assert!(err.contains(want), "{want:?} not in {err}");
    }
    assert!(h.podman.calls_of("rmi").is_empty());
    assert!(h.podman.calls_of("rm").is_empty());
    assert!(h.podman.world().find(&a).is_some());

    let done = op_ok(
        &gw,
        "container_image_delete",
        json!({"image": a, "force": true}),
    )
    .await;
    assert_eq!(done["removed"], json!([tag, a]));
    assert_eq!(
        done["removed_containers"],
        json!(["someone-elses", "llama-Qwen3-old"])
    );
    let named = done["still_named_by"].as_array().unwrap();
    assert_eq!(named.len(), 1, "{named:?}");
    assert_eq!(
        (named[0]["kind"].clone(), named[0]["class"].clone()),
        (json!("class_default"), json!("chat"))
    );
    let w = h.podman.world();
    assert!(w.find(&a).is_none());
    assert!(w.running.is_empty());
    assert!(!w.external.iter().any(|c| c.name == "llama-Qwen3-old"));
}

#[tokio::test]
async fn one_entry_per_image_named_after_its_build_and_the_footer_on_request() {
    let h = Harness::new().await;
    let gw = common::serve(h.state.clone()).await;
    let id = create(
        &gw,
        BuildSpec {
            name: "official master".into(),
            ..h.spec()
        },
    )
    .await;
    let (a, b) = ("a".repeat(64), "b".repeat(64));
    let build = id.to_string();
    let labels = [
        ("dev.lmgw.run", "7"),
        ("dev.lmgw.build", build.as_str()),
        ("dev.lmgw.slug", "official-master"),
        ("dev.lmgw.engine", "llama"),
    ];
    // Three tags: podman prints this row three times.
    h.podman.add_image(
        &a,
        &[
            "localhost/lmgw-llama-server:official-master-abc1234-def567",
            MOVING,
            "localhost/llama-server-cuda:official-4b1a27fa0",
        ],
        &labels,
    );
    // Built by a build that is gone since.
    h.podman.add_image(
        &b,
        &["localhost/lmgw-llama-server:old-abc1234-def567"],
        &[
            ("dev.lmgw.run", "3"),
            ("dev.lmgw.build", "999"),
            ("dev.lmgw.slug", "old"),
            ("dev.lmgw.engine", "llama"),
        ],
    );

    let list = op_ok(&gw, "container_images", json!({})).await;
    let images = list["images"].as_array().unwrap();
    assert_eq!(images.len(), 2, "one entry per image ID: {list}");
    let img_a = images.iter().find(|i| i["id"] == a).unwrap();
    assert_eq!(img_a["tags"].as_array().unwrap().len(), 3);
    assert_eq!(img_a["provenance"]["build_name"], "official master");
    let img_b = images.iter().find(|i| i["id"] == b).unwrap();
    assert!(img_b["provenance"]["build_name"].is_null(), "{img_b}");
    assert_eq!(list["disk_skipped"], false);
    assert_eq!(list["disk"]["images_total"], 3_000_000);
    let dfs = h.podman.calls_of("system").len();
    assert_eq!(dfs, 1);

    // The picker's call: no footer, and podman is not asked for one.
    let quick = op_ok(&gw, "container_images", json!({"disk": false})).await;
    assert_eq!(quick["images"].as_array().unwrap().len(), 2);
    assert_eq!(quick["disk_skipped"], true);
    assert_eq!(quick["disk"]["images_total"], 0);
    assert_eq!(h.podman.calls_of("system").len(), dfs);
}

#[tokio::test]
async fn a_running_model_container_is_named_by_model_even_without_labels() {
    let h = Harness::new().await;
    let gw = common::serve(h.state.clone()).await;
    let a = "a".repeat(64);
    h.podman.add_image(&a, &[MOVING], &[]);
    store::insert_local_model(&h.state.db, &local_model("qwen", MOVING))
        .await
        .unwrap();
    h.state.reload_snapshot().await.unwrap();
    let prefix = h.state.snapshot().settings.container_prefix.clone();
    let name = lmgw_core::runtime::container_name(&prefix, lmgw_core::runtime::Class::Chat, "qwen");
    for (container, class, model) in [
        (name.as_str(), "", ""),
        ("lmgw-aux-embed-labelled", "aux", "embed"),
        ("someone-elses", "", ""),
    ] {
        h.podman.world().running.push(FakeContainer {
            name: container.into(),
            image_id: a.clone(),
            class: class.into(),
            model: model.into(),
        });
    }

    let list = op_ok(&gw, "container_images", json!({})).await;
    let users = list["images"][0]["used_by"].as_array().unwrap().clone();
    let running = |c: &str| {
        users
            .iter()
            .find(|u| u["kind"] == "running_container" && u["container"] == c)
            .unwrap_or_else(|| panic!("{c} not among {users:?}"))
            .clone()
    };
    let unlabelled = running(&name);
    assert_eq!(
        (unlabelled["class"].clone(), unlabelled["model_id"].clone()),
        (json!("chat"), json!("qwen"))
    );
    let labelled = running("lmgw-aux-embed-labelled");
    assert_eq!(
        (labelled["class"].clone(), labelled["model_id"].clone()),
        (json!("aux"), json!("embed"))
    );
    assert!(
        running("someone-elses")["model_id"].is_null(),
        "no model's container"
    );
}

#[tokio::test]
async fn the_editor_gets_edit_outcomes_and_free_disk() {
    let h = Harness::new().await;
    let gw = common::serve(h.state.clone()).await;

    let env = op_ok(&gw, "build_env", json!({})).await;
    assert!(env["builds_dir_free_bytes"].as_u64().unwrap() > 0, "{env}");
    assert_eq!(env["builds_dir"], h.builds_dir().display().to_string());

    let preview = op_ok(&gw, "build_resolve", json!({"spec": h.spec()})).await;
    let edits = preview["edits"].as_array().unwrap();
    let outcomes = preview["edit_outcomes"].as_array().unwrap();
    assert!(!edits.is_empty());
    assert_eq!(
        outcomes.len(),
        edits.len(),
        "one outcome per edit, in order"
    );
    for (i, (e, o)) in edits.iter().zip(outcomes).enumerate() {
        assert_eq!(o["index"], i);
        assert_eq!(
            (o["role"].clone(), o["required"].clone()),
            (e["role"].clone(), e["required"].clone())
        );
    }
    assert!(
        outcomes.iter().any(|o| o["applied"].as_u64().unwrap() > 0),
        "{outcomes:?}"
    );
    assert!(outcomes
        .iter()
        .filter(|o| o["required"] == true)
        .all(|o| o["applied"].as_u64().unwrap() > 0));
}

// ---------------------------------------------------------------------------
// Forge
// ---------------------------------------------------------------------------

const REPO: &str = "https://github.com/ggml-org/llama.cpp";

fn gh_pull(number: u64, title: &str) -> Value {
    let sha = |c: char| std::iter::repeat_n(c, 40).collect::<String>();
    json!({
        "number": number,
        "title": title,
        "user": {"login": "someone"},
        "updated_at": "2026-09-25T10:00:00Z",
        "draft": false,
        "state": "open",
        "head": {"sha": sha('a'), "ref": "feature"},
        "base": {"sha": sha('b'), "ref": "master"},
        "merged_at": null,
        "html_url": format!("{REPO}/pull/{number}"),
    })
}

fn auth_of(r: &Request) -> Option<String> {
    r.headers
        .get("authorization")
        .map(|v| v.to_str().unwrap().to_string())
}

async fn set_github_token(state: &lmgw_core::state::SharedState, token: &str) {
    let mut s = state.snapshot().settings.clone();
    s.forge_tokens.insert("github.com".into(), token.into());
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

#[tokio::test]
async fn forge_ops_and_the_run_lookup_use_the_gateway_client_and_the_current_token() {
    let gh = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/ggml-org/llama.cpp/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            gh_pull(16500, "model : add Gemma 4 vision"),
            gh_pull(16391, "CUDA: faster FA"),
        ])))
        .mount(&gh)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/ggml-org/llama.cpp/pulls/16391"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gh_pull(16391, "CUDA: faster FA")))
        .mount(&gh)
        .await;

    let state = AppState::init_for_tests().await.unwrap();
    // The mock, injected — not LMGW_GITHUB_API, which every parallel test
    // in this process would see.
    state
        .builds
        .set_forge_client(ForgeClient::new().unwrap().with_github_api(gh.uri()));
    set_github_token(&state, "ghp_first").await;
    let gw = common::serve(state.clone()).await;

    let page = op_ok(
        &gw,
        "forge_prs",
        json!({"repo_url": REPO, "forge": "github"}),
    )
    .await;
    let numbers: Vec<u64> = page["prs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["number"].as_u64().unwrap())
        .collect();
    assert_eq!(numbers, [16500, 16391]);
    let filtered = op_ok(
        &gw,
        "forge_prs",
        json!({"repo_url": REPO, "forge": "github", "query": "gemma"}),
    )
    .await;
    assert_eq!(filtered["prs"].as_array().unwrap().len(), 1);
    let pr = op_ok(
        &gw,
        "forge_pr",
        json!({"repo_url": REPO, "forge": "github", "number": 16391}),
    )
    .await;
    assert_eq!(pr["number"], 16391);
    assert_eq!(pr["head_sha"], "a".repeat(40));
    let err = op_err(
        &gw,
        "forge_prs",
        json!({"repo_url": "https://codeberg.org/a/b", "forge": "plain"}),
    )
    .await;
    assert!(err.contains("plain git repository"), "{err}");

    // The run's lookup, as installed at startup: the same client, and the
    // token as it is when asked — a token saved since is the one sent.
    state.builds.set_forge(Arc::new(GatewayForge::new(&state)));
    set_github_token(&state, "ghp_second").await;
    let looked = state
        .builds
        .forge()
        .pr(REPO, Forge::Github, 16391)
        .await
        .unwrap();
    assert_eq!(looked.number, 16391);

    let auths: Vec<Option<String>> = gh
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(auth_of)
        .collect();
    assert_eq!(auths.len(), 4, "{auths:?}");
    assert!(auths[..3]
        .iter()
        .all(|a| a.as_deref() == Some("Bearer ghp_first")));
    assert_eq!(auths[3].as_deref(), Some("Bearer ghp_second"));
}

// ---------------------------------------------------------------------------
// The self-admin tools
// ---------------------------------------------------------------------------

const ADMIN_TOKEN: &str = "backends-admin-token";

async fn admin_plane(h: &Harness, mode: SelfAdmin) -> String {
    h.settings(|s| s.self_admin = mode).await;
    lmgw_core::agents::token::set_owner_key(
        &h.state,
        lmgw_core::agents::token::OWNER_SELF_ADMIN,
        ADMIN_TOKEN,
        true,
    )
    .await
    .unwrap();
    common::serve(h.state.clone()).await.base
}

async fn rpc(base: &str, sid: Option<&str>, body: Value) -> (Option<String>, Value) {
    let mut req = reqwest::Client::new()
        .post(format!("{base}/mcp/admin"))
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {ADMIN_TOKEN}"))
        .json(&body);
    if let Some(sid) = sid {
        req = req.header("mcp-session-id", sid);
    }
    let resp = req.send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    (sid, resp.json().await.unwrap())
}

async fn session(base: &str) -> String {
    let (sid, body) = rpc(
        base,
        None,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                        "clientInfo": { "name": "backends-test", "version": "0" } }
        }),
    )
    .await;
    assert!(body["error"].is_null(), "{body}");
    sid.unwrap()
}

async fn tool_names(base: &str, sid: &str) -> Vec<String> {
    let (_, body) = rpc(
        base,
        Some(sid),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// `(isError, text)` of one tool call.
async fn tool(base: &str, sid: &str, name: &str, args: Value) -> (bool, String) {
    let (_, body) = rpc(
        base,
        Some(sid),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
               "params": {"name": name, "arguments": args}}),
    )
    .await;
    assert!(body["error"].is_null(), "{name}: {body}");
    let r = &body["result"];
    (
        r["isError"] == true,
        r["content"][0]["text"].as_str().unwrap().to_string(),
    )
}

async fn tool_ok(base: &str, sid: &str, name: &str, args: Value) -> Value {
    let (err, text) = tool(base, sid, name, args).await;
    assert!(!err, "{name} failed: {text}");
    serde_json::from_str(&text).unwrap()
}

const READS: [&str; 4] = [
    "lmgw__builds",
    "lmgw__build_log",
    "lmgw__container_images",
    "lmgw__forge_prs",
];
const WRITES: [&str; 5] = [
    "lmgw__build_set",
    "lmgw__build_run",
    "lmgw__build_check_merge",
    "lmgw__container_image_delete",
    "lmgw__container_image_pull",
];

/// A 300-line log: `build_run_log` reads it from an offset (the dashboard)
/// or its last lines (`tail`); `lmgw__build_log` defaults to the last 200
/// lines rather than the first megabyte, and refuses a typo'd argument.
#[tokio::test]
async fn the_log_reads_from_an_offset_or_its_tail() {
    let h = Harness::new().await;
    let id = h.build(&h.spec()).await;
    let run_id = seed_run(&h, id, &"a".repeat(64), &[], false).await;
    let log = h.root.join("run.log");
    let lines: Vec<String> = (1..=300).map(|n| format!("line {n}")).collect();
    let whole = lines.join("\n") + "\n";
    std::fs::write(&log, &whole).unwrap();
    store::update_build_run(
        &h.state.db,
        run_id,
        &BuildRunPatch {
            log_path: Some(log.display().to_string()),
            ..BuildRunPatch::default()
        },
    )
    .await
    .unwrap();
    let len = whole.len() as u64;

    let gw = common::serve(h.state.clone()).await;
    let tail = op_ok(&gw, "build_run_log", json!({"run_id": run_id, "tail": 2})).await;
    assert_eq!(tail["text"], "line 299\nline 300\n");
    assert_eq!(tail["next_offset"], len);
    assert_eq!(tail["done"], true);
    let from = op_ok(&gw, "build_run_log", json!({"run_id": run_id, "offset": 0})).await;
    assert_eq!(from["text"], whole.as_str());
    let e = op_err(
        &gw,
        "build_run_log",
        json!({"run_id": run_id, "offset": 5, "tail": 2}),
    )
    .await;
    assert!(e.contains("not both"), "{e}");
    let e = op_err(&gw, "build_run_log", json!({"run_id": run_id, "ofset": 5})).await;
    assert!(e.contains("ofset"), "{e}");

    let base = admin_plane(&h, SelfAdmin::ReadOnly).await;
    let sid = session(&base).await;
    let last = tool_ok(&base, &sid, "lmgw__build_log", json!({"run_id": run_id})).await;
    let text = last["text"].as_str().unwrap();
    assert_eq!(text.lines().count(), 200);
    assert!(text.starts_with("line 101\n") && text.ends_with("line 300\n"));
    assert_eq!(last["next_offset"], len);
    let one = tool_ok(
        &base,
        &sid,
        "lmgw__build_log",
        json!({"run_id": run_id, "tail": 1}),
    )
    .await;
    assert_eq!(one["text"], "line 300\n");
    let all = tool_ok(
        &base,
        &sid,
        "lmgw__build_log",
        json!({"run_id": run_id, "offset": 0}),
    )
    .await;
    assert_eq!(all["text"], whole.as_str());
    let (refused, msg) = tool(
        &base,
        &sid,
        "lmgw__build_log",
        json!({"run_id": run_id, "offest": 0}),
    )
    .await;
    assert!(
        refused && msg.contains("unknown argument 'offest'"),
        "{msg}"
    );
    let (refused, msg) = tool(
        &base,
        &sid,
        "lmgw__build_log",
        json!({"run_id": run_id, "offset": 0, "tail": 1}),
    )
    .await;
    assert!(refused && msg.contains("not both"), "{msg}");
}

#[tokio::test]
async fn read_only_lists_the_build_reads_and_refuses_the_writes() {
    let h = Harness::new().await;
    let id = h.build(&h.spec()).await;
    let base = admin_plane(&h, SelfAdmin::ReadOnly).await;
    let sid = session(&base).await;

    let names = tool_names(&base, &sid).await;
    for r in READS {
        assert!(names.iter().any(|n| n == r), "{r} missing at read_only");
    }
    for w in WRITES {
        assert!(!names.iter().any(|n| n == w), "{w} listed at read_only");
    }

    let list = tool_ok(&base, &sid, "lmgw__builds", json!({})).await;
    assert_eq!(list["builds"][0]["build"]["id"], id);
    let presets: Vec<&str> = list["repo_presets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["preset"].as_str().unwrap())
        .collect();
    assert_eq!(presets, ["official", "ik", "audio", "sdcpp"]);
    let one = tool_ok(&base, &sid, "lmgw__builds", json!({"id": id})).await;
    assert_eq!(one["view"]["build"]["slug"], "official-master");
    assert_eq!(one["more"], false);
    let images = tool_ok(&base, &sid, "lmgw__container_images", json!({})).await;
    assert_eq!(images["images"], json!([]));

    let (refused, msg) = tool(&base, &sid, "lmgw__build_run", json!({"id": id})).await;
    assert!(refused);
    assert!(msg.contains("read_only") && msg.contains("full"), "{msg}");
    assert!(store::list_build_runs(&h.state.db, Some(id), 0)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn full_defines_runs_and_reads_a_build_through_the_tools() {
    let h = Harness::new().await;
    let base = admin_plane(&h, SelfAdmin::Full).await;
    let sid = session(&base).await;
    let names = tool_names(&base, &sid).await;
    for t in READS.iter().chain(WRITES.iter()) {
        assert!(names.iter().any(|n| n == t), "{t} missing at full");
    }

    // A preset fills the repository; the fields passed win.
    let from_preset = tool_ok(
        &base,
        &sid,
        "lmgw__build_set",
        json!({"action": "create", "preset": "ik", "slug": "ik-main",
               "extras": "pr 1234\nref https://github.com/fork/ik_llama.cpp feature",
               "arch": "86, 89", "keep_runs": 2}),
    )
    .await;
    let b = &from_preset["build"];
    assert_eq!(b["repo_url"], "https://github.com/ikawrakow/ik_llama.cpp");
    assert_eq!(
        (b["forge"].clone(), b["ref"].clone()),
        (json!("github"), json!("main"))
    );
    assert_eq!(b["extras"][0], json!({"kind": "pr", "number": 1234}));
    assert_eq!(b["extras"][1]["ref"], "feature");
    assert_eq!(b["arch"], json!(["86", "89"]));
    assert_eq!(b["keep_runs"], 2);
    assert!(from_preset["next_step"]
        .as_str()
        .unwrap()
        .contains("lmgw__build_run"));
    let ik = b["id"].as_i64().unwrap();
    let (bad, msg) = tool(
        &base,
        &sid,
        "lmgw__build_set",
        json!({"action": "update", "id": ik, "extras": "pr 0"}),
    )
    .await;
    assert!(bad && msg.contains("extras[0]"), "{msg}");
    let partial = tool_ok(
        &base,
        &sid,
        "lmgw__build_set",
        json!({"action": "update", "id": ik, "keep_runs": -1, "arch": "auto"}),
    )
    .await;
    assert!(partial["build"]["keep_runs"].is_null());
    assert!(partial["build"]["arch"].is_null());
    assert_eq!(
        partial["build"]["extras"].as_array().unwrap().len(),
        2,
        "kept"
    );

    // A build of the local fixture repository, run from the tool plane.
    let local = tool_ok(
        &base,
        &sid,
        "lmgw__build_set",
        json!({"action": "create", "slug": "official-master", "engine": "llama",
               "repo_url": h.upstream.url(), "ref": "master", "arch": "89"}),
    )
    .await;
    assert_eq!(
        local["build"]["forge"], "plain",
        "a file:// repo has no forge"
    );
    let id = local["build"]["id"].as_i64().unwrap();
    let started = tool_ok(&base, &sid, "lmgw__build_run", json!({"id": id})).await;
    let run_id = started["run_id"].as_i64().unwrap();
    assert!(started["next_step"]
        .as_str()
        .unwrap()
        .contains(&format!("lmgw__build_log run_id={run_id}")));
    let r = wait_run(&h.state, run_id).await;
    assert_eq!(r.status, BuildRunStatus::Succeeded, "{:?}", r.error);
    assert_eq!(r.trigger, BuildTrigger::Mcp);
    let log = tool_ok(&base, &sid, "lmgw__build_log", json!({"run_id": run_id})).await;
    assert!(
        log["text"].as_str().unwrap().contains("(mcp)"),
        "{}",
        log["text"]
    );
    let check = tool_ok(&base, &sid, "lmgw__build_check_merge", json!({"id": id})).await;
    assert_eq!(check["ok"], true);
    assert_eq!(check["steps"], json!([]));

    let deleted = tool_ok(
        &base,
        &sid,
        "lmgw__build_set",
        json!({"action": "delete", "id": ik}),
    )
    .await;
    assert!(deleted["build"].is_null());
}
