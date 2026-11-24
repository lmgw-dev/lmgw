//! Settings › Documents' "Allowed sites" (`documents.source_hosts`,
//! document-conversion design ruling 9, §7.4.1): the places a docling
//! caller's web address may name, and how a URL's host is matched against
//! them.
//!
//! **The matching rule.** An entry is a host with an optional port, nothing
//! else: `docs.example`, `docs.example:8443`, `203.0.113.7`, `[2001:db8::7]:8080`.
//! - The host is read by the WHATWG host parser (`url::Host::parse`), the
//!   one a source URL's host goes through: names are lowercased and IDNA
//!   names become punycode (`bücher.example` is `xn--bcher-kva.example`);
//!   every IPv4 spelling (`2130706433`, `0x7f.1`, `0177.0.0.1`) is the
//!   address it means; a trailing dot is dropped on both sides.
//! - A name matches the same name only: no wildcard, no subdomain, no
//!   suffix. `docs.example` does not let `www.docs.example` through.
//! - An address matches the same address of the same family only: an IPv6
//!   URL host (also `[::ffff:203.0.113.7]`) never matches an IPv4 entry, and
//!   IPv4-mapped IPv6 entries are refused (write the IPv4 address).
//! - Without a port, an entry allows the scheme's default port only: 80
//!   for `http`, 443 for `https` (`http://docs.example:443/` is not
//!   `docs.example`); with one, that port only, either scheme.
//! - A URL with a user name or password is refused before it is matched
//!   (`http://docs.example@other.example/` is the host `other.example`, and
//!   is refused for its userinfo first).
//!
//! **Private addresses need their own entry** ("refused unless listed
//! explicitly"): a hop whose host matches and that resolves into a range
//! [`super::ranges`] refuses unless listed goes ahead only when the list
//! also names that very address, with the port the hop connects to, as an
//! address entry. `intranet.example` resolving to `172.16.0.5` is fetched with
//! the entries `intranet.example` and `172.16.0.5`; `127.0.0.1:8897` lets
//! `http://127.0.0.1:8897/` through, and nothing else on loopback.
//! Wildcards and address ranges as entries are not yet.

use std::net::IpAddr;

/// A host as matched: a name (lowercase, punycode, no trailing dot) or an
/// address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    Name(String),
    Ip(IpAddr),
}

impl Host {
    /// From a parsed host; a trailing dot goes.
    pub fn from_url_host(h: url::Host<&str>) -> Option<Self> {
        match h {
            url::Host::Domain(d) => {
                let d = d.strip_suffix('.').unwrap_or(d);
                (!d.is_empty()).then(|| Host::Name(d.to_ascii_lowercase()))
            }
            url::Host::Ipv4(a) => Some(Host::Ip(IpAddr::V4(a))),
            url::Host::Ipv6(a) => Some(Host::Ip(IpAddr::V6(a))),
        }
    }
}

impl std::fmt::Display for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Host::Name(n) => f.write_str(n),
            Host::Ip(IpAddr::V4(a)) => write!(f, "{a}"),
            Host::Ip(IpAddr::V6(a)) => write!(f, "[{a}]"),
        }
    }
}

/// One allowed site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub host: Host,
    /// `None`: 80 and 443.
    pub port: Option<u16>,
}

/// The characters no entry has: a scheme, a path, userinfo, a query, a
/// fragment, a wildcard, white space.
const NOT_IN_AN_ENTRY: &[char] = &['/', '\\', '@', '?', '#', '*', '%'];

impl Entry {
    /// Read one entry as the owner typed it.
    pub fn parse(text: &str) -> Result<Self, String> {
        let t = text.trim();
        let say = |why: &str| {
            Err(format!(
                "the allowed site '{t}' {why}; write a host name or address with an optional \
                 :port, such as docs.example, docs.example:8443, 203.0.113.7 or [2001:db8::7]"
            ))
        };
        if t.is_empty() {
            return say("is empty");
        }
        if t.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return say("has a space in it");
        }
        if t.contains("://") {
            return say("has a scheme (http:// or https://), which an entry leaves out");
        }
        if t.contains('*') {
            return say("has a wildcard, which lmgw does not match yet");
        }
        if t.contains(NOT_IN_AN_ENTRY) {
            return say("has a path, a user name, a query or a fragment in it");
        }
        let (host, port) = match split_port(t) {
            Ok(x) => x,
            Err(why) => return say(why),
        };
        let port = match port {
            None => None,
            Some(p) => match p.parse::<u16>() {
                Ok(n) if n > 0 => Some(n),
                _ => return say("has a port that is not a number from 1 to 65535"),
            },
        };
        // A bare IPv6 address goes to the parser in its brackets.
        let bracketed;
        let host = if host.contains(':') && !host.starts_with('[') {
            bracketed = format!("[{host}]");
            bracketed.as_str()
        } else {
            host
        };
        let parsed = match url::Host::parse(host) {
            Ok(h) => h,
            Err(_) => return say("is not a host name or address"),
        };
        let Some(host) = Host::from_url_host(match &parsed {
            url::Host::Domain(d) => url::Host::Domain(d.as_str()),
            url::Host::Ipv4(a) => url::Host::Ipv4(*a),
            url::Host::Ipv6(a) => url::Host::Ipv6(*a),
        }) else {
            return say("is not a host name or address");
        };
        if let Host::Ip(IpAddr::V6(a)) = &host {
            if a.to_ipv4_mapped().is_some() {
                return say("is an IPv4-mapped IPv6 address; write the IPv4 address itself");
            }
        }
        Ok(Entry { host, port })
    }

    /// Whether `host` on `port` under `scheme` is this entry.
    pub fn matches(&self, host: &Host, port: u16, scheme: &str) -> bool {
        self.host == *host && self.port_matches(port, scheme)
    }

    /// Its port, or without one the scheme's default port.
    fn port_matches(&self, port: u16, scheme: &str) -> bool {
        match self.port {
            None => default_port(scheme) == Some(port),
            Some(p) => p == port,
        }
    }
}

impl std::fmt::Display for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.port {
            None => write!(f, "{}", self.host),
            Some(p) => write!(f, "{}:{p}", self.host),
        }
    }
}

/// The port an entry without one allows under `scheme`.
fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

/// `host[:port]`, `[v6][:port]` or a bare IPv6 address.
fn split_port(t: &str) -> Result<(&str, Option<&str>), &'static str> {
    if t.starts_with('[') {
        let Some(end) = t.find(']') else {
            return Err("opens a [ without closing it");
        };
        let (host, rest) = t.split_at(end + 1);
        return match rest {
            "" => Ok((host, None)),
            r => match r.strip_prefix(':') {
                Some(p) => Ok((host, Some(p))),
                None => Err("has something after its ] that is not a :port"),
            },
        };
    }
    match t.matches(':').count() {
        0 => Ok((t, None)),
        1 => {
            let (h, p) = t.split_once(':').unwrap_or((t, ""));
            Ok((h, Some(p)))
        }
        // A bare IPv6 address: the parser wants it in brackets.
        _ if t.parse::<std::net::Ipv6Addr>().is_ok() => Ok((t, None)),
        _ => Err("has more than one ':' and is not an IPv6 address"),
    }
}

/// The owner's list, read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostList {
    entries: Vec<Entry>,
}

impl HostList {
    /// Read the setting; the first entry that does not read is the error
    /// (a save is refused by it).
    pub fn parse(entries: &[String]) -> Result<Self, String> {
        let mut out = Vec::new();
        for e in entries {
            let e = Entry::parse(e)?;
            if !out.contains(&e) {
                out.push(e);
            }
        }
        Ok(Self { entries: out })
    }

    /// The setting as stored, each entry in its normalised spelling, in
    /// order, without repeats: what a save writes.
    pub fn normalise(entries: &[String]) -> Result<Vec<String>, String> {
        Ok(Self::parse(entries)?
            .entries
            .iter()
            .map(ToString::to_string)
            .collect())
    }

    /// The stored setting at use: an entry that no longer reads (a database
    /// edited by hand) allows nothing and is logged by its position.
    pub fn lenient(entries: &[String]) -> Self {
        let mut out = Vec::new();
        for (i, e) in entries.iter().enumerate() {
            match Entry::parse(e) {
                Ok(e) => out.push(e),
                Err(_) => tracing::warn!(
                    "documents.source_hosts entry {i} does not read as a host and allows nothing"
                ),
            }
        }
        Self { entries: out }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether a URL's `host` on `port` under `scheme` is listed.
    pub fn allows(&self, host: &Host, port: u16, scheme: &str) -> bool {
        self.entries.iter().any(|e| e.matches(host, port, scheme))
    }

    /// Whether the list names `ip` itself as an address entry for `port`:
    /// what lets a hop into a range refused unless listed. An IPv4-mapped
    /// address counts as its IPv4 address here (a resolver may answer
    /// either form for the same place).
    pub fn lists_address(&self, ip: IpAddr, port: u16, scheme: &str) -> bool {
        let ip = ip.to_canonical();
        self.entries.iter().any(|e| match &e.host {
            Host::Ip(a) => a.to_canonical() == ip && e.port_matches(port, scheme),
            Host::Name(_) => false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(t: &str) -> Entry {
        Entry::parse(t).unwrap_or_else(|e| panic!("{t}: {e}"))
    }

    #[test]
    fn entries_read_to_their_normal_spelling() {
        for (typed, normal) in [
            ("Docs.Example", "docs.example"),
            ("docs.example.", "docs.example"),
            ("docs.example:8443", "docs.example:8443"),
            ("bücher.example", "xn--bcher-kva.example"),
            ("2130706433", "127.0.0.1"),
            ("0x7f.1", "127.0.0.1"),
            ("0177.0.0.1:8897", "127.0.0.1:8897"),
            ("[2001:DB8::7]:8080", "[2001:db8::7]:8080"),
            ("2001:db8::7", "[2001:db8::7]"),
            ("  203.0.113.7  ", "203.0.113.7"),
        ] {
            assert_eq!(one(typed).to_string(), normal, "{typed}");
        }
    }

    #[test]
    fn what_is_not_a_host_is_refused_by_name() {
        for bad in [
            "",
            "https://docs.example",
            "docs.example/path",
            "user@docs.example",
            "*.docs.example",
            "docs.example:0",
            "docs.example:99999",
            "docs.example:x",
            "docs example",
            "[::1",
            "[::1]x",
            "a:b:c",
            "[::ffff:127.0.0.1]",
            "docs.example?q",
            "%64ocs.example",
        ] {
            assert!(Entry::parse(bad).is_err(), "{bad:?} was read");
        }
    }

    #[test]
    fn a_name_matches_itself_on_default_ports_only() {
        let l = HostList::parse(&["docs.example".into(), "files.example:8080".into()]).unwrap();
        let n = |s: &str| Host::Name(s.into());
        assert!(l.allows(&n("docs.example"), 443, "https"));
        assert!(l.allows(&n("docs.example"), 80, "http"));
        // The default port is the scheme's own: not 443 for http, nor 80
        // for https.
        assert!(!l.allows(&n("docs.example"), 443, "http"));
        assert!(!l.allows(&n("docs.example"), 80, "https"));
        assert!(!l.allows(&n("docs.example"), 8080, "http"));
        assert!(!l.allows(&n("www.docs.example"), 443, "https"));
        assert!(!l.allows(&n("example"), 443, "https"));
        assert!(l.allows(&n("files.example"), 8080, "http"));
        assert!(l.allows(&n("files.example"), 8080, "https"));
        assert!(!l.allows(&n("files.example"), 80, "http"));
    }

    #[test]
    fn an_address_entry_lets_that_address_through_on_its_port() {
        let l = HostList::parse(&["127.0.0.1:8897".into(), "172.16.0.5".into()]).unwrap();
        let v4: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(l.allows(&Host::Ip(v4), 8897, "http"));
        assert!(!l.allows(&Host::Ip(v4), 8001, "http"));
        assert!(l.lists_address(v4, 8897, "http"));
        assert!(!l.lists_address(v4, 80, "http"));
        // The mapped form is the same address for the range exemption, but
        // never matches the IPv4 entry as a URL host.
        let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        assert!(l.lists_address(mapped, 8897, "http"));
        assert!(!l.allows(&Host::Ip(mapped), 8897, "http"));
        let private: IpAddr = "172.16.0.5".parse().unwrap();
        assert!(l.lists_address(private, 443, "https"));
        assert!(!l.lists_address(private, 443, "http"));
        assert!(!l.lists_address("172.16.0.6".parse().unwrap(), 443, "https"));
    }

    #[test]
    fn normalise_drops_repeats_and_lenient_skips_what_does_not_read() {
        let n = HostList::normalise(&["Docs.Example".into(), "docs.example.".into()]).unwrap();
        assert_eq!(n, ["docs.example"]);
        let l = HostList::lenient(&["https://x.example".into(), "docs.example".into()]);
        assert!(l.allows(&Host::Name("docs.example".into()), 443, "https"));
        assert_eq!(l.entries.len(), 1);
    }
}
