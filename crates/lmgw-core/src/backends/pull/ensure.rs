//! A start that needs its engine image (machine-check design §5): is the
//! image on this machine, and if not, pull it through the `image_pull` job
//! — a new one, or the one already pulling it — and wait for it to end.
//!
//! The waiting is the caller's: admission does it before the gate
//! (`vram::download`), so nothing is evicted or reserved while the bytes
//! come down, and two starts of one image, or a start and an Engines-page
//! pull, share one download (the job is keyed by the reference). A caller
//! that must not wait — a guest's start — only [`begin`]s it.

use std::time::Duration;

use lmgw_api_types::builds::ImagePullJobDetail;

use super::{job_key, Input};
use crate::backends::oci::{is_digest, ImageRef};
use crate::backends::run::Podman;
use crate::jobs::{self, JobKind};
use crate::state::SharedState;
use crate::store;

/// How often a waiting start reads its pull's progress. A sampling rate:
/// the job updates its live view on every report, and nothing is skipped.
const WATCH: Duration = Duration::from_millis(250);

/// Whether an image is on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    Present,
    Missing,
    /// podman could not say: the start goes ahead as it would have, and its
    /// own failure, if any, says why.
    Unknown(String),
}

/// `podman image exists -- <image>`: exit 0 is there, 1 is not, anything
/// else is podman failing to say. The image is a row's setting, checked by
/// nothing yet: after `--` it is never read as a flag.
pub async fn presence(state: &SharedState, image: &str) -> Presence {
    match Podman::of(state)
        .run(&["image", "exists", "--", image])
        .await
    {
        Ok(out) if out.status == 0 => Presence::Present,
        Ok(out) if out.status == 1 => Presence::Missing,
        Ok(out) => Presence::Unknown(format!(
            "podman image exists {image} exited {}: {}",
            out.status,
            out.stderr.trim()
        )),
        Err(e) => Presence::Unknown(e),
    }
}

/// The spelling a start's image is pulled as, and its job keyed by: fully
/// qualified like [`super::canonical_reference`] (`registry/repository:tag`),
/// and — unlike an Engines-page pull — also pinned by digest
/// (`registry/repository@sha256:…`), which a start needs as much as a tag.
/// Refuses what no registry can give: another transport, a `localhost/`
/// build. A registry at `localhost:<port>` is pulled from like any other.
pub fn start_reference(image: &str) -> Result<String, String> {
    let bare = super::registry_spelling(image)?;
    if super::is_local_build(bare) {
        return Err(
            "it was built on this machine, not pulled — run its build again on the Engines page"
                .into(),
        );
    }
    if let Some(r) = super::on_local_registry(bare) {
        return r;
    }
    match bare.split_once('@') {
        Some((name, digest)) => {
            let r = ImageRef::parse(name)
                .filter(|_| is_digest(digest))
                .ok_or_else(|| format!("'{image}' is not an image reference"))?;
            Ok(format!("{}@{digest}", r.repo_name()))
        }
        None => ImageRef::parse(bare)
            .map(|r| r.reference())
            .ok_or_else(|| format!("'{image}' is not an image reference")),
    }
}

/// The bytes of a pull so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bytes {
    pub done: u64,
    /// `None` while the size is not known.
    pub total: Option<u64>,
    /// Why the size is not known, when the job said: its
    /// [`ImagePullJobDetail::size_note`] ("size not known — …").
    pub size_note: Option<String>,
}

/// Why a start's pull did not store its image. Its words finish the
/// sentence "<image> is not on this machine, and …".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullFailed {
    /// No registry can give it ([`start_reference`]'s refusal).
    NotPullable(String),
    /// Its download ended without it: how.
    Ended(String),
}

impl std::fmt::Display for PullFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPullable(why) => write!(f, "it cannot be downloaded: {why}"),
            Self::Ended(how) => write!(f, "its download {how}"),
        }
    }
}

/// Start the pull of `image` through the `image_pull` job, or join the one
/// already pulling it under any spelling: the job's id. Nothing waits here,
/// and the job goes on by itself whoever follows it.
pub async fn begin(state: &SharedState, image: &str) -> Result<i64, PullFailed> {
    let reference = start_reference(image).map_err(PullFailed::NotPullable)?;
    let ended = |how: String| PullFailed::Ended(how);
    let input = serde_json::to_value(Input {
        image: reference.clone(),
    })
    .map_err(|e| ended(format!("could not begin: {e}")))?;
    let spawned = jobs::spawn(
        state,
        JobKind::ImagePull,
        Some(job_key(&reference)),
        format!("pull {reference}"),
        input,
    )
    .await
    .map_err(|e| ended(format!("could not begin: {e}")))?;
    Ok(spawned.id())
}

/// The bytes pull job `id` has so far; `None` once it is no longer live
/// (or, a moment after [`begin`] joined it, not live yet).
pub fn job_bytes(state: &SharedState, id: i64) -> Option<Bytes> {
    let view = state.jobs.live_one(id)?;
    let detail: ImagePullJobDetail = serde_json::from_value(view.detail).unwrap_or_default();
    Some(Bytes {
        done: detail.done_bytes,
        total: detail.total_bytes,
        size_note: detail.size_note,
    })
}

/// Pull `image` through the `image_pull` job and wait until it ends:
/// `Ok` once the image is stored. `seen` hears the bytes each time they
/// moved, and once at the start. Dropping the future stops the waiting, not
/// the pull: a caller that gave up and comes back joins the same job.
pub async fn pull_and_wait(
    state: &SharedState,
    image: &str,
    seen: impl FnMut(Bytes),
) -> Result<(), PullFailed> {
    let id = begin(state, image).await?;
    follow(state, image, id, seen).await
}

/// Wait for pull job `id` (of `image`) to end, telling `seen` its bytes.
async fn follow(
    state: &SharedState,
    image: &str,
    id: i64,
    mut seen: impl FnMut(Bytes),
) -> Result<(), PullFailed> {
    let ended = |how: String| PullFailed::Ended(how);
    let mut last: Option<Bytes> = None;
    // Since when the row has said running while no pull of it was live.
    let mut unseen: Option<std::time::Instant> = None;
    loop {
        if let Some(now) = job_bytes(state, id) {
            unseen = None;
            if last.as_ref() != Some(&now) {
                seen(now.clone());
                last = Some(now);
            }
            tokio::time::sleep(WATCH).await;
            continue;
        }
        // Not live: ended, or — when another start's spawn claimed it a
        // moment ago — not live yet (`jobs::spawn` answers a joiner with
        // the job's id before the job that claimed it is in the live map).
        let row = store::get_job(&state.db, id)
            .await
            .map_err(|e| ended(format!("(job {id}) could not be followed: {e}")))?
            .ok_or_else(|| ended(format!("(job {id}) is gone from the jobs table")))?;
        match row.status.as_str() {
            "done" => return Ok(()),
            "canceled" => return Err(ended(format!("(job {id}) was canceled"))),
            "failed" => {
                return Err(ended(match row.error {
                    Some(e) => format!("failed: {e}"),
                    None => format!("(job {id}) failed without saying why"),
                }))
            }
            other => {
                // A pull that ended whose outcome could not be recorded
                // (`store::finish_job` failed and left the row running):
                // the image it stored is what the start needs. Asked once
                // each time the job is found not live.
                if unseen.is_none() && presence(state, image).await == Presence::Present {
                    return Ok(());
                }
                let since = *unseen.get_or_insert_with(std::time::Instant::now);
                // A row boot did not close (`fail_orphaned_jobs`) and no
                // spawn here goes on with: said, not waited for forever.
                if since.elapsed() >= ORPHANED {
                    return Err(ended(format!(
                        "(job {id}) has been recorded as {other} for {} s, but no pull of it \
                         runs in this lmgw — cancel it on the Engines page and start again",
                        ORPHANED.as_secs()
                    )));
                }
                tokio::time::sleep(WATCH).await;
            }
        }
    }
}

/// How long a job row may say it runs while no pull of it is live before
/// the wait says so: the window between `jobs::spawn` claiming a row and
/// putting its job in the live map is a few database writes.
const ORPHANED: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_pulls_tags_and_digests_never_local_builds() {
        assert_eq!(
            start_reference("ghcr.io/ggml-org/llama.cpp:server-cuda").as_deref(),
            Ok("ghcr.io/ggml-org/llama.cpp:server-cuda")
        );
        assert_eq!(
            start_reference("busybox").as_deref(),
            Ok("docker.io/library/busybox:latest")
        );
        let d = format!("sha256:{}", "0123456789abcdef".repeat(4));
        assert_eq!(
            start_reference(&format!("docker://busybox:1.37@{d}")),
            Ok(format!("docker.io/library/busybox@{d}"))
        );
        let e = start_reference("localhost/lmgw-llama-server:official-master").unwrap_err();
        assert!(
            e.contains("built on this machine") && e.contains("Engines page"),
            "{e}"
        );
        assert_eq!(
            PullFailed::NotPullable(e).to_string(),
            "it cannot be downloaded: it was built on this machine, not pulled — run its build \
             again on the Engines page"
        );
        assert!(start_reference("busybox@sha256:nothex").is_err());
        assert!(start_reference("oci-archive:/tmp/x.tar").is_err());
        assert!(start_reference("").is_err());
        assert!(start_reference("-q").is_err());
    }

    /// A registry at `localhost:<port>` is one podman pulls from: only a
    /// `localhost/` image is a build on this machine.
    #[test]
    fn a_start_pulls_from_a_registry_on_localhost() {
        assert_eq!(
            start_reference("localhost:5000/lmgw/engine").as_deref(),
            Ok("localhost:5000/lmgw/engine:latest")
        );
        assert_eq!(
            start_reference("localhost:5000/lmgw/engine:cuda").as_deref(),
            Ok("localhost:5000/lmgw/engine:cuda")
        );
        let d = format!("sha256:{}", "0123456789abcdef".repeat(4));
        assert_eq!(
            start_reference(&format!("localhost:5000/engine:cuda@{d}")),
            Ok(format!("localhost:5000/engine@{d}"))
        );
        assert!(start_reference("localhost:5000/engine@sha256:nothex").is_err());
        assert!(start_reference("localhost/engine:cuda")
            .unwrap_err()
            .contains("built on this machine"));
    }
}
