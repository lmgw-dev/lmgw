//! The Backends ops (container-builds design §9.2, bound by §15): builds,
//! their runs, the forge pickers and the local images — one function per op,
//! shared by the dashboard (`POST /api/op/<name>`) and the `lmgw__build*` /
//! `lmgw__container_image*` / `lmgw__forge_prs` self-admin tools.
//!
//! Unlike most of [`crate::ops`], the ops here take and return the typed
//! shapes of [`lmgw_api_types::builds`]: the Backends page deserializes
//! exactly those, so a field renamed on one side is a compile error rather
//! than an empty column. The dispatchers turn them into JSON at the edge.
//!
//! Everything below the ops layer lives in [`crate::backends`]; these
//! functions only pick arguments apart, compose, and put errors into words.
//! The one piece of logic of their own is `build_set`'s **delete with
//! images**, which decides per image what may go (see [`remove_build_images`]).
//!
//! The tool plane cannot send nested JSON (see [`crate::mcp::selfadmin`]), so
//! `lmgw__build_set` goes through [`BuildPatch`] — a flat, sparse patch whose
//! list fields are text — and [`build_patch`] turns it into the same
//! [`BuildSetArgs`] the dashboard sends.

use lmgw_api_types::builds::{
    Build, BuildCheckMergeArgs, BuildEnv, BuildExtra, BuildGetResponse, BuildPromoteArgs,
    BuildResolveArgs, BuildRunArgs, BuildRunLogArgs, BuildRunStarted, BuildSetAction, BuildSetArgs,
    BuildSetResponse, BuildSpec, BuildTrigger, BuildUpdateEntry, BuildUpdatesCheckArgs,
    BuildUpdatesResponse, BuildVerifyArgs, BuildView, BuildsGetArgs, BuildsResponse,
    CheckMergeReport, ContainerImageDeleteArgs, ContainerImageDeleteResponse,
    ContainerImagePullArgs, ContainerImagePullStarted, ContainerImagePullStatus,
    ContainerImagePullStatusArgs, ContainerImageTagArgs, ContainerImageTagResponse,
    ContainerImagesArgs, ContainerImagesResponse, Engine, Forge, ForgePr, ForgePrArgs, ForgePrPage,
    ForgePrsArgs, ForgeRefsArgs, GpuBackend, ImageUse, KeptImage, PromoteResponse, RemoteRefsView,
    ResolvedPreview, RunLogChunk, VerifyReport,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::backends::images::{self, describe_users, UsageIndex};
use crate::backends::run::{self, short_id, Podman};
use crate::backends::{forge, presets, pull, tags, updates, validate};
use crate::error::GatewayError;
use crate::hf::fmt_bytes;
use crate::jobs::JobKind;
use crate::state::SharedState;
use crate::store;

/// A store error in words: the store's own sentence, without the
/// `bad request:` / `not found:` prefix its `Display` adds for HTTP.
fn db(e: GatewayError) -> String {
    match e {
        GatewayError::BadRequest(m) | GatewayError::NotFound(m) => m,
        other => other.to_string(),
    }
}

/// A typed op result as the JSON both dispatchers answer with.
pub fn to_json<T: Serialize>(r: Result<T, String>) -> Result<Value, String> {
    serde_json::to_value(r?).map_err(|e| format!("serializing the answer: {e}"))
}

// ---------------------------------------------------------------------------
// Builds
// ---------------------------------------------------------------------------

/// Who uses what, once per call — `podman images` and `podman ps` — or why
/// that could not be told. Never an empty index standing in for an unknown
/// one: "nothing uses this image" is what a delete button reads.
async fn usage(state: &SharedState) -> (Option<UsageIndex>, Option<String>) {
    match UsageIndex::load(state).await {
        Ok(u) => (Some(u), None),
        Err(e) => (
            None,
            Some(format!(
                "who uses each build's image could not be determined, so every used_by is empty \
                 for that reason, not because nothing uses it: {e}"
            )),
        ),
    }
}

/// The users of a build's moving tag (§15 `BuildView.used_by`): everything
/// using the image it names now — by that tag or any other name, and the
/// containers running it — plus the class defaults and model overrides that
/// name the tag while it names nothing on this machine (they follow the
/// build, and are broken until it is built).
fn moving_tag_users(usage: &UsageIndex, moving: &str) -> Vec<ImageUse> {
    let mut users = usage
        .id_of(moving)
        .map(|id| usage.users_of(&id))
        .unwrap_or_default();
    for u in usage.followers(moving, None) {
        if !users.contains(&u) {
            users.push(u);
        }
    }
    users
}

/// One Builds-tab row: the build, its newest and its current (promoted) run,
/// its live job, who uses its moving tag, and its update badge — the last
/// check's remote facts evaluated against the build and its runs as they are
/// now (§8, [`updates::status_for`]).
async fn view_of(
    state: &SharedState,
    build: Build,
    usage: Option<&UsageIndex>,
) -> Result<BuildView, String> {
    // The same order `build_get` pages the history in, so the row's newest
    // run is the history's first.
    let last_run = store::list_build_runs_page(&state.db, build.id, None, Some(1))
        .await
        .map_err(db)?
        .0
        .into_iter()
        .next();
    let current_run = store::promoted_build_run(&state.db, build.id)
        .await
        .map_err(db)?;
    let live_job_id = state
        .jobs
        .live_by_key(JobKind::BuildRun, &run::job_key(build.id))
        .map(|j| j.id);
    // This instance's namespace — what its runs tag (a dev instance's builds
    // follow `localhost/lmgw-dev-…`, never production's tag).
    let moving = tags::moving_tag_for(build.spec.engine, &build.spec.slug, state.dev());
    let used_by = usage
        .map(|u| moving_tag_users(u, &moving))
        .unwrap_or_default();
    let update = updates::status_for(state, &build).await;
    Ok(BuildView {
        build,
        last_run,
        current_run,
        live_job_id,
        used_by,
        update,
        moving_tag: moving,
    })
}

async fn get_build(state: &SharedState, id: i64) -> Result<Build, String> {
    store::get_build(&state.db, id)
        .await
        .map_err(db)?
        .ok_or_else(|| format!("no build with id {id}"))
}

/// `builds` (§15): every build, by slug, each with its runs' summary, live
/// job and users. The image usage is read once for all of them.
pub async fn builds(state: &SharedState) -> Result<BuildsResponse, String> {
    let rows = store::list_builds(&state.db).await.map_err(db)?;
    let (index, usage_error) = if rows.is_empty() {
        (None, None)
    } else {
        usage(state).await
    };
    let mut builds = Vec::with_capacity(rows.len());
    for b in rows {
        builds.push(view_of(state, b, index.as_ref()).await?);
    }
    Ok(BuildsResponse {
        builds,
        usage_error,
    })
}

/// `build_get` (§15): one build's row plus a page of its runs, newest first.
/// `limit` is the caller's page size — absent means every run; `before` is an
/// exclusive run-id cursor (the oldest run id already shown; `run_id + 1`
/// with `limit` 1 fetches exactly that run).
pub async fn build_get(
    state: &SharedState,
    args: BuildsGetArgs,
) -> Result<BuildGetResponse, String> {
    let build = get_build(state, args.id).await?;
    let (index, usage_error) = usage(state).await;
    let (runs, more) = store::list_build_runs_page(&state.db, build.id, args.before, args.limit)
        .await
        .map_err(db)?;
    Ok(BuildGetResponse {
        view: view_of(state, build, index.as_ref()).await?,
        runs,
        more,
        usage_error,
    })
}

/// `build_set` (§15): create, update, delete or duplicate a build.
///
/// - `create` / `update` take the whole definition in `spec`, checked by
///   [`validate::validate_build`] (whose refusals name the field). `update`
///   replaces the definition; the slug is fixed once the build has a run.
/// - `duplicate` copies `id` under `slug` / `name` (defaults: the first free
///   `<slug>-copy[-N]`, `<name> (copy)`).
/// - `delete` removes the row — its runs stay, without a build — and with
///   `delete_images` also this build's images where nothing uses them
///   ([`remove_build_images`]). Refused while the build has a live run.
pub async fn build_set(
    state: &SharedState,
    args: BuildSetArgs,
) -> Result<BuildSetResponse, String> {
    let resp = build_set_inner(state, args).await?;
    // An edit can start "definition changed since last run"; a delete ends
    // a badge (§8).
    updates::publish(state).await;
    Ok(resp)
}

/// A `cpus` this podman cannot enforce is refused on save, with what to
/// delegate, rather than failing the next run after its clone. When podman
/// cannot be asked the definition is saved as it is, and the note saying so
/// comes back with the save — not only in a later run's log.
async fn cpus_enforceable(state: &SharedState, spec: &BuildSpec) -> Result<Vec<String>, String> {
    run::check_cpus(&Podman::of(state), spec.cpus.as_deref())
        .await
        .map(|note| note.into_iter().collect())
}

async fn build_set_inner(
    state: &SharedState,
    args: BuildSetArgs,
) -> Result<BuildSetResponse, String> {
    let built = |b: Build, notes: Vec<String>| BuildSetResponse {
        build: Some(b),
        notes,
        ..BuildSetResponse::default()
    };
    match args.action {
        BuildSetAction::Create => {
            let spec = args
                .spec
                .ok_or("create needs spec: the new build's fields")?;
            let spec = validate::validate_build(spec)?;
            let notes = cpus_enforceable(state, &spec).await?;
            let id = store::insert_build(&state.db, &spec).await.map_err(db)?;
            Ok(built(get_build(state, id).await?, notes))
        }
        BuildSetAction::Update => {
            let id = args.id.ok_or("update needs the build's id")?;
            let spec = args.spec.ok_or(
                "update needs spec: every field of the build (an update replaces the \
                 definition)",
            )?;
            let spec = validate::validate_build(spec)?;
            let notes = cpus_enforceable(state, &spec).await?;
            store::update_build(&state.db, id, &spec)
                .await
                .map_err(db)?;
            Ok(built(get_build(state, id).await?, notes))
        }
        BuildSetAction::Duplicate => {
            let id = args
                .id
                .ok_or("duplicate needs the id of the build to copy")?;
            let copy =
                store::duplicate_build(&state.db, id, args.slug.as_deref(), args.name.as_deref())
                    .await
                    .map_err(db)?;
            Ok(built(get_build(state, copy).await?, Vec::new()))
        }
        BuildSetAction::Delete => {
            let id = args.id.ok_or("delete needs the build's id")?;
            delete_build(state, id, args.delete_images).await
        }
    }
}

async fn delete_build(
    state: &SharedState,
    id: i64,
    delete_images: bool,
) -> Result<BuildSetResponse, String> {
    let build = get_build(state, id).await?;
    if let Some(job) = state.jobs.live_by_key(JobKind::BuildRun, &run::job_key(id)) {
        return Err(format!(
            "build '{}' has a run in progress (job {}) — cancel it (job_cancel) or let it finish, \
             then delete the build",
            build.spec.slug, job.id
        ));
    }
    // Read before the row goes: afterwards its runs no longer name it.
    let runs = store::list_runs_for_build(&state.db, id)
        .await
        .map_err(db)?;
    store::delete_build(&state.db, id).await.map_err(db)?;
    let mut resp = BuildSetResponse::default();
    if delete_images {
        let mut own: Vec<String> = runs.iter().flat_map(|r| r.tags.iter().cloned()).collect();
        // The moving tag of every engine the build ever had (a build can be
        // switched to another engine), in this instance's namespace.
        for engine in std::iter::once(build.spec.engine).chain(runs.iter().map(|r| r.engine)) {
            own.push(tags::moving_tag_for(engine, &build.spec.slug, state.dev()));
        }
        let mut seen = std::collections::HashSet::new();
        own.retain(|t| seen.insert(t.clone()));
        let (removed, kept) = remove_build_images(state, &own).await;
        resp.removed_images = removed;
        resp.kept = kept;
    }
    // Cache cleanup is independent of `delete_images`: it is disk hygiene for
    // an id nothing will ever build into again, not an image the build's
    // class defaults or model overrides could still be pointed at.
    resp.removed_caches = remove_build_caches(state, &build, &runs).await;
    Ok(resp)
}

/// Remove a deleted build's images — `own` is every tag its runs put on an
/// image, and its moving tag — and say what was kept and why. Decided per
/// image, by ID:
///
/// - an image carrying only this build's tags goes as a whole (every tag,
///   then the image), unless anything uses it — a class default or model
///   override by any name, or a running container;
/// - an image that also carries a name from outside this build only loses
///   this build's tags (each kept while a class default or model override
///   names it), and stays under its other names;
/// - nothing at all on a dev instance (§10): the image store is production's.
///
/// A tag that names no image any more is skipped silently — it is already
/// gone, which is what was asked.
async fn remove_build_images(state: &SharedState, own: &[String]) -> (Vec<String>, Vec<KeptImage>) {
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    let usage = match UsageIndex::load(state).await {
        Ok(u) => u,
        Err(e) => {
            let reason = format!("not removed: could not tell whether it is in use ({e})");
            kept.extend(own.iter().map(|tag| KeptImage {
                tag: tag.clone(),
                reason: reason.clone(),
            }));
            return (removed, kept);
        }
    };
    // Group the build's tags by the image each names now.
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for tag in own {
        let Some((img, Some(name))) = images::resolve(tag, &usage.images) else {
            continue;
        };
        match groups.iter_mut().find(|(id, _)| *id == img.id) {
            Some((_, names)) => {
                if !names.contains(&name) {
                    names.push(name)
                }
            }
            None => groups.push((img.id.clone(), vec![name])),
        }
    }
    let dev = state.refuse_in_dev("deleting a build's images").err();
    let podman = Podman::of(state);
    for (id, names) in groups {
        let keep_all = |reason: String, kept: &mut Vec<KeptImage>| {
            kept.extend(names.iter().map(|tag| KeptImage {
                tag: tag.clone(),
                reason: reason.clone(),
            }))
        };
        if let Some(why) = &dev {
            keep_all(why.clone(), &mut kept);
            continue;
        }
        let Some(img) = usage.images.iter().find(|i| i.id == id) else {
            continue;
        };
        let others: Vec<&String> = img.names.iter().filter(|n| !names.contains(n)).collect();
        if others.is_empty() {
            let users = usage.users_of(&id);
            if !users.is_empty() {
                keep_all(
                    format!(
                        "image {} is in use by {}",
                        short_id(&id),
                        describe_users(&users)
                    ),
                    &mut kept,
                );
                continue;
            }
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            match podman.rmi(&refs).await {
                Ok(_) => removed.extend(names.iter().cloned()),
                Err(e) => keep_all(format!("could not be removed: {e}"), &mut kept),
            }
            continue;
        }
        for name in &names {
            let users = usage.users_of_name(name);
            if !users.is_empty() {
                kept.push(KeptImage {
                    tag: name.clone(),
                    reason: format!("named by {}", describe_users(&users)),
                });
                continue;
            }
            match podman.untag(&id, name).await {
                Ok(()) => removed.push(name.clone()),
                Err(e) => kept.push(KeptImage {
                    tag: name.clone(),
                    reason: format!("could not be untagged: {e}"),
                }),
            }
        }
        for other in others {
            kept.push(KeptImage {
                tag: other.clone(),
                reason: format!(
                    "not one of this build's tags — image {} stays under it",
                    short_id(&id)
                ),
            });
        }
    }
    (removed, kept)
}

/// A deleted build's own per-build cache-mount directories — the
/// slug-suffixed ccache and npm ids of its engine and backend now and of
/// every engine and backend one of its `runs` was built with
/// ([`tags::own_cache_ids_of`]), which by construction are never the
/// shared, non-suffixed ids a plain build's run writes into and never
/// another build's — removed from buildah's cache root
/// under `/var/tmp` (§14.2), each one reported with its size when that could
/// be read for free from the same walk that removes it.
///
/// Never touches anything on a dev instance: its `/var/tmp` is the real
/// host's, shared with production, and a dev build of lmgw has no business
/// judging which of production's cache directories to remove — it is kept
/// (and logged), the same [`AppState::dev`](crate::state::AppState::dev)
/// check that gates image deletion. Never reached with a run of this build
/// still live: `delete_build` already refuses the delete before this runs.
async fn remove_build_caches(
    state: &SharedState,
    build: &Build,
    runs: &[lmgw_api_types::builds::BuildRun],
) -> Vec<String> {
    if state.dev() {
        tracing::info!(
            "build '{}' delete: per-build cache dirs kept (dev instance; LMGW_DEV is set)",
            build.spec.slug
        );
        return Vec::new();
    }
    let spec = &build.spec;
    let root = run::cache_mount_root(&state.builds.buildah_tmp(), run::real_uid());
    let podman = Podman::of(state);
    let mut removed = Vec::new();
    for id in tags::own_cache_ids_of(spec, runs) {
        let dir = run::cache_mount_dir(&root, &id);
        match podman.remove_cache_dir(&dir).await {
            Ok(Some(size)) => removed.push(format!("{id} ({})", fmt_bytes(size))),
            Ok(None) => {} // never built with extras (or already gone): nothing to remove
            Err(e) => tracing::warn!(
                "build '{}' delete: could not remove cache dir {} ({id}): {e}",
                spec.slug,
                dir.display()
            ),
        }
    }
    removed
}

/// `build_resolve` (§15): what a spec resolves to right now, fetched but not
/// built.
pub async fn build_resolve(
    state: &SharedState,
    args: BuildResolveArgs,
) -> Result<ResolvedPreview, String> {
    run::resolve_preview(state, args.spec).await
}

/// `build_check_merge` (§15): base + extras assembled in a throwaway
/// worktree, each extra's outcome reported.
pub async fn build_check_merge(
    state: &SharedState,
    args: BuildCheckMergeArgs,
) -> Result<CheckMergeReport, String> {
    run::check_merge(state, args).await
}

/// `build_run` (§15): start a run. `trigger` is who asked — the dashboard
/// (`manual`) or a tool call (`mcp`) — and is recorded on the run.
pub async fn build_run(
    state: &SharedState,
    args: BuildRunArgs,
    trigger: BuildTrigger,
) -> Result<BuildRunStarted, String> {
    run::start_run(state, args.id, trigger, args.rebuild).await
}

/// `build_run_log` (§15): a byte-offset chunk of a run's log (at most
/// [`run::LOG_CHUNK_BYTES`]), or with `tail` its last `tail` lines. Asking
/// for both a non-zero offset and a tail is refused — they are two different
/// places to start.
pub async fn build_run_log(
    state: &SharedState,
    args: BuildRunLogArgs,
) -> Result<RunLogChunk, String> {
    match args.tail {
        Some(_) if args.offset != 0 => Err(format!(
            "pass offset {} or tail {}, not both — offset reads on from a byte position, tail \
             reads the last lines",
            args.offset,
            args.tail.unwrap_or_default()
        )),
        Some(lines) => run::read_log_tail(state, args.run_id, lines).await,
        None => run::read_log(state, args.run_id, args.offset).await,
    }
}

/// `build_promote` (§15, §6 "Make current").
pub async fn build_promote(
    state: &SharedState,
    args: BuildPromoteArgs,
) -> Result<PromoteResponse, String> {
    let resp = run::promote_run(state, args.run_id).await?;
    // "Make current" changes the run the badge compares against (§8).
    updates::publish(state).await;
    Ok(resp)
}

/// `build_verify` (§15, "Verify now").
pub async fn build_verify(
    state: &SharedState,
    args: BuildVerifyArgs,
) -> Result<VerifyReport, String> {
    let report = run::verify_run(state, args.run_id).await?;
    // A run turning `succeeded` can become the one the badge compares against.
    updates::publish(state).await;
    Ok(report)
}

/// `build_updates_check` (§15, §8 **Check now**): one build (`id`), or every
/// build and every registry image in use. Asks the remotes now; the answer
/// is each checked build's badge. A build with no succeeded or unverified run
/// has nothing to compare with — its `update` is `None` and nothing is asked
/// for it.
pub async fn build_updates_check(
    state: &SharedState,
    args: BuildUpdatesCheckArgs,
) -> Result<BuildUpdatesResponse, String> {
    match args.id {
        Some(id) => Ok(BuildUpdatesResponse {
            updates: vec![BuildUpdateEntry {
                build_id: id,
                update: updates::check_build(state, id).await?,
            }],
        }),
        None => updates::check_all(state).await,
    }
}

/// `build_env` (§15): what the build editor needs when it opens.
pub async fn build_env(state: &SharedState) -> Result<BuildEnv, String> {
    Ok(run::build_env(state).await)
}

// ---------------------------------------------------------------------------
// Forge
// ---------------------------------------------------------------------------

/// `forge_refs` (§15): the remote's branches and tags, for the ref combobox.
pub async fn forge_refs(
    state: &SharedState,
    args: ForgeRefsArgs,
) -> Result<RemoteRefsView, String> {
    forge::session(state)?.refs(args.repo_url.trim()).await
}

/// `forge_prs` (§15): one page of a repository's open PRs/MRs, or the one a
/// number or pasted URL in `query` names.
pub async fn forge_prs(state: &SharedState, args: ForgePrsArgs) -> Result<ForgePrPage, String> {
    forge::session(state)?
        .prs(
            args.repo_url.trim(),
            args.forge,
            args.query.as_deref(),
            args.page,
        )
        .await
}

/// `forge_pr` (§15): one PR's/MR's current state.
pub async fn forge_pr(state: &SharedState, args: ForgePrArgs) -> Result<ForgePr, String> {
    forge::session(state)?
        .pr(args.repo_url.trim(), args.forge, args.number)
        .await
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// `container_images` (§15): one entry per image ID. `disk: false` skips the
/// disk footer (the slow part) for a caller that shows none.
pub async fn container_images(
    state: &SharedState,
    args: ContainerImagesArgs,
) -> Result<ContainerImagesResponse, String> {
    images::list_images(state, args.engine, args.disk).await
}

/// `container_image_delete` (§15): refused while in use unless forced, and
/// always on a dev instance.
pub async fn container_image_delete(
    state: &SharedState,
    args: ContainerImageDeleteArgs,
) -> Result<ContainerImageDeleteResponse, String> {
    images::delete_image(state, args.image.trim(), args.force).await
}

/// `container_image_pull` (§8 **Pull update**): `podman pull` as a
/// background job. Recreates nothing; allowed on a dev instance (it deletes
/// nothing production uses).
pub async fn container_image_pull(
    state: &SharedState,
    args: ContainerImagePullArgs,
) -> Result<ContainerImagePullStarted, String> {
    pull::start_pull(state, &args.image).await
}

/// `container_image_pull_status`: what a pull job is doing, or did — the
/// Pull update panel's result and its "Recreate N running containers".
pub async fn container_image_pull_status(
    state: &SharedState,
    args: ContainerImagePullStatusArgs,
) -> Result<ContainerImagePullStatus, String> {
    pull::status(state, args.job_id).await
}

/// `container_image_tag` (§15).
pub async fn container_image_tag(
    state: &SharedState,
    args: ContainerImageTagArgs,
) -> Result<ContainerImageTagResponse, String> {
    images::tag_image(
        state,
        args.image.trim(),
        args.add.as_deref(),
        args.remove.as_deref(),
    )
    .await
}

// ---------------------------------------------------------------------------
// The tool plane's shapes
// ---------------------------------------------------------------------------

/// How many runs `lmgw__builds id=…` returns when the caller names no
/// `limit`: a page, with `more` saying whether older runs exist and `before`
/// reaching them. Stated in the tool's description.
pub const TOOL_RUNS_PAGE: u32 = 10;

/// How many lines `lmgw__build_log` returns when the caller names neither
/// `offset` nor `tail`: the end of the log, which is where a run's outcome
/// is — rather than the first megabyte. Stated in the tool's description.
pub const TOOL_LOG_TAIL: usize = 200;

/// `lmgw__build_log`: the `build_run_log` op, with its tool-plane default —
/// neither `offset` nor `tail` given reads the last [`TOOL_LOG_TAIL`] lines.
pub async fn build_log_tool(
    state: &SharedState,
    run_id: i64,
    offset: Option<i64>,
    tail: Option<i64>,
) -> Result<Value, String> {
    let offset = offset
        .map(|o| u64::try_from(o).map_err(|_| format!("offset {o} is not a byte offset")))
        .transpose()?;
    let tail = tail
        .map(|t| usize::try_from(t).map_err(|_| format!("tail {t} is not a line count")))
        .transpose()?;
    let tail = match (offset, tail) {
        (Some(_), Some(_)) => {
            return Err(
                "pass offset or tail, not both — offset reads on from a byte position, tail \
                 reads the last lines"
                    .into(),
            )
        }
        (None, None) => Some(TOOL_LOG_TAIL),
        (_, t) => t,
    };
    to_json(
        build_run_log(
            state,
            BuildRunLogArgs {
                run_id,
                offset: offset.unwrap_or(0),
                tail,
            },
        )
        .await,
    )
}

/// `lmgw__builds`: without `id`, every build (the `builds` op) plus the repo
/// presets `lmgw__build_set preset=` accepts; with `id`, that build and a
/// page of its runs (the `build_get` op).
pub async fn builds_tool(
    state: &SharedState,
    id: Option<i64>,
    limit: Option<i64>,
    before: Option<i64>,
) -> Result<Value, String> {
    let Some(id) = id else {
        let mut v = to_json(builds(state).await)?;
        v["repo_presets"] = presets::REPO_PRESETS
            .iter()
            .map(|p| {
                json!({
                    "preset": p.id,
                    "name": p.name,
                    "engine": p.engine,
                    "repo_url": p.repo_url,
                    "forge": p.forge,
                    "default_ref": p.default_ref,
                })
            })
            .collect();
        return Ok(v);
    };
    let limit = match limit {
        None => TOOL_RUNS_PAGE,
        Some(n) => u32::try_from(n).map_err(|_| format!("limit {n} is not a run count"))?,
    };
    to_json(
        build_get(
            state,
            BuildsGetArgs {
                id,
                limit: Some(limit),
                before,
            },
        )
        .await,
    )
}

/// `lmgw__forge_prs`: the `forge_prs` op, with the repository taken from a
/// saved build when `id` is given.
pub async fn forge_prs_tool(
    state: &SharedState,
    id: Option<i64>,
    repo_url: Option<&str>,
    forge_name: Option<&str>,
    query: Option<&str>,
    page: Option<i64>,
) -> Result<Value, String> {
    let (repo_url, chosen) = match (id, repo_url) {
        (Some(id), _) => {
            let b = get_build(state, id).await?;
            (b.spec.repo_url, b.spec.forge)
        }
        (None, Some(url)) if !url.trim().is_empty() => (url.trim().to_string(), Forge::Plain),
        _ => return Err("pass repo_url, or id of a build whose repository to list".into()),
    };
    let forge = match forge_name {
        Some(f) => parse_forge(f)?,
        None if id.is_some() => chosen,
        None => {
            let tokens = state.snapshot().settings.forge_tokens.clone();
            implied_forge(&repo_url, |h| tokens.contains_key(h))
        }
    };
    let page = page
        .map(|p| u32::try_from(p).map_err(|_| format!("page {p} is not a page number")))
        .transpose()?;
    to_json(
        forge_prs(
            state,
            ForgePrsArgs {
                repo_url,
                forge,
                query: query.map(str::to_string),
                page,
            },
        )
        .await,
    )
}

/// The forge a repository without a chosen one gets
/// ([`validate::default_forge`]), with "has a token" asked under the
/// repository's [`forge::token_host`] — the key the token is looked up by
/// when it is used, so the guess and the use agree on ports.
fn implied_forge(repo_url: &str, has_token: impl Fn(&str) -> bool) -> Forge {
    let has = forge::token_host(repo_url).is_some_and(|h| has_token(&h));
    validate::default_forge(repo_url, |_| has)
}

fn parse_engine(s: &str) -> Result<Engine, String> {
    Engine::parse(s.trim()).ok_or_else(|| format!("engine '{s}' is not one of llama, audio, sdcpp"))
}

fn parse_forge(s: &str) -> Result<Forge, String> {
    Forge::parse(s.trim()).ok_or_else(|| format!("forge '{s}' is not one of github, gitlab, plain"))
}

fn parse_backend(s: &str) -> Result<GpuBackend, String> {
    GpuBackend::parse(s.trim())
        .ok_or_else(|| format!("backend '{s}' is not one of cuda, vulkan, rocm, cpu"))
}

/// The `extras` text of [`BuildPatch`]: one extra per line, merged in order.
///
/// - `pr <number> [<pin>]` — a PR/MR of the build's own repository
///   (`#123` / `!45` are accepted for the number);
/// - `ref <remote_url> <ref> [<pin>]` — a branch, tag, commit or full ref
///   of another remote.
///
/// A pin is a full commit SHA, optionally written `pin=<sha>`. Blank lines
/// are skipped; anything else that does not parse is refused with its line.
pub fn parse_extras(text: &str) -> Result<Vec<BuildExtra>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        let at = format!("extras line {} ('{}')", n + 1, line.trim());
        let pin = |w: Option<&&str>| w.map(|p| p.trim_start_matches("pin=").to_string());
        let extra = match words[0].to_ascii_lowercase().as_str() {
            "pr" | "mr" => {
                if !(2..=3).contains(&words.len()) {
                    return Err(format!("{at}: write 'pr <number>' or 'pr <number> <pin>'"));
                }
                let number = words[1]
                    .trim_start_matches(['#', '!'])
                    .parse::<u64>()
                    .map_err(|_| format!("{at}: '{}' is not a PR number", words[1]))?;
                BuildExtra::Pr {
                    number,
                    pin: pin(words.get(2)),
                }
            }
            "ref" => {
                if !(3..=4).contains(&words.len()) {
                    return Err(format!(
                        "{at}: write 'ref <remote_url> <ref>' or 'ref <remote_url> <ref> <pin>'"
                    ));
                }
                BuildExtra::Ref {
                    remote_url: words[1].to_string(),
                    git_ref: words[2].to_string(),
                    pin: pin(words.get(3)),
                }
            }
            other => {
                return Err(format!(
                    "{at}: an extra starts with 'pr' or 'ref', not '{other}'"
                ))
            }
        };
        out.push(extra);
    }
    Ok(out)
}

/// The `arch` text of [`BuildPatch`]: compute capabilities separated by
/// commas or spaces; empty or `auto` means auto-detect.
fn parse_arch(text: &str) -> Option<Vec<String>> {
    let list: Vec<String> = text
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    match list.as_slice() {
        [] => None,
        [one] if one.eq_ignore_ascii_case("auto") => None,
        _ => Some(list),
    }
}

/// `lmgw__build_set`'s arguments: flat and sparse (§20's tool-plane rule —
/// no nested JSON). `update` overlays only what is given on the stored
/// definition; `create` starts from the defaults, or from `preset`. The
/// Dockerfile edits are not settable here (they are find/replace blocks);
/// a new build uses the preset's, an update keeps the stored ones unless
/// `reset_edits` drops them back to the preset's.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BuildPatch {
    pub action: String,
    pub id: Option<i64>,
    pub preset: Option<String>,
    pub slug: Option<String>,
    pub name: Option<String>,
    pub engine: Option<String>,
    pub repo_url: Option<String>,
    pub forge: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub extras: Option<String>,
    pub backend: Option<String>,
    pub cuda_version: Option<String>,
    pub arch: Option<String>,
    pub dockerfile: Option<String>,
    pub target: Option<String>,
    pub reset_edits: Option<bool>,
    pub ccache: Option<bool>,
    pub ccache_max_size: Option<String>,
    pub cpus: Option<String>,
    pub build_args: Option<String>,
    pub keep_layers: Option<bool>,
    pub keep_runs: Option<i64>,
    pub notes: Option<String>,
    pub delete_images: Option<bool>,
}

impl BuildPatch {
    /// Overlay the given fields on `spec`.
    fn apply(&self, spec: &mut BuildSpec, has_token: impl Fn(&str) -> bool) -> Result<(), String> {
        if let Some(id) = &self.preset {
            let p = presets::repo_preset(id.trim()).ok_or_else(|| {
                let ids: Vec<&str> = presets::REPO_PRESETS.iter().map(|p| p.id).collect();
                format!("preset '{id}' is not one of {}", ids.join(", "))
            })?;
            spec.engine = p.engine;
            spec.repo_url = p.repo_url.to_string();
            spec.forge = p.forge;
            spec.git_ref = p.default_ref.to_string();
        }
        if let Some(v) = &self.slug {
            spec.slug = v.clone();
        }
        if let Some(v) = &self.name {
            spec.name = v.clone();
        }
        if let Some(v) = &self.engine {
            spec.engine = parse_engine(v)?;
        }
        if let Some(v) = &self.repo_url {
            spec.repo_url = v.trim().to_string();
        }
        match (&self.forge, &self.repo_url) {
            (Some(f), _) => spec.forge = parse_forge(f)?,
            // A new repository without a forge gets the one its host implies.
            (None, Some(url)) => spec.forge = implied_forge(url.trim(), has_token),
            (None, None) => {}
        }
        if let Some(v) = &self.git_ref {
            spec.git_ref = v.clone();
        }
        if let Some(v) = &self.extras {
            spec.extras = parse_extras(v)?;
        }
        if let Some(v) = &self.backend {
            spec.backend = parse_backend(v)?;
        }
        if let Some(v) = &self.cuda_version {
            spec.cuda_version = Some(v.clone());
        }
        if let Some(v) = &self.arch {
            spec.arch = parse_arch(v);
        }
        if let Some(v) = &self.dockerfile {
            spec.dockerfile = Some(v.clone());
        }
        if let Some(v) = &self.target {
            spec.target = Some(v.clone());
        }
        if self.reset_edits == Some(true) {
            spec.edits = None;
        }
        if let Some(v) = self.ccache {
            spec.ccache = v;
        }
        if let Some(v) = &self.ccache_max_size {
            spec.ccache_max_size = v.clone();
        }
        if let Some(v) = &self.cpus {
            spec.cpus = Some(v.clone());
        }
        if let Some(v) = &self.build_args {
            spec.build_args = v.clone();
        }
        if let Some(v) = self.keep_layers {
            spec.keep_layers = v;
        }
        if let Some(n) = self.keep_runs {
            spec.keep_runs = if n < 0 {
                None
            } else {
                Some(u32::try_from(n).map_err(|_| format!("keep_runs {n} is too large"))?)
            };
        }
        if let Some(v) = &self.notes {
            spec.notes = v.clone();
        }
        Ok(())
    }
}

/// `lmgw__build_set`: [`BuildPatch`] → [`BuildSetArgs`] → [`build_set`], with
/// the next step named in the answer.
pub async fn build_patch(state: &SharedState, p: BuildPatch) -> Result<Value, String> {
    let action = match p.action.trim() {
        "create" => BuildSetAction::Create,
        "update" => BuildSetAction::Update,
        "delete" => BuildSetAction::Delete,
        "duplicate" => BuildSetAction::Duplicate,
        other => {
            return Err(format!(
                "action '{other}' is not one of create, update, delete, duplicate"
            ))
        }
    };
    let tokens = state.snapshot().settings.forge_tokens.clone();
    let has_token = |h: &str| tokens.contains_key(h);
    let spec = match action {
        BuildSetAction::Create => {
            let mut spec = BuildSpec::default();
            p.apply(&mut spec, has_token)?;
            Some(spec)
        }
        BuildSetAction::Update => {
            let id =
                p.id.ok_or("update needs the build's id (see lmgw__builds)")?;
            let mut spec = get_build(state, id).await?.spec;
            p.apply(&mut spec, has_token)?;
            Some(spec)
        }
        BuildSetAction::Delete | BuildSetAction::Duplicate => None,
    };
    let resp = build_set(
        state,
        BuildSetArgs {
            action,
            id: p.id,
            spec,
            slug: p.slug.clone(),
            name: p.name.clone(),
            delete_images: p.delete_images.unwrap_or(false),
        },
    )
    .await?;
    let mut v = to_json::<BuildSetResponse>(Ok(resp.clone()))?;
    if let Some(b) = &resp.build {
        v["next_step"] = json!(format!(
            "lmgw__build_run id={} starts a run (it takes minutes; poll lmgw__builds id={} and \
             read lmgw__build_log). With extras, lmgw__build_check_merge id={} first says in \
             seconds whether they merge cleanly.",
            b.id, b.id, b.id
        ));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extras_text_parses_prs_refs_and_pins() {
        let pin = "a".repeat(40);
        let got = parse_extras(&format!(
            "pr 16391\n\n  pr #7 pin={pin}\nref https://github.com/fork/llama.cpp feature-x\n\
             ref https://github.com/o/r refs/pull/9/head {pin}\n"
        ))
        .unwrap();
        assert_eq!(
            got,
            vec![
                BuildExtra::Pr {
                    number: 16391,
                    pin: None
                },
                BuildExtra::Pr {
                    number: 7,
                    pin: Some(pin.clone())
                },
                BuildExtra::Ref {
                    remote_url: "https://github.com/fork/llama.cpp".into(),
                    git_ref: "feature-x".into(),
                    pin: None
                },
                BuildExtra::Ref {
                    remote_url: "https://github.com/o/r".into(),
                    git_ref: "refs/pull/9/head".into(),
                    pin: Some(pin)
                },
            ]
        );
        assert!(parse_extras("").unwrap().is_empty());
        let err = parse_extras("pr 1\nbranch x").unwrap_err();
        assert!(err.contains("line 2"), "{err}");
        assert!(parse_extras("pr twelve").unwrap_err().contains("PR number"));
        assert!(parse_extras("ref https://x/y").is_err());
    }

    #[test]
    fn a_token_implies_gitlab_under_the_same_host_it_is_used_by() {
        let tokens = ["git.example".to_string()];
        let has = |h: &str| tokens.iter().any(|t| t == h);
        for url in [
            "https://git.example/g/p",
            "https://git.example:443/g/p",
            "ssh://git@git.example:2222/g/p",
        ] {
            assert_eq!(implied_forge(url, has), Forge::Gitlab, "{url}");
        }
        assert_eq!(
            implied_forge("https://git.example:8443/g/p", has),
            Forge::Plain
        );
        assert_eq!(implied_forge("https://github.com/o/r", has), Forge::Github);
    }

    #[test]
    fn arch_text_is_a_list_or_auto() {
        assert_eq!(parse_arch(""), None);
        assert_eq!(parse_arch(" auto "), None);
        assert_eq!(
            parse_arch("86, 89"),
            Some(vec!["86".to_string(), "89".to_string()])
        );
        assert_eq!(parse_arch("89"), Some(vec!["89".to_string()]));
    }

    #[test]
    fn a_patch_overlays_only_what_it_names() {
        let mut spec = BuildSpec {
            slug: "x".into(),
            repo_url: "https://git.example/a/b".into(),
            forge: Forge::Gitlab,
            git_ref: "main".into(),
            keep_runs: Some(3),
            ..BuildSpec::default()
        };
        BuildPatch {
            git_ref: Some("dev".into()),
            keep_runs: Some(-1),
            ..BuildPatch::default()
        }
        .apply(&mut spec, |_| false)
        .unwrap();
        assert_eq!(spec.git_ref, "dev");
        assert_eq!(spec.keep_runs, None, "negative = keep every run");
        assert_eq!(spec.forge, Forge::Gitlab, "untouched without a new repo");

        BuildPatch {
            preset: Some("ik".into()),
            ..BuildPatch::default()
        }
        .apply(&mut spec, |_| false)
        .unwrap();
        assert_eq!(spec.repo_url, "https://github.com/ikawrakow/ik_llama.cpp");
        assert_eq!((spec.forge, spec.git_ref.as_str()), (Forge::Github, "main"));

        BuildPatch {
            repo_url: Some("https://codeberg.org/a/b".into()),
            ..BuildPatch::default()
        }
        .apply(&mut spec, |_| false)
        .unwrap();
        assert_eq!(
            spec.forge,
            Forge::Plain,
            "a new repo's forge follows its host"
        );
        let err = BuildPatch {
            preset: Some("nope".into()),
            ..BuildPatch::default()
        }
        .apply(&mut spec, |_| false)
        .unwrap_err();
        assert!(err.contains("official, ik, audio, sdcpp"), "{err}");
    }
}
