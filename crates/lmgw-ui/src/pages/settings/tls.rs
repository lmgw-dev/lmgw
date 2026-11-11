//! Settings → Network & access → TLS (headless server design §4.7): the
//! main listener's certificate, key and address, its handshake bound and the
//! public URL — ordinary rows on the desktop that apply at the next start,
//! read-only in the server image, where its unit sets them — proxy mode's
//! trusted proxies (§4.5), and a panel with the listener as it runs: the
//! certificate it serves, what that certificate does not cover, a reload
//! that failed, and what proxy mode has seen since the start.

use leptos::prelude::*;
use lmgw_api_types::{ProxyStatus, TlsListenerState, TlsStatus};

use super::{f, labels, Ctl, Def, Page, RESTART_UNIT};

/// The group's rows, in page order.
pub(super) const TLS: &[Def] = &[
    f(
        "network",
        "tls",
        "tls.status",
        "TLS listener",
        Ctl::TlsStatus,
    )
    .terms("tls https certificate secure context listener serial expiry san names reload"),
    f(
        "network",
        "tls",
        "tls.cert_path",
        labels::TLS_CERTIFICATE,
        Ctl::Mono,
    )
    .l()
    .unit(RESTART_UNIT)
    .ph("/etc/lmgw/tls/tls.crt")
    .hint(
        "the PEM certificate chain, the leaf first, as a path this account can read; a renewed \
         file at the same path is served from the next connection on, without a restart",
    )
    .terms("tls https certificate chain pem fullchain acme letsencrypt"),
    f(
        "network",
        "tls",
        "tls.key_path",
        labels::TLS_PRIVATE_KEY,
        Ctl::Mono,
    )
    .l()
    .unit(RESTART_UNIT)
    .ph("/etc/lmgw/tls/tls.key")
    .hint("the PEM private key of that certificate; set together with it")
    .terms("tls https private key pem"),
    f(
        "network",
        "tls",
        "tls.listen",
        labels::TLS_ADDRESS,
        Ctl::Mono,
    )
    .unit(RESTART_UNIT)
    .ph("0.0.0.0:8443")
    .hint(
        "where the TLS listener binds; 0.0.0.0:8443 is every address on port 8443, which the \
         firewall has to let in. Empty = no TLS listener. The plain listener stays on its own \
         bind address",
    )
    .terms("tls https listen port bind address 8443 firewall"),
    f(
        "network",
        "tls",
        "tls.handshake_timeout_s",
        labels::TLS_HANDSHAKE_TIMEOUT,
        Ctl::Int(1),
    )
    .s()
    .unit("s")
    .hint("a client that has not finished its TLS handshake by then is closed; at least 1")
    .terms("tls handshake timeout slow client"),
    f(
        "network",
        "tls",
        "public_url",
        labels::PUBLIC_URL,
        Ctl::Mono,
    )
    .l()
    .ph("https://lmgw.example:8443")
    .hint(
        "the address people and other machines open: login links, snippets, pairing proposals \
         and agent origins are built from it. Empty = the TLS listener's address, else the bind \
         address. Applies at once",
    )
    .terms("public url origin domain name host link snippets https"),
    f(
        "network",
        "tls",
        "trusted_proxies",
        labels::TRUSTED_PROXIES,
        Ctl::Lines,
    )
    .unit("one per line")
    .ph("10.89.0.0/24")
    .hint(
        "proxy mode: the address or CIDR network of a reverse proxy in front of lmgw (Caddy, \
         Traefik, nginx). From these peers only, X-Forwarded-For, -Proto and -Host (or \
         Forwarded) say the client's address, scheme and host; from any other they are \
         ignored. Set the public URL to the address the proxy serves. Empty = off. Applies at \
         once",
    )
    .terms(
        "proxy reverse proxy trusted proxies caddy traefik nginx x-forwarded-for forwarded \
         cidr client address",
    ),
];

/// What the panel says of the listener: its line and the line's tone.
pub(super) fn state_line(s: &TlsStatus) -> (String, &'static str) {
    match s.state {
        TlsListenerState::Off => ("off: no certificate, key and address are set".into(), "off"),
        TlsListenerState::Serving => (
            format!(
                "serving on {} (TLS 1.2 and 1.3)",
                s.listening.as_deref().unwrap_or("?")
            ),
            "ok",
        ),
        TlsListenerState::NotServing if s.error.is_some() => {
            ("not serving: it did not start".into(), "warn")
        }
        TlsListenerState::NotServing => (
            "not serving yet: configured since the server started".into(),
            "warn",
        ),
    }
}

#[component]
pub(super) fn TlsPanel(page: Page, id: String, hidden: Signal<bool>) -> impl IntoView {
    let status = move || {
        page.data.with(|d| {
            d.as_ref()
                .and_then(|v| serde_json::from_value::<TlsStatus>(v["tls_status"].clone()).ok())
        })
    };
    let base = move || {
        page.data.with(|d| {
            d.as_ref()
                .and_then(|v| v["public_base_url"].as_str().map(str::to_string))
        })
    };
    let proxy = move || {
        page.data.with(|d| {
            d.as_ref()
                .and_then(|v| serde_json::from_value::<ProxyStatus>(v["proxy_status"].clone()).ok())
        })
    };
    let restart = move || {
        if page.container() {
            format!(
                "restart the service ({}) to apply them",
                lmgw_api_types::RESTART_COMMAND
            )
        } else {
            "Restart gateway in the tray menu applies them".to_string()
        }
    };
    view! {
        <div class="set-about" id=id hidden=move || hidden.get()>
            {move || {
                status()
                    .map(|s| {
                        let (line, tone) = state_line(&s);
                        let cert = s
                            .certificate
                            .clone()
                            .map(|c| {
                                let names = if c.names.is_empty() {
                                    "none (no subject alternative names)".to_string()
                                } else {
                                    c.names.join(", ")
                                };
                                view! {
                                    <div class="set-line">
                                        <span class="set-line-label">"Certificate"</span>
                                        <span class="mono">{names}</span>
                                    </div>
                                    <div class="dim">
                                        {format!(
                                            "serial {}, valid until {} UTC, issued by {}; loaded {} UTC",
                                            c.serial,
                                            c.not_after,
                                            c.issuer,
                                            c.loaded_at,
                                        )}
                                    </div>
                                }
                            });
                        let warnings = s
                            .warnings
                            .iter()
                            .map(|w| view! { <div class="notice warn">{w.clone()}</div> })
                            .collect::<Vec<_>>();
                        let error = s
                            .error
                            .clone()
                            .map(|e| view! { <div class="notice warn">{e}</div> });
                        let pending = s
                            .restart_pending
                            .then(|| {
                                view! {
                                    <div class="notice">
                                        {format!(
                                            "The TLS settings changed since the server started: {}.",
                                            restart(),
                                        )}
                                    </div>
                                }
                            });
                        view! {
                            <div class="set-line">
                                <span class="set-line-label">"TLS listener"</span>
                                <span
                                    class="chip chip-line"
                                    class:ok=tone == "ok"
                                    class:warn=tone == "warn"
                                    class:off=tone == "off"
                                >
                                    {line}
                                </span>
                            </div>
                            {cert}
                            {error}
                            {warnings}
                            {pending}
                        }
                    })
            }}
            {move || {
                proxy()
                    .filter(|p| p.on || !p.warnings.is_empty())
                    .map(|p| {
                        let peers = p
                            .first_peers
                            .iter()
                            .map(|l| view! { <div class="dim">{l.clone()}</div> })
                            .collect::<Vec<_>>();
                        let warnings = p
                            .warnings
                            .iter()
                            .map(|w| view! { <div class="notice warn">{w.clone()}</div> })
                            .collect::<Vec<_>>();
                        let refused = p
                            .refused
                            .clone()
                            .map(|r| view! { <div class="notice warn">{r}</div> });
                        view! {
                            <div class="set-line">
                                <span class="set-line-label">"Proxy mode"</span>
                                <span
                                    class="chip chip-line"
                                    class:ok=p.on
                                    class:off=!p.on
                                >
                                    {proxy_line(&p)}
                                </span>
                            </div>
                            {peers}
                            {warnings}
                            {refused}
                        }
                    })
            }}
            {move || {
                base()
                    .map(|b| {
                        view! {
                            <div class="set-line">
                                <span class="set-line-label">"Links are built from"</span>
                                <span class="mono">{b}</span>
                            </div>
                        }
                    })
            }}
        </div>
    }
}

/// What the panel says of proxy mode.
pub(super) fn proxy_line(p: &ProxyStatus) -> String {
    if p.on {
        "on: forwarded headers are read from the trusted proxies only".into()
    } else {
        "off: no peer's forwarded headers are read".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_listener_line_says_where_it_stands() {
        let off = TlsStatus::default();
        assert_eq!(state_line(&off).1, "off");
        let serving = TlsStatus {
            state: TlsListenerState::Serving,
            listening: Some("0.0.0.0:8443".into()),
            ..Default::default()
        };
        assert_eq!(
            state_line(&serving),
            ("serving on 0.0.0.0:8443 (TLS 1.2 and 1.3)".into(), "ok")
        );
        let failed = TlsStatus {
            state: TlsListenerState::NotServing,
            error: Some("the pair does not load".into()),
            ..Default::default()
        };
        assert_eq!(state_line(&failed).1, "warn");
    }
}
