//! `lab_presets` (labs-store design §3.1, migration 0084): named form parts a
//! lab keeps, the Decision lab's question sets first. Each write is one
//! `begin_write`; a name the lab already gives a preset of that kind is
//! [`PresetWrite::NameTaken`]. Every write moves `updated_at` forward (by a
//! millisecond when the clock has not), so an update can be conditional on
//! the `updated_at` it read.

use lmgw_api_types::lab_store::{LabKind, LabPresetKind};
use sqlx::Row;

use super::*;

/// One stored preset, its body as the text it was written as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabPresetRow {
    pub id: i64,
    pub lab: String,
    pub kind: String,
    pub name: String,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
}

/// How a preset write ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetWrite {
    Written(LabPresetRow),
    /// `UNIQUE (lab, kind, name)`: the lab has a preset of that kind by that
    /// name.
    NameTaken,
    /// No preset of the lab by that id.
    Missing,
    /// The preset is no longer the one the write was computed from: its
    /// `updated_at` (an update) or its body (a measured threshold) moved.
    Changed,
}

fn row(r: &sqlx::sqlite::SqliteRow) -> LabPresetRow {
    LabPresetRow {
        id: r.get("id"),
        lab: r.get("lab"),
        kind: r.get("kind"),
        name: r.get("name"),
        body: r.get("body"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

/// The columns a [`LabPresetRow`] reads.
macro_rules! columns {
    () => {
        "id, lab, kind, name, body, created_at, updated_at"
    };
}

/// The `updated_at` a write sets: now, or a millisecond past the row's own
/// when the clock has not moved past it (two writes in one millisecond, a
/// clock set back), so every write moves it.
macro_rules! next_stamp {
    () => {
        "CASE WHEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now') > updated_at
              THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              ELSE strftime('%Y-%m-%dT%H:%M:%fZ', updated_at, '+0.001 seconds') END"
    };
}

fn taken(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.is_unique_violation())
}

/// Insert a preset.
pub async fn insert_lab_preset(
    pool: &SqlitePool,
    lab: LabKind,
    kind: LabPresetKind,
    name: &str,
    body: &str,
) -> DbResult<PresetWrite> {
    let mut tx = begin_write(pool).await?;
    let r = sqlx::query(concat!(
        "INSERT INTO lab_presets (lab, kind, name, body) VALUES (?1, ?2, ?3, ?4) RETURNING ",
        columns!()
    ))
    .bind(lab.as_str())
    .bind(kind.as_str())
    .bind(name)
    .bind(body)
    .fetch_one(&mut *tx)
    .await;
    match r {
        Ok(r) => {
            tx.commit().await?;
            Ok(PresetWrite::Written(row(&r)))
        }
        Err(e) if taken(&e) => Ok(PresetWrite::NameTaken),
        Err(e) => Err(e.into()),
    }
}

/// Rewrite preset `id` of `lab` whole (its kind, name and body) when its
/// `updated_at` is still `read_at`, the one the write was computed from;
/// [`PresetWrite::Changed`] when it moved.
pub async fn update_lab_preset(
    pool: &SqlitePool,
    lab: LabKind,
    id: i64,
    read_at: &str,
    kind: LabPresetKind,
    name: &str,
    body: &str,
) -> DbResult<PresetWrite> {
    let mut tx = begin_write(pool).await?;
    let r = sqlx::query(concat!(
        "UPDATE lab_presets SET kind = ?4, name = ?5, body = ?6, updated_at = ",
        next_stamp!(),
        " WHERE id = ?1 AND lab = ?2 AND updated_at = ?3 RETURNING ",
        columns!()
    ))
    .bind(id)
    .bind(lab.as_str())
    .bind(read_at)
    .bind(kind.as_str())
    .bind(name)
    .bind(body)
    .fetch_optional(&mut *tx)
    .await;
    match r {
        Ok(Some(r)) => {
            tx.commit().await?;
            Ok(PresetWrite::Written(row(&r)))
        }
        Ok(None) => {
            let there: Option<i64> =
                sqlx::query_scalar("SELECT id FROM lab_presets WHERE id = ?1 AND lab = ?2")
                    .bind(id)
                    .bind(lab.as_str())
                    .fetch_optional(&mut *tx)
                    .await?;
            Ok(if there.is_some() {
                PresetWrite::Changed
            } else {
                PresetWrite::Missing
            })
        }
        Err(e) if taken(&e) => Ok(PresetWrite::NameTaken),
        Err(e) => Err(e.into()),
    }
}

/// Rewrite preset `id`'s body alone, when it still is `expected`: a write
/// computed from what was read (a measured threshold) does not overwrite an
/// edit made in between. [`PresetWrite::Changed`] when it is gone or was
/// changed.
pub async fn replace_lab_preset_body(
    pool: &SqlitePool,
    id: i64,
    expected: &str,
    body: &str,
) -> DbResult<PresetWrite> {
    let mut tx = begin_write(pool).await?;
    let r = sqlx::query(concat!(
        "UPDATE lab_presets SET body = ?3, updated_at = ",
        next_stamp!(),
        " WHERE id = ?1 AND body = ?2 RETURNING ",
        columns!()
    ))
    .bind(id)
    .bind(expected)
    .bind(body)
    .fetch_optional(&mut *tx)
    .await?;
    match r {
        Some(r) => {
            tx.commit().await?;
            Ok(PresetWrite::Written(row(&r)))
        }
        None => Ok(PresetWrite::Changed),
    }
}

/// Delete preset `id` of `lab`; whether there was one. The set runs that ran
/// it keep their snapshot (`ON DELETE SET NULL`).
pub async fn delete_lab_preset(pool: &SqlitePool, lab: LabKind, id: i64) -> DbResult<bool> {
    let mut tx = begin_write(pool).await?;
    let n = sqlx::query("DELETE FROM lab_presets WHERE id = ?1 AND lab = ?2")
        .bind(id)
        .bind(lab.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(n > 0)
}

/// Preset `id`, of `lab` when given.
pub async fn lab_preset(
    pool: &SqlitePool,
    lab: Option<LabKind>,
    id: i64,
) -> DbResult<Option<LabPresetRow>> {
    let r = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM lab_presets WHERE id = ?1 AND (?2 IS NULL OR lab = ?2)"
    ))
    .bind(id)
    .bind(lab.map(LabKind::as_str))
    .fetch_optional(pool)
    .await?;
    Ok(r.as_ref().map(row))
}

/// `lab`'s presets, of `kind` when given, by name.
pub async fn lab_presets(
    pool: &SqlitePool,
    lab: LabKind,
    kind: Option<LabPresetKind>,
) -> DbResult<Vec<LabPresetRow>> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM lab_presets WHERE lab = ?1 AND (?2 IS NULL OR kind = ?2)
         ORDER BY name COLLATE NOCASE, id"
    ))
    .bind(lab.as_str())
    .bind(kind.map(LabPresetKind::as_str))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row).collect())
}
