//! **Pull update** (container-builds design §8, last bullet):
//! `container_image_pull` runs `podman pull <reference>` as a background job
//! (`JobKind::ImagePull`, key `image:<reference>`), podman's output as its
//! progress, and reports the image ID and digests before and after.
//!
//! Only registry references are pulled ([`canonical_reference`]), optionally
//! `docker://`-prefixed; podman's other transports (`docker-archive:`,
//! `oci-archive:`, `dir:`, `containers-storage:`, …) read files or another
//! store on this machine and are refused. The reference is pulled — and the
//! job keyed — in its fully qualified spelling (`busybox` is
//! `docker.io/library/busybox:latest`), so two spellings of one image are one
//! pull.
//!
//! A pull replaces the image a reference names; it recreates nothing. The
//! containers running the old image keep running it until they are recreated
//! (the Backends page offers that separately), which is why the result lists
//! who still uses the reference. The old image's cached `--help`
//! vocabularies are dropped ([`Registry::forget_help`]), and the reference's
//! stored registry check is re-evaluated against the new digests, so its
//! update badge goes without asking the registry again.
//!
//! Allowed on a dev instance: a pull adds an image to the shared store and
//! moves a registry tag to it; it deletes nothing production uses.
//!
//! **Bytes** (machine-check design §5). The job pulls through Podman's own
//! API when it can ([`service`]): its compat-mode answer reports each
//! blob's bytes ([`compat`]), the registry's manifest gives the total up
//! front ([`crate::backends::oci::RegistryClient::platform_blobs`]), and the
//! job's `done`/`total` are those bytes ([`via_api`]). Without the API it
//! runs `podman pull` as it always did, which reports no byte counts, and
//! its detail says "size not known" and why.
//!
//! **A model's first start** pulls its missing image through this same job
//! ([`ensure`]), before admission — so a start, a second start of the same
//! image and an Engines-page pull share one download.
//!
//! [`Registry::forget_help`]: crate::runtime::registry::Registry::forget_help

use std::time::{Duration, Instant};

use lmgw_api_types::builds::{
    ContainerImagePullStarted, ContainerImagePullStatus, ImagePullJobDetail, ImagePullResult,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::images::UsageIndex;
use super::oci::ImageRef;
use super::run::{short_id, Podman};
use super::updates;
use crate::agents::container::{terminate, Exit, Spawned};
use crate::erased::Erased;
use crate::jobs::{self, JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress};
use crate::state::SharedState;
use crate::store;

pub mod compat;
pub mod ensure;
pub mod service;
mod via_api;

pub use ensure::{
    begin, job_bytes, presence, pull_and_wait, start_reference, Bytes, Presence, PullFailed,
};
pub use service::{ApiPull, NoPullApi, PullApi, ServicePull};

/// How often a running pull checks the cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(250);
/// SIGTERM to SIGKILL on cancel; podman stops a pull at once on SIGTERM.
const CANCEL_GRACE: Duration = Duration::from_secs(10);
/// How long the output pipes may stay open after podman exited before the
/// reader stops waiting (and the log says so).
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(30);

/// One live pull per reference.
pub fn job_key(image: &str) -> String {
    format!("image:{image}")
}

/// The job's input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub image: String,
}

/// podman's transports other than a registry's (`docker://`): each reads an
/// archive, a directory or another image store on this machine — nothing a
/// **Pull update** means.
const OTHER_TRANSPORTS: [&str; 8] = [
    "containers-storage",
    "dir",
    "docker-archive",
    "docker-daemon",
    "oci",
    "oci-archive",
    "sif",
    "tarball",
];

/// The registry reference `image` names, fully qualified
/// (`registry/repository:tag`, [`ImageRef::reference`]) — what is pulled and
/// what the job is keyed by. Refuses what `podman pull` cannot mean here,
/// before a job exists for it: another transport, a `localhost/` build, a
/// digest pin.
pub fn canonical_reference(image: &str) -> Result<String, String> {
    let bare = registry_spelling(image)?;
    check_reference(bare)?;
    if let Some(r) = on_local_registry(bare) {
        return r;
    }
    ImageRef::parse(bare)
        .map(|r| r.reference())
        .ok_or_else(|| format!("'{}' is not an image reference", image.trim()))
}

/// A `localhost/` image: a build on this machine made it, and no registry
/// has it. A registry at `localhost:<port>` is not one ([`on_local_registry`]).
fn is_local_build(bare: &str) -> bool {
    bare.starts_with("localhost/")
}

/// A reference on a registry at `localhost:<port>`, which podman pulls from
/// like from any other (unlike a `localhost/` build). [`ImageRef::parse`]
/// leaves such a registry out, so it is qualified here: `:latest` when it
/// names neither a tag nor a digest, as podman does. `None` when `bare`
/// names another registry; `Some(Err)` when it is no reference.
fn on_local_registry(bare: &str) -> Option<Result<String, String>> {
    let (host, path) = bare.split_once('/')?;
    let port = host.strip_prefix("localhost:")?;
    let bad = || Err(format!("'{bare}' is not an image reference"));
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Some(bad());
    }
    let (name, digest) = match path.split_once('@') {
        Some((n, d)) => (n, Some(d)),
        None => (path, None),
    };
    let last = name.rsplit('/').next().unwrap_or(name);
    let (repo, tag) = match last.rsplit_once(':') {
        Some((_, t)) => (&name[..name.len() - t.len() - 1], Some(t)),
        None => (name, None),
    };
    if repo.is_empty() || repo.split('/').any(str::is_empty) || tag == Some("") {
        return Some(bad());
    }
    Some(match digest {
        Some(d) if super::oci::is_digest(d) => Ok(format!("{host}/{repo}@{d}")),
        Some(_) => bad(),
        None => Ok(format!("{host}/{repo}:{}", tag.unwrap_or("latest"))),
    })
}

/// `image` as a registry spelling, `docker://` taken off; refuses what is
/// no reference at all and podman's other transports.
fn registry_spelling(image: &str) -> Result<&str, String> {
    let image = image.trim();
    if image.is_empty()
        || image.starts_with('-')
        || image.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!("'{image}' is not an image reference"));
    }
    let only = "only registry references are pulled here — registry/repository:tag, optionally \
                docker://-prefixed";
    let bare = image.strip_prefix("docker://").unwrap_or(image);
    if let Some((scheme, _)) = bare.split_once("://") {
        return Err(format!(
            "'{image}' is a {scheme}:// address, not an image reference; {only}"
        ));
    }
    if let Some(t) = bare
        .split_once(':')
        .map(|(t, _)| t)
        .filter(|t| OTHER_TRANSPORTS.contains(t))
    {
        return Err(format!(
            "'{image}' uses podman's {t}: transport, which reads from this machine rather than \
             pulling from a registry; {only}"
        ));
    }
    Ok(bare)
}

/// The registry-side refusals: a `localhost/` build, a digest pin.
fn check_reference(image: &str) -> Result<(), String> {
    if is_local_build(image) {
        return Err(format!(
            "'{image}' is a localhost/ image — it was built on this machine, not pulled; run its \
             build instead"
        ));
    }
    if image.contains('@') {
        return Err(format!(
            "'{image}' is pinned by digest, so there is no update to pull — pull a tag instead"
        ));
    }
    Ok(())
}

/// `container_image_pull` (§8 **Pull update**): start the pull of `image`
/// (as its [`canonical_reference`]) — or, when that reference is already
/// being pulled under any spelling, answer with that job.
pub async fn start_pull(
    state: &SharedState,
    image: &str,
) -> Result<ContainerImagePullStarted, String> {
    let image = canonical_reference(image)?;
    let image = image.as_str();
    let input = serde_json::to_value(Input {
        image: image.to_string(),
    })
    .map_err(|e| e.to_string())?;
    let spawned = jobs::spawn(
        state,
        JobKind::ImagePull,
        Some(job_key(image)),
        format!("pull {image}"),
        input,
    )
    .await?;
    Ok(ContainerImagePullStarted {
        job_id: spawned.id(),
        image: image.to_string(),
    })
}

/// `container_image_pull_status`: the `image_pull` job `job_id` as it stands
/// — its live progress while it runs, its [`ImagePullResult`] once done, its
/// error once failed. The jobs feed carries running jobs only and no job's
/// result, so this is how the page learns what a finished pull did.
pub async fn status(state: &SharedState, job_id: i64) -> Result<ContainerImagePullStatus, String> {
    let row = store::get_job(&state.db, job_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no job with id {job_id}"))?;
    if row.kind != JobKind::ImagePull.as_str() {
        return Err(format!(
            "job {job_id} is a {} job, not an image pull",
            row.kind
        ));
    }
    let view = state
        .jobs
        .live_one(job_id)
        .unwrap_or_else(|| jobs::JobView::from_row(&row));
    let detail: Option<ImagePullJobDetail> = serde_json::from_value(view.detail.clone()).ok();
    let result: Option<ImagePullResult> = row
        .result
        .as_deref()
        .and_then(|r| serde_json::from_str(r).ok());
    let image = result
        .as_ref()
        .map(|r| r.image.clone())
        .or_else(|| {
            detail
                .as_ref()
                .map(|d| d.image.clone())
                .filter(|i| !i.is_empty())
        })
        .or_else(|| {
            row.key
                .as_deref()
                .and_then(|k| k.strip_prefix("image:"))
                .map(str::to_string)
        })
        .unwrap_or_default();
    Ok(ContainerImagePullStatus {
        job_id,
        image,
        status: view.status,
        detail,
        result,
        error: row.error,
    })
}

fn describe(exit: &std::io::Result<Exit>) -> String {
    match exit {
        Ok(Exit::Code(n)) => format!("exited with status {n}"),
        Ok(Exit::Signal(n)) => format!("was killed by signal {n}"),
        Err(e) => format!("could not be waited for: {e}"),
    }
}

pub struct ImagePullExecutor;

/// A `podman pull` command in flight: its log and the job detail it keeps
/// current. The command reports no byte counts, so the detail carries the
/// reason the size is not known instead.
struct Pulling<'a> {
    ctx: &'a JobCtx,
    detail: ImagePullJobDetail,
    log: Vec<String>,
}

impl Pulling<'_> {
    async fn line(&mut self, line: &str) {
        let line = line.trim_end();
        if line.is_empty() {
            return;
        }
        self.log.push(line.to_string());
        self.detail.last_line = line.to_string();
        self.publish().await;
    }

    async fn publish(&self) {
        self.ctx
            .progress(JobProgress {
                done: 0,
                total: None,
                stage: self.detail.last_line.clone(),
                detail: serde_json::to_value(&self.detail).unwrap_or(Value::Null),
            })
            .await;
    }

    /// Read what is left in the pipes of a pull that ended, until both close
    /// or [`PIPE_DRAIN_GRACE`] passes.
    async fn drain(
        &mut self,
        stdout: &mut tokio::sync::mpsc::Receiver<String>,
        stderr: &mut tokio::sync::mpsc::Receiver<String>,
    ) {
        let deadline = tokio::time::sleep(PIPE_DRAIN_GRACE);
        tokio::pin!(deadline);
        let (mut out_open, mut err_open) = (true, true);
        while out_open || err_open {
            tokio::select! {
                l = stdout.recv(), if out_open => match l {
                    Some(l) => self.line(&l).await,
                    None => out_open = false,
                },
                l = stderr.recv(), if err_open => match l {
                    Some(l) => self.line(&l).await,
                    None => err_open = false,
                },
                _ = &mut deadline => return,
            }
        }
    }
}

/// How a pull's download ended, either way it ran.
enum Pulled {
    /// The image is stored: the log of how.
    Log(Vec<String>),
    Canceled,
}

/// The `podman pull` command (the path without Podman's API): its output
/// as the progress, `why` — the API's absence — as the size note.
async fn via_command(
    ctx: &JobCtx,
    image: &str,
    old_id: Option<String>,
    why: String,
) -> Result<Pulled, String> {
    let state = ctx.state.clone();
    let mut pulling = Pulling {
        ctx,
        detail: ImagePullJobDetail {
            image: image.to_string(),
            old_id,
            last_line: format!("podman pull {image}"),
            size_note: Some(format!("size not known — {why}")),
            ..Default::default()
        },
        log: Vec::new(),
    };
    pulling.publish().await;

    let argv = ["pull", "--", image].map(String::from).to_vec();
    let Spawned {
        mut stdout,
        mut stderr,
        mut status,
        kill,
    } = state
        .agent_spawner()
        .spawn("podman", &argv)
        .await
        .map_err(|e| format!("podman pull {image} could not be started: {e}"))?;
    let (mut out_open, mut err_open) = (true, true);
    let mut exit: Option<std::io::Result<Exit>> = None;
    let mut exited_at: Option<Instant> = None;
    let mut tick = tokio::time::interval(CANCEL_POLL);
    while exit.is_none() || out_open || err_open {
        tokio::select! {
            l = stdout.recv(), if out_open => match l {
                Some(l) => pulling.line(&l).await,
                None => out_open = false,
            },
            l = stderr.recv(), if err_open => match l {
                Some(l) => pulling.line(&l).await,
                None => err_open = false,
            },
            ended = &mut status, if exit.is_none() => {
                exit = Some(ended);
                exited_at = Some(Instant::now());
            }
            _ = tick.tick() => {
                if exit.is_none() && ctx.canceled() {
                    let ended = terminate(&kill, &mut status, CANCEL_GRACE).await;
                    pulling.drain(&mut stdout, &mut stderr).await;
                    tracing::info!("pull of {image} canceled: podman {}", describe(&ended));
                    return Ok(Pulled::Canceled);
                }
                if exited_at.is_some_and(|t| t.elapsed() > PIPE_DRAIN_GRACE) {
                    break;
                }
            }
        }
    }
    let ended = exit.expect("the loop ends only once podman has exited");
    if !matches!(ended, Ok(Exit::Code(0))) {
        let tail: Vec<&str> = pulling
            .log
            .iter()
            .rev()
            .take(5)
            .rev()
            .map(String::as_str)
            .collect();
        return Err(format!(
            "podman pull {image} {}: {}",
            describe(&ended),
            if tail.is_empty() {
                "it printed nothing".to_string()
            } else {
                tail.join(" / ")
            }
        ));
    }
    Ok(Pulled::Log(pulling.log))
}

impl JobExecutor for ImagePullExecutor {
    fn kind(&self) -> JobKind {
        JobKind::ImagePull
    }

    fn run<'a>(&'a self, ctx: JobCtx, input: Value) -> Erased<'a, Result<JobOutcome, String>> {
        Box::pin(async move {
            let Input { image } =
                serde_json::from_value(input).map_err(|e| format!("image_pull input: {e}"))?;
            let state = ctx.state.clone();
            let podman = Podman::of(&state);
            let old_id = podman.image_id(&image).await?;
            let old_digests = match &old_id {
                Some(_) => podman.repo_digests(&image).await.unwrap_or_default(),
                None => Vec::new(),
            };

            let api = state.builds.pull_api();
            let pulled = match api.pull(&image).await {
                ApiPull::Streaming(lines) => {
                    match via_api::run(&ctx, &image, old_id.clone(), lines, CANCEL_POLL).await {
                        via_api::Ended::Pulled(log) => Pulled::Log(log),
                        via_api::Ended::Canceled => {
                            tracing::info!("pull of {image} canceled");
                            Pulled::Canceled
                        }
                        via_api::Ended::Failed(e) => {
                            return Err(format!("podman pull {image} failed: {e}"))
                        }
                    }
                }
                ApiPull::Refused(e) => return Err(format!("podman pull {image} failed: {e}")),
                ApiPull::Unavailable(why) => {
                    tracing::info!("pulling {image} with the podman command: {why}");
                    via_command(&ctx, &image, old_id.clone(), why).await?
                }
            };
            let log = match pulled {
                Pulled::Log(log) => log,
                Pulled::Canceled => return Ok(JobOutcome::Canceled),
            };

            let new_id = podman.image_id(&image).await?.ok_or_else(|| {
                format!(
                    "podman pull {image} succeeded, but the reference names no image afterwards"
                )
            })?;
            let new_digests = podman.repo_digests(&image).await.unwrap_or_default();
            let updated = old_id.as_deref() != Some(new_id.as_str());
            if let (true, Some(old)) = (updated, &old_id) {
                let dropped = state.runtime().forget_help(old);
                tracing::info!(
                    "pulled {image}: {} → {}{}",
                    short_id(old),
                    short_id(&new_id),
                    if dropped > 0 {
                        format!(" (dropped {dropped} cached flag vocabular(ies) of the old image)")
                    } else {
                        String::new()
                    }
                );
            }
            let mut used_by = match UsageIndex::load(&state).await {
                Ok(u) => u.followers(&image, old_id.as_deref()),
                Err(e) => {
                    tracing::warn!("after pulling {image}: who uses it could not be told: {e}");
                    Vec::new()
                }
            };
            let mut seen = Vec::new();
            used_by.retain(|u| {
                let fresh = !seen.contains(u);
                if fresh {
                    seen.push(u.clone());
                }
                fresh
            });
            // The update check keys a reference by its qualified spelling.
            let qualified = ImageRef::parse(&image)
                .map(|r| r.reference())
                .unwrap_or_else(|| image.clone());
            updates::after_pull(&state, &qualified, &new_digests).await;

            let result = ImagePullResult {
                image,
                old_id,
                new_id,
                old_digests,
                new_digests,
                updated,
                used_by,
                log,
            };
            Ok(JobOutcome::Done(
                serde_json::to_value(&result).map_err(|e| e.to_string())?,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_registry_tags_are_pulled() {
        assert_eq!(
            canonical_reference("ghcr.io/0xshug0/audio.cpp:full-cuda12").as_deref(),
            Ok("ghcr.io/0xshug0/audio.cpp:full-cuda12")
        );
        assert!(canonical_reference("busybox").is_ok());
        let e = canonical_reference("localhost/lmgw-llama-server:official-master").unwrap_err();
        assert!(e.contains("built on this machine"), "{e}");
        assert!(canonical_reference("ghcr.io/o/r@sha256:abc")
            .unwrap_err()
            .contains("pinned by digest"));
        assert!(canonical_reference("--all").is_err());
        assert!(canonical_reference("a b").is_err());
        assert!(canonical_reference("").is_err());
    }

    #[test]
    fn other_transports_are_refused() {
        for (bad, transport) in [
            ("docker-archive:/tmp/x.tar", "docker-archive"),
            ("oci-archive:/tmp/x.tar:tag", "oci-archive"),
            ("oci:/srv/layout:tag", "oci"),
            ("dir:/tmp/img", "dir"),
            ("containers-storage:localhost/x:y", "containers-storage"),
            ("docker-daemon:busybox:latest", "docker-daemon"),
        ] {
            let e = canonical_reference(bad).unwrap_err();
            assert!(
                e.contains(&format!("podman's {transport}: transport"))
                    && e.contains("only registry references are pulled here"),
                "{bad}: {e}"
            );
        }
        let e = canonical_reference("https://ghcr.io/o/r:t").unwrap_err();
        assert!(e.contains("https:// address"), "{e}");
        let e = canonical_reference("docker://docker-archive:/tmp/x.tar").unwrap_err();
        assert!(e.contains("docker-archive: transport"), "{e}");
        // A registry on a port is not a transport.
        assert_eq!(
            canonical_reference("registry.example:5000/o/r:t").as_deref(),
            Ok("registry.example:5000/o/r:t")
        );
    }

    /// `localhost/` is a build on this machine; `localhost:<port>/` is a
    /// registry podman pulls from like any other.
    #[test]
    fn a_registry_on_localhost_is_pulled_a_localhost_build_is_not() {
        assert_eq!(
            canonical_reference("localhost:5000/o/r:t").as_deref(),
            Ok("localhost:5000/o/r:t")
        );
        assert_eq!(
            canonical_reference("docker://localhost:5000/r").as_deref(),
            Ok("localhost:5000/r:latest")
        );
        assert!(canonical_reference("localhost/r:t")
            .unwrap_err()
            .contains("built on this machine"));
        for bad in [
            "localhost:/r",
            "localhost:50x0/r",
            "localhost:5000/",
            "localhost:5000//r",
            "localhost:5000/r:",
        ] {
            assert!(
                canonical_reference(bad)
                    .unwrap_err()
                    .contains("not an image reference"),
                "{bad}"
            );
        }
        assert!(canonical_reference("localhost:5000/r@sha256:abc")
            .unwrap_err()
            .contains("pinned by digest"));
    }

    /// Two spellings of one image are one pull: the job key is the qualified
    /// reference.
    #[test]
    fn equivalent_references_share_one_job_key() {
        for spelling in [
            "busybox",
            "busybox:latest",
            "docker.io/library/busybox",
            "docker://busybox",
            "docker://docker.io/library/busybox:latest",
            "index.docker.io/library/busybox:latest",
            "  busybox  ",
        ] {
            assert_eq!(
                canonical_reference(spelling)
                    .map(|r| job_key(&r))
                    .as_deref(),
                Ok("image:docker.io/library/busybox:latest"),
                "{spelling}"
            );
        }
    }
}
