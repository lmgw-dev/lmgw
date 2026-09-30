//! The fake podman, fixture repositories and harness the container-build
//! suites share (`backends_run.rs`, `backends_ops.rs`): real git against local
//! fixture repositories, and a fake podman that is both the registry's
//! `CommandRunner` and the agent `Spawner` — it keeps a small image store,
//! replays a scripted `podman build`, and answers the verify probes. Nothing
//! here touches the real podman, the real build lock or `/var/tmp`: each
//! harness has its own lock file, scratch root and builds dir, all under
//! `CARGO_TARGET_TMPDIR` (a disk — the executor refuses a builds dir on
//! tmpfs, as it should).
//!
//! Reached as `crate::support::backends_fake` by each suite, so what one of
//! them does not use is dead code from that binary's point of view.

#![allow(dead_code)]

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use lmgw_core::agents::container::{Exit, KillHandle, Signal, Spawned, Spawner};
use lmgw_core::backends::run;
use lmgw_core::backends::validate::validate_build;
use lmgw_core::backends::{BuildRun, BuildSpec, Engine, Forge};
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner, Registry};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot, Notify};

pub const OFFICIAL_CUDA: &str = include_str!("../../fixtures/dockerfiles/official-cuda.Dockerfile");

// ---------------------------------------------------------------------------
// Fixture repositories (as in backends_git.rs: an isolated git, fixed dates)
// ---------------------------------------------------------------------------

pub fn fixture_git(dir: &Path, args: &[&str], date: &str) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "init.defaultBranch=master",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "fixture git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub struct Repo {
    pub dir: PathBuf,
    pub clock: Cell<u64>,
}

impl Repo {
    pub fn new(dir: PathBuf, bare: bool) -> Self {
        std::fs::create_dir_all(&dir).unwrap();
        let r = Self {
            dir,
            clock: Cell::new(1_790_000_000),
        };
        if bare {
            r.git(&["init", "--quiet", "--bare"]);
        } else {
            r.git(&["init", "--quiet"]);
        }
        r
    }

    pub fn git(&self, args: &[&str]) -> String {
        let t = self.clock.get() + 60;
        self.clock.set(t);
        fixture_git(&self.dir, args, &format!("@{t} +0000"))
    }

    pub fn url(&self) -> String {
        format!("file://{}", self.dir.display())
    }

    pub fn write(&self, path: &str, content: &str) {
        let p = self.dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    pub fn commit(&self, msg: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "--quiet", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }

    pub fn push(&self, remote: &Repo, refspec: &str) {
        self.git(&["push", "--quiet", "--force", &remote.url(), refspec]);
    }
}

// ---------------------------------------------------------------------------
// The fake podman
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct FakeImage {
    pub id: String,
    pub names: Vec<String>,
    pub labels: HashMap<String, String>,
    pub size: u64,
    /// podman's `RepoDigests` (`repo@sha256:…`) — a registry pull has them.
    pub digests: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FakeContainer {
    pub name: String,
    pub image_id: String,
    pub class: String,
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    Succeed,
    /// Print the progress lines, then wait for [`Inner::release`].
    HoldThenSucceed,
    /// Print the progress lines, then wait for a signal; exit 1 on it.
    HoldUntilSignal,
    /// Print the progress lines, wait for a signal — and finish the image as
    /// it arrives (the build was committing when the cancel went out).
    HoldThenFinishOnSignal,
}

pub struct Spawn {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub containerfile: String,
    pub ignorefile: String,
}

/// A container `podman ps -a --external` lists besides [`World::running`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeExternal {
    pub id: String,
    pub name: String,
    pub state: String,
    /// The image it was created from; empty prints no `ImageID`.
    pub image_id: String,
}

impl FakeExternal {
    pub fn new(id: &str, name: &str, state: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            state: state.into(),
            image_id: String::new(),
        }
    }

    pub fn on_image(mut self, image_id: &str) -> Self {
        self.image_id = image_id.into();
        self
    }
}

pub struct World {
    pub images: Vec<FakeImage>,
    pub running: Vec<FakeContainer>,
    pub external: Vec<FakeExternal>,
    /// `podman tag <src> <dst>` fails for these `dst`s.
    pub fail_tag: Vec<String>,
    pub calls: Vec<Vec<String>>,
    pub devices_out: String,
    pub next: u64,
    /// What `podman pull <name>` brings: the image ID and `RepoDigests` the
    /// registry would hand over. A name not here fails the pull.
    pub remote: HashMap<String, (String, Vec<String>)>,
}

impl World {
    pub fn find(&self, r: &str) -> Option<usize> {
        let bare = r.trim_start_matches("sha256:");
        self.images.iter().position(|i| {
            i.names.iter().any(|n| n == r) || (bare.len() >= 12 && i.id.starts_with(bare))
        })
    }

    pub fn tag(&mut self, src: &str, dst: &str) -> bool {
        let Some(i) = self.find(src) else {
            return false;
        };
        for img in &mut self.images {
            img.names.retain(|n| n != dst);
        }
        self.images[i].names.push(dst.to_string());
        true
    }

    pub fn id_of(&self, r: &str) -> Option<String> {
        self.find(r).map(|i| self.images[i].id.clone())
    }
}

pub struct Inner {
    pub world: Mutex<World>,
    pub behavior: Mutex<Behavior>,
    pub release: Notify,
    pub signals: Mutex<Vec<Signal>>,
    pub spawns: Mutex<Vec<Spawn>>,
    pub buildah_tmp: PathBuf,
}

#[derive(Clone)]
pub struct FakePodman(pub Arc<Inner>);

pub fn ok(stdout: impl Into<String>) -> CmdOutput {
    CmdOutput {
        status: 0,
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

pub fn fail(status: i32, stderr: &str) -> CmdOutput {
    CmdOutput {
        status,
        stdout: String::new(),
        stderr: stderr.into(),
    }
}

/// A `--help` long enough for `Registry::help_text` to cache.
pub fn llama_help() -> String {
    let mut s = String::from("----- common params -----\n\n--port PORT  port to listen\n");
    for i in 0..40 {
        s.push_str(&format!("--flag-{i} N    something useful\n"));
    }
    s
}

pub const PROGRESS: [&str; 3] = [
    "[1/2] STEP 1/3: FROM docker.io/nvidia/cuda:13.0.0-devel-ubuntu24.04 AS build",
    "[ 45%] Building CXX object ggml/src/ggml.cpp.o",
    "[20/40] Building CUDA object ggml/src/ggml-cuda/fattn.cu.o",
];

impl FakePodman {
    pub fn new(buildah_tmp: PathBuf) -> Self {
        Self(Arc::new(Inner {
            world: Mutex::new(World {
                images: Vec::new(),
                running: Vec::new(),
                external: vec![
                    FakeExternal::new("old1", "old-working-container", "storage"),
                    FakeExternal::new("exited1", "bridges_db_1", "exited"),
                ],
                fail_tag: Vec::new(),
                calls: Vec::new(),
                devices_out: "Available devices:\n  CUDA0: NVIDIA GeForce RTX 4090 (24080 MiB, \
                              23000 MiB free)\n"
                    .into(),
                next: 0,
                remote: HashMap::new(),
            }),
            behavior: Mutex::new(Behavior::Succeed),
            release: Notify::new(),
            signals: Mutex::new(Vec::new()),
            spawns: Mutex::new(Vec::new()),
            buildah_tmp,
        }))
    }

    pub fn world(&self) -> std::sync::MutexGuard<'_, World> {
        self.0.world.lock().unwrap()
    }

    pub fn set_behavior(&self, b: Behavior) {
        *self.0.behavior.lock().unwrap() = b;
    }

    pub fn spawn_count(&self) -> usize {
        self.0.spawns.lock().unwrap().len()
    }

    pub fn calls_of(&self, verb: &str) -> Vec<Vec<String>> {
        self.world()
            .calls
            .iter()
            .filter(|c| c.first().map(String::as_str) == Some(verb))
            .cloned()
            .collect()
    }

    pub fn add_image(&self, id: &str, names: &[&str], labels: &[(&str, &str)]) {
        self.world().images.push(FakeImage {
            id: id.into(),
            names: names.iter().map(|n| n.to_string()).collect(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            size: 1_000_000,
            digests: Vec::new(),
        });
    }

    /// An image as a registry pull leaves it: its names and `RepoDigests`.
    pub fn add_pulled(&self, id: &str, names: &[&str], digests: &[&str]) {
        self.world().images.push(FakeImage {
            id: id.into(),
            names: names.iter().map(|n| n.to_string()).collect(),
            digests: digests.iter().map(|d| d.to_string()).collect(),
            size: 4_500_000_000,
            ..FakeImage::default()
        });
    }
}

#[async_trait]
impl CommandRunner for FakePodman {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        if program == "nvidia-smi" {
            return Ok(match a.as_slice() {
                [] => ok("| NVIDIA-SMI 615.71   Driver Version: 615.71   CUDA Version: 13.1 |"),
                _ => ok("8.9\n"),
            });
        }
        assert_eq!(program, "podman");
        let mut w = self.world();
        w.calls.push(args.to_vec());
        Ok(match a.as_slice() {
            ["image", "inspect", "--format", fmt, r] => match w.find(r) {
                None => fail(125, "Error: failed to find image x: image not known"),
                Some(i) => {
                    let img = &w.images[i];
                    match *fmt {
                        "{{.Id}}" => ok(format!("{}\n", img.id)),
                        "{{.Size}}" => ok(format!("{}\n", img.size)),
                        "{{json .RepoTags}}" => ok(serde_json::to_string(&img.names).unwrap()),
                        "{{json .RepoDigests}}" => ok(serde_json::to_string(&img.digests).unwrap()),
                        _ => ok(format!("{} [\"/app/llama-server\"]\n", img.id)),
                    }
                }
            },
            ["tag", _, dst] if w.fail_tag.iter().any(|t| t == dst) => {
                fail(125, "Error: tag failed (injected)")
            }
            ["tag", src, dst] => {
                if w.tag(src, dst) {
                    ok("")
                } else {
                    fail(125, "image not known")
                }
            }
            ["untag", img, tag] => match w.find(img) {
                Some(i) => {
                    w.images[i].names.retain(|n| n != tag);
                    ok("")
                }
                None => fail(125, "image not known"),
            },
            ["rmi", refs @ ..] => {
                let mut out = Vec::new();
                for r in refs {
                    let Some(i) = w.find(r) else {
                        return Ok(fail(1, "image not known"));
                    };
                    // As podman: the image itself goes (not only a tag of it)
                    // only once no container, running or not, uses it.
                    let img = &w.images[i];
                    let whole = !img.names.iter().any(|n| n == r) || img.names.len() == 1;
                    let holder = w
                        .running
                        .iter()
                        .map(|c| (&c.image_id, &c.name))
                        .chain(w.external.iter().map(|c| (&c.image_id, &c.id)))
                        .find(|(id, _)| **id == img.id)
                        .map(|(_, c)| c.clone());
                    if let (true, Some(c)) = (whole, holder) {
                        return Ok(fail(
                            2,
                            &format!(
                                "Error: image used by {c}: image is in use by a container: \
                                 consider listing external containers and force-removing image"
                            ),
                        ));
                    }
                    if w.images[i].names.iter().any(|n| n == r) {
                        w.images[i].names.retain(|n| n != r);
                        out.push(format!("Untagged: {r}"));
                        if w.images[i].names.is_empty() {
                            let img = w.images.remove(i);
                            out.push(format!("Deleted: {}", img.id));
                        }
                    } else {
                        let img = w.images.remove(i);
                        out.extend(img.names.iter().map(|n| format!("Untagged: {n}")));
                        out.push(format!("Deleted: {}", img.id));
                    }
                }
                ok(out.join("\n"))
            }
            ["ps", "-a", "--external", "-q"] => ok(w
                .external
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>()
                .join("\n")),
            // Every container, as podman prints it: the running ones too.
            ["ps", "-a", "--external", "--format", "json"] => ok(serde_json::to_string(
                &w.running
                    .iter()
                    .map(|c| {
                        json!({
                            "Id": format!("{:0>64}", c.name.len()),
                            "ImageID": c.image_id,
                            "Names": [c.name],
                            "Labels": {"lmgw.class": c.class, "lmgw.model": c.model},
                            "State": "running",
                        })
                    })
                    .chain(w.external.iter().map(|c| {
                        json!({
                            "Id": c.id,
                            "ImageID": c.image_id,
                            "Names": [c.name],
                            "State": c.state,
                            "Status": c.state,
                        })
                    }))
                    .collect::<Vec<_>>(),
            )
            .unwrap()),
            ["rm", "-f", ids @ ..] => {
                w.external
                    .retain(|c| !ids.contains(&c.id.as_str()) && !ids.contains(&c.name.as_str()));
                w.running.retain(|c| !ids.contains(&c.name.as_str()));
                ok("")
            }
            ["unshare", "rm", "-rf", "--", dir] => {
                let _ = std::fs::remove_dir_all(dir);
                ok("")
            }
            // As podman 5 prints it: the whole row once per tag (an untagged
            // image once), which lmgw must merge back into one image.
            ["images", "--format", "json"] => ok(serde_json::to_string(
                &w.images
                    .iter()
                    .flat_map(|i| std::iter::repeat_n(i, i.names.len().max(1)))
                    .map(|i| {
                        json!({
                            "Id": i.id,
                            "Names": if i.names.is_empty() { Value::Null } else { json!(i.names) },
                            "RepoDigests": i.digests,
                            "Size": i.size,
                            "Created": 1_790_000_000,
                            "Labels": if i.labels.is_empty() { Value::Null } else { json!(i.labels) },
                        })
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap()),
            ["system", "df", "--format", "json"] => ok(
                r#"[{"Type":"Images","Total":3,"RawSize":3000000,"RawReclaimable":1000000},
                    {"Type":"Containers","Total":0,"RawSize":0,"RawReclaimable":0}]"#,
            ),
            ["run", ..] => {
                let e = a.iter().position(|x| *x == "--entrypoint").expect("an entrypoint");
                let probe = &a[e + 3..];
                if probe.contains(&"--help") {
                    ok(llama_help())
                } else if probe.contains(&"--list-devices") {
                    ok(w.devices_out.clone())
                } else {
                    ok("")
                }
            }
            _ => ok(""),
        })
    }
}

#[async_trait]
impl Spawner for FakePodman {
    async fn spawn(&self, program: &str, args: &[String]) -> std::io::Result<Spawned> {
        self.spawn_env(program, args, &[]).await
    }

    async fn spawn_env(
        &self,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> std::io::Result<Spawned> {
        assert_eq!(program, "podman");
        if args.first().map(String::as_str) == Some("pull") {
            return Ok(self.pull(args));
        }
        let argv = args.to_vec();
        let flag = |f: &str| {
            argv.iter()
                .position(|a| a == f)
                .map(|i| argv[i + 1].clone())
                .unwrap_or_default()
        };
        let labels: HashMap<String, String> = argv
            .windows(2)
            .filter(|w| w[0] == "--label")
            .filter_map(|w| w[1].split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let (tag, iid) = (flag("-t"), flag("--iidfile"));
        self.0.spawns.lock().unwrap().push(Spawn {
            containerfile: std::fs::read_to_string(flag("-f")).unwrap_or_default(),
            ignorefile: std::fs::read_to_string(flag("--ignorefile")).unwrap_or_default(),
            argv: argv.clone(),
            env: env.to_vec(),
        });
        // What a build leaves behind while it runs (§14.2): a working
        // container, and buildah's scratch dir under `$TMPDIR` (`/var/tmp`
        // without one). And, as happens on a live machine, a model container
        // `podman run --replace` recreated meanwhile — which is not the
        // build's and must survive its cleanup.
        {
            let mut w = self.world();
            w.external.push(FakeExternal::new(
                "new1",
                "cuda-working-container",
                "storage",
            ));
            w.external.push(FakeExternal::new(
                "model2",
                "lmgw-chat-qwen-a1b2c3",
                "running",
            ));
        }
        let scratch_root = env
            .iter()
            .find(|(k, _)| k == "TMPDIR")
            .map(|(_, v)| PathBuf::from(v))
            .unwrap_or_else(|| self.0.buildah_tmp.clone());
        let scratch = scratch_root.join("buildah222");
        std::fs::create_dir_all(&scratch).unwrap();

        let behavior = *self.0.behavior.lock().unwrap();
        let (kill, mut signals) = KillHandle::channel();
        let (out_tx, stdout) = mpsc::channel(64);
        let (err_tx, stderr) = mpsc::channel(64);
        let (done_tx, done_rx) = oneshot::channel();
        let me = self.clone();
        tokio::spawn(async move {
            for l in PROGRESS {
                let _ = out_tx.send(l.to_string()).await;
            }
            let _ = err_tx.send("a warning on stderr".to_string()).await;
            let exit = match behavior {
                Behavior::Succeed => Exit::Code(0),
                Behavior::HoldThenSucceed => {
                    me.0.release.notified().await;
                    Exit::Code(0)
                }
                Behavior::HoldUntilSignal => {
                    let sig = signals.recv().await.expect("a signal");
                    me.0.signals.lock().unwrap().push(sig);
                    Exit::Code(1)
                }
                Behavior::HoldThenFinishOnSignal => {
                    let sig = signals.recv().await.expect("a signal");
                    me.0.signals.lock().unwrap().push(sig);
                    Exit::Code(0)
                }
            };
            if exit == Exit::Code(0) {
                let id = {
                    let mut w = me.world();
                    w.next += 1;
                    let id = format!("{:0>64x}", 0xb000 + w.next);
                    for img in &mut w.images {
                        img.names.retain(|n| *n != tag);
                    }
                    w.images.push(FakeImage {
                        id: id.clone(),
                        names: vec![tag.clone()],
                        labels,
                        size: 2_500_000_000,
                        digests: Vec::new(),
                    });
                    // A finished build leaves nothing behind.
                    w.external.retain(|c| c.id != "new1");
                    id
                };
                let _ = std::fs::remove_dir_all(&scratch);
                std::fs::write(&iid, format!("sha256:{id}")).unwrap();
                let _ = out_tx.send(format!("[2/2] COMMIT {tag}")).await;
            }
            drop(out_tx);
            drop(err_tx);
            let _ = done_tx.send(exit);
        });
        Ok(Spawned {
            stdout,
            stderr,
            kill,
            status: Box::pin(async move { Ok(done_rx.await.unwrap_or(Exit::Code(-1))) }),
        })
    }

    async fn run(&self, _program: &str, _args: &[String]) -> std::io::Result<CmdOutput> {
        panic!("the build executor runs buffered podman verbs through the registry's runner");
    }
}

impl FakePodman {
    /// `podman pull [--] <name>`: the image [`World::remote`] holds for the
    /// name takes the name over (the old image keeps its other names, or
    /// goes dangling), with podman's usual lines on stderr.
    fn pull(&self, args: &[String]) -> Spawned {
        let name = args.last().cloned().unwrap_or_default();
        self.world().calls.push(args.to_vec());
        let remote = self.world().remote.get(&name).cloned();
        let (out_tx, stdout) = mpsc::channel(64);
        let (err_tx, stderr) = mpsc::channel(64);
        let (done_tx, done_rx) = oneshot::channel();
        let me = self.clone();
        tokio::spawn(async move {
            let _ = err_tx.send(format!("Trying to pull {name}...")).await;
            let exit = match remote {
                Some((id, digests)) => {
                    let _ = err_tx.send("Getting image source signatures".into()).await;
                    let _ = err_tx
                        .send("Copying blob sha256:0123 done   |".into())
                        .await;
                    let _ = err_tx
                        .send("Writing manifest to image destination".into())
                        .await;
                    {
                        let mut w = me.world();
                        for img in &mut w.images {
                            img.names.retain(|n| *n != name);
                        }
                        w.images.push(FakeImage {
                            id: id.clone(),
                            names: vec![name.clone()],
                            digests,
                            size: 4_600_000_000,
                            ..FakeImage::default()
                        });
                    }
                    let _ = out_tx.send(id).await;
                    Exit::Code(0)
                }
                None => {
                    let _ = err_tx
                        .send(format!(
                            "Error: initializing source docker://{name}: reading manifest: \
                             manifest unknown"
                        ))
                        .await;
                    Exit::Code(125)
                }
            };
            drop(out_tx);
            drop(err_tx);
            let _ = done_tx.send(exit);
        });
        Spawned {
            stdout,
            stderr,
            kill: KillHandle::detached(),
            status: Box::pin(async move { Ok(done_rx.await.unwrap_or(Exit::Code(-1))) }),
        }
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

pub struct Harness {
    pub _tmp: TempDir,
    pub root: PathBuf,
    pub state: SharedState,
    pub podman: FakePodman,
    pub upstream: Repo,
    pub work: Repo,
}

impl Harness {
    pub async fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("backends-run-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let root = tmp.path().to_path_buf();
        let state = AppState::init_for_tests().await.unwrap();
        let vartmp = root.join("vartmp");
        std::fs::create_dir_all(&vartmp).unwrap();
        std::fs::create_dir(vartmp.join("buildah111")).unwrap();
        let podman = FakePodman::new(vartmp.clone());
        state.set_runtime_for_tests(Arc::new(Registry::new(
            Arc::new(podman.clone()),
            reqwest::Client::new(),
        )));
        state.set_agent_spawner_for_tests(Arc::new(podman.clone()));
        state
            .builds
            .set_lock_path_for_tests(root.join("run").join("lmgw-build.lock"));
        state.builds.set_buildah_tmp_for_tests(vartmp);
        let mut s = state.snapshot().settings.clone();
        s.builds_dir = Some(root.join("builds").display().to_string());
        s.router.extra_run_args = vec!["--device".into(), "nvidia.com/gpu=all".into()];
        store::save_settings(&state.db, &s).await.unwrap();
        state.reload_snapshot().await.unwrap();

        // The "upstream": a work tree pushed to a bare remote.
        let work = Repo::new(root.join("src"), false);
        work.write(".devops/cuda.Dockerfile", OFFICIAL_CUDA);
        work.write("a.txt", "one\ntwo\nthree\n");
        work.commit("base");
        let upstream = Repo::new(root.join("upstream.git"), true);
        work.push(&upstream, "refs/heads/*:refs/heads/*");
        Self {
            _tmp: tmp,
            root,
            state,
            podman,
            upstream,
            work,
        }
    }

    pub fn spec(&self) -> BuildSpec {
        validate_build(BuildSpec {
            slug: "official-master".into(),
            name: "official master".into(),
            engine: Engine::Llama,
            repo_url: self.upstream.url(),
            forge: Forge::Plain,
            git_ref: "master".into(),
            arch: Some(vec!["89".into()]),
            build_args: "GGML_CUDA_FA_ALL_QUANTS=ON".into(),
            keep_runs: Some(3),
            ..BuildSpec::default()
        })
        .unwrap()
    }

    pub async fn build(&self, spec: &BuildSpec) -> i64 {
        store::insert_build(&self.state.db, spec).await.unwrap()
    }

    pub async fn settings(&self, f: impl FnOnce(&mut lmgw_core::config::Settings)) {
        let mut s = self.state.snapshot().settings.clone();
        f(&mut s);
        store::save_settings(&self.state.db, &s).await.unwrap();
        self.state.reload_snapshot().await.unwrap();
    }

    pub fn builds_dir(&self) -> PathBuf {
        self.root.join("builds")
    }

    pub fn base_sha(&self) -> String {
        self.work.git(&["rev-parse", "HEAD"])
    }
}

pub async fn wait_run(state: &SharedState, run_id: i64) -> BuildRun {
    for _ in 0..1200 {
        let r = store::get_build_run(&state.db, run_id)
            .await
            .unwrap()
            .unwrap();
        if r.status.is_terminal() {
            if let Some(job) = r.job_id {
                // …and the job row closed behind it.
                for _ in 0..200 {
                    if state.jobs.live_one(job).is_none() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            return r;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("run {run_id} did not finish");
}

pub async fn wait_detail(state: &SharedState, job: i64, pred: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..1200 {
        if let Some(v) = state.jobs.live_one(job) {
            if pred(&v.detail) {
                return v.detail;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "job {job}'s detail never matched: {:?}",
        state.jobs.live_one(job).map(|v| v.detail)
    );
}

pub async fn whole_log(state: &SharedState, run_id: i64) -> String {
    let (mut text, mut offset) = (String::new(), 0);
    loop {
        let chunk = run::read_log(state, run_id, offset).await.unwrap();
        text.push_str(&chunk.text);
        offset = chunk.next_offset;
        if chunk.done {
            return text;
        }
    }
}

pub fn phases_in(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(|l| l.strip_prefix("==> "))
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(str::to_string)
        .collect()
}

pub fn has_arg_pair(argv: &[String], flag: &str, value: &str) -> bool {
    argv.windows(2).any(|w| w[0] == flag && w[1] == value)
}
