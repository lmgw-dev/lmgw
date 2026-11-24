//! A model's pin (migration 0111; device-ledger design §8): the cards it
//! may run on, per row of the four local classes. Read into the snapshot
//! beside the rows ([`crate::config::Snapshot::gpu_pins`]) and written by
//! the rows' ops; the rows' own types do not carry it.

use lmgw_api_types::DeviceId;
use sqlx::{Row, SqlitePool};

use super::DbResult;
use crate::runtime::Class;

/// The statements of `class`'s table: (list, set).
fn statements(class: Class) -> (&'static str, &'static str) {
    match class {
        Class::Chat => (
            "SELECT model_id, gpu_pin FROM local_models WHERE gpu_pin IS NOT NULL",
            "UPDATE local_models SET gpu_pin = ?1 WHERE id = ?2",
        ),
        Class::Aux => (
            "SELECT model_id, gpu_pin FROM aux_models WHERE gpu_pin IS NOT NULL",
            "UPDATE aux_models SET gpu_pin = ?1 WHERE id = ?2",
        ),
        Class::Audio => (
            "SELECT model_id, gpu_pin FROM audio_models WHERE gpu_pin IS NOT NULL",
            "UPDATE audio_models SET gpu_pin = ?1 WHERE id = ?2",
        ),
        Class::Image => (
            "SELECT model_id, gpu_pin FROM image_models WHERE gpu_pin IS NOT NULL",
            "UPDATE image_models SET gpu_pin = ?1 WHERE id = ?2",
        ),
    }
}

/// Every row's pin, by class and model id; rows without one are absent.
pub async fn list_gpu_pins(pool: &SqlitePool) -> DbResult<Vec<(Class, String, Vec<DeviceId>)>> {
    let mut out = Vec::new();
    for class in [Class::Chat, Class::Aux, Class::Audio, Class::Image] {
        let rows = sqlx::query(statements(class).0).fetch_all(pool).await?;
        for r in rows {
            let text: String = r.get("gpu_pin");
            let cards: Vec<String> = serde_json::from_str(&text).unwrap_or_default();
            if !cards.is_empty() {
                out.push((
                    class,
                    r.get("model_id"),
                    cards.into_iter().map(DeviceId).collect(),
                ));
            }
        }
    }
    Ok(out)
}

/// Set (or with `None` or an empty list, clear) the pin of `class`'s row
/// `id`.
pub async fn set_gpu_pin(
    pool: &SqlitePool,
    class: Class,
    id: i64,
    pin: Option<&[DeviceId]>,
) -> DbResult<()> {
    let text = pin.filter(|p| !p.is_empty()).map(|p| {
        serde_json::to_string(&p.iter().map(DeviceId::as_str).collect::<Vec<_>>())
            .unwrap_or_default()
    });
    sqlx::query(statements(class).1)
        .bind(text)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
