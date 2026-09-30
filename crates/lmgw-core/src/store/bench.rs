//! Benchmark runs (benchmark design §7): `bench_runs`, one row per run.
//!
//! Written at start ([`insert_bench_run`]), updated after every phase
//! ([`set_bench_run_record`]) so a crash keeps the phases done so far, and
//! finalised at the end ([`finish_bench_run`]). The JSON columns decode into
//! the `lmgw_api_types::bench` shapes, every one `#[serde(default)]`, so a
//! row an older build wrote still reads. A column that does not decode at
//! all reads as its empty default and is named in the run's `unreadable`
//! (§13 decision 51): the run is still listed and shown with what could be
//! read, and is comparable with nothing — read as empty, it would compare as
//! "measured nothing". One bad row no longer fails a whole list.

use lmgw_api_types::bench::{
    BuildIdentity, GpuIdentity, ModelIdentity, ProbeReport, RunResults, SuiteParams, Timeline,
};
use lmgw_api_types::bench_ops::{BenchRun, BenchSettings, BenchStatus};
use sqlx::{Row, SqlitePool};

use super::*;

/// The identity half of a run, as the start writes it.
pub struct NewBenchRun<'a> {
    pub model: &'a ModelIdentity,
    pub build: &'a BuildIdentity,
    pub settings: &'a BenchSettings,
    pub settings_hash: &'a str,
    pub command_line: &'a str,
    pub params: &'a SuiteParams,
    pub notes: &'a str,
}

/// One JSON column, or its empty default with the reason pushed onto
/// `unreadable` (module doc).
fn json_col<T: serde::de::DeserializeOwned + Default>(
    id: i64,
    col: &str,
    raw: &str,
    unreadable: &mut Vec<String>,
) -> T {
    if raw.trim().is_empty() {
        return T::default();
    }
    serde_json::from_str(raw).unwrap_or_else(|e| {
        tracing::warn!("benchmark run {id}: the stored {col} do not parse: {e}");
        unreadable.push(format!("{col}: {e}"));
        T::default()
    })
}

fn run_from_row(row: &sqlx::sqlite::SqliteRow) -> DbResult<BenchRun> {
    let id: i64 = row.get("id");
    let status: String = row.get("status");
    let mut bad = Vec::new();
    let mut run = BenchRun {
        id,
        job_id: row.get("job_id"),
        created_at: row.get("created_at"),
        finished_at: row.get("finished_at"),
        // CHECK-constrained by 0045.
        status: BenchStatus::parse(&status).unwrap_or_default(),
        status_reason: row.get("status_reason"),
        error: row.get("error"),
        model: ModelIdentity {
            model_id: row.get("model_id"),
            gguf_path: row.get("gguf_path"),
            gguf_size: row.get::<i64, _>("gguf_size").max(0) as u64,
            gguf_mtime: row.get("gguf_mtime"),
            quant: row.get("quant"),
            rung: row.get::<i64, _>("rung").max(0) as u32,
        },
        image_ref: row.get("image_ref"),
        image_id: row.get("image_id"),
        build: json_col(id, "build", row.get::<&str, _>("build"), &mut bad),
        settings: json_col(id, "settings", row.get::<&str, _>("settings"), &mut bad),
        settings_hash: row.get("settings_hash"),
        command_line: row.get("command_line"),
        gpu: json_col(id, "gpu", row.get::<&str, _>("gpu"), &mut bad),
        suite_version: row.get::<i64, _>("suite_version").max(0) as u32,
        params: json_col(id, "params", row.get::<&str, _>("params"), &mut bad),
        results: json_col(id, "results", row.get::<&str, _>("results"), &mut bad),
        probes: json_col(id, "probes", row.get::<&str, _>("probes"), &mut bad),
        timeline: json_col(id, "timeline", row.get::<&str, _>("timeline"), &mut bad),
        notes: row.get("notes"),
        unreadable: Vec::new(),
    };
    run.unreadable = bad;
    Ok(run)
}

/// Every column but the timeline, which is the bulk of a run and which a
/// list or a comparison never reads. A macro, not a `const`, so the queries
/// below stay static strings (`concat!`).
macro_rules! without_timeline {
    () => {
        "SELECT id, job_id, created_at, finished_at, status, status_reason, error, model_id, \
         gguf_path, gguf_size, gguf_mtime, quant, rung, image_ref, image_id, build, settings, \
         settings_hash, command_line, gpu, suite_version, params, results, probes, \
         '{}' AS timeline, notes FROM bench_runs"
    };
}

/// Insert a run in `running`, with its identity; the job id follows.
pub async fn insert_bench_run(pool: &SqlitePool, r: &NewBenchRun<'_>) -> DbResult<i64> {
    let res = sqlx::query(
        "INSERT INTO bench_runs (model_id, gguf_path, gguf_size, gguf_mtime, quant, rung,
                                 image_ref, image_id, build, settings, settings_hash,
                                 command_line, suite_version, params, notes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
    )
    .bind(&r.model.model_id)
    .bind(&r.model.gguf_path)
    .bind(r.model.gguf_size as i64)
    .bind(&r.model.gguf_mtime)
    .bind(&r.model.quant)
    .bind(i64::from(r.model.rung))
    .bind(&r.build.image_ref)
    .bind(&r.build.image_id)
    .bind(to_json(r.build)?)
    .bind(to_json(r.settings)?)
    .bind(r.settings_hash)
    .bind(r.command_line)
    .bind(i64::from(r.params.suite_version))
    .bind(to_json(r.params)?)
    .bind(r.notes)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

/// Rewrite a run's identity: what the run itself rendered, with the port it
/// really got (the start wrote the plan's).
pub async fn set_bench_run_identity(
    pool: &SqlitePool,
    id: i64,
    r: &NewBenchRun<'_>,
) -> DbResult<()> {
    sqlx::query(
        "UPDATE bench_runs SET gguf_path=?2, gguf_size=?3, gguf_mtime=?4, quant=?5, rung=?6,
                image_ref=?7, image_id=?8, build=?9, settings=?10, settings_hash=?11,
                command_line=?12, suite_version=?13, params=?14
          WHERE id=?1",
    )
    .bind(id)
    .bind(&r.model.gguf_path)
    .bind(r.model.gguf_size as i64)
    .bind(&r.model.gguf_mtime)
    .bind(&r.model.quant)
    .bind(i64::from(r.model.rung))
    .bind(&r.build.image_ref)
    .bind(&r.build.image_id)
    .bind(to_json(r.build)?)
    .bind(to_json(r.settings)?)
    .bind(r.settings_hash)
    .bind(r.command_line)
    .bind(i64::from(r.params.suite_version))
    .bind(to_json(r.params)?)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_bench_run_job(pool: &SqlitePool, id: i64, job_id: i64) -> DbResult<()> {
    sqlx::query("UPDATE bench_runs SET job_id=?2 WHERE id=?1")
        .bind(id)
        .bind(job_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The measured columns, as they stand after a phase (§7).
pub async fn set_bench_run_record(
    pool: &SqlitePool,
    id: i64,
    results: &RunResults,
    probes: &ProbeReport,
    timeline: &Timeline,
    gpu: &GpuIdentity,
) -> DbResult<()> {
    sqlx::query("UPDATE bench_runs SET results=?2, probes=?3, timeline=?4, gpu=?5 WHERE id=?1")
        .bind(id)
        .bind(to_json(results)?)
        .bind(to_json(probes)?)
        .bind(to_json(timeline)?)
        .bind(to_json(gpu)?)
        .execute(pool)
        .await?;
    Ok(())
}

/// The build identity, once `/props` has named its `build_info`.
pub async fn set_bench_run_build(
    pool: &SqlitePool,
    id: i64,
    build: &BuildIdentity,
) -> DbResult<()> {
    sqlx::query("UPDATE bench_runs SET build=?2 WHERE id=?1")
        .bind(id)
        .bind(to_json(build)?)
        .execute(pool)
        .await?;
    Ok(())
}

/// Finalise a run that is still `running`; a run already final is left
/// alone (`false`).
pub async fn finish_bench_run(
    pool: &SqlitePool,
    id: i64,
    status: BenchStatus,
    reason: Option<&str>,
    error: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "UPDATE bench_runs SET status=?2, status_reason=?3, error=?4, finished_at=datetime('now')
          WHERE id=?1 AND status='running'",
    )
    .bind(id)
    .bind(status.as_str())
    .bind(reason)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() == 1)
}

pub async fn get_bench_run(pool: &SqlitePool, id: i64) -> DbResult<Option<BenchRun>> {
    let row = sqlx::query("SELECT * FROM bench_runs WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(run_from_row).transpose()
}

/// Runs newest first, without their timelines: all of them, or one row's
/// (`model_id`), older than `before`, at most `limit`.
pub async fn list_bench_runs(
    pool: &SqlitePool,
    model_id: Option<&str>,
    before: Option<i64>,
    limit: Option<u32>,
) -> DbResult<Vec<BenchRun>> {
    let rows = sqlx::query(concat!(
        without_timeline!(),
        " WHERE (?1 IS NULL OR model_id = ?1) AND (?2 IS NULL OR id < ?2)
          ORDER BY id DESC LIMIT ?3"
    ))
    .bind(model_id)
    .bind(before)
    .bind(limit.map_or(-1, i64::from))
    .fetch_all(pool)
    .await?;
    rows.iter().map(run_from_row).collect()
}

/// `run`'s previous comparable run (§6): the latest `done` run before it
/// with the same model file, settings hash, suite version and GPU name —
/// one indexed lookup, rather than every earlier candidate decoded and
/// filtered (which made `bench_runs` quadratic). Without its timeline.
///
/// A candidate that does not decode is comparable with nothing (module doc),
/// so the lookup goes on past it to the next earlier one. A run that does not
/// decode itself has none.
pub async fn previous_comparable_bench_run(
    pool: &SqlitePool,
    run: &BenchRun,
) -> DbResult<Option<BenchRun>> {
    if !run.unreadable.is_empty() {
        return Ok(None);
    }
    let mut before = run.id;
    loop {
        // `json_valid` first: `json_extract` on text that is not JSON is an
        // error for the whole query, not a NULL.
        let row = sqlx::query(concat!(
            without_timeline!(),
            " WHERE status = 'done' AND id < ?1 AND gguf_path = ?2 AND gguf_size = ?3
                AND settings_hash = ?4 AND suite_version = ?5
                AND (CASE WHEN json_valid(gpu) THEN json_extract(gpu, '$.name') END) IS ?6
              ORDER BY id DESC LIMIT 1"
        ))
        .bind(before)
        .bind(&run.model.gguf_path)
        .bind(run.model.gguf_size as i64)
        .bind(&run.settings_hash)
        .bind(i64::from(run.suite_version))
        .bind(run.gpu.name.as_deref())
        .fetch_optional(pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let candidate = run_from_row(&row)?;
        if candidate.unreadable.is_empty() {
            return Ok(Some(candidate));
        }
        before = candidate.id;
    }
}

/// The owner's notes; `false` when there is no such run.
pub async fn set_bench_run_notes(pool: &SqlitePool, id: i64, notes: &str) -> DbResult<bool> {
    let res = sqlx::query("UPDATE bench_runs SET notes=?2 WHERE id=?1")
        .bind(id)
        .bind(notes)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Delete a run that is not running; `false` when there is no such run (or
/// it is still running — the caller has told the two apart already).
pub async fn delete_bench_run(pool: &SqlitePool, id: i64) -> DbResult<bool> {
    let res = sqlx::query("DELETE FROM bench_runs WHERE id=?1 AND status <> 'running'")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Boot (§3.3): a run still `running` in a process that has just opened the
/// database was cut off by a crash or a shutdown. Its phases so far stay.
pub async fn interrupt_running_bench_runs(pool: &SqlitePool) -> DbResult<u64> {
    let res = sqlx::query(
        "UPDATE bench_runs SET status='interrupted', status_reason='shutdown',
                error='lmgw stopped while the run was going', finished_at=datetime('now')
          WHERE status = 'running'",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}
