//! Proxy mode's trusted peers (headless server design §4.5, ruling 8's
//! second step): `trusted_proxies`, the addresses and networks whose
//! connections have their forwarded headers read
//! (`server::forwarded`). Empty = proxy mode off: no peer's forwarded
//! headers are read, as before proxy mode existed.
//!
//! Each entry is an address (`127.0.0.1`, `fd00::5`) or a network in CIDR
//! notation (`10.89.0.0/24`, `fd00::/8`). A peer is compared as its
//! canonical address, so an IPv4 client of a dual-stack listener
//! (`::ffff:10.89.0.7`) is the IPv4 address it maps, and IPv4 peers are
//! compared with IPv4 networks only: an IPv4-mapped IPv6 network
//! (`::ffff:192.0.2.0/120`) is read as the IPv4 network it maps, and no
//! other IPv6 network (not even `::/0`) holds an IPv4 peer.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use lmgw_api_types::settings_labels::TRUSTED_PROXIES_AT;

/// One trusted address or network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedNet {
    /// The network's first address (no bits set past the prefix).
    addr: IpAddr,
    /// The prefix length: up to 32 for IPv4, 128 for IPv6.
    prefix: u8,
}

impl TrustedNet {
    /// `text` read: an address, or an address and a prefix length. A
    /// network with bits set past its prefix (`192.0.2.5/24`) is refused
    /// naming both readings, since which one was meant is the writer's to
    /// say. Every refusal names the row on Settings and the value.
    pub fn parse(text: &str) -> Result<Self, String> {
        let t = text.trim();
        let bad = |why: String| {
            format!(
                "a trusted proxy ({TRUSTED_PROXIES_AT}) is an address or a network in CIDR \
                 notation such as 10.89.0.0/24 or fd00::/8 ({why}); got '{t}'"
            )
        };
        let (addr, prefix) = match t.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (t, None),
        };
        let addr: IpAddr = addr
            .parse()
            .map_err(|_| bad("the address does not parse".into()))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => max,
            Some(p) => match p.parse::<u8>() {
                Ok(p) if p <= max => p,
                _ => return Err(bad(format!("the prefix length is 0 to {max}"))),
            },
        };
        // The mapped range written as IPv6 is the IPv4 network it maps.
        let (addr, prefix) = match addr {
            IpAddr::V6(v6) if prefix >= 96 => match v6.to_ipv4_mapped() {
                Some(v4) => (IpAddr::V4(v4), prefix - 96),
                None => (addr, prefix),
            },
            _ => (addr, prefix),
        };
        let net = Self {
            addr: masked(addr, prefix),
            prefix,
        };
        if net.addr != addr {
            return Err(bad(format!(
                "it has bits set past its prefix: the network is {net}, the one address {}",
                Self {
                    addr,
                    prefix: if addr.is_ipv4() { 32 } else { 128 },
                }
            )));
        }
        Ok(net)
    }

    /// Whether `ip` is inside it, compared as its canonical address (an
    /// IPv4-mapped IPv6 peer is the IPv4 address it maps), and only with a
    /// network of its own family.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        ip.is_ipv4() == self.addr.is_ipv4() && masked(ip, self.prefix) == self.addr
    }

    /// Whether it is every address of its family (`0.0.0.0/0`, `::/0`):
    /// every client can then write its own address and scheme.
    pub fn is_everyone(&self) -> bool {
        self.prefix == 0
    }
}

impl std::fmt::Display for TrustedNet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// `ip` with every bit past `prefix` cleared.
fn masked(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let bits = u32::from(v4);
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V4(Ipv4Addr::from(bits & mask))
        }
        IpAddr::V6(v6) => {
            let bits = u128::from(v6);
            let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V6(Ipv6Addr::from(bits & mask))
        }
    }
}

/// A save's reading of `trusted_proxies`: every entry parsed and written
/// as the network it is (`127.0.0.1` is stored as `127.0.0.1/32`), blank
/// entries dropped, a repeated one kept once. The first entry that does not
/// parse refuses the save, naming it.
pub fn normalize_trusted_proxies(list: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for entry in list {
        if entry.trim().is_empty() {
            continue;
        }
        let net = TrustedNet::parse(entry)?.to_string();
        if !out.contains(&net) {
            out.push(net);
        }
    }
    Ok(out)
}

/// The stored list read for use: every entry that parses. Every save path
/// and the server's environment check the list, so one that does not parse
/// is a hand-edited row; it trusts nothing, and
/// [`unreadable_trusted_proxies`] names it for the start's log and the TLS
/// card.
pub fn trusted_nets(list: &[String]) -> Vec<TrustedNet> {
    list.iter()
        .filter_map(|e| TrustedNet::parse(e).ok())
        .collect()
}

/// The stored entries that do not parse (a hand-edited settings row): each
/// trusts nothing, said rather than skipped in silence.
pub fn unreadable_trusted_proxies(list: &[String]) -> Vec<String> {
    list.iter()
        .filter(|e| !e.trim().is_empty() && TrustedNet::parse(e).is_err())
        .cloned()
        .collect()
}

/// A text list of trusted proxies as the environment writes it
/// (`LMGW_TRUSTED_PROXIES`): entries separated by commas, whitespace or
/// both.
pub fn split_trusted_proxies(text: &str) -> Vec<String> {
    text.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(t: &str) -> TrustedNet {
        TrustedNet::parse(t).unwrap()
    }

    fn ip(t: &str) -> IpAddr {
        t.parse().unwrap()
    }

    #[test]
    fn an_entry_is_an_address_or_a_network() {
        assert_eq!(net("127.0.0.1").to_string(), "127.0.0.1/32");
        assert_eq!(net(" 10.89.0.0/24 ").to_string(), "10.89.0.0/24");
        assert_eq!(net("fd00::/8").to_string(), "fd00::/8");
        assert_eq!(net("::1").to_string(), "::1/128");
        assert_eq!(net("0.0.0.0/0").to_string(), "0.0.0.0/0");
        // The mapped range is the IPv4 network it maps.
        assert_eq!(net("::ffff:192.0.2.0/120").to_string(), "192.0.2.0/24");
        assert_eq!(net("::ffff:127.0.0.1").to_string(), "127.0.0.1/32");
        for bad in [
            "",
            "192.0.2.0/33",
            "fd00::/129",
            "192.0.2.0/",
            "192.0.2.0/x",
            "lmgw.example",
            "[::1]",
            "fe80::1%eth0",
            "192.0.2.0/24/24",
        ] {
            let e = TrustedNet::parse(bad).unwrap_err();
            assert!(
                e.starts_with(
                    "a trusted proxy (Settings → Network & access → TLS → Trusted proxies) is \
                     an address or a network"
                ),
                "{bad}: {e}"
            );
        }
        let e = TrustedNet::parse("192.0.2.5/24").unwrap_err();
        assert!(
            e.contains("the network is 192.0.2.0/24, the one address 192.0.2.5/32"),
            "{e}"
        );
    }

    #[test]
    fn a_peer_is_compared_as_its_canonical_address() {
        let lan = net("10.89.0.0/24");
        assert!(lan.contains(ip("10.89.0.7")));
        assert!(!lan.contains(ip("10.89.1.7")));
        // An IPv4 client of a dual-stack listener.
        assert!(lan.contains(ip("::ffff:10.89.0.7")));
        assert!(!lan.contains(ip("fd00::7")));
        let v6 = net("fd00::/8");
        assert!(v6.contains(ip("fd12:3456::1")));
        assert!(!v6.contains(ip("10.89.0.7")));
        // An IPv4 peer is compared with IPv4 networks only; the mapped
        // range written in IPv6 is the IPv4 network it maps.
        assert!(!net("::/0").contains(ip("192.0.2.1")));
        assert!(!net("::/1").contains(ip("192.0.2.1")));
        assert!(net("::ffff:0:0/96").contains(ip("192.0.2.1")));
        assert_eq!(net("::ffff:0:0/96").to_string(), "0.0.0.0/0");
        assert!(net("0.0.0.0/0").contains(ip("192.0.2.1")));
        assert!(!net("0.0.0.0/0").contains(ip("2001:db8::1")));
        assert!(net("127.0.0.1").contains(ip("127.0.0.1")));
        assert!(!net("127.0.0.1").contains(ip("127.0.0.2")));
        assert!(net("0.0.0.0/0").is_everyone() && !lan.is_everyone());
    }

    #[test]
    fn a_save_writes_each_entry_as_its_network_once() {
        let list = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            normalize_trusted_proxies(&list(&["127.0.0.1", " ", "127.0.0.1/32", "fd00::/8"]))
                .unwrap(),
            list(&["127.0.0.1/32", "fd00::/8"])
        );
        assert!(normalize_trusted_proxies(&list(&["192.0.2.0/24", "nope"]))
            .unwrap_err()
            .contains("got 'nope'"));
        assert_eq!(
            split_trusted_proxies("10.89.0.0/24, 127.0.0.1\tfd00::/8,,"),
            list(&["10.89.0.0/24", "127.0.0.1", "fd00::/8"])
        );
        let stored = list(&["192.0.2.0/24", "hand-edited"]);
        assert_eq!(trusted_nets(&stored), vec![net("192.0.2.0/24")]);
        assert_eq!(unreadable_trusted_proxies(&stored), list(&["hand-edited"]));
    }
}
