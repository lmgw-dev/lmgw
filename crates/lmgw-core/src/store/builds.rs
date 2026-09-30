//! Container builds (container-builds design §3–§5)
//!
//! Unlike the model tables, a build row that will not parse is an **error**,
//! not an empty field: an `extras` column read as `[]` would build master
//! without the PRs the owner asked for, and an `edits` column read as NULL
//! would quietly swap their edits for the preset's. Builds are not part of the
//! snapshot, so a bad row fails the call that reads it — naming the build —
//! rather than the gateway's boot.

use sqlx::{Row, SqlitePool};

use crate::backends::{
    Build, BuildRun, BuildRunPatch, BuildRunStatus, BuildSpec, BuildTrigger, Engine, Forge,
    GpuBackend, NewBuildRun,
};
use crate::error::GatewayError;

use super::*;

fn json_col<T: serde::de::DeserializeOwned>(what: &str, col: &str, raw: &str) -> DbResult<T> {
    serde_json::from_str(raw)
        .map_err(|e| GatewayError::Internal(format!("{what}: the stored {col} do not parse: {e}")))
}

fn json_col_opt<T: serde::de::DeserializeOwned>(
    what: &str,
    col: &str,
    raw: Option<String>,
) -> DbResult<Option<T>> {
    raw.map(|r| json_col(what, col, &r)).transpose()
}

fn build_from_row(row: &sqlx::sqlite::SqliteRow) -> DbResult<Build> {
    let slug: String = row.get("slug");
    let what = format!("build '{slug}'");
    // The three enums are CHECK-constrained by 0040, so a value outside them
    // cannot be in the table.
    Ok(Build {
        id: row.get("id"),
        spec: BuildSpec {
            name: row.get("name"),
            engine: Engine::parse(row.get::<&str, _>("engine")).unwrap_or_default(),
            repo_url: row.get("repo_url"),
            forge: Forge::parse(row.get::<&str, _>("forge")).unwrap_or_default(),
            git_ref: row.get("ref"),
            extras: json_col(&what, "extras", row.get::<&str, _>("extras"))?,
            backend: GpuBackend::parse(row.get::<&str, _>("backend")).unwrap_or_default(),
            cuda_version: row.get("cuda_version"),
            arch: json_col_opt(&what, "arch", row.get("arch"))?,
            dockerfile: row.get("dockerfile"),
            target: row.get("target"),
            edits: json_col_opt(&what, "edits", row.get("edits"))?,
            ccache: row.get::<i64, _>("ccache") != 0,
            ccache_max_size: row.get("ccache_max_size"),
            cpus: row.get("cpus"),
            build_args: row.get("build_args"),
            keep_layers: row.get::<i64, _>("keep_layers") != 0,
            // CHECK-constrained to >= 0; a value past u32 is not one anybody
            // typed, and keeping every run is the reading that deletes nothing.
            keep_runs: row
                .get::<Option<i64>, _>("keep_runs")
                .and_then(|n| u32::try_from(n).ok()),
            notes: row.get("notes"),
            slug,
        },
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

/// The unique slug index answering for a write, in words: slugs are image
/// tags, so two builds cannot share one.
fn slug_taken(e: sqlx::Error, slug: &str) -> GatewayError {
    match &e {
        sqlx::Error::Database(d) if d.is_unique_violation() => GatewayError::BadRequest(format!(
            "a build with the slug '{slug}' already exists — the slug is the image tag, so it \
             must be unique"
        )),
        _ => e.into(),
    }
}

/// Every build, by slug.
pub async fn list_builds(pool: &SqlitePool) -> DbResult<Vec<Build>> {
    let rows = sqlx::query("SELECT * FROM builds ORDER BY slug")
        .fetch_all(pool)
        .await?;
    rows.iter().map(build_from_row).collect()
}

pub async fn get_build(pool: &SqlitePool, id: i64) -> DbResult<Option<Build>> {
    let row = sqlx::query("SELECT * FROM builds WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(build_from_row).transpose()
}

pub async fn get_build_by_slug(pool: &SqlitePool, slug: &str) -> DbResult<Option<Build>> {
    let row = sqlx::query("SELECT * FROM builds WHERE slug = ?1")
        .bind(slug)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(build_from_row).transpose()
}

/// The JSON columns of one build, in bind order: extras, arch, edits.
fn build_json(s: &BuildSpec) -> DbResult<(String, Option<String>, Option<String>)> {
    Ok((
        to_json(&s.extras)?,
        to_json_opt(&s.arch)?,
        to_json_opt(&s.edits)?,
    ))
}

/// Insert a definition — already checked by
/// [`crate::backends::validate::validate_build`]; the store does not validate
/// fields. A taken slug comes back as a `BadRequest` that says so.
pub async fn insert_build(pool: &SqlitePool, s: &BuildSpec) -> DbResult<i64> {
    let (extras, arch, edits) = build_json(s)?;
    let res = sqlx::query(
        "INSERT INTO builds (slug, name, engine, repo_url, forge, ref, extras, backend,
                             cuda_version, arch, dockerfile, target, edits, ccache,
                             ccache_max_size, cpus, build_args, keep_layers, keep_runs, notes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                 ?18, ?19, ?20)",
    )
    .bind(&s.slug)
    .bind(&s.name)
    .bind(s.engine.as_str())
    .bind(&s.repo_url)
    .bind(s.forge.as_str())
    .bind(&s.git_ref)
    .bind(extras)
    .bind(s.backend.as_str())
    .bind(&s.cuda_version)
    .bind(arch)
    .bind(&s.dockerfile)
    .bind(&s.target)
    .bind(edits)
    .bind(s.ccache as i64)
    .bind(&s.ccache_max_size)
    .bind(&s.cpus)
    .bind(&s.build_args)
    .bind(s.keep_layers as i64)
    .bind(s.keep_runs.map(i64::from))
    .bind(&s.notes)
    .execute(pool)
    .await
    .map_err(|e| slug_taken(e, &s.slug))?;
    Ok(res.last_insert_rowid())
}

/// Replace a build's definition. **The slug is immutable once the build has a
/// run** (§4): its runs' images are tagged under it, and a renamed build would
/// orphan every one of them from its moving tag. Checked inside the `UPDATE`
/// itself, so no run can be inserted between the check and the write.
pub async fn update_build(pool: &SqlitePool, id: i64, s: &BuildSpec) -> DbResult<()> {
    let (extras, arch, edits) = build_json(s)?;
    let res = sqlx::query(
        "UPDATE builds SET slug=?2, name=?3, engine=?4, repo_url=?5, forge=?6, ref=?7,
                extras=?8, backend=?9, cuda_version=?10, arch=?11, dockerfile=?12, target=?13,
                edits=?14, ccache=?15, ccache_max_size=?16, cpus=?17, build_args=?18,
                keep_layers=?19, keep_runs=?20, notes=?21, updated_at=datetime('now')
          WHERE id=?1
            AND (slug = ?2 OR NOT EXISTS (SELECT 1 FROM build_runs WHERE build_id = ?1))",
    )
    .bind(id)
    .bind(&s.slug)
    .bind(&s.name)
    .bind(s.engine.as_str())
    .bind(&s.repo_url)
    .bind(s.forge.as_str())
    .bind(&s.git_ref)
    .bind(extras)
    .bind(s.backend.as_str())
    .bind(&s.cuda_version)
    .bind(arch)
    .bind(&s.dockerfile)
    .bind(&s.target)
    .bind(edits)
    .bind(s.ccache as i64)
    .bind(&s.ccache_max_size)
    .bind(&s.cpus)
    .bind(&s.build_args)
    .bind(s.keep_layers as i64)
    .bind(s.keep_runs.map(i64::from))
    .bind(&s.notes)
    .execute(pool)
    .await
    .map_err(|e| slug_taken(e, &s.slug))?;
    if res.rows_affected() == 1 {
        return Ok(());
    }
    // Nothing written: either there is no such build, or the slug guard held.
    let Some(cur) = get_build(pool, id).await? else {
        return Err(GatewayError::NotFound(format!("no build with id {id}")));
    };
    let runs = count_build_runs(pool, id).await?;
    Err(GatewayError::BadRequest(format!(
        "the slug of build '{}' cannot change to '{}': it has {runs} run(s), whose images are \
         tagged {}-… — duplicate the build to get one under a new slug",
        cur.spec.slug,
        s.slug,
        cur.moving_tag()
    )))
}

/// Delete a build. Its runs stay, with `build_id` set to NULL (§3); its
/// images are podman's, and whether they go too is the caller's decision.
/// `false` when there was no such build.
pub async fn delete_build(pool: &SqlitePool, id: i64) -> DbResult<bool> {
    let res = sqlx::query("DELETE FROM builds WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Copy a build under a new slug (§4 "Duplicate"). `slug` defaults to the
/// first free `<slug>-copy[-N]` and `name` to `<name> (copy)`; a slug that is
/// given is validated here, since nothing else has seen it.
pub async fn duplicate_build(
    pool: &SqlitePool,
    id: i64,
    slug: Option<&str>,
    name: Option<&str>,
) -> DbResult<i64> {
    let src = get_build(pool, id)
        .await?
        .ok_or_else(|| GatewayError::NotFound(format!("no build with id {id}")))?;
    let slug = match slug.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => {
            crate::backends::validate::validate_slug(s).map_err(GatewayError::BadRequest)?;
            s.to_string()
        }
        None => {
            let taken: Vec<String> = sqlx::query_scalar("SELECT slug FROM builds")
                .fetch_all(pool)
                .await?;
            crate::backends::validate::copy_slug(&src.spec.slug, |c| taken.iter().any(|t| t == c))
        }
    };
    let name = match name.map(str::trim).filter(|s| !s.is_empty()) {
        Some(n) => n.to_string(),
        None => format!("{} (copy)", src.spec.name),
    };
    let spec = BuildSpec {
        slug,
        name,
        ..src.spec
    };
    insert_build(pool, &spec).await
}

/// How many runs a build has — what decides whether its slug may still change.
pub async fn count_build_runs(pool: &SqlitePool, build_id: i64) -> DbResult<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM build_runs WHERE build_id = ?1")
            .bind(build_id)
            .fetch_one(pool)
            .await?,
    )
}

fn build_run_from_row(row: &sqlx::sqlite::SqliteRow) -> DbResult<BuildRun> {
    let id: i64 = row.get("id");
    let what = format!("build run {id}");
    Ok(BuildRun {
        id,
        build_id: row.get("build_id"),
        job_id: row.get("job_id"),
        slug: row.get("slug"),
        engine: Engine::parse(row.get::<&str, _>("engine")).unwrap_or_default(),
        trigger: BuildTrigger::parse(row.get::<&str, _>("trigger")).unwrap_or_default(),
        // CHECK-constrained; `failed` is only the type's fallback, never a
        // value this table can hold.
        status: BuildRunStatus::parse(row.get::<&str, _>("status"))
            .unwrap_or(BuildRunStatus::Failed),
        inputs: json_col(&what, "inputs", row.get::<&str, _>("inputs"))?,
        base_sha: row.get("base_sha"),
        cfg_hash: row.get("cfg_hash"),
        image_id: row.get("image_id"),
        tags: json_col(&what, "tags", row.get::<&str, _>("tags"))?,
        promoted: row.get::<i64, _>("promoted") != 0,
        size_bytes: row
            .get::<Option<i64>, _>("size_bytes")
            .and_then(|b| u64::try_from(b).ok()),
        verify: json_col_opt(&what, "verify", row.get("verify"))?,
        error: row.get("error"),
        log_path: row.get("log_path"),
        started_at: row.get("started_at"),
        finished_at: row.get("finished_at"),
    })
}

/// A byte count as SQLite's signed INTEGER — refused out loud rather than
/// stored as NULL (which would read as "not measured") in the case that cannot
/// happen.
fn size_col(bytes: Option<u64>) -> DbResult<Option<i64>> {
    bytes
        .map(|b| {
            i64::try_from(b).map_err(|_| {
                GatewayError::Internal(format!(
                    "an image size of {b} bytes does not fit an INTEGER"
                ))
            })
        })
        .transpose()
}

/// Open a run: `running`, `started_at` now.
pub async fn insert_build_run(pool: &SqlitePool, r: &NewBuildRun) -> DbResult<i64> {
    let res = sqlx::query(
        "INSERT INTO build_runs (build_id, job_id, slug, engine, trigger, inputs, log_path)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(r.build_id)
    .bind(r.job_id)
    .bind(&r.slug)
    .bind(r.engine.as_str())
    .bind(r.trigger.as_str())
    .bind(to_json(&r.inputs)?)
    .bind(&r.log_path)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

/// Apply a [`BuildRunPatch`]: every `Some` field is written, every `None`
/// column is left alone.
pub async fn update_build_run(pool: &SqlitePool, id: i64, p: &BuildRunPatch) -> DbResult<()> {
    if let Some(s) = p.status.filter(|s| s.is_terminal()) {
        return Err(GatewayError::Internal(format!(
            "build run {id}: '{}' is a terminal status — finish_build_run sets it",
            s.as_str()
        )));
    }
    let inputs = p.inputs.as_ref().map(to_json).transpose()?;
    let tags = p.tags.as_ref().map(to_json).transpose()?;
    let verify = p.verify.as_ref().map(to_json).transpose()?;
    let res = sqlx::query(
        "UPDATE build_runs SET
             status     = COALESCE(?2, status),
             job_id     = COALESCE(?3, job_id),
             inputs     = COALESCE(?4, inputs),
             base_sha   = COALESCE(?5, base_sha),
             cfg_hash   = COALESCE(?6, cfg_hash),
             image_id   = COALESCE(?7, image_id),
             tags       = COALESCE(?8, tags),
             size_bytes = COALESCE(?9, size_bytes),
             verify     = COALESCE(?10, verify),
             error      = COALESCE(?11, error),
             log_path   = COALESCE(?12, log_path)
         WHERE id = ?1",
    )
    .bind(id)
    .bind(p.status.map(|s| s.as_str()))
    .bind(p.job_id)
    .bind(inputs)
    .bind(&p.base_sha)
    .bind(&p.cfg_hash)
    .bind(&p.image_id)
    .bind(tags)
    .bind(size_col(p.size_bytes)?)
    .bind(verify)
    .bind(&p.error)
    .bind(&p.log_path)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(GatewayError::NotFound(format!("no build run with id {id}")));
    }
    Ok(())
}

/// End a run: a terminal status, its error (if any) and `finished_at` now.
/// Also the way a finished run's verdict is revised later (**Verify now**
/// turning `unverified` into `succeeded`); `finished_at` then moves to the
/// revision.
/// A later verdict on a run that has already finished (Verify now, §5 step
/// 6): its status and error change, when it finished does not — the run
/// history's "Took" is how long the run took, not how long until someone
/// verified its image (it read 14m 59s for a 2m 02s build in the Backends
/// UI live pass).
pub async fn set_build_run_verdict(
    pool: &SqlitePool,
    id: i64,
    status: BuildRunStatus,
    error: Option<&str>,
) -> DbResult<()> {
    if !status.is_terminal() {
        return Err(GatewayError::Internal(format!(
            "build run {id}: a verdict is a terminal status, not '{}'",
            status.as_str()
        )));
    }
    let res = sqlx::query("UPDATE build_runs SET status=?2, error=?3 WHERE id=?1")
        .bind(id)
        .bind(status.as_str())
        .bind(error)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(GatewayError::NotFound(format!("no build run with id {id}")));
    }
    Ok(())
}

pub async fn finish_build_run(
    pool: &SqlitePool,
    id: i64,
    status: BuildRunStatus,
    error: Option<&str>,
) -> DbResult<()> {
    if !status.is_terminal() {
        return Err(GatewayError::Internal(format!(
            "build run {id}: finishing needs a terminal status, not '{}'",
            status.as_str()
        )));
    }
    let res = sqlx::query(
        "UPDATE build_runs SET status=?2, error=?3, finished_at=datetime('now') WHERE id=?1",
    )
    .bind(id)
    .bind(status.as_str())
    .bind(error)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(GatewayError::NotFound(format!("no build run with id {id}")));
    }
    Ok(())
}

/// Record that the build's moving tag now points at this run's image (§5 step
/// 7, and §6's **Make current**): `promoted` on this run, off on every other
/// run of the same build, in one statement.
pub async fn promote_build_run(pool: &SqlitePool, id: i64) -> DbResult<()> {
    let res = sqlx::query(
        "UPDATE build_runs SET promoted = (id = ?1)
          WHERE build_id = (SELECT build_id FROM build_runs WHERE id = ?1 AND build_id IS NOT NULL)",
    )
    .bind(id)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        // Either no such run, or its build is gone — and a run of a deleted
        // build has no moving tag left to point anywhere.
        return Err(match get_build_run(pool, id).await? {
            None => GatewayError::NotFound(format!("no build run with id {id}")),
            Some(_) => GatewayError::BadRequest(format!(
                "build run {id} belongs to a deleted build, which has no moving tag to promote \
                 it to"
            )),
        });
    }
    Ok(())
}

pub async fn get_build_run(pool: &SqlitePool, id: i64) -> DbResult<Option<BuildRun>> {
    let row = sqlx::query("SELECT * FROM build_runs WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(build_run_from_row).transpose()
}

/// Runs newest first — of one build, or of every build (deleted ones
/// included) when `build_id` is `None`. `limit <= 0` means all of them: runs
/// are the history the owner asked to keep, and there is no retention setting
/// to bound it behind their back.
pub async fn list_build_runs(
    pool: &SqlitePool,
    build_id: Option<i64>,
    limit: i64,
) -> DbResult<Vec<BuildRun>> {
    let rows = sqlx::query(
        "SELECT * FROM build_runs
          WHERE (?1 IS NULL OR build_id = ?1)
          ORDER BY started_at DESC, id DESC
          LIMIT (CASE WHEN ?2 > 0 THEN ?2 ELSE -1 END)",
    )
    .bind(build_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter().map(build_run_from_row).collect()
}

/// Every run of one build, newest first.
pub async fn list_runs_for_build(pool: &SqlitePool, build_id: i64) -> DbResult<Vec<BuildRun>> {
    list_build_runs(pool, Some(build_id), 0).await
}

/// One page of a build's runs, newest first (§15 `build_get`), and whether an
/// older page exists. `before` is an exclusive run-id cursor: only runs with
/// a smaller id — the oldest id the caller already has, for the next page; or
/// `run_id + 1` with `limit` 1 to fetch exactly that run. It need not name an
/// existing run. Ordered by id, which is the order runs started in. `limit`
/// is the caller's (the UI's ShowMore page size); `None` means every
/// remaining run — nothing here picks a bound.
pub async fn list_build_runs_page(
    pool: &SqlitePool,
    build_id: i64,
    before: Option<i64>,
    limit: Option<u32>,
) -> DbResult<(Vec<BuildRun>, bool)> {
    // One more than asked for says whether there is more.
    let fetch = limit.map_or(-1, |l| i64::from(l) + 1);
    let rows = sqlx::query(
        "SELECT * FROM build_runs
          WHERE build_id = ?1 AND (?2 IS NULL OR id < ?2)
          ORDER BY id DESC
          LIMIT ?3",
    )
    .bind(build_id)
    .bind(before)
    .bind(fetch)
    .fetch_all(pool)
    .await?;
    let mut runs = rows
        .iter()
        .map(build_run_from_row)
        .collect::<DbResult<Vec<_>>>()?;
    let more = match limit {
        Some(l) if runs.len() > l as usize => {
            runs.truncate(l as usize);
            true
        }
        _ => false,
    };
    Ok((runs, more))
}

/// The run whose image the build's moving tag was last moved to by lmgw —
/// its **current** run (§6, §15 `current_run`) — if any.
pub async fn promoted_build_run(pool: &SqlitePool, build_id: i64) -> DbResult<Option<BuildRun>> {
    let row = sqlx::query(
        "SELECT * FROM build_runs WHERE build_id = ?1 AND promoted = 1
          ORDER BY started_at DESC, id DESC LIMIT 1",
    )
    .bind(build_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(build_run_from_row).transpose()
}

/// The run a build's update check compares against (container-builds §8):
/// its promoted run — the image the moving tag points at — else its newest
/// run that built an image worth comparing, `succeeded` or `unverified`.
/// `None` when it has neither (never run, or only failed attempts).
pub async fn reference_build_run(pool: &SqlitePool, build_id: i64) -> DbResult<Option<BuildRun>> {
    let row = sqlx::query(
        "SELECT * FROM build_runs
          WHERE build_id = ?1 AND (promoted = 1 OR status IN ('succeeded', 'unverified'))
          ORDER BY promoted DESC, id DESC LIMIT 1",
    )
    .bind(build_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(build_run_from_row).transpose()
}

/// Fail every run still marked `running`. Called once at startup beside
/// [`fail_orphaned_jobs`], for the same reason: nothing can be building in a
/// process that has just opened the DB, so a `running` row is the residue of
/// a crash or a shutdown. Returns the number of rows closed.
pub async fn fail_orphaned_build_runs(pool: &SqlitePool) -> DbResult<u64> {
    let res = sqlx::query(
        "UPDATE build_runs SET status='failed', error='interrupted by shutdown',
             finished_at=datetime('now')
         WHERE status = 'running'",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}
