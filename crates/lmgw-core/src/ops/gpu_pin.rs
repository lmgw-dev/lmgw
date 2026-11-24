//! A model's pin as its op patches it (device-ledger design §8, ruling 8):
//! `gpu_pin` on `local_model_set`, `aux_model_set`, `image_model_set` and
//! `audio_model_set`. Stored beside the row (migration 0111,
//! `store::set_gpu_pin`) once the row's own save went through, and read
//! into the snapshot (`Snapshot::gpu_pins`).

use lmgw_api_types::DeviceId;

use crate::runtime::Class;
use crate::state::SharedState;

use super::common::clear_has;

/// What a patch does to the pin: `None` leaves it, `Some(None)` clears it
/// (`clear: "gpu_pin"`, or an empty list), `Some(Some(cards))` sets it.
/// Only a `create` or `update` touches it.
pub(crate) fn patched(
    action: &str,
    pin: &Option<Vec<String>>,
    clear: Option<&str>,
) -> Option<Option<Vec<DeviceId>>> {
    if !matches!(action, "create" | "update") {
        return None;
    }
    if clear_has(clear, "gpu_pin") {
        return Some(None);
    }
    let cards: Vec<DeviceId> = pin
        .as_ref()?
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .map(DeviceId::new)
        .collect();
    Some((!cards.is_empty()).then_some(cards))
}

/// Store `change` on `class`'s row `id` and publish it.
pub(crate) async fn write(
    state: &SharedState,
    class: Class,
    id: i64,
    change: Option<Option<Vec<DeviceId>>>,
) -> Result<(), String> {
    let Some(pin) = change else {
        return Ok(());
    };
    crate::store::set_gpu_pin(&state.db, class, id, pin.as_deref())
        .await
        .map_err(|e| format!("the GPU pin could not be saved: {e}"))?;
    state
        .reload_snapshot()
        .await
        .map_err(|e| format!("the GPU pin was saved but not reloaded: {e}"))?;
    Ok(())
}

/// A row's pin and the line under its command line, as its read answers
/// them (device-ledger design §7.1, §8).
pub(crate) async fn read(
    state: &SharedState,
    class: Class,
    model_id: &str,
) -> (Option<Vec<DeviceId>>, Option<String>) {
    let snap = state.snapshot();
    let pin = snap.gpu_pins.get(&(class, model_id.to_string())).cloned();
    let note = state.vram.card_note(state, &snap, class, model_id).await;
    (pin, note)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_patch_sets_clears_or_leaves_the_pin() {
        let some = Some(vec!["GPU-a".to_string(), " ".to_string()]);
        assert_eq!(
            patched("update", &some, None),
            Some(Some(vec![DeviceId::new("GPU-a")]))
        );
        assert_eq!(patched("update", &some, Some("gpu_pin")), Some(None));
        assert_eq!(patched("create", &Some(vec![]), None), Some(None));
        assert_eq!(patched("update", &None, None), None);
        assert_eq!(patched("delete", &some, None), None);
    }
}
