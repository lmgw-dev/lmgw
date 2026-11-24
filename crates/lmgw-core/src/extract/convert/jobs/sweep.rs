//! A job's end of life (document-conversion design §7.3, §9): at start,
//! the jobs the last process left running become `interrupted` and the
//! conversion spool of a gateway that no longer runs is swept; every hour
//! (and after a docling client's fetch, [`after_fetch`]) the ended jobs
//! past their time go with their stored results, and a results directory
//! without a job row is deleted.

use chrono::{DateTime, Utc};

use super::{results, RESULT_REMOVAL_DELAY};
use crate::state::SharedState;
use crate::store;

/// At start, before any job can run: the jobs still marked running are
/// `interrupted`.
pub async fn boot(db: &sqlx::SqlitePool, data_dir: &std::path::Path) {
    // What a gateway that was killed left in the conversion spool (the
    // uploads LibreOffice and ffmpeg read, page renders, poppler's working
    // directories): gone before the first conversion, not at the first
    // LibreOffice or ffmpeg run (security round A, hardening 10).
    let spool = data_dir.join(crate::extract::convert::ocr::spool::SPOOL_DIR);
    let _ = tokio::task::spawn_blocking(move || {
        crate::extract::convert::libreoffice::scratch::sweep_once(&spool)
    })
    .await;
    match store::convert_jobs::interrupt_running_jobs(db).await {
        Ok(ids) if ids.is_empty() => {}
        Ok(ids) => {
            // The uploads that were still waiting will not be read now.
            for id in &ids {
                results::remove_inputs(data_dir, id).await;
            }
            tracing::info!(
                "marked {} conversion job(s) interrupted by the last shutdown; their finished \
             files keep their results",
                ids.len()
            );
        }
        Err(e) => tracing::warn!("closing interrupted conversion jobs: {e}"),
    }
}

/// The time before which an ended job goes under a retention of `days`;
/// `None` keeps them (0, or a retention too long for a date to hold).
fn ended_before(now: DateTime<Utc>, days: i64) -> Option<DateTime<Utc>> {
    if days <= 0 {
        return None;
    }
    chrono::Duration::try_days(days).and_then(|d| now.checked_sub_signed(d))
}

/// Delete the ended jobs past `documents.job_retention_days` (0 keeps them)
/// and the docling results fetched more than [`RESULT_REMOVAL_DELAY`] ago,
/// then the results directories no job row owns. How many jobs went.
pub async fn upkeep(state: &SharedState) -> usize {
    let days = state.snapshot().settings.documents.job_retention_days;
    let now = Utc::now();
    let ended_before = ended_before(now, days);
    let fetched_before =
        now - chrono::Duration::from_std(RESULT_REMOVAL_DELAY).unwrap_or(chrono::Duration::zero());
    let ids =
        match store::convert_jobs::jobs_to_delete(&state.db, ended_before, fetched_before).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!("listing conversion jobs to delete: {e}");
                return 0;
            }
        };
    let mut gone = 0;
    for id in &ids {
        match store::convert_jobs::delete_job(&state.db, id).await {
            Ok(()) => {
                results::remove(&state.data_dir, id).await;
                gone += 1;
            }
            Err(e) => tracing::warn!("deleting conversion job {id}: {e}"),
        }
    }
    // The directories are listed before the rows are read: a job's row is
    // written before its first file, so a directory that has no row in the
    // later read is stray, and a job that starts meanwhile is not.
    let listed = results::stored(&state.data_dir).await;
    if let Ok(known) = store::convert_jobs::job_ids(&state.db).await {
        for stray in listed {
            if !known.contains(&stray) && !state.convert_jobs.is_live(&stray) {
                results::remove(&state.data_dir, &stray).await;
            }
        }
    }
    if gone > 0 {
        tracing::info!("deleted {gone} ended conversion job(s) and their results");
    }
    // What the jobs store, read from the disk again (`quota`).
    super::quota::resync(state).await;
    gone
}

/// A docling client fetched job `id`'s result: it goes
/// [`RESULT_REMOVAL_DELAY`] later (or at the next hourly upkeep after a
/// restart).
pub fn after_fetch(state: &SharedState) {
    let st = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(RESULT_REMOVAL_DELAY + std::time::Duration::from_secs(1)).await;
        upkeep(&st).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retention_too_long_for_a_date_keeps_the_jobs() {
        let now = Utc::now();
        assert_eq!(ended_before(now, 0), None);
        assert_eq!(ended_before(now, -3), None);
        for days in [999_999_999, i64::MAX, 400_000_000_000] {
            assert_eq!(ended_before(now, days), None, "{days}");
        }
        assert_eq!(
            ended_before(now, 30),
            Some(now - chrono::Duration::days(30))
        );
    }
}
