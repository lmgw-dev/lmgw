//! The public origin (headless server design D9, §4.3): the scheme, host
//! and port every URL lmgw prints or hands a person or another machine is
//! built from, and the TLS listener's own addresses.
//!
//! `public_url` when it is set; else the TLS listener's primary URL while one
//! **serves** ([`ServingTls`]); else the plain listener's
//! ([`super::primary_base_url`]). The TLS part is read from the listener a
//! start brought up, never from the stored `tls` section: that applies at the
//! next start, and a URL built from it before then (a desktop's save, a pair
//! that did not load) names a port nothing answers on. lmgw's own dials (an
//! agent's `/agents/<id>/mcp`, which carries the owner's bearer) keep
//! [`super::primary_base_url`]: they stay on this process's plain listener.

use std::net::{IpAddr, SocketAddr};

use crate::config::{parse_public_url, PublicUrl, Settings, TlsListenerConfig};

use super::{reachable_urls_as, Reachable};

/// A URL scheme lmgw serves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Scheme {
    /// The plain listener.
    #[default]
    Http,
    /// The TLS listener and the device listener.
    Https,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    /// The port a URL of this scheme means when it names none.
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }
}

impl std::fmt::Display for Scheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The TLS listener as it serves now: the address it is bound to and what
/// its start read. On the snapshot ([`crate::config::Snapshot::serving_tls`])
/// from the moment a start brings it up until that start stops or the next
/// start decides otherwise (`server::main_tls::TlsListenerSlot`); `None` while
/// no TLS listener serves, which includes a desktop start whose pair did not
/// load (D35).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServingTls {
    /// The bound address (a port of 0 in `tls.listen` is the one the OS
    /// chose).
    pub addr: SocketAddr,
    /// What the start brought it up with.
    pub config: TlsListenerConfig,
}

/// `public_url` read, when it is set (and parses: every save path and the
/// server's environment check it, so one that does not is a hand-edited
/// row, read as unset).
pub fn public_url(s: &Settings) -> Option<PublicUrl> {
    let t = s.public_url.trim();
    if t.is_empty() {
        return None;
    }
    parse_public_url(t).ok()
}

/// Every URL that reaches the TLS listener serving now, most useful first:
/// as [`super::reachable_urls`] does for the plain listener, with `https`.
/// Empty while none serves.
pub fn tls_reachable_urls(tls: Option<&ServingTls>) -> Vec<Reachable> {
    match tls {
        Some(t) => reachable_urls_as(Scheme::Https, &t.addr.to_string()),
        None => Vec::new(),
    }
}

/// The one base URL (no trailing slash) to hand a person or another machine
/// (D9): `public_url`, else the serving TLS listener's primary URL, else the
/// plain listener's primary URL on `bind_addr`.
pub fn public_base_url(s: &Settings, tls: Option<&ServingTls>) -> String {
    public_base_url_on(s, tls, &s.bind_addr)
}

/// [`public_base_url`] with the plain listener on `plain`: the start's login
/// line passes the address it actually bound (a `bind_addr` port of 0 is the
/// one the OS chose).
pub fn public_base_url_on(s: &Settings, tls: Option<&ServingTls>, plain: &str) -> String {
    if let Some(u) = public_url(s) {
        return u.base();
    }
    if let Some(first) = tls_reachable_urls(tls).into_iter().next() {
        return first.url;
    }
    super::primary_base_url(plain)
}

/// The public origin's scheme and port: `public_url`'s, else the serving TLS
/// listener's (`https`), else the plain listener's (`http`). `None` for the
/// port when the plain listener's address names none (a hand-written row).
pub fn public_scheme_port(s: &Settings, tls: Option<&ServingTls>) -> (Scheme, Option<u16>) {
    if let Some(u) = public_url(s) {
        return (u.scheme, Some(u.port));
    }
    if let Some(t) = tls {
        return (Scheme::Https, Some(t.addr.port()));
    }
    (Scheme::Http, crate::agents::service::own_port(&s.bind_addr))
}

/// The dashboard's URL for a device's `hello` (device link design §7.2,
/// §4.3 of the headless server design): `public_url` for every device that
/// is not "this computer"; else [`super::dashboard_url`]'s rule on the TLS
/// listener while one serves, and on `bind_addr` otherwise. "This computer"
/// keeps the plain listener's primary URL, which its own window loads.
pub fn device_dashboard_url(
    s: &Settings,
    tls: Option<&ServingTls>,
    this_computer: bool,
    arrived_on: Option<IpAddr>,
) -> Option<String> {
    if this_computer {
        return super::dashboard_url(&s.bind_addr, true, arrived_on);
    }
    if let Some(u) = public_url(s) {
        return Some(format!("{}/", u.base()));
    }
    match tls {
        Some(t) => super::dashboard_url_as(Scheme::Https, &t.addr.to_string(), false, arrived_on),
        None => super::dashboard_url(&s.bind_addr, false, arrived_on),
    }
}

/// `/api/connect`'s base URLs (D42): the public origin first (`public_url`,
/// labelled "public URL"), then the serving TLS listener's addresses, then
/// the plain listener's. In the server image (`container`) the plain
/// listener's loopback addresses are left out behind those, since they are
/// the container's own loopback; on the desktop they stay, because the
/// clients on this machine keep using them.
pub fn connect_bases(s: &Settings, tls: Option<&ServingTls>, container: bool) -> Vec<Reachable> {
    let mut out = Vec::new();
    if let Some(u) = public_url(s) {
        out.push(Reachable {
            url: u.base(),
            label: "public URL".into(),
        });
    }
    for r in tls_reachable_urls(tls) {
        if !out.iter().any(|o| o.url == r.url) {
            out.push(r);
        }
    }
    let loopback_only = s
        .bind_addr
        .trim()
        .parse::<SocketAddr>()
        .is_ok_and(|a| a.ip().is_loopback());
    if out.is_empty() || !(container && loopback_only) {
        for r in super::reachable_urls(&s.bind_addr) {
            if !out.iter().any(|o| o.url == r.url) {
                out.push(r);
            }
        }
    }
    out
}

/// Every host name this gateway answers on (origins §4.1, §4.3 here):
/// [`super::own_host_names`] of the plain listener, the configured TLS
/// listener's reachable hosts and `public_url`'s host. The suffix shadowing and
/// cookie-parent checks must see the names people use, not only the
/// container's host name.
pub fn own_host_names_for(s: &Settings) -> Vec<String> {
    let mut out = super::own_host_names(&s.bind_addr);
    // The configured listener's, not only the one serving: these checks
    // guard what the next start serves too.
    let configured = s
        .tls
        .listener()
        .map(|l| reachable_urls_as(Scheme::Https, &l.addr.to_string()))
        .unwrap_or_default();
    for r in configured {
        if let Some(h) = reqwest::Url::parse(&r.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        {
            super::push_name(&mut out, h);
        }
    }
    if let Some(u) = public_url(s) {
        super::push_name(&mut out, u.host.clone());
    }
    out
}

/// Whether other machines reach this dashboard (D27): container mode, a
/// TLS listener, or a plain listener on an address that is not loopback.
pub fn reached_from_elsewhere(s: &Settings, container: bool) -> bool {
    if container || s.tls.on() {
        return true;
    }
    match s.bind_addr.trim().parse::<std::net::SocketAddr>() {
        Ok(a) => !a.ip().is_loopback(),
        // A host name: only `localhost` is surely this machine alone.
        Err(_) => !super::host_only(&s.bind_addr).eq_ignore_ascii_case("localhost"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(bind: &str, tls_listen: &str, public: &str) -> Settings {
        let mut s = Settings {
            bind_addr: bind.into(),
            public_url: public.into(),
            ..Default::default()
        };
        if !tls_listen.is_empty() {
            s.tls.cert_path = "/etc/lmgw/tls/tls.crt".into();
            s.tls.key_path = "/etc/lmgw/tls/tls.key".into();
            s.tls.listen = tls_listen.into();
        }
        s
    }

    /// The TLS listener `s` configures, as a start that brought it up on
    /// `addr` publishes it.
    fn serving(s: &Settings, addr: &str) -> ServingTls {
        ServingTls {
            addr: addr.parse().unwrap(),
            config: s.tls.listener().unwrap(),
        }
    }

    #[test]
    fn the_public_base_is_the_public_url_then_the_serving_tls_then_plain() {
        let plain = with("127.0.0.1:8787", "", "");
        assert_eq!(public_base_url(&plain, None), "http://127.0.0.1:8787");
        assert_eq!(public_scheme_port(&plain, None), (Scheme::Http, Some(8787)));
        let tls = with("127.0.0.1:8787", "0.0.0.0:8443", "");
        let up = serving(&tls, "0.0.0.0:8443");
        assert_eq!(public_base_url(&tls, Some(&up)), "https://127.0.0.1:8443");
        assert_eq!(
            public_scheme_port(&tls, Some(&up)),
            (Scheme::Https, Some(8443))
        );
        // Configured and not serving (saved since the start, or its pair did
        // not load): the plain listener's, nothing on a port nobody answers.
        assert_eq!(public_base_url(&tls, None), "http://127.0.0.1:8787");
        assert_eq!(public_scheme_port(&tls, None), (Scheme::Http, Some(8787)));
        // The bound port, not the configured one.
        let any = with("127.0.0.1:8787", "127.0.0.1:0", "");
        let up_any = serving(&any, "127.0.0.1:41443");
        assert_eq!(
            public_base_url(&any, Some(&up_any)),
            "https://127.0.0.1:41443"
        );
        let public = with(
            "127.0.0.1:8787",
            "0.0.0.0:8443",
            "https://lmgw.example:9443",
        );
        assert_eq!(public_base_url(&public, None), "https://lmgw.example:9443");
        assert_eq!(
            public_scheme_port(&public, Some(&up)),
            (Scheme::Https, Some(9443))
        );
        let default_port = with("127.0.0.1:8787", "0.0.0.0:8443", "https://lmgw.example");
        assert_eq!(
            public_scheme_port(&default_port, None),
            (Scheme::Https, Some(443))
        );
        // The login line's plain fallback is the bound address.
        let zero = with("127.0.0.1:0", "", "");
        assert_eq!(
            public_base_url_on(&zero, None, "127.0.0.1:40123"),
            "http://127.0.0.1:40123"
        );
    }

    #[test]
    fn a_remote_device_gets_the_public_url() {
        let lan: IpAddr = "198.51.100.20".parse().unwrap();
        let public = with(
            "127.0.0.1:8787",
            "0.0.0.0:8443",
            "https://lmgw.example:8443",
        );
        assert_eq!(
            device_dashboard_url(&public, None, false, Some(lan)).as_deref(),
            Some("https://lmgw.example:8443/")
        );
        assert_eq!(
            device_dashboard_url(&public, None, true, Some(lan)).as_deref(),
            Some("http://127.0.0.1:8787/"),
            "this computer keeps the plain loopback its window loads"
        );
        let tls = with("127.0.0.1:8787", "0.0.0.0:8443", "");
        let up = serving(&tls, "0.0.0.0:8443");
        assert_eq!(
            device_dashboard_url(&tls, Some(&up), false, Some(lan)).as_deref(),
            Some("https://198.51.100.20:8443/")
        );
        assert_eq!(
            device_dashboard_url(&tls, None, false, Some(lan)),
            None,
            "configured, not serving: the loopback plain listener reaches no device"
        );
        let plain = with("127.0.0.1:8787", "", "");
        assert_eq!(device_dashboard_url(&plain, None, false, Some(lan)), None);
    }

    /// D42: the public URL, the serving TLS listener, then the plain one,
    /// whose loopback addresses stay on the desktop and are left out in the
    /// server image.
    #[test]
    fn connect_bases_keep_the_desktop_s_loopback() {
        let s = with(
            "127.0.0.1:8787",
            "127.0.0.1:8443",
            "https://lmgw.example:8443",
        );
        let up = serving(&s, "127.0.0.1:8443");
        let urls = |b: Vec<Reachable>| b.into_iter().map(|r| r.url).collect::<Vec<_>>();
        assert_eq!(
            urls(connect_bases(&s, Some(&up), false)),
            [
                "https://lmgw.example:8443",
                "https://127.0.0.1:8443",
                "http://127.0.0.1:8787"
            ]
        );
        assert_eq!(
            urls(connect_bases(&s, Some(&up), true)),
            ["https://lmgw.example:8443", "https://127.0.0.1:8443"]
        );
        assert_eq!(
            urls(connect_bases(
                &with("127.0.0.1:8787", "127.0.0.1:8443", ""),
                None,
                true
            )),
            ["http://127.0.0.1:8787"],
            "nothing serves TLS: the plain listener, whatever its address"
        );
    }

    #[test]
    fn own_host_names_carry_the_public_host_and_the_tls_listener() {
        let s = with(
            "127.0.0.1:8787",
            "192.0.2.10:8443",
            "https://lmgw.example:8443",
        );
        let names = own_host_names_for(&s);
        for n in ["127.0.0.1", "192.0.2.10", "lmgw.example"] {
            assert!(names.contains(&n.to_string()), "{n}: {names:?}");
        }
    }

    #[test]
    fn other_machines_reach_a_container_a_tls_listener_or_a_wide_bind() {
        assert!(!reached_from_elsewhere(
            &with("127.0.0.1:8787", "", ""),
            false
        ));
        assert!(!reached_from_elsewhere(&with("[::1]:8787", "", ""), false));
        assert!(reached_from_elsewhere(
            &with("127.0.0.1:8787", "", ""),
            true
        ));
        assert!(reached_from_elsewhere(
            &with("127.0.0.1:8787", "0.0.0.0:8443", ""),
            false
        ));
        assert!(reached_from_elsewhere(&with("0.0.0.0:8787", "", ""), false));
    }
}
