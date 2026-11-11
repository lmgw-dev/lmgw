//! The scheme and port an agent origin is published on, and the warning that
//! agent pages are unreachable from other machines (origins §4.1, headless
//! server design §4.3, D27, D33).

use super::is_localhost_suffix;
use crate::config::Settings;
use crate::net::ServingTls;

/// The scheme and port an agent origin is published on (headless server
/// design §4.3): the public origin's (`public_url`, else the TLS listener
/// that serves now, `tls`, else the plain listener), except under the suffix
/// `localhost`. That suffix resolves to the browser's own machine, so its
/// agent pages only ever work on this one (D27 says so to everyone else),
/// where the app's window loads the plain listener and no certificate covers
/// `*.localhost`: they stay on the plain listener's `http` and port.
pub fn origin_scheme_port(
    settings: &Settings,
    tls: Option<&ServingTls>,
) -> (crate::net::Scheme, Option<u16>) {
    if is_localhost_suffix(settings) {
        return (
            crate::net::Scheme::Http,
            super::own_port(&settings.bind_addr),
        );
    }
    crate::net::public_scheme_port(settings, tls)
}

/// "Agent pages are unreachable from other machines" (D27): wherever other
/// machines reach this dashboard (container mode, a TLS listener, a plain
/// listener off loopback) while the suffix is `localhost`, which every
/// browser resolves to its own machine.
pub fn remote_suffix_warning(settings: &Settings, container: bool) -> Option<String> {
    (is_localhost_suffix(settings) && crate::net::reached_from_elsewhere(settings, container))
        .then(|| {
            "agent pages are unreachable from other machines: the agent origin suffix is \
             `localhost`, which every browser resolves to its own machine. Set a suffix outside \
             the dashboard's own domain, with a wildcard DNS record for it pointing here and a \
             certificate that covers it (Settings → Agents & tools → Agent containers → Agent origin \
             suffix)"
                .to_string()
        })
}
