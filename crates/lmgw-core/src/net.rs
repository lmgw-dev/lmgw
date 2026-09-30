//! Turning the configured bind address into URLs a client can actually dial.
//!
//! `bind_addr` may be a wildcard (`0.0.0.0` / `[::]`) — a fine thing to listen
//! on, never a valid thing to connect *to*. The dashboard's click-to-copy
//! chips must hand out something pasteable, so a wildcard bind is expanded
//! into loopback plus one entry per non-loopback interface address (§10).

use std::net::{IpAddr, SocketAddr};

/// One address the gateway is reachable at, plus a hint for the chip tooltip
/// ("localhost", or the interface name for LAN addresses).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reachable {
    pub url: String,
    pub label: String,
}

/// Render an IP as a URL authority host (IPv6 needs brackets).
fn url_host(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

/// Every URL that reaches this gateway, most-useful first.
///
/// A concrete bind address yields exactly one entry (itself). A wildcard bind
/// yields loopback followed by each interface address the socket actually
/// answers on — a `0.0.0.0` socket answers on IPv4 only, `[::]` on both
/// (Linux dual-stack).
pub fn reachable_urls(bind_addr: &str) -> Vec<Reachable> {
    let Ok(addr) = bind_addr.parse::<SocketAddr>() else {
        // Not host:port (a hostname, say) — pass it through untouched rather
        // than inventing something.
        return vec![Reachable {
            url: format!("http://{bind_addr}"),
            label: String::new(),
        }];
    };
    let port = addr.port();
    if !addr.ip().is_unspecified() {
        return vec![Reachable {
            url: format!("http://{}:{port}", url_host(addr.ip())),
            label: if addr.ip().is_loopback() {
                "localhost".into()
            } else {
                "bind address".into()
            },
        }];
    }

    let mut out = vec![Reachable {
        url: format!("http://127.0.0.1:{port}"),
        label: "localhost".into(),
    }];
    let dual_stack = addr.is_ipv6();
    let mut ifaces = if_addrs::get_if_addrs().unwrap_or_default();
    ifaces.retain(|i| {
        !i.is_loopback() && !i.is_link_local() && i.is_oper_up() && (dual_stack || i.ip().is_ipv4())
    });
    // IPv4 before IPv6, then by interface name — stable output across renders.
    ifaces.sort_by(|a, b| {
        (a.ip().is_ipv6(), &a.name, a.ip()).cmp(&(b.ip().is_ipv6(), &b.name, b.ip()))
    });
    for i in ifaces {
        let url = format!("http://{}:{port}", url_host(i.ip()));
        if out.iter().any(|r| r.url == url) {
            continue;
        }
        out.push(Reachable { url, label: i.name });
    }
    out
}

/// The one URL to print where only one fits (snippets, MCP/wiring hints):
/// loopback for a wildcard bind, the configured address otherwise.
pub fn primary_base_url(bind_addr: &str) -> String {
    reachable_urls(bind_addr)
        .into_iter()
        .next()
        .map(|r| r.url)
        .unwrap_or_else(|| format!("http://{bind_addr}"))
}

/// The machine's own host name, or `None` where the kernel does not publish
/// one in a file.
///
/// Read from `/proc` (the live value) with `/etc/hostname` behind it, rather
/// than through a new dependency or a `hostname` subprocess. lmgw ships for
/// Linux; on anything else this answers `None` and the one caller — the
/// shadowing check in [`own_host_names`] — rests on the bind address alone,
/// which is the conservative half of that rule anyway.
pub fn machine_host_name() -> Option<String> {
    for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            let name = text.trim().to_ascii_lowercase();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// The names a resolver appends to this machine's short host name, read out of
/// a `resolv.conf`: every entry of a `search` line and the argument of a
/// `domain` line, lower-cased and without a trailing dot.
///
/// Parsed from the text rather than from the path so the rule is testable
/// without a file: the shape is a keyword, whitespace, then names, with `#`
/// and `;` starting a comment. `resolv.conf` says the last `search`/`domain`
/// wins, but both are collected here — an entry that *used* to be in effect
/// still names a zone this box answers in, and this list is a refusal's
/// evidence, not a lookup order.
fn resolv_search_domains(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split(['#', ';']).next().unwrap_or_default().trim();
        let Some((keyword, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if !matches!(keyword, "search" | "domain") {
            continue;
        }
        for name in rest.split_whitespace() {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            if !name.is_empty() && !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// [`resolv_search_domains`] of this box's own `/etc/resolv.conf`, or nothing
/// where there is none to read.
fn system_search_domains() -> Vec<String> {
    std::fs::read_to_string("/etc/resolv.conf")
        .map(|t| resolv_search_domains(&t))
        .unwrap_or_default()
}

/// Every host name this gateway answers on (origins §4.1), lower-cased and
/// without repeats:
///
/// - the hosts of [`reachable_urls`] — the bind address, or loopback plus each
///   interface address for a wildcard bind;
/// - [`machine_host_name`], which is the canonical name and so already the
///   FQDN where `/etc/hostname` holds one;
/// - `<short host name>.local`, the mDNS name every stock Fedora answers on;
/// - `<short host name>.<domain>` for each `search`/`domain` entry of
///   `/etc/resolv.conf`, which is the name every resolver client on this
///   network completes the short name to.
///
/// The last two are why this is not just the first two: with `bind_addr`
/// `0.0.0.0` and a host name of `myhost`, the bare suffix `local` would pass the
/// shadowing check and an agent with the id `myhost` would then be served at
/// `myhost.local` — this box's own mDNS address.
///
/// The two shadowing checks read this from both ends — an `agent_origin_suffix`
/// may not be a suffix of any of these nor share a cookie-able parent domain
/// with one, and a service agent's `<id>.<suffix>` may not equal one. IP
/// literals are in the list as they are spelled in a URL: an agent label can
/// never collide with one, and leaving them out would mean explaining which
/// entries count.
pub fn own_host_names(bind_addr: &str) -> Vec<String> {
    compose_own_host_names(bind_addr, machine_host_name(), &system_search_domains())
}

/// [`own_host_names`] with its two environment reads handed in, so every
/// branch of the composition is testable on a box whose own name and search
/// domain are whatever they are.
fn compose_own_host_names(
    bind_addr: &str,
    machine: Option<String>,
    search: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in reachable_urls(bind_addr) {
        if let Some(h) = reqwest::Url::parse(&r.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        {
            push_name(&mut out, h);
        }
    }
    if let Some(machine) = machine {
        // The short name is what a resolver completes; an `/etc/hostname` that
        // already holds an FQDN contributes both it and its own first label's
        // completions.
        let short = machine
            .split('.')
            .next()
            .unwrap_or(&machine)
            .trim_end_matches('.')
            .to_string();
        push_name(&mut out, machine.trim_end_matches('.').to_string());
        if !short.is_empty() {
            push_name(&mut out, format!("{short}.local"));
            for domain in search {
                push_name(&mut out, format!("{short}.{domain}"));
            }
        }
    }
    out
}

fn push_name(out: &mut Vec<String>, name: String) {
    let name = name.to_ascii_lowercase();
    if !name.is_empty() && !out.contains(&name) {
        out.push(name);
    }
}

/// Host part of an HTTP `Host` header (`example:8001` → `example`,
/// `[::1]:8001` → `[::1]`). Empty input yields an empty string.
pub fn host_only(host_header: &str) -> String {
    let h = host_header.trim();
    if let Some(end) = h.strip_prefix('[').and_then(|_| h.find(']')) {
        return h[..=end].to_string();
    }
    match h.split_once(':') {
        Some((h, _)) => h.to_string(),
        None => h.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concrete_bind_is_passed_through() {
        assert_eq!(
            reachable_urls("127.0.0.1:8787"),
            vec![Reachable {
                url: "http://127.0.0.1:8787".into(),
                label: "localhost".into()
            }]
        );
        assert_eq!(
            reachable_urls("192.168.1.10:8001")[0].url,
            "http://192.168.1.10:8001"
        );
        assert_eq!(reachable_urls("[::1]:8001")[0].url, "http://[::1]:8001");
    }

    #[test]
    fn wildcard_bind_never_yields_a_wildcard_url() {
        for bind in ["0.0.0.0:8001", "[::]:8001"] {
            let urls = reachable_urls(bind);
            assert_eq!(urls[0].url, "http://127.0.0.1:8001", "{bind}");
            assert!(
                !urls.iter().any(|r| r.url.contains("0.0.0.0")
                    || r.url.contains("[::]")
                    || r.url.contains("fe80")),
                "{bind}: {urls:?}"
            );
            // Interface addresses (if any) are all port-suffixed absolute URLs.
            assert!(urls.iter().all(|r| r.url.ends_with(":8001")), "{urls:?}");
        }
        // An IPv4 wildcard socket does not answer on IPv6 addresses.
        assert!(reachable_urls("0.0.0.0:8001")
            .iter()
            .all(|r| !r.url.contains('[')));
    }

    #[test]
    fn unparseable_bind_survives() {
        assert_eq!(
            primary_base_url("my-host.lan:8001"),
            "http://my-host.lan:8001"
        );
    }

    #[test]
    fn own_host_names_are_the_reachable_hosts_plus_this_machine() {
        let names = own_host_names("127.0.0.1:8001");
        assert!(names.contains(&"127.0.0.1".to_string()), "{names:?}");
        assert!(!names.iter().any(|h| h.contains(':')), "{names:?}");
        if let Some(h) = machine_host_name() {
            assert!(names.contains(&h), "{names:?}");
        }
        // A bind address that is a name rather than an address is passed
        // through by `reachable_urls`, and is a name the gateway answers on.
        assert!(own_host_names("board.lan:8001").contains(&"board.lan".to_string()));
    }

    /// The mDNS name and the resolver's completions are names this box answers
    /// on too (origins §4.1): with `0.0.0.0` and a host name of `myhost`, the
    /// suffix `local` would otherwise be free and the agent `myhost` would take
    /// `myhost.local`.
    #[test]
    fn own_host_names_include_the_mdns_and_search_domain_completions() {
        let names = compose_own_host_names(
            "0.0.0.0:8001",
            Some("Myhost".into()),
            &["lmgw.lan".into(), "home.arpa".into()],
        );
        for expected in [
            "127.0.0.1",
            "myhost",
            "myhost.local",
            "myhost.lmgw.lan",
            "myhost.home.arpa",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "{expected}: {names:?}"
            );
        }
        // Lower-cased once, and never twice in the list.
        assert!(
            !names.iter().any(|h| h.chars().any(|c| c.is_uppercase())),
            "{names:?}"
        );
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "{names:?}");
        // An `/etc/hostname` holding an FQDN keeps it, and its first label is
        // what gets completed.
        let fqdn = compose_own_host_names("127.0.0.1:8001", Some("myhost.lmgw.lan".into()), &[]);
        assert!(fqdn.contains(&"myhost.lmgw.lan".to_string()), "{fqdn:?}");
        assert!(fqdn.contains(&"myhost.local".to_string()), "{fqdn:?}");
        // No host name published: the bind address alone, as before.
        assert_eq!(
            compose_own_host_names("127.0.0.1:8001", None, &["lmgw.lan".into()]),
            vec!["127.0.0.1".to_string()]
        );
    }

    #[test]
    fn search_domains_are_read_off_both_keywords_and_comments_are_not() {
        let text = "# generated by resolvconf\nnameserver 127.0.0.53\nsearch lmgw.lan \
                    home.arpa.\ndomain lmgw.lan ; the old spelling\noptions edns0\n";
        assert_eq!(
            resolv_search_domains(text),
            vec!["lmgw.lan".to_string(), "home.arpa".to_string()]
        );
        assert!(resolv_search_domains("nameserver 1.1.1.1\n").is_empty());
        assert!(resolv_search_domains("#search lmgw.lan\n").is_empty());
    }

    #[test]
    fn host_header_port_is_stripped() {
        assert_eq!(host_only("127.0.0.1:8001"), "127.0.0.1");
        assert_eq!(host_only("myhost.lan"), "myhost.lan");
        assert_eq!(host_only("[fd00::1]:8001"), "[fd00::1]");
        assert_eq!(host_only(""), "");
    }
}
