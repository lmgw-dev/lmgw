//! Pairing (client-apps design §1.4, device link design §3.1): the
//! `lmgw-pair:` link v2 that hands a one-time pairing token to the client,
//! and the device's lmgw URLs it carries.

use std::net::IpAddr;

use lmgw_api_types::device_link::{LINK_PREFIX, LINK_VERSION};

use crate::config::{DevicesListen, Settings};

/// The name prefix a device row carries, written by the server (§1.1).
pub const NAME_PREFIX: &str = "device:";

/// The prefix of the bearer key a device was paired with before TLS
/// (client-apps §1.1): a bearer that carries it is a device's, which the
/// gateway names when it refuses it (device link design §4.8).
pub const KEY_PREFIX: &str = "lmgw-device-";

/// The note a "this computer" pairing carries on the pairing form (§1.4).
pub const LOOPBACK_NOTE: &str = "reachable from this computer only";

/// The pairing link v2 (device link design §3.1, D6):
/// `lmgw-pair:?v=2&name=…&url=…[&url=…]&fp=sha256:…&token=lmgw-enrol-…`.
///
/// `name` is the device's own name, without the `device:` prefix; `urls` in
/// the owner's order. Each value is percent-encoded with RFC 3986's
/// unreserved set kept, so a client may parse the query with any URL
/// library: no `+` for a space, which only form decoding reads as one. The
/// link carries the token: it is a credential while the token lives, and no
/// log line carries it.
pub fn pairing_link(name: &str, urls: &[String], fp: &str, token: &str) -> String {
    let mut link = format!("{LINK_PREFIX}v={LINK_VERSION}&name={}", encode(name));
    for url in urls {
        link.push_str("&url=");
        link.push_str(&encode(url));
    }
    link.push_str(&format!("&fp={}&token={}", encode(fp), encode(token)));
    link
}

/// `urls` as a device's URL list (§3.1): each an `https` base URL (a host,
/// an optional port and path prefix; no user info, query or fragment) with a
/// host a server certificate can carry (an IP address, or a name of
/// letters, digits and hyphens), none twice, at least one. Returned
/// normalized (no trailing slash), as `lmgw-client` compares them. The
/// refusal names the URL and the rule.
pub fn check_urls(urls: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::with_capacity(urls.len());
    for (i, raw) in urls.iter().enumerate() {
        let url = normalize_base_url(raw)
            .map_err(|why| format!("url {} ('{}') is not usable: {why}", i + 1, raw.trim()))?;
        if super::tls::UrlHost::of_url(&url).is_none() {
            return Err(format!(
                "url {} ('{url}') names a host no server certificate may carry: use an IP \
                 address, or a name of letters, digits and hyphens",
                i + 1
            ));
        }
        if out.contains(&url) {
            return Err(format!("url {} ('{url}') repeats an earlier one", i + 1));
        }
        out.push(url);
    }
    if out.is_empty() {
        return Err("a device needs at least one URL to reach lmgw at".into());
    }
    Ok(out)
}

/// The URLs the pairing form proposes (§3.1), on the device listener's port:
/// for this computer (`local_only`), `https://127.0.0.1:<port>` alone; for
/// another device, the address the listener is set to, or the interface
/// addresses a wildcard bind expands to (`[::]` once widened, which
/// `loopback` becomes at the first remote pairing) that a device can count
/// on ([`propose_labelled`]). The refusal says why there is none.
pub fn proposed_urls(settings: &Settings, local_only: bool) -> Result<Vec<String>, String> {
    let port = settings.devices.effective_port(&settings.bind_addr)?;
    if local_only {
        return Ok(vec![format!("https://127.0.0.1:{port}")]);
    }
    let urls: Vec<String> = proposed_for_device(settings)?
        .into_iter()
        .map(|p| p.url)
        .collect();
    if urls.is_empty() {
        return Err(
            "this machine has no network address beyond loopback, link-local, temporary and \
             container addresses to propose for another device; pass the device's urls (a \
             WireGuard or LAN address, or a name)"
                .into(),
        );
    }
    Ok(urls)
}

/// The proposal for another device, with what the pairing form shows beside
/// each URL (*changed 2026-10-15*, WP4): the interface it is on, and whether
/// that is a point-to-point link (WireGuard) or a LAN's. The listener's
/// concrete address alone when it is set to one; empty when the machine has
/// none to propose (the form says so, and the owner adds a URL by hand).
pub fn proposed_for_device(settings: &Settings) -> Result<Vec<Proposed>, String> {
    let port = settings.devices.effective_port(&settings.bind_addr)?;
    if let DevicesListen::Addr(ip) = settings.devices.listen {
        if !ip.is_loopback() {
            return Ok(vec![Proposed {
                url: format!("https://{}:{port}", url_host(ip)),
                interface: String::new(),
                wireguard: false,
            }]);
        }
    }
    let temporary = ipv6_temporary();
    let ifaces: Vec<IfaceAddr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .map(|i| {
            let ip = i.ip();
            IfaceAddr {
                up: i.is_oper_up(),
                p2p: i.is_p2p(),
                temporary: matches!(ip, IpAddr::V6(v6) if temporary.contains(&v6)),
                name: i.name,
                ip,
            }
        })
        .collect();
    Ok(propose_labelled(&ifaces, port))
}

/// One proposed URL, with the interface it is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposed {
    pub url: String,
    /// The interface's name; empty for a listener set to one address.
    pub interface: String,
    /// On a point-to-point link: a WireGuard (or Tailscale) interface.
    pub wireguard: bool,
}

/// One interface address, as [`propose_labelled`] reads it.
#[derive(Debug, Clone)]
pub(crate) struct IfaceAddr {
    pub name: String,
    pub ip: IpAddr,
    pub up: bool,
    /// A point-to-point link: a WireGuard interface.
    pub p2p: bool,
    /// An IPv6 temporary (privacy) or deprecated address, which changes
    /// within a day or so.
    pub temporary: bool,
}

/// The URLs on `port` a device can count on among `ifaces` (*changed
/// 2026-10-15*, review): an up interface's address that is neither
/// loopback, link-local nor an IPv6 temporary one, and not on a container
/// or VM bridge ([`is_virtual_bridge`]), whose addresses only the machine's
/// own guests reach. The stable LAN and WireGuard addresses come first (a
/// point-to-point link, an IPv4 private or shared address, an IPv6 unique
/// local one), then the others; IPv4 before IPv6, then by interface name.
pub(crate) fn propose_labelled(ifaces: &[IfaceAddr], port: u16) -> Vec<Proposed> {
    let mut kept: Vec<&IfaceAddr> = ifaces
        .iter()
        .filter(|i| {
            i.up && !i.temporary
                && !i.ip.is_loopback()
                && !link_local(i.ip)
                && !is_virtual_bridge(&i.name)
        })
        .collect();
    let local = |i: &IfaceAddr| {
        i.p2p
            || match i.ip {
                IpAddr::V4(v4) => {
                    v4.is_private() || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
                }
                IpAddr::V6(v6) => (v6.segments()[0] & 0xfe00) == 0xfc00,
            }
    };
    kept.sort_by(|a, b| {
        (!local(a), a.ip.is_ipv6(), &a.name, a.ip).cmp(&(!local(b), b.ip.is_ipv6(), &b.name, b.ip))
    });
    let mut urls: Vec<Proposed> = Vec::new();
    for i in kept {
        let url = format!("https://{}:{port}", url_host(i.ip));
        if !urls.iter().any(|p| p.url == url) {
            urls.push(Proposed {
                url,
                interface: i.name.clone(),
                wireguard: i.p2p,
            });
        }
    }
    urls
}

/// [`propose_labelled`]'s URLs alone.
#[cfg(test)]
pub(crate) fn propose(ifaces: &[IfaceAddr], port: u16) -> Vec<String> {
    propose_labelled(ifaces, port)
        .into_iter()
        .map(|p| p.url)
        .collect()
}

fn link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_unicast_link_local(),
    }
}

/// Whether interface `name` is a container's or a VM's bridge or link:
/// podman's, CNI's, docker's (`docker0`, `br-` and twelve hex digits),
/// libvirt's (`virbr`, `vnet`), LXC's, LXD's and Incus's bridges, and the
/// `veth` ends of container pairs. A bridge the owner named (`br0`) is a
/// LAN's.
fn is_virtual_bridge(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "podman", "cni", "docker", "virbr", "vnet", "veth", "lxcbr", "lxdbr", "incusbr",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p))
        || name
            .strip_prefix("br-")
            .is_some_and(|id| id.len() == 12 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The IPv6 addresses the kernel marks temporary or deprecated
/// (`/proc/net/if_inet6`); none where the file does not exist.
fn ipv6_temporary() -> std::collections::HashSet<std::net::Ipv6Addr> {
    std::fs::read_to_string("/proc/net/if_inet6")
        .map(|text| parse_if_inet6(&text))
        .unwrap_or_default()
}

/// `/proc/net/if_inet6`'s addresses whose flags say `IFA_F_TEMPORARY`
/// (0x01, an RFC 8981 privacy address) or `IFA_F_DEPRECATED` (0x20, past its
/// preferred lifetime). A line is the address as 32 hex digits, the
/// interface's index, the prefix length, the scope and the flags in hex,
/// and the interface's name.
fn parse_if_inet6(text: &str) -> std::collections::HashSet<std::net::Ipv6Addr> {
    const TEMPORARY: u8 = 0x01;
    const DEPRECATED: u8 = 0x20;
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let flags = u8::from_str_radix(f.get(4)?, 16).ok()?;
            if flags & (TEMPORARY | DEPRECATED) == 0 {
                return None;
            }
            u128::from_str_radix(f.first()?, 16)
                .ok()
                .map(std::net::Ipv6Addr::from)
        })
        .collect()
}

/// Whether every URL of `urls` names a loopback host: a "this computer"
/// list, which a `loopback` device listener serves.
pub fn all_loopback(urls: &[String]) -> bool {
    urls.iter().all(|u| match super::tls::UrlHost::of_url(u) {
        Some(super::tls::UrlHost::Ip(ip)) => ip.is_loopback(),
        Some(super::tls::UrlHost::Name(n)) => n == "localhost",
        None => false,
    })
}

/// An lmgw base URL on the device link, by `lmgw-client`'s rule
/// (`pairing::normalize_base_url`, which a test holds this to): `https`, a
/// host, an optional port and path prefix; no user info, query or fragment;
/// without a trailing slash. The error is the rule.
fn normalize_base_url(s: &str) -> Result<String, String> {
    let u = reqwest::Url::parse(s.trim()).map_err(|e| format!("not a URL ({e})"))?;
    if u.scheme() != "https" {
        return Err("the scheme must be https: the device link is TLS only".into());
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err("it has no host".into());
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err("it must not carry a user name or password".into());
    }
    if u.query().is_some() {
        return Err("it must not carry a query (?…)".into());
    }
    if u.fragment().is_some() {
        return Err("it must not carry a fragment (#…)".into());
    }
    Ok(u.as_str().trim_end_matches('/').to_owned())
}

/// Whether `c` would make a name or a free text read as something else in a
/// close reason, a log line or the card (review W3-17, W4-19): a control
/// character, a format character ([`is_format`]: direction overrides and
/// isolates, zero-width characters) or a line or paragraph separator.
pub(crate) fn is_invisible(c: char) -> bool {
    c.is_control() || is_format(c) || is_separator(c)
}

/// Unicode's format characters (general category Cf), every one of them as
/// of Unicode 16 (review W4-19): direction overrides and isolates,
/// zero-width characters, the soft hyphen, the byte-order mark, the
/// prepended number marks, the hieroglyph, shorthand and musical format
/// controls, and the tag characters.
fn is_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// The line and paragraph separators (general categories Zl, Zp): neither a
/// control nor a format character, and a line break all the same in a close
/// reason or a log line (review W4-19).
fn is_separator(c: char) -> bool {
    matches!(c, '\u{2028}' | '\u{2029}')
}

/// An IP as a URL's host (IPv6 in brackets).
fn url_host(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Why `label` cannot be a device's hosting label, or `None` when it can
/// (§1.5).
///
/// Validated like a server's `tool_prefix`: the characters an exposed tool
/// name can carry, not one of lmgw's own namespaces, and not already a
/// registered server's prefix or name — or another device's label, which the
/// unique index also backstops. `own_id` is the device being written, so
/// restating its own label is not a clash.
// The backstop is migration 0062's case-insensitive unique index (review
// W2-25). Created on a database that already held two labels differing only
// in case, it would fail and stop the start. That cannot happen to a
// shipped install (0061 and 0062 shipped together, and labels are ASCII,
// so NOCASE folds exactly what these checks fold), and 0062 is left as it
// is: editing it would change its checksum (review W3-16). Should it ever be
// needed, the remedy is a repair ahead of the migrator,
// `store/migration_guards.rs::repair_before_migrations`, as 0018 got: clear
// the label of all but the lowest id of each colliding group, with a warn
// line, before 0062 runs.
pub fn label_refusal(
    snap: &crate::config::Snapshot,
    label: &str,
    own_id: Option<i64>,
) -> Option<String> {
    if label.is_empty() {
        return Some("a hosting label cannot be empty — leave it out for no grant".into());
    }
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Some(format!(
            "hosting label '{label}' may only contain letters, digits, '_' and '-' — it becomes \
             the prefix of the device's tool names ('{label}__…')"
        ));
    }
    if label.contains("__") {
        return Some(format!(
            "hosting label '{label}' cannot contain '__', the separator between a prefix and a \
             tool name"
        ));
    }
    if label.ends_with('_') {
        return Some(format!(
            "hosting label '{label}' cannot end in '_': its tools' names would run into the \
             '__' separator ('{label}__…') and into another label's namespace"
        ));
    }
    let folded = label.to_ascii_lowercase();
    if crate::mcp::RESERVED_NAMESPACES
        .iter()
        .any(|(p, _)| *p == folded)
    {
        return Some(format!(
            "'{label}' is one of lmgw's own tool namespaces ({}) — pick another hosting label",
            crate::mcp::RESERVED_NAMESPACES
                .iter()
                .map(|(p, _)| *p)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // A device's own hosted-tools row carries its label (client-apps
    // design §5.2): another device's is said by the key check below.
    if let Some(s) = snap
        .mcp_servers
        .values()
        .filter(|s| !s.is_device())
        .find(|s| {
            namespaces_overlap(&s.tool_prefix, label)
                || s.name.eq_ignore_ascii_case(label)
                // Its name as a prefix: what a collision gives its tools.
                || namespaces_overlap(&crate::mcp::names::name_qualifier(&s.name), label)
        })
    {
        return Some(format!(
            "'{label}' is already the MCP server '{}', or its namespace runs into it — its tools \
             are named '{}__…' — pick another hosting label",
            s.name,
            if s.tool_prefix.is_empty() {
                &s.name
            } else {
                &s.tool_prefix
            }
        ));
    }
    if let Some(k) = snap.api_keys.iter().find(|k| {
        Some(k.id) != own_id
            && k.hosts_label
                .as_deref()
                .is_some_and(|l| namespaces_overlap(l, label))
    }) {
        return Some(format!(
            "device '{}' already hosts tools under '{}' — one label, one device, and no label's \
             namespace inside another's",
            super::short_name(&k.name),
            k.hosts_label.as_deref().unwrap_or_default()
        ));
    }
    None
}

/// A key's device-tools grant as written (client-apps design L16 as changed
/// 2026-10-15): each label trimmed, blanks dropped, a repeat (in any case)
/// dropped, in the order given — or why a label cannot be in it.
///
/// A label must be one a device could host: the hosting label's characters,
/// no `__`, no trailing `_`, none of lmgw's own namespaces. Whether a device
/// hosts it now is not asked: a key may be granted Kai's tools before Kai
/// desktop was ever paired, and the Devices card says which labels no device
/// hosts.
pub fn device_tools_grant(labels: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for label in labels.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        let why = if !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            Some("may only contain letters, digits, '_' and '-', as a hosting label does")
        } else if label.contains("__") || label.ends_with('_') {
            Some("cannot contain '__' or end in '_', as no hosting label does")
        } else if crate::mcp::RESERVED_NAMESPACES
            .iter()
            .any(|(p, _)| p.eq_ignore_ascii_case(label))
        {
            Some("is one of lmgw's own tool namespaces, which no device hosts")
        } else {
            None
        };
        if let Some(why) = why {
            return Err(format!("device_tools: '{label}' {why}"));
        }
        if !out.iter().any(|l| l.eq_ignore_ascii_case(label)) {
            out.push(label.to_string());
        }
    }
    Ok(out)
}

/// Whether the namespaces of two prefixes, `<a>__…` and `<b>__…`, share a
/// name: one starts with the other (`desktop` and `desktop_`, whose names
/// begin `desktop___`; `desktop` and `desktop__x`). Case-insensitive, as
/// labels are compared. An empty prefix has no namespace.
pub(crate) fn namespaces_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let a = format!("{}__", a.to_ascii_lowercase());
    let b = format!("{}__", b.to_ascii_lowercase());
    a.starts_with(&b) || b.starts_with(&a)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grant keeps any label a device could host, hosted or not, trimmed
    /// and once; one no device could host is refused by name.
    #[test]
    fn a_device_tools_grant_takes_any_label_a_device_could_host() {
        let v = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            device_tools_grant(&v(&[" kai ", "", "desk-top", "KAI", "x_1"])).unwrap(),
            v(&["kai", "desk-top", "x_1"])
        );
        assert_eq!(device_tools_grant(&[]).unwrap(), Vec::<String>::new());
        for bad in ["kai desktop", "a__b", "kai_", "lmgw", "Docs", "kb", "k*"] {
            let e = device_tools_grant(&v(&[bad])).unwrap_err();
            assert!(e.starts_with("device_tools: '"), "{bad}: {e}");
        }
    }

    /// The link is the design's shape, and `lmgw-client`'s strict v2 parser
    /// reads back every value lmgw put in.
    #[test]
    fn the_link_is_the_spec_s_shape_and_the_client_reads_it() {
        let fp = format!("sha256:{}", "9f".repeat(32));
        let token = format!("lmgw-enrol-{}", "4b1d".repeat(8));
        let urls = vec![
            "https://192.0.2.1:8788".to_string(),
            "https://[2001:db8::1]:8788".to_string(),
        ];
        let link = pairing_link("my phone", &urls, &fp, &token);
        assert_eq!(
            link,
            format!(
                "lmgw-pair:?v=2&name=my%20phone&url=https%3A%2F%2F192.0.2.1%3A8788\
                 &url=https%3A%2F%2F%5B2001%3Adb8%3A%3A1%5D%3A8788&fp=sha256%3A{}&token={token}",
                "9f".repeat(32)
            )
        );
        let parsed = lmgw_client::pairing::PairingLink::parse(&link).unwrap();
        assert_eq!(parsed.name, "my phone");
        assert_eq!(parsed.urls.as_slice(), urls.as_slice());
        assert_eq!(parsed.fp.to_string(), fp);
        assert_eq!(parsed.token.expose(), token);
    }

    #[test]
    fn a_url_list_is_https_hosts_a_certificate_carries_each_once() {
        let ok = check_urls(&[
            "https://192.0.2.1:8788/".into(),
            "https://phone-gw.example".into(),
        ])
        .unwrap();
        assert_eq!(ok, ["https://192.0.2.1:8788", "https://phone-gw.example"]);
        for (bad, says) in [
            (vec!["http://192.0.2.1:8788".to_string()], "https"),
            (
                vec!["https://*.example:1".into()],
                "letters, digits and hyphens",
            ),
            (vec!["https://u:p@h:1".into()], "user name"),
            (vec!["https://h:1".into(), "https://h:1/".into()], "repeats"),
            (vec![], "at least one"),
        ] {
            let why = check_urls(&bad).unwrap_err();
            assert!(why.contains(says), "{bad:?}: {why}");
        }
        // The client's rule, the same answer.
        for u in [
            "https://192.0.2.1:8788/",
            "https://h/p/",
            "HTTPS://H:1",
            "http://h",
            "https://u@h",
            "https://h?q",
            "https://h#f",
            "nonsense",
        ] {
            assert_eq!(
                normalize_base_url(u),
                lmgw_client::pairing::normalize_base_url(u),
                "{u}"
            );
        }
        assert!(all_loopback(&[
            "https://127.0.0.1:8788".into(),
            "https://[::1]:1".into()
        ]));
        assert!(!all_loopback(&[
            "https://127.0.0.1:8788".into(),
            "https://192.0.2.1:1".into()
        ]));
    }

    /// Review (2026-10-15): the proposal leaves out what a device cannot
    /// count on, and puts the stable LAN and WireGuard addresses first.
    #[test]
    fn the_proposal_keeps_the_addresses_a_device_can_count_on() {
        let at = |name: &str, ip: &str| IfaceAddr {
            name: name.into(),
            ip: ip.parse().unwrap(),
            up: true,
            p2p: name.starts_with("wg"),
            temporary: false,
        };
        let ifaces = vec![
            at("lo", "127.0.0.1"),
            at("lo", "::1"),
            at("eth0", "fe80::1"),
            at("eth0", "169.254.3.4"),
            at("eth0", "203.0.113.7"),
            at("eth0", "2001:db8::10"),
            IfaceAddr {
                temporary: true,
                ..at("eth0", "2001:db8::beef")
            },
            at("eth0", "172.30.1.20"),
            at("wg0", "192.0.2.1"),
            at("wg0", "2001:db8::1"),
            at("br0", "172.30.2.1"),
            at("tailscale0", "100.64.0.2"),
            at("podman0", "10.88.0.1"),
            at("cni-podman1", "10.89.0.1"),
            at("docker0", "172.17.0.1"),
            at("br-0123456789ab", "172.18.0.1"),
            at("virbr0", "192.168.122.1"),
            at("veth1a2b3c", "10.88.0.9"),
            IfaceAddr {
                up: false,
                ..at("wlan0", "172.30.1.30")
            },
        ];
        assert_eq!(
            propose(&ifaces, 8788),
            [
                "https://172.30.2.1:8788",
                "https://172.30.1.20:8788",
                "https://100.64.0.2:8788",
                "https://192.0.2.1:8788",
                "https://[2001:db8::1]:8788",
                "https://203.0.113.7:8788",
                "https://[2001:db8::10]:8788",
            ]
        );
        assert!(propose(&ifaces[..4], 1).is_empty());
        // The form labels each with its interface, and says which are a
        // point-to-point link's.
        let labelled = propose_labelled(&ifaces, 8788);
        let of = |url: &str| labelled.iter().find(|p| p.url == url).unwrap().clone();
        assert_eq!(
            (
                of("https://192.0.2.1:8788").interface.as_str(),
                of("https://192.0.2.1:8788").wireguard
            ),
            ("wg0", true)
        );
        assert_eq!(
            (
                of("https://172.30.1.20:8788").interface.as_str(),
                of("https://172.30.1.20:8788").wireguard
            ),
            ("eth0", false)
        );
    }

    #[test]
    fn temporary_and_deprecated_ipv6_addresses_are_read_from_the_kernel_s_flags() {
        let text = "\
20010db8000000000000000000000010 02 40 00 80   eth0
20010db800000000000000000000beef 02 40 00 01   eth0
20010db800000000000000000000dead 02 40 00 21   eth0
fe800000000000000000000000000001 02 40 20 80   eth0
00000000000000000000000000000001 01 80 10 80       lo
";
        let temporary = parse_if_inet6(text);
        assert_eq!(temporary.len(), 2, "{temporary:?}");
        assert!(temporary.contains(&"2001:db8::beef".parse().unwrap()));
        assert!(temporary.contains(&"2001:db8::dead".parse().unwrap()));
    }

    /// A label ending in `_`, or whose namespace runs into a server's
    /// prefix or another device's label, is refused (review finding 2).
    #[test]
    fn a_label_may_not_run_into_another_namespace() {
        use crate::config::{ApiKey, ApiKeyKind, KeyPolicy, McpServer, McpTransport, Snapshot};
        let mut snap = Snapshot::default();
        let server = |id: i64, name: &str, prefix: &str| McpServer {
            id,
            name: name.into(),
            enabled: true,
            transport: McpTransport::Http,
            command: None,
            args: vec![],
            env: vec![],
            cwd: None,
            container_image: None,
            extra_run_args: vec![],
            url: None,
            headers: vec![],
            tool_prefix: prefix.into(),
            timeout_ms: 60_000,
            autostart: false,
            idle_seconds: 0,
            allow_sampling: false,
            sampling_alias: None,
            agent_id: None,
            device_key_id: None,
        };
        snap.mcp_servers.insert(1, server(1, "under", "web_"));
        snap.mcp_servers.insert(2, server(2, "deep", "git__hub"));
        snap.mcp_servers.insert(3, server(3, "phone.", ""));
        snap.api_keys.push(ApiKey {
            id: 7,
            name: "device:desk".into(),
            key_hash: String::new(),
            enabled: true,
            kind: ApiKeyKind::Device,
            key_plain: None,
            agent_id: None,
            policy: KeyPolicy::default(),
            note: String::new(),
            hosts_label: Some("desk".into()),
            self_admin: crate::config::DeviceAdmin::Off,
            link: Default::default(),
        });
        let refused = |l: &str| label_refusal(&snap, l, None);
        assert!(refused("tablet_").is_some_and(|w| w.contains("cannot end in '_'")));
        assert!(
            refused("web").is_some_and(|w| w.contains("'under'")),
            "web_ → web___…"
        );
        assert!(
            refused("git").is_some_and(|w| w.contains("'deep'")),
            "git__hub → git__hub__…"
        );
        assert!(refused("DESK").is_some_and(|w| w.contains("one label, one device")));
        assert!(
            refused("phone").is_some_and(|w| w.contains("'phone.'")),
            "phone. → phone___…, a collision's names"
        );
        assert_eq!(refused("phon"), None);
        assert_eq!(refused("webx"), None);
        assert_eq!(refused("desktop"), None);
        assert_eq!(label_refusal(&snap, "desk", Some(7)), None, "its own label");
        assert!(namespaces_overlap("a", "a_") && namespaces_overlap("a_", "a"));
        assert!(!namespaces_overlap("a", "ab") && !namespaces_overlap("", "a"));
    }
}
