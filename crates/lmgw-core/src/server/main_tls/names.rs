//! Which names a certificate covers (RFC 6125 §6.4: a wildcard is the whole
//! left-most label and stands for exactly one label, under at least two
//! labels; an IP address matches an IP address entry only, a name a DNS
//! entry only), and the warnings Settings and the log carry for what it
//! should cover and does not (headless server design §4.3).

use std::net::IpAddr;

use crate::config::Settings;

/// One subject alternative name of a leaf, by its kind: an IP address
/// written as a DNS name is not an IP address entry, and a client never
/// matches it as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum San {
    /// A `dNSName`, lower-cased as written (`*.lmgw-apps.example` for a
    /// wildcard).
    Dns(String),
    /// An `iPAddress`.
    Ip(IpAddr),
}

impl std::fmt::Display for San {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dns(d) => f.write_str(d),
            Self::Ip(ip) => write!(f, "{ip}"),
        }
    }
}

/// Whether `sans` (a leaf's subject alternative names,
/// [`super::CertInfo::sans`]) cover `host`: a name, an IP address, or a
/// bracketed IPv6 address; one trailing dot and the case do not matter.
pub fn covers(sans: &[San], host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(&host);
    if let Ok(ip) = host.parse::<IpAddr>() {
        return sans
            .iter()
            .any(|n| matches!(n, San::Ip(a) if a.to_canonical() == ip.to_canonical()));
    }
    sans.iter().any(|n| match n {
        San::Dns(d) => dns_covers(d, host),
        San::Ip(_) => false,
    })
}

/// A DNS entry against a host name: equal, or a wildcard for the host's one
/// left-most label under a parent of two labels or more (`*.example` is no
/// wildcard a client accepts).
fn dns_covers(name: &str, host: &str) -> bool {
    let name = name.trim_end_matches('.');
    match name.strip_prefix("*.") {
        Some(parent) => {
            parent.contains('.')
                && host
                    .split_once('.')
                    .is_some_and(|(label, rest)| !label.is_empty() && rest == parent)
        }
        None => name == host,
    }
}

/// What the certificate whose names are `sans` does not cover that `s`
/// says it serves: the public URL's host, and the agents' addresses
/// (`*.<agent_origin_suffix>`) once the suffix is not `localhost`. Each a
/// sentence for Settings → Network & access → TLS and the log.
pub fn name_warnings(s: &Settings, sans: &[San]) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(u) = crate::net::public_url(s) {
        if !covers(sans, &u.host) {
            out.push(format!(
                "this certificate does not cover the public URL's host ({}): browsers opening \
                 {} will show a certificate error. It covers {}",
                u.bare_host(),
                u.base(),
                shown(sans)
            ));
        }
    }
    let suffix = s
        .agent_origin_suffix
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if !suffix.is_empty() && suffix != "localhost" {
        let wildcard = format!("*.{suffix}");
        if !suffix.contains('.') {
            out.push(format!(
                "the agents' addresses are under a single-label suffix ({suffix}): no \
                 certificate covers {wildcard}, since a client accepts a wildcard only under two \
                 labels or more. Set a suffix such as lmgw-apps.example"
            ));
        } else if !sans
            .iter()
            .any(|n| matches!(n, San::Dns(d) if d.trim_end_matches('.') == wildcard))
        {
            out.push(format!(
                "this certificate does not cover the agents' addresses ({wildcard}): their \
                 pages will show a certificate error. It covers {}",
                shown(sans)
            ));
        }
    }
    out
}

fn shown(sans: &[San]) -> String {
    if sans.is_empty() {
        "no name (it has no subject alternative names)".into()
    } else {
        sans.iter()
            .map(San::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dns(n: &[&str]) -> Vec<San> {
        n.iter().map(|s| San::Dns(s.to_string())).collect()
    }

    #[test]
    fn a_wildcard_covers_exactly_one_label() {
        let mut n = dns(&["lmgw.example", "*.lmgw-apps.example"]);
        n.push(San::Ip("192.0.2.10".parse().unwrap()));
        n.push(San::Ip("2001:db8::5".parse().unwrap()));
        assert!(covers(&n, "lmgw.example"));
        assert!(covers(&n, "LMGW.example."));
        assert!(covers(&n, "board.lmgw-apps.example"));
        assert!(
            !covers(&n, "lmgw-apps.example"),
            "the wildcard is no name itself"
        );
        assert!(!covers(&n, "a.board.lmgw-apps.example"), "one label only");
        assert!(!covers(&n, "other.example"));
        assert!(covers(&n, "192.0.2.10"));
        assert!(covers(&n, "[2001:db8::5]"));
        assert!(
            covers(&n, "::ffff:192.0.2.10"),
            "a mapped address is the address"
        );
        assert!(!covers(&n, "192.0.2.11"));
        // An IP address is never matched by a DNS name, a wildcard or the
        // address itself written as one.
        assert!(!covers(&dns(&["*.0.2.10"]), "192.0.2.10"));
        assert!(!covers(&dns(&["192.0.2.10"]), "192.0.2.10"));
        // Nor a name by an IP entry.
        assert!(!covers(
            &[San::Ip("192.0.2.10".parse().unwrap())],
            "lmgw.example"
        ));
    }

    #[test]
    fn a_single_label_wildcard_covers_nothing() {
        let n = dns(&["*.example", "*.lan"]);
        assert!(!covers(&n, "lmgw.example"));
        assert!(!covers(&n, "box.lan"));
        assert!(covers(&dns(&["*.a.example"]), "b.a.example"));
    }

    #[test]
    fn the_public_host_and_the_agent_suffix_are_checked() {
        let mut s = Settings {
            public_url: "https://lmgw.example:8443".into(),
            ..Default::default()
        };
        assert!(
            name_warnings(&s, &dns(&["lmgw.example"])).is_empty(),
            "localhost suffix"
        );
        let w = name_warnings(&s, &dns(&["other.example"]));
        assert_eq!(w.len(), 1);
        assert!(
            w[0].contains("public URL's host (lmgw.example)") && w[0].contains("other.example"),
            "{w:?}"
        );
        s.agent_origin_suffix = "lmgw-apps.example".into();
        let w = name_warnings(&s, &dns(&["lmgw.example"]));
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("(*.lmgw-apps.example)"), "{w:?}");
        assert!(name_warnings(&s, &dns(&["lmgw.example", "*.lmgw-apps.example"])).is_empty());
        s.agent_origin_suffix = "lan".into();
        let w = name_warnings(&s, &dns(&["lmgw.example", "*.lan"]));
        assert!(
            w.len() == 1 && w[0].contains("single-label suffix (lan)"),
            "{w:?}"
        );
    }
}
