//! The buffered `podman` verbs of container builds and the Images view —
//! `image inspect`, `tag`, `untag`, `rmi`, `ps --external`, `rm -f`,
//! `unshare rm -rf` — through the registry's [`CommandRunner`] seam, plus the
//! pure `podman build` argv (container-builds §5 step 5, §14.2). The build
//! itself streams, so it goes through the agent [`Spawner`] instead.
//!
//! [`Spawner`]: crate::agents::container::Spawner

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::backends::tags;
use crate::runtime::registry::{CmdOutput, CommandRunner};
use crate::state::AppState;

/// `podman` through the registry's runner.
#[derive(Clone)]
pub(crate) struct Podman {
    runner: Arc<dyn CommandRunner>,
    /// A dev instance's: [`Self::tag`], [`Self::untag`] and [`Self::rmi`]
    /// then refuse every name outside the dev namespace
    /// ([`tags::is_dev_name`]) — the one gate every image mutation of the
    /// builds code passes, so no code path of a dev instance can move or
    /// remove one of production's names in the image store they share.
    dev: bool,
}

/// The first 12 characters of an image or container ID, for messages.
pub(crate) fn short_id(id: &str) -> &str {
    let id = id.trim_start_matches("sha256:");
    id.get(..12).unwrap_or(id)
}

/// podman's ways of saying "there is no such image".
fn no_such_image(stderr: &str) -> bool {
    let e = stderr.to_ascii_lowercase();
    e.contains("image not known")
        || e.contains("no such image")
        || e.contains("failed to find image")
}

impl Podman {
    pub fn of(state: &AppState) -> Self {
        Self {
            runner: state.runtime().runner(),
            dev: state.dev(),
        }
    }

    /// On a dev instance, refuse `name` unless it is a dev name.
    fn dev_guard(&self, verb: &str, name: &str) -> Result<(), String> {
        if self.dev && !tags::is_dev_name(name) {
            return Err(format!(
                "a dev instance (LMGW_DEV) does not {verb} '{name}': it only touches names under \
                 {}…, never production's",
                tags::DEV_REPO_PREFIX
            ));
        }
        Ok(())
    }

    /// Run `podman <args>`; any exit status is `Ok`.
    pub async fn run(&self, args: &[&str]) -> Result<CmdOutput, String> {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.runner
            .run("podman", &argv)
            .await
            .map_err(|e| format!("podman could not be run: {e}"))
    }

    /// [`Self::run`], a non-zero exit being the error.
    pub async fn run_ok(&self, args: &[&str]) -> Result<CmdOutput, String> {
        let out = self.run(args).await?;
        if !out.ok() {
            return Err(format!(
                "podman {} failed (exit {}): {}",
                args.first().copied().unwrap_or(""),
                out.status,
                out.stderr.trim()
            ));
        }
        Ok(out)
    }

    /// The full ID `reference` (a tag or an ID) names right now, `None` when
    /// there is no such image on this machine.
    pub async fn image_id(&self, reference: &str) -> Result<Option<String>, String> {
        let out = self
            .run(&["image", "inspect", "--format", "{{.Id}}", reference])
            .await?;
        if !out.ok() {
            if no_such_image(&out.stderr) {
                return Ok(None);
            }
            return Err(format!(
                "podman image inspect {reference} failed (exit {}): {}",
                out.status,
                out.stderr.trim()
            ));
        }
        let id = out
            .stdout
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .trim_start_matches("sha256:")
            .to_string();
        Ok((!id.is_empty()).then_some(id))
    }

    /// The image's size in bytes.
    pub async fn image_size(&self, image: &str) -> Result<u64, String> {
        let out = self
            .run_ok(&["image", "inspect", "--format", "{{.Size}}", image])
            .await?;
        let text = out.stdout.trim();
        text.parse()
            .map_err(|_| format!("podman image inspect printed {text:?} as the size of {image}"))
    }

    /// Every `repo:tag` the image carries (empty for a dangling one).
    pub async fn repo_tags(&self, image: &str) -> Result<Vec<String>, String> {
        let out = self
            .run_ok(&["image", "inspect", "--format", "{{json .RepoTags}}", image])
            .await?;
        let tags: Option<Vec<String>> = serde_json::from_str(out.stdout.trim())
            .map_err(|e| format!("podman image inspect printed unreadable tags: {e}"))?;
        Ok(tags.unwrap_or_default())
    }

    /// podman's `RepoDigests` of the image (`repo@sha256:…`, one per
    /// registry manifest it was pulled as); empty for one built here.
    pub async fn repo_digests(&self, image: &str) -> Result<Vec<String>, String> {
        let out = self
            .run_ok(&[
                "image",
                "inspect",
                "--format",
                "{{json .RepoDigests}}",
                image,
            ])
            .await?;
        let digests: Option<Vec<String>> = serde_json::from_str(out.stdout.trim())
            .map_err(|e| format!("podman image inspect printed unreadable digests: {e}"))?;
        Ok(digests.unwrap_or_default())
    }

    pub async fn tag(&self, image: &str, tag: &str) -> Result<(), String> {
        self.dev_guard("tag", tag)?;
        self.run_ok(&["tag", image, tag]).await.map(|_| ())
    }

    pub async fn untag(&self, image: &str, tag: &str) -> Result<(), String> {
        self.dev_guard("untag", tag)?;
        self.run_ok(&["untag", image, tag]).await.map(|_| ())
    }

    /// `podman rmi <refs…>` (never `--force`: that would also remove the
    /// containers using the image). What podman reports it untagged and
    /// deleted, in order. On a dev instance every ref must be a dev name —
    /// never a bare ID, which could be anyone's image.
    pub async fn rmi(&self, refs: &[&str]) -> Result<Vec<String>, String> {
        for r in refs {
            self.dev_guard("remove", r)?;
        }
        let mut args = vec!["rmi"];
        args.extend_from_slice(refs);
        let out = self.run_ok(&args).await?;
        Ok(out
            .stdout
            .lines()
            .map(|l| {
                let l = l.trim();
                l.strip_prefix("Untagged: ")
                    .or_else(|| l.strip_prefix("Deleted: "))
                    .unwrap_or(l)
                    .to_string()
            })
            .filter(|l| !l.is_empty())
            .collect())
    }

    /// buildah's working containers (§14.2): of `podman ps -a --external`,
    /// the ones in state `storage` whose name is buildah's
    /// `<image>-working-container[-N]` — never a model's, an agent's or
    /// anything else a user runs, which is every other container there.
    pub async fn working_containers(&self) -> Result<Vec<WorkingContainer>, String> {
        let out = self
            .run_ok(&["ps", "-a", "--external", "--format", "json"])
            .await?;
        if out.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<PsExternalRow> = serde_json::from_str(&out.stdout)
            .map_err(|e| format!("podman ps --external returned unreadable JSON: {e}"))?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let name = r.names.and_then(|n| n.into_iter().next())?;
                let storage = r.state.eq_ignore_ascii_case("storage")
                    || r.status.eq_ignore_ascii_case("storage");
                (storage && is_working_container_name(&name))
                    .then_some(WorkingContainer { id: r.id, name })
            })
            .collect())
    }

    /// `podman rm -f <ids…>` — for buildah working containers, where
    /// `inspect` fails and `rm` works (§14.2), and for the containers a
    /// forced image delete takes down.
    pub async fn rm_force(&self, ids: &[String]) -> Result<(), String> {
        let mut args = vec!["rm", "-f"];
        args.extend(ids.iter().map(String::as_str));
        self.run_ok(&args).await.map(|_| ())
    }

    /// `podman unshare rm -rf <dir>`: buildah's scratch directories are owned
    /// by subordinate UIDs, so only the user namespace can delete them.
    pub async fn unshare_rm(&self, dir: &Path) -> Result<(), String> {
        let d = dir
            .to_str()
            .ok_or_else(|| format!("{} is not a UTF-8 path", dir.display()))?;
        self.run_ok(&["unshare", "rm", "-rf", "--", d])
            .await
            .map(|_| ())
    }

    /// Remove a buildah cache-mount directory (a deleted build's own ccache or
    /// npm cache, §14.2), reporting its size in bytes when it existed —
    /// `None` when there was nothing to remove, which is not an error: the
    /// directory only exists once a run has actually written to that id.
    /// Plain removal first; `podman unshare rm -rf` ([`Self::unshare_rm`]) on
    /// any error, since a buildah cache mount can leave files owned by
    /// subordinate UIDs the way its scratch directories do.
    pub async fn remove_cache_dir(&self, dir: &Path) -> Result<Option<u64>, String> {
        if !dir.is_dir() {
            return Ok(None);
        }
        let size = dir_size(dir);
        if let Err(plain_err) = std::fs::remove_dir_all(dir) {
            self.unshare_rm(dir).await.map_err(|e| {
                format!("plain removal failed ({plain_err}) and so did podman unshare rm -rf: {e}")
            })?;
        }
        Ok(Some(size))
    }
}

/// The total size, in bytes, of every regular file under `dir`, followed
/// recursively; `0` for a directory that cannot be (fully) read rather than
/// failing — a size worth less than blocking the removal it is reported
/// alongside on being able to read it.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

/// The real (host) uid podman/buildah run as — never the 0:0 a cache mount's
/// own uid/gid default to inside the build, which is what
/// [`tags::cache_mount_dir_name`] hashes.
pub(crate) fn real_uid() -> u32 {
    // SAFETY: `getuid()` takes no arguments and always succeeds.
    unsafe { libc::getuid() }
}

/// Buildah's cache-mount root on this host: `/var/tmp/buildah-cache-<uid>`
/// (§14.2). `tmp` is the scratch root ([`super::BuildSeams::buildah_tmp`],
/// `/var/tmp` outside tests); `uid` is [`real_uid`], taken as a parameter so
/// this stays pure and testable.
pub(crate) fn cache_mount_root(tmp: &Path, uid: u32) -> PathBuf {
    tmp.join(format!("buildah-cache-{uid}"))
}

/// Where buildah keeps `id`'s cache-mount data under `root`
/// ([`cache_mount_root`]).
pub(crate) fn cache_mount_dir(root: &Path, id: &str) -> PathBuf {
    root.join(tags::cache_mount_dir_name(id))
}

/// The `buildah<digits>` directories under `root` (`/var/tmp`): a build's
/// scratch space, left behind by an interrupted one (§14.2). Never the
/// `buildah-cache-<uid>` cache-mount directory, whose name has a dash.
pub(crate) fn buildah_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.strip_prefix("buildah")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                && e.file_type().is_ok_and(|t| t.is_dir())
        })
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// One row of `podman ps -a --external --format json`.
#[derive(Debug, serde::Deserialize)]
struct PsExternalRow {
    #[serde(rename = "Id", default)]
    id: String,
    #[serde(rename = "Names", default)]
    names: Option<Vec<String>>,
    #[serde(rename = "State", default)]
    state: String,
    #[serde(rename = "Status", default)]
    status: String,
}

/// A buildah working container: `podman rm -f` removes it (inspect fails on
/// it, rm works, §14.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkingContainer {
    pub id: String,
    pub name: String,
}

/// buildah names a stage's working container after its base image:
/// `<image>-working-container`, then `-working-container-1`, `-2`, … when the
/// name is taken (`node-working-container`, measured 2026-09-26).
pub(crate) fn is_working_container_name(name: &str) -> bool {
    match name.rsplit_once("-working-container") {
        Some((image, rest)) => {
            !image.is_empty()
                && (rest.is_empty()
                    || rest
                        .strip_prefix('-')
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())))
        }
        None => false,
    }
}

/// The buildah working containers an interrupted `podman build` leaves
/// (§14.2) — one per started stage. Snapshotted before the build so that
/// afterwards only the ones that **appeared during it** are removed: never
/// one that was there before, and — being filtered to working containers
/// ([`Podman::working_containers`]) — never a model container `podman run
/// --replace` gave a new ID while the build ran, nor an agent's. (Its
/// scratch directory needs no diff: the build's `TMPDIR` is a directory of
/// the run's own, removed whole.)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Leftovers {
    pub containers: Vec<WorkingContainer>,
}

impl Leftovers {
    pub async fn snapshot(podman: &Podman) -> Result<Self, String> {
        Ok(Self {
            containers: podman.working_containers().await?,
        })
    }

    /// What is in `now` and was not in `self`.
    pub fn new_since(&self, now: &Leftovers) -> Leftovers {
        Leftovers {
            containers: now
                .containers
                .iter()
                .filter(|c| !self.containers.iter().any(|b| b.id == c.id))
                .cloned()
                .collect(),
        }
    }
}

/// One `podman build` invocation (§5 step 5).
pub(crate) struct BuildCommand<'a> {
    /// Empty: build the last stage (no `--target`).
    pub target: &'a str,
    pub containerfile: &'a Path,
    pub ignorefile: &'a Path,
    pub keep_layers: bool,
    /// **Rebuild anyway**: `--pull=newer`, since the base image tags move.
    pub pull_newer: bool,
    pub cpus: Option<&'a str>,
    pub build_args: &'a [(String, String)],
    pub labels: &'a [(String, String)],
    pub iidfile: &'a Path,
    pub tag: &'a str,
    pub context: &'a Path,
}

/// The argv after `podman`, in §5's order.
pub(crate) fn build_argv(c: &BuildCommand<'_>) -> Vec<String> {
    let p = |p: &Path| p.to_string_lossy().into_owned();
    let mut argv: Vec<String> = vec!["build".into()];
    if !c.target.is_empty() {
        argv.push("--target".into());
        argv.push(c.target.into());
    }
    argv.push("-f".into());
    argv.push(p(c.containerfile));
    argv.push("--ignorefile".into());
    argv.push(p(c.ignorefile));
    argv.push(format!("--layers={}", c.keep_layers));
    if c.pull_newer {
        argv.push("--pull=newer".into());
    }
    if let Some(cpus) = c.cpus.map(str::trim).filter(|s| !s.is_empty()) {
        argv.push("--cpuset-cpus".into());
        argv.push(cpus.into());
    }
    for (k, v) in c.build_args {
        argv.push("--build-arg".into());
        argv.push(format!("{k}={v}"));
    }
    for (k, v) in c.labels {
        argv.push("--label".into());
        argv.push(format!("{k}={v}"));
    }
    argv.push("--iidfile".into());
    argv.push(p(c.iidfile));
    argv.push("-t".into());
    argv.push(c.tag.into());
    argv.push(p(c.context));
    argv
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn cache_mount_paths_are_the_uid_rooted_dir_of_the_hashed_id() {
        assert_eq!(
            cache_mount_root(Path::new("/var/tmp"), 1000),
            Path::new("/var/tmp/buildah-cache-1000")
        );
        assert_eq!(
            cache_mount_dir(Path::new("/var/tmp/buildah-cache-1000"), "lmgw-llama-cuda"),
            Path::new("/var/tmp/buildah-cache-1000/3b85a8397e0a7074")
        );
    }

    #[tokio::test]
    async fn a_dev_instance_touches_only_dev_names() {
        let recorder = Arc::new(RecordingRunner {
            calls: Mutex::new(Vec::new()),
        });
        let podman = Podman {
            runner: recorder.clone(),
            dev: true,
        };
        let prod = "localhost/lmgw-llama-server:official-master";
        let dev = "localhost/lmgw-dev-llama-server:official-master";
        assert!(podman
            .tag("abc", prod)
            .await
            .unwrap_err()
            .contains("dev instance"));
        assert!(podman.untag("abc", prod).await.is_err());
        assert!(podman.rmi(&[dev, prod]).await.is_err(), "all or nothing");
        assert!(
            podman.rmi(&[&"a".repeat(64)]).await.is_err(),
            "never a bare ID"
        );
        assert!(
            recorder.calls.lock().unwrap().is_empty(),
            "podman never ran"
        );
        podman.tag("abc", dev).await.unwrap();
        podman.untag("abc", dev).await.unwrap();
        podman.rmi(&[dev]).await.unwrap();
        assert_eq!(recorder.calls.lock().unwrap().len(), 3);
    }

    #[test]
    fn dir_size_sums_regular_files_recursively_and_a_missing_dir_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(dir_size(&tmp.path().join("nope")), 0);
        std::fs::write(tmp.path().join("a"), [0u8; 10]).unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub").join("b"), [0u8; 20]).unwrap();
        assert_eq!(dir_size(tmp.path()), 30);
    }

    /// A fake `podman` that only ever answers `unshare rm -rf`, recording the
    /// call — for asserting the fallback path is the one taken.
    struct RecordingRunner {
        calls: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl CommandRunner for RecordingRunner {
        async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
            assert_eq!(program, "podman");
            self.calls.lock().unwrap().push(args.to_vec());
            Ok(CmdOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[tokio::test]
    async fn remove_cache_dir_reports_none_for_a_missing_one_and_the_size_for_a_present_one() {
        let recorder = Arc::new(RecordingRunner {
            calls: Mutex::new(Vec::new()),
        });
        let podman = Podman {
            runner: recorder.clone(),
            dev: false,
        };
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert_eq!(podman.remove_cache_dir(&missing).await.unwrap(), None);
        assert!(
            recorder.calls.lock().unwrap().is_empty(),
            "never shelled out for a no-op"
        );

        let present = tmp.path().join("lmgw-llama-cuda-master-pr1");
        std::fs::create_dir(&present).unwrap();
        std::fs::write(present.join("stats"), [0u8; 42]).unwrap();
        assert_eq!(
            podman.remove_cache_dir(&present).await.unwrap(),
            Some(42),
            "plain removal succeeds here, so the size is read before it runs"
        );
        assert!(!present.exists());
        assert!(
            recorder.calls.lock().unwrap().is_empty(),
            "plain removal worked; unshare rm was never needed"
        );
    }

    #[tokio::test]
    async fn remove_cache_dir_falls_back_to_unshare_rm_when_plain_removal_is_refused() {
        if real_uid() == 0 {
            eprintln!("skipped: root ignores the permission bits this test relies on");
            return;
        }
        let recorder = Arc::new(RecordingRunner {
            calls: Mutex::new(Vec::new()),
        });
        let podman = Podman {
            runner: recorder.clone(),
            dev: false,
        };
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("lmgw-npm-llama-master-pr1");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("f"), [0u8; 5]).unwrap();
        // No write permission on the directory itself: unlinking the file
        // inside it (part of a plain `remove_dir_all`) is refused, forcing
        // the `podman unshare rm -rf` fallback — which this fake never
        // actually removes anything for, so the size read beforehand is what
        // proves the directory was seen before the attempt.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = podman.remove_cache_dir(&dir).await;
        // Cleanup first, whether the assertions below pass or not.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(result.unwrap(), Some(5));
        let calls = recorder.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0][0], "unshare");
        assert_eq!(calls[0][1], "rm");
        assert!(calls[0].contains(&dir.to_string_lossy().into_owned()));
    }

    #[test]
    fn buildah_scratch_dirs_are_told_from_the_cache_and_from_files() {
        let tmp = tempfile::tempdir().unwrap();
        for d in ["buildah123", "buildah-cache-1000", "buildah", "buildahx1"] {
            std::fs::create_dir(tmp.path().join(d)).unwrap();
        }
        std::fs::write(tmp.path().join("buildah999"), "a file").unwrap();
        assert_eq!(
            buildah_dirs(tmp.path()),
            vec![tmp.path().join("buildah123")]
        );
    }

    #[test]
    fn only_working_containers_that_appeared_since_the_snapshot_are_new() {
        let wc = |id: &str| WorkingContainer {
            id: id.into(),
            name: format!("cuda-working-container-{id}"),
        };
        let before = Leftovers {
            containers: vec![wc("1"), wc("2")],
        };
        let now = Leftovers {
            containers: vec![wc("2"), wc("3")],
        };
        assert_eq!(
            before.new_since(&now),
            Leftovers {
                containers: vec![wc("3")],
            }
        );
    }

    #[test]
    fn working_containers_are_told_by_buildahs_name() {
        for yes in [
            "node-working-container",
            "cuda-working-container-1",
            "ubuntu-working-container-12",
        ] {
            assert!(is_working_container_name(yes), "{yes}");
        }
        for no in [
            "lmgw-chat-qwen-a1b2c3",
            "working-container",
            "-working-container",
            "node-working-container-x",
            "node-working-container-",
            "my-working-containers",
        ] {
            assert!(!is_working_container_name(no), "{no}");
        }
    }

    /// Of everything `ps --external` lists, only buildah's working containers
    /// in `storage` — a model container recreated during the build, an
    /// exited one, an agent's, are never the build's.
    #[tokio::test]
    async fn working_containers_are_filtered_by_state_and_name() {
        struct Ps;
        #[async_trait::async_trait]
        impl CommandRunner for Ps {
            async fn run(&self, _p: &str, args: &[String]) -> std::io::Result<CmdOutput> {
                assert_eq!(args, ["ps", "-a", "--external", "--format", "json"]);
                Ok(CmdOutput {
                    status: 0,
                    stdout: r#"[
                        {"Id":"a1","Names":["cuda-working-container"],"State":"storage"},
                        {"Id":"b2","Names":["lmgw-chat-qwen-a1b2c3"],"State":"running"},
                        {"Id":"c3","Names":["node-working-container-1"],"State":"exited"},
                        {"Id":"d4","Names":["lmgw-agent-x"],"State":"storage"},
                        {"Id":"e5","Names":null,"State":"storage"},
                        {"Id":"f6","Names":["ubuntu-working-container-2"],"Status":"Storage"}
                    ]"#
                    .into(),
                    stderr: String::new(),
                })
            }
        }
        let podman = Podman {
            runner: Arc::new(Ps),
            dev: false,
        };
        let ids: Vec<String> = podman
            .working_containers()
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, ["a1", "f6"]);
    }

    #[test]
    fn the_build_argv_is_in_the_designs_order() {
        let args = vec![("CUDA_VERSION".to_string(), "13.0.0".to_string())];
        let labels = vec![("dev.lmgw.run".to_string(), "7".to_string())];
        let argv = build_argv(&BuildCommand {
            target: "server",
            containerfile: Path::new("/b/work/7.ctx/Containerfile"),
            ignorefile: Path::new("/b/work/7.ctx/ignorefile"),
            keep_layers: false,
            pull_newer: true,
            cpus: Some("0-15"),
            build_args: &args,
            labels: &labels,
            iidfile: Path::new("/b/work/7.ctx/iid"),
            tag: "localhost/lmgw-llama-server:x-abcdef0-123456",
            context: Path::new("/b/work/7"),
        });
        assert_eq!(
            argv.join(" "),
            "build --target server -f /b/work/7.ctx/Containerfile --ignorefile \
             /b/work/7.ctx/ignorefile --layers=false --pull=newer --cpuset-cpus 0-15 \
             --build-arg CUDA_VERSION=13.0.0 --label dev.lmgw.run=7 --iidfile /b/work/7.ctx/iid \
             -t localhost/lmgw-llama-server:x-abcdef0-123456 /b/work/7"
        );
    }
}
