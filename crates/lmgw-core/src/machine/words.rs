//! The check's small wording helpers: an item, sizes, times.

use lmgw_api_types::machine::{ItemState, MachineCause, MachineItem, MachineItemId};

pub(super) fn item(
    id: MachineItemId,
    state: ItemState,
    sentence: &str,
    detail: Vec<String>,
    cause: Option<MachineCause>,
) -> MachineItem {
    MachineItem {
        id,
        state,
        sentence: sentence.to_string(),
        detail,
        cause,
        action: None,
    }
}

/// An item not checked, because `because`.
pub(super) fn not_checked(id: MachineItemId, because: &str) -> MachineItem {
    item(
        id,
        ItemState::NotChecked,
        &format!("Not checked yet: {because}."),
        Vec::new(),
        None,
    )
}

/// A file time as the other DTOs write times: UTC, `YYYY-MM-DD HH:MM:SS`.
pub(super) fn utc(ns: i128) -> String {
    let secs = i64::try_from(ns.div_euclid(1_000_000_000)).unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// A file time as a person reads a day: `October 14, 2026` (UTC).
pub(super) fn day(ns: i128) -> String {
    let secs = i64::try_from(ns.div_euclid(1_000_000_000)).unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|t| t.format("%B %-d, %Y").to_string())
        .unwrap_or_else(|| "an unknown day".into())
}
