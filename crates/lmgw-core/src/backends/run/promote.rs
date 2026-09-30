//! Phase 7, promote (container-builds §5 step 7, §6 "Make current", §14.2):
//! the build's moving tag goes to the run's image, the old image's cached
//! flag vocabulary is dropped, the run is marked current — and then the
//! housekeeping: `keep_runs` retention, and the image a rebuild onto the same
//! immutable tag left dangling.
//!
//! Nothing here ever removes an image something uses. "Uses" is decided by
//! image **ID** ([`UsageIndex`]): a class default, a model's image override,
//! or a running container. A dev instance removes nothing at all — the image
//! store is production's too (§10).

use lmgw_api_types::builds::{KeptImage, PromoteResponse};

use super::log::RunLog;
use super::podman::{short_id, Podman};
use super::verify::immutable_tag_of;
use crate::backends::images::{describe_users, UsageIndex};
use crate::backends::model::{BuildRun, BuildRunPatch, BuildRunStatus};
use crate::backends::tags;
use crate::state::SharedState;
use crate::store;

/// Move `run`'s build's moving tag to `image` and mark the run current:
/// `(moving tag, the image it pointed at before)`.
async fn move_tag(
    state: &SharedState,
    run: &BuildRun,
    image: &str,
    log: &RunLog,
) -> Result<(String, Option<String>), String> {
    let moving = tags::moving_tag_for(run.engine, &run.slug, state.dev());
    let podman = Podman::of(state);
    let from = podman.image_id(&moving).await?;
    podman.tag(image, &moving).await?;
    match &from {
        Some(old) if old != image => {
            let dropped = state.runtime().forget_help(old);
            log.line(&format!(
                "moved {moving} from {} to {}{}",
                short_id(old),
                short_id(image),
                if dropped > 0 {
                    format!(" (dropped {dropped} cached flag vocabular(ies) of the old image)")
                } else {
                    String::new()
                }
            ));
        }
        Some(_) => log.line(&format!("{moving} already points at {}", short_id(image))),
        None => log.line(&format!("tagged {moving} → {}", short_id(image))),
    }
    store::promote_build_run(&state.db, run.id)
        .await
        .map_err(|e| e.to_string())?;
    if !run.tags.contains(&moving) {
        let mut tags = run.tags.clone();
        tags.push(moving.clone());
        store::update_build_run(
            &state.db,
            run.id,
            &BuildRunPatch {
                tags: Some(tags),
                ..BuildRunPatch::default()
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    }
    Ok((moving, from))
}

/// Step 7 for a run just verified: move the tag, then retention, then the
/// image `replaced` — the one the immutable tag named before a rebuild
/// retagged it — if it is now dangling and unused (§14.2: builds are not
/// reproducible, so the same inputs give a new ID). Only the tag move can
/// fail the step; the housekeeping logs what it did and what it kept.
pub(crate) async fn after_verify(
    state: &SharedState,
    run: &BuildRun,
    image: &str,
    replaced: Option<&str>,
    log: &RunLog,
) -> Result<(), String> {
    move_tag(state, run, image, log).await?;
    retention(state, run, log).await;
    if let Some(old) = replaced.filter(|old| *old != image) {
        remove_if_dangling(state, old, log).await;
    }
    Ok(())
}

/// `keep_runs` (§4, §5 step 7): of this build's previous runs' immutable
/// tags that still exist, keep the newest `keep_runs`, and `podman rmi` the
/// rest — unless in use, which keeps it and says by what. Each kept tag is
/// logged with its reason. Skipped entirely on a dev instance.
pub(crate) async fn retention(
    state: &SharedState,
    run: &BuildRun,
    log: &RunLog,
) -> (Vec<String>, Vec<KeptImage>) {
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    let Some(keep) = run.inputs.config.keep_runs else {
        log.line("keep_runs is empty: every previous run's image is kept");
        return (removed, kept);
    };
    if state.dev() {
        log.line(
            "keep_runs pruning skipped: this is a dev instance (LMGW_DEV), and the podman image \
             store is shared with the production lmgw",
        );
        return (removed, kept);
    }
    let Some(build_id) = run.build_id else {
        return (removed, kept);
    };
    let runs = match store::list_runs_for_build(&state.db, build_id).await {
        Ok(r) => r,
        Err(e) => {
            log.line(&format!("keep_runs pruning skipped: {e}"));
            return (removed, kept);
        }
    };
    let usage = match UsageIndex::load(state).await {
        Ok(u) => u,
        Err(e) => {
            log.line(&format!(
                "keep_runs pruning skipped: could not tell which images are in use ({e})"
            ));
            return (removed, kept);
        }
    };
    let current = immutable_tag_of(run);
    let podman = Podman::of(state);
    let mut seen: Vec<String> = current.iter().cloned().collect();
    let mut within = 0u32;
    // Newest first: the ones kept by count are the most recent.
    for r in runs.iter().filter(|r| r.id != run.id && r.status.built()) {
        let Some(tag) = immutable_tag_of(r) else {
            continue;
        };
        if seen.contains(&tag) {
            continue;
        }
        seen.push(tag.clone());
        let Some(id) = usage.id_of(&tag) else {
            continue; // already gone
        };
        let reason = if within < keep {
            within += 1;
            Some(format!(
                "one of the {keep} newest previous runs (keep_runs)"
            ))
        } else {
            let users = usage.users_of(&id);
            (!users.is_empty()).then(|| format!("in use by {}", describe_users(&users)))
        };
        match reason {
            Some(reason) => {
                log.line(&format!("kept {tag} (run {}): {reason}", r.id));
                kept.push(KeptImage { tag, reason });
            }
            None => match podman.rmi(&[&tag]).await {
                Ok(_) => {
                    log.line(&format!("removed {tag} (run {})", r.id));
                    removed.push(tag);
                }
                Err(e) => {
                    let reason = format!("could not be removed: {e}");
                    log.line(&format!("kept {tag} (run {}): {reason}", r.id));
                    kept.push(KeptImage { tag, reason });
                }
            },
        }
    }
    (removed, kept)
}

/// Remove the image `old` if nothing tags it any more and nothing uses it —
/// what a rebuild onto the same immutable tag leaves behind (§14.2).
async fn remove_if_dangling(state: &SharedState, old: &str, log: &RunLog) {
    if state.dev() {
        log.line(&format!(
            "the replaced image {} is left in place: this is a dev instance (LMGW_DEV)",
            short_id(old)
        ));
        return;
    }
    let podman = Podman::of(state);
    let tags = match podman.repo_tags(old).await {
        Ok(t) => t,
        Err(_) => return, // gone already
    };
    if !tags.is_empty() {
        log.line(&format!(
            "the replaced image {} is kept: it is still tagged {}",
            short_id(old),
            tags.join(", ")
        ));
        return;
    }
    let users = match UsageIndex::load(state).await {
        Ok(u) => u.users_of(old),
        Err(e) => {
            log.line(&format!(
                "the replaced image {} is kept: could not tell whether it is in use ({e})",
                short_id(old)
            ));
            return;
        }
    };
    if !users.is_empty() {
        log.line(&format!(
            "the replaced image {} is kept: in use by {}",
            short_id(old),
            describe_users(&users)
        ));
        return;
    }
    match podman.rmi(&[old]).await {
        Ok(_) => log.line(&format!(
            "removed the image this rebuild replaced ({}), now untagged and unused",
            short_id(old)
        )),
        Err(e) => log.line(&format!(
            "the replaced image {} could not be removed: {e}",
            short_id(old)
        )),
    }
}

/// `build_promote` (§15, §6 "Make current" / rollback): move the build's
/// moving tag to run `run_id`'s image. Allowed for a run that is
/// `succeeded`, or `unverified` (built, just not GPU-checked — the owner's
/// call); refused for a broken one and for one whose image is gone. No
/// retention here: rolling back must not prune the newer runs.
///
/// `used_by` is who the move concerns: every class default and model
/// override that names the moving tag (they follow it, and their running
/// containers are still on the old image), plus the running containers of
/// the old image — the success panel's "Recreate N running containers".
pub async fn promote_run(state: &SharedState, run_id: i64) -> Result<PromoteResponse, String> {
    let run = store::get_build_run(&state.db, run_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no build run with id {run_id}"))?;
    if run.build_id.is_none() {
        return Err(format!(
            "run {run_id} belongs to a deleted build, which has no moving tag to move"
        ));
    }
    match run.status {
        BuildRunStatus::Succeeded | BuildRunStatus::Unverified => {}
        BuildRunStatus::Broken => {
            return Err(format!(
                "run {run_id} is broken (a verify check failed) — Verify now, or run the build \
                 again, before making it current"
            ))
        }
        other => {
            return Err(format!(
                "run {run_id} built no image to make current (it ended {})",
                other.as_str()
            ))
        }
    }
    let image = run
        .image_id
        .clone()
        .ok_or_else(|| format!("run {run_id} recorded no image ID"))?;
    if Podman::of(state).image_id(&image).await?.is_none() {
        return Err(format!(
            "the image of run {run_id} ({}) is no longer on this machine",
            short_id(&image)
        ));
    }
    let log = RunLog::open(&super::log_path_of(state, &run))?;
    log.header("promote (Make current)");
    let (moving, from) = move_tag(state, &run, &image, &log).await?;
    let used_by = UsageIndex::load(state)
        .await?
        .followers(&moving, from.as_deref());
    Ok(PromoteResponse {
        moving_tag: moving,
        from_image: from,
        to_image: image,
        used_by,
    })
}
