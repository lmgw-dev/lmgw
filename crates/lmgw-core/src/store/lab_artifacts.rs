//! `lab_artifacts` (labs-store design §1.2, §1.4, migration 0083): the rows
//! that name the files under `<data_dir>/labs/artifacts/`. The files are
//! `labs::artifacts`'s; a row's `sha256` names one.

use std::collections::HashSet;

use lmgw_api_types::lab_store::{LabArtifactRole, LabKind};
use sqlx::{Row, SqliteConnection};

use super::*;

/// One stored artifact row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabArtifactRow {
    pub id: i64,
    pub group_id: i64,
    pub run_id: Option<i64>,
    pub role: String,
    pub ord: i64,
    pub name: String,
    pub mime: String,
    pub size: i64,
    pub sha256: String,
    /// An input that came as a data URL: its head as typed (everything
    /// before the comma), which a cell sends it back with.
    pub data_url_head: Option<String>,
    pub created_at: String,
}

/// What a new artifact row says about its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabArtifactFile {
    pub name: String,
    pub mime: String,
    pub size: i64,
    pub sha256: String,
    pub data_url_head: Option<String>,
}

fn row(r: &sqlx::sqlite::SqliteRow) -> LabArtifactRow {
    LabArtifactRow {
        id: r.get("id"),
        group_id: r.get("group_id"),
        run_id: r.get("run_id"),
        role: r.get("role"),
        ord: r.get("ord"),
        name: r.get("name"),
        mime: r.get("mime"),
        size: r.get("size"),
        sha256: r.get("sha256"),
        data_url_head: r.get("data_url_head"),
        created_at: r.get("created_at"),
    }
}

/// Insert an artifact row of group `group_id` (and run `run_id` for an
/// output); its id.
pub async fn insert_lab_artifact(
    conn: &mut SqliteConnection,
    group_id: i64,
    run_id: Option<i64>,
    role: LabArtifactRole,
    ord: i64,
    file: &LabArtifactFile,
) -> DbResult<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO lab_artifacts (group_id, run_id, role, ord, name, mime, size, sha256,
                                    data_url_head)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING id",
    )
    .bind(group_id)
    .bind(run_id)
    .bind(role.as_str())
    .bind(ord)
    .bind(&file.name)
    .bind(&file.mime)
    .bind(file.size)
    .bind(&file.sha256)
    .bind(&file.data_url_head)
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

/// Artifact `id`, read inside a write transaction (a re-used input's file
/// facts, under the artifacts lock).
pub async fn lab_artifact_in(
    conn: &mut SqliteConnection,
    id: i64,
) -> DbResult<Option<LabArtifactRow>> {
    let r = sqlx::query("SELECT * FROM lab_artifacts WHERE id = ?1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(r.as_ref().map(row))
}

/// The place the next input of group `group_id` takes: one past its last
/// (an input a run brings, after the group's own).
pub async fn next_lab_input_ord(conn: &mut SqliteConnection, group_id: i64) -> DbResult<i64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(ord) + 1, 0) FROM lab_artifacts
         WHERE group_id = ?1 AND run_id IS NULL",
    )
    .bind(group_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(n)
}

/// Artifact `id`.
pub async fn lab_artifact(pool: &SqlitePool, id: i64) -> DbResult<Option<LabArtifactRow>> {
    let r = sqlx::query("SELECT * FROM lab_artifacts WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(r.as_ref().map(row))
}

/// Every artifact row of the groups `ids`: by group, inputs first, then by
/// run and place.
pub async fn lab_artifacts_of(pool: &SqlitePool, ids: &[i64]) -> DbResult<Vec<LabArtifactRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
    let rows = sqlx::query(
        "SELECT * FROM lab_artifacts WHERE group_id IN (SELECT value FROM json_each(?1)) \
         ORDER BY group_id, run_id IS NOT NULL, run_id, ord, id",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row).collect())
}

/// The distinct files the artifact rows of `lab` name, of group `group` only
/// when given: what a delete may leave unnamed.
pub async fn lab_artifact_shas(
    conn: &mut SqliteConnection,
    lab: LabKind,
    group: Option<i64>,
) -> DbResult<Vec<String>> {
    let shas = sqlx::query_scalar(
        "SELECT DISTINCT a.sha256 FROM lab_artifacts a JOIN lab_groups g ON g.id = a.group_id
         WHERE g.lab = ?1 AND (?2 IS NULL OR g.id = ?2)",
    )
    .bind(lab.as_str())
    .bind(group)
    .fetch_all(&mut *conn)
    .await?;
    Ok(shas)
}

/// Those of `shas` that no artifact row names any more.
pub async fn unnamed_lab_shas(
    conn: &mut SqliteConnection,
    shas: &[String],
) -> DbResult<Vec<String>> {
    if shas.is_empty() {
        return Ok(Vec::new());
    }
    let list = serde_json::to_string(shas).unwrap_or_else(|_| "[]".into());
    let gone = sqlx::query_scalar(
        "SELECT value FROM json_each(?1)
         WHERE NOT EXISTS (SELECT 1 FROM lab_artifacts a WHERE a.sha256 = json_each.value)",
    )
    .bind(list)
    .fetch_all(&mut *conn)
    .await?;
    Ok(gone)
}

/// Every file any artifact row names.
pub async fn named_lab_shas(pool: &SqlitePool) -> DbResult<HashSet<String>> {
    let shas: Vec<String> = sqlx::query_scalar("SELECT DISTINCT sha256 FROM lab_artifacts")
        .fetch_all(pool)
        .await?;
    Ok(shas.into_iter().collect())
}
