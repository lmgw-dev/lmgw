//! Which container a route lands on

use crate::config::{
    Route, AUDIO_UPSTREAM_ID, AUX_UPSTREAM_ID, IMAGE_UPSTREAM_ID, ROUTER_UPSTREAM_ID,
};
use crate::runtime::Class;

/// A resolved route's GPU destination: the registry key it would acquire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub class: Class,
    /// The concrete model id inside that class.
    pub model_id: String,
}

/// Which class a resolved route lands on, if any.
///
/// One sentinel id per class, and nothing else (§5). The old function also
/// matched the aux row's *name* and, failing that, sniffed the loopback port
/// out of the base URL and compared it with the three class `listen_port`
/// settings. Both are gone: the managed rows are gone with them, and per-model
/// containers have no class-wide port to compare against — a local route's
/// base URL is a placeholder until a hold overwrites it, so a port match could
/// only ever have been wrong. A stored upstream cannot hold a negative id, so
/// nothing a user configures can be mistaken for a local container.
pub fn classify(route: &Route) -> Option<Target> {
    let class = match route.upstream.id {
        ROUTER_UPSTREAM_ID => Class::Chat,
        AUX_UPSTREAM_ID => Class::Aux,
        AUDIO_UPSTREAM_ID => Class::Audio,
        IMAGE_UPSTREAM_ID => Class::Image,
        _ => return None,
    };
    Some(Target {
        class,
        model_id: route.upstream_model.clone(),
    })
}
