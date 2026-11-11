//! Settings in the server image (container mode, headless server design
//! D2): the rows its container pins (§2.2) and the restart unit shown on the
//! desktop only (§6.2), read from the settings answer.

use leptos::prelude::*;
use serde_json::Value;

use super::{Def, Page};

/// The unit of a row that applies at the next start of the server, which
/// the tray's **Restart gateway** gives it (headless server design §6.2).
pub(super) const RESTART_UNIT: &str = "restart to apply";

/// Whether the settings answer `data` is the server image's (container
/// mode, headless server design D2).
pub(super) fn container_in(data: &Value) -> bool {
    data["deployment"].as_str() == Some("container")
}

/// `unit` as a row shows it under the settings answer `data`: the server
/// image has no in-process restart yet, so [`RESTART_UNIT`] is shown on the
/// desktop only (§6.2); every other unit as it is.
pub(super) fn unit_shown(unit: &'static str, data: Option<&Value>) -> &'static str {
    if unit == RESTART_UNIT && data.is_some_and(container_in) {
        ""
    } else {
        unit
    }
}

/// Why the settings answer `data` says the server's container sets `key`,
/// when it does ([`Page::pinned_why`]).
pub(super) fn pinned_why_in(data: &Value, key: &str) -> Option<String> {
    data["pinned"]
        .as_array()?
        .iter()
        .find(|p| p["setting"].as_str() == Some(key))
        .and_then(|p| p["why"].as_str())
        .map(str::to_string)
}

impl Page {
    /// Why the server's container sets `key` (headless server design §2.2),
    /// when it does: the row shows it read-only and says so.
    pub(super) fn pinned_why(&self, key: &str) -> Option<String> {
        self.data.with(|d| pinned_why_in(d.as_ref()?, key))
    }

    /// `d`'s unit as its row shows it now ([`unit_shown`]).
    pub(super) fn unit_of(&self, d: &Def) -> &'static str {
        self.data.with(|data| unit_shown(d.unit, data.as_ref()))
    }

    /// Whether this is the server image's settings answer ([`container_in`]).
    pub(super) fn container(&self) -> bool {
        self.data.with(|d| d.as_ref().is_some_and(container_in))
    }
}
