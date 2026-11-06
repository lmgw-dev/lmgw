//! A model row's own container settings (its `image` and `extra_run_args`
//! overrides) written alone, as the machine check's settings switch writes
//! them (machine-check design §1.4, §4.2, WP12): inside the settings save's
//! own transaction, and only while the row still holds what the switch was
//! worked out from.

use sqlx::{Row, SqliteConnection};

use super::*;

/// The four tables of model rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTable {
    Local,
    Aux,
    Audio,
    Image,
}

impl ModelTable {
    /// The read of a row's two columns, and their write, as literals.
    fn queries(self) -> (&'static str, &'static str) {
        match self {
            Self::Local => (
                "SELECT image, extra_run_args FROM local_models WHERE id = ?1",
                "UPDATE local_models SET image = ?1, extra_run_args = ?2 WHERE id = ?3",
            ),
            Self::Aux => (
                "SELECT image, extra_run_args FROM aux_models WHERE id = ?1",
                "UPDATE aux_models SET image = ?1, extra_run_args = ?2 WHERE id = ?3",
            ),
            Self::Audio => (
                "SELECT image, extra_run_args FROM audio_models WHERE id = ?1",
                "UPDATE audio_models SET image = ?1, extra_run_args = ?2 WHERE id = ?3",
            ),
            Self::Image => (
                "SELECT image, extra_run_args FROM image_models WHERE id = ?1",
                "UPDATE image_models SET image = ?1, extra_run_args = ?2 WHERE id = ?3",
            ),
        }
    }
}

/// A row's own image and run arguments; `None` inherits the class's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOverrides {
    pub image: Option<String>,
    pub extra_run_args: Option<Vec<String>>,
}

/// One row's overrides, from what they were to what they become.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOverridesWrite {
    pub table: ModelTable,
    pub id: i64,
    /// The model's id, for a message that names the row.
    pub model_id: String,
    pub was: RunOverrides,
    pub now: RunOverrides,
}

/// Write `w` in `tx`: `Ok(false)`, writing nothing, when the row is gone or
/// no longer holds `w.was` (an edit landed after the switch was worked
/// out), so the caller can refuse the whole save rather than overwrite it.
pub async fn set_run_overrides_in(
    tx: &mut SqliteConnection,
    w: &RunOverridesWrite,
) -> DbResult<bool> {
    let (read, write) = w.table.queries();
    let row = sqlx::query(read)
        .bind(w.id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let held = RunOverrides {
        image: row.get::<Option<String>, _>("image"),
        extra_run_args: parse_json_opt(row.get::<Option<String>, _>("extra_run_args")),
    };
    if held != w.was {
        return Ok(false);
    }
    sqlx::query(write)
        .bind(&w.now.image)
        .bind(run_args_json(&w.now.extra_run_args)?)
        .bind(w.id)
        .execute(&mut *tx)
        .await?;
    Ok(true)
}
