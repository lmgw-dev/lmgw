//! Reading a manifest out of an image, without podman.
//!
//! The fake spawner answers the three invocations §3.4 prescribes and records
//! them in order, so the *sequence* is the assertion — `create`, `cp`, `rm -f`
//! — and every failure path is checked for the `rm -f` that has to run anyway.
//! `tests/it/agents_package.rs` drives the ops on top of this, and its
//! `the_real_thing_*` half against a real locally built image.

use super::*;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::agents::container::{Spawned, Spawner};
use crate::runtime::registry::CmdOutput;
use crate::state::AppState;
use crate::store::AgentRow;

static NEXT_PREFIX: AtomicU32 = AtomicU32::new(0);

async fn gateway() -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.container_prefix = format!(
        "lmgwpkg{}-{}",
        std::process::id(),
        NEXT_PREFIX.fetch_add(1, Ordering::Relaxed)
    );
    crate::store::save_settings(&state.db, &settings)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    state
}

/// What podman says for each verb, and what it recorded being asked.
#[derive(Default)]
struct Fake {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    /// Written to the `podman cp` destination, standing in for the file the
    /// real thing copies out of the image.
    manifest: Option<String>,
    /// `podman image exists` answers "no".
    absent: bool,
    /// `podman create` fails.
    create_fails: bool,
    digest: String,
}

impl Fake {
    fn with_manifest(text: &str) -> Self {
        Self {
            manifest: Some(text.to_string()),
            digest: "sha256:aaa".into(),
            ..Default::default()
        }
    }
    fn calls(&self) -> Arc<Mutex<Vec<Vec<String>>>> {
        self.calls.clone()
    }
}

fn ok(stdout: &str) -> std::io::Result<CmdOutput> {
    Ok(CmdOutput {
        status: 0,
        stdout: stdout.to_string(),
        stderr: String::new(),
    })
}

fn failed(status: i32, stderr: &str) -> std::io::Result<CmdOutput> {
    Ok(CmdOutput {
        status,
        stdout: String::new(),
        stderr: stderr.to_string(),
    })
}

#[async_trait]
impl Spawner for Fake {
    async fn spawn(&self, _p: &str, _a: &[String]) -> std::io::Result<Spawned> {
        panic!("reading a package must never start anything");
    }

    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        assert_eq!(program, "podman");
        self.calls.lock().unwrap().push(args.to_vec());
        match args.first().map(String::as_str) {
            // `podman image exists` answers with its exit code alone. The
            // reference is the **last** argument, after the `--` terminator.
            Some("image") if args.get(1).map(String::as_str) == Some("exists") => {
                if self.absent {
                    failed(1, "")
                } else {
                    ok("")
                }
            }
            Some("image") => ok(&self.digest),
            Some("create") if self.create_fails => failed(
                125,
                "Error: short-name resolution enforced but cannot prompt",
            ),
            Some("create") => ok("6158a95fea92\n"),
            Some("cp") => match &self.manifest {
                Some(text) => {
                    let dest = args.last().expect("cp has a destination");
                    std::fs::write(dest, text).expect("the run dir is writable");
                    ok("")
                }
                None => failed(
                    125,
                    "Error: \"/lmgw/agent.json\" could not be found on container \
                     lmgw-pkg-x: no such file or directory",
                ),
            },
            _ => ok(""),
        }
    }
}

const DOC: &str = r#"{
  "schema_version": 1,
  "id": "labeler",
  "name": "Labeler",
  "model": { "alias": "m1" },
  "run": { "kind": "container", "image": "localhost/labeler:1" }
}"#;

fn verbs(calls: &Arc<Mutex<Vec<Vec<String>>>>) -> Vec<String> {
    calls.lock().unwrap().iter().map(|a| a.join(" ")).collect()
}

// ---------------------------------------------------------------------------
// create → cp → rm (§3.4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_manifest_is_read_with_create_cp_rm_and_nothing_is_started() {
    let state = gateway().await;
    let fake = Arc::new(Fake::with_manifest(DOC));
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);

    let text = read_manifest(&state, "localhost/labeler:1")
        .await
        .expect("the image carries a manifest");
    assert_eq!(text, DOC);

    let seen = verbs(&calls);
    assert_eq!(seen.len(), 3, "{seen:?}");
    // The container is created and never run: `--pull=never`, because the
    // download question was already settled (and said out loud) by
    // `ensure_image`.
    assert!(
        seen[0].starts_with("create --pull=never --name lmgw-pkg-")
            && seen[0].ends_with("-- localhost/labeler:1"),
        "{seen:?}"
    );
    // Labelled like every other container this instance makes, so a SIGKILL
    // between the create and the rm leaves something boot reconciliation can
    // see. `lmgw.run=package` is deliberately not a job id.
    let prefix = state.snapshot().settings.container_prefix.clone();
    for label in [
        format!("--label lmgw.instance={prefix}"),
        "--label lmgw.kind=agent".to_string(),
        "--label lmgw.run=package".to_string(),
    ] {
        assert!(seen[0].contains(&label), "{label} missing from {seen:?}");
    }
    assert!(
        seen[1].starts_with("cp lmgw-pkg-") && seen[1].contains(":/lmgw/agent.json "),
        "{seen:?}"
    );
    assert!(seen[1].contains("/pkg-lmgw-pkg-"), "{seen:?}");
    assert!(seen[2].starts_with("rm -f lmgw-pkg-"), "{seen:?}");
    // Same throwaway container across all three.
    let name = seen[2].trim_start_matches("rm -f ").to_string();
    assert!(
        seen[0].contains(&name) && seen[1].contains(&name),
        "{seen:?}"
    );
}

#[tokio::test]
async fn an_image_without_the_manifest_says_where_it_looked_and_still_removes_the_container() {
    let state = gateway().await;
    let fake = Arc::new(Fake::default());
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);

    let e = read_manifest(&state, "docker.io/library/alpine:3")
        .await
        .expect_err("alpine is not an agent package");
    assert_eq!(e.code, "package_no_manifest");
    assert!(
        e.message
            .contains("carries no /lmgw/agent.json; an lmgw agent package puts its manifest there"),
        "{}",
        e.message
    );
    // podman's own words are kept after lmgw's, so a path typo stays legible.
    assert!(e.message.contains("could not be found on container"), "{e}");
    let seen = verbs(&calls);
    assert!(seen.last().unwrap().starts_with("rm -f "), "{seen:?}");
}

#[tokio::test]
async fn a_create_that_fails_is_reported_and_nothing_is_left_to_remove() {
    let state = gateway().await;
    let fake = Arc::new(Fake {
        create_fails: true,
        ..Default::default()
    });
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);

    let e = read_manifest(&state, "localhost/nope:1")
        .await
        .expect_err("create failed");
    assert_eq!(e.code, "package_create_failed");
    assert!(e.message.contains("short-name resolution"), "{e}");
    // Nothing was created, so nothing is removed: the `rm` is for a container
    // that exists, not a reflex.
    let seen = verbs(&calls);
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0].starts_with("create --pull=never --name "),
        "{seen:?}"
    );
}

#[tokio::test]
async fn the_run_directory_the_read_used_is_gone_afterwards() {
    let state = gateway().await;
    let fake = Arc::new(Fake::with_manifest(DOC));
    state.set_agent_spawner_for_tests(fake);
    read_manifest(&state, "localhost/labeler:1").await.unwrap();

    let prefix = state.snapshot().settings.container_prefix.clone();
    let (root, _) = container::runs_root(&state.data_dir, &prefix);
    let left: Vec<String> = std::fs::read_dir(&root)
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(left.is_empty(), "left behind: {left:?}");
}

// ---------------------------------------------------------------------------
// The pull policy (§4.1), which is the caller's visible choice
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_absent_image_under_pull_never_is_refused_without_downloading() {
    let state = gateway().await;
    let fake = Arc::new(Fake {
        absent: true,
        ..Default::default()
    });
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);

    let e = ensure_image(&state, "docker.io/acme/agent:1", PullPolicy::Never)
        .await
        .expect_err("pull is never");
    assert_eq!(e.code, "image_absent_pull_never");
    assert!(e.message.contains("install with pull 'missing'"), "{e}");
    assert_eq!(verbs(&calls), ["image exists -- docker.io/acme/agent:1"]);
}

#[tokio::test]
async fn pull_missing_downloads_only_what_is_absent_and_always_downloads_regardless() {
    // `missing`, image absent → one pull.
    let state = gateway().await;
    let fake = Arc::new(Fake {
        absent: true,
        ..Default::default()
    });
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    assert!(ensure_image(&state, "acme/a:1", PullPolicy::Missing)
        .await
        .unwrap());
    assert_eq!(
        verbs(&calls),
        ["image exists -- acme/a:1", "pull -- acme/a:1"]
    );

    // `missing`, image present → no pull at all.
    let state = gateway().await;
    let fake = Arc::new(Fake::default());
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    assert!(!ensure_image(&state, "acme/a:1", PullPolicy::Missing)
        .await
        .unwrap());
    assert_eq!(verbs(&calls), ["image exists -- acme/a:1"]);

    // `always` → straight to the pull, present or not. Whether anything was
    // **downloaded** is the digest before and after, not the policy: an image
    // that has not moved is `pulled: false`, because reporting a download that
    // did not happen is lmgw inventing an event.
    let state = gateway().await;
    let fake = Arc::new(Fake::with_manifest(DOC));
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    assert!(!ensure_image(&state, "acme/a:1", PullPolicy::Always)
        .await
        .unwrap());
    assert_eq!(
        verbs(&calls),
        [
            "image inspect --format {{.Digest}} -- acme/a:1",
            "pull -- acme/a:1",
            "image inspect --format {{.Digest}} -- acme/a:1"
        ]
    );

    // `always` on an image that was not here at all: no digest before, one
    // after — an arrival, and the report says so.
    let state = gateway().await;
    let fake = Arc::new(Fake::default());
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    assert!(ensure_image(&state, "acme/a:1", PullPolicy::Always)
        .await
        .unwrap());
    // Short-circuited: "there was no digest before" already settles it, so the
    // second inspect is not asked for.
    assert_eq!(
        verbs(&calls),
        [
            "image inspect --format {{.Digest}} -- acme/a:1",
            "pull -- acme/a:1"
        ]
    );
}

#[tokio::test]
async fn an_image_reference_that_reads_as_a_flag_is_refused_before_podman_sees_it() {
    let state = gateway().await;
    let fake = Arc::new(Fake::with_manifest(DOC));
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    // `podman image exists --help` exits 0, so without this guard `--help`
    // would be "present" and everything after it would diagnose the wrong
    // thing.
    for bad in ["--help", "-f", "acme/a:1 --rm", ""] {
        let e = ensure_image(&state, bad, PullPolicy::Missing)
            .await
            .expect_err("{bad} is not an image reference");
        assert_eq!(e.code, "image_ref_invalid", "{bad}: {e}");
        let e = read_manifest(&state, bad).await.expect_err("same");
        assert_eq!(e.code, "image_ref_invalid", "{bad}: {e}");
        assert_eq!(image_digest(&state, bad).await, None, "{bad}");
    }
    assert!(
        verbs(&calls).is_empty(),
        "podman was asked: {:?}",
        verbs(&calls)
    );
}

#[tokio::test]
async fn a_digest_podman_cannot_report_is_none_rather_than_a_placeholder() {
    let state = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(Fake::default()));
    assert_eq!(image_digest(&state, "acme/a:1").await, None);

    let state = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(Fake::with_manifest(DOC)));
    assert_eq!(
        image_digest(&state, "acme/a:1").await,
        Some("sha256:aaa".to_string())
    );
}

// ---------------------------------------------------------------------------
// Provenance and portability — pure
// ---------------------------------------------------------------------------

fn agent(manifest: &str, provenance: &str, dev_url: Option<&str>) -> Agent {
    Agent::from_row(AgentRow {
        id: "labeler".into(),
        manifest: manifest.into(),
        config: "{}".into(),
        enabled: true,
        source: "imported".into(),
        provenance: provenance.into(),
        dev_url: dev_url.map(str::to_string),
        created_at: String::new(),
        updated_at: String::new(),
    })
    .expect("the fixture manifest parses")
}

#[test]
fn a_row_with_no_package_has_no_provenance_and_an_unreadable_one_is_not_fatal() {
    let a = agent(DOC, "{}", None);
    assert!(Provenance::of_row(&a.row).is_empty());
    // A column a newer build wrote: no provenance, not a broken agent.
    let a = agent(DOC, "not json at all", None);
    assert!(Provenance::of_row(&a.row).is_empty());
}

#[test]
fn provenance_round_trips_through_the_column() {
    let p = Provenance {
        image: "localhost/labeler:1".into(),
        digest: "sha256:beef".into(),
        manifest_path: MANIFEST_INSIDE.into(),
        installed_at: "2026-09-19T07:00:00Z".into(),
        pulled_at: "2026-09-19T08:00:00Z".into(),
    };
    let a = agent(DOC, &p.to_json(), None);
    assert_eq!(Provenance::of_row(&a.row), p);
    assert!(!p.is_empty());
}

#[test]
fn the_image_acted_on_is_the_manifests_with_provenance_as_the_fallback() {
    let with_image = agent(DOC, r#"{"image":"localhost/other:9"}"#, None);
    assert_eq!(
        image_of(&with_image).as_deref(),
        Some("localhost/labeler:1"),
        "the manifest's image is the one every phase runs"
    );

    const NO_IMAGE: &str = r#"{
      "schema_version": 1, "id": "labeler", "name": "L",
      "model": { "alias": "m1" },
      "run": { "kind": "container", "service": { "port": 5173 } }
    }"#;
    let dev_only = agent(NO_IMAGE, r#"{"image":"localhost/other:9"}"#, None);
    assert_eq!(image_of(&dev_only).as_deref(), Some("localhost/other:9"));
    assert_eq!(image_of(&agent(NO_IMAGE, "{}", None)), None);
}

#[test]
fn portability_names_a_local_image_a_dev_url_and_a_missing_image() {
    let (portable, notes) = portability(&agent(DOC, "{}", None), false);
    assert!(!portable);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("'localhost/labeler:1' is local to the machine"),
        "{notes:?}"
    );

    const REMOTE: &str = r#"{
      "schema_version": 1, "id": "labeler", "name": "L",
      "model": { "alias": "m1" },
      "run": { "kind": "container", "image": "registry.example.com/acme/labeler:1",
               "service": { "port": 8080 } }
    }"#;
    let (portable, notes) = portability(&agent(REMOTE, "{}", None), false);
    assert!(portable, "{notes:?}");
    assert!(notes.is_empty());

    // A dev_url is never in the file, which is exactly why the line has to say
    // the agent works here for a reason the receiver will not get — and why the
    // **exported** wording must not name the address. Redacted for the file,
    // named on the owner's own dashboard.
    let dev = agent(REMOTE, "{}", Some("http://127.0.0.1:5173"));
    let (portable, notes) = portability(&dev, false);
    assert!(!portable);
    assert!(
        notes[0].contains("dev server at http://127.0.0.1:5173"),
        "{notes:?}"
    );
    let (portable, notes) = portability(&dev, true);
    assert!(!portable);
    assert!(
        notes[0].contains("a dev server on the exporting machine"),
        "{notes:?}"
    );
    assert!(
        !notes.iter().any(|n| n.contains("127.0.0.1:5173")),
        "the export must not carry the dev server's address: {notes:?}"
    );

    const NO_IMAGE: &str = r#"{
      "schema_version": 1, "id": "labeler", "name": "L",
      "model": { "alias": "m1" },
      "run": { "kind": "container", "service": { "port": 5173 } }
    }"#;
    let (portable, notes) = portability(&agent(NO_IMAGE, "{}", None), false);
    assert!(!portable);
    assert!(notes[0].contains("names no run.image"), "{notes:?}");
}
