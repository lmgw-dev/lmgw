//! Candidate aliases CRUD (candidate-aliases design §4.1)

use sqlx::SqlitePool;

use crate::config::{CandidateAlias, HoldFallbackMode};

use super::*;

pub struct NewCandidateAlias {
    pub alias: String,
    pub candidates: Vec<String>,
    pub background: bool,
    pub fallback_mode: HoldFallbackMode,
    pub fallback: Option<String>,
    pub capabilities_disabled: Vec<String>,
    pub capabilities_enabled: Vec<String>,
    pub enabled: bool,
    pub notes: String,
}

pub async fn list_candidate_aliases(pool: &SqlitePool) -> DbResult<Vec<CandidateAlias>> {
    let rows = sqlx::query("SELECT * FROM candidate_aliases ORDER BY alias")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(candidate_alias_from_row).collect())
}

pub async fn insert_candidate_alias(pool: &SqlitePool, a: &NewCandidateAlias) -> DbResult<i64> {
    let candidates = to_json(&a.candidates)?;
    let capabilities_disabled = to_json(&a.capabilities_disabled)?;
    let capabilities_enabled = to_json(&a.capabilities_enabled)?;
    let res = sqlx::query(
        "INSERT INTO candidate_aliases (alias, candidates, background, fallback_mode, fallback,
                                         capabilities_disabled, capabilities_enabled, enabled, notes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(&a.alias)
    .bind(candidates)
    .bind(a.background as i64)
    .bind(a.fallback_mode.as_str())
    .bind(&a.fallback)
    .bind(capabilities_disabled)
    .bind(capabilities_enabled)
    .bind(a.enabled as i64)
    .bind(&a.notes)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn update_candidate_alias(
    pool: &SqlitePool,
    id: i64,
    a: &NewCandidateAlias,
) -> DbResult<()> {
    let candidates = to_json(&a.candidates)?;
    let capabilities_disabled = to_json(&a.capabilities_disabled)?;
    let capabilities_enabled = to_json(&a.capabilities_enabled)?;
    sqlx::query(
        "UPDATE candidate_aliases SET alias=?2, candidates=?3, background=?4, fallback_mode=?5,
         fallback=?6, capabilities_disabled=?7, capabilities_enabled=?8, enabled=?9, notes=?10,
         updated_at=datetime('now') WHERE id=?1",
    )
    .bind(id)
    .bind(&a.alias)
    .bind(candidates)
    .bind(a.background as i64)
    .bind(a.fallback_mode.as_str())
    .bind(&a.fallback)
    .bind(capabilities_disabled)
    .bind(capabilities_enabled)
    .bind(a.enabled as i64)
    .bind(&a.notes)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_candidate_alias(pool: &SqlitePool, id: i64) -> DbResult<()> {
    sqlx::query("DELETE FROM candidate_aliases WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
