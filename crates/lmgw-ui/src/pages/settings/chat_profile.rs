//! Settings -> Chat -> "Profile for new threads" (`chat_profile`): the
//! options are the gateway's personality profiles, read when the page opens.

use leptos::prelude::*;
use lmgw_ui_kit::profiles::api;

/// `(value, label)` for a select: "Default" (none), then the profiles.
pub(super) fn options() -> Signal<Vec<(String, String)>> {
    let list = LocalResource::new(api::list);
    Signal::derive(move || {
        let profiles = match list.get() {
            Some(Ok(l)) => l.profiles,
            _ => Vec::new(),
        };
        std::iter::once((String::new(), "Default".to_string()))
            .chain(profiles.into_iter().map(|p| (p.id.to_string(), p.name)))
            .collect()
    })
}
